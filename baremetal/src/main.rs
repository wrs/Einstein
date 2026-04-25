#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(test))]
use core::arch::global_asm;

mod cpu;
mod guest;
mod guest_bp;
mod guest_mem;
mod mmio;
mod mmu;
mod panic;
mod peripherals;
mod platform;
mod rom_patches;
mod shadow_stub;
mod snapshot;
mod stage2;
mod tarmac;
mod task_dump;
mod timer;
#[cfg(feature = "trace")]
mod tracer;
mod trap;
pub mod uart;
mod unaligned;

#[cfg(not(test))]
global_asm!(include_str!("boot.s"));
#[cfg(not(test))]
global_asm!(include_str!("vectors.s"));

extern "C" {
    static el2_vector_table: u8;
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point called from `boot.s` on core 0 after stack and bss are ready.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    platform::init_cpu_sysregs();
    uart::init();
    print_banner();
    print_caps();

    // SAFETY: called exactly once from boot.s on core 0 before any
    // cache- or virtual-addressing-dependent code runs.
    unsafe { mmu::init(); }
    install_vectors();

    // SAFETY: load ROM bytes into guest backing store before stage-2 maps it.
    unsafe { guest_mem::load_rom(); }

    // Seed the Newton flash filesystem header before stage-2 exposes
    // the backing to the guest. Safe because the backing is a static
    // mut touched only from core 0 during boot.
    peripherals::flash::init();

    // Seed the 10-entry ROM+REx checksum table into both blocks of
    // flash bank 0. The kernel's `TReservedBlockAccessor` reads these
    // during early init.
    #[cfg(not(nh_guest_test))]
    {
        peripherals::flash::seed_rom_rex_checksums(
            guest_mem::rom_host_pa() as *const u32,
            guest_mem::ROM_SIZE,
        );
    }

    // SAFETY: stage-2 tables reference the backing store we just populated.
    unsafe {
        stage2::init();
        stage2::enable();
    }

    // Pre-patch every ROM site the classify-rom bitmap marked as an
    // endianness-sensitive subword access. Must happen after
    // stage2::enable() (the bitmap path writes through the ROM backing
    // and branches into the shadow-stub pool IPAs, both of which need
    // stage-2 live so the subsequent guest fetches land correctly) but
    // before the guest runs, so the kernel's early init — including
    // the `STRH #0, [gGlobals, #0x20]` at RExScanner entry — already
    // sees the BE-32 semantics it expects. Skipped in guest-test mode
    // because the ROM slot holds a test binary, not Newton 2.x; the
    // bitmap's hash check would reject the mismatch anyway.
    #[cfg(not(nh_guest_test))]
    {
        let stats = shadow_stub::patch_rom_from_bitmap();
        shadow_stub::log_stats(&stats);
    }

    peripherals::vic::init();
    timer::init();

    // Seed the snapshot ring's sequence counter from existing slots
    // (so resumed runs don't reuse seq numbers), then attempt to
    // load the newest valid slot. If nothing qualifies we fall
    // through to a cold boot.
    snapshot::init();
    if let Some(state) = snapshot::load_latest() {
        kprintln!();
        kprintln!("Resuming guest from snapshot at PC={:#x}", state.pc);
        // SAFETY: snapshot::load already restored EL1 sysregs; we
        // configure EL2 traps and ERET to the saved PC.
        unsafe { guest::eret_to_restored(state); }
    }

    // Auto-install one-shot BPs to dump processor state and memory at
    // two key points on the PrimGetEnvDomainName path — post-both-STRBs
    // in the kernel, and post-LDRB in USR — so we can compare byte flag
    // state with Einstein at the same cycle.
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
    kprintln!(" Target: {}", platform::NAME);
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
