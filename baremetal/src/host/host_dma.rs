//! BCM2835 DMA controller — minimum slice needed to feed PL011 TX
//! without busy-waiting on the FIFO.
//!
//! Only compiled into real-hardware Pi builds (`nh_real_hw`). On the
//! default QEMU build, console output goes
//! through Arm Semihosting, which already doesn't block on a peripheral
//! FIFO, so there's no need to model DMA there.
//!
//! References (verbatim against the Broadcom datasheet):
//! - DMA register map, CB layout, TI/CS bit fields, DREQ table:
//!   BCM2835 ARM Peripherals (2012-02-06), §4.2.1 pp.39–62.
//! - ARM interrupt controller register map and IRQ source numbering:
//!   BCM2835 ARM Peripherals §7.5 pp.112–117. Source numbers for DMA
//!   channels (the rows Broadcom's table leaves blank) cross-checked
//!   against rsta2/circle `include/circle/bcm2835int.h` (ARM_IRQ_DMA0
//!   = 16, …, ARM_IRQ_DMA11 = 27).
//! - Bus vs ARM physical address aliases: BCM2835 §1.2.3–1.2.4 pp.6–7.
//!   RAM bus alias = `arm_phys | 0xC000_0000`; peripheral bus alias =
//!   `(arm_phys & 0x00FF_FFFF) | 0x7E00_0000`.

#![cfg(nh_real_hw)]

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

// ---- DMA controller register map (BCM2835 §4.2.1, pp.39–47) ---------

/// DMA controller, ARM physical (peripheral bus 0x7E00_7000 ↔ Pi 3 /
/// Zero 2 W peripheral phys base 0x3F00_0000).
const DMA_BASE: usize = 0x3F00_7000;

/// Per-channel block stride. Channels 0..14 occupy 0x100 each
/// (BCM2835 §4.2.1 Table 4-1 p.40).
const CHAN_STRIDE: usize = 0x100;

/// Register offsets within a channel block (BCM2835 §4.2.1.2 p.41).
const REG_CS: usize = 0x00;
const REG_CONBLK_AD: usize = 0x04;
#[cfg(nh_audio_pi_hdmi)]
const REG_DEBUG: usize = 0x20;

/// Global DMA registers (BCM2835 §4.2.1.2 p.46).
const REG_INT_STATUS: usize = 0xFE0;
const REG_ENABLE: usize = 0xFF0;

// ---- CS register bits (BCM2835 §4.2.1 pp.47–48) ---------------------

pub const CS_RESET: u32 = 1 << 31;
pub const CS_ERROR: u32 = 1 << 8;
pub const CS_INT: u32 = 1 << 2;
pub const CS_END: u32 = 1 << 1;
pub const CS_ACTIVE: u32 = 1 << 0;
pub const CS_PRIORITY_SHIFT: u32 = 16;
pub const CS_PANIC_PRIORITY_SHIFT: u32 = 20;

// ---- TI register bits (BCM2835 §4.2.1 pp.50–51) ---------------------

pub const TI_PERMAP_SHIFT: u32 = 16;
pub const TI_SRC_INC: u32 = 1 << 8;
pub const TI_DEST_DREQ: u32 = 1 << 6;
pub const TI_WAIT_RESP: u32 = 1 << 3;
pub const TI_INTEN: u32 = 1 << 0;

// ---- DREQ peripheral mapping (BCM2835 §4.2.1.3 p.61) ----------------

pub const DREQ_UART_TX: u32 = 12;
/// HDMI MAI write side. Per BCM2835 §4.2.1.3 p.61 the value for the
/// BCM2835/2836/2837 (Pi 0/2/3) is 17; Circle's TDREQ enum agrees
/// (`DREQSourceHDMI = 17` for RASPPI <= 3 in
/// `include/circle/dmacommon.h`). On Pi 4 (RASPPI >= 4) the value
/// changes to 10, but this hypervisor doesn't target Pi 4.
#[cfg(nh_audio_pi_hdmi)]
pub const DREQ_HDMI: u32 = 17;
/// BCM2835 SDHOST FIFO (`SDDATA`). Cross-checked two ways: Linux's
/// device tree has `sdhost { dmas = <&dma 13>; }`
/// (`bcm2835-common.dtsi`), and Circle's `TDREQ` leaves 13 between
/// `DREQSourceUARTTX = 12` and `DREQSourceUARTRX = 14`.
pub const DREQ_SDHOST: u32 = 13;

