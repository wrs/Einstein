#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(test))]
use core::arch::global_asm;

mod arm_decode;
mod audio;
mod banked;
mod cpu;
mod diag_util;
#[cfg(feature = "platform-raspi3b")]
mod display;
mod flash_persist;
mod guest;
mod guest_bp;
mod guest_endian;
mod guest_mem;
mod heap_check;
mod host_io;
mod hvc_imm;
mod input;
#[cfg(feature = "platform-raspi3b")]
mod mailbox;
mod mmio;
mod mmu;
mod panic;
mod peripherals;
mod platform;
mod rep_print;
mod rom_patches;
#[cfg(feature = "platform-raspi3b")]
mod sd;
mod shadow_stub;
mod snapshot;
mod stage2;
mod symbols;
#[cfg(feature = "platform-fvp-base")]
mod tarmac;
mod task_dump;
mod timer;
#[cfg(nh_input_mtouch)]
mod usb;
#[cfg(feature = "trace")]
mod tracer;
mod trap;
mod trap_context;
mod trap_hist;
pub mod uart;
mod unaligned;
mod unaligned_inline;

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
    // Now that RAM is mapped Normal-WB inner-shareable, the ring's
    // atomic RMW operations (used internally by AtomicU32::swap /
    // fetch_add) can run without aborting on the Cortex-A53. Switch
    // the kprintln backend from polled to DMA. Before this line all
    // output went through the busy-wait fallback in `write_str`.
    uart::init_dma_tx();
    install_vectors();

    // Real-hardware SDHOST bring-up probe. Halts at the end regardless
    // of outcome — see src/sd/probe.rs.
    #[cfg(feature = "sd-probe")]
    sd::probe::run();

    // Real-hardware VC framebuffer first-light probe. Same halt
    // semantics as sd-probe; build with one OR the other.
    #[cfg(feature = "fb-probe")]
    display::probe::run();

    // SAFETY: load ROM bytes into guest backing store before stage-2 maps it.
    unsafe { guest_mem::load_rom(); }

    // Bring the HDMI framebuffer up as soon as we can and paint the
    // splash (light-blue background + logo + progress bar). The bar
    // advances as the guest takes sync traps; the splash disappears
    // when the guest's first blit fires (see host_io::pi_fb). Built
    // only with the pi_fb host_io backend — every other backend
    // (null / semihost / pico) skips this entirely. pi_fb implies
    // platform-raspi3b in practice (the only platform with VC
    // mailbox), but be explicit so a misconfigured build doesn't
    // fall over on the missing `display` module.
    #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
    {
        display::splash::init();
    }

    // Bring audio up here, before the slow flash_persist::init load.
    // For the normal boot path this is just an early move — audio
    // doesn't depend on anything below this point. For the tone-test
    // diagnostic in `audio::pi_hdmi::init`, this lets the test
    // take over the CPU without waiting 5+ seconds for the 8 MiB
    // NEWTON.BIN copy from SD.
    audio::init();

    // Unmask EL2 physical IRQs for the rest of boot. The vector table
    // is installed (above), and the IRQ sources we drive — BCM2835 DMA
    // completions for UART TX (ch 5) and the HDMI MAI ring (ch 4), and
    // later CNTHP — now arrive as real interrupts into
    // `trap::irq_from_el2` instead of cooperative polls. This is what
    // lets the 5-second SD flash load (and other long EL2 operations)
    // run without starving the HDMI audio ring.
    cpu::unmask_irqs_el2();

    // Seed the Newton flash filesystem header before stage-2 exposes
    // the backing to the guest. Safe because the backing is a static
    // mut touched only from core 0 during boot.
    peripherals::flash::init();

    // Backend-specific bring-up (e.g. SDHOST init for flash-persist-sd).
    // No-op for null / semihost.
    flash_persist::init();
    // If a persistent flash file exists, overwrite GUEST_FLASH with
    // its contents. No-op on first boot, in guest-test mode (null
    // backend), or if the chosen backend can't reach its store.
    flash_persist::try_load();

    // SAFETY: stage-2 tables reference the backing store we just populated.
    unsafe {
        stage2::init();
        stage2::enable();
    }

    // Seed the 10-entry ROM+REx checksum table into both blocks of
    // flash bank 0. The kernel's `TReservedBlockAccessor` reads these
    // during early init and compares against its own runtime computation
    // over the live ROM bytes. Must happen AFTER all ROM mutations
    // (rom_patches in load_rom, UND/DABT/PABT vector trampolines) so the
    // seeded checksums match what the guest will compute.
    #[cfg(not(nh_guest_test))]
    {
        peripherals::flash::seed_rom_rex_checksums(
            guest_mem::rom_host_pa() as *const u32,
            guest_mem::ROM_SIZE,
        );
    }

    peripherals::vic::init();
    timer::init();
    host_io::init();
    input::init();
    // audio::init() moved earlier — see comment above flash::init.

    // Seed the snapshot ring's sequence counter from existing slots
    // (so resumed runs don't reuse seq numbers), then attempt to
    // load the newest valid slot. If nothing qualifies we fall
    // through to a cold boot.
    snapshot::init();
    if let Some(state) = snapshot::load_latest() {
        kprintln!();
        kprintln!("Resuming guest from snapshot at PC={:#x}", state.pc);
        host_io::on_resume();
        // SAFETY: snapshot::load already restored EL1 sysregs; we
        // configure EL2 traps and ERET to the saved PC.
        unsafe { guest::eret_to_restored(state); }
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
