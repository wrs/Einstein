//! PCMCIA controller stub (no card inserted).
//!
//! Newton wires four 256-MiB PCMCIA windows starting at 0x3000_0000,
//! 0x4000_0000, 0x5000_0000, 0x6000_0000 (Einstein
//! `TMemoryConsts::kPCMCIA{0..3}Base`). Real Newton hardware only
//! populates slots 0 and 1 (the MP2x00 has two PCMCIA bays); slots 2
//! and 3 have no controller chip on the bus. We model slots 0/1 as
//! "controller present, no card inserted" and slots 2/3 as "no
//! controller" — the latter is critical because `TCardServer`'s init
//! loop iterates 4 sockets and only skips `RegisterInterrupt` for
//! sockets where `TCardSocket::GetChipInfo` returns 0. If we let
//! slots 2/3 chip-detect succeed, socket 3 calls
//! `RegisterInterrupt(bit=14)` which then writes out-of-bounds past
//! gGPIOInterface's 13-entry handler array into gLocaleCache.
//!
//! Within each slot the layout (Einstein `TPCMCIAController.h`) is:
//!
//!   offset 0x0000_0000..0x0400_0000  attribute space   (card-side)
//!   offset 0x0400_0000..0x0800_0000  IO space          (card-side)
//!   offset 0x0800_0000..0x0C00_0000  memory space      (card-side)
//!   offset 0x0C00_0000..0x0C00_4400  controller registers (host-side)
//!
//! The card-side ranges return 0 with no card inserted (Einstein's
//! TPCMCIAController returns 0 when `mCard == nullptr`). The
//! controller-register range is what `TCardSocket::GetChipInfo` (ROM
//! 0x55714) probes — it writes 0xa5a5 to reg_3000, 0x5a5a to reg_3800,
//! reads them back, and only proceeds with socket bring-up if the
//! values stuck. The controller therefore backs its registers with
//! real read/write storage: returning a fixed sentinel for every read
//! fails chip-detect and steers the boot down the heavy "no chip"
//! teardown path that exhausts the kernel's stack-page pool and
//! triggers an L2 alias wedge.
//!
//! Storage scope: every write to a known controller register sticks;
//! every read returns the stored value, with two exceptions that
//! match Einstein:
//!
//!   reg_1c00 (status): on read, OR with `k1C00_CardIsPresent (0x000C)`
//!     to report "no card" — the kernel uses this bit instead of a
//!     failed chip-detect to drive the no-card UI path.
//!   reg_4400 (chip ID): always reads as 0xFC.
//!
//! Writes to unknown controller-register offsets are dropped (with a
//! budgeted log) rather than halting, because the kernel does write a
//! handful of unknown offsets during init and we'd rather discover
//! them lazily than block the boot. Reads of unknown offsets in the
//! controller range return 0.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::kprintln;

/// Slot 0..3 base addresses — TMemoryConsts::kPCMCIA{0..3}Base. The
/// Newton architecture wires four sockets; real hardware only
/// populates 0 and 1 but the kernel's bring-up code probes all four
/// (TCardSocket::GetChipInfo runs once per slot). All four behave
/// identically in our model: chip-detect storage works, no card.
const SLOT0_BASE: u64 = 0x3000_0000;
const SLOT1_BASE: u64 = 0x4000_0000;
const SLOT2_BASE: u64 = 0x5000_0000;
const SLOT3_BASE: u64 = 0x6000_0000;
const SLOT_SIZE:  u64 = 0x1000_0000;

/// Within-slot offsets (Einstein `TPCMCIAController.h`).
const ATTR_END:   u64 = 0x03FF_FFFF;
const IO_BASE:    u64 = 0x0400_0000;
const IO_END:     u64 = 0x07FF_FFFF;
const MEM_BASE:   u64 = 0x0800_0000;
const MEM_END:    u64 = 0x0BFF_FFFF;
const REG_BASE:   u64 = 0x0C00_0000;
/// Inclusive end — kHdWr_Reg4400 is the last register (returns 0xFC).
const REG_END:    u64 = 0x0C00_4400;

/// Reg_1C00 status bit indicating "no card present". Einstein:
/// `k1C00_CardIsPresent = 0x000C`. Counter-intuitively the bit is set
/// when the socket is empty (the card-detect lines float high).
const K1C00_CARD_IS_PRESENT: u32 = 0x000C;

