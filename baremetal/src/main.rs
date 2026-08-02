#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(test))]
use core::arch::global_asm;

mod arch;
mod diag;
mod host;
mod hv;
mod newton;
mod panic;
mod peripherals;

#[cfg(not(test))]
global_asm!(include_str!("arch/boot.s"));
#[cfg(not(test))]
global_asm!(include_str!("arch/vectors.s"));

extern "C" {
    static el2_vector_table: u8;
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point called from `boot.s` on core 0 after stack and bss are ready.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    host::platform::init_cpu_sysregs();
    host::console::init();
    // Register the halt-path console drain: `cpu::halt` parks with
    // IRQs masked, so the DMA console ring must be drained by polling
    // or the final context dump never reaches the wire. A hook rather
    // than a direct call so `arch` keeps zero upward imports.
    arch::cpu::set_halt_flush(host::console::flush_tx_dma_polled);
    print_banner();
    print_caps();

    // SAFETY: called exactly once from boot.s on core 0 before any
    // cache- or virtual-addressing-dependent code runs. The platform's
    // memory map is passed in so `arch` stays free of upward imports.
    unsafe {
        arch::mmu::init(
            host::platform::DEVICE_MMIO_START..host::platform::DEVICE_MMIO_END,
            host::platform::DEVICE_MMIO_1GIB_BLOCK,
            host::platform::DRAM_1GIB_BLOCK,
        );
    }
    // Now that RAM is mapped Normal-WB inner-shareable, the ring's
    // atomic RMW operations (used internally by AtomicU32::swap /
    // fetch_add) can run without aborting on the Cortex-A53. Switch
    // the kprintln backend from polled to DMA. Before this line all
    // output went through the busy-wait fallback in `write_str`.
    host::console::init_dma_tx();
    install_vectors();

    // Real-hardware SDHOST bring-up probe. Halts at the end regardless
    // of outcome — see src/host/sd/probe.rs.
    #[cfg(feature = "sd-probe")]
    host::sd::probe::run();

    // Real-hardware VC framebuffer first-light probe. Same halt
    // semantics as sd-probe; build with one OR the other.
    #[cfg(feature = "fb-probe")]
    host::display::probe::run();

    // Wire the layout manifest's host-backing resolvers: `hv::layout`
    // imports nothing above arch, so the upper-layer statics that back
    // each guest region are registered here instead. `stage2::init`'s
    // cross-check halts if any region is left unwired.
    hv::layout::register_backing(hv::layout::RegionTag::Rom, hv::guest_mem::rom_host_pa);
    hv::layout::register_backing(hv::layout::RegionTag::Ram, hv::guest_mem::ram_host_pa);
    hv::layout::register_backing(
        hv::layout::RegionTag::Framebuffer,
        hv::guest_mem::fb_host_pa,
    );
    hv::layout::register_backing(
        hv::layout::RegionTag::ScratchPool,
        newton::inline_patch::scratch_pool_host_pa,
    );
    hv::layout::register_backing(hv::layout::RegionTag::Flash, peripherals::flash::host_pa);
    // Same inversion for the hypervisor-written code ranges (tracer
    // pool, patch-stub arena, trampoline tail): newton registers its
    // ranges with the manifest, `layout::is_hyp_code` serves the
    // byte-order and snapshot-gating queries.
    newton::guest_trampolines::register_hyp_code_ranges();

    // Cross-layer seams (fn-pointer inversions): guest device models
    // and the hv core consult host services only through ops installed
    // here, so the lower layers stay free of upward imports. All of
    // these must be wired before the guest runs; the flash-persist
    // backing additionally before `flash_persist::try_load` below.
    peripherals::console::install(peripherals::console::GuestConsoleOps {
        tx: host::console::write_byte,
        rx: host::console::read_byte_nonblock,
    });
    peripherals::screen::install_blit_sink(host::host_io::push_guest_blit);
    peripherals::tablet::install_pen_source(host::host_io::pop_pen_sample);
    peripherals::sound::install_audio_ops(peripherals::sound::AudioOps {
        set_interrupt_mask: host::audio::set_interrupt_mask,
        set_output_buffers: host::audio::set_output_buffers,
        schedule_output: host::audio::schedule_output,
        start_output: host::audio::start_output,
        stop_output: host::audio::stop_output,
        output_is_running: host::audio::output_is_running,
        output_volume_set: host::audio::output_volume_set,
        output_volume_get: host::audio::output_volume_get,
    });
    peripherals::flash::install_dirty_sink(host::flash_persist::mark_dirty);
    host::flash_persist::set_backing(peripherals::flash::host_pa(), peripherals::flash::SIZE);
    hv::snapshot::set_flash_provider(hv::snapshot::FlashProvider {
        maybe_save: host::flash_persist::maybe_save,
        fingerprint: host::flash_persist::fingerprint,
    });
    hv::trap::hvc::install_pen_inject(host::host_io::queue::enqueue_pen_sample);
    // The guest interrupt model rearms the EL2 timer deadline through
    // this sink when the kernel reprograms a match register.
    peripherals::vic::install_match_rearm(hv::timer::rearm);
    // Host pumps the NewtonOs trap-tail hooks drive (newton must not
    // import host directly): input pumps, audio tick, splash progress.
    newton::os::install_host_pumps(newton::os::HostPumpOps {
        host_io_pump_input: host::host_io::pump_input,
        input_pump: host::input::pump,
        audio_tick: host::audio::tick,
        #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
        splash_progress: host::display::splash::update_progress,
    });

    // SAFETY: load ROM bytes into guest backing store before stage-2 maps it.
    unsafe {
        newton::loader::load_rom();
    }

    // Bring the HDMI framebuffer up as soon as we can and paint the
    // splash (light-blue background + logo + progress bar). The bar
    // advances as the guest takes sync traps; the splash disappears
    // when the guest's first blit fires (see host::host_io::pi_fb). Built
    // only with the pi_fb host_io backend — every other backend
    // (null / semihost / pico) skips this entirely. pi_fb implies
    // platform-raspi3b in practice (the only platform with VC
    // mailbox), but be explicit so a misconfigured build doesn't
    // fall over on the missing `display` module.
    #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
    {
        host::display::splash::init();
    }

    // Bring audio up here, before the slow host::flash_persist::init load.
    // For the normal boot path this is just an early move — audio
    // doesn't depend on anything below this point. For the tone-test
    // diagnostic in `host::audio::pi_hdmi::init`, this lets the test
    // take over the CPU without waiting 5+ seconds for the 8 MiB
    // NEWTON.BIN copy from SD.
    host::audio::init();

    // Unmask EL2 physical IRQs for the rest of boot. The vector table
    // is installed (above), and the IRQ sources we drive — BCM2835 DMA
    // completions for UART TX (ch 5) and the HDMI MAI ring (ch 4), and
    // later CNTHP — now arrive as real interrupts into
    // `hv::trap::irq_from_el2` instead of cooperative polls. This is what
    // lets the 5-second SD flash load (and other long EL2 operations)
    // run without starving the HDMI audio ring.
    arch::cpu::unmask_irqs_el2();

    // Seed the Newton flash filesystem header before stage-2 exposes
    // the backing to the guest. Safe because the backing is a static
    // mut touched only from core 0 during boot.
    peripherals::flash::init();

    // Backend-specific bring-up (e.g. SDHOST init for flash-persist-sd).
    // No-op for null / semihost.
    host::flash_persist::init();
    // If a persistent flash file exists, overwrite GUEST_FLASH with
    // its contents. No-op on first boot, in guest-test mode (null
    // backend), or if the chosen backend can't reach its store.
    host::flash_persist::try_load();

    // SAFETY: stage-2 tables reference the backing store we just populated.
    unsafe {
        hv::stage2::init();
        hv::stage2::enable();
    }

    // Seed the non-trapping tick page so any read before the first
    // timer IRQ returns something non-zero-but-consistent. (Re-seeded
    // after `vic::init` below once the calendar baseline is real.)
    newton::os::seed_tick_page();

    // Seed the 10-entry ROM+REx checksum table into both blocks of
    // flash bank 0. The kernel's `TReservedBlockAccessor` reads these
    // during early init and compares against its own runtime computation
    // over the live ROM bytes. Must happen AFTER all ROM mutations
    // (rom_patches in load_rom, UND/DABT/PABT vector trampolines) so the
    // seeded checksums match what the guest will compute.
    #[cfg(not(nh_guest_test))]
    {
        peripherals::flash::seed_rom_rex_checksums(
            hv::guest_mem::rom_host_pa() as *const u32,
            hv::guest_mem::ROM_SIZE,
        );
    }

    peripherals::vic::init();
    // Re-publish the tick page now that calendar_seconds() returns a
    // real value — the post-stage-2 seed above ran while the calendar
    // baseline was still zero.
    newton::os::seed_tick_page();
    hv::timer::init();
    host::host_io::init();
    // Pull the Newton screen geometry the host-IO backend mandates
    // (pi_fb pins the MP2100-native 320×480) into the screen model;
    // `None` keeps the model's own 320×480 default.
    if let Some((w, h)) = host::host_io::panel_geometry() {
        peripherals::screen::set_screen_size(w, h);
    }
    host::input::init();

    // Seed the snapshot ring's sequence counter from existing slots
    // (so resumed runs don't reuse seq numbers), then attempt to
    // load the newest valid slot. If nothing qualifies we fall
    // through to a cold boot.
    hv::snapshot::init();
    if let Some(state) = hv::snapshot::load_latest() {
        kprintln!();
        kprintln!("Resuming guest from snapshot at PC={:#x}", state.pc);
        host::host_io::on_resume();
        // SAFETY: hv::snapshot::load already restored EL1 sysregs; we
        // configure EL2 traps and ERET to the saved PC.
        unsafe {
            hv::guest::eret_to_restored(state);
        }
    }

    kprintln!();
    kprintln!("Entering Newton ROM...");

    // SAFETY: every subsystem the guest relies on is up.
    unsafe {
        hv::guest::run_newton_rom();
    }

    // If we ever reach this (we won't) — halt so the machine is safe.
    #[allow(unreachable_code)]
    arch::cpu::halt();
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
    kprintln!(" Target: {}", host::platform::NAME);
    kprintln!(" ROM:    {}", newton::rom_ver::NAME);
    kprintln!("===============================================");
    kprintln!("Current EL: {}", arch::cpu::current_el());
    kprintln!("Core ID:    {}", arch::cpu::core_id());
}

