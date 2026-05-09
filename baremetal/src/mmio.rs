//! MMIO dispatch for trapped guest accesses to Newton peripheral space.
//!
//! Every access that lands here comes from a stage-2 fault — the IPA
//! is outside our mapped ROM / RAM / flash / framebuffer regions.
//! We route each IPA to the owning peripheral module where we can,
//! and halt loudly on anything we don't recognise. Per Phase A (see
//! baremetal/PLAN.md and baremetal/CLAUDE.md): unknown sub-cases
//! return a loud error, not a silent stub value. Silent drops mask
//! exactly the bugs the halts are meant to surface.
//!
//! Routing order (first match wins):
//!
//!   1. peripherals::vic     — interrupt controller + tick clock
//!                             (0x0F18_xxxx).
//!   2. peripherals::dma     — DMA bank 1 / 2 + chip-wide registers
//!                             (0x0F08_0000..0x0F09_9000).
//!   3. peripherals::pcmcia  — "no card" for slot 0 and slot 1
//!                             (0x30000000..0x50000000).
//!   4. peripherals::serial  — four TSerialChip windows
//!                             (0x0F1C_0000..0x0F20_0000).
//!   5. A handful of still-inline stubs for registers the Newton ROM
//!      reads at boot time (RAM size, chipset revision, power/GPIO
//!      bits). These are **known, deliberately-stubbed** registers;
//!      any new unknown register halts so we add it here on purpose.
//!   6. Unknown IPAs (either inside `0x0F00_0000..0x0F40_0000`
//!      hardware window or outside it): halt with full context so we
//!      model the peripheral properly.
//!
//! When you find yourself guessing what a register should return,
//! build a probe run and check Einstein's behaviour first — see
//! `probe/FINDINGS.md`.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::{cpu, kprintln, peripherals::{dma, pcmcia, serial, vic}};

const HW_BASE: u64 = 0x0F00_0000;
const HW_END: u64 = 0x0F40_0000;

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
const ROM_SERIAL_CHIP_IPA: u64 = 0x0F24_3000;
const ROM_SERIAL_NUMBER_0: u32 = 0x5C4E_6577;
const ROM_SERIAL_NUMBER_1: u32 = 0x746F_6E01;
static ROM_SERIAL_IX: AtomicU32 = AtomicU32::new(64);

// Stateful backing for kHdWr_BankCtrlReg (0x0F241000). Mirrors Einstein's
// `TMemory::mBankCtrlRegister` (TMemory.h:896 — init 0; writes update
// at TMemory.cpp:1930-1932; reads return at TMemory.cpp:981-983).
static BANK_CTRL_REG: AtomicU32 = AtomicU32::new(0);

// Specific register reads the Newton kernel does very early.
//   TMemoryConsts::kHdWr_04RAMSize = 0x0F00_1800  — encodes installed RAM
//   TMemoryConsts::kHdWr_08RAMSize = 0x0F00_1C00  — secondary bank size
const HW_RAM_SIZE_1: u64 = 0x0F00_1800;
const HW_RAM_SIZE_2: u64 = 0x0F00_1C00;


// MP2x00 RAM-bank probe window. BootOS probes 0x04000000 (present,
// 4 MiB — we map it) and 0x08000000 (absent — the "we have 4 MiB not
// 8 MiB" path). The probe does a signature write/read at `base +
// 0x200000`; if the read doesn't match the signature, the bank is
// declared absent. We model the second bank as "no memory": writes
// are dropped deterministically, reads return 0. That gives the
// probe a clean "absent" signal without a silent ignored write.
const RAM_PROBE_ABSENT_BASE: u64 = 0x0800_0000;
const RAM_PROBE_ABSENT_END:  u64 = 0x0900_0000;

// "No extra ROM / REx / flash" probe window. The Newton kernel's
// TestForREx (rom 0x3137dc) and related probes scan fixed addresses
// past the mapped flash-bank-2 window (0x10400000 upward) looking
// for RExBlock magic at fixed offsets. We explicitly model these as
// absent so reads return 0 and the probe's magic-compare fails
// cleanly. PCMCIA (0x30000000+) is handled separately.
const NO_REX_PROBE_BASE: u64 = 0x1040_0000;
const NO_REX_PROBE_END:  u64 = 0x2000_0000;