/// Per-slot controller-register storage: 17 word registers at offsets
/// 0x0000, 0x0400, 0x0800, ..., 0x4000 — every 0x400 bytes. One named
/// field per register, selected by the `cell()` match below; offsets
/// that don't map to a register fall through to the unknown-offset
/// path. (0x4400 is not stored — `read` answers it with a constant
/// 0xFC.)
struct SlotRegs {
    reg_0000: AtomicU32, // int raised
    reg_0800: AtomicU32,
    reg_0c00: AtomicU32, // int raised (?)
    reg_1000: AtomicU32,
    reg_1400: AtomicU32,
    reg_1800: AtomicU32,
    reg_1c00: AtomicU32, // status — read OR'd with k1C00_CardIsPresent
    reg_2000: AtomicU32,
    reg_2400: AtomicU32,
    reg_2800: AtomicU32,
    reg_2c00: AtomicU32,
    reg_3000: AtomicU32, // chip-detect target #1
    reg_3400: AtomicU32,
    reg_3800: AtomicU32, // chip-detect target #2
    reg_3c00: AtomicU32,
    reg_4000: AtomicU32,
    int_ctrl: AtomicU32, // kHdWr_IntCtrlReg = 0x0400 — Einstein has it as a separate field
}

impl SlotRegs {
    const fn new() -> Self {
        Self {
            reg_0000: AtomicU32::new(0),
            reg_0800: AtomicU32::new(0),
            reg_0c00: AtomicU32::new(0),
            reg_1000: AtomicU32::new(0),
            reg_1400: AtomicU32::new(0),
            reg_1800: AtomicU32::new(0),
            reg_1c00: AtomicU32::new(0),
            reg_2000: AtomicU32::new(0),
            reg_2400: AtomicU32::new(0),
            reg_2800: AtomicU32::new(0),
            reg_2c00: AtomicU32::new(0),
            reg_3000: AtomicU32::new(0),
            reg_3400: AtomicU32::new(0),
            reg_3800: AtomicU32::new(0),
            reg_3c00: AtomicU32::new(0),
            reg_4000: AtomicU32::new(0),
            int_ctrl: AtomicU32::new(0),
        }
    }

    fn cell(&self, reg_off: u64) -> Option<&AtomicU32> {
        match reg_off {
            0x0000 => Some(&self.reg_0000),
            0x0400 => Some(&self.int_ctrl),
            0x0800 => Some(&self.reg_0800),
            0x0C00 => Some(&self.reg_0c00),
            0x1000 => Some(&self.reg_1000),
            0x1400 => Some(&self.reg_1400),
            0x1800 => Some(&self.reg_1800),
            0x1C00 => Some(&self.reg_1c00),
            0x2000 => Some(&self.reg_2000),
            0x2400 => Some(&self.reg_2400),
            0x2800 => Some(&self.reg_2800),
            0x2C00 => Some(&self.reg_2c00),
            0x3000 => Some(&self.reg_3000),
            0x3400 => Some(&self.reg_3400),
            0x3800 => Some(&self.reg_3800),
            0x3C00 => Some(&self.reg_3c00),
            0x4000 => Some(&self.reg_4000),
            _ => None,
        }
    }
}

static SLOT0: SlotRegs = SlotRegs::new();
static SLOT1: SlotRegs = SlotRegs::new();
static SLOT2: SlotRegs = SlotRegs::new();
static SLOT3: SlotRegs = SlotRegs::new();

/// Split log budgets (periph-M4): routine/expected traffic (card-side
/// "no card" reads/writes, known controller-register accesses) burns a
/// tight budget so the boot-time chip-detect probes can't exhaust the
/// console, while genuinely-unknown register offsets and out-of-range
/// accesses get their own generous budget so lazy discovery never goes
/// silent behind routine traffic.
static LOG: crate::diag::diag_util::TwoTierLog = crate::diag::diag_util::TwoTierLog::new(8, 64);

/// Marker for the [`crate::hv::mmio::MmioPeripheral`] router. The four slot
/// register banks are module-level statics; this zero-sized type only
/// names the model for static dispatch.
pub struct Pcmcia;

impl crate::hv::mmio::MmioPeripheral for Pcmcia {
    fn read(ipa: u64) -> u32 {
        read(ipa)
    }
    fn write(ipa: u64, value: u32) {
        write(ipa, value)
    }
}

fn ipa_to_slot(ipa: u64) -> Option<(&'static SlotRegs, u64, u8)> {
    if (SLOT0_BASE..SLOT0_BASE + SLOT_SIZE).contains(&ipa) {
        Some((&SLOT0, ipa - SLOT0_BASE, 0))
    } else if (SLOT1_BASE..SLOT1_BASE + SLOT_SIZE).contains(&ipa) {
        Some((&SLOT1, ipa - SLOT1_BASE, 1))
    } else if (SLOT2_BASE..SLOT2_BASE + SLOT_SIZE).contains(&ipa) {
        Some((&SLOT2, ipa - SLOT2_BASE, 2))
    } else if (SLOT3_BASE..SLOT3_BASE + SLOT_SIZE).contains(&ipa) {
        Some((&SLOT3, ipa - SLOT3_BASE, 3))
    } else {
        None
    }
}

