//! BCM2835 SDHOST controller driver (polled mode, no IRQ, no DMA).
//!
//! Targets the Raspberry Pi Zero 2 W's micro-SD slot, which on
//! BCM2710 routes to the **SDHOST** controller (NOT the SDHCI-style
//! Arasan EMMC block — that one is wired to the on-package
//! BCM43436B0 Wi-Fi/BT chip via SDIO on this SoC).
//!
//! Layering:
//! - Register access (`read`/`write`) — trivial volatile MMIO.
//! - Command execution (`send_cmd`) — bit-pack SDCMD, poll SDHSTS,
//!   read SDRSPx. Returns the response or an error code.
//! - Init / probe (`init`) — CMD0, CMD8, ACMD41, CMD2, CMD3, CMD9,
//!   CMD7. Determines whether the card is SDHC (block-addressed) or
//!   SDSC (byte-addressed) and stashes the RCA.
//! - Block I/O (`read_block` / `write_block`) — single-block PIO
//!   transfer via SDDATA FIFO.
//!
//! Ported from Circle's
//! [`addon/SDCard/sdhost.cpp`](https://github.com/rsta2/circle/blob/master/addon/SDCard/sdhost.cpp)
//! (P. Elwell @ RPi Trading, Rust port-by-hand). Constants live in
//! [`super::regs`].
//!
//! ## Bring-up status
//!
//! All of the driver is implemented and the binary should boot
//! through `SdHost::init` without panicking. None of it has been
//! exercised on real hardware yet — first real-silicon test will
//! confirm GPIO ALT routing, the mailbox-set core clock value, and
//! whether our SDCDIV math matches what the controller wants.
//! Likely first-failure modes:
//!
//! - CRC errors on the response or data → bus pulls wrong on
//!   `SD_CMD` / `SD_DAT0..3` (see [`gpio_setup`]).
//! - `CmdError::Timeout` on every command → SDCDIV too high (or
//!   the controller never received its core clock; check the
//!   mailbox response in [`clock_setup`]).
//! - `CmdError::HardwareWedge` on CMD0 → SDHOST MMIO not reachable
//!   (stage-1 doesn't map `0x3F20_2000` — but it should, via the
//!   raspi3b `DEVICE_MMIO_START..DEVICE_MMIO_END` window).

#![allow(dead_code)] // Reachable once the SDHOST bring-up is wired in.

use core::ptr::{read_volatile, write_volatile};

use super::regs::*;

/// Stage-by-stage trace, on only under the sd-probe feature. The
/// probe halts at the end of init so noise here doesn't matter, but
/// when sd-probe is off (i.e. the production hypervisor path once
/// SDHOST is wired into flash-persist) we don't want this output.
macro_rules! trace {
    ($($arg:tt)*) => {
        // Always type-checks (so referenced bindings don't go unused
        // when the feature is off); LLVM drops the branch when
        // `cfg!(feature = "sd-probe") == false`.
        if cfg!(feature = "sd-probe") {
            $crate::kprintln!($($arg)*);
        }
    };
}

/// SDHOST base on the BCM2710 peripheral window.
const SDHOST_BASE: usize = 0x3F20_2000;

/// Result of a single command execution.
#[derive(Debug, Clone, Copy)]
pub enum CmdError {
    /// `SDHSTS_CMD_TIME_OUT` — card didn't respond in the 1.6 ms window
    /// programmed via SDTOUT.
    Timeout,
    /// CRC7 mismatch on the response. Usually a bus / pull issue.
    CrcError,
    /// FIFO over- or under-run during a data-bearing command.
    FifoError,
    /// `SDHSTS_REW_TIME_OUT` during data phase.
    DataTimeout,
    /// SW timeout polling `SDCMD_NEW_FLAG` — we never observed the
    /// hardware accept the command.
    HardwareWedge,
}

/// Card-type signal we get from the OCR response to ACMD41.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardCapacity {
    /// SDSC — byte-addressed. CMD17 argument is a byte offset.
    StandardCapacity,
    /// SDHC / SDXC — block-addressed (512 B/block). CMD17 argument is
    /// a sector index.
    HighCapacity,
}