// "Unknown bank #5" silent-zero window — the gap between Newton MP2x00's
// kFlashBank2End (0x1040_0000) and kPCMCIA0Base (0x3000_0000). Einstein's
// `TMemory::ReadP` (Emulator/TMemory.cpp:1026-1034) returns 0 silently
// for any read in this range and absorbs writes. The 717006 kernel hits
// this on a TInterpreter-side `MakeString__FPCc` whose to-Unicode
// translator descriptor's `+16` slot (the per-encoding lookup table
// base) is 0x2000_0110 — a bogus pointer the kernel computed from
// uninitialised / partially-installed encoding state. Einstein tolerates
// it via this silent-zero path (the convert function reads 0 → emits
// U+0000 → boot continues with garbled string output instead of a hard
// fault). Match that behaviour here so the trip-wire isn't load-bearing
// past the modelled-MMIO window. The deeper "why is the descriptor
// wrong" question is decoupled from this wedge: it's a NewtonScript-
// level bug Einstein masks the same way.
const UNKNOWN_BANK5_BASE: u64 = 0x2000_0000;
const UNKNOWN_BANK5_END:  u64 = 0x3000_0000;

// Test-only R/W scratch registers above XOR_LIMIT (= 0x1000_0000), used
// by `guest-tests/tests/test_shadow_stub.S` subtest_11 to verify that
// shadow-stub byte/halfword accesses bypass the BE-32 XOR for IPAs >=
// XOR_LIMIT. Real Newton hardware doesn't expose anything in this
// window; the kernel never touches it during boot. A byte-granular
// 16-byte storage cell is enough for the test.
const TEST_SCRATCH_BASE: u64 = 0x1200_0000;
const TEST_SCRATCH_END:  u64 = 0x1200_0010;
static mut TEST_SCRATCH: [u8; 16] = [0; 16];

// BIO interface register bank. `TBIOInterface::BIOReadRegister` /
// `BIOWriteCommand` / etc. at ROM `0x26b878..0x26ba10` compute the
// target register address as `0x0F05_0000 + (bank_index << 10)`, so
// the 32 registers live at `0x0F05_0000`, `0x0F05_0400`, …,
// `0x0F05_7C00`. The early-boot kernel iterates over several banks
// (14, 15, 16, 17, 18, 19, 20, …) during BIO init; Einstein's TMemory
// doesn't model these registers — the "unknown bank #3" fallback
// accepts writes silently and returns 0 for reads (TMemory.cpp:952-959).
// Rather than whack-a-mole each register as the iterator advances,
// accept the whole known-stride range in one explicit entry. This is
// still a closed whitelist — addresses outside the stride or outside
// the 32-register window continue to halt loudly.
const BIO_BANK_BASE: u64 = 0x0F05_0000;
const BIO_BANK_END:  u64 = 0x0F05_8000;

fn in_bio_bank(ipa: u64) -> bool {
    ipa >= BIO_BANK_BASE && ipa < BIO_BANK_END && (ipa & 0x3FF) == 0
}

