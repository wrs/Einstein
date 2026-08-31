//! Diagnostics layer: halt-path dumps, trap history, symbolication,
//! guest breakpoints, REP output rendering, tracer, tarmac markers.
//! Reachable from any layer through one stable surface, compiled two
//! ways: the real modules when the `diag` feature is on (cfg
//! `nh_diag` — the default everywhere, including the `pi-bare-metal*`
//! aggregates), or `#[inline(always)]` no-op stubs with identical
//! paths and signatures when it is off. Call sites stay
//! unconditional; only the recording/rendering depth changes.
//!
//! Loud halts are correctness surface, not diagnostics: the stubbed
//! `trap_diag` handlers still dump basic trap context and park via
//! `cpu::halt()`. What disappears without `diag` is the kernel-aware
//! rendering (TStack invariants, APCS walks, symbolication), the trap
//! histograms, task dumps, REP output decoding, guest breakpoints,
//! and the ~743 KiB symbol-table rodata (build.rs skips staging it).

// `diag_util` is always compiled: it carries `halt_unknown_subfn`
// (the peripherals' unknown-subfn loud-halt trip-wire — correctness
// surface) and the cheap log-dedup containers (SeenSet / TopK /
// LogBudget / TwoTierLog) that always-on log paths embed as statics.
pub mod diag_util;

#[cfg(nh_diag)]
pub mod blit_timing;
#[cfg(nh_diag)]
pub mod guest_bp;
#[cfg(nh_diag)]
pub mod heap_check;
#[cfg(nh_diag)]
pub mod rep_print;
// No stub for `symbols`: its consumers (task_dump, tracer) all live
// inside diag, so the module simply vanishes with the feature — and
// with it the symbol-table rodata.
#[cfg(nh_diag)]
pub mod stall;
#[cfg(nh_diag)]
pub mod symbols;
#[cfg(all(nh_diag, feature = "platform-fvp-base"))]
pub mod tarmac;
#[cfg(nh_diag)]
pub mod task_dump;
// `trace = ["diag"]` in Cargo.toml, so the feature check implies
// `nh_diag`; the tracer has no stub because every call site is
// already `#[cfg(feature = "trace")]`.
#[cfg(feature = "trace")]
pub mod tracer;
#[cfg(nh_diag)]
pub mod trap_diag;
#[cfg(nh_diag)]
pub mod trap_hist;

/// Raw VIC state pair `(int_ctrl, int_present)` for host-side log
/// decoration. Lives in diag — which may import peripherals — so host
/// backends don't reach into the guest VIC model for a log line.
/// Always compiled (independent of `nh_diag`) so the decorated log
/// stays truthful in stub builds. Sole consumer: the semihost host-io
/// backend's pen-event log, hence the backend cfg.
#[cfg(nh_host_io_semihost)]
pub fn vic_raw_summary() -> (u32, u32) {
    (
        crate::peripherals::vic::int_ctrl_raw(),
        crate::peripherals::vic::int_present_raw(),
    )
}

// ---------------------------------------------------------------------
// No-op stubs (`diag` feature off). One stub module per diag module a
// non-diag layer calls into. Signatures mirror the real ones exactly;
// stubs take only cheap, already-computed arguments so no call site
// pays for a value that only diagnostics would consume.
// ---------------------------------------------------------------------


#[cfg(not(nh_diag))]
pub mod blit_timing {
    //! Stub: no blit timing accumulators without `diag`.

    pub struct BlitTimer;

    impl BlitTimer {
        #[inline(always)]
        pub fn record_since(&self, _t0_us: u64) {}
    }

    pub static EMULATE: BlitTimer = BlitTimer;
    pub static PAINT: BlitTimer = BlitTimer;

    #[inline(always)]
    pub fn begin() -> u64 {
        0
    }
}

#[cfg(not(nh_diag))]
pub mod guest_bp {
    //! Stub: no guest breakpoints without `diag`.

    use crate::arch::trap_context::TrapContext;