/// State the driver needs to keep between calls.
pub struct SdHost {
    rca: u32,
    capacity: CardCapacity,
}

impl SdHost {
    /// Bring up the controller and enumerate the card. Returns a
    /// driver instance ready for `read_block` / `write_block`.
    ///
    /// Untested on real hardware. See the module-level "Bring-up
    /// status" note for likely first-failure modes.
    pub fn init() -> Result<Self, CmdError> {
        trace!(
            "sd: pre-init SDEDM=0x{:08x} (FSM={:#x}) SDVDD=0x{:08x}",
            read_reg(SDEDM),
            read_reg(SDEDM) & SDEDM_FSM_MASK,
            read_reg(SDVDD),
        );

        gpio_setup();
        trace!("sd: gpio_setup done");

        let core_clock = match clock_setup() {
            Ok(r) => {
                trace!("sd: CLOCK_ID_CORE = {} Hz", r);
                r
            }
            Err(e) => {
                trace!("sd: clock_setup FAILED: {:?}", e);
                return Err(CmdError::HardwareWedge);
            }
        };

        reset_controller();
        delay_us(10_000);
        trace!("sd: reset done; SDEDM=0x{:08x}", read_reg(SDEDM));

        // Card power-up via SDVDD (1 = on).
        write_reg(SDVDD, 1);
        delay_us(10_000);
        trace!("sd: power-up; SDVDD readback={}", read_reg(SDVDD));

        // Default host config: relax CMD line, enable wide internal
        // bus (4-bit data path inside the controller; outside-bus
        // width is negotiated separately via ACMD6).
        write_reg(SDHCFG, SDHCFG_WIDE_INT_BUS | SDHCFG_REL_CMD_LINE);
        // Identification-phase clock: ≤400 kHz on the SD bus per the
        // SD spec. The SDHOST divides core_clock by (cdiv + 2).
        program_sdcdiv(core_clock, 400_000);
        write_reg(SDHSTS, SDHSTS_CLEAR_MASK);
        trace!(
            "sd: SDHCFG=0x{:08x} SDCDIV={} SDHSTS=0x{:08x}",
            read_reg(SDHCFG),
            read_reg(SDCDIV),
            read_reg(SDHSTS),
        );

        // Identification phase. Per SD Physical Layer Spec §4.2.
        trace!("sd: CMD0 GO_IDLE_STATE...");
        send_cmd(CMD_GO_IDLE_STATE, 0, ResponseKind::None)?;
        delay_us(1_000);
        trace!("sd: CMD0 ok");

        // CMD8: probe for SDv2 / supply-voltage match. A v1.x card
        // returns CMD_TIME_OUT here; that's not a fatal error.
        trace!("sd: CMD8 SEND_IF_COND...");
        let v2 = match send_cmd(CMD_SEND_IF_COND, CMD8_VHS_27_36_PATTERN, ResponseKind::Short) {
            Ok(resp) => {
                trace!("sd: CMD8 resp=0x{:08x}", resp);
                (resp & CMD8_R7_PATTERN_MASK) == CMD8_R7_PATTERN_VALUE
            }
            Err(CmdError::Timeout) => {
                trace!("sd: CMD8 timeout (treating as v1.x card)");
                false
            }
            Err(e) => return Err(e),
        };

        // ACMD41 loop until OCR_BUSY clears. Argument carries our
        // voltage window and (for v2 cards) the HCS bit.
        let arg = OCR_VOLT_3V2_3V4 | if v2 { OCR_HCS } else { 0 };
        trace!("sd: ACMD41 loop (arg=0x{:08x})...", arg);
        let mut acmd41_iter: u32 = 0;
        let ocr = loop {
            send_cmd(CMD_APP_CMD, 0, ResponseKind::Short)?;
            let resp = send_cmd(ACMD_SD_SEND_OP_COND, arg, ResponseKind::Short)?;
            acmd41_iter += 1;
            if resp & OCR_BUSY != 0 {
                trace!("sd: ACMD41 ready after {} iter, ocr=0x{:08x}", acmd41_iter, resp);
                break resp;
            }
            if acmd41_iter >= 1000 {
                trace!("sd: ACMD41 never ready ({} iters)", acmd41_iter);
                return Err(CmdError::HardwareWedge);
            }
            delay_us(10_000);
        };
        let capacity = if v2 && (ocr & OCR_CCS) != 0 {
            CardCapacity::HighCapacity
        } else {
            CardCapacity::StandardCapacity
        };

        // CMD2 — fetch CID (we don't decode it; just complete the
        // protocol step).
        trace!("sd: CMD2 ALL_SEND_CID...");
        send_cmd(CMD_ALL_SEND_CID, 0, ResponseKind::Long)?;
        // CMD3 — card returns its RCA in bits [31:16].
        trace!("sd: CMD3 SEND_RELATIVE_ADDR...");
        let rca = send_cmd(CMD_SEND_RELATIVE_ADDR, 0, ResponseKind::Short)? & 0xFFFF_0000;
        trace!("sd: RCA=0x{:08x}", rca);
        // CMD9 — CSD; again we don't decode it yet.
        trace!("sd: CMD9 SEND_CSD...");
        send_cmd(CMD_SEND_CSD, rca, ResponseKind::Long)?;
        // CMD7 — select the card, putting it in transfer state.
        trace!("sd: CMD7 SELECT_CARD...");
        send_cmd(CMD_SELECT_CARD, rca, ResponseKind::Short)?;

        // Set 512-byte block length for byte-addressed cards. SDHC
        // ignores CMD16 (always 512); send it anyway for uniformity.
        send_cmd(CMD_SET_BLOCKLEN, 512, ResponseKind::Short)?;

        // Card is in transfer state — bump the bus clock to the
        // default-speed 25 MHz target. The SD spec allows this
        // immediately after CMD7 (no SD switch command required for
        // default-speed mode).
        //
        // 4-bit bus width via ACMD6 is deliberately deferred until
        // single-bit reads are confirmed solid; CRC diagnosis is
        // easier without bus-width complications.
        program_sdcdiv(core_clock, 25_000_000);

        Ok(SdHost { rca, capacity })
    }

