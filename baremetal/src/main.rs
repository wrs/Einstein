#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::arch::global_asm;

mod cpu;
mod guest;
mod guest_mem;
mod mmio;
mod mmu;
mod panic;
mod stage2;
mod trap;
pub mod uart;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));

extern "C" {
    static el2_vector_table: u8;
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point called from `boot.s` on core 0 after stack and bss are ready.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    uart::init();
    print_banner();
    print_caps();

    // SAFETY: called exactly once from boot.s on core 0 before any
    // cache- or virtual-addressing-dependent code runs.
    unsafe { mmu::init(); }
    install_vectors();

    // SAFETY: load ROM bytes into guest backing store before stage-2 maps it.
    unsafe { guest_mem::load_rom(); }

    // SAFETY: stage-2 tables reference the backing store we just populated.
    unsafe {
        stage2::init();
        stage2::enable();
    }

    kprintln!();
    kprintln!("Entering Newton ROM...");

    // SAFETY: every subsystem the guest relies on is up.
    unsafe { guest::run_newton_rom(); }

    // If we ever reach this (we won't) — halt so the machine is safe.
    #[allow(unreachable_code)]
    cpu::halt();
}

fn install_vectors() {
    // SAFETY: `el2_vector_table` is defined in vectors.s, is 2 KiB-aligned
    // per the `.balign 0x800` there, and lives in rodata/text that the
    // stage-1 identity map covers.
    let vbar: u64 = unsafe { &el2_vector_table as *const u8 as u64 };
    // SAFETY: writing VBAR_EL2 only takes effect on the next exception;
    // isb ensures the write is visible before we return.
    unsafe {
        core::arch::asm!(
            "msr vbar_el2, {}",
            "isb",
            in(reg) vbar,
            options(nostack, preserves_flags),
        );
    }
    kprintln!("VBAR_EL2 = {:#018x}", vbar);
}

fn print_banner() {
    kprintln!();
    kprintln!("===============================================");
    kprintln!(" Newton Hypervisor v{}  (baremetal, M0)", VERSION);
    kprintln!(" Target: Cortex-A53 / BCM2837 (Pi 3B, Zero 2 W)");
    kprintln!("===============================================");
    kprintln!("Current EL: {}", cpu::current_el());
    kprintln!("Core ID:    {}", cpu::core_id());
}

/// Dump the capability registers we need to confirm before M1.5 — EL2
/// presence, stage-2 / virtualization support, cache and MMU granularity
/// support. We only print; interpretation is in the user-facing output.
fn print_caps() {
    let midr = cpu::midr_el1();
    let pfr0 = cpu::id_aa64pfr0_el1();
    let mmfr0 = cpu::id_aa64mmfr0_el1();
    let mmfr1 = cpu::id_aa64mmfr1_el1();
    let isar0 = cpu::id_aa64isar0_el1();
    let hcr = cpu::hcr_el2();

    kprintln!();
    kprintln!("--- CPU capability registers ---");
    kprintln!("MIDR_EL1          = {:#018x}", midr);
    kprintln!("  implementer     = {:#04x}", (midr >> 24) & 0xff);
    kprintln!("  part number     = {:#05x}", (midr >> 4) & 0xfff);
    kprintln!("  variant/rev     = {:#x}/{:#x}", (midr >> 20) & 0xf, midr & 0xf);
    kprintln!("ID_AA64PFR0_EL1   = {:#018x}", pfr0);
    kprintln!("  EL0             = {:#x}  (0=not, 1=AArch64, 2=AArch64+AArch32)", (pfr0 >> 0) & 0xf);
    kprintln!("  EL1             = {:#x}", (pfr0 >> 4) & 0xf);
    kprintln!("  EL2             = {:#x}  (non-zero = virtualisation supported)", (pfr0 >> 8) & 0xf);
    kprintln!("  EL3             = {:#x}", (pfr0 >> 12) & 0xf);
    kprintln!("ID_AA64MMFR0_EL1  = {:#018x}", mmfr0);
    kprintln!("  PARange         = {:#x}  (0=32b, 1=36b, 2=40b, 3=42b, 4=44b, 5=48b)", (mmfr0 >> 0) & 0xf);
    kprintln!("  ASIDBits        = {:#x}  (0=8-bit, 2=16-bit)", (mmfr0 >> 4) & 0xf);
    kprintln!("  TGran4          = {:#x}  (0=supported, F=not)", (mmfr0 >> 28) & 0xf);
    kprintln!("  TGran16         = {:#x}  (1=supported, 0=not)", (mmfr0 >> 20) & 0xf);
    kprintln!("  TGran64         = {:#x}  (0=supported, F=not)", (mmfr0 >> 24) & 0xf);
    kprintln!("ID_AA64MMFR1_EL1  = {:#018x}", mmfr1);
    kprintln!("  HAFDBS          = {:#x}  (hardware access flag / dirty state)", (mmfr1 >> 0) & 0xf);
    kprintln!("  VMIDBits        = {:#x}  (0=8-bit, 2=16-bit)", (mmfr1 >> 4) & 0xf);
    kprintln!("  VH              = {:#x}  (virtualisation host extensions)", (mmfr1 >> 8) & 0xf);
    kprintln!("ID_AA64ISAR0_EL1  = {:#018x}", isar0);
    kprintln!("HCR_EL2 (current) = {:#018x}", hcr);
    kprintln!("--- end capability registers ---");
}
