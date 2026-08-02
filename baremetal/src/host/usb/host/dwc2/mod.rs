//! Synopsys DWC2 USB 2.0 OTG host-mode driver — polled.
//!
//! Targets the BCM2710 / BCM2837 instance at MMIO base
//! `0x3F98_0000`. Host-mode only, full-speed only, no hub, no
//! splits, no isochronous — the touchscreen sits direct on the OTG
//! port and reports at ~16 ms cadence.
//!
//! Init sequence ported from Circle's `lib/usb/dwhcidevice.cpp`
//! (`Initialize` / `InitCore` / `InitHost` / `Reset` / `PowerOn` /
//! `EnableRootPort`) — line for line, just dropping the IRQ
//! plumbing because we poll. Reading Circle's source is the
//! shortest path to understanding the order: most steps' bit values
//! have aliases that look like they don't matter on the BCM2710 PHY
//! configuration but in fact change the controller's interpretation
//! of subsequent writes (e.g. ULPI_UTMI_SEL has to be cleared after
//! soft reset, not before, or the PHY clock won't restart).
//!
//! `control_transfer` (used during enumeration) is polled. The
//! touchscreen's interrupt-IN runs IRQ-driven instead: `start_int_in`
//! arms channel `INT_CH` and enables the core's host-channel IRQ, and
//! `service_int_in` (called from the trap-IRQ path on BCM2835 GPU
//! source 9) harvests each report and re-arms. See the "IRQ-driven
//! interrupt-IN" section below.

pub mod regs;

use super::super::{SetupPacket, Speed, UsbError, UsbResult};
use super::{ControlData, UsbHostController};

use crate::kprintln;
use core::sync::atomic::{AtomicBool, Ordering};

/// Default MMIO base on BCM2710 / BCM2837. Pi 4 / Pi 5 use different
/// addresses; we only support BCM2710 here.
pub const DWC2_BASE: usize = 0x3F98_0000;

/// Host-controller state. Owned by the `INSTANCE` singleton.
pub struct Dwc2 {
    base: usize,
    inited: bool,
    /// Number of host channels exposed by the core (read from
    /// GHWCFG2 during init; BCM2710 reports 8).
    pub num_channels: u8,
    /// Per-endpoint data-toggle state. Indexed by `ep_num & 0xF`.
    /// In DMA mode the DWC2 core does *not* auto-advance the host's
    /// expected DATA0/DATA1 PID across separate transfers — the
    /// host driver maintains it the same way Circle's
    /// `CUSBEndpoint::SkipPID()` does, toggling on each successful
    /// non-control transaction. Control transfers always start with
    /// SETUP, which has its own PID encoding and resets the toggle
    /// implicitly, so they ignore this table.
    int_next_pid: [Pid; 16],

    /// IRQ-driven interrupt-IN endpoint state (the touchscreen). Once
    /// `start_int_in` arms channel `INT_CH`, the panel's reports arrive
    /// as USB IRQs (BCM2835 GPU source 9) and are harvested by
    /// `service_int_in` from the trap-IRQ path — no more polling. Zero
    /// until armed.
    int_in_armed: bool,
    int_in_addr: u8,
    int_in_ep: u8,
    int_in_mps: u16,
}

/// Dedicated host channel for the persistent interrupt-IN. Kept
/// distinct from channel 0 (which `dma_xfer` uses for control transfers
/// during enumeration) so a future control transfer can't clobber the
/// armed touchscreen channel.
const INT_CH: usize = 1;

impl Dwc2 {
    const fn new(base: usize) -> Self {
        Self {
            base,
            inited: false,
            num_channels: 0,
            // First IN packet on a freshly-configured endpoint is
            // DATA0 (USB 2.0 §8.5.1). After enumeration's
            // SET_CONFIGURATION the device's toggles all reset to
            // DATA0, so we mirror it host-side.
            int_next_pid: [Pid::Data0; 16],
            int_in_armed: false,
            int_in_addr: 0,
            int_in_ep: 0,
            int_in_mps: 0,
        }
    }

    #[inline]
    fn reg(&self, offset: usize) -> *mut u32 {
        (self.base + offset) as *mut u32
    }

    #[inline]
    pub(crate) fn read(&self, offset: usize) -> u32 {
        // SAFETY: MMIO at fixed address mapped Device-nGnRE.
        unsafe { core::ptr::read_volatile(self.reg(offset)) }
    }

    #[inline]
    pub(crate) fn write(&self, offset: usize, value: u32) {
        // SAFETY: same as `read`.
        unsafe { core::ptr::write_volatile(self.reg(offset), value) }
    }

    #[inline]
    pub(crate) fn modify(&self, offset: usize, clear: u32, set: u32) {
        let v = self.read(offset);
        self.write(offset, (v & !clear) | set);
    }

