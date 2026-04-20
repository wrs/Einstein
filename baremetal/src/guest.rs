//! Toy AArch32 guest and the EL2→EL1-AArch32 drop path.
//!
//! The guest is five ARM instructions that move a few constants, do an
//! add, then `HVC #0` to fall back into the hypervisor. The bytes below
//! are the assembled ARM encodings:
//!
//!   e3a0_0011   mov  r0, #0x11
//!   e3a0_1022   mov  r1, #0x22
//!   e3a0_2033   mov  r2, #0x33
//!   e080_3001   add  r3, r0, r1
//!   e140_0070   hvc  #0
//!
//! Embedded in the hypervisor image as a 4-byte-aligned rodata array so
//! its physical address is stable and reachable through the EL2 stage-1
//! identity map. We ERET to that address in AArch32 SVC mode and watch
//! the HVC land in the EL2 vector table.

use core::arch::asm;

use crate::{cpu, kprintln};

#[repr(C, align(4))]
struct GuestImage([u32; 5]);

static GUEST: GuestImage = GuestImage([
    0xE3A0_0011,
    0xE3A0_1022,
    0xE3A0_2033,
    0xE080_3001,
    0xE140_0070,
]);

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

    // Configure HCR_EL2:
    //   RW = 0   (bit 31) — lower ELs execute AArch32
    //   All other bits 0 for M1.5a: no stage-2, no interrupt routing, no
    //   trapping of guest system instructions. Those land in M1.5b and
    //   later.
    let hcr_val: u64 = 0;

    kprintln!();
    kprintln!("Dropping to EL1 AArch32 at guest PC = {:#018x}", entry);
    kprintln!("Guest will run 4 ops and HVC #0; expect trap back at EL2.");

    // SAFETY: writing HCR_EL2, ELR_EL2, SPSR_EL2 and ERETing is exactly
    // the documented path to take a return from an exception level we're
    // not currently handling. The caller pattern (called once from kmain
    // after MMU/caches/vectors are up) is part of the contract.
    unsafe {
        asm!(
            "msr hcr_el2, {hcr}",
            "msr elr_el2, {entry}",
            "msr spsr_el2, {spsr}",
            "isb",
            "eret",
            hcr = in(reg) hcr_val,
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