// ---- Control block (BCM2835 §4.2.1.1 p.40 — 8 × 32-bit words,
// 256-bit aligned) -----------------------------------------------------

#[repr(C, align(32))]
pub struct DmaCb {
    pub ti: u32,
    pub source_ad: u32,
    pub dest_ad: u32,
    pub txfr_len: u32,
    pub stride: u32,
    pub nextconbk: u32,
    pub _reserved: [u32; 2],
}

impl DmaCb {
    pub const fn zero() -> Self {
        Self {
            ti: 0,
            source_ad: 0,
            dest_ad: 0,
            txfr_len: 0,
            stride: 0,
            nextconbk: 0,
            _reserved: [0; 2],
        }
    }
}

// ---- Address translation (BCM2835 §1.2.3–1.2.4 pp.6–7) --------------

/// ARM physical RAM address → DMA bus address (uncached alias). ARM
/// RAM on Pi Zero 2 W is below 0x4000_0000, so the OR is unambiguous.
#[inline]
pub fn bus_addr_ram(arm_phys: u64) -> u32 {
    (arm_phys as u32) | 0xC000_0000
}

/// ARM physical peripheral address → DMA bus address. Peripheral PA
/// base on Pi 3 / Zero 2 W is 0x3F00_0000, bus base is 0x7E00_0000.
#[inline]
pub fn bus_addr_periph(arm_phys: u32) -> u32 {
    (arm_phys & 0x00FF_FFFF) | 0x7E00_0000
}

// ---- Channel access -------------------------------------------------

#[inline]
fn chan_reg(ch: u32, off: usize) -> *mut u32 {
    (DMA_BASE + (ch as usize) * CHAN_STRIDE + off) as *mut u32
}

#[inline]
fn read_enable() -> u32 {
    // SAFETY: MMIO read at a fixed peripheral address; Device-nGnRE
    // mapped by mmu::init.
    unsafe { read_volatile((DMA_BASE + REG_ENABLE) as *const u32) }
}

#[inline]
fn read_int_status() -> u32 {
    // SAFETY: MMIO read at a fixed peripheral address.
    unsafe { read_volatile((DMA_BASE + REG_INT_STATUS) as *const u32) }
}

#[inline]
fn read_cs(ch: u32) -> u32 {
    // SAFETY: MMIO read at a fixed peripheral address.
    unsafe { read_volatile(chan_reg(ch, REG_CS) as *const u32) }
}

#[inline]
fn write_cs(ch: u32, v: u32) {
    // SAFETY: MMIO write at a fixed peripheral address.
    unsafe { write_volatile(chan_reg(ch, REG_CS), v) }
}

#[inline]
fn write_conblk_ad(ch: u32, cb_bus: u32) {
    // SAFETY: MMIO write at a fixed peripheral address.
    unsafe { write_volatile(chan_reg(ch, REG_CONBLK_AD), cb_bus) }
}

#[cfg(nh_audio_pi_hdmi)]
#[inline]
fn read_debug(ch: u32) -> u32 {
    // SAFETY: MMIO read at a fixed peripheral address.
    unsafe { read_volatile(chan_reg(ch, REG_DEBUG) as *const u32) }
}

// ---- TX completion hook ---------------------------------------------
//
// The uart layer registers itself implicitly: it owns channel 5, so
// when `on_completion(5)` fires we forward to it. Keeping the
// indirection trivial (one hard-coded channel, one callback) avoids
// the function-pointer table you'd want for a real subsystem.

/// The DMA channel console::tx_dma owns. Channel 5 is conventionally free
/// on Pi 3 / Zero 2 W; we assert at init() that firmware has powered
/// it on.
pub const UART_TX_CHANNEL: u32 = 5;

/// The DMA channel that audio::pi_hdmi owns for HDMI MAI feed.
/// Channel 4 is conventionally free on Pi 3 / Zero 2 W; firmware
/// reservations typically touch 0, 2, 3.
pub const MAI_TX_CHANNEL: u32 = 4;

/// The DMA channel the SDHOST flash-autosave owns for block writes.
/// Channel 6 is conventionally free on Pi 3 / Zero 2 W (firmware
/// reservations touch 0/2/3; 4 and 5 are MAI / UART).
pub const SD_TX_CHANNEL: u32 = 6;

