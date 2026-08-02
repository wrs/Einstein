//! GuestOs extension hooks: the single sanctioned hv → newton edge.
//!
//! The generic hypervisor core (trap dispatch, stage-2, timer) is
//! guest-OS-agnostic mechanism; everything Newton-specific it needs —
//! trap-tail pump sequences, the SCTLR/TTBR MMU rituals, ROM-probe
//! dispatch, the UND-trampoline resume path — is reached through the
//! [`GuestOs`] trait below. Call sites use `hooks::ActiveGuest::…`
//! (static dispatch, monomorphized; no dyn — generics can't flow
//! through the `extern "C"` vector entries anyway).
//!
//! The `ActiveGuest` alias is THE one place in `src/hv/` allowed to
//! name `crate::newton`; `scripts/check-layering.sh` sanctions exactly
//! this file for that edge. Every other hv file reaches Newton logic
//! only through `hooks::ActiveGuest`.

use crate::arch::trap_context::TrapContext;

/// Outcome of [`GuestOs::handle_und_hvc`] — the USR-mode HVC probe
/// re-route consulted by `trap::und::handle_und`.
pub enum UndHvcOutcome {
    /// Probe handled; resume the guest at `pc` with source CPSR `spsr`
    /// via the UND-return stub ([`GuestOs::und_resume`]).
    Resume { pc: u64, spsr: u64 },
    /// Probe handled and the handler already staged the return state
    /// (or never returns to this UND site) — nothing more to do.
    Done,
    /// Not a probe instruction this guest OS claims; fall through to
    /// the generic UND emulation arms.
    NotMine,
}

/// Guest-OS-supplied hints for the UND-history caller-LR heuristic in
/// `trap::und::record_und_history`: the semaphore `Swap` helper's PC
/// plus the Acquire/Release return-LR signatures and the stack offsets
/// their compiled prologues put the caller LRs at. Pure diagnostics —
/// `None` just drops the caller columns from the UND history dump.
#[derive(Copy, Clone)]
pub struct UndDiagHints {
    /// The SWP-bearing semaphore `Swap` helper's PC.
    pub swap_helper_pc: u32,
    /// LR_usr value identifying a Swap call from `Acquire`.
    pub acquire_ret_lr: u32,
    /// Stack offset of Acquire's caller LR (the Grabber constructor).
    pub acquire_caller_sp_off: u32,
    /// Stack offset of the Grabber constructor's own caller LR.
    pub acquire_outer_sp_off: u32,
    /// LR_usr value identifying a Swap call from `Release`.
    pub release_ret_lr: u32,
    /// Stack offset of Release's caller LR.
    pub release_caller_sp_off: u32,
}

/// Guest-OS extension points consumed by the hv core. Implemented by
/// the zero-sized [`crate::newton::NewtonOs`]; a future guest (or a
/// null guest for bring-up) swaps in by retargeting [`ActiveGuest`].
pub trait GuestOs {
    /// Sync-trap exit tail: input pumps → `trap::update_virq` →
    /// tick-page advance/publish. Runs after every guest sync trap,
    /// before the diag beacon.
    fn on_sync_trap_exit(ctx: &mut TrapContext);

    /// Guest-path IRQ tail: DMA pumps, input pumps, audio tick,
    /// `trap::update_virq`, splash progress — the behavior-bearing
    /// (order-sensitive) part of `irq_from_guest` between
    /// `timer::on_irq` and the snapshot autosave.
    fn on_irq_tail(ctx: &mut TrapContext);

    /// CNTHP heartbeat body, called from `timer::on_irq` on both the
    /// guest-path and EL2-path IRQ flows: advance the guest's tick
    /// model for non-trapping busy-waits and republish the tick page.
    fn on_heartbeat();

    /// Current virtual interrupt line state `(irq, fiq)` consumed by
    /// `trap::update_virq` when refreshing HCR_EL2.VI / VF.
    fn virq_lines() -> (bool, bool);