    /// Poll register `offset` until `(read() & mask) == target_mask`
    /// or until `timeout_ms` ms elapse on CNTPCT_EL0. Returns `true`
    /// on success, `false` on timeout. Mirrors Circle's `WaitForBit`.
    fn wait_for_bit(
        &self,
        offset: usize,
        mask: u32,
        want_set: bool,
        timeout_ms: u32,
    ) -> bool {
        let freq: u64;
        let start: u64;
        // SAFETY: sysreg reads.
        unsafe {
            core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
                options(nomem, nostack, preserves_flags));
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) start,
                options(nomem, nostack, preserves_flags));
        }
        let deadline = start.wrapping_add((freq * timeout_ms as u64) / 1000);
        loop {
            let v = self.read(offset);
            let set = (v & mask) == mask;
            if want_set == set {
                return true;
            }
            let now: u64;
            // SAFETY: sysreg read.
            unsafe {
                core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                    options(nomem, nostack, preserves_flags));
            }
            if now.wrapping_sub(deadline) as i64 >= 0 {
                return false;
            }
        }
    }

    /// Core soft reset. Wait for AHB idle → set SoftReset →
    /// poll for clear → 100 ms settle delay. Circle's
    /// `CDWHCIDevice::Reset()` ported verbatim.
    fn core_reset(&self) -> UsbResult<()> {
        if !self.wait_for_bit(regs::GRSTCTL, regs::GRSTCTL_AHBIDLE, true, 100) {
            kprintln!("dwc2: AHBIdle never asserted before reset");
            return Err(UsbError::Other);
        }
        self.modify(regs::GRSTCTL, 0, regs::GRSTCTL_CSFTRST);
        if !self.wait_for_bit(regs::GRSTCTL, regs::GRSTCTL_CSFTRST, false, 10) {
            kprintln!("dwc2: SoftReset never cleared");
            return Err(UsbError::Other);
        }
        crate::arch::cpu::delay_ms(100);
        Ok(())
    }

    /// Flush every host TX FIFO. `txfnum=0x10` is the magic value
    /// "all FIFOs" per the Synopsys PG.
    fn flush_tx_fifo_all(&self) -> UsbResult<()> {
        self.write(
            regs::GRSTCTL,
            regs::GRSTCTL_TXFFLSH | regs::GRSTCTL_TXFNUM_ALL,
        );
        if !self.wait_for_bit(regs::GRSTCTL, regs::GRSTCTL_TXFFLSH, false, 10) {
            kprintln!("dwc2: TxFFlsh never cleared");
            return Err(UsbError::Other);
        }
        Ok(())
    }

    fn flush_rx_fifo(&self) -> UsbResult<()> {
        self.write(regs::GRSTCTL, regs::GRSTCTL_RXFFLSH);
        if !self.wait_for_bit(regs::GRSTCTL, regs::GRSTCTL_RXFFLSH, false, 10) {
            kprintln!("dwc2: RxFFlsh never cleared");
            return Err(UsbError::Other);
        }
        Ok(())
    }
}