pub fn read(ipa: u64, sas: u8, elr: u64) -> u32 {
    // BE-8 (production builds): byte/halfword accesses from the guest
    // land at the natural IPA (the CPU does the byte-lane transform
    // itself). Guest-test builds run the guest LE under the legacy
    // shadow-stub path, where inline-stub byte/halfword accesses are
    // pre-XOR'd by 3/2; un-XOR here.
    #[cfg(nh_guest_test)]
    let ipa = unxor_sub_word(ipa, sas);
    let value = match ipa {
        a if vic::owns(a) => vic::read(a),
        a if dma::owns(a) => dma::read(a),
        a if pcmcia::owns(a) => pcmcia::read(a),
        a if serial::owns(a) => serial::read(a),

        // kHdWr_04RAMSize: Einstein TMemory.cpp:868-873 computes
        //   thePageCount = (mRAMSize >> 16) & 0xFF;
        //   return (thePageCount << 24) | (thePageCount << 16) | thePageCount;
        // For our 4 MiB RAM (guest_mem::RAM_SIZE = 0x40_0000), pageCount
        // = 0x40, result = 0x40400040.
        HW_RAM_SIZE_1 => {
            let page_count = ((crate::guest_mem::RAM_SIZE as u32) >> 16) & 0xFF;
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
        // "unknown bank #3" default of 0. Writes are already accepted
        // as no-ops in the write path below.
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
            ROM_SERIAL_IX.store((ix + 1) % 65, Ordering::Relaxed);
            (bit & 1) << 1
        }

        // BIO-interface register bank (0x0F05_0000 + bank<<10, 32 banks).
        // See `BIO_BANK_BASE` / `in_bio_bank` near the top of the file.
        // Einstein returns 0 for all of these (TMemory.cpp:952-959 unknown-
        // bank-#3 fallback); match it. Covers the TMemoryConsts-named
        // registers (0x2C00 / 0x3000 / 0x3400 / 0x3800 / 0x4400 / 0x4800
        // / 0x5000) plus the anonymous banks the 717006 kernel's BIO init
        // loop touches (0x3C00, 0x4C00, …).
        a if in_bio_bank(a) => 0,

        // GPIO input (PCMCIA door-lock + misc sense lines).
        // Einstein returns all-ones = "no cards / switches open".
        0x0F18_D400 => 0xFFFF_FFFF,

        // kHdWr_P0F184C00 (TMemoryConsts.h:101, "R"): Einstein's TMemory.cpp
        // Bank #3 read path (lines 803-960) has NO specific handler for this
        // address — it falls through to the "unknown bank #3" default at
        // lines 950-960, which returns 0. The previous "all-ok high =
        // 0xFFFFFFFF per Einstein" comment was wrong (no such Einstein
        // code exists). Bit 21 of this register gates a kernel polling
        // path at ROM 0x00019d34 / 0x00019d90 / 0x00019e34 (`tst r1,
        // #0x00200000`); returning 0 makes us take the same branches as
        // Einstein.
        0x0F18_4C00 => 0,

        // RAM-probe "absent bank" window (see const comment above).
        a if (RAM_PROBE_ABSENT_BASE..RAM_PROBE_ABSENT_END).contains(&a) => 0,

        // Test-only scratch window (see TEST_SCRATCH_BASE comment) —
        // ordered before the NO_REX_PROBE arm because the scratch
        // sub-window sits inside the same 0x1040_0000..0x2000_0000 IPA
        // range.
        a if (TEST_SCRATCH_BASE..TEST_SCRATCH_END).contains(&a) => {
            test_scratch_read(a, sas)
        }

        // REx / extra-flash "absent" probe window (see const comment).
        a if (NO_REX_PROBE_BASE..NO_REX_PROBE_END).contains(&a) => 0,

        // "Unknown bank #5" silent-zero window (see const comment).
        a if (UNKNOWN_BANK5_BASE..UNKNOWN_BANK5_END).contains(&a) => 0,

        a => halt_on_unknown("read", a, sas, 0, elr),
    };

    mask_for_size(value, sas)
}

/// Byte-granular read from the test scratch window. Byte (sas=0) and
/// halfword (sas=1) accesses return the raw bytes from `TEST_SCRATCH`;
/// word reads (sas=2) assemble a u32 from four consecutive bytes.
fn test_scratch_read(ipa: u64, sas: u8) -> u32 {
    let off = (ipa - TEST_SCRATCH_BASE) as usize;
    // SAFETY: single-threaded EL2 access; bounds checked above.
    unsafe {
        let p = core::ptr::addr_of!(TEST_SCRATCH) as *const u8;
        match sas {
            0 => *p.add(off) as u32,
            1 => u16::from_le_bytes([*p.add(off), *p.add(off + 1)]) as u32,
            _ => u32::from_le_bytes([
                *p.add(off),
                *p.add(off + 1),
                *p.add(off + 2),
                *p.add(off + 3),
            ]),
        }
    }
}

/// Byte-granular write into the test scratch window. Mirrors the
/// `test_scratch_read` size dispatch.
fn test_scratch_write(ipa: u64, sas: u8, value: u32) {
    let off = (ipa - TEST_SCRATCH_BASE) as usize;
    // SAFETY: single-threaded EL2 access; bounds checked.
    unsafe {
        let p = core::ptr::addr_of_mut!(TEST_SCRATCH) as *mut u8;
        match sas {
            0 => *p.add(off) = value as u8,
            1 => {
                let bytes = (value as u16).to_le_bytes();
                *p.add(off) = bytes[0];
                *p.add(off + 1) = bytes[1];
            }
            _ => {
                let bytes = value.to_le_bytes();
                *p.add(off) = bytes[0];
                *p.add(off + 1) = bytes[1];
                *p.add(off + 2) = bytes[2];
                *p.add(off + 3) = bytes[3];
            }
        }
    }
}

