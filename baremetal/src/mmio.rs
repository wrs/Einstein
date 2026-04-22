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

use crate::{cpu, kprintln, peripherals::{dma, pcmcia, serial, vic}};

const HW_BASE: u64 = 0x0F00_0000;
const HW_END: u64 = 0x0F40_0000;

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

pub fn read(ipa: u64, sas: u8, elr: u64) -> u32 {
    let value = match ipa {
        a if vic::owns(a) => vic::read(a),
        a if dma::owns(a) => dma::read(a),
        a if pcmcia::owns(a) => pcmcia::read(a),
        a if serial::owns(a) => serial::read(a),

        HW_RAM_SIZE_1 => 0x4040_0040,
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

        // kHdWr_P0F241000: adjacent to the chipset-rev register and
        // read by the same probe. No TMemoryConsts comment; return 0
        // to match Einstein's TMemory default (read-zero on unmodelled).
        0x0F24_1000 => 0,

        // kHdWr_P0F048000: R/W, typical value 0. TMemoryConsts.h:63.
        0x0F04_8000 => 0,

        // GPIO input (PCMCIA door-lock + misc sense lines).
        // Einstein returns all-ones = "no cards / switches open".
        0x0F18_D400 => 0xFFFF_FFFF,

        // Power status: 0x0F184C00 read as "all-ok high" per Einstein.
        0x0F18_4C00 => 0xFFFF_FFFF,

        // IOPower1 / IOPower2. TMemoryConsts.h labels these as W-only
        // but the kernel's EarlyIOPowerOn does a read-modify-write
        // (OR 0x30 / 0x10). Return 0 so the OR yields the intended
        // "power on" bit pattern the kernel writes back. If a later
        // code path relies on specific preserved bits we'll halt on
        // the subsequent divergence.
        0x0F18_E800 => 0,
        0x0F18_EC00 => 0,

        // RAM-probe "absent bank" window (see const comment above).
        a if (RAM_PROBE_ABSENT_BASE..RAM_PROBE_ABSENT_END).contains(&a) => 0,

        // REx / extra-flash "absent" probe window (see const comment).
        a if (NO_REX_PROBE_BASE..NO_REX_PROBE_END).contains(&a) => 0,

        a => halt_on_unknown("read", a, sas, 0, elr),
    };

    mask_for_size(value, sas)
}

pub fn write(ipa: u64, sas: u8, value: u32, elr: u64) {
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
    // Probe-for-absent-REx window — same semantics.
    if (NO_REX_PROBE_BASE..NO_REX_PROBE_END).contains(&ipa) {
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
        0x0F05_2C00 => {} // P0F052C00        R/W (0x4E)
        0x0F05_3000 => {} // P0F053000        R/W (0x7000)
        0x0F05_3400 => {} // P0F053400        R/W (0x8C00)
        0x0F05_3800 => {} // P0F053800        R/W (0)
        0x0F05_4400 => {} // P0F054400        W (0x8400)
        0x0F05_4800 => {} // P0F054800        W (0x8400)
        0x0F05_5000 => {} // P0F055000        W (0x8400)

        // --- External data-abort / bank-control / chip-rev area ---
        0x0F24_0000 => {} // ExtDataAbt1      R (write path accepted no-op)
        0x0F24_0400 => {} // ExtDataAbt2      W
        0x0F24_0800 => {} // ExtDataAbt3      W
        0x0F24_1000 => {} // BankCtrlReg      R/W
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
        0x0F18_CC00 => {} // P0F18CC00        W (0x103)
        0x0F18_D000 => {} // P0F18D000        W (0x0F)
        0x0F18_D800 => {} // P0F18D800        W (0)
        0x0F18_DC00 => {} // P0F18DC00        W (0x1EF0, 0xFFFF0FF0)
        // kHdWr_P0F18E000 isn't in TMemoryConsts.h but sits between
        // GPIO_CReg (0x0F18C800) and IOPower1 (0x0F18E800). The ROM
        // writes it from the same setup routine; treat as an extended
        // power/GPIO register and no-op the write.
        0x0F18_E000 => {}
        0x0F18_E800 => {} // IOPower1         W (EarlyIOPowerOn | 0x30)
        0x0F18_EC00 => {} // IOPower2         W (EarlyIOPowerOn | 0x10)

        a => halt_on_unknown("write", a, sas, value, elr),
    }
    let _ = value;
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
