//! EL2 trap handlers.
//!
//! The vector table in `vectors.s` dispatches to one of these on any
//! exception taken to EL2. For M1.5a every handler is terminal — we print
//! and halt. Full context save/restore and resume comes in M1.5b when we
//! wire the real stage-2 path.

use crate::{cpu, kprintln};

/// Generic fatal handler for vectors we don't expect to take in M1.5a.
#[no_mangle]
pub extern "C" fn trap_unexpected() -> ! {
    kprintln!();
    kprintln!("*** UNEXPECTED TRAP AT EL2 ***");
    dump_trap_state();
    cpu::halt();
}

/// Synchronous exception from a lower EL running AArch32 — this is where
/// our toy guest's `HVC #0` lands, and also where stage-2 data aborts
/// from the guest arrive.
#[no_mangle]
pub extern "C" fn trap_from_guest_aarch32() -> ! {
    let esr = read_esr_el2();
    let elr = read_elr_el2();
    let spsr = read_spsr_el2();
    let ec = (esr >> 26) & 0x3f;

    kprintln!();
    kprintln!("*** EL2 trap from AArch32 guest ***");
    kprintln!("ESR_EL2  = {:#018x}", esr);
    kprintln!("  EC     = {:#x}  ({})", ec, describe_ec(ec));
    kprintln!("  IL     = {}", (esr >> 25) & 1);
    kprintln!("  ISS    = {:#x}", esr & 0x01ff_ffff);
    kprintln!("ELR_EL2  = {:#018x}  (guest PC of the trapping insn)", elr);
    kprintln!("SPSR_EL2 = {:#018x}  (guest CPSR at trap time)", spsr);

    if ec == 0x24 || ec == 0x20 {
        // Data abort (0x24) or instruction abort (0x20) from the lower EL.
        // Both encode IPA information in HPFAR_EL2 and FAR_EL2.
        let far = read_far_el2();
        let hpfar = read_hpfar_el2();
        // HPFAR_EL2.FIPA = IPA[47:12], stored in bits [43:4] of the reg.
        let ipa_hi = (hpfar >> 4) << 12;
        let ipa = ipa_hi | (far & 0xFFF);
        kprintln!("FAR_EL2   = {:#018x}  (guest VA)", far);
        kprintln!("HPFAR_EL2 = {:#018x}  (IPA[47:12]<<4)", hpfar);
        kprintln!("           -> reconstructed IPA = {:#018x}", ipa);
        let iss = esr & 0x01ff_ffff;
        if ec == 0x24 {
            kprintln!("Data abort ISS decode:");
            kprintln!("  ISV  = {}     (instruction syndrome valid)", (iss >> 24) & 1);
            if (iss >> 24) & 1 != 0 {
                kprintln!("  SAS  = {}     (0=byte, 1=half, 2=word, 3=dword)", (iss >> 22) & 3);
                kprintln!("  SSE  = {}     (sign-extended)", (iss >> 21) & 1);
                kprintln!("  SRT  = {}    (guest register operand)", (iss >> 16) & 0x1f);
                kprintln!("  SF   = {}     (64-bit operand)", (iss >> 15) & 1);
                kprintln!("  AR   = {}     (acquire/release)", (iss >> 14) & 1);
            }
            kprintln!("  WnR  = {}     (0=read, 1=write)", (iss >> 6) & 1);
            kprintln!("  DFSC = {:#x}   (data fault status code)", iss & 0x3f);
        }
    }

    kprintln!();
    kprintln!("Halting after trap.");
    cpu::halt();
}

fn dump_trap_state() {
    kprintln!("ESR_EL2  = {:#018x}", read_esr_el2());
    kprintln!("ELR_EL2  = {:#018x}", read_elr_el2());
    kprintln!("SPSR_EL2 = {:#018x}", read_spsr_el2());
    kprintln!("FAR_EL2  = {:#018x}", read_far_el2());
}

fn describe_ec(ec: u64) -> &'static str {
    match ec {
        0x00 => "Unknown reason",
        0x0E => "Illegal execution state",
        0x11 => "SVC from AArch32",
        0x12 => "HVC from AArch32",
        0x13 => "SMC from AArch32",
        0x15 => "SVC from AArch64",
        0x16 => "HVC from AArch64",
        0x17 => "SMC from AArch64",
        0x18 => "Trapped MSR/MRS/system instruction",
        0x20 => "Instruction abort from lower EL",
        0x21 => "Instruction abort from current EL",
        0x22 => "PC alignment fault",
        0x24 => "Data abort from lower EL",
        0x25 => "Data abort from current EL",
        0x26 => "SP alignment fault",
        0x3C => "BRK instruction",
        _ => "other",
    }
}

macro_rules! read_el2 {
    ($name:ident, $reg:literal) => {
        fn $name() -> u64 {
            let v: u64;
            // SAFETY: reading an EL2 sysreg has no side effects.
            unsafe {
                core::arch::asm!(
                    concat!("mrs {}, ", $reg),
                    out(reg) v,
                    options(nomem, nostack, preserves_flags),
                );
            }
            v
        }
    };
}

read_el2!(read_esr_el2, "esr_el2");
read_el2!(read_elr_el2, "elr_el2");
read_el2!(read_spsr_el2, "spsr_el2");
read_el2!(read_far_el2, "far_el2");
read_el2!(read_hpfar_el2, "hpfar_el2");