pub fn write(ipa: u64, sas: u8, value: u32, elr: u64) {
    // BE-8 (production): byte/halfword accesses land at the natural
    // IPA. Splice the sub-word value into the addressed lane of the
    // surrounding word so the peripheral, which dispatches at word-
    // aligned register addresses, sees the full register's post-write
    // state. Guest-test mode keeps the legacy un-XOR path.
    #[cfg(nh_guest_test)]
    let ipa = unxor_sub_word(ipa, sas);
    #[cfg(not(nh_guest_test))]
    let (ipa, value) = match sas {
        0 => {
            let aligned = ipa & !0x3;
            let prev = read(aligned, 2, elr);
            (aligned, splice_byte(prev, ipa, value))
        }
        1 => {
            let aligned = ipa & !0x3;
            let prev = read(aligned, 2, elr);
            (aligned, splice_halfword(prev, ipa, value))
        }
        _ => (ipa, value),
    };
    // Tick-page sub-word write catch-net. The tick cluster at
    // 0x0F18_1000..0x0F18_2000 is stage-2 RO (see
    // `stage2::install_tick_page`). Under BE-8 the original sub-word
    // write may have been spliced into a word at this point, but the
    // address still lies in the tick page; halt so we notice if any
    // guest code legitimately writes here. Fix when / if it fires:
    // route through `backed_*_write` on `stage2::TICK_PAGE`.
    if sas < 2 && (0x0F18_1000..0x0F18_2000).contains(&ipa) {
        kprintln!();
        kprintln!(
            "*** tick-page sub-word write reached mmio::write — \
             IPA={:#010x} size={} value={:#010x} @ELR={:#x}",
            ipa, sas_label(sas), value, elr
        );
        kprintln!(
            "  (inline stub wrote to stage-2 RO tick page. See the \
             'MMIO routing' section of the inline-stub plan — route \
             back through backed_*_write on stage2::TICK_PAGE.)"
        );
        cpu::halt();
    }
    if vic::owns(ipa) {
        vic::write(ipa, value);
        return;
    }
    if dma::owns(ipa) {
        dma::write(ipa, value);
        return;
    }
    if pcmcia::owns(ipa) {
        pcmcia::write(ipa, value);
        return;
    }
    if serial::owns(ipa) {
        serial::write(ipa, value);
        return;
    }
    // RAM-probe "absent bank" window — dropped writes, deterministic
    // (see const comment above).
    if (RAM_PROBE_ABSENT_BASE..RAM_PROBE_ABSENT_END).contains(&ipa) {
        return;
    }
    // Test-only scratch window — round-trip storage above XOR_LIMIT.
    // Checked before NO_REX_PROBE because it sits inside that range.
    if (TEST_SCRATCH_BASE..TEST_SCRATCH_END).contains(&ipa) {
        test_scratch_write(ipa, sas, value);
        return;
    }
    // Probe-for-absent-REx window — same semantics.
    if (NO_REX_PROBE_BASE..NO_REX_PROBE_END).contains(&ipa) {
        return;
    }
    // "Unknown bank #5" silent-drop (see UNKNOWN_BANK5_BASE comment).
    if (UNKNOWN_BANK5_BASE..UNKNOWN_BANK5_END).contains(&ipa) {
        return;
    }
    // Platform "write-only" control registers. Each is a Newton ASIC
    // pin-strap / bus-control / power-gate register that the kernel
    // configures once at BootOS time. Einstein's TMemory doesn't model
    // any observable state behind them — the writes are accepted and
    // never read back. TMemoryConsts.h cites the typical values in
    // comments; we model each as explicit write-accept no-ops so the
    // set of recognised addresses is a closed whitelist (Phase A),
    // not an open silent-drop fallback.
    match ipa {
        // --- Memory-controller-ish (TMemoryConsts.h ~56-67) ---
        0x0F00_1000 => {} // P0F001000        R/W, memory-access speed
        0x0F00_1800 => {} // 04RAMSize        "W (also written with 0x00 & 0x40)"
        0x0F00_1C00 => {} // 08RAMSize        W
        0x0F00_2000 => {} // P0F002000        W (0x80)
        0x0F04_3000 => {} // P0F043000        W (0x7400)
        0x0F04_3800 => {} // P0F043800        W (0x2000)
        0x0F04_8000 => {} // P0F048000        R/W (0)
        // BIO-interface register bank — see read path / `in_bio_bank`
        // for the stride + Einstein rationale. Writes are accepted as
        // no-ops.
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

        // --- Power / GPIO miscellany (0x0F18xxxx area outside VIC) ---
        // Note: 0x0F18_CC00..0x0F18_EC00 are owned by peripherals::vic
        // (in vic::owns), where their writes silently drop to match
        // Einstein's unknown-bank-#3 default. Reads return 0.

        a => halt_on_unknown("write", a, sas, value, elr),
    }
    let _ = value;
}