impl UsbHostController for Dwc2 {
    fn init(&mut self) -> UsbResult<()> {
        if self.inited {
            return Ok(());
        }

        // 1. Sanity: DWC2 OTG core present? (read GSNPSID).
        let id = self.read(regs::GSNPSID);
        kprintln!("dwc2: GSNPSID = {:#010x}", id);
        if (id >> 16) != 0x4F54 {
            kprintln!("dwc2: SNPSID mismatch — controller not present");
            return Err(UsbError::NotReady);
        }

        // 2. Power on the USB HCD rail via the VC mailbox. Circle's
        //    `PowerOn()`. On a Pi that booted with USB already on
        //    (firmware default) this is a no-op, but Circle defends
        //    against the suspended case so we mirror it.
        match crate::host::mailbox::set_power_state(
            crate::host::mailbox::DEVICE_ID_USB_HCD,
            crate::host::mailbox::POWER_STATE_ON | crate::host::mailbox::POWER_STATE_WAIT,
        ) {
            Ok(state) => {
                kprintln!("dwc2: power-state response = {:#x}", state);
                if state & crate::host::mailbox::POWER_STATE_ON == 0 {
                    kprintln!("dwc2: USB HCD did not power on");
                    return Err(UsbError::NotReady);
                }
            }
            Err(e) => {
                kprintln!("dwc2: set_power_state mailbox call failed: {:?}", e);
                return Err(UsbError::NotReady);
            }
        }

        // 3. Disable global IRQ delivery (we poll). Circle clears
        //    `GAHBCFG.GlobalIntMask` here and then re-enables it
        //    later for the IRQ-driven path; we leave it off
        //    permanently.
        self.modify(regs::GAHBCFG, regs::GAHBCFG_GLOBALINT_MASK, 0);

        // 4. InitCore() — Circle line-for-line:
        //
        //    a. GUSBCFG: clear ULPI_EXT_VBUS_DRV (bit 20) +
        //       TERM_SEL_DL_PULSE (bit 22). We don't drive an
        //       external VBUS regulator and we're not on a ULPI
        //       PHY anyway.
        self.modify(
            regs::GUSBCFG,
            regs::GUSBCFG_ULPI_EXT_VBUS_DRV | regs::GUSBCFG_TERM_SEL_DL_PULSE,
            0,
        );

        //    b. Core soft reset.
        self.core_reset()?;

        //    c. After reset, clear ULPI_UTMI_SEL (bit 4) + PHYIF
        //       (bit 3) — i.e. select UTMI+ 8-bit. The BCM2710's
        //       on-chip PHY is exactly that.
        self.modify(
            regs::GUSBCFG,
            regs::GUSBCFG_ULPI_UTMI_SEL | regs::GUSBCFG_PHYIF,
            0,
        );

        //    d. Read HWCFG2; sanity-check architecture==internal DMA.
        let hw2 = self.read(regs::GHWCFG2);
        let arch = (hw2 >> 3) & 0x3;
        let hs_phy = (hw2 >> 6) & 0x3;
        let fs_phy = (hw2 >> 8) & 0x3;
        self.num_channels = ((hw2 >> 14) & 0xF) as u8 + 1;
        kprintln!(
            "dwc2: HWCFG2 = {:#010x} (arch={} hs_phy={} fs_phy={} channels={})",
            hw2, arch, hs_phy, fs_phy, self.num_channels
        );
        if arch != 2 {
            kprintln!("dwc2: only internal-DMA architecture supported");
            return Err(UsbError::Other);
        }

        //    e. ULPI_FSLS / ULPI_CLK_SUS_M. For BCM2710 the chip's
        //       HS PHY is UTMI+ (hs_phy=1), so we fall into the
        //       "else" branch and clear both bits.
        let want_ulpi_fsls = hs_phy == 2 /* ULPI */ && fs_phy == 1 /* DEDICATED */;
        let mask = regs::GUSBCFG_ULPI_FSLS | regs::GUSBCFG_ULPI_CLK_SUS_M;
        if want_ulpi_fsls {
            self.modify(regs::GUSBCFG, 0, mask);
        } else {
            self.modify(regs::GUSBCFG, mask, 0);
        }

        //    f. GAHBCFG: enable internal DMA + wait-AXI-writes. Clear
        //       MAX_AXI_BURST so the core picks the default. Don't
        //       touch GlobalIntMask (kept off above).
        self.modify(
            regs::GAHBCFG,
            regs::GAHBCFG_MAX_AXI_BURST_MASK,
            regs::GAHBCFG_DMA_ENABLE | regs::GAHBCFG_WAIT_AXI_WRITES,
        );

        //    g. GUSBCFG: clear HNP + SRP capability (host-only,
        //       no role swap).
        self.modify(
            regs::GUSBCFG,
            regs::GUSBCFG_HNP_CAPABLE | regs::GUSBCFG_SRP_CAPABLE,
            0,
        );

        // 5. InitHost() — same source.
        //
        //    a. Restart the PHY clock by writing 0 to PCGCCTL.
        self.write(regs::PCGCCTL, 0);

        //    b. HCFG.FSLSPClkSel. For BCM2710 (UTMI+ PHY) Circle
        //       picks 30/60 MHz. ULPI+FSLS would pick 48 MHz.
        self.modify(regs::HCFG, regs::HCFG_FSLSPCLK_SEL_MASK, 0);
        if want_ulpi_fsls {
            self.modify(regs::HCFG, 0, regs::HCFG_FSLSPCLK_SEL_48M);
        } else {
            self.modify(regs::HCFG, 0, regs::HCFG_FSLSPCLK_SEL_30_60M);
        }

        //    c. FIFO sizing (DWC_CFG_DYNAMIC_FIFO in Circle).
        //       1024 dwords each for RX / NP-TX / P-TX.
        const RX: u32 = 1024;
        const NPTX: u32 = 1024;
        const PTX: u32 = 1024;
        self.write(regs::GRXFSIZ, RX);
        self.write(regs::GNPTXFSIZ, RX | (NPTX << 16));
        self.write(regs::HPTXFSIZ, (RX + NPTX) | (PTX << 16));

        //    d. Flush all FIFOs.
        self.flush_tx_fifo_all()?;
        self.flush_rx_fifo()?;

        //    e. HPRT.PrtPwr = 1. Mask W1C bits or the read-modify
        //       will ACK any pending change-status bits.
        let hprt = self.read(regs::HPRT);
        if hprt & regs::HPRT_PRT_PWR == 0 {
            let new = (hprt & !regs::HPRT_W1C) | regs::HPRT_PRT_PWR;
            self.write(regs::HPRT, new);
        }
        kprintln!("dwc2: HPRT after PrtPwr = {:#010x}", self.read(regs::HPRT));

        self.inited = true;
        Ok(())
    }

