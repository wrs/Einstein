//! Newton DMA manager — Rust port of Einstein's `TDMAManager`.
//!
//! Einstein's DMA is almost entirely an API stub: the Newton kernel's
//! driver manages transfer state machines in software, so the only
//! piece of real per-chip state is `mAssignmentReg` (read-only write,
//! write-only read). Per-channel and chip-wide enable / disable /
//! status registers are logged for diagnostics and otherwise observed
//! as zero on read / dropped on write. See
//! `Emulator/TDMAManager.cpp:69-95` for ground truth and
//! `docs/peripherals.md` §DMA manager for the register map.
//!
//! We deliberately do not plumb channels 0 and 1 through to the
//! external-serial DMA driver yet — once a serial port peripheral
//! lands, that dispatch goes here.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::kprintln;

/// Bank 1 channel-register window (8 channels × 8 regs × 4 B, stride
/// 0x2000 per channel, 0x400 per register).
const BANK1_BASE: u64 = 0x0F08_0000;
const BANK1_END: u64 = 0x0F08_FC00;

/// Chip-wide channel-assignment register. R/W; writes latch, reads
/// return the last write. Einstein `TDMAManager.cpp:69-95`.
const K_HDWR_ASSIGN: u64 = 0x0F08_FC00;

/// Bank 2 channel-register window (same layout as bank 1).
const BANK2_BASE: u64 = 0x0F09_0000;
const BANK2_END: u64 = 0x0F09_8000;

/// Chip-wide enable / status register: writes enable a channel,
/// reads return status. Einstein always reads 0.
const K_HDWR_ENABLE_STATUS: u64 = 0x0F09_8000;

/// Chip-wide disable register. Write-only; no observable side effect
/// in Einstein's port.
const K_HDWR_DISABLE: u64 = 0x0F09_8400;

/// Chip-wide word-status register. Read-only; Einstein always reads 0.
const K_HDWR_WORD_STATUS: u64 = 0x0F09_8800;

#[derive(Default)]
struct DmaState {
    assign: u32,
}

struct DmaCell(UnsafeCell<DmaState>);
// SAFETY: accessed only from the single EL2 trap handler on core 0.
unsafe impl Sync for DmaCell {}

static DMA: DmaCell = DmaCell(UnsafeCell::new(DmaState { assign: 0 }));

/// Log budget for per-channel / chip-wide stub accesses so a spinning
/// kernel driver doesn't drown the console.
static LOG_BUDGET: AtomicUsize = AtomicUsize::new(0);
const LOG_MAX: usize = 32;

/// Returns true if `ipa` falls in the DMA register window this module
/// owns.
pub fn owns(ipa: u64) -> bool {
    (BANK1_BASE..BANK1_END).contains(&ipa)
        || ipa == K_HDWR_ASSIGN
        || (BANK2_BASE..BANK2_END).contains(&ipa)
        || ipa == K_HDWR_ENABLE_STATUS
        || ipa == K_HDWR_DISABLE
        || ipa == K_HDWR_WORD_STATUS
}

pub fn read(ipa: u64) -> u32 {
    // SAFETY: single-threaded.
    let s = unsafe { &*DMA.0.get() };
    match ipa {
        K_HDWR_ASSIGN => s.assign,
        K_HDWR_ENABLE_STATUS | K_HDWR_WORD_STATUS => 0,
        // Per-channel reads inside bank 1 / bank 2 windows — Einstein
        // reads 0 for all per-channel registers it doesn't delegate
        // to the serial driver. This matches Einstein's TDMAManager
        // behaviour (`Emulator/TDMAManager.cpp:69-95`) explicitly:
        // deliberate stub, not a silent default.
        _ if is_channel_reg(ipa) => {
            log_stub("dma channel read (modeled 0 per Einstein)", ipa, 0);
            0
        }
        _ => halt_unknown_dma("read", ipa, 0),
    }
}

pub fn write(ipa: u64, value: u32) {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *DMA.0.get() };
    match ipa {
        K_HDWR_ASSIGN => s.assign = value,
        K_HDWR_ENABLE_STATUS | K_HDWR_DISABLE => log_stub("dma enable/disable", ipa, value),
        // Per-channel writes mirror Einstein: log once, do nothing.
        _ if is_channel_reg(ipa) => log_stub("dma channel write (modeled drop per Einstein)", ipa, value),
        _ => halt_unknown_dma("write", ipa, value),
    }
}

/// True if `ipa` falls inside one of the two per-channel register
/// banks. Einstein models these as "read 0, write drop" for all
/// channels except 0/1 which would route to the serial driver. We
/// don't have a serial DMA driver wired up yet, so every channel
/// register is treated identically.
fn is_channel_reg(ipa: u64) -> bool {
    (BANK1_BASE..BANK1_END).contains(&ipa)
        || (BANK2_BASE..BANK2_END).contains(&ipa)
}

fn log_stub(what: &str, ipa: u64, value: u32) {
    let n = LOG_BUDGET.fetch_add(1, Ordering::Relaxed);
    if n < LOG_MAX {
        kprintln!("{} IPA={:#010x} val={:#010x}", what, ipa, value);
    }
}

fn halt_unknown_dma(op: &'static str, ipa: u64, value: u32) -> ! {
    kprintln!();
    kprintln!("*** dma::{} IPA={:#010x} val={:#010x} — owns() said mine, no match ***",
        op, ipa, value);
    kprintln!(
        "  (IPA is inside DMA register window but outside any modeled register."
    );
    kprintln!(
        "   Extend peripherals/dma.rs — see Emulator/TDMAManager.cpp.)"
    );
    crate::cpu::halt();
}