/// Splice a guest BE-8 byte write into the existing word at `prev`.
/// The byte goes at the IPA-selected lane: lane 0 (= IPA mod 4 == 0)
/// is bits[31:24] (MSB-side under BE-8, since the guest sees byte 0
/// of an aligned word as the MSB), lane 3 is bits[7:0].
#[cfg(not(nh_guest_test))]
fn splice_byte(prev: u32, ipa: u64, byte: u32) -> u32 {
    let lane = (ipa & 3) as u32;
    let shift = 24 - 8 * lane; // lane 0 → 24 (bits[31:24] = MSB)
    let mask = !(0xFFu32 << shift);
    (prev & mask) | ((byte & 0xFF) << shift)
}

/// Splice a guest BE-8 halfword write into the existing word at
/// `prev`. Halfword 0 (IPA aligned mod 4 == 0) is bits[31:16];
/// halfword 1 is bits[15:0].
#[cfg(not(nh_guest_test))]
fn splice_halfword(prev: u32, ipa: u64, half: u32) -> u32 {
    let lane = ((ipa >> 1) & 1) as u32;
    let shift = if lane == 0 { 16 } else { 0 };
    let mask = !(0xFFFFu32 << shift);
    (prev & mask) | ((half & 0xFFFF) << shift)
}

fn sas_label(sas: u8) -> &'static str {
    match sas {
        0 => "B",
        1 => "H",
        2 => "W",
        _ => "?",
    }
}

/// Un-XOR the BE-32 byte / halfword XOR that the inline-stub emitter
/// applies before an MMIO-range access. Only used in guest-test mode
/// (the legacy shadow-stub path). Above XOR_LIMIT (PCMCIA etc.),
/// inline stubs skip the XOR and we shouldn't un-XOR.
#[cfg(nh_guest_test)]
fn unxor_sub_word(ipa: u64, sas: u8) -> u64 {
    const XOR_LIMIT: u64 = 0x1000_0000;
    if ipa >= XOR_LIMIT { return ipa; }
    match sas {
        0 => ipa ^ 3,
        1 => ipa ^ 2,
        _ => ipa,
    }
}

fn mask_for_size(value: u32, sas: u8) -> u32 {
    match sas {
        0 => value & 0xFF,
        1 => value & 0xFFFF,
        _ => value,
    }
}

/// Per Phase A's "instrument every unknown thing" rule, any IPA that
/// isn't owned by a peripheral module or hard-coded above as a known
/// stubbed register halts here with full context. Silent drops mask
/// exactly the divergence we're trying to see — a guest write to a
/// dropped IPA whose value the kernel later reads back is one of the
/// most common ways a run-away Thumb / bad-function-pointer bug slips
/// in. Extend the peripheral modules (or add a new one) to service
/// the IPA this halts on.
fn halt_on_unknown(op: &'static str, ipa: u64, sas: u8, value: u32, elr: u64) -> ! {
    let width = match sas {
        0 => "B", 1 => "H", 2 => "W", _ => "D",
    };
    let region = if (HW_BASE..HW_END).contains(&ipa) {
        "inside 0x0F00_0000..0x0F40_0000 (Newton hardware window — add to a peripheral module)"
    } else {
        "outside known windows (unmapped IPA — decide whether to model it or widen stage-2)"
    };
    kprintln!();
    kprintln!("*** unknown MMIO {} halted ***", op);
    kprintln!(
        "  IPA    = {:#010x}  {}  value={:#010x}  @ELR={:#x}",
        ipa, width, value, elr
    );
    kprintln!("  region: {}", region);
    kprintln!(
        "  (Phase A contract: every unknown sub-case is a loud trip-wire, not a silent stub.)"
    );
    cpu::halt();
}
