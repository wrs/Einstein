//! EL2 → EL1 AArch32 guest drop.
//!
//! For M2a we boot the real Newton ROM: the guest enters at IPA 0x00000000
//! (the ROM reset vector) in AArch32 SVC mode with stage-1 MMU off. Stage-2
//! carries every load/store through the mapping installed by `stage2::init`:
//! ROM is read-only at IPA 0, RAM is R/W at IPA 0x04000000, MMIO faults.

use core::arch::asm;

use crate::kprintln;

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
        // TPC / TPU stay clear (iter-58). Newton's flash-store init
        // hot path drives `CleanRangeInDCSWIGlue` (0x18ae8) and
        // `FlushDataCache__11TFlashRangeCFUlT1`, which iterate 32-byte
        // cache lines emitting three CP15 c7 ops per line
        // (DCCMVAC / DSB / DCIMVAC). Trapping them to EL2 turned a
        // millisecond-scale loop into the dominant trap source (75% of
        // beacon samples) and stalled cold boot inside DiagBootStub.
        // Einstein's `TARMProcessor::SystemCoprocRegisterTransfer`
        // case 7 is a silent no-op (TARMProcessor.cpp:253); on
        // Cortex-A53 in AArch32, DC-by-VA / IC-by-VA on an unmapped
        // VA is implementation-defined and treated as a no-op (per
        // the Cortex-A53 TRM, matching the SA-1100 semantics the
        // Newton kernel relies on for `CleanPageInDcache`-on-unmapped-
        // VA before L2-entry population). So we let those ops run
        // natively at EL1.
        //
        // (FVP Base RevC may behave differently — keep this in mind
        // if the FVP path regresses; we'd add a stage-1 translation-
        // fault filter that no-ops the fault when ELR points at a
        // c7 cache-maintenance MCR.)
        hcr |= 1u64 << 12;    // DC:    force Normal-WB cacheable while
                              //        guest stage-1 MMU is off. Without
                              //        this the guest's reset-time
                              //        accesses are Normal Non-cacheable
                              //        (AArch32) or Device (AArch64),
                              //        which makes them incoherent with
                              //        the hypervisor's own Normal-WB
                              //        view of the same DRAM. DC=1 is
                              //        NOT harmless once the guest turns
                              //        its MMU on: per DDI 0487 D13.2.50
                              //        it also forces SCTLR_EL1.M to
                              //        behave as 0 for the Non-secure
                              //        EL1&0 translation regime — i.e.
                              //        the guest's stage-1 page tables
                              //        are ignored from EL2's side. So
                              //        `set_dc_for_stage1_off(false)` is
                              //        called at the M=0→M=1 rising edge
                              //        (and the inverse at the falling
                              //        edge of a soft reboot).
        hcr |= 1u64 << 3;     // FMO:   route physical FIQ to EL2
        hcr |= 1u64 << 4;     // IMO:   route physical IRQ to EL2
        hcr |= 1u64 << 5;     // AMO:   route SError to EL2
        asm!("msr hcr_el2, {}", "isb", in(reg) hcr,
            options(nostack, preserves_flags));

        // Set AArch32 FPEXC.EN = 1 on behalf of the guest before
        // touching CPTR_EL2.TFP. Without EN=1 the guest's first
        // access to CP10/CP11 (any VFP or MCR p10 / MRC p10 — the
        // native-primitive gateway) UNDEFs at EL1 *before*
        // CPTR_EL2.TFP has a chance to trap it. Must happen before
        // TFP is set: writing FPEXC32_EL2 at EL2 with TFP=1 would
        // trap to EL2 itself and recurse.
        asm!(
            "msr S3_4_C5_C3_0, {}",   // FPEXC32_EL2
            "isb",
            in(reg) 1u64 << 30,       // EN
            options(nostack, preserves_flags),
        );

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