/// Per-channel CS flag bits (priority / panic-priority / wait-for-
/// writes / dis-debug) ORed into both the arm-time and ACK-time
/// writes. Mirrors Linux's `BCM2835_DMA_CS_FLAGS(dreq)` pattern,
/// where the consumer encodes its AXI-arbitration preferences via
/// the DT dma-cell and the driver carries them into every CS write.
///
/// UART = 0: matches Linux's PL011 DT entry `dmas = <&dma 12>` (bare
/// DREQ, no high bits).
///
/// MAI = `PRIORITY(8) | PANIC_PRIORITY(15)`: diverges from Linux on
/// purpose; see [`arm_mai_tx`] for the rationale.
const UART_TX_CS_FLAGS: u32 = 0;
const MAI_TX_CS_FLAGS: u32 =
    (8u32 << CS_PRIORITY_SHIFT) | (15u32 << CS_PANIC_PRIORITY_SHIFT);
/// SD block-write: bare DREQ like UART (Linux's sdhost DT cookie is
/// `dmas = <&dma 13>`, no flag bits). Audio (MAI) is deliberately kept
/// at higher AXI priority, so SD writes yield to it under contention —
/// which is exactly the point of moving the save off the CPU.
const SD_TX_CS_FLAGS: u32 = 0;

/// Set true once UART TX init() succeeds. `arm_uart_tx()` and the
/// uart-side completion dispatch are gated on this so the uart layer
/// can call into us unconditionally.
static READY: AtomicBool = AtomicBool::new(false);

/// Set true once MAI TX init_mai_tx() succeeds. Same role for the
/// audio backend.
#[cfg(nh_audio_pi_hdmi)]
static MAI_READY: AtomicBool = AtomicBool::new(false);

/// Set true once SD TX init_sd_tx() succeeds.
static SD_READY: AtomicBool = AtomicBool::new(false);

/// Bring up one DMA channel: assert firmware has powered it, reset
/// it, clear stale END/INT, enable its GPU IRQ. Used by both
/// `init` (UART) and `init_mai_tx` (MAI). Returns true on success.
fn init_channel(ch: u32) -> bool {
    let enable = read_enable();
    if (enable >> ch) & 1 == 0 {
        return false;
    }
    // CS_RESET self-clears; wait for it to drop. Worst case a handful
    // of MMIO read cycles per the datasheet.
    write_cs(ch, CS_RESET);
    for _ in 0..1000 {
        if read_cs(ch) & CS_RESET == 0 {
            break;
        }
    }
    // Clear any latched INT/END/ERROR bits from a prior owner.
    write_cs(ch, CS_INT | CS_END);
    // Wire the channel's completion IRQ at the BCM2835 IRQ
    // controller. DMA channel N → GPU IRQ source 16+N (Circle's
    // ARM_IRQ_DMA0 = 16). trap_irq's additive dispatch picks it up
    // from IRQ_PEND_1 on the next CPU IRQ.
    crate::host::platform::enable_bcm2835_irq(16 + ch);
    true
}

/// One-time bring-up of the UART-TX channel. Idempotent.
pub fn init() -> bool {
    if READY.load(Ordering::Acquire) {
        return true;
    }
    if !init_channel(UART_TX_CHANNEL) {
        return false;
    }
    READY.store(true, Ordering::Release);
    true
}

/// One-time bring-up of the MAI-TX channel. Idempotent. Must run
/// after `mmu::init` (see `console::init` for the AArch64/Cortex-A53
/// LDXR-on-non-cacheable rationale).
#[cfg(nh_audio_pi_hdmi)]
pub fn init_mai_tx() -> bool {
    if MAI_READY.load(Ordering::Acquire) {
        return true;
    }
    if !init_channel(MAI_TX_CHANNEL) {
        return false;
    }
    MAI_READY.store(true, Ordering::Release);
    true
}

/// Returns whether `init()` has completed successfully.
#[inline]
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Returns whether `init_mai_tx()` has completed successfully.
#[cfg(nh_audio_pi_hdmi)]
#[inline]
pub fn is_mai_ready() -> bool {
    MAI_READY.load(Ordering::Acquire)
}

/// One-time bring-up of the SD-TX channel. Idempotent. Returns false
/// if firmware hasn't powered channel `SD_TX_CHANNEL` (the caller then
/// falls back to PIO).
pub fn init_sd_tx() -> bool {
    if SD_READY.load(Ordering::Acquire) {
        return true;
    }
    if !init_channel(SD_TX_CHANNEL) {
        return false;
    }
    SD_READY.store(true, Ordering::Release);
    true
}

