//! Newton ASIC / memory-controller miscellany.
//!
//! The modelled registers that don't belong to a richer peripheral
//! model (VIC, DMA, serial, PCMCIA): the memory-controller bank-config
//! area at 0x0F00_xxxx, the BIO register banks, the external-abort /
//! bank-control / chip-revision cluster at 0x0F24_xxxx (including the
//! ROM serial-number 1-Wire chip), and the write-only bus / pin-strap
//! configuration registers at 0x0F28_xxxx. The `hv::mmio` router
//! dispatches here for every `layout::MMIO_WINDOWS` entry with policy
//! `Peripheral(PeriphId::Asic)`.
//!
//! Every register is modelled after Einstein's `TMemory` behaviour
//! (mostly the "unknown bank" silent-zero/silent-drop defaults, cited
//! per register). Unknown addresses inside the ASIC windows halt
//! loudly per the Phase A contract — the recognised set is a closed
//! whitelist, not an open silent-drop fallback.

use core::sync::atomic::{AtomicU32, Ordering};

// ---------- Register addresses ------------------------------------------------

/// kHdWr_PlatformVers (TMemoryConsts). Lives in the memory-controller
/// bank at 0x0F00_0000, not the interrupt controller — Einstein
/// services it via TPlatformManager, and we model it here with the
/// rest of the bank-config registers.
const K_HDWR_PLATFORM_VERS: u64 = 0x0F00_0008;

// Specific register reads the Newton kernel does very early.
//   TMemoryConsts::kHdWr_04RAMSize = 0x0F00_1800  — encodes installed RAM
//   TMemoryConsts::kHdWr_08RAMSize = 0x0F00_1C00  — secondary bank size
const HW_RAM_SIZE_1: u64 = 0x0F00_1800;
const HW_RAM_SIZE_2: u64 = 0x0F00_1C00;

// ROM serial-chip (kHdWr_P0F243000). Einstein models this as a 1-Wire
// serial-ROM bit stream (TMemory.cpp:984-999, 2723-2762): a 65-tick
// loop that returns the "end marker" (0) once, then 64 bits of the
// 2-word `mSerialNumber`, derived from the emulator's `mNewtonID[2]`
// via `TMemory::ComputeSerialNumber`. Einstein's default NewtonID is
// `{0x00004E65, 0x77746F6E}` (kMyNewtonIDHigh/Low at TEmulator.cpp:65,
// assigned in the TEmulator ctor at lines 97-98 — overrides the
// `{0, 0}` field initialiser at TEmulator.h:515). The resulting
// `mSerialNumber` values are computed by ComputeSerialNumber and the
// constants below match that calculation (verified by Python port).
// The kernel reads bit-by-bit via TSerialNumberROM::Init; each read
// returns `(bit & 1) << 1` and advances the index mod 65.
//
// Why this matters: TFlashStore's "Untitled" record (the internal
// store) seeds its `signature` slot from `GetSystemSerialNumber()`
// (ROM 0x003543ac–0x003543c8), which packs as
// `(mSerialNumber[0] << 24) | (mSerialNumber[1] >> 8) = 0x77746F6E`.
// NewtonScript encodes that as an integer Ref via `value << 2`, and
// decoding with arithmetic-shift-right-by-2 yields the signed int
// `-143364242`, which is the value Einstein's NS trace shows for
// `(internalFlashStore):GetSignature()`. Returning `{0,0}` here gives
// `0` instead, which then mismatches the saved signature in the
// CheckSerialNumber bytecode and routes the boot through an
// uninitialised-gLocaleCache crash.
//
// These constants are per-DEVICE identity, not ROM-version facts —
// they model the physical serial-number chip, and an existing flash
// store's signature was seeded from them, so they must stay stable
// for that store to keep validating. That is why they live here and
// not in `newton::rom_ver`.
const ROM_SERIAL_CHIP_IPA: u64 = 0x0F24_3000;
const ROM_SERIAL_NUMBER_0: u32 = 0x5C4E_6577;
const ROM_SERIAL_NUMBER_1: u32 = 0x746F_6E01;
static ROM_SERIAL_IX: AtomicU32 = AtomicU32::new(64);

// Stateful backing for kHdWr_BankCtrlReg (0x0F241000). Mirrors Einstein's
// `TMemory::mBankCtrlRegister` (TMemory.h:896 — init 0; writes update
// at TMemory.cpp:1930-1932; reads return at TMemory.cpp:981-983).
static BANK_CTRL_REG: AtomicU32 = AtomicU32::new(0);