    /// Marker UDF the real implementation patches into guest code.
    /// Matched (and never hit — nothing installs BPs) by the UND
    /// dispatcher.
    pub const BP_UDF_INSN: u32 = 0xE7FF_F0FE;

    /// Nothing can be installed, so snapshot autosave is never gated.
    #[inline(always)]
    pub fn any_installed() -> bool {
        false
    }

    /// Never recognises the trap; the caller falls through to the
    /// unrecognised-UND halt.
    #[inline(always)]
    pub fn handle_user_bp_und(
        _ctx: &mut TrapContext,
        _faulting_pc: u32,
        _spsr_und: u64,
        _insn: u32,
    ) -> bool {
        false
    }
}

#[cfg(not(nh_diag))]
pub mod heap_check {
    //! Stub: no object-heap probing without `diag`.

    #[inline(always)]
    pub fn log_heap_bounds_once() {}

    /// No `newton-objects` without `diag`: the notify probes' args
    /// render as the raw Ref word instead of a pretty-printed frame.
    #[inline(always)]
    pub fn pretty_print_ref_inline(ref_value: u32, _depth: u32) {
        crate::kprint!("#{:x}", ref_value);
    }
}

#[cfg(not(nh_diag))]
pub mod rep_print {
    //! Stub: ROM debug output ("REP>") is dropped without `diag`. The
    //! always-on POutTranslator body patches still fire their HVCs;
    //! the handlers discard the bytes and the guest continues.

    pub struct VaArgs;

    impl VaArgs {
        #[inline(always)]
        pub fn new(_r2: u32, _r3: u32, _sp: u32) -> Self {
            VaArgs
        }
    }

    #[inline(always)]
    pub fn render_and_log(_prefix: &str, _fmt_ptr: u32, _args: VaArgs) {}

    /// Renders nothing; callers see a zero-length result. Sole
    /// external consumer is the `GuestTestRepRender` HVC, hence the
    /// cfg (mirrors the call site).
    #[cfg(nh_guest_test)]
    #[inline(always)]
    pub fn render_into(_buf: &mut [u8], _fmt_ptr: u32, _args: VaArgs) -> usize {
        0
    }

    #[inline(always)]
    pub fn putc(_prefix: &str, _c: u8) {}

    #[inline(always)]
    pub fn flush_line(_prefix: &str) {}
}

#[cfg(all(not(nh_diag), feature = "platform-fvp-base"))]
pub mod tarmac {
    //! Stub: no TarmacTrace window markers without `diag`. Only the
    //! window-close hook is exposed — the window-open path lives
    //! inside `trap_diag::sync_trap_beacon`, which is itself stubbed.

    #[inline(always)]
    pub fn emit_stop() {}
}

#[cfg(not(nh_diag))]
pub mod stall {
    //! Stub: no IRQs-masked stretch watermark without `diag`.

    pub const KIND_SYNC: u8 = 1;
    pub const KIND_IRQ: u8 = 2;

    pub struct StretchGuard(());

    /// No-op guard; `Drop` is trivial so the whole mechanism
    /// compiles out.
    #[must_use]
    #[inline(always)]
    pub fn trap_stretch(_kind: u8, _ec: u32, _pc: u32) -> StretchGuard {
        StretchGuard(())
    }

    #[inline(always)]
    pub fn window_open() {}

    #[inline(always)]
    pub fn window_close() {}

    #[inline(always)]
    pub fn take_max_us() -> Option<(u64, u8, u32, u32)> {
        None
    }

    #[inline(always)]
    pub fn kind_label(_kind: u8) -> &'static str {
        "?"
    }
}

#[cfg(not(nh_diag))]
pub mod task_dump {
    //! Stub: no scheduler / kernel-object dumps without `diag`.

    use crate::arch::trap_context::TrapContext;

    /// Stub: no task census without `diag`.
    #[inline(always)]
    pub fn dump() {}
    /// Never fires a dump.
    #[inline(always)]
    pub fn periodic(_ctx: &TrapContext) -> bool {
        false
    }