/// True while the SD-TX channel is still running a transfer (CS.ACTIVE
/// set). The polled `write_block_dma` spins on this; the future
/// background save will instead take the channel's completion IRQ.
#[inline]
pub fn sd_tx_active() -> bool {
    read_cs(SD_TX_CHANNEL) & CS_ACTIVE != 0
}

/// True if the SD-TX channel latched a DMA error (CS.ERROR) on its
/// last transfer. Checked by `write_block_dma` after completion.
#[inline]
pub fn sd_tx_error() -> bool {
    read_cs(SD_TX_CHANNEL) & CS_ERROR != 0
}

/// Arm a DMA transfer on `ch` with caller-supplied CS bits. The CB
/// must already describe a transfer; the caller is responsible for
/// cache-cleaning the source range before calling.
///
/// We `dc civac` the CB itself here: the controller reads CB through
/// the uncached bus alias (BCM2835 §1.2.3), so any CB writes the
/// caller made via the cacheable ARM mapping must be pushed to RAM
/// first.
///
/// SAFETY: `cb` must be a `'static`-lived DmaCb whose contents remain
/// stable for the duration of the transfer (until `on_completion`
/// fires for this channel). The caller has exclusive use of the
/// channel.
unsafe fn arm_with_cs(ch: u32, cb: &DmaCb, cs: u32) {
    let cb_arm_phys = cb as *const DmaCb as u64;
    crate::arch::cpu::dc_civac_range(cb_arm_phys, core::mem::size_of::<DmaCb>());
    let cb_bus = bus_addr_ram(cb_arm_phys);
    // Match Linux's `bcm2835_dma_start_desc` byte-for-byte: three
    // writes — CS=RESET (BIT(31), self-clearing) → CONBLK_AD →
    // CS=ACTIVE|FLAGS. The pre-arm reset ensures any stale half-
    // configured channel state from a prior arm is cleared before
    // the new CB is loaded.
    write_cs(ch, CS_RESET);
    write_conblk_ad(ch, cb_bus);
    write_cs(ch, cs);
}

/// Arm a DMA transfer on the UART TX channel.
///
/// Matches Linux's `bcm2835_dma_start_desc` for the PL011 audio path:
/// `BCM2835_DMA_CS_FLAGS(dreq)` extracts priority/wait-for-writes/
/// dis-debug bits from the DT dma-cookie, and the Pi DT entry for
/// PL011 is `dmas = <&dma 12>` — a bare DREQ number with no flag
/// bits set — so `CS_FLAGS` evaluates to 0 and the write is just
/// `ACTIVE`. Circle's `priority 1 + WAIT_FOR_OUTSTANDING_WRITES`
/// pattern instead creates an AXI-arbitration imbalance that lets UART
/// DMA bursts perturb the concurrent HDMI MAI feed (audible glitch
/// correlated with each flash-persist load dot).
///
/// SAFETY: see [`arm_with_cs`].
pub unsafe fn arm_uart_tx(cb: &DmaCb) {
    // SAFETY: caller's invariant matches `arm_with_cs`'s.
    unsafe { arm_with_cs(UART_TX_CHANNEL, cb, CS_ACTIVE | UART_TX_CS_FLAGS) }
}

/// Arm a DMA transfer on the MAI TX channel.
///
/// JUSTIFIED divergence from Linux's `bcm2835_dma_start_desc` (which
/// writes just `ACTIVE`, since the vc4_hdmi DT cookie `dmas = <&dma 17>`
/// has no priority bits set): we raise AXI priority + panic_priority
/// because our usage pattern differs from Linux's. Linux runs the
/// HDMI MAI DMA intermittently — only while ALSA has data to stream
/// — so DMA-controller arbitration with concurrent UART TX / EMMC
/// reads never builds enough pressure to underrun the MAI FIFO.
/// We feed MAI continuously (real audio when Newton is playing, a
/// silence/tone fill otherwise) to keep the HDMI link's audio
/// channel from renegotiating, and during heavy EL2 I/O (flash_sd
/// restore = SD reads + UART log spam) the resulting sustained
/// contention DOES underrun the FIFO. Promoting MAI to AXI
/// priority 8 + panic priority 15 keeps the FIFO ahead through
/// those bursts.
///
/// SAFETY: see [`arm_with_cs`].
#[cfg(nh_audio_pi_hdmi)]
pub unsafe fn arm_mai_tx(cb: &DmaCb) {
    // SAFETY: caller's invariant matches `arm_with_cs`'s.
    unsafe { arm_with_cs(MAI_TX_CHANNEL, cb, CS_ACTIVE | MAI_TX_CS_FLAGS) }
}

