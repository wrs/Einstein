//! BCM2835 DMA controller — minimum slice needed to feed PL011 TX
//! without busy-waiting on the FIFO.
//!
//! Only compiled into real-hardware Pi builds (`no-semihost` +
//! `platform-raspi3b`). On the default QEMU build, console output goes
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

#![cfg(all(feature = "no-semihost", feature = "platform-raspi3b"))]

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
const REG_DEBUG: usize = 0x20;

/// Global DMA registers (BCM2835 §4.2.1.2 p.46).
const REG_INT_STATUS: usize = 0xFE0;
const REG_ENABLE: usize = 0xFF0;

// ---- CS register bits (BCM2835 §4.2.1 pp.47–48) ---------------------

pub const CS_RESET: u32 = 1 << 31;
pub const CS_WAIT_FOR_OUTSTANDING_WRITES: u32 = 1 << 28;
pub const CS_ERROR: u32 = 1 << 8;
pub const CS_INT: u32 = 1 << 2;
pub const CS_END: u32 = 1 << 1;
pub const CS_ACTIVE: u32 = 1 << 0;
pub const CS_PRIORITY_SHIFT: u32 = 16;

// ---- TI register bits (BCM2835 §4.2.1 pp.50–51) ---------------------

pub const TI_PERMAP_SHIFT: u32 = 16;
pub const TI_SRC_INC: u32 = 1 << 8;
pub const TI_DEST_DREQ: u32 = 1 << 6;
pub const TI_WAIT_RESP: u32 = 1 << 3;
pub const TI_INTEN: u32 = 1 << 0;

// ---- DREQ peripheral mapping (BCM2835 §4.2.1.3 p.61) ----------------

pub const DREQ_UART_TX: u32 = 12;

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

/// The DMA channel uart::tx_dma owns. Channel 5 is conventionally free
/// on Pi 3 / Zero 2 W; we assert at init() that firmware has powered
/// it on.
pub const UART_TX_CHANNEL: u32 = 5;

/// Set true once init() succeeds. `kick()` and `on_completion()` are
/// no-ops before this; lets the uart layer call into us unconditionally.
static READY: AtomicBool = AtomicBool::new(false);

/// One-time bring-up. Asserts that the chosen UART-TX channel is
/// firmware-enabled, resets it, and clears any stale END/INT bits.
/// Returns true on success. Idempotent.
pub fn init() -> bool {
    if READY.load(Ordering::Acquire) {
        return true;
    }
    let enable = read_enable();
    if (enable >> UART_TX_CHANNEL) & 1 == 0 {
        return false;
    }
    // CS_RESET self-clears; wait for it to drop. Worst case a handful
    // of MMIO read cycles per the datasheet.
    write_cs(UART_TX_CHANNEL, CS_RESET);
    for _ in 0..1000 {
        if read_cs(UART_TX_CHANNEL) & CS_RESET == 0 {
            break;
        }
    }
    // Clear any latched INT/END/ERROR bits from a prior owner.
    write_cs(UART_TX_CHANNEL, CS_INT | CS_END);
    // Wire the channel's completion IRQ at the BCM2835 IRQ
    // controller. DMA channel N → GPU IRQ source 16+N (Circle's
    // ARM_IRQ_DMA0 = 16). trap_irq's additive dispatch picks it up
    // from IRQ_PEND_1 on the next CPU IRQ.
    crate::platform::enable_bcm2835_irq(16 + UART_TX_CHANNEL);
    READY.store(true, Ordering::Release);
    true
}

/// Returns whether `init()` has completed successfully.
#[inline]
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Arm a DMA transfer on the UART TX channel. The CB must already
/// describe a single-shot mem→PL011 transfer (NEXTCONBK = 0); the
/// caller is responsible for cache-cleaning the source range before
/// calling.
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
pub unsafe fn arm_uart_tx(cb: &DmaCb) {
    let cb_arm_phys = cb as *const DmaCb as u64;
    crate::cpu::dc_civac_range(cb_arm_phys, core::mem::size_of::<DmaCb>());
    let cb_bus = bus_addr_ram(cb_arm_phys);
    write_conblk_ad(UART_TX_CHANNEL, cb_bus);
    // Channel arming sequence per Circle's `CDMAChannel::Start`:
    // WAIT_FOR_OUTSTANDING_WRITES at end of transfer + priority 1 +
    // ACTIVE.
    let cs = CS_WAIT_FOR_OUTSTANDING_WRITES | (1 << CS_PRIORITY_SHIFT) | CS_ACTIVE;
    write_cs(UART_TX_CHANNEL, cs);
}

/// Called from `trap_irq` when the BCM2835 IRQ controller reports a
/// pending DMA completion on `ch`. Acks the channel's INT/END bits
/// and dispatches to the registered consumer.
pub fn on_completion(ch: u32) {
    let cs = read_cs(ch);
    // W1C — write the bits set, the chip clears them.
    write_cs(ch, cs & (CS_INT | CS_END | CS_ERROR));
    if ch == UART_TX_CHANNEL {
        crate::uart::on_tx_done();
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

/// Snapshot for diagnostic logging — CS / DEBUG of the UART TX channel.
pub fn uart_tx_diag() -> (u32, u32) {
    (read_cs(UART_TX_CHANNEL), read_debug(UART_TX_CHANNEL))
}
