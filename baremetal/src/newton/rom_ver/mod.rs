//! Per-ROM-version constants, selected by the `rom-*` cargo feature
//! (the `host::platform` `#[path]` pattern). Everything that is a fact
//! about a *specific ROM build* lives behind this module: code
//! addresses, probe PCs, kernel-global VAs, REx placement, ROM-tail
//! placement budgets, the load-time patch tables. Facts about the
//! MP2x00 *hardware* (bus apertures, MMIO map, VIC, RAM base) stay in
//! `hv::layout` / `peripherals` — see the taxonomy note in the plan
//! and the doc comment on `hv::layout::high_rom_end`.
//!
//! The explicit re-export list below IS the version contract: a new
//! version module that misses an item fails to compile naming it.
//! `mod imp` is private — consumers name items through `rom_ver`, never
//! a version module directly.
//!
//! Layering: `newton::rom_ver` is newton-layer. `hv` reaches the few
//! values it needs (FPE redirect target, UND diag hints) through the
//! `GuestOs` hooks; `diag` imports this module directly (diag sits
//! atop every layer).

pub mod types;

mod common_2x;

#[cfg(feature = "rom-717006")]
#[path = "r717006/mod.rs"]
mod imp;

#[cfg(feature = "rom-710031")]
#[path = "r710031/mod.rs"]
mod imp;

// The version contract. Grouped as: identity / image geometry,
// ROM-tail placement, load-time patch tables, probe sites,
// fault-handler entry points, diagnostics anchors, 2.x-shared
// defaults.
pub use imp::{
    // Identity / image geometry.
    NAME, ROM_IMAGE_SIZE, ROM_CODE_END, REX,
    // Hypervisor placement in the ROM-aperture tail.
    ROM_TAIL,
    // Load-time patch tables.
    PATCHES, NS_TRACE_PATCH,
    // Probe / canary sites.
    BOOT, LOUD_HALT, HAMMER, UNHANDLED, REMEMBER_SWIRET, DAH_MRS_SPSR,
    DEBUG_UND_SLOTS, REAL_CLOCK_SECONDS, FTIME_IN_SECONDS,
    FDATE_FROM_SECONDS, PACKAGE_PAGER, INSN_AS_DATA_LDRS, FPE_LDRS, STORE_PROBES,
    NOTIFY_PROBES,
    // Kernel fault-handler entry points.
    DATA_ABORT_HANDLER_VA, FPE_JT_VA,
    // Diagnostics anchors.
    KERNEL_GLOBALS, WEDGE_DIAG, BP_REARM_QUIET_IPA,
    // NewtonOS-2.x-shared defaults (from common_2x).
    KERNEL_TTBR0_BASE, SAFE_INTERVAL_DELTA_SECONDS,
};

/// Compile-time exercise of the full version contract. The explicit
/// re-export list above makes a missing item an unresolved-import
/// error; this anonymous const additionally references every item so
/// the ones whose runtime consumers are feature-gated (the
/// diagnostics anchors under `diag`, for instance) don't fall out of
/// leaner builds as dead code.
const _: () = {
    let _ = (NAME, ROM_IMAGE_SIZE, ROM_CODE_END);
    let _ = (&REX, &ROM_TAIL);
    let _ = (&PATCHES, &NS_TRACE_PATCH);
    let _ = (&BOOT, &LOUD_HALT, &HAMMER, &UNHANDLED);
    let _ = (&REMEMBER_SWIRET, &DAH_MRS_SPSR, &DEBUG_UND_SLOTS);
    let _ = (&REAL_CLOCK_SECONDS, &FTIME_IN_SECONDS, &FDATE_FROM_SECONDS, &PACKAGE_PAGER);
    let _ = (&INSN_AS_DATA_LDRS, &FPE_LDRS, &STORE_PROBES, &NOTIFY_PROBES);
    let _ = (&DATA_ABORT_HANDLER_VA, &FPE_JT_VA);
    let _ = (&KERNEL_GLOBALS, &WEDGE_DIAG, &BP_REARM_QUIET_IPA);
    let _ = (KERNEL_TTBR0_BASE, SAFE_INTERVAL_DELTA_SECONDS);
    let _ = loud_halt_site_name as fn(u32) -> Option<&'static str>;
    // Field-level exercise of `KernelGlobals` — its only runtime
    // readers live behind `nh_diag`, but the anchors are version
    // contract regardless of the build's diagnostics tier.
    if let Some(kg) = KERNEL_GLOBALS {
        let _ = (kg.scheduler_ptr, kg.current_task, kg.want_schedule,
                 kg.hold_schedule, kg.current_globals, kg.object_table,
                 kg.object_table_a_ptr, kg.object_table_b_ptr,
                 kg.object_heap_ptr, kg.interpreter_ptr,
                 kg.stack_mgr_heap_literal, kg.dacr_shadow);
    }
};

/// Name of the loud-halt canary site at `pc`, if `pc` is one. Shared
/// by `diag::trap_diag`'s halt banner and `diag::tracer`'s
/// reserved-range check.
pub fn loud_halt_site_name(pc: u32) -> Option<&'static str> {
    let lh = LOUD_HALT?;
    if pc == lh.reboot.pc {
        Some("Reboot")
    } else if pc == lh.poweroff_reboot.pc {
        Some("PowerOffAndReboot")
    } else if pc == lh.stop_image.pc {
        Some("StopImage")
    } else if pc == lh.bus_error_throw.pc {
        Some("BusErrorThrow")
    } else {
        None
    }
}