    /// Read one 512-byte sector. `lba` is a sector index regardless
    /// of card capacity — we translate to a byte offset for SDSC
    /// cards internally.
    pub fn read_block(&self, lba: u32, buf: &mut [u8; 512]) -> Result<(), CmdError> {
        let arg = match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        };
        write_reg(SDHBCT, 512);
        write_reg(SDHBLC, 1);
        send_cmd_kind(CMD_READ_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Read)?;
        drain_fifo_to(buf)
    }

    /// Write one 512-byte sector. See [`read_block`] for argument
    /// semantics.
    pub fn write_block(&self, lba: u32, buf: &[u8; 512]) -> Result<(), CmdError> {
        let arg = match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        };
        write_reg(SDHBCT, 512);
        write_reg(SDHBLC, 1);
        send_cmd_kind(CMD_WRITE_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Write)?;
        fill_fifo_from(buf)
    }

    pub fn capacity(&self) -> CardCapacity {
        self.capacity
    }

    pub fn rca(&self) -> u32 {
        self.rca
    }
}

// ---- Register helpers ------------------------------------------------

#[inline]
fn read_reg(off: usize) -> u32 {
    // SAFETY: SDHOST MMIO at fixed BCM2710 base; identity-mapped
    // Device-nGnRE by mmu::init via DEVICE_MMIO_START..DEVICE_MMIO_END.
    unsafe { read_volatile((SDHOST_BASE + off) as *const u32) }
}

#[inline]
fn write_reg(off: usize, val: u32) {
    // SAFETY: see read_reg.
    unsafe { write_volatile((SDHOST_BASE + off) as *mut u32, val) }
}