    fn port_reset_and_speed(&mut self) -> UsbResult<Speed> {
        if !self.inited {
            return Err(UsbError::NotReady);
        }

        // 1. Wait up to 510 ms for ConnSts to assert (= device
        //    attached). Circle's `EnableRootPort()`.
        if !self.wait_for_bit(regs::HPRT, regs::HPRT_PRT_CONN_STS, true, 510) {
            kprintln!("dwc2: no device connected on root port");
            return Err(UsbError::NotReady);
        }
        // 2. USB 2.0 attach-debounce.
        crate::arch::cpu::delay_ms(100);

        // 3. Assert reset.
        let hprt = self.read(regs::HPRT);
        let new = (hprt & !regs::HPRT_W1C) | regs::HPRT_PRT_RST;
        self.write(regs::HPRT, new);
        // 4. USB 2.0 tDRSTR.
        crate::arch::cpu::delay_ms(50);
        // 5. Deassert reset.
        let hprt = self.read(regs::HPRT);
        let new = (hprt & !regs::HPRT_W1C) & !regs::HPRT_PRT_RST;
        self.write(regs::HPRT, new);
        // 6. USB 2.0 tRSTRCY (Circle uses 20 ms — some devices need
        //    longer than the spec's 10 ms).
        crate::arch::cpu::delay_ms(20);

        let hprt = self.read(regs::HPRT);
        let spd = (hprt & regs::HPRT_PRT_SPD_MASK) >> regs::HPRT_PRT_SPD_SHIFT;
        let speed = match spd {
            0 => Speed::High,
            1 => Speed::Full,
            2 => Speed::Low,
            _ => return Err(UsbError::Other),
        };
        kprintln!("dwc2: port up, speed = {:?}, HPRT = {:#010x}", speed, hprt);

        // For full-speed-only mode set the host frame interval to
        // 1 ms (48000 cycles at 48 MHz). Circle does this when its
        // "full-speed" build option is on; we always run that way.
        if speed == Speed::Full {
            self.write(regs::HFIR, 48_000);
        }

        Ok(speed)
    }

    fn control_transfer(
        &mut self,
        addr: u8,
        max_packet_size0: u8,
        setup: &SetupPacket,
        data: ControlData<'_>,
    ) -> UsbResult<usize> {
        if !self.inited {
            return Err(UsbError::NotReady);
        }

        // SETUP stage. Copy the 8-byte setup packet into the DMA
        // scratch and ship it.
        let scratch = scratch_buf();
        let setup_bytes = {
            // SAFETY: SetupPacket is repr(C, packed), 8 bytes; we read
            // it byte-wise to dodge alignment surprises.
            let p = setup as *const SetupPacket as *const u8;
            // SAFETY: p points to 8 valid bytes.
            unsafe { core::slice::from_raw_parts(p, 8) }
        };
        scratch[..8].copy_from_slice(setup_bytes);
        self.dma_xfer(
            addr,
            0,                                /* ep0 */
            false,                            /* OUT */
            EpType::Control,
            max_packet_size0 as u16,
            Pid::Setup,
            scratch.as_mut_ptr(),
            8,
        )?;

        // DATA stage. Direction comes from bmRequestType bit 7.
        let dir_in = (setup.bm_request_type & 0x80) != 0;
        let had_data_stage;
        let bytes_xferred = if setup.w_length == 0 {
            had_data_stage = false;
            0
        } else {
            match data {
                ControlData::None => {
                    had_data_stage = false;
                    0
                }
                ControlData::In(buf) => {
                    had_data_stage = true;
                    let want = buf.len().min(scratch.len());
                    let got = self.dma_xfer(
                        addr,
                        0,
                        true, /* IN */
                        EpType::Control,
                        max_packet_size0 as u16,
                        Pid::Data1,
                        scratch.as_mut_ptr(),
                        want,
                    )?;
                    buf[..got].copy_from_slice(&scratch[..got]);
                    got
                }
            }
        };

        // STATUS stage. Zero-length, opposite direction of DATA. If
        // there was no DATA stage (or w_length=0), STATUS is IN.
        let status_in = !had_data_stage || !dir_in;
        self.dma_xfer(
            addr,
            0,
            status_in,
            EpType::Control,
            max_packet_size0 as u16,
            Pid::Data1,
            scratch.as_mut_ptr(),
            0,
        )?;

        Ok(bytes_xferred)
    }

}

/// Endpoint type encoded into HCCHAR.EpType[19:18]. Hardware also
/// defines Isochronous = 1 and Bulk = 2; permanently out of scope.
#[derive(Copy, Clone, PartialEq, Eq)]
enum EpType {
    Control = 0,
    Interrupt = 3,
}

/// PID encoded into HCTSIZ[30:29]. Hardware also defines Data2 = 1
/// (high-speed high-bandwidth only; never used at full speed).
#[derive(Copy, Clone, PartialEq, Eq)]
enum Pid {
    Data0 = 0,
    Data1 = 2,
    /// MDATA for non-control; SETUP for control (same encoding 0b11).
    Setup = 3,
}

