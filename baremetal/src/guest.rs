//! Toy AArch32 guest and the EL2→EL1-AArch32 drop path.
//!
//! The guest is six ARM instructions that touch a deliberately-unmapped
//! stage-2 IPA and then issue HVC so we get both kinds of trap in one run:
//!
//!   e59f_0010   ldr  r0, [pc, #16]     ; r0 ← trap IPA (stage2::TRAP_IPA)
//!   e590_1000   ldr  r1, [r0]          ; *** stage-2 data abort *** (if caught)
//!   e3a0_2033   mov  r2, #0x33         ; not reached if abort halts, but if we
//!   e080_3001   add  r3, r0, #0x??     ; resume we still want legitimate ops
//!   e140_0070   hvc  #0                ; clean trap to EL2
//!   eafffffe    b .                    ; safety belt
//!   <word literal: TRAP_IPA>
//!
//! For M1.5b we halt in the data-abort handler, so the later instructions
//! are only safety-belt. If the trap page ever gets mapped, the guest
//! falls through to HVC and we still see the round-trip.

use core::arch::asm;

use crate::{cpu, kprintln, stage2};

#[repr(C, align(4))]
struct GuestImage([u32; 8]);

// Layout: 6 instructions + literal at word 6 + padding at word 7.
// `ldr r0, [pc, #16]` with PC at word 0 resolves to address (0 + 8) + 16 = 24
// which is word 6 — where the TRAP_IPA literal sits.
static GUEST: GuestImage = GuestImage([
    0xE59F_0010, // word 0: ldr r0, [pc, #16]   ; r0 ← *(pc+8+16) = word 6
    0xE590_1000, // word 1: ldr r1, [r0]        ; *** stage-2 data abort ***
    0xE3A0_2033, // word 2: mov r2, #0x33
    0xE080_3001, // word 3: add r3, r0, r1
    0xE140_0070, // word 4: hvc #0
    0xEAFF_FFFE, // word 5: b .                  (safety belt)
    0x0010_0000, // word 6: TRAP_IPA literal — must match stage2::TRAP_IPA
    0xEAFF_FFFE, // word 7: padding              (safety belt)
]);
const _: () = assert!(stage2::TRAP_IPA == 0x0010_0000);

/// Drop to EL1 AArch32 at the toy guest's entry point. Never returns — the
/// guest runs, executes HVC, and control flows into the EL2 vector table.
pub unsafe fn run_toy_guest() -> ! {
    let entry = GUEST.0.as_ptr() as u64;

    // SPSR for EL1 AArch32 SVC:
    //   M[4:0] = 0b10011   SVC mode
    //   M[4]   = 1         AArch32 (explicit, already set in M[4:0])
    //   F = I = A = 1      mask all async exceptions so the first guest
    //                      instruction won't be preempted
    //   T = 0              ARM, not Thumb
    // Result: 0x1C0 | 0x13 = 0x1D3.
    let spsr_aarch32_svc: u64 = 0x0000_01D3;

    kprintln!();
    kprintln!("Dropping to EL1 AArch32 at guest PC = {:#018x}", entry);

    // SAFETY: At this point our EL2 MMU is on, stage-2 is configured, and
    // HCR_EL2.VM is already set by stage2::enable(). We explicitly zero
    // SCTLR_EL1 so the guest enters AArch32 with its own stage-1 MMU
    // and caches off — guarantees VA == IPA, so every guest load/store
    // is routed through stage-2.
    unsafe {
        // Read current HCR_EL2 and preserve the bits stage2::enable() set
        // (VM=1 in particular). RW=0 means lower ELs run AArch32.
        let mut hcr: u64;
        asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
        hcr &= !(1u64 << 31); // RW = 0 (AArch32)
        asm!("msr hcr_el2, {}", "isb", in(reg) hcr,
            options(nostack, preserves_flags));

        // Zero SCTLR_EL1 (guest MMU / caches off). The architectural
        // RES1 bits are zero on ARMv8 AArch32 SCTLR; QEMU accepts 0.
        asm!("msr sctlr_el1, xzr", "isb",
            options(nostack, preserves_flags));

        asm!(
            "msr elr_el2, {entry}",
            "msr spsr_el2, {spsr}",
            "isb",
            "eret",
            entry = in(reg) entry,
            spsr = in(reg) spsr_aarch32_svc,
            options(noreturn),
        );
    }
}

// Exists so that if the guest ever returns without trapping (it won't,
// but: defence-in-depth), we halt rather than execute random memory.
#[allow(dead_code)]
fn _unreachable() -> ! {
    cpu::halt();
}