fn reset_controller() {
    // Power off, zero command/argument/timeout/divider, clear status,
    // program FIFO thresholds. Sequence taken from Circle's
    // `reset_internal()`.
    write_reg(SDVDD, 0);
    write_reg(SDCMD, 0);
    write_reg(SDARG, 0);
    // 1.6 ms timeout at the core clock the firmware leaves us with;
    // overwritten once `clock_setup` runs.
    write_reg(SDTOUT, 0xF00000);
    write_reg(SDCDIV, 0);
    write_reg(SDHSTS, SDHSTS_CLEAR_MASK);
    let edm = (FIFO_READ_THRESHOLD << SDEDM_READ_THRESHOLD_SHIFT)
        | (FIFO_WRITE_THRESHOLD << SDEDM_WRITE_THRESHOLD_SHIFT);
    write_reg(SDEDM, edm);
}

// ---- Command path ----------------------------------------------------

#[derive(Clone, Copy)]
enum ResponseKind {
    None,
    Short, // 48-bit; SDRSP0 holds bits [39:8] of the response token.
    Long,  // 136-bit; SDRSP0..3 hold the CID/CSD contents.
}

#[derive(Clone, Copy)]
enum CmdDir {
    NoData,
    Read,
    Write,
}

fn send_cmd(cmd: u8, arg: u32, kind: ResponseKind) -> Result<u32, CmdError> {
    send_cmd_kind(cmd, arg, kind, CmdDir::NoData)
}

fn send_cmd_kind(cmd: u8, arg: u32, kind: ResponseKind, dir: CmdDir) -> Result<u32, CmdError> {
    // Clear stale status from the previous transfer.
    write_reg(SDHSTS, SDHSTS_CLEAR_MASK);

    write_reg(SDARG, arg);
    let mut cmd_word: u32 = SDCMD_NEW_FLAG | (cmd as u32 & SDCMD_CMD_MASK);
    cmd_word |= match kind {
        ResponseKind::None => SDCMD_NO_RESPONSE,
        ResponseKind::Short => 0,
        ResponseKind::Long => SDCMD_LONG_RESPONSE,
    };
    cmd_word |= match dir {
        CmdDir::NoData => 0,
        CmdDir::Read => SDCMD_READ_CMD,
        CmdDir::Write => SDCMD_WRITE_CMD,
    };
    // Treat all R1b commands as busy-wait. We don't currently issue
    // any (CMD7/CMD12 are R1b; the only one we send is CMD7 during
    // init, which the hardware can wait for without us looking at the
    // BUSY line). Left as a TODO to revisit if a future caller needs
    // CMD12 mid-transfer.
    if matches!(cmd, CMD_SELECT_CARD | CMD_STOP_TRANSMISSION) {
        cmd_word |= SDCMD_BUSYWAIT;
    }
    write_reg(SDCMD, cmd_word);

    // Poll NEW_FLAG to drop. Bounded; if we sit here too long the
    // controller is wedged (clock not running, card not present,
    // bus floating).
    for _ in 0..1_000_000 {
        let c = read_reg(SDCMD);
        if (c & SDCMD_NEW_FLAG) == 0 {
            if (c & SDCMD_FAIL_FLAG) != 0 {
                let hsts = read_reg(SDHSTS);
                return Err(map_hsts_error(hsts));
            }
            return Ok(read_reg(SDRSP0));
        }
    }
    Err(CmdError::HardwareWedge)
}

fn map_hsts_error(hsts: u32) -> CmdError {
    if hsts & SDHSTS_CMD_TIME_OUT != 0 {
        CmdError::Timeout
    } else if hsts & SDHSTS_CRC7_ERROR != 0 || hsts & SDHSTS_CRC16_ERROR != 0 {
        CmdError::CrcError
    } else if hsts & SDHSTS_FIFO_ERROR != 0 {
        CmdError::FifoError
    } else if hsts & SDHSTS_REW_TIME_OUT != 0 {
        CmdError::DataTimeout
    } else {
        CmdError::HardwareWedge
    }
}

// ---- FIFO drain / fill ----------------------------------------------