/// Toggle HCR_EL2.DC. Set (true) while the guest's stage-1 MMU is
/// disabled to force its data accesses to Normal-WB so they share the
/// hypervisor's cache view of DRAM. Cleared (false) at the M=0→M=1
/// transition: with DC=1 the guest's own stage-1 translation is
/// suppressed from EL2's side (DDI 0487 D13.2.50), which breaks every
/// guest VA→IPA that differs from identity — e.g. the post-MMU UND
/// trampoline's save slot at VA 0x0C00_4F00 → IPA 0x0400_5F00.
pub fn set_dc_for_stage1_off(enable: bool) {
    // SAFETY: single bit in HCR_EL2; read-modify-write, ISB barrier.
    unsafe {
        let mut hcr: u64;
        asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
        if enable {
            hcr |= 1u64 << 12;
        } else {
            hcr &= !(1u64 << 12);
        }
        asm!("msr hcr_el2, {}", "isb", in(reg) hcr,
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

        // SCTLR mostly off; the rest the ROM/test programs as it brings
        // the stage-1 MMU up. In normal (non-guest-test) builds we set
        // EE (bit 25) + E0E (bit 24) so AArch32 EL1/EL0 data accesses
        // run in BE-8 mode (matching the BE-8 migration). In guest-test
        // mode the test binaries are AArch32 LE flat images; leave
        // SCTLR=0 (LE).
        #[cfg(not(nh_guest_test))]
        let sctlr: u64 = (1u64 << 25) | (1u64 << 24); // EE | E0E
        #[cfg(nh_guest_test)]
        let sctlr: u64 = 0;
        asm!("msr sctlr_el1, {}", "isb", in(reg) sctlr,
            options(nostack, preserves_flags));

        // TCR zero so guest-stage-1 uses short-descriptor VMSAv7
        // (TTBCR.EAE = 0); A53 reset isn't architecturally zero.
        asm!("msr tcr_el1, xzr", "isb",
            options(nostack, preserves_flags));

        // TTBR{0,1} to known state; ROM overwrites via the CP15 shim.
        asm!("msr ttbr0_el1, xzr", "msr ttbr1_el1, xzr", "isb",
            options(nostack, preserves_flags));

        // VBAR_EL1 (AArch32 guest legacy vector base) to 0. The Newton
        // guest needs its vectors at VA 0; trusting whatever the
        // firmware left in VBAR_EL1 would send the first guest
        // exception into the weeds. (Snapshot resume restores the saved
        // value separately; this is the cold-boot guarantee.)
        asm!("msr vbar_el1, xzr", "isb",
            options(nostack, preserves_flags));
    }
}

/// Enter AArch32 SVC mode at the given guest IPA. Never returns.
unsafe fn eret_to_guest(entry_ipa: u64) -> ! {
    // SPSR = AArch32 SVC, I=F=A=1. In normal (non-guest-test) builds
    // we additionally set bit 9 (E) so the guest starts with CPSR.E=1
    // (BE-8 data accesses), matching SCTLR_EL1.EE=1 set above. Guest-
    // test images run in LE so leave E=0 there.
    #[cfg(not(nh_guest_test))]
    let spsr_aarch32_svc: u64 = 0x0000_03D3;
    #[cfg(nh_guest_test)]
    let spsr_aarch32_svc: u64 = 0x0000_01D3;

    // SAFETY: caller has invoked us exactly once, after the hypervisor's
    // own MMU / stage-2 / vector setup.
    unsafe {
        configure_el2_traps();
        zero_el1_guest_state();

        // Point-of-unification sync. `guest_mem::load_rom` /
        // `load_guest_test` wrote instruction bytes into Normal-WB DRAM
        // through the data-cache path; the guest's first fetch comes
        // through the I-cache. Without a DSB+IC+ISB the guest can fetch
        // stale (zero) bytes on platforms that honour I/D cache
        // separation — we saw this as an EC=0x20 abort at IPA 0x1000000
        // on FVP_Base_RevC (guest fell through 16 MiB of zero-NOPs to
        // the end of the ROM aperture). HCR_EL2.DC (set above) keeps
        // the guest's data accesses Normal-WB cacheable while its
        // stage-1 MMU is off, so they share our cache and no data-
        // side flush is needed.
        asm!(
            "dsb ish",
            "ic iallu",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );

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
///
/// Per ARM ARM DDI 0487 D1.21.1 Table D1-79, the AArch64 GPR file
/// X0..X30 aliases AArch32 R0..R12 plus every banked SP/LR by name:
/// X13/X14 = SP_usr/LR_usr, X18/X19 = LR_svc/SP_svc, X20/X21 =
/// LR_abt/SP_abt, X22/X23 = LR_und/SP_und, X16/X17 = LR_irq/SP_irq,
/// X24..X28 = R8_fiq..R12_fiq, X29/X30 = SP_fiq/LR_fiq. So loading
/// the saved 31-entry GPR view directly into x0..x30 and ERET-ing
/// makes every banked register land in the right slot for the
/// AArch32 mode SPSR_EL2 names. No banked sysreg dance needed.
pub unsafe fn eret_to_restored(state: crate::snapshot::RestoreState) -> ! {
    // Widen u32 GPRs to u64 so we can pair-load with LDP.
    let mut gprs_u64: [u64; 31] = [0; 31];
    for i in 0..31 {
        gprs_u64[i] = state.gprs[i] as u64;
    }
    let pc = state.pc as u64;
    let cpsr = state.cpsr as u64;
    let ptr = gprs_u64.as_ptr() as u64;

    // SAFETY: ELR_EL2 / SPSR_EL2 set the post-ERET guest PC and CPSR;
    // ERET consumes them. The ldp sequence loads x24..x26 LAST so we
    // can keep using x26 as the base pointer through the whole
    // sequence, then overwrite scratch values from the snapshot in
    // the final two instructions.
    unsafe {
        configure_el2_traps();

        // Match the M=0→M=1 transition the cold-boot path takes when
        // the guest first turns its stage-1 MMU on (see the
        // `(0, 1, 0, 0, false)` CP15-write handler in `trap.rs` at the
        // `was_off && now_on` branch). Snapshots are saved deep into the
        // boot, well past that transition, so SCTLR_EL1.M is already 1
        // when the snapshot loads — but `configure_el2_traps` just
        // unconditionally re-asserted HCR_EL2.DC=1, which per DDI 0487
        // D13.2.50 forces the Non-secure EL1&0 stage-1 translation
        // regime to behave as `SCTLR_EL1.M=0`. The result is every
        // guest VA passing through stage-2 as if it were the IPA,
        // followed by a stage-2 translation fault (the common
        // "unknown MMIO read at 0x0c100…" resume failure mode).
        //
        // Drop DC iff the restored SCTLR says the guest had its MMU
        // on at save time. The one-shot RAM-side setups the live
        // transition handler does (XN-bit rewrite, scratch-pool L1
        // section install) are already baked into the snapshot's RAM
        // image, so we don't need to repeat them here.
        let sctlr_el1: u64;
        asm!("mrs {}, sctlr_el1", out(reg) sctlr_el1,
            options(nomem, nostack, preserves_flags));
        if sctlr_el1 & 1 != 0 {
            set_dc_for_stage1_off(false);
        }

        asm!(
            "msr elr_el2, x24",
            "msr spsr_el2, x25",
            "isb",
            // x0..x14 (R0..R12 + SP_usr + LR_usr).
            "ldp x0, x1,    [x26, #0]",
            "ldp x2, x3,    [x26, #16]",
            "ldp x4, x5,    [x26, #32]",
            "ldp x6, x7,    [x26, #48]",
            "ldp x8, x9,    [x26, #64]",
            "ldp x10, x11,  [x26, #80]",
            "ldp x12, x13,  [x26, #96]",
            "ldr x14,       [x26, #112]",
            // x15..x23: SP_hyp + banked LR/SP for IRQ/SVC/ABT/UND.
            "ldr x15,       [x26, #120]",
            "ldp x16, x17,  [x26, #128]",
            "ldp x18, x19,  [x26, #144]",
            "ldp x20, x21,  [x26, #160]",
            "ldp x22, x23,  [x26, #176]",
            // x27..x30 first (FIQ-banked R10..R12, SP_fiq, LR_fiq);
            // x26 must be the last write so it stays valid as the
            // base for the previous loads.
            "ldp x27, x28,  [x26, #216]",
            "ldp x29, x30,  [x26, #232]",
            // Finally x24..x26 (FIQ-banked R8/R9 + R10's neighbour);
            // last instruction overwrites x26 with the saved value.
            "ldp x24, x25,  [x26, #192]",
            "ldr x26,       [x26, #208]",
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