impl Dwc2 {
    /// One DMA-mode transfer on channel 0. Returns bytes actually
    /// transferred (≤ `len`). The DWC2 core handles packet
    /// splitting up to PktCnt internally; on IN it halts early on
    /// a short packet.
    #[allow(clippy::too_many_arguments)]
    fn dma_xfer(
        &mut self,
        dev_addr: u8,
        ep_num: u8,
        ep_in: bool,
        ep_type: EpType,
        mps: u16,
        pid: Pid,
        buf_va: *mut u8,
        len: usize,
    ) -> UsbResult<usize> {
        const CH: usize = 0;
        /// DWC2's internal AHB master sees DRAM through the GPU bus
        /// uncached alias (`pa | 0xC0000000`). Writing the bare ARM
        /// PA to HCDMA gives a transaction error every time. The
        /// cached alias (0x40000000) would also work if we kept VC
        /// L2 maintenance in sync, but we don't.
        const GPU_UNCACHED_BASE: u32 = 0xC000_0000;

        // Port-state gate. If the device dropped off the bus (the
        // touchscreen panel rebooting takes its hub function down),
        // an enabled channel raises no HCINT bits at all, so the
        // poll loop below would burn its full 50 ms timeout — and
        // since this runs from the trap_irq tail, a 50 ms tail per
        // ~16 ms timer period means the next IRQ is already pending
        // at every ERET and the guest executes ZERO instructions
        // until the device returns (observed as the "kernel hang"
        // with the guest PC frozen mid- straight-line code). Bail
        // before touching the channel; the caller is responsible
        // for detaching its device on NotReady.
        let hprt = self.read(regs::HPRT);
        if hprt & regs::HPRT_PRT_CONN_STS == 0 || hprt & regs::HPRT_PRT_ENA == 0 {
            return Err(UsbError::NotReady);
        }

        // Defensive: Circle's StartTransaction checks HCCHAR.CHENA
        // on entry and runs a CHDIS sequence if the channel hasn't
        // fully halted from a previous transfer. We always disable
        // after each transfer (step 8 below), but the hardware can
        // leave CHENA=1 momentarily on error halts — observed in
        // the post-error HCCHAR dumps (0xa0508040 had CHENA still
        // set). Mirror Circle's safeguard.
        let pre = self.read(regs::hcchar(CH));
        if pre & regs::HCCHAR_CHENA != 0 {
            self.write(
                regs::hcchar(CH),
                (pre & !regs::HCCHAR_CHENA) | regs::HCCHAR_CHDIS,
            );
            // Wait up to 10 ms for CHHLTD to confirm the disable.
            // Cheap: usually 0 cycles on an already-halted channel.
            let _ = self.wait_for_bit(regs::hcint(CH), regs::HCINT_CHHLTD, true, 10);
        }

        // Flush our writes / invalidate ahead of the DMA so the
        // controller and CPU agree on the buffer contents. dc_civac
        // does both in one pass.
        let buf_pa = buf_va as u64;
        crate::arch::cpu::dc_civac_range(buf_pa, len.max(1));

        // 1. Clear all HCINT bits (W1C).
        self.write(regs::hcint(CH), 0xFFFF_FFFF);
        // 2. Polling — no IRQ mask needed.
        self.write(regs::hcintmsk(CH), 0);
        // 3. Buffer address. Convert ARM PA → VC bus uncached
        //    address — the DMA master uses that view.
        let bus_addr = (buf_pa as u32) | GPU_UNCACHED_BASE;
        self.write(regs::hcdma(CH), bus_addr);
        // 4. Clear HCSPLT — we never use splits but stale bits
        //    from a previous channel use can carry over.
        self.write(regs::hcsplt(CH), 0);
        // 5. Transfer size + packet count + initial PID. Even for a
        //    zero-length status stage the spec requires PktCnt=1.
        let pkt_count: u32 = if len == 0 {
            1
        } else {
            ((len as u32) + (mps as u32) - 1) / (mps as u32)
        };
        let tsiz = (len as u32 & 0x7_FFFF)
            | (pkt_count << regs::HCTSIZ_PKTCNT_SHIFT)
            | ((pid as u32) << regs::HCTSIZ_PID_SHIFT);
        self.write(regs::hctsiz(CH), tsiz);
        // 6. Characteristics + enable.
        let mut hcchar = (mps as u32) & regs::HCCHAR_MPS_MASK;
        hcchar |= ((ep_num as u32) & 0xF) << regs::HCCHAR_EPNUM_SHIFT;
        if ep_in {
            hcchar |= regs::HCCHAR_EPDIR_IN;
        }
        hcchar |= ((ep_type as u32) & 0x3) << regs::HCCHAR_EPTYPE_SHIFT;
        hcchar |= 1 << 20; // MULTI_CNT = 1 (non-iso)
        hcchar |= ((dev_addr as u32) & 0x7F) << regs::HCCHAR_DEV_ADDR_SHIFT;
        // PER_ODD_FRAME — Synopsys PG §10.4 says this bit is
        // "valid only for periodic transfers and is not used for
        // non-periodic transfers", but Circle programs it on every
        // start anyway. Match polarity: set when CURRENT frame
        // number is odd (Circle dwhcidevice.cpp:994-1004).
        let frame_num = self.read(regs::HFNUM) & 0xFFFF;
        if frame_num & 1 != 0 {
            hcchar |= regs::HCCHAR_ODD_FRAME;
        }
        hcchar |= regs::HCCHAR_CHENA;
        self.write(regs::hcchar(CH), hcchar);

        // 6. Poll. Timeout has to be long enough for the slowest
        //    legitimate transaction but short enough that an
        //    unanswered interrupt-IN bails quickly (the touchscreen
        //    NAKs continuously when idle). 50 ms is comfortable for
        //    a 64-byte control transfer and short enough for the
        //    pump-cadence on idle.
        let freq: u64;
        let start: u64;
        // SAFETY: sysreg reads.
        unsafe {
            core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
                options(nomem, nostack, preserves_flags));
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) start,
                options(nomem, nostack, preserves_flags));
        }
        let deadline = start.wrapping_add(freq * 50 / 1000);
        // Error-bit classification:
        //
        //   Hard bus errors: AHB_ERR, XACT_ERR (after the core's
        //     own 3-NAK retry), BBL_ERR — log + TransactionError.
        //   Endpoint stall: STALL — return Stall.
        //   Soft "no data this poll": FRM_OVRUN (SOF mid-xfer is
        //     the *normal* periodic-IN idle response on a device
        //     that NAKs when nothing's ready), DATA_TGL_ERR
        //     (transient toggle drift on retries), and bare CHHLTD
        //     without XFER_COMPL (NAK absorbed by the core in DMA
        //     mode). Caller treats Timeout as "no data, retry next
        //     pump" — not logged because at the 16 ms pump cadence
        //     this would spam the console for every idle frame.
        let outcome = loop {
            let int = self.read(regs::hcint(CH));
            if int & regs::HCINT_STALL != 0 {
                break Err(UsbError::Stall);
            }
            if int
                & (regs::HCINT_AHBERR | regs::HCINT_XACT_ERR | regs::HCINT_BBL_ERR)
                != 0
            {
                kprintln!(
                    "dwc2: xfer err ch{}: dir={} pid={} addr={} ep={} len={} hcint={:#010x} hctsiz={:#010x} hcchar={:#010x} bus={:#x}",
                    CH,
                    if ep_in { "IN" } else { "OUT" },
                    pid as u32,
                    dev_addr, ep_num, len,
                    int,
                    self.read(regs::hctsiz(CH)),
                    self.read(regs::hcchar(CH)),
                    bus_addr,
                );
                break Err(UsbError::TransactionError);
            }
            if int & regs::HCINT_CHHLTD != 0 {
                if int & regs::HCINT_XFER_COMPL != 0 {
                    break Ok(());
                }
                // No data this poll (NAK / FRM_OVRUN / DATA_TGL_ERR).
                // Don't log — would spam every idle interrupt frame.
                break Err(UsbError::Timeout);
            }
            let now: u64;
            // SAFETY: sysreg read.
            unsafe {
                core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                    options(nomem, nostack, preserves_flags));
            }
            if now.wrapping_sub(deadline) as i64 >= 0 {
                break Err(UsbError::Timeout);
            }
        };

        // 7. Compute bytes actually moved before disabling the
        //    channel. HCTSIZ.XferSize decrements as bytes flow; the
        //    leftover is how many we asked for that didn't make it.
        let remaining = (self.read(regs::hctsiz(CH))) & 0x7_FFFF;
        let xferred = (len as u32).saturating_sub(remaining) as usize;

        // 8. Disable the channel for the next caller. ChDis without
        //    ChEna doesn't actually halt anything if it's already
        //    stopped; if it's running, we'd need to set both bits
        //    and wait for ChHltd. In our flow the channel halts
        //    itself on transfer end or error, so a plain disable is
        //    enough.
        self.modify(regs::hcchar(CH), regs::HCCHAR_CHENA, 0);

        // 9. Invalidate so the CPU sees DMA writes (relevant for IN).
        crate::arch::cpu::dc_civac_range(buf_pa, len.max(1));

        match outcome {
            Ok(()) => Ok(xferred),
            Err(e) => Err(e),
        }
    }
}

