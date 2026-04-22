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

/// Configure the EL2 trap bits that stay constant for the life of
/// the guest. Called unconditionally from both cold boot and
/// snapshot-resume paths; does not touch guest EL1 sysregs.
unsafe fn configure_el2_traps() {
    // SAFETY: sysreg writes at EL2, barrier in the final msr.
    unsafe {
        let mut hcr: u64;
        asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
        hcr &= !(1u64 << 31); // RW = 0 (AArch32)
        hcr |= 1u64 << 20;    // TIDCP: trap implementation-defined CP15
        hcr |= 1u64 << 26;    // TVM:   trap guest writes to virtual-memory CP15 regs
        hcr |= 1u64 << 22;    // TSW:   trap set/way cache maintenance
        hcr |= 1u64 << 3;     // FMO:   route physical FIQ to EL2
        hcr |= 1u64 << 4;     // IMO:   route physical IRQ to EL2
        hcr |= 1u64 << 5;     // AMO:   route SError to EL2
        asm!("msr hcr_el2, {}", "isb", in(reg) hcr,
            options(nostack, preserves_flags));

        // CPTR_EL2.TFP routes lower-EL FP/SIMD (and thus AArch32
        // MCR/MRC to CP10/11 — the native-primitive gateway) to EL2
        // as EC=0x07. See peripherals/native_primitives.rs.
        let mut cptr: u64;
        asm!("mrs {}, cptr_el2", out(reg) cptr,
            options(nomem, nostack, preserves_flags));
        cptr |= 1u64 << 10;
        asm!("msr cptr_el2, {}", "isb", in(reg) cptr,
            options(nostack, preserves_flags));

        // Virtualise MIDR_EL1 so guest `MRC p15, 0, Rt, c0, c0, 0`
        // reads return the StrongARM SA-1100 value Newton OS branches
        // on rather than the Cortex-A53 ID. Einstein hard-codes the
        // same value at `Emulator/TARMProcessor.cpp:99`. VPIDR_EL2
        // overrides MIDR_EL1 reads from EL1/EL0 without trapping
        // (see Arm ARM "MIDR_EL1, Main ID Register").
        asm!("msr vpidr_el2, {}", "isb",
            in(reg) 0x4401_A100u64,
            options(nostack, preserves_flags));
    }
}

/// Cold-boot EL1 state: guest stage-1 off, TTBRs cleared, CPACR
/// opened so MCR p10 can propagate through CPTR_EL2.TFP. Called only
/// on cold boot; snapshot resume restores EL1 sysregs from the file.
unsafe fn zero_el1_guest_state() {
    // SAFETY: sysreg writes before ERET; barriers serialise them.
    unsafe {
        let cpacr: u64 = (0b11 << 20) | (0b11 << 22);
        asm!("msr cpacr_el1, {}", "isb", in(reg) cpacr,
            options(nostack, preserves_flags));

        // SCTLR off — ROM boot code programs it as it brings the
        // stage-1 MMU up.
        asm!("msr sctlr_el1, xzr", "isb",
            options(nostack, preserves_flags));

        // TCR zero so guest-stage-1 uses short-descriptor VMSAv7
        // (TTBCR.EAE = 0); A53 reset isn't architecturally zero.
        asm!("msr tcr_el1, xzr", "isb",
            options(nostack, preserves_flags));

        // TTBR{0,1} to known state; ROM overwrites via the CP15 shim.
        asm!("msr ttbr0_el1, xzr", "msr ttbr1_el1, xzr", "isb",
            options(nostack, preserves_flags));
    }
}

/// Enter AArch32 SVC mode at the given guest IPA. Never returns.
unsafe fn eret_to_guest(entry_ipa: u64) -> ! {
    // SPSR = AArch32 SVC, I=F=A=1.
    let spsr_aarch32_svc: u64 = 0x0000_01D3;

    // SAFETY: caller has invoked us exactly once, after the hypervisor's
    // own MMU / stage-2 / vector setup.
    unsafe {
        configure_el2_traps();
        zero_el1_guest_state();

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

/// ERET into a guest restored from a snapshot. Assumes
/// `snapshot::load` has already re-applied EL1 sysregs (SCTLR, TTBRx,
/// TCR, DACR, VBAR, CPACR, banked SPSRs).
pub unsafe fn eret_to_restored(state: crate::snapshot::RestoreState) -> ! {
    // Widen u32 GPRs to u64 so we can pair-load with LDP.
    let mut gprs_u64: [u64; 15] = [0; 15];
    for i in 0..15 {
        gprs_u64[i] = state.gprs[i] as u64;
    }
    let pc = state.pc as u64;
    let cpsr = state.cpsr as u64;
    let ptr = gprs_u64.as_ptr() as u64;

    // SAFETY: sysreg writes to ELR_EL2 / SPSR_EL2 set the post-ERET
    // guest PC and CPSR; ERET consumes them. Named registers x24-x26
    // keep our address scratch pointers out of the x0..x14 range the
    // LDPs load into, and out of x29 (FP) which Rust reserves.
    unsafe {
        configure_el2_traps();

        asm!(
            "msr elr_el2, x24",
            "msr spsr_el2, x25",
            "isb",
            "ldp x0, x1,   [x26, #0]",
            "ldp x2, x3,   [x26, #16]",
            "ldp x4, x5,   [x26, #32]",
            "ldp x6, x7,   [x26, #48]",
            "ldp x8, x9,   [x26, #64]",
            "ldp x10, x11, [x26, #80]",
            "ldp x12, x13, [x26, #96]",
            "ldr x14,      [x26, #112]",
            "eret",
            in("x24") pc,
            in("x25") cpsr,
            in("x26") ptr,
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