// BIO registers sit on a 0x400 stride inside `layout::BIO_BANKS`
// (address = 0x0F05_0000 + bank << 10); off-stride addresses inside
// the window still halt loudly.
fn in_bio_bank(ipa: u64) -> bool {
    crate::hv::layout::BIO_BANKS.contains(ipa) && (ipa & 0x3FF) == 0
}

// ---------- MMIO dispatch -----------------------------------------------------

/// Marker for the [`crate::hv::mmio::MmioPeripheral`] router. Register
/// state lives in the module-level statics; this zero-sized type only
/// names the model for static dispatch.
pub struct Asic;

impl crate::hv::mmio::MmioPeripheral for Asic {
    fn read(ipa: u64) -> u32 {
        match read_opt(ipa, /*advance_serial=*/ true) {
            Some(v) => v,
            // Write-only registers have no read behaviour: the kernel
            // never reads them back, so a real read reaching one is an
            // unknown access, same as any unrecognised address.
            None => halt_unknown_asic("read", ipa, 0),
        }
    }

    fn write(ipa: u64, value: u32) {
        write(ipa, value)
    }

    /// Side-effect-free read used by the router's BE-8 sub-word splice
    /// and extraction: never advances the ROM-serial-chip bit index,
    /// and reports write-only registers as 0 (write-only: the guest
    /// never reads them back, and a sub-word RMW of one must not halt
    /// as an "unknown read").
    fn peek(ipa: u64) -> u32 {
        if let Some(v) = read_opt(ipa, /*advance_serial=*/ false) {
            return v;
        }
        if is_write_only_reg(ipa) {
            return 0;
        }
        halt_unknown_asic("peek", ipa, 0)
    }
}