    /// Rewrite a guest SCTLR write value before it reaches hardware
    /// (Newton forces A|EE|E0E; guest-test builds force only A).
    fn massage_sctlr(value: u32) -> u32;

    /// Stage-1 MMU rising edge (SCTLR.M 0→1): DC drop, stage-1 table
    /// normalisation, flash-checksum reseed, UND-vector literal swap.
    /// `ttbr0` is the live TTBR0_EL1 value at the transition.
    fn on_stage1_mmu_enable(ctx: &mut TrapContext, ttbr0: u32);

    /// Stage-1 MMU falling edge (SCTLR.M 1→0, soft reboot): re-enable
    /// DC, revert the UND-vector literal to the pre-MMU slot.
    fn on_stage1_mmu_disable(ctx: &mut TrapContext);

    /// Guest TTBR0 write (post hardware update): first-write stage-1
    /// table normalisation + checksum reseed. `raw` is the value the
    /// guest wrote (before the hv's walker-cacheability OR-in).
    fn on_stage1_ttbr0_write(raw: u32);

    /// USR-mode HVC probe re-route: a guest probe patched as `HVC #imm`
    /// executed from USR mode raises UND (HVC is UNDEFINED at EL0) and
    /// lands in `handle_und` instead of `handle_hvc`. Claims the probe
    /// instructions and returns how to resume.
    fn handle_und_hvc(
        ctx: &mut TrapContext,
        insn: u32,
        faulting_pc: u32,
        spsr_und: u64,
    ) -> UndHvcOutcome;

    /// Resume the guest from the UND trampoline at `pc` with source
    /// CPSR `spsr` — the guest-side UND-return stub mechanism (the
    /// stub is installed by the guest-OS trampoline patcher, so the
    /// staging logic lives guest-side).
    fn und_resume(ctx: &mut TrapContext, pc: u64, spsr: u64);

    /// SVC-path ROM-probe HVC tags (probe bodies, Hammer thunks, GPIO
    /// test trigger). Returns true when `imm` was claimed; false falls
    /// through to the unknown-HVC loud halt.
    fn handle_hvc_probe(ctx: &mut TrapContext, imm: u32) -> bool;

    /// FP/SIMD trap (EC=0x07) body: decode the `MCR p10/p11` native-
    /// call convention and dispatch to the native-primitive models.
    /// `insn` is the faulting instruction word, `elr` its guest PC.
    fn handle_native_call(ctx: &mut TrapContext, insn: u32, elr: u32);

    /// `HVC #DabtDispatch` — the DABT-trampoline fall-through: route
    /// alignment faults to the unaligned emulator, forward routine
    /// DFSCs to the kernel's DataAbortHandler, halt on the rest.
    fn handle_dabt_dispatch(ctx: &mut TrapContext);

    /// `HVC #Align` — alignment-fault emulation entry.
    fn handle_align_fault(ctx: &mut TrapContext);

    /// Guest store into the flash-bank IPA window: absorb it (AMD
    /// command-sequence stores never reach the backing). Returns true
    /// when the write was recognised and dropped — caller advances
    /// ELR; false falls through to the generic DABT paths.
    fn maybe_drop_flash_write(ctx: &mut TrapContext, iss: u32, ipa: u64, elr: u32) -> bool;

    /// Guest VA the guest OS wants FPA-class UND instructions routed
    /// to (the kernel's floating-point emulator entry). `None` — the
    /// address isn't known for this ROM version — makes `handle_und`
    /// halt loudly on the first FPA-class UND instead of ERETing into
    /// the wrong place.
    fn fpe_redirect_va() -> Option<u32>;

    /// Diagnostic hints for the UND-history caller-LR heuristic.
    fn und_diag_hints() -> Option<UndDiagHints>;

    /// Base of the guest-side UND trampoline, for the unrecognised-UND
    /// halt path's trampoline-area dump.
    fn und_tramp_base() -> u32;
}

/// The guest OS this build runs. The single sanctioned hv → newton
/// edge; see the module docs.
pub type ActiveGuest = crate::newton::NewtonOs;