/// Arm a DMA transfer on the SD-TX channel (RAM → `SDDATA` FIFO,
/// DREQ-paced). Bare DREQ flags, mirroring Linux's sdhost DT cookie.
///
/// SAFETY: see [`arm_with_cs`].
pub unsafe fn arm_sd_tx(cb: &DmaCb) {
    // SAFETY: caller's invariant matches `arm_with_cs`'s.
    unsafe { arm_with_cs(SD_TX_CHANNEL, cb, CS_ACTIVE | SD_TX_CS_FLAGS) }
}

/// Tear down the SD-TX channel (CS.RESET) after a failed or aborted
/// transfer so a half-armed channel can't linger.
pub fn sd_tx_abort() {
    write_cs(SD_TX_CHANNEL, CS_RESET);
}

/// Called from `trap_irq` when the BCM2835 IRQ controller reports a
/// pending DMA completion on `ch`. Acks the channel's INT/END bits
/// and dispatches to the registered consumer.
pub fn on_completion(ch: u32) {
    // Match Linux's `bcm2835_dma_callback` ACK shape:
    //   writel(BCM2835_DMA_INT | BCM2835_DMA_ACTIVE | CS_FLAGS(dreq),
    //          chan_base + CS);
    // — INT to W1C the IRQ, ACTIVE to keep the cyclic chain running,
    // plus the per-channel CS_FLAGS so priority bits aren't clobbered
    // on every IRQ. (Without re-asserting CS_FLAGS, our MAI priority
    // promotion would only last one period.)
    match ch {
        UART_TX_CHANNEL => {
            write_cs(ch, CS_INT | CS_ACTIVE | UART_TX_CS_FLAGS);
            crate::host::console::on_tx_done();
        }
        MAI_TX_CHANNEL => {
            write_cs(ch, CS_INT | CS_ACTIVE | MAI_TX_CS_FLAGS);
            crate::host::audio::on_mai_dma_done();
        }
        SD_TX_CHANNEL => {
            // One-shot block write, not a cyclic chain — ack the latched
            // INT/END without re-asserting ACTIVE (no next CB to run).
            // The next save's `arm_sd_tx` issues CS_RESET first anyway.
            write_cs(ch, CS_INT | CS_END);
            crate::host::flash_persist::on_sd_dma_done();
        }
        _ => {
            write_cs(ch, CS_INT | CS_END);
        }
    }
}

/// Returns `true` if the UART-TX channel currently has its INT bit
/// asserted in the global INT_STATUS register. Used so the uart
/// layer can poll for completion even before the BCM2835 IRQ
/// controller is wired (and as a safety net if a completion IRQ is
/// somehow lost).
#[inline]
pub fn uart_tx_pending() -> bool {
    is_ready() && (read_int_status() & (1 << UART_TX_CHANNEL)) != 0
}