    #[inline(always)]
    pub fn dump_full() {}

    #[inline(always)]
    pub fn dump_object_by_id(_id: u32) {}
}

#[cfg(not(nh_diag))]
pub mod trap_diag {
    //! Minimal always-on halt rendering. The halt itself (context
    //! dump + park) is correctness surface and must work without
    //! `diag`; what's missing relative to the real module is the
    //! kernel-aware rendering — TStack invariant walks, stage-1
    //! walks, APCS backtraces, symbolication.

    use crate::arch::cpu;
    use crate::arch::trap_context::{read_sysreg, TrapContext};
    use crate::kprintln;

    /// Basic trap-context dump shared by the stubbed halt entry
    /// points: the EL2 syndrome registers plus the guest r0..r14 view
    /// (banked SP/LR per ARM ARM Table D1-79 live in x13..x23).
    fn basic_halt_dump(what: &str, ctx: &TrapContext) {
        kprintln!();
        kprintln!(
            "*** {} (diagnostics stubbed out; rebuild with feature `diag` for the full dump) ***",
            what
        );
        kprintln!(
            "  ELR_EL2={:#010x} SPSR_EL2={:#010x} ESR_EL2={:#010x} FAR_EL1={:#010x}",
            read_sysreg!("elr_el2"),
            read_sysreg!("spsr_el2"),
            read_sysreg!("esr_el2"),
            read_sysreg!("far_el1"),
        );
        for base in [0usize, 5, 10] {
            kprintln!(
                "  r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x}",
                base, ctx.x[base] as u32,
                base + 1, ctx.x[base + 1] as u32,
                base + 2, ctx.x[base + 2] as u32,
                base + 3, ctx.x[base + 3] as u32,
                base + 4, ctx.x[base + 4] as u32,
            );
        }
    }

    pub(crate) fn handle_loud_halt(ctx: &TrapContext) -> ! {
        basic_halt_dump("LoudHalt canary fired", ctx);
        cpu::halt();
    }

    pub(crate) fn handle_unhandled_exception(ctx: &TrapContext, non_user: bool) -> ! {
        basic_halt_dump(
            if non_user {
                "kernel reached UnhandledNonUserModeException"
            } else {
                "kernel reached UnhandledException"
            },
            ctx,
        );
        cpu::halt();
    }

    pub(crate) fn handle_diag(ctx: &mut TrapContext) {
        basic_halt_dump("DIAG vector intercept", ctx);
        cpu::halt();
    }

    #[inline(always)]
    pub fn sync_trap_beacon() {}

    /// Stub: no IRQ-heartbeat PC sampling without `diag`.
    #[inline(always)]
    pub fn irq_heartbeat(_ctx: &TrapContext, _intid: u32) {}
}

#[cfg(not(nh_diag))]
pub mod trap_hist {
    //! Stub: no trap-frequency recording without `diag`.

    #[inline(always)]
    pub fn record_sync(_ec: u32) {}

    #[inline(always)]
    pub fn record_hvc(_imm: u32) {}

    #[inline(always)]
    pub fn record_dabt(_elr_pc: u32, _ipa: u32) {}

    #[inline(always)]
    pub fn cp15_key(_opc1: u32, _crn: u32, _crm: u32, _opc2: u32, _is_read: bool) -> u32 {
        0
    }

    #[inline(always)]
    pub fn record_cp15(_key: u32, _elr_pc: u32) {}

    #[inline(always)]
    pub fn record_fp_simd(_elr_pc: u32) {}

    /// Progress source for the boot-splash bar; without `diag` the
    /// bar stays at zero until the guest's first blit replaces it.
    /// Only the pi_fb host-io backend consumes it, hence the cfg
    /// (mirrors the call site).
    #[cfg(nh_host_io_pi_fb)]
    #[inline(always)]
    pub fn sync_count() -> u64 {
        0
    }

    #[inline(always)]
    pub fn histogram_tick() {}
}
