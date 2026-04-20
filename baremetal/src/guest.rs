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

    // SAFETY: preserves stage-2 enable; only sets RW=0 for AArch32 execution.
    unsafe {
        let mut hcr: u64;
        asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
        hcr &= !(1u64 << 31); // RW = 0 (AArch32)
        asm!("msr hcr_el2, {}", "isb", in(reg) hcr,
            options(nostack, preserves_flags));

        // Zero guest SCTLR_EL1 so its stage-1 is off. The ROM's own
        // boot code will configure SCTLR_EL1 / TTBR0 / DACR etc. on
        // its way up.
        asm!("msr sctlr_el1, xzr", "isb",
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