// ---- IRQ-driven interrupt-IN (touchscreen) --------------------------
//
// Polling the panel from the trap tail captured a report only when the
// guest happened to trap soon after it arrived; reports landing while
// the guest ran straight-line code (inker / recognizer) were dropped,
// breaking strokes. Instead `INT_CH` stays armed and the DWC2 raises a
// USB IRQ (BCM2835 GPU source 9) on each channel halt, harvested below
// from the trap-IRQ path the instant it fires.

/// GPU-bus uncached alias the DWC2 AHB master uses to view DRAM (see
/// `dma_xfer`'s note on HCDMA addressing).
const GPU_UNCACHED_BASE: u32 = 0xC000_0000;

/// 64-byte DMA landing buffer for the persistent interrupt-IN channel,
/// cache-line aligned so `dc_civac` maintenance is exact. The channel
/// DMAs each report here; `service_int_in` invalidates and copies it
/// out on the channel-halt IRQ.
#[repr(C, align(64))]
struct IntInBuf([u8; 64]);
static mut INT_IN_BUF: IntInBuf = IntInBuf([0; 64]);

#[allow(static_mut_refs)]
fn int_in_buf_pa() -> u64 {
    // SAFETY: a single fixed static; the DWC2 channel and the harvest
    // ISR are its only accessors, in one (interrupt) context.
    unsafe { core::ptr::addr_of!(INT_IN_BUF.0) as u64 }
}