/// Word read of a readable register; `None` for addresses with no read
/// behaviour (write-only registers, genuinely-unknown addresses).
/// `advance_serial` gates the one read side effect in this model (the
/// ROM-serial-chip bit index): `read` passes `true`, `peek` `false`.
fn read_opt(ipa: u64, advance_serial: bool) -> Option<u32> {
    let value = match ipa {
        // PlatformVers: TPlatformManager::GetVersion() returns 5
        // (Emulator/Platform/TPlatformManager.cpp:110). Newton's native
        // apps read this register to know the platform driver revision.
        K_HDWR_PLATFORM_VERS => 5,

        // kHdWr_04RAMSize: Einstein TMemory.cpp:868-873 computes
        //   thePageCount = (mRAMSize >> 16) & 0xFF;
        //   return (thePageCount << 24) | (thePageCount << 16) | thePageCount;
        // For our 4 MiB RAM (guest_mem::RAM_SIZE = 0x40_0000), pageCount
        // = 0x40, result = 0x40400040.
        HW_RAM_SIZE_1 => {
            let page_count = ((crate::hv::guest_mem::RAM_SIZE as u32) >> 16) & 0xFF;
            (page_count << 24) | (page_count << 16) | page_count
        }
        // kHdWr_08RAMSize: Einstein TMemory.cpp:874-876 returns 0.
        HW_RAM_SIZE_2 => 0,

        // kHdWr_P0F242400: chipset revision ID. TMemoryConsts.h:144
        // documents observed values 0, 0x01F9453C, 0x01F94573 and we
        // initially returned 0x01F94573 on the assumption that "the
        // ROM accepts any of them". It doesn't: ROMBoot at 0x186D0
        // does `BICS r0, r0, #0xFF000000 ; BNE 0x191D0`, so a non-zero
        // low-24 payload takes the WARM-reset fast-path that expects
        // `gParamBlockFromImagePhysical` (RAM 0x0400_6400) to already
        // hold the per-mode stack-table. On cold boot that RAM is
        // zero and SP_und ends up 0, producing a zero-SP STMDB abort
        // at ROM 0x19410. Einstein returns 0 for this register
        // (unknown-Bank-#4 default in TMemory.cpp), so the BNE isn't
        // taken and the kernel falls through to the COLD-boot path
        // that calls SetFIQStack/SetIRQStack/... with explicit stack
        // values. Match Einstein.
        0x0F24_2400 => 0,

        // kHdWr_P0F001000: memory-access-speed-related. R/W; kernel
        // reads 0 during probe. TMemoryConsts.h:56.
        0x0F00_1000 => 0,

        // kHdWr_BankCtrlReg (TMemoryConsts.h:137 = 0x0F241000): bank
        // control register. Einstein's TMemory.cpp:981-983 returns the
        // stateful `mBankCtrlRegister` (init 0, see TMemory.h:896);
        // writes at TMemory.cpp:1930-1932 store `inWord`. The kernel's
        // bus-config init at ROM 0x00019644/0x00019808/0x00019840 writes
        // values (0, 0x300, 0x0F241000) and on those latter two paths
        // does an immediate read-back, but the read result is overwritten
        // by the next `ldr` before consumption — so a non-stateful read
        // returns 0 here harmlessly today. Keep it stateful regardless,
        // matching Einstein's code.
        0x0F24_1000 => BANK_CTRL_REG.load(Ordering::Relaxed),

        // ExtDataAbt1/2/3 — external data-abort status registers. The
        // kernel's DataAbortHandler at 0x0039_3268 reads all three
        // (0x0F24_0000 / 0x0F24_0800 then ANDs with 0x1FF, plus
        // 0x0F24_0400 on the bne strne path) to classify the abort
        // source. Return 0 so the kernel falls through to its normal
        // translation-fault path rather than the "external data abort"
        // diagnostic branch at 0x0039_3894. Matches Einstein's TMemory
        // "unknown bank #3" default of 0. Writes are accepted as
        // no-ops in the write path below.
        0x0F24_0000 => 0,
        0x0F24_0400 => 0,
        0x0F24_0800 => 0,

        // kHdWr_P0F048000: R/W, typical value 0. TMemoryConsts.h:63.
        0x0F04_8000 => 0,

        // ROM serial chip — see constants above. Returns (bit & 1) << 1
        // following Einstein's bit-stream model of TMemory.cpp:984-999.
        ROM_SERIAL_CHIP_IPA => {
            let ix = ROM_SERIAL_IX.load(Ordering::Relaxed);
            let bit = if ix == 64 {
                0
            } else if ix >= 32 {
                ROM_SERIAL_NUMBER_0 >> (ix - 32)
            } else {
                ROM_SERIAL_NUMBER_1 >> ix
            };
            // The bit-index advance is this model's only read side
            // effect; `peek` (advance_serial=false) reads the current
            // bit without consuming it.
            if advance_serial {
                ROM_SERIAL_IX.store((ix + 1) % 65, Ordering::Relaxed);
            }
            (bit & 1) << 1
        }

        // BIO-interface register bank (0x0F05_0000 + bank<<10, 32 banks).
        // See `in_bio_bank` near the top of the file. Einstein returns 0
        // for all of these (TMemory.cpp:952-959 unknown-bank-#3
        // fallback); match it. Covers the TMemoryConsts-named registers
        // (0x2C00 / 0x3000 / 0x3400 / 0x3800 / 0x4400 / 0x4800 / 0x5000)
        // plus the anonymous banks the 717006 kernel's BIO init loop
        // touches (0x3C00, 0x4C00, …).
        a if in_bio_bank(a) => 0,

        // No read behaviour (write-only or genuinely unknown): report
        // absence so `read` halts and `peek` falls through to the
        // write-only check.
        _ => return None,
    };
    Some(value)
}