/// Dump the capability registers we need to confirm before M1.5 — EL2
/// presence, stage-2 / virtualization support, cache and MMU granularity
/// support. We only print; interpretation is in the user-facing output.
fn print_caps() {
    let midr = arch::cpu::midr_el1();
    let pfr0 = arch::cpu::id_aa64pfr0_el1();
    let mmfr0 = arch::cpu::id_aa64mmfr0_el1();
    let mmfr1 = arch::cpu::id_aa64mmfr1_el1();
    let isar0 = arch::cpu::id_aa64isar0_el1();
    let hcr = arch::cpu::hcr_el2();

    kprintln!();
    kprintln!("--- CPU capability registers ---");
    kprintln!("MIDR_EL1          = {:#018x}", midr);
    kprintln!("  implementer     = {:#04x}", (midr >> 24) & 0xff);
    kprintln!("  part number     = {:#05x}", (midr >> 4) & 0xfff);
    kprintln!(
        "  variant/rev     = {:#x}/{:#x}",
        (midr >> 20) & 0xf,
        midr & 0xf
    );
    kprintln!("ID_AA64PFR0_EL1   = {:#018x}", pfr0);
    kprintln!(
        "  EL0             = {:#x}  (0=not, 1=AArch64, 2=AArch64+AArch32)",
        (pfr0 >> 0) & 0xf
    );
    kprintln!("  EL1             = {:#x}", (pfr0 >> 4) & 0xf);
    kprintln!(
        "  EL2             = {:#x}  (non-zero = virtualisation supported)",
        (pfr0 >> 8) & 0xf
    );
    kprintln!("  EL3             = {:#x}", (pfr0 >> 12) & 0xf);
    kprintln!("ID_AA64MMFR0_EL1  = {:#018x}", mmfr0);
    kprintln!(
        "  PARange         = {:#x}  (0=32b, 1=36b, 2=40b, 3=42b, 4=44b, 5=48b)",
        (mmfr0 >> 0) & 0xf
    );
    kprintln!(
        "  ASIDBits        = {:#x}  (0=8-bit, 2=16-bit)",
        (mmfr0 >> 4) & 0xf
    );
    kprintln!(
        "  TGran4          = {:#x}  (0=supported, F=not)",
        (mmfr0 >> 28) & 0xf
    );
    kprintln!(
        "  TGran16         = {:#x}  (1=supported, 0=not)",
        (mmfr0 >> 20) & 0xf
    );
    kprintln!(
        "  TGran64         = {:#x}  (0=supported, F=not)",
        (mmfr0 >> 24) & 0xf
    );
    kprintln!("ID_AA64MMFR1_EL1  = {:#018x}", mmfr1);
    kprintln!(
        "  HAFDBS          = {:#x}  (hardware access flag / dirty state)",
        (mmfr1 >> 0) & 0xf
    );
    kprintln!(
        "  VMIDBits        = {:#x}  (0=8-bit, 2=16-bit)",
        (mmfr1 >> 4) & 0xf
    );
    kprintln!(
        "  VH              = {:#x}  (virtualisation host extensions)",
        (mmfr1 >> 8) & 0xf
    );
    kprintln!("ID_AA64ISAR0_EL1  = {:#018x}", isar0);
    kprintln!("HCR_EL2 (current) = {:#018x}", hcr);
    kprintln!("--- end capability registers ---");
}