impl Dwc2 {
    /// Switch host-channel interrupts to IRQ delivery and arm a
    /// persistent interrupt-IN on `INT_CH` for the touchscreen. After
    /// this the panel's ~16 ms reports arrive as USB IRQs instead of
    /// being polled. The caller is responsible for enabling BCM2835 GPU
    /// source 9 so the line reaches the CPU.
    pub fn start_int_in(&mut self, addr: u8, ep_addr: u8, mps: u16) -> UsbResult<()> {
        if !self.inited {
            return Err(UsbError::NotReady);
        }
        self.int_in_addr = addr;
        self.int_in_ep = ep_addr & 0x0F;
        self.int_in_mps = mps;

        // CHHLTD fires at the end of every transaction (NAK or data) —
        // our single re-arm point. Route INT_CH up to GINTSTS.HCINT,
        // unmask HCINT in the core, then enable global IRQ delivery
        // (init left it off for the polled path).
        self.write(regs::hcintmsk(INT_CH), regs::HCINT_CHHLTD);
        self.write(regs::HAINTMSK, 1 << INT_CH);
        self.modify(regs::GINTMSK, 0, regs::GINTSTS_HCINT);
        self.modify(regs::GAHBCFG, 0, regs::GAHBCFG_GLOBALINT_MASK);

        self.int_in_armed = true;
        self.arm_int_in();
        Ok(())
    }

    /// (Re-)enable `INT_CH` for one interrupt-IN transaction into
    /// `INT_IN_BUF`. The core runs it at the next (odd/even) frame and
    /// halts with CHHLTD on NAK or completion. Maintains the data
    /// toggle in `int_next_pid`.
    fn arm_int_in(&mut self) {
        let ch = INT_CH;
        let ep = self.int_in_ep as usize;
        let pid = self.int_next_pid[ep];
        let mps = self.int_in_mps;
        let buf_pa = int_in_buf_pa();

        // Force-disable if the channel didn't fully halt from its
        // previous use (mirrors dma_xfer's CHENA safeguard).
        let pre = self.read(regs::hcchar(ch));
        if pre & regs::HCCHAR_CHENA != 0 {
            self.write(
                regs::hcchar(ch),
                (pre & !regs::HCCHAR_CHENA) | regs::HCCHAR_CHDIS,
            );
            let _ = self.wait_for_bit(regs::hcint(ch), regs::HCINT_CHHLTD, true, 5);
        }

        crate::arch::cpu::dc_civac_range(buf_pa, mps.max(1) as usize);

        self.write(regs::hcint(ch), 0xFFFF_FFFF); // clear stale W1C bits
        self.write(regs::hcintmsk(ch), regs::HCINT_CHHLTD);
        self.write(regs::hcdma(ch), (buf_pa as u32) | GPU_UNCACHED_BASE);
        self.write(regs::hcsplt(ch), 0);

        let tsiz = (mps as u32 & 0x7_FFFF)
            | (1 << regs::HCTSIZ_PKTCNT_SHIFT)
            | ((pid as u32) << regs::HCTSIZ_PID_SHIFT);
        self.write(regs::hctsiz(ch), tsiz);

        let mut hcchar = (mps as u32) & regs::HCCHAR_MPS_MASK;
        hcchar |= ((self.int_in_ep as u32) & 0xF) << regs::HCCHAR_EPNUM_SHIFT;
        hcchar |= regs::HCCHAR_EPDIR_IN;
        hcchar |= (EpType::Interrupt as u32 & 0x3) << regs::HCCHAR_EPTYPE_SHIFT;
        hcchar |= 1 << 20; // MULTI_CNT = 1 (non-iso)
        hcchar |= ((self.int_in_addr as u32) & 0x7F) << regs::HCCHAR_DEV_ADDR_SHIFT;
        if self.read(regs::HFNUM) & 1 != 0 {
            hcchar |= regs::HCCHAR_ODD_FRAME;
        }
        hcchar |= regs::HCCHAR_CHENA;
        self.write(regs::hcchar(ch), hcchar);
    }