fn read(ipa: u64) -> u32 {
    let (regs, off, slot) = match ipa_to_slot(ipa) {
        Some(x) => x,
        // The router only dispatches here for IPAs inside the layout
        // PCMCIA window, which the four slot ranges cover exactly, so
        // this arm is unreachable. If it ever fires, the window and
        // ipa_to_slot have desynced — halt loudly like
        // vic::halt_vic_unknown / dma::halt_unknown_dma rather than
        // silently fabricating a value.
        None => halt_pcmcia_unreachable("read", ipa, 0),
    };
    if off <= ATTR_END || (IO_BASE..=IO_END).contains(&off) || (MEM_BASE..=MEM_END).contains(&off) {
        // Card-side spaces — no card inserted, return 0 (Einstein default).
        log_expected("pcmcia read (card-side, no card)", ipa, 0);
        return 0;
    }
    if (REG_BASE..=REG_END).contains(&off) {
        let reg_off = off - REG_BASE;
        // Slots 2/3 have no controller — every read returns 0 so
        // GetChipInfo's 0xa5a5/0x5a5a write-and-read-back probe fails.
        if slot >= 2 {
            log_expected("pcmcia read (no controller in slot)", ipa, 0);
            return 0;
        }
        // kHdWr_Reg4400 — Einstein hardcodes 0xFC.
        if reg_off == 0x4400 {
            log_expected("pcmcia read reg_4400", ipa, 0xFC);
            return 0xFC;
        }
        if let Some(cell) = regs.cell(reg_off) {
            let mut v = cell.load(Ordering::Relaxed);
            if reg_off == 0x1C00 {
                // No card → set k1C00_CardIsPresent (counter-intuitively
                // named — set means "no card"). Match Einstein.
                v |= K1C00_CARD_IS_PRESENT;
            }
            log_expected("pcmcia read reg", ipa, v);
            return v;
        }
        log_unknown("pcmcia read unknown reg (returning 0)", ipa, 0);
        return 0;
    }
    log_unknown("pcmcia read out-of-range (returning 0)", ipa, 0);
    0
}

fn write(ipa: u64, value: u32) {
    let (regs, off, slot) = match ipa_to_slot(ipa) {
        Some(x) => x,
        // Unreachable for the same reason as `read` — see there.
        None => halt_pcmcia_unreachable("write", ipa, value),
    };
    if off <= ATTR_END || (IO_BASE..=IO_END).contains(&off) || (MEM_BASE..=MEM_END).contains(&off) {
        // Card-side write with no card inserted — drop silently.
        log_expected("pcmcia write (card-side, no card; dropped)", ipa, value);
        return;
    }
    if (REG_BASE..=REG_END).contains(&off) {
        let reg_off = off - REG_BASE;
        // Slots 2/3 have no controller — drop all controller-register
        // writes so chip-detect (read-back) sees zero.
        if slot >= 2 {
            log_expected("pcmcia write (no controller in slot; dropped)", ipa, value);
            return;
        }
        if let Some(cell) = regs.cell(reg_off) {
            cell.store(value, Ordering::Relaxed);
            log_expected("pcmcia write reg", ipa, value);
            return;
        }
        log_unknown("pcmcia write unknown reg (dropped)", ipa, value);
        return;
    }
    log_unknown("pcmcia write out-of-range (dropped)", ipa, value);
}

/// Routine/expected traffic (card-side, known registers): tight budget.
fn log_expected(what: &str, ipa: u64, value: u32) {
    if LOG.expected() {
        kprintln!("{} IPA={:#010x} val={:#010x}", what, ipa, value);
    }
}

/// Genuinely-unknown offsets / out-of-range (discovery): own budget so
/// routine traffic can't silence it.
fn log_unknown(what: &str, ipa: u64, value: u32) {
    if LOG.unknown() {
        kprintln!("{} IPA={:#010x} val={:#010x}", what, ipa, value);
    }
}

fn halt_pcmcia_unreachable(op: &'static str, ipa: u64, value: u32) -> ! {
    kprintln!();
    kprintln!(
        "*** pcmcia::{} IPA={:#010x} val={:#010x} — inside the PCMCIA window but no slot ***",
        op, ipa, value
    );
    kprintln!(
        "  (the layout PCMCIA window and ipa_to_slot's slot ranges have"
    );
    kprintln!(
        "   desynced. Reconcile layout::MMIO_WINDOWS's PCMCIA entry and"
    );
    kprintln!(
        "   ipa_to_slot in peripherals/pcmcia.rs.)"
    );
    crate::arch::cpu::halt();
}