/// Read 512 bytes (128 32-bit words) from the FIFO into `buf`.
///
/// We poll `SDHSTS_DATA_FLAG` per word rather than burst-checking
/// against the FIFO threshold; this is simpler and the loss vs. the
/// burst path is irrelevant for a once-per-snapshot flash write.
fn drain_fifo_to(buf: &mut [u8; 512]) -> Result<(), CmdError> {
    for word_ix in 0..128 {
        wait_for_data()?;
        let w = read_reg(SDDATA);
        let off = word_ix * 4;
        buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
    }
    finish_data_phase()
}

fn fill_fifo_from(buf: &[u8; 512]) -> Result<(), CmdError> {
    for word_ix in 0..128 {
        wait_for_fifo_space()?;
        let off = word_ix * 4;
        let w = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        write_reg(SDDATA, w);
    }
    finish_data_phase()
}

fn wait_for_data() -> Result<(), CmdError> {
    for _ in 0..2_000_000 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        if h & SDHSTS_DATA_FLAG != 0 {
            return Ok(());
        }
    }
    Err(CmdError::HardwareWedge)
}

fn wait_for_fifo_space() -> Result<(), CmdError> {
    // The SDHOST exposes "FIFO has space" via the same DATA_FLAG bit,
    // direction-dependent. For writes Circle treats it as a single
    // ready/not-ready signal; we follow that lead. If we observe
    // FIFO_ERROR in practice the threshold-aware path will replace
    // this.
    wait_for_data()
}

fn finish_data_phase() -> Result<(), CmdError> {
    // Wait for the block to land (BLOCK_IRPT) and clear the sticky
    // status flag so the next command starts clean.
    for _ in 0..2_000_000 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        if h & SDHSTS_BLOCK_IRPT != 0 {
            write_reg(SDHSTS, SDHSTS_BLOCK_IRPT);
            return Ok(());
        }
    }
    Err(CmdError::HardwareWedge)
}

// ---- GPIO pinmux -----------------------------------------------------

const GPIO_BASE: usize = 0x3F20_0000;
const GPFSEL5: *mut u32 = (GPIO_BASE + 0x14) as *mut u32; // pins 50–53 here, plus 54–57.
const GPFSEL4: *mut u32 = (GPIO_BASE + 0x10) as *mut u32; // pins 40–49 here (includes 48, 49).
const GPPUD: *mut u32 = (GPIO_BASE + 0x94) as *mut u32;
const GPPUDCLK1: *mut u32 = (GPIO_BASE + 0x9C) as *mut u32;

const GPIO_PULL_OFF: u32 = 0;
const GPIO_PULL_UP: u32 = 2;
const GPIO_ALT0: u32 = 0b100;