    /// Service a host-channel IRQ on `INT_CH`. Returns `Some(n)` with
    /// the bytes harvested into `out` when a report completed, else
    /// `None` (NAK / error / channel not halted). Always re-arms so the
    /// next report is captured. ISR context.
    fn service_int_in(&mut self, out: &mut [u8]) -> Option<usize> {
        let ch = INT_CH;
        let int = self.read(regs::hcint(ch));
        if int & regs::HCINT_CHHLTD == 0 {
            return None; // not our channel's halt
        }
        let ep = self.int_in_ep as usize;
        let mps = self.int_in_mps;

        let mut harvested = None;
        if int & regs::HCINT_XFER_COMPL != 0 {
            let remaining = self.read(regs::hctsiz(ch)) & 0x7_FFFF;
            let n = (mps as u32).saturating_sub(remaining) as usize;
            if n > 0 {
                let buf_pa = int_in_buf_pa();
                crate::arch::cpu::dc_civac_range(buf_pa, n);
                // SAFETY: INT_IN_BUF holds `n <= mps <= 64` valid bytes.
                let src = unsafe {
                    core::slice::from_raw_parts(buf_pa as *const u8, n.min(out.len()))
                };
                let take = src.len();
                out[..take].copy_from_slice(src);
                harvested = Some(take);
            }
            // Completed transaction → advance the data toggle.
            self.int_next_pid[ep] = match self.int_next_pid[ep] {
                Pid::Data0 => Pid::Data1,
                Pid::Data1 => Pid::Data0,
                other => other,
            };
        }
        // Clear the channel interrupt (W1C); GINTSTS.HCINT clears with
        // it. Errors (XACT/BBL/AHB/STALL) just fall through to a re-arm
        // — the panel recovers on its next frame.
        self.write(regs::hcint(ch), 0xFFFF_FFFF);
        self.arm_int_in();
        harvested
    }
}

/// Harvest a touchscreen report from the IRQ-driven interrupt-IN
/// channel, if one completed this IRQ. Trap-IRQ (ISR) context: never
/// blocks and never runs core init. Returns the bytes copied into
/// `out`, or `None` for a NAK / error / no-op.
pub fn service_int_in_irq(out: &mut [u8]) -> Option<usize> {
    // SAFETY: single-core EL2. In IRQ mode (after `start_int_in`) the
    // channel and this accessor are the only users of INSTANCE — the
    // touchscreen does not poll through `with`.
    let d = unsafe { &mut *INSTANCE.0.get() };
    if !d.inited || !d.int_in_armed {
        return None;
    }
    d.service_int_in(out)
}

/// 1 KiB DMA scratch buffer. Aligned to a cache line so cache
/// maintenance is conservative; sized to cover any single control
/// transfer we issue (configuration descriptor is the largest at
/// ~100 bytes on the MTouch panel, and a Report ID 1 interrupt-IN
/// frame is 56 bytes).
#[repr(C, align(64))]
struct ScratchBuf([u8; 1024]);
static mut SCRATCH: ScratchBuf = ScratchBuf([0; 1024]);

#[allow(static_mut_refs)]
fn scratch_buf() -> &'static mut [u8] {
    // SAFETY: dma_xfer is the single caller path; runs from the
    // trap-return tail on a single core; not re-entrant.
    unsafe { &mut SCRATCH.0 }
}

// ---- single global instance ----

struct Wrapper(core::cell::UnsafeCell<Dwc2>);
// SAFETY: single-core EL2; `with` is the single access point and is
// not re-entrant.
unsafe impl Sync for Wrapper {}

static INSTANCE: Wrapper = Wrapper(core::cell::UnsafeCell::new(Dwc2::new(DWC2_BASE)));
static INIT_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// Run `f` against the global DWC2 driver with `&mut self`. Lazily
/// runs `init` on first access; on init failure subsequent calls
/// return `UsbError::NotReady` without retrying (init isn't
/// idempotent across re-attempts — we'd need a separate "reset
/// state" path).
pub fn with<F, R>(f: F) -> UsbResult<R>
where
    F: FnOnce(&mut Dwc2) -> UsbResult<R>,
{
    // SAFETY: see `Wrapper` above.
    let dwc2 = unsafe { &mut *INSTANCE.0.get() };
    if !dwc2.inited && !INIT_ATTEMPTED.swap(true, Ordering::AcqRel) {
        if dwc2.init().is_err() {
            return Err(UsbError::NotReady);
        }
    }
    if !dwc2.inited {
        return Err(UsbError::NotReady);
    }
    f(dwc2)
}