/// Hardware self-check for the DMA CB-chain arming machinery, for the
/// `sd-probe` validation route (QEMU's BCM2835 DMA model can't
/// exercise `host_dma` — see `guest-tests/tests/MANIFEST`). Arms a
/// single RAM→RAM control block on `SD_TX_CHANNEL` (no DREQ pacing,
/// so the controller runs it to completion immediately) and verifies:
///   1. `init_sd_tx` brought the channel up (firmware powered it),
///   2. `arm_with_cs` loaded CONBLK_AD and the controller walked the CB
///      (CS.ACTIVE asserts then clears, CS.END latches),
///   3. the bytes actually moved (dest == src after completion),
///   4. no CS.ERROR latched.
/// This covers the same `init_channel` + `arm_with_cs` + bus-address
/// translation + CB cache-clean + completion path the DREQ-paced
/// UART/SD arms use; the only piece it can't self-contain is the
/// peripheral-FIFO DREQ feed, which the probe validates implicitly by
/// the fact that its own console output reaches the wire through the
/// real `arm_uart_tx` path. Returns `Ok(())` on success.
///
/// The check MUST NOT run on `UART_TX_CHANNEL`: by probe time the
/// console kprintln backend is already feeding that channel
/// (`console::init_dma_tx` runs before `sd::probe::run`), and
/// `arm_with_cs`'s pre-arm CS_RESET would kill an in-flight console
/// transfer and eat the CS_INT/END completion the uart ring polls for
/// — wedging all subsequent console output. `SD_TX_CHANNEL` is idle
/// until the probe's own `write_block_dma` later in the run, and is
/// the channel whose arming the probe exists to validate anyway.
#[cfg(feature = "sd-probe")]
pub fn sd_tx_dma_selfcheck() -> Result<(), &'static str> {
    use core::ptr::addr_of_mut;

    if !init_sd_tx() {
        return Err("SD-TX channel not powered by firmware");
    }

    // Distinct, cache-line-aligned src/dst so the cache-clean spans
    // exactly the buffers we DMA.
    #[repr(C, align(64))]
    struct Buf([u32; 4]);
    static mut SRC: Buf = Buf([0xDEAD_0001, 0xBEEF_0002, 0xF00D_0003, 0xCAFE_0004]);
    static mut DST: Buf = Buf([0; 4]);
    static mut CB: DmaCb = DmaCb::zero();

    // BCM2835 §4.2.1 p.50: TI.DEST_INC is bit 4 (increment the
    // destination address per beat — needed for a RAM→RAM copy, where
    // neither side is a fixed FIFO).
    const TI_DEST_INC: u32 = 1 << 4;

    // SAFETY: single-core EL2 probe, exclusive use of these statics and
    // the channel; the buffers outlive the (immediate, unpaced) transfer.
    unsafe {
        let src_phys = addr_of_mut!(SRC) as u64;
        let dst_phys = addr_of_mut!(DST) as u64;
        for d in (*addr_of_mut!(DST)).0.iter_mut() {
            *d = 0;
        }
        // Clean both buffers: the controller reads/writes via the
        // uncached 0xC000_0000 bus alias.
        crate::arch::cpu::dc_civac_range(src_phys, core::mem::size_of::<Buf>());
        crate::arch::cpu::dc_civac_range(dst_phys, core::mem::size_of::<Buf>());

        let cb = &mut *addr_of_mut!(CB);
        cb.ti = TI_SRC_INC | TI_DEST_INC | TI_WAIT_RESP | TI_INTEN;
        cb.source_ad = bus_addr_ram(src_phys);
        cb.dest_ad = bus_addr_ram(dst_phys);
        cb.txfr_len = core::mem::size_of::<Buf>() as u32;
        cb.stride = 0;
        cb.nextconbk = 0;

        arm_with_cs(SD_TX_CHANNEL, &*addr_of_mut!(CB), CS_ACTIVE | SD_TX_CS_FLAGS);

        // RAM→RAM with no DREQ completes in a handful of cycles; spin a
        // bounded number of MMIO reads on CS.ACTIVE.
        let mut spun = 0u32;
        while read_cs(SD_TX_CHANNEL) & CS_ACTIVE != 0 {
            spun += 1;
            if spun > 1_000_000 {
                return Err("SD-TX DMA never cleared CS.ACTIVE");
            }
        }
        let cs = read_cs(SD_TX_CHANNEL);
        // Ack the latched INT/END so the channel is clean for the real
        // SD write path that follows.
        write_cs(SD_TX_CHANNEL, CS_INT | CS_END);

        if cs & CS_ERROR != 0 {
            return Err("SD-TX DMA latched CS.ERROR");
        }
        if cs & CS_END == 0 {
            return Err("SD-TX DMA did not set CS.END");
        }
        // Invalidate dst in cache before reading it back (the DMA wrote
        // it via the bus alias, bypassing our caches).
        crate::arch::cpu::dc_civac_range(dst_phys, core::mem::size_of::<Buf>());
        let src_vals = (*addr_of_mut!(SRC)).0;
        let dst_vals = (*addr_of_mut!(DST)).0;
        if src_vals != dst_vals {
            return Err("SD-TX DMA copied wrong bytes");
        }
    }
    Ok(())
}

/// Snapshot for diagnostic logging — CS / DEBUG of the MAI TX channel.
#[cfg(nh_audio_pi_hdmi)]
pub fn mai_tx_diag() -> (u32, u32) {
    (read_cs(MAI_TX_CHANNEL), read_debug(MAI_TX_CHANNEL))
}

/// Bus address of the control block the MAI TX channel is currently
/// executing (CONBLK_AD). With the cyclic one-CB-per-period chain
/// this identifies the period the DMA is reading *right now*,
/// independent of how many completion IRQs were actually dispatched
/// — the ground truth the IRQ-counted consumer estimate is checked
/// against.
#[cfg(nh_audio_pi_hdmi)]
pub fn mai_tx_conblk() -> u32 {
    // SAFETY: MMIO read in the Device-nGnRE window.
    unsafe { read_volatile(chan_reg(MAI_TX_CHANNEL, REG_CONBLK_AD)) }
}