fn write(ipa: u64, value: u32) {
    // Platform "write-only" control registers. Each is a Newton ASIC
    // pin-strap / bus-control / power-gate register that the kernel
    // configures once at BootOS time. Einstein's TMemory doesn't model
    // any observable state behind them — the writes are accepted and
    // never read back. TMemoryConsts.h cites the typical values in
    // comments; we model each as explicit write-accept no-ops so the
    // set of recognised addresses is a closed whitelist (Phase A),
    // not an open silent-drop fallback.
    match ipa {
        // PlatformVers is readable state with no write handler in
        // Einstein; writes fall through to the silent-drop default.
        K_HDWR_PLATFORM_VERS => {} // drop per Einstein

        // --- Memory-controller-ish (TMemoryConsts.h ~56-67) ---
        0x0F00_1000 => {} // P0F001000        R/W, memory-access speed
        0x0F00_1800 => {} // 04RAMSize        "W (also written with 0x00 & 0x40)"
        0x0F00_1C00 => {} // 08RAMSize        W
        0x0F00_2000 => {} // P0F002000        W (0x80)
        0x0F04_3000 => {} // P0F043000        W (0x7400)
        0x0F04_3800 => {} // P0F043800        W (0x2000)
        0x0F04_8000 => {} // P0F048000        R/W (0)
        // BIO-interface register bank — see the read path /
        // `in_bio_bank` for the stride + Einstein rationale. Writes are
        // accepted as no-ops.
        a if in_bio_bank(a) => {}

        // --- External data-abort / bank-control / chip-rev area ---
        0x0F24_0000 => {} // ExtDataAbt1      R (write path accepted no-op)
        0x0F24_0400 => {} // ExtDataAbt2      W
        0x0F24_0800 => {} // ExtDataAbt3      W
        // BankCtrlReg (0x0F241000): write updates the stateful mirror.
        // Einstein TMemory.cpp:1930-1932 stores `inWord` to
        // `mBankCtrlRegister`. Match that.
        0x0F24_1000 => BANK_CTRL_REG.store(value, Ordering::Relaxed),
        0x0F24_1800 => {} // P0F241800        W (0x3916)
        0x0F24_2400 => {} // P0F242400        R/W chipset rev
        0x0F24_3000 => {} // ROMSerialChip    R/W (0, 1)
        0x0F24_7000 => {} // P0F247000        W (1)

        // --- Bus / pin-strap configuration the kernel touches early ---
        0x0F28_0000 => {} // P0F280000        W (0x465A, 0xC044)
        0x0F28_0400 => {} // P0F280400        W (0x181A, 0x2C34)
        0x0F28_0800 => {} // P0F280800        W (0x2003)
        // P0F280C00 and P0F282000 aren't cited in TMemoryConsts.h but
        // the unrolled bus-config init at ROM 0x192c8..0x19330 writes
        // to both alongside the documented 0x0F28_{0000,0400,0800,
        // 3000,3400}. Einstein's TMemory silently no-ops all unmapped
        // Bank #4 writes; we accept each explicitly so the Phase A
        // whitelist stays a closed set.
        0x0F28_0C00 => {}
        0x0F28_2000 => {}
        0x0F28_3000 => {} // P0F283000        W (0, 0x255, 0x257)
        // kHdWr_P0F283400 isn't documented in TMemoryConsts.h but is
        // written with value 0x23 by the same init routine (PC 0x19598
        // inside the 0x1955c setup function) that writes 0x23 to the
        // documented 0x0F284000. Treat it as an adjacent bus-control
        // register — an entry we've added because the ROM trips the
        // Phase A halt, not because Einstein documents it.
        0x0F28_3400 => {}
        0x0F28_4000 => {} // P0F284000        W (0x23)

        a => halt_unknown_asic("write", a, value),
    }
}

/// True for registers that exist only in the write whitelist above —
/// the kernel writes them at BootOS time and never reads them back, so
/// `read_opt` returns `None` for them. `peek` reports these as 0 so a
/// sub-word RMW of a write-only register splices onto 0 instead of
/// misfiring the unknown-read halt.
///
/// This list must track the write-only arms of `write`'s match (the
/// closed Phase-A whitelist). Readable registers there (PlatformVers,
/// 0x0F00_1000, 0x0F00_1800/1C00, 0x0F04_8000, 0x0F24_0000/0400/0800,
/// 0x0F24_1000 BankCtrl, 0x0F24_2400, 0x0F24_3000 ROM serial,
/// in_bio_bank) are intentionally absent — `read_opt` already returns
/// their value, so peek never reaches here for them.
fn is_write_only_reg(ipa: u64) -> bool {
    matches!(ipa,
        0x0F00_2000
        | 0x0F04_3000 | 0x0F04_3800
        | 0x0F24_1800 | 0x0F24_7000
        | 0x0F28_0000 | 0x0F28_0400 | 0x0F28_0800 | 0x0F28_0C00
        | 0x0F28_2000 | 0x0F28_3000 | 0x0F28_3400 | 0x0F28_4000
    )
}

/// Loud halt for an access inside an ASIC-policy window that no arm
/// above recognises. Per Phase A, extend the whitelist deliberately
/// (with the Einstein cross-reference) rather than silently dropping.
fn halt_unknown_asic(op: &'static str, ipa: u64, value: u32) -> ! {
    crate::kprintln!();
    crate::kprintln!(
        "*** asic::{} IPA={:#010x} val={:#010x} — inside an ASIC window but not a recognised register ***",
        op, ipa, value
    );
    crate::kprintln!(
        "  (add the register to peripherals/asic.rs with its Einstein \
         cross-reference, or fix the layout window.)"
    );
    crate::arch::cpu::halt();
}
