//! EL2 → EL1 AArch32 guest drop.
//!
//! For M2a we boot the real Newton ROM: the guest enters at IPA 0x00000000
//! (the ROM reset vector) in AArch32 SVC mode with stage-1 MMU off. Stage-2
//! carries every load/store through the mapping installed by `stage2::init`:
//! ROM is read-only at IPA 0, RAM is R/W at IPA 0x04000000, MMIO faults.
//!
//! The toy guest (used for M1.5) is kept as a hidden alternative in case we
//! need to isolate stage-2 behaviour from real-ROM behaviour later; call
//! `run_toy_guest` instead of `run_newton_rom` for that.

use core::arch::asm;

use crate::kprintln;

#[repr(C, align(4))]
struct ToyImage([u32; 8]);

#[allow(dead_code)]
static TOY: ToyImage = ToyImage([
    0xE59F_0010,
    0xE590_1000,
    0xE3A0_2033,
    0xE080_3001,
    0xE140_0070,
    0xEAFF_FFFE,
    0x0010_0000,
    0xEAFF_FFFE,
]);

/// Enter AArch32 SVC mode at the given guest IPA. Never returns.
unsafe fn eret_to_guest(entry_ipa: u64) -> ! {
    // SPSR = AArch32 SVC, interrupts masked.
    let spsr_aarch32_svc: u64 = 0x0000_01D3;

    // SAFETY: preserves stage-2 enable; adds CP15 trap bits so the Newton's
    // ARMv4-era writes route to EL2 (where we can emulate or skip them)
    // instead of becoming undef exceptions the guest's own handler can't
    // service without its stage-1 MMU.
    unsafe {
        let mut hcr: u64;
        asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
        hcr &= !(1u64 << 31); // RW = 0 (AArch32)
        hcr |= 1u64 << 20;    // TIDCP: trap implementation-defined CP15
        hcr |= 1u64 << 26;    // TVM:   trap guest writes to virtual-memory CP15 regs
                              //        (SCTLR/TTBR/DACR change what's translated,
                              //        so we need to mediate them; we intentionally
                              //        do NOT set TRVM — guest reads of DFSR/DFAR
                              //        etc. should go straight to hardware so the
                              //        kernel's abort handler sees real fault info)
        hcr |= 1u64 << 22;    // TSW:   trap set/way cache maintenance
        hcr |= 1u64 << 3;     // FMO:   route physical FIQ to EL2 (needed for VF to deliver)
        hcr |= 1u64 << 4;     // IMO:   route physical IRQ to EL2 (needed for VI to deliver)
        hcr |= 1u64 << 5;     // AMO:   route SError to EL2
        asm!("msr hcr_el2, {}", "isb", in(reg) hcr,
            options(nostack, preserves_flags));

        // CPTR_EL2.TFP = 1 traps every AArch32 MCR/MRC/LDC/STC/CDP to
        // CP10/CP11 from lower EL to EL2 as an FP/SIMD access
        // (EC=0x07). Newton OS doesn't use FPU — it uses MCR p10 as
        // the "native primitive" call gateway (Emulator/TARMProcessor.cpp
        // :374, Emulator/TNativePrimitives.cpp:177). Trapping gets
        // every such call into peripherals::native_primitives::execute
        // where we emulate or halt loudly on unknown codes. EL2's own
        // FP is untouched (TFP only affects lower EL when E2H=0).
        let mut cptr: u64;
        asm!("mrs {}, cptr_el2", out(reg) cptr,
            options(nomem, nostack, preserves_flags));
        cptr |= 1u64 << 10;    // TFP
        asm!("msr cptr_el2, {}", "isb", in(reg) cptr,
            options(nostack, preserves_flags));

        // CPACR_EL1: enable CP10 and CP11 access at EL1. The default
        // value has both disabled, so an AArch32 MCR p10,... would
        // raise UND locally at EL1 (taking the guest's UND vector)
        // before CPTR_EL2.TFP has a chance to route it to EL2.
        // Setting FPEN = 0b11 (bits 21:20) — which also covers the
        // AArch32 CPACR cp10/cp11 fields at 23:22 / 21:20 — removes
        // that local UND so MCR p10 propagates through the TFP trap.
        // Newton doesn't use FP itself; enabling these bits is
        // side-effect-free for the guest's own code paths.
        let cpacr: u64 = (0b11 << 20) | (0b11 << 22);
        asm!("msr cpacr_el1, {}", "isb", in(reg) cpacr,
            options(nostack, preserves_flags));

        // Zero guest SCTLR_EL1 so its stage-1 is off at reset. The ROM's
        // own boot code will configure SCTLR_EL1 / TTBR0 / DACR etc. on
        // its way up.
        asm!("msr sctlr_el1, xzr", "isb",
            options(nostack, preserves_flags));

        // Zero TCR_EL1 so guest-stage-1 uses short-descriptor VMSAv7
        // format (TTBCR.EAE = 0). This is what the 717006 ROM expects;
        // A53's reset value isn't architecturally guaranteed to be 0.
        asm!("msr tcr_el1, xzr", "isb",
            options(nostack, preserves_flags));

        // Clear TTBR0_EL1 / TTBR1_EL1 to known state; ROM will write
        // its real tables via the CP15 shim.
        asm!("msr ttbr0_el1, xzr", "msr ttbr1_el1, xzr", "isb",
            options(nostack, preserves_flags));

        asm!(
            "msr elr_el2, {entry}",
            "msr spsr_el2, {spsr}",
            "isb",
            "eret",
            entry = in(reg) entry_ipa,
            spsr = in(reg) spsr_aarch32_svc,
            options(noreturn),
        );
    }
}

/// Boot the Newton ROM: drop to AArch32 SVC, PC = guest IPA 0x00000000.
pub unsafe fn run_newton_rom() -> ! {
    kprintln!();
    kprintln!("Dropping to EL1 AArch32 at guest IPA 0x00000000 (ROM reset vector)");
    // SAFETY: MMU, stage-2, vectors all up by the time the caller invokes us.
    unsafe { eret_to_guest(0x0000_0000) }
}

/// Alternative entry: the small toy guest that exercises stage-2 trap-and-halt.
/// Left in for diagnostics; not used in the M2a boot path.
#[allow(dead_code)]
pub unsafe fn run_toy_guest() -> ! {
    let entry = TOY.0.as_ptr() as u64;
    kprintln!("Dropping to EL1 AArch32 at toy-guest PC = {:#x}", entry);
    // SAFETY: as above.
    unsafe { eret_to_guest(entry) }
}