/// Route GPIO 48..53 to the SDHOST controller (ALT0) with
/// appropriate pulls.
///
/// On BCM2835/2710 the alternate-function table puts SDHOST signals
/// on GPIO 48..53 ALT0:
/// - GPIO 48: `SD_CLK_N`  — no pull (clock-only line).
/// - GPIO 49: `SD_CMD_N`  — pull-up.
/// - GPIO 50..53: `SD_DAT0..3` — pull-up.
///
/// We deliberately do **not** touch GPIO 34..39 here: those go to
/// the on-package WLAN/BT chip via the Arasan EMMC controller, and
/// firmware has already configured them. Re-driving them risks
/// dropping the Bluetooth wakeup path during a future
/// `dtoverlay=disable-bt`-less boot. Circle's driver does touch
/// them, but Circle is single-OS — it owns everything. We're
/// targeted at a hypervisor that should leave anything not directly
/// used alone.
fn gpio_setup() {
    // Function-select for GPIO 48 and 49 lives in GPFSEL4 at bit
    // offsets (48-40)*3 = 24 and (49-40)*3 = 27.
    // SAFETY: GPIO MMIO at fixed BCM2710 base.
    unsafe {
        let mut fsel4 = read_volatile(GPFSEL4);
        fsel4 &= !(0b111 << 24);
        fsel4 &= !(0b111 << 27);
        fsel4 |= GPIO_ALT0 << 24;
        fsel4 |= GPIO_ALT0 << 27;
        write_volatile(GPFSEL4, fsel4);

        // GPIO 50..53 in GPFSEL5 at offsets (50-50)*3, ..., (53-50)*3.
        let mut fsel5 = read_volatile(GPFSEL5);
        for pin in 50..=53 {
            let shift = ((pin - 50) * 3) as u32;
            fsel5 &= !(0b111 << shift);
            fsel5 |= GPIO_ALT0 << shift;
        }
        write_volatile(GPFSEL5, fsel5);
    }

    // Pull config: BCM2835 mechanism (still works on BCM2710; Pi 4's
    // BCM2711 changed this — irrelevant for the Zero 2 W).
    //
    // Sequence per the ARM Peripherals manual:
    //   1. Write to GPPUD to set the required control signal.
    //   2. Wait 150 cycles — for the control signal to settle.
    //   3. Write to GPPUDCLK0/1 to clock the control signal into the
    //      GPIO pads we care about.
    //   4. Wait another 150 cycles.
    //   5. Write 0 to GPPUD to remove the control signal.
    //   6. Write 0 to GPPUDCLK0/1 to remove the clock.
    gpio_set_pull(1 << (48 - 32), GPIO_PULL_OFF);
    gpio_set_pull(
        (1 << (49 - 32)) | (1 << (50 - 32)) | (1 << (51 - 32)) | (1 << (52 - 32)) | (1 << (53 - 32)),
        GPIO_PULL_UP,
    );
}

fn gpio_set_pull(pin_mask_in_high_bank: u32, mode: u32) {
    // SAFETY: GPIO MMIO; `delay_cycles` provides the mandated 150-
    // cycle settle windows.
    unsafe {
        write_volatile(GPPUD, mode);
        delay_cycles(200);
        write_volatile(GPPUDCLK1, pin_mask_in_high_bank);
        delay_cycles(200);
        write_volatile(GPPUD, 0);
        write_volatile(GPPUDCLK1, 0);
    }
}

#[inline]
fn delay_cycles(n: u32) {
    for _ in 0..n {
        unsafe { core::arch::asm!("nop") }
    }
}

// ---- Clock setup via VC mailbox -------------------------------------

/// Query (and pin) the SoC core clock the SDHOST is hung off of.
/// Returns the rate in Hz.
///
/// We don't change the core clock rate — that would knock several
/// other peripherals around. We just read what firmware has set and
/// derive SDCDIV from it.
fn clock_setup() -> Result<u32, crate::mailbox::MailboxError> {
    let rate = crate::mailbox::get_clock_rate(crate::mailbox::CLOCK_ID_CORE)?;
    // The cut-down GPU firmware on the Zero 2 W typically reports
    // 250 MHz here. If we see something outside a sane window the
    // mailbox response is likely garbled — fail loudly rather than
    // dial in a wrong divider.
    if !(50_000_000..=600_000_000).contains(&rate) {
        return Err(crate::mailbox::MailboxError::FirmwareError);
    }
    Ok(rate)
}

/// Program SDCDIV so that `core_clock / (cdiv + 2)` is at most
/// `target_hz`. SDHOST's effective SD-bus clock is
/// `core_clock / (SDCDIV + 2)`; rounding up keeps us under the
/// target (the SD spec ceilings are inclusive).
fn program_sdcdiv(core_clock: u32, target_hz: u32) {
    let div = ((core_clock + target_hz - 1) / target_hz).saturating_sub(2);
    let cdiv = core::cmp::min(div, SDCDIV_MAX_CDIV);
    write_reg(SDCDIV, cdiv);
}

fn delay_us(us: u32) {
    // Coarse busy loop. Refined once we have a real-silicon timer
    // reference; CNTPCT runs at 19.2 MHz on the Zero 2 W so a "rough"
    // loop count of ~50 per microsecond is a placeholder, not a
    // calibrated delay.
    let iters = us.saturating_mul(50);
    for _ in 0..iters {
        unsafe { core::arch::asm!("nop") }
    }
}
