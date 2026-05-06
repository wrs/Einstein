//! EL2 synchronous trap dispatcher.
//!
//! The vector at offset 0x600 (lower-EL AArch32 sync) saves the full x0..x30
//! context, hands us a `*mut TrapContext`, and we dispatch based on ESR_EL2.EC.
//!
//! Handlers that emulate a guest instruction and want to resume mutate the
//! context in place, advance ELR_EL2 past the faulting instruction, then
//! return — the vector trailer restores the context and ERETs. Handlers that
//! don't want to resume never return (they call `cpu::halt`).

use crate::{cpu, guest_mem, kprintln, mmio, peripherals, peripherals::{native_primitives, vic}, platform, shadow_stub, timer};

macro_rules! read_sysreg {
    ($reg:literal) => {{
        let v: u64;
        // SAFETY: reading a sysreg has no side effects.
        unsafe {
            core::arch::asm!(
                concat!("mrs {}, ", $reg),
                out(reg) v,
                options(nomem, nostack, preserves_flags),
            );
        }
        v
    }};
}

/// Mirror of the AArch64 GPR layout saved by `vectors.s::save_context`.
/// Index `i` is register `xi` (with `x31` intentionally absent — that slot
/// would be SP).
#[repr(C)]
pub struct TrapContext {
    pub x: [u64; 31],
}

const EC_UNKNOWN: u32 = 0x00;
// EC=0x03: trapped MCR/MRC access to CP15 with opc1==0 (and some other
// combinations). This is what we see when HCR_EL2.TVM/TRVM/TIDCP steer a
// guest CP15 access to EL2 instead of letting it go through on real CP15.
const EC_TRAPPED_CP15: u32 = 0x03;
const EC_FP_SIMD: u32 = 0x07;
const EC_HVC_A32: u32 = 0x12;
const EC_INSN_ABORT_LOWER: u32 = 0x20;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

/// HVC immediate used by the guest's UND-vector trampoline at VA 0x04.
/// ARMv7 AArch32 has no HCR_EL2 bit that traps UND directly to EL2, so
/// we install a one-word `HVC #UND_TAG` at the UND vector and decode
/// the faulting instruction ourselves in `handle_und`.
pub const UND_TAG: u32 = 0x10;

/// Alignment-fault fast path. DABT trampoline at `DABT_TRAMP_OFFSET`
/// checks `DFSR.FS[3:0] == 1` (alignment fault, ARMv7 short descriptor)
/// right after saving R0/R1 to TPIDR scratch, and issues
/// `HVC #ALIGN_TAG` directly instead of the DIAG state-capture path.
/// `handle_align_fault` decodes the faulting LDR/STR and emulates it
/// with SA-1100 rotate-LDR semantics, then ERETs past the faulting
/// insn. See `unaligned.rs`.
pub const ALIGN_TAG: u32 = 0x13;

/// Shadow-byte-access retry HVC. Issued by the pre-fault stub at
/// `SBA_PREFAULT_STUB_VA` after its LDRB probe completes (either
/// natively, or after the kernel's DABT path paged in the missing
/// page and the kernel's `subs pc, lr, #8` retried the probe).
/// Handler restores the stashed emulator context and resumes in
/// `shadow_stub::handle_sba_retry`.
pub const SBA_RETRY_TAG: u32 = 0x14;

/// Test-only GPIO line-rise trigger. A guest test issues `HVC #GPIO_TRIGGER_TAG`
/// to inject a virtual GPIO event so the test can verify
/// `peripherals::vic::INT_GPIO` (bit `0x01_000000`) lights up `int_present`
/// and gets delivered as a virtual IRQ. Real GPIO sources don't use this
/// path — they call `vic::raise(INT_GPIO)` directly. This is purely a
/// test hook so `tests/test_gpio.S` doesn't need to fabricate a real
/// device event.
pub const GPIO_TRIGGER_TAG: u32 = 0x06;

/// iter-59 diagnostic counters: per-immediate HVC histogram (256
/// slots covering imm[0..256]). Pulled from `trap_irq` every ~2 s of
/// wall time so we can see which paths dominate the residual trap
/// rate.
static HVC_IMM_HIST: [core::sync::atomic::AtomicU64; 256] = {
    const INIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    [INIT; 256]
};
/// Counter for HVCs with imm ≥ 256 (probe HVCs we don't expect to
/// dominate but want visibility on).
static HVC_HIGH_HIST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn hvc_count(imm: u32) {
    use core::sync::atomic::Ordering;
    if imm < 256 {
        HVC_IMM_HIST[imm as usize].fetch_add(1, Ordering::Relaxed);
    } else {
        HVC_HIGH_HIST.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn dump_hvc_tag_stats() {
    use core::sync::atomic::Ordering;
    // Sort top 8 by hit count, dump as "imm=NN n=COUNT" tuples.
    let mut totals = [0u64; 256];
    for (i, c) in HVC_IMM_HIST.iter().enumerate() {
        totals[i] = c.load(Ordering::Relaxed);
    }
    let mut indices: [usize; 256] = [0; 256];
    for i in 0..256 { indices[i] = i; }
    // Selection sort top 8 (n=256 is small enough).
    for k in 0..8 {
        let mut best = k;
        for j in (k + 1)..256 {
            if totals[indices[j]] > totals[indices[best]] {
                best = j;
            }
        }
        indices.swap(k, best);
    }
    let high = HVC_HIGH_HIST.load(Ordering::Relaxed);
    kprintln!(
        "hvc-imm-hist: top imm[0]=0x{:02x}n={} imm[1]=0x{:02x}n={} imm[2]=0x{:02x}n={} imm[3]=0x{:02x}n={} imm[4]=0x{:02x}n={} imm[5]=0x{:02x}n={} imm[6]=0x{:02x}n={} imm[7]=0x{:02x}n={} (high={})",
        indices[0], totals[indices[0]],
        indices[1], totals[indices[1]],
        indices[2], totals[indices[2]],
        indices[3], totals[indices[3]],
        indices[4], totals[indices[4]],
        indices[5], totals[indices[5]],
        indices[6], totals[indices[6]],
        indices[7], totals[indices[7]],
        high
    );
    crate::unaligned_inline::log_stats();
}


/// Generic "inspect-then-halt" HVC immediate, used by temporary
/// vector-intercept patches during Phase B debugging. When we need to
/// see the CPU state at the moment of an abort we don't otherwise see
/// from EL2 (stage-1 aborts handled entirely by the guest), we patch
/// the relevant guest-mode vector to `HVC #DIAG_TAG`. The handler
/// dumps registers / banked SPSR / banked SP/LR via the X-register
/// mapping (DDI 0487 D1.21.1 Table D1-79), walks the guest stage-1
/// table for the faulting VA, and halts. Remove the patch once the
/// root cause is identified.
pub const DIAG_TAG: u32 = 0x11;

/// Synchronous exception from a lower EL running AArch32.
#[no_mangle]
pub extern "C" fn trap_sync_lower_aarch32(ctx: &mut TrapContext) {
    let esr = read_sysreg!("esr_el2");
    let ec = ((esr >> 26) & 0x3f) as u32;
    let iss = (esr & 0x01ff_ffff) as u32;

    // Forensic sentinel for the RelocHeap-header corruption stop:
    // sample heap[0x0ca6b010] on every guest sync trap and log the
    // value transition with the current ELR_EL2. Bisects the corruption
    // writer to a function range. See `INVESTIGATION.md` and
    // `src/heap_watch.rs`. The ctx + spsr are passed in so the
    // sanity-halt path can dump banked SPs and probe hypothesis #1.
    crate::heap_watch::sample(
        read_sysreg!("elr_el2"),
        crate::heap_watch::Source::Sync,
        ctx,
        read_sysreg!("spsr_el2"),
    );

    // (Group-1 capture re-arm intentionally NOT here — see
    // `trap_irq` for the rationale. Sync traps include the
    // data-abort our own capture handler triggered, so re-arming
    // here would race with STMIA-style multi-register stores that
    // span a page boundary: each STMIA retry re-faults on the still-
    // RO page and the loop never makes progress.)

    // DIAG: log the first N sync traps' EC + ELR + ESR, no dedup, so
    // we can see the guest PC timeline in the window leading up to a
    // stall. Remove once Phase B stall is past.
    //
    // When the `trace` feature is on, suppress the trap log for
    // HVC #TRACE_TAG: the tracer's own `trace <seq> ...` line
    // carries the same signal, and pairing each with a `trap: EC=...`
    // line doubles the output volume for no added information.
    #[cfg(feature = "trace")]
    let is_trace_hvc = ec == EC_HVC_A32 && (iss & 0xFFFF) == crate::tracer::TRACE_TAG;
    #[cfg(not(feature = "trace"))]
    let is_trace_hvc = false;

    static mut TRAP_LOG_BUDGET: usize = 500;
    // SAFETY: single-threaded; only core 0 services EL2 traps.
    // `trap_trace_armed()` overrides the budget so iter-105 can capture
    // every trap from the moment drvr is scheduled in through the wedge.
    let should_log = !is_trace_hvc
        && (trap_trace_armed() || unsafe {
            let go = TRAP_LOG_BUDGET > 0;
            if go { TRAP_LOG_BUDGET -= 1; }
            go
        });
    if should_log {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        let far = read_sysreg!("far_el1");
        kprintln!(
            "trap: EC={:#x} ({}) ELR={:#x} SPSR={:#010x} FAR={:#x} ESR={:#x}",
            ec, describe_ec(ec), elr, spsr, far, esr
        );
        if trap_trace_armed() {
            kprintln!(
                "  r0..r7  = {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
                ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32,
            );
            kprintln!(
                "  r8..r14 = {:08x} {:08x} {:08x} {:08x} {:08x} sp_usr={:08x} lr_usr={:08x}",
                ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32,
                ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32,
            );
        }
    }

    // iter-105: catch the first sync trap from a guest in Thumb mode.
    // Newton 2.x is pure ARM; T=1 in SPSR_EL2 means an interworking
    // branch (BX, BLX, mov pc/Rm with ARMv7+ semantics, ldr pc, pop pc)
    // landed with a Thumb-bit-set target. Logging the first such trap
    // pins the wild-branch event close to its source.
    {
        let spsr_el2 = read_sysreg!("spsr_el2") as u32;
        if (spsr_el2 & 0x20) != 0 {
            static mut THUMB_LOGGED: bool = false;
            // SAFETY: single-threaded.
            let first = unsafe {
                let was = THUMB_LOGGED;
                THUMB_LOGGED = true;
                !was
            };
            if first {
                let elr = read_sysreg!("elr_el2");
                let far = read_sysreg!("far_el1");
                kprintln!(
                    "thumb-source: first sync trap with SPSR_EL2.T=1 — \
                     ELR={:#x} SPSR={:#010x} FAR={:#x} EC={:#x} ({})",
                    elr, spsr_el2, far, ec, describe_ec(ec),
                );
                kprintln!(
                    "thumb-source:   r0..r7:   {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                    ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
                    ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32,
                );
                kprintln!(
                    "thumb-source:   r8..r15:  {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                    ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32,
                    ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32, ctx.x[15] as u32,
                );
            }
        }
    }

    match ec {
        EC_DATA_ABORT_LOWER => handle_data_abort(ctx, iss),
        EC_INSN_ABORT_LOWER => handle_instruction_abort(ctx, iss),
        EC_HVC_A32 => handle_hvc(ctx, iss),
        EC_TRAPPED_CP15 => handle_cp15_trap(ctx, iss),
        EC_FP_SIMD => handle_fp_simd(ctx, iss),
        EC_UNKNOWN => handle_unknown(iss),
        _ => {
            kprintln!(
                "*** Unhandled sync trap EC={:#x} ({}), ESR={:#x} ELR={:#x}",
                ec,
                describe_ec(ec),
                esr,
                read_sysreg!("elr_el2")
            );
            cpu::halt();
        }
    }

    // Guest MMIO writes to IntCtrl / FIQMask / IntClear change the
    // effective (`int_present & int_ctrl & ~fiq_mask`) pending set and
    // must be reflected into HCR_EL2.VI / VF before ERET, or a cleared
    // interrupt keeps re-firing (or an unmasked one never delivers).
    update_virq();

    // Refresh the non-trapping tick page on every sync-trap exit so the
    // guest's tight delay loops (e.g. TSerialNumberROM::Init at 0x1dd8d0,
    // bit-bang protocol with cmp-against-#20-tick deadlines) see a fresh
    // tick value on the next read instead of spinning until the 16 ms
    // CNTHP heartbeat fires. Without this each delay loop runs ~heartbeat
    // wall time regardless of the requested delay, which on QEMU TCG (with
    // tracer overhead amplifying per-trap wall) makes us run ~4x more
    // delay-loop iterations than Einstein for the same kernel logic — and
    // the resulting trace-count drift is what causes the heap-allocator
    // divergence at TStackInfo::Init #12 (see INVESTIGATION.md).
    crate::stage2::tick_page::update();

    // Budget-limited "progress beacon": print PC every 10k traps so we
    // can see if the guest is making forward progress or looping in one
    // place. Doesn't halt — lets boot continue.
    static mut TRAP_COUNTER: u64 = 0;
    // SAFETY: single-threaded.
    let n = unsafe { TRAP_COUNTER += 1; TRAP_COUNTER };
    if n % 10_000 == 0 {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        kprintln!(
            "beacon: {} traps, ELR={:#x} SPSR={:#x} int_present={:#x}",
            n, elr, spsr, vic::raised()
        );
    }
    crate::tarmac::maybe_emit_start(n);
}

/// Asynchronous IRQ taken at EL2. On this target the only physical IRQ
/// source we wire up is CNTHP (EL2 physical timer). Any fire is a Newton
/// timer-match deadline expiring: we latch the crossed match bit(s) into
/// `vic::int_present`, rearm CNTHP_CVAL_EL2 to the next pending deadline,
/// and update HCR_EL2.VI so the guest takes a virtual IRQ on ERET.
#[no_mangle]
pub extern "C" fn trap_irq(ctx: &mut TrapContext) {
    // Forensic sentinel for the RelocHeap-header corruption stop —
    // sample heap[0x0ca6b010] every IRQ. Pairs with the per-sync-trap
    // sample at trap_sync_lower_aarch32. See `src/heap_watch.rs`.
    crate::heap_watch::sample(
        read_sysreg!("elr_el2"),
        crate::heap_watch::Source::Irq,
        ctx,
        read_sysreg!("spsr_el2"),
    );

    // Group-1 capture re-arm: any G1 page that took a permission-fault
    // since the last IRQ has been auto-flipped to RW (so the store
    // could complete). Re-impose RO+XN now so the *next* G1 write
    // after this IRQ also faults and gets logged. Re-arming on IRQ
    // (not sync trap) avoids racing with multi-register stores —
    // STMIA-style instructions can span a page boundary, and each
    // retry takes a sync trap; re-arming between retries would
    // perpetually re-fault the same store. Timer IRQs fire on a
    // ~16 ms cadence which is long enough for in-flight multi-stores
    // to complete and short enough to capture distinct writer PCs
    // through the boot.
    crate::g1_capture::maybe_rearm();
    crate::alrt_capture::maybe_rearm();

    // Acknowledge the interrupt on the host CPU-interface (GICv3 on
    // FVP, no-op on BCM2836) before doing any work. On GICv3 the
    // returned INTID identifies which source fired; a spurious ACK
    // means nothing is pending and we skip timer::on_irq.
    let intid = platform::irq_ack();
    let spurious = intid == platform::irq_spurious();

    // Diagnostic heartbeat: sample guest PC so we can see where it's
    // executing when no MMIO traps are firing.
    //
    // Two-phase behaviour: the first `HB_FIRST_BUDGET` distinct PCs
    // get logged unconditionally — useful while early boot is still
    // walking new code. After that we switch to a "stuck detector":
    // every `HB_LATE_STRIDE`-th IRQ we log the current PC+SPSR, so a
    // guest that's wedged in an idle / alarm loop shows its actual
    // steady-state PC rather than just the first time we saw it.
    static mut HB_LAST_PC: u64 = u64::MAX;
    static mut HB_FIRST_BUDGET: usize = 16;
    static mut HB_IRQ_COUNT: u64 = 0;
    const HB_LATE_STRIDE: u64 = 64;
    let elr = read_sysreg!("elr_el2");
    // SAFETY: single-threaded. `trap_trace_armed()` overrides the
    // budget/stride so iter-105 captures every IRQ between drvr's
    // task-switch arming and the wedge UND.
    let (should_log, tag) = unsafe {
        HB_IRQ_COUNT += 1;
        if trap_trace_armed() {
            (true, "armed")
        } else if HB_FIRST_BUDGET > 0 && elr != HB_LAST_PC {
            HB_LAST_PC = elr;
            HB_FIRST_BUDGET -= 1;
            (true, "first")
        } else if HB_IRQ_COUNT % HB_LATE_STRIDE == 0 {
            (true, "late")
        } else {
            (false, "")
        }
    };
    if should_log {
        let spsr = read_sysreg!("spsr_el2");
        let far = read_sysreg!("far_el1");
        let hcr = read_sysreg!("hcr_el2");
        let vi = (hcr >> 7) & 1;
        let int_present = vic::int_present_raw();
        let int_ctrl = vic::int_ctrl_raw();
        let irq_pend = vic::irq_pending();
        // SP_svc / LR_svc via the AArch64 GPR file per ARM ARM
        // DDI 0487 D1.21.1 Table D1-79: R13_svc ↔ X19, R14_svc ↔ X18.
        // (SP_EL1 / ELR_EL1 are AArch64-only EL0/EL1 sysregs with no
        // architectural alias to AArch32 banked R13/R14; reading them
        // here was the same misdiagnosis docs/QEMU_BUGS.md warns
        // against.) Tells us where the SVC-mode call stack is parked
        // when the guest is wedged in a kernel idle loop.
        let sp_svc = ctx.x[19] as u32;
        let lr_svc = ctx.x[18] as u32;
        kprintln!(
            "timer_irq[{}]: ELR={:#x} SPSR={:#x} SP_svc={:#x} LR_svc={:#x} FAR_EL1={:#x} intid={} VI={} ipres={:#x} ictrl={:#x} pend={}",
            tag, elr, spsr, sp_svc, lr_svc, far, intid, vi, int_present, int_ctrl, irq_pend
        );
    }

    // Periodic scheduler / run-queue dump. Cheap (64-iteration stride) and
    // gives forward-progress signal that's independent of the function
    // tracer (which only sees calls into traced ROM functions). Pass ctx
    // so the dump can render the current task's chain from live banked
    // regs (its SWIBoot save area is stale for the running task).
    crate::task_dump::periodic(ctx);

    // iter-79: periodically check whether the runtime heap has come
    // up; on the first successful check, fire the force-enable
    // sequence (sets gWantSerialDebugging + gInterpreter trace
    // flag). Cheap idempotent — atomic guard inside the helper
    // ensures it only does real work once.
    crate::heap_check::log_heap_bounds_once();

    // One-shot tripwire: poll PA 0x0402a250 every heartbeat and log the
    // first time it transitions to 0x6e657774 ("newt"). Lets us bound
    // the trace event range during which the corruption was written
    // (see INVESTIGATION.md "Currently at — pckm task at sp_usr=
    // 0x0cc7a248"). Cleared once it fires.
    {
        static FIRED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !FIRED.load(core::sync::atomic::Ordering::Relaxed) {
            if let Some(v) = crate::guest_endian::guest_read_u32_pa(0x0402_a250) {
                if v == 0x6e65_7774 {
                    FIRED.store(true, core::sync::atomic::Ordering::Relaxed);
                    let next_v = crate::guest_endian::guest_read_u32_pa(0x0402_a254).unwrap_or(0);
                    kprintln!(
                        "*** newt-tripwire: PA 0x0402a250=0x{:08x} 0x0402a254=0x{:08x} at heartbeat ELR={:#x}",
                        v, next_v, elr
                    );
                }
            }
        }
    }

    // Wedge probe: if the guest's PC parks at the same value across many
    // consecutive heartbeats AND the int_ctrl mask says sound-DMA IRQs
    // are enabled (TSoundServer::TheMain has run and registered them),
    // periodically inject a synthetic sound-DMA-complete IRQ. This
    // tests the Phase-B hypothesis that the boot wedges after sound
    // init because the kernel has armed a wait on a sound-DMA IRQ that
    // we never fire. If the kernel resumes forward progress after
    // injection, we know the gating factor; we can then move the
    // injection to a more targeted path (e.g., a real sound-driver
    // StartOutput emulation).
    static mut WEDGE_SAME_PC: u64 = 0;
    static mut WEDGE_LAST_PC: u64 = u64::MAX;
    static mut WEDGE_INJECT_COUNT: u64 = 0;
    // SAFETY: single-threaded.
    unsafe {
        if elr == WEDGE_LAST_PC {
            WEDGE_SAME_PC += 1;
        } else {
            WEDGE_LAST_PC = elr;
            WEDGE_SAME_PC = 1;
        }
        let int_ctrl = vic::int_ctrl_raw();
        let sound_armed = (int_ctrl & 0x0000_1400) == 0x0000_1400; // DMA3+DMA5
        if WEDGE_SAME_PC >= 64 && WEDGE_SAME_PC % 32 == 0 && sound_armed {
            WEDGE_INJECT_COUNT += 1;
            let same_pc = WEDGE_SAME_PC;
            let inject_count = WEDGE_INJECT_COUNT;
            if inject_count <= 4 {
                kprintln!(
                    "wedge-probe: PC={:#x} stuck for {} samples; injecting sound DMA IRQ (#{})",
                    elr, same_pc, inject_count
                );
            }
            vic::inject_sound_dma_irq();
        }
    }

    if !spurious {
        timer::on_irq();
    }
    update_virq();
    // Wall-clock-paced snapshot save. Timer IRQ is a cleaner hook
    // than sync traps: it fires regardless of whether the guest is
    // making forward progress, so we keep rolling a fresh snapshot
    // into the ring even when the guest is wedged. See
    // src/snapshot.rs.
    crate::snapshot::maybe_autosave(ctx);
    crate::fb_dump::maybe_dump();

    // iter-59 diagnostic: every ~2 s of wall, print HVC-tag counters
    // so we can see whether the residual trap rate is byte-access
    // (UND_TAG) or alignment-fault (ALIGN_TAG) dominated. Independent
    // of snapshot autosave (which is gated when guest_bp is live).
    {
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT_DUMP_TICKS: AtomicU64 = AtomicU64::new(0);
        let now: u64;
        let freq: u64;
        // SAFETY: sysreg reads, side-effect free.
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                options(nomem, nostack, preserves_flags));
            core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
                options(nomem, nostack, preserves_flags));
        }
        let interval = freq.wrapping_mul(2);  // 2 seconds
        let next = NEXT_DUMP_TICKS.load(Ordering::Relaxed);
        if next == 0 {
            NEXT_DUMP_TICKS.store(now.wrapping_add(interval), Ordering::Relaxed);
        } else if now >= next {
            dump_hvc_tag_stats();
            NEXT_DUMP_TICKS.store(now.wrapping_add(interval), Ordering::Relaxed);
        }
    }

    // EOI last so the GIC is ready to deliver the next interrupt.
    // No-op on BCM2836.
    platform::irq_eoi(intid);
}

/// Set HCR_EL2.VI / VF according to whether the VIC has any enabled IRQ
/// or FIQ pending. Sampled on every trap exit.
fn update_virq() {
    let irq = vic::irq_pending();
    let fiq = vic::fiq_pending();
    let mut hcr: u64;
    // SAFETY: sysreg access at EL2.
    unsafe {
        core::arch::asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
    }
    let mut new = hcr & !((1u64 << 6) | (1u64 << 7)); // clear VF and VI
    if irq { new |= 1u64 << 7; }
    if fiq { new |= 1u64 << 6; }
    if new != hcr {
        // SAFETY: writing HCR_EL2.VI/VF toggles virtual IRQ/FIQ pending.
        unsafe {
            core::arch::asm!(
                "msr hcr_el2, {}",
                "isb",
                in(reg) new,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Generic fatal handler for vectors we don't expect to take.
#[no_mangle]
pub extern "C" fn trap_unexpected(_ctx: &mut TrapContext) -> ! {
    let esr = read_sysreg!("esr_el2");
    let elr = read_sysreg!("elr_el2");
    let spsr = read_sysreg!("spsr_el2");
    kprintln!();
    kprintln!("*** UNEXPECTED TRAP AT EL2 ***");
    kprintln!("ESR_EL2  = {:#018x}", esr);
    kprintln!(
        "  EC     = {:#x}  ({})",
        (esr >> 26) & 0x3f,
        describe_ec(((esr >> 26) & 0x3f) as u32)
    );
    kprintln!("ELR_EL2  = {:#018x}", elr);
    kprintln!("SPSR_EL2 = {:#018x}", spsr);
    cpu::halt();
}

// ----------------- individual handlers -----------------

fn handle_data_abort(ctx: &mut TrapContext, iss: u32) {
    let far = read_sysreg!("far_el2");
    let hpfar = read_sysreg!("hpfar_el2");
    let ipa = ((hpfar >> 4) << 12) | (far & 0xFFF);

    let isv = (iss >> 24) & 1;
    let wnr = ((iss >> 6) & 1) != 0;
    let sas = ((iss >> 22) & 3) as u8;
    let srt = ((iss >> 16) & 0x1F) as usize;
    let ifsc = (iss & 0x3f) as u32;

    let elr = read_sysreg!("elr_el2") as u32;

    // Heap-watch debug: log every DABT on the carved-out heap page,
    // regardless of fault class, so we can see WHY the post-rebind
    // RO carve-out doesn't seem to trap. Remove with the rest of the
    // heap-watch scaffolding. Always emit; the trap log budget is
    // separate and we want every hit on this narrow window.
    //
    // Pre-rebind hits saturate any global cap quickly, so reset the
    // cap whenever the armed PA changes — gives each rebind window
    // its own log budget.
    {
        let armed = crate::heap_watch::carved_pa();
        if armed != 0 && (ipa as u32) & !0xFFF == armed {
            static HITS: core::sync::atomic::AtomicU32 =
                core::sync::atomic::AtomicU32::new(0);
            static LAST_ARMED: core::sync::atomic::AtomicU32 =
                core::sync::atomic::AtomicU32::new(0);
            if LAST_ARMED.load(core::sync::atomic::Ordering::Relaxed) != armed {
                HITS.store(0, core::sync::atomic::Ordering::Relaxed);
                LAST_ARMED.store(armed, core::sync::atomic::Ordering::Relaxed);
            }
            let n = HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Always log writes to the corrupted-header window
            // (offsets +0x10..+0x28 from heap base, = page offsets
            // +0x10..+0x28). Filter the rest below the cap.
            let off = (ipa as u32) & 0xFFF;
            let in_corrupt_window = (0x10..0x28).contains(&off);
            let value = if isv != 0 { ctx.x[srt] as u32 } else { 0 };
            let is_wedge = (isv != 0) && matches!(
                value,
                0x002d_d804 | 0x001a_48f0 | 0x002d_fa20 | 0x002d_d7c4
            );
            if n < 256 || in_corrupt_window || is_wedge {
                kprintln!(
                    "heap-watch dabt-on-carve[{}]: ipa={:#010x} elr={:#010x} ifsc={:#x} wnr={} isv={} value={:#010x} srt={} (carved_pa={:#010x}){}",
                    n, ipa, elr, ifsc, wnr as u32, isv, value, srt, armed,
                    if is_wedge { "  *** WEDGE-VALUE ***" }
                    else if in_corrupt_window { "  [header window]" }
                    else { "" },
                );
            }
        }
    }

    // Stage-2 RO-permission fault on a RAM code page. Newton's
    // demand-pager is overwriting a page the hypervisor previously
    // froze RO+X after shadow-stub patching; flip the page back to
    // RW+XN and retry the write natively. The next fetch into the
    // page will trap again (XN) so the handler re-scans the fresh
    // bytes. See `src/stage2.rs::set_ram_page_{ro_x,rw_xn}`.
    let ram_base = guest_mem::RAM_IPA_BASE as u64;
    let ram_end = ram_base + guest_mem::RAM_SIZE as u64;
    let is_permission = (ifsc & 0b111100) == 0b001100;
    if wnr && is_permission && (ram_base..ram_end).contains(&ipa) {
        let page = (ipa as u32) & !0xFFF;

        // Group-1 kernel-globals self-map capture: log the writer
        // before the auto-flip lets the store complete. The
        // re-arm-on-next-trap hook (`g1_capture::maybe_rearm`) will
        // restore RO+XN so subsequent writes also fault.
        if crate::g1_capture::is_armed_pa(page) {
            let value = if isv != 0 { Some(ctx.x[srt] as u32) } else { None };
            crate::g1_capture::note_perm_fault(elr, ipa as u32, value, srt as u32);
        }

        // alrt-task CList header capture: same shape as g1, dynamically
        // armed when Prim Remember installs VA=0x0cca3000 (see
        // `src/alrt_capture.rs`). Try instruction-level emulation
        // first so the page can stay RO across consecutive writes;
        // falls through to the auto-flip path on unrecognized
        // instruction forms.
        if crate::alrt_capture::is_armed_pa(page) {
            let value = if isv != 0 { Some(ctx.x[srt] as u32) } else { None };
            let spsr_el2 = read_sysreg!("spsr_el2") as u32;
            crate::alrt_capture::note_perm_fault(
                elr, ipa as u32, value, srt as u32, spsr_el2,
            );
            let result = crate::pa_emulate::try_emulate_store(
                ctx, elr, spsr_el2, ipa as u32,
            );
            crate::pa_emulate::note(result);
            if matches!(
                result,
                crate::pa_emulate::EmulationResult::Emulated
                    | crate::pa_emulate::EmulationResult::Skipped,
            ) {
                // ELR already advanced and the store applied. Leave
                // the page RO so the next write also faults; we keep
                // capturing every store until the corruption is
                // pinned to a specific (PC, value) pair.
                return;
            }
            // Fell through with Unrecognized — fall back to the
            // auto-flip behavior below so boot can continue.
        }

        // Heap-watch carve-out: log writer info before the auto-flip,
        // and arm a re-RO at the next trap so the next write also
        // faults. See `src/heap_watch.rs`.
        if crate::heap_watch::is_carved_out_ipa(page) {
            let value = if isv != 0 {
                Some(ctx.x[srt] as u32)
            } else {
                None
            };
            crate::heap_watch::note_perm_fault_on_carve_out(
                elr, ipa as u32, value, isv != 0, srt as u32,
            );
        }

        // SAFETY: helper performs its own TLB maintenance.
        unsafe { crate::stage2::set_ram_page_rw_xn(page); }
        // Don't advance ELR — the CPU retries the write.
        return;
    }

    // Direct CPU writes to flash bank addresses are silently dropped
    // (matching Einstein's `TMemory::WriteP` at `Emulator/TMemory.cpp:1777`,
    // which logs and returns without touching the backing). The kernel's
    // flash chip code emits AMD-style command-sequence stores
    // (e.g. `0xAA` to magic offsets) that on real hardware are absorbed
    // by the chip's command latches and never reach the storage cells;
    // on emulation those stores have to be neutralised so the seeded
    // calibration header (`flash::seed_block`) survives. Mutations the
    // kernel actually wants to commit go through `TEinsteinFlashDriver`'s
    // native primitives → `peripherals::flash_driver` → `flash::program_word`
    // / `flash::erase_block`, which write the host backing directly and
    // bypass stage-2 entirely.
    if wnr && peripherals::flash::is_flash_pa(ipa) && drop_flash_write(ctx, iss, elr) {
        advance_elr(4);
        return;
    }

    // Phase B diagnostic: log any access from inside the REx-scanner
    // function range with full register context, to understand what
    // addresses it's probing (for pre-MMU first boot).
    if (0x003137dc..0x00313960).contains(&elr) {
        kprintln!(
            "rex-dabt: ELR={:#010x} {} IPA={:#x} FAR={:#x}  r0={:#x} r1={:#x} r2={:#x} r3={:#x} r4={:#x}",
            elr,
            if wnr { "W" } else { "R" },
            ipa, far,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32, ctx.x[4] as u32
        );
    }

    // Under the UDF-trap shadow-byte-access path, byte/halfword
    // accesses are emulated in EL2 Rust (see shadow_stub::handle_sba_udf)
    // rather than by an in-guest stub. No guest PC therefore lies
    // inside a "stub pool" any more, and a stage-2 abort from the SBA
    // emulator is a bug in the emulator itself — those fall through
    // to the normal `mmio` dispatch with the actual (un-XORed) IPA.

    if isv == 0 {
        // No decodable syndrome — typically LDR/STR with writeback,
        // LDM/STM, or exclusive access. The Newton kernel uses
        // pre-indexed-with-writeback LDR (`ldr Rd, [Rn, #imm]!`) for
        // PCMCIA controller register access (e.g. `DisableSocketInterrupt`
        // at 0x55208). Try to fetch the instruction and emulate the
        // simple LDR/STR-immediate forms; fall through to halt on
        // anything we can't handle so the failure stays loud.
        if try_emulate_isv0_dabt(ctx, ipa, wnr, elr) {
            advance_elr(4);
            return;
        }
        // Mirror Einstein's `TMemory::WriteP` (Emulator/TMemory.cpp:1755-
        // 1766): writes to anywhere `< kHighROMEnd` (0x01000000) are
        // silently dropped, no fault raised. The Newton kernel's PCMCIA
        // path ends up calling `Swap(0, 1)` (atomic SWP via `Acquire`'s
        // semaphore-acquire helper) when `gPowerSemaphore[idx]` is NULL,
        // so the SWP loads ROM[0] and the kernel spins on a non-zero
        // value — matching that behaviour keeps the boot walking.
        if wnr && try_absorb_rom_write(ctx, ipa, elr) {
            advance_elr(4);
            return;
        }
        let spsr = read_sysreg!("spsr_el2");
        let sctlr_el1 = read_sysreg!("sctlr_el1");
        kprintln!(
            "*** data abort ISV=0 at ELR={:#x} SPSR={:#x} IPA={:#x} FAR={:#x} iss={:#x}",
            elr, spsr, ipa, far, iss
        );
        kprintln!(
            "    SCTLR_EL1 (guest) M-bit = {} (stage-1 {})",
            sctlr_el1 & 1,
            if (sctlr_el1 & 1) != 0 { "ON" } else { "OFF" }
        );
        cpu::halt();
    }

    // Before dispatching an "unknown IPA" write to the MMIO halt path,
    // dump the caller context. Cheap enough (runs once, then halt) and
    // decisive for diagnosing MCR-then-STR patterns where the faulting
    // instruction is in a tight helper far from where the bad address
    // was computed. The check mirrors the regions mmio::write would
    // silently accept — anything outside an MMIO window AND outside
    // the stage-2 RW RAM/flash/FB blocks is obviously unreachable.
    if is_obviously_unreachable_ipa(ipa) {
        let spsr = read_sysreg!("spsr_el2") as u32;
        let mode = spsr & 0x1F;
        let mode_label = aarch32_mode_label(mode);
        // r13/r14 of the source mode via Table D1-79 (ctx.x[13]/[14]
        // are SP_usr/LR_usr regardless of source mode).
        let cur_sp = crate::banked::sp_for_mode(ctx, spsr);
        let cur_lr = crate::banked::lr_for_mode(ctx, spsr);
        let dir = if wnr { "writing" } else { "reading" };
        let val = if wnr { ctx.x[srt] as u32 } else { 0 };
        kprintln!(
            "dabt-trip: PC={:#010x} mode={} {} {:#010x} -> IPA={:#x}",
            elr, mode_label, dir, val, ipa
        );
        kprintln!(
            "           r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
        );
        kprintln!(
            "           r4={:#010x} r5={:#010x} r6={:#010x} r7={:#010x}",
            ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32
        );
        kprintln!(
            "           r8={:#010x} r9={:#010x} r10={:#010x} r11={:#010x}",
            ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32
        );
        kprintln!(
            "           r12={:#010x} sp({})={:#010x} lr({})={:#010x}",
            ctx.x[12] as u32, mode_label, cur_sp, mode_label, cur_lr
        );
        // Dump the instruction word at the faulting PC + 1 word of
        // surrounding context, both via stage-1 (so we honour the
        // kernel's view) and direct PA (in case stage-1 is off).
        // Helps when the PC is past the disassembly's coverage —
        // e.g. the post-SearchFreeList halt at 0xf76368.
        for off in [-4i32, 0, 4, 8] {
            let addr = elr.wrapping_add(off as u32);
            let via_va = crate::guest_endian::guest_read_u32_va(addr).unwrap_or(0xDEADBEEF);
            let via_pa = crate::guest_endian::guest_read_u32_pa(addr).unwrap_or(0xDEADBEEF);
            kprintln!(
                "           insn[pc{:+#3x}] @{:#010x} = via-va:{:#010x}  via-pa:{:#010x}",
                off, addr, via_va, via_pa,
            );
        }
        // If the faulting PC sits inside shadow_stub's SBA inline-stub
        // pool, decode slot 14 of the containing stub — that's a
        // `B orig_pc + 4` and gives us the original ROM PC the stub
        // emulates. Helps localise wild branches into the pool to a
        // specific ROM instruction.
        if (crate::shadow_stub::SBA_STUB_POOL_IPA
            ..crate::shadow_stub::SBA_STUB_POOL_END)
            .contains(&elr)
        {
            let off = elr.wrapping_sub(crate::shadow_stub::SBA_STUB_POOL_IPA);
            let slot_idx = off / crate::shadow_stub::SBA_STUB_BYTES;
            let slot_base = crate::shadow_stub::SBA_STUB_POOL_IPA
                + slot_idx * crate::shadow_stub::SBA_STUB_BYTES;
            let back_branch_pa = slot_base.wrapping_add(14 * 4);
            if let Some(insn) = crate::guest_endian::guest_read_u32_pa(back_branch_pa) {
                // ARM A1 B encoding: cond=1110 1010 imm24
                if (insn & 0x0F00_0000) == 0x0A00_0000 {
                    let imm24 = insn & 0x00FF_FFFF;
                    let signed = if imm24 & 0x0080_0000 != 0 {
                        imm24 | 0xFF00_0000
                    } else {
                        imm24
                    };
                    let target = back_branch_pa
                        .wrapping_add(8)
                        .wrapping_add((signed as i32 as u32) << 2);
                    let orig_pc = target.wrapping_sub(4);
                    kprintln!(
                        "           sba-stub: slot {} (base {:#010x}) emulates ROM PC {:#010x} (back-branch {:#010x} -> {:#010x})",
                        slot_idx, slot_base, orig_pc, back_branch_pa, target,
                    );
                } else {
                    kprintln!(
                        "           sba-stub: slot {} (base {:#010x}) — back-branch slot {:#010x} = {:#010x} (not a B insn)",
                        slot_idx, slot_base, back_branch_pa, insn,
                    );
                }
            }
        }
        // Walk a few words of the source-mode stack via stage-1 — the
        // top entry is normally the caller's saved LR after a leaf
        // function's `stmfd sp!, {lr}` prologue. Also walk the access
        // base register so the table-pointer dereference is visible
        // even when the bad value was already overwritten in `ctx`.
        for off in 0..8u32 {
            if let Some(w) = crate::guest_endian::guest_read_u32_va(cur_sp.wrapping_add(off * 4)) {
                kprintln!(
                    "           stack[sp+{:#04x}] @{:#010x} = {:#010x}",
                    off * 4, cur_sp.wrapping_add(off * 4), w
                );
            }
        }
    }

    if wnr {
        let value = ctx.x[srt] as u32;
        mmio::write(ipa, sas, value as u32, elr as u64);
    } else {
        let value = mmio::read(ipa, sas, elr as u64);
        // Sign-extension (SSE) is ignored for stub reads — everything we
        // return here is either zero or a known non-negative constant.
        ctx.x[srt] = value as u64;
    }

    // Advance past the 32-bit ARM instruction that faulted.
    advance_elr(4);
}

/// Attempt to emulate an ISV=0 stage-2 data abort. Used when the
/// faulting instruction is an LDR/STR (immediate, A1) form whose
/// stage-2 syndrome can't carry the destination register — most
/// commonly the pre-indexed-with-writeback variant the Newton kernel
/// uses for PCMCIA-controller register access. Returns true on
/// successful emulation; the caller advances ELR. Returns false if
/// the instruction isn't a form we recognise — caller halts loudly.
///
/// We only handle the unconditional and a small set of common
/// conditional encodings; LDM/STM, exclusives, and register-offset
/// LDR/STR all return false on purpose so they keep halting.
fn try_emulate_isv0_dabt(ctx: &mut TrapContext, ipa: u64, wnr: bool, elr: u32) -> bool {
    let insn = match crate::guest_endian::guest_read_u32_va(elr) {
        Some(v) => v,
        None => return false,
    };
    // Decode LDR/STR (immediate, A1): cond 010 P U 0 W L Rn Rt imm12.
    // We require word access (B=0); halfword/byte forms have
    // different bit 22 values and we don't support them yet.
    if (insn & 0x0E40_0000) != 0x0400_0000 {
        return false;
    }
    let cond = (insn >> 28) & 0xF;
    if cond != 0xE {
        // Conditional: caller already trapped because the access
        // happened, so the condition was true. Same emulation works
        // regardless of which condition was used; allow any cond.
    }
    let p = (insn >> 24) & 1 != 0;
    let u = (insn >> 23) & 1 != 0;
    let w = (insn >> 21) & 1 != 0;
    let l = (insn >> 20) & 1 != 0;
    let rn = ((insn >> 16) & 0xF) as usize;
    let rt = ((insn >> 12) & 0xF) as usize;
    let imm12 = insn & 0xFFF;
    if l != !wnr {
        // Syndrome WnR disagrees with insn L bit — instruction must
        // not be the one we think; bail.
        return false;
    }
    if rn == 15 || rt == 15 {
        // PC-relative or PC-target — too tricky for the simple path.
        return false;
    }
    let writeback = (!p) || w;
    let signed_off: i32 = if u { imm12 as i32 } else { -(imm12 as i32) };
    let pre_rn = ctx.x[rn] as u32;
    let post_rn = pre_rn.wrapping_add(signed_off as u32);

    if l {
        let value = mmio::read(ipa, 2 /* word */, elr as u64);
        ctx.x[rt] = value as u64;
    } else {
        let value = ctx.x[rt] as u32;
        mmio::write(ipa, 2 /* word */, value, elr as u64);
    }
    if writeback {
        ctx.x[rn] = post_rn as u64;
    }
    true
}

/// Mirror Einstein's `TMemory::WriteP` (Emulator/TMemory.cpp:1755-1766)
/// for stage-2 permission faults that target the ROM aperture
/// (`IPA < kHighROMEnd = 0x01000000`). Einstein logs and drops every
/// such write without raising a fault; we map ROM RO at stage-2 so the
/// same writes surface as ISV=0 stage-2 perm faults (no decodable
/// syndrome — SWP, LDM/STM with a base in ROM, etc.).
///
/// For atomic `SWP/SWPB` we still have to run the load piece — the
/// Newton kernel's lock-acquire glue calls `Swap(addr, val)` with
/// `addr = gPowerSemaphore[idx]`, which is NULL on a fresh PCMCIA path,
/// and spins on the loaded value. The load returns `ROM[ipa]` (here
/// `ROM[0]` = the reset vector), the store is dropped.
///
/// Returns `true` if the instruction shape was recognised and the write
/// has been absorbed; the caller advances ELR. Returns `false` for
/// anything we don't recognise so the loud halt path stays the trip-
/// wire for novel cases (pre/post-indexed STR with writeback, LDM/STM,
/// inline-stub byte/halfword stores, …).
fn try_absorb_rom_write(ctx: &mut TrapContext, ipa: u64, elr: u32) -> bool {
    if ipa >= 0x0100_0000 {
        return false;
    }
    // Stage-1 off (pre-MMU and the guest-test runtime) makes
    // `read_word_va` return None — fall back to a PA-direct read,
    // matching the architectural rule that VA == IPA == PA when the
    // MMU is disabled.
    let insn = match crate::guest_endian::guest_read_u32_va(elr).or_else(|| crate::guest_endian::guest_read_u32_pa(elr)) {
        Some(v) => v,
        None => return false,
    };
    // SWP / SWPB (A1):  cond 00010 B 00 Rn Rd SBZ 1001 Rm
    // Mask leaves cond, B (bit 22), Rn, Rd, Rm free; fixes everything
    // else. Rd holds the loaded data on completion; Rm holds the value
    // to write (which we drop). Rn holds the address.
    if (insn & 0x0FB0_0FF0) == 0x0100_0090 {
        let b = ((insn >> 22) & 1) != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let rd = ((insn >> 12) & 0xF) as usize;
        if rn == 15 || rd == 15 {
            return false;
        }
        let pa = ipa as u32;
        let value = if b {
            guest_mem::read_byte_pa(pa).unwrap_or(0) as u32
        } else {
            // Plain SWP zero-extends bits[31:0] of the loaded word into
            // Rd; `read_word_pa` already returns a u32 in the guest's
            // little-endian view (matches the BE→LE byteswap done at
            // ROM load time).
            crate::guest_endian::guest_read_u32_pa(pa).unwrap_or(0)
        };
        ctx.x[rd] = value as u64;
        return true;
    }
    false
}

/// IPA ranges that the stage-2 map intentionally leaves as fault /
/// read-only and that no peripheral module owns. A write here is
/// almost certainly a wild pointer — worth dumping context before
/// halting.
fn is_obviously_unreachable_ipa(ipa: u64) -> bool {
    // Inside ROM (stage-2 RO). Any write is doomed.
    if ipa < 0x0100_0000 { return true; }
    // "Unknown bank #5" gap (between flash bank 2 end at 0x10400000
    // and PCMCIA0Base at 0x30000000). Einstein's TMemory silently
    // returns 0 here; we now do the same in mmio.rs but the kernel
    // still gets here only via uninitialised-pointer paths (e.g.
    // the TEncodingMap.+16 = 0x20000110 from the MakeString fault
    // we resolved on 2026-04-27). Surfacing the register context
    // for the first such access per boot is cheap and decisive.
    // Skip the NO_REX_PROBE sub-window (0x10400000..0x20000000) —
    // that's a known ROM-driven scan that legitimately reads zeros.
    if (0x2000_0000..0x3000_0000).contains(&ipa) { return true; }
    false
}

/// Drop a guest write to the flash bank IPA window. Stage-2 maps the
/// banks RO to surface AMD-style command-sequence stores (the kernel's
/// flash chip code emits `0xAA` / `0x55` / `0x80` to magic offsets);
/// Einstein's `TMemory::WriteP` ignores them, so we do too.
///
/// For ISV=1 syndromes (simple LDR/STR-immediate without writeback):
/// nothing to update on the guest side, just advance ELR.
///
/// For ISV=0 syndromes (writeback or register-offset addressing): we
/// fetch the instruction at ELR, decode the destination register and
/// any base-register writeback, and update Rn so the kernel observes
/// the same post-instruction CPU state it would have if the store had
/// been silently absorbed by the flash chip's command latch. The store
/// itself is dropped.
///
/// Returns false on instruction shapes we don't recognise (LDM/STM,
/// load-exclusive, vector loads, …) so the caller halts loudly. Drop
/// in fresh forms here as the kernel turns out to use them.
fn drop_flash_write(ctx: &mut TrapContext, iss: u32, elr: u32) -> bool {
    let isv = (iss >> 24) & 1;
    if isv != 0 {
        // Simple LDR/STR-immediate or LDR/STR-byte/halfword without
        // writeback — no register state changes besides the (dropped)
        // memory store. Caller advances ELR.
        return true;
    }

    // ISV=0: writeback or unusual addressing. Decode the faulting
    // instruction enough to apply the writeback to Rn (if any).
    let insn = match crate::guest_endian::guest_read_u32_va(elr) {
        Some(v) => v,
        None => return false,
    };

    // STR (immediate, A1): cond 010 P U B W L Rn Rt imm12, L=0.
    // Word B=0, byte B=1. Writeback when (P=0) || (W=1).
    if (insn & 0x0E10_0000) == 0x0400_0000 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let imm12 = insn & 0xFFF;
        if rn == 15 {
            return false;
        }
        let writeback = (!p) || w;
        if writeback {
            let signed_off: i32 = if u { imm12 as i32 } else { -(imm12 as i32) };
            let pre_rn = ctx.x[rn] as u32;
            ctx.x[rn] = pre_rn.wrapping_add(signed_off as u32) as u64;
        }
        return true;
    }

    // STRH (immediate, A1): cond 000 P U 1 W 0 Rn Rt imm4H 1011 imm4L.
    // imm = (imm4H << 4) | imm4L. Writeback when (P=0) || (W=1).
    if (insn & 0x0E40_00F0) == 0x0040_00B0 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let imm = ((insn >> 4) & 0xF0) | (insn & 0xF);
        if rn == 15 {
            return false;
        }
        let writeback = (!p) || w;
        if writeback {
            let signed_off: i32 = if u { imm as i32 } else { -(imm as i32) };
            let pre_rn = ctx.x[rn] as u32;
            ctx.x[rn] = pre_rn.wrapping_add(signed_off as u32) as u64;
        }
        return true;
    }

    // STR (register, A1): cond 011 P U B W L Rn Rt imm5 type Rm, L=0.
    // Bit 4 must be 0 (else it's a register-shift form we don't decode).
    if (insn & 0x0E10_0010) == 0x0600_0000 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let rm = (insn & 0xF) as usize;
        let imm5 = (insn >> 7) & 0x1F;
        let shift_type = (insn >> 5) & 0x3;
        if rn == 15 || rm == 15 {
            return false;
        }
        let writeback = (!p) || w;
        if writeback {
            let rm_val = ctx.x[rm] as u32;
            let shifted = arm_shift(rm_val, shift_type, imm5);
            let pre_rn = ctx.x[rn] as u32;
            let post_rn = if u {
                pre_rn.wrapping_add(shifted)
            } else {
                pre_rn.wrapping_sub(shifted)
            };
            ctx.x[rn] = post_rn as u64;
        }
        return true;
    }

    false
}

/// ARMv7 immediate-shift evaluation for the `imm5/type` field of LDR/STR
/// register-offset forms. The carry-out is unused here (we only need the
/// shifted value for address arithmetic).
fn arm_shift(value: u32, shift_type: u32, imm5: u32) -> u32 {
    match shift_type {
        // LSL
        0 => value.wrapping_shl(imm5),
        // LSR — imm5==0 means shift by 32, yielding 0
        1 => if imm5 == 0 { 0 } else { value.wrapping_shr(imm5) },
        // ASR — imm5==0 means shift by 32 (sign extend)
        2 => {
            if imm5 == 0 {
                ((value as i32) >> 31) as u32
            } else {
                ((value as i32).wrapping_shr(imm5)) as u32
            }
        }
        // ROR / RRX — imm5==0 is RRX (one-bit rotate through carry); we
        // don't have carry here, so approximate with a logical right-1.
        _ => {
            if imm5 == 0 {
                value >> 1
            } else {
                value.rotate_right(imm5)
            }
        }
    }
}

fn aarch32_mode_label(mode: u32) -> &'static str {
    match mode {
        0x10 => "usr",
        0x11 => "fiq",
        0x12 => "irq",
        0x13 => "svc",
        0x17 => "abt",
        0x1B => "und",
        0x1F => "sys",
        _    => "???",
    }
}

fn handle_instruction_abort(ctx: &TrapContext, iss: u32) {
    let far = read_sysreg!("far_el2");
    let hpfar = read_sysreg!("hpfar_el2");
    let ipa = ((hpfar >> 4) << 12) | (far & 0xFFF);
    let elr = read_sysreg!("elr_el2");

    // Lazy shadow-stub discovery for RAM-resident code.
    //
    // RAM is mapped XN at stage-2 so the first fetch into any 2 MiB
    // RAM block traps here. We scan the block for byte/halfword
    // accesses, install stubs, and flip XN off on that block. ERET
    // retries the fetch, which now succeeds.
    //
    // IFSC values (ISS bits [5:0]) we care about:
    //   0b000101  Translation fault, level 1 (page isn't mapped)
    //   0b001111  Permission fault, level 3 (XN)
    //   0b001110..0b001111 various permission-fault levels
    // We act on any permission fault whose IPA is inside a RAM range.
    let ifsc = (iss & 0x3f) as u32;
    let is_permission = (ifsc & 0b111100) == 0b001100; // 0x0C..0x0F
    let ram_base = guest_mem::RAM_IPA_BASE as u64;
    let ram_end = ram_base + guest_mem::RAM_SIZE as u64;
    let in_ram = (ram_base..ram_end).contains(&ipa);

    if is_permission && in_ram {
        let scan_ipa = ipa as u32;

        // Scan one 4 KiB page — Newton's demand-pager operates at
        // 4 KiB granularity, so re-arming a whole 2 MiB block on
        // every fresh code page would force us to re-patch already-
        // patched pages (and potentially double-emit UDFs). The
        // stage-2 state machine (RW+XN ↔ RO+¬X per page) lets us
        // track each page independently.
        let page_start = scan_ipa & !0xFFFu32;
        let page_end = page_start.wrapping_add(0x1000);

        kprintln!(
            "shadow_stub: lazy RAM patch — page {:#x}..{:#x} (fetch at {:#x})",
            page_start, page_end, ipa
        );
        let stats = shadow_stub::patch_code_range(page_start, page_end);
        shadow_stub::log_stats(&stats);

        // Flip the page to RO + executable. The next write will
        // stage-2-fault into the data-abort handler and re-arm it
        // RW + XN, forcing a re-scan on the subsequent fetch.
        // SAFETY: helper performs its own TLB maintenance.
        unsafe {
            crate::stage2::set_ram_page_ro_x(page_start);
        }

        // Retry the fetch — don't advance ELR, just return.
        return;
    }

    kprintln!();
    kprintln!("*** instruction abort from lower EL (no silent skip per Phase A) ***");
    kprintln!(
        "  ELR={:#x}  FAR_EL2={:#x}  IPA={:#x}  IFSC={:#x}",
        elr, far, ipa, ifsc
    );
    let spsr = read_sysreg!("spsr_el2") as u32;
    let mode = spsr & 0x1F;
    let mode_name = match mode {
        0x10 => "usr", 0x11 => "fiq", 0x12 => "irq", 0x13 => "svc",
        0x16 => "mon", 0x17 => "abt", 0x1A => "hyp", 0x1B => "und",
        0x1F => "sys", _ => "???",
    };
    // R14 of the active mode via Table D1-79 (ctx.x[14] is LR_usr
    // regardless of source mode; LR_und lives in ctx.x[22], etc.).
    let mode_lr = crate::banked::lr_for_mode(ctx, spsr);
    kprintln!(
        "  SPSR_EL2={:#x}  mode={}  R14({})={:#x}  R0={:#x}  R1={:#x}",
        spsr, mode_name, mode_name, mode_lr, ctx.x[0] as u32, ctx.x[1] as u32
    );
    if mode == 0x1B {
        kprintln!(
            "  (in UND mode: R14 = faulting_pc + 4 = {:#x}; dig there for the real UND)",
            mode_lr.wrapping_sub(4)
        );
    }
    kprintln!(
        "  (guest tried to fetch an instruction at an IPA our stage-2 doesn't map."
    );
    kprintln!(
        "   Either widen the stage-2 map to cover this IPA, or figure out why the"
    );
    kprintln!(
        "   guest's PC went here — the instruction preceding this is a suspect.)"
    );
    cpu::halt();
}

fn handle_hvc(ctx: &mut TrapContext, iss: u32) {
    // Guest-test protocol — see baremetal/guest-tests/README.md.
    let imm = iss & 0xFFFF;
    let r0 = ctx.x[0] as u32;
    // iter-59 diagnostic: per-imm HVC histogram. See dump_hvc_tag_stats.
    hvc_count(imm);
    match imm {
        0x01 => {
            // Print one ASCII byte from r0.
            let b = r0 as u8;
            if b == b'\n' { crate::uart::write_byte(b'\r'); }
            crate::uart::write_byte(b);
        }
        0x02 => {
            kprintln!("guest-hex: {:#010x}", r0);
        }
        0x03 => {
            kprintln!();
            kprintln!("*** guest test PASSED (r0={:#x}) ***", r0);
            cpu::halt();
        }
        0x04 => {
            kprintln!();
            kprintln!("*** guest test FAILED (code={:#x}) ***", r0);
            cpu::halt();
        }
        0x05 => {
            kprintln!("guest-mark: {:#010x}", r0);
        }
        0x40 => {
            // DebugStr ROM-patch trap: the ROM-patched stub at
            // DEBUG_STR_STUB_PC does `MOV r7, LR` before this HVC so we
            // can read LR without relying on AArch64 banked-register
            // accesses (MRS LR_svc is unimplemented on QEMU raspi3b's
            // Cortex-A53 model). r0 is the guest's string pointer; we
            // log it and resume at LR + 4, matching Einstein's callback
            // (Emulator/JIT/Generic/TJITGenericROMPatch.cpp:76).
            let addr = r0;
            log_guest_string("DebugStr", addr);
            let lr = ctx.x[7] as u32;
            // SAFETY: ELR_EL2 controls the post-ERET guest PC.
            unsafe {
                core::arch::asm!(
                    "msr elr_el2, {}",
                    in(reg) lr.wrapping_add(4) as u64,
                    options(nostack, preserves_flags),
                );
            }
            return;
        }
        0x41 => {
            // Debugger ROM-patch trap. Stub stashed LR into r7 for the
            // same reason as DebugStr above. Einstein's callback breaks
            // into the host debugger and returns PC = LR + 8
            // (TJITGenericROMPatch.cpp:96); we have no host debugger, so
            // log the site and continue.
            let elr = read_sysreg!("elr_el2");
            kprintln!("Debugger trap @ELR={:#x}", elr);
            let lr = ctx.x[7] as u32;
            unsafe {
                core::arch::asm!(
                    "msr elr_el2, {}",
                    in(reg) lr.wrapping_add(8) as u64,
                    options(nostack, preserves_flags),
                );
            }
            return;
        }
        0x30 => {
            // Shadow-stub patch request: r0=start_ipa, r1=end_ipa (exclusive).
            // Scans that IPA range of the ROM backing (the guest-test image)
            // and patches every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH. Emits stubs
            // into the shadow-stub pool and rewrites originals to Bcc stub.
            let start = ctx.x[0] as u32;
            let end = ctx.x[1] as u32;
            let stats = crate::shadow_stub::patch_code_range(start, end);
            crate::shadow_stub::log_stats(&stats);
            // Echo the patched count back to r0 so the guest can check it.
            ctx.x[0] = stats.patched as u64;
        }
        0x20 => {
            // Save snapshot — see src/snapshot.rs. ctx.x[0..30] is
            // the AArch64 GPR view that aliases AArch32 R0..R12 plus
            // every banked SP/LR per ARM ARM Table D1-79; ELR_EL2 /
            // SPSR_EL2 give the PC and CPSR to resume at.
            let mut gprs = [0u64; 31];
            for i in 0..31 {
                gprs[i] = ctx.x[i];
            }
            if let Err(e) = crate::snapshot::save(&gprs) {
                kprintln!("snapshot: save failed: {}", e);
            }
        }
        0x21 => {
            // Full kernel-state dump on demand. Issued from a guest
            // ROM patch at well-chosen PCs (e.g. just before a
            // suspected stall, or right after Init__5TTask of a task
            // we want to trace) to capture scheduler + ports +
            // monitors in one shot.
            crate::task_dump::dump_full();
        }
        0x22 => {
            // Dump one kernel object by id. Guest puts the id in r0.
            let id = ctx.x[0] as u32;
            kprintln!("=== HVC dump_object_by_id({:#x}) ===", id);
            crate::task_dump::dump_object_by_id(id);
        }
        v if v == crate::rom_patches::POWEROFF_REBOOT_HVC_IMM => {
            handle_poweroff_reboot(ctx);
        }
        v if v == crate::rom_patches::REBOOT_HVC_IMM => {
            handle_reboot(ctx);
        }
        v if v == crate::rom_patches::BOOTOS_HVC_IMM => {
            handle_bootos_canary(ctx);
        }
        v if v == crate::rom_patches::REMEMBER_SWIRET_HVC_IMM => {
            handle_remember_swiret_probe(ctx);
        }
        v if v == crate::rom_patches::DAH_MRS_SPSR_HVC_IMM => {
            handle_dah_mrs_spsr_patch(ctx);
        }
        v if v == crate::rom_patches::DAH_FME_RET_HVC_IMM => {
            handle_dah_fme_ret_probe(ctx);
        }
        v if v == crate::rom_patches::DAH_FME_ENTRY_HVC_IMM => {
            handle_fme_entry_probe(ctx);
        }
        v if v == crate::rom_patches::DAH_OR_CHAIN_HVC_IMM => {
            handle_dah_or_chain_probe(ctx);
        }
        v if v == crate::rom_patches::UNHANDLED_EXCEPTION_HVC_IMM => {
            handle_unhandled_exception(ctx, false);
        }
        v if v == crate::rom_patches::UNHANDLED_NUM_EXCEPTION_HVC_IMM => {
            handle_unhandled_exception(ctx, true);
        }
        v if v == crate::rom_patches::FPE_ENTRY_PROBE_HVC_IMM => {
            handle_fpe_entry_probe(ctx);
        }
        v if v == crate::rom_patches::TASK_SWITCH_PROBE_HVC_IMM => {
            handle_task_switch_probe(ctx);
        }
        v if v == crate::rom_patches::TASK_SWITCH_PRE_ERET_HVC_IMM => {
            handle_task_switch_pre_eret_probe(ctx);
        }
        v if v == crate::rom_patches::TASK_SWITCH_ERET_HVC_IMM => {
            handle_task_switch_eret_emulator(ctx);
        }
        v if v == UND_TAG => {
            handle_und(ctx);
        }
        v if v == DIAG_TAG => {
            handle_diag(ctx);
        }
        v if v == ALIGN_TAG => {
            crate::unaligned::handle_align_fault(ctx);
        }
        v if v == SBA_RETRY_TAG => {
            crate::shadow_stub::handle_sba_retry(ctx);
        }
        v if v == GPIO_TRIGGER_TAG => {
            // Test-only: raise the GPIO IRQ line. Delivered as a
            // virtual IRQ on ERET via the shared update_virq path.
            vic::raise(vic::INT_GPIO);
        }
        #[cfg(feature = "trace")]
        v if v == crate::tracer::TRACE_TAG => {
            crate::tracer::handle_trace_hvc(ctx);
        }
        _ => {
            let elr = read_sysreg!("elr_el2");
            kprintln!();
            kprintln!("*** unknown HVC #{:#x} at ELR={:#x} (halting)", imm, elr);
            cpu::halt();
        }
    }
    // No ELR advance needed: HVC entry sets ELR_EL2 to the PC of the
    // instruction after the HVC (DDI 0487 G1.11.1 "HVC from AArch32"),
    // so ERET returns to the guest's next instruction as-is.
}

/// Trampoline-based undefined-instruction handler at EL2.
///
/// Flow: the guest's UND vector at VA 0x04 branches to a small AArch32
/// stub (see `UND_CTX_SAVE_*` constants below). The stub runs in UND
/// mode, saves R14_und (faulting_pc + 4) and SPSR_und (pre-UND CPSR)
/// to fixed RAM slots, then issues `HVC #UND_TAG` to enter EL2. We
/// decode the faulting instruction from guest memory, emulate, then
/// override ELR_EL2 / SPSR_EL2 so ERET resumes in the original mode
/// at the correct address.
///
/// Why the RAM-save stub: reading the AArch32 banked registers
/// (LR_und / SPSR_und) from AArch64 EL2 via MRS returns 0 under QEMU
/// raspi3b — the banked state doesn't propagate into the AArch64 view
/// even though it's set correctly on the AArch32 side (verified with
/// a pure-AArch32 probe; see the commit). So the trampoline persists
/// what we need before bouncing to EL2.
///
/// Covered instructions (PLAN.md Phase A.2):
/// - SWP / SWPB (any encoding). Emulated by plain load-store on the
///   translated guest PA; no atomic primitive needed because we hold
///   DAIF.I at EL2 for the entire emulation and the guest is single-
///   core.
/// - `0xE6000010` SystemBootUND: ELR += 4 (single-instruction NOP).
///   Einstein's JIT sets R15 = inVAddr + 8, which in its pipeline
///   convention (GetJITUnitForPC does `pc = inPC - 4`) resumes at
///   inVAddr + 4 — one-instruction advance. The only SystemBootUND
///   site in 717006 is at 0x000188cc; the word at 0x188d0 is a real
///   `LDR R0, [PC, #0xc40]` instruction that feeds the following
///   `LDR PC, [R0]` at 0x188d8 — not a payload.
/// - `0xE6000510` DebuggerUND: ELR advances past the null-terminated
///   ASCII payload (aligned to next word boundary); log the string.
/// - `0xE6000810` TapFileCntlUND: ELR += 8, log the payload word.
///   (Einstein's JIT uses GETCALLER()+4 for TapFileCntl; we match the
///   JIT's page-compilation step for now — Phase B revisit when the
///   ROM actually exercises filesystem UNDs.)
/// - Anything else: log opcode + PC, halt loudly.
///
/// Fixed RAM slots used by the trampoline (must match guest tests and,
/// eventually, the ROM's patch_und_vector):
///   0x04000400  — saved LR_und (faulting_pc + 4)
///   0x04000404  — saved SPSR_und (pre-UND CPSR)
// 2026-04-28: relocated trampoline scratch from PA=0x04005F00 (the
// kernel-globals self-mapped region at L1[0xc0]) to the last 4 KiB of
// the hypervisor scratch pool at IPA=0x0600_F000. The previous PA was
// reachable post-MMU only through the kernel's pre-baked L2[0x4]
// descriptor — a deliberate ARMv4 subpage-AP permission-overlay
// mapping the kernel-globals page at PA=0x04005000 at multiple kernel
// VAs. That created a verify-mmu Group-1 alias under our flat AP=011.
// The new IPA lives in the hypervisor-managed `SCRATCH_POOL` region
// (mapped via the L1[0x60] section we install at MMU-enable time),
// so the same value works pre-MMU (stage-1 off → stage-2 maps IPA →
// host SCRATCH_POOL) and post-MMU (kernel L1[0x60] → IPA → stage-2).
// No swap pre/post-MMU needed.
//
// Older (buggy) slots at 0x0400_0400 — those lived inside the kernel's
// L1 table; writing there fails post-MMU and would corrupt the guest's
// own L1 if it succeeded.
//
// Layout (offsets from `HYP_TRAMP_SCRATCH_BASE`):
//   +0x00 LR_und       +0x10 R1
//   +0x04 SPSR_und     +0x14 R2
//   +0x08 LR_svc       +0x18 banked SP
//   +0x0C R0           +0x1C banked LR
//   +0xA0..+0xB7  DABT trampoline save (lr_abt, sp_abt, spsr_abt,
//                                       sp_svc, spsr_svc, lr_svc)
//
// 2026-05-03: relocated from 0x0600_F000 (slot 7680 inside SCRATCH_POOL)
// to 0x0600_0000 (slot 0). The old location overlapped shadow_stub's
// per-stub literal area once iter-85 grew SCRATCH_POOL to 384 KiB —
// shadow_stub's NEXT_SCRATCH_SLOT now starts past RESERVED_SCRATCH_SLOTS
// (32 slots = 256 B), keeping the trampoline's footprint (offsets
// 0x00..0xAC) reserved and never claimed by a stub.
pub const HYP_TRAMP_SCRATCH_BASE: u32 = crate::shadow_stub::SCRATCH_POOL_IPA;
pub const UND_SAVE_LR_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x00;
pub const UND_SAVE_SPSR_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x04;
/// LR_svc captured by the trampoline's brief SVC-mode bounce. Only
/// meaningful when SPSR_und's mode field says the caller was SVC
/// (which is the case for all Newton 2.x kernel-internal calls).
#[allow(dead_code)]
pub const UND_SAVE_LR_SVC_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x08;

/// Pre-UND R0 and R1. The trampoline persists them here before
/// clobbering R0 (to hold the save-slot VA) and R1 (to read SPSR /
/// LR_svc). `handle_und` restores `ctx.x[0]` and `ctx.x[1]` from
/// these slots at entry so the traced guest sees its arguments
/// intact across the UND round-trip.
pub const UND_SAVE_R0_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x0C;
pub const UND_SAVE_R1_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x10;

/// R2 stash — the trampoline briefly clobbers R2 while executing the
/// mode-switch dance that reads the faulting mode's banked SP/LR.
#[allow(dead_code)]
pub const UND_SAVE_R2_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x14;

/// Banked SP (R13) and LR (R14) of the faulting mode. Populated by the
/// trampoline after switching to the faulting mode (or SYS when the
/// faulting mode is USR) and saving its SP/LR. `handle_und` reads these
/// so the shadow-byte-access emulator can access `[SP, #imm]`-style
/// addressing with the right bank. The SBA post-emulation trampoline
/// (see `guest_mem::patch_und_vector`) also reads these slots on the
/// way out to write the updated values into the faulting mode's banked
/// SP / LR, for sites that writeback Rn ∈ {13, 14}.
pub const UND_SAVE_BANKED_SP_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x18;
pub const UND_SAVE_BANKED_LR_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x1C;

// iter-87 diag: rolling buffer of recent UND faults. The wedge fires
// inside our trampoline (PC=0xffff54) — the trampoline's own HVC,
// caught by handle_und's catch-all. To learn how USR ended up at the
// trampoline's HVC instruction, we need to see the prior UNDs.
#[derive(Copy, Clone)]
struct UndHistEntry {
    faulting_pc: u32,
    insn: u32,
    spsr_und: u32,
    lr_usr: u32,
    sp_for_mode: u32,
}
const UND_HIST_LEN: usize = 32;
static mut UND_HISTORY: [UndHistEntry; UND_HIST_LEN] = [
    UndHistEntry { faulting_pc: 0, insn: 0, spsr_und: 0, lr_usr: 0, sp_for_mode: 0 };
    UND_HIST_LEN
];
static mut UND_HIST_NEXT: usize = 0;
static mut UND_HIST_COUNT: u64 = 0;

fn record_und_history(faulting_pc: u32, insn: u32, spsr_und: u32, ctx: &TrapContext) {
    // Capture banked SP for the faulting mode, so dumps show where
    // the faulting code's stack was. lr_usr is ctx.x[14]; for non-USR
    // sources it's still informative as the user-space caller LR.
    let sp = crate::banked::sp_for_mode(ctx, spsr_und);
    let entry = UndHistEntry {
        faulting_pc,
        insn,
        spsr_und,
        lr_usr: ctx.x[14] as u32,
        sp_for_mode: sp,
    };
    // SAFETY: single-threaded EL2.
    unsafe {
        let i = UND_HIST_NEXT;
        UND_HISTORY[i] = entry;
        UND_HIST_NEXT = (i + 1) % UND_HIST_LEN;
        UND_HIST_COUNT = UND_HIST_COUNT.wrapping_add(1);
    }
}

fn dump_und_history() {
    // SAFETY: single-threaded EL2.
    let (count, next) = unsafe { (UND_HIST_COUNT, UND_HIST_NEXT) };
    let n = if count < UND_HIST_LEN as u64 { count as usize } else { UND_HIST_LEN };
    kprintln!("UND history (last {} of {} total UNDs, oldest first):", n, count);
    for k in 0..n {
        let i = (next + UND_HIST_LEN - n + k) % UND_HIST_LEN;
        // SAFETY: index in range, single-threaded.
        let e = unsafe { UND_HISTORY[i] };
        let mode = e.spsr_und & 0x1F;
        kprintln!(
            "  #{:>3}  PC={:#010x} insn={:#010x} mode={:#x}({}) sp={:#010x} lr_usr={:#010x}",
            (count - n as u64 + k as u64),
            e.faulting_pc, e.insn, mode, describe_aarch32_mode(mode),
            e.sp_for_mode, e.lr_usr,
        );
    }
}

fn handle_und(ctx: &mut TrapContext) {
    // Restore pre-UND R0, R1, R12 from the stash slots the trampoline
    // populated at entry. R0/R1 go through RAM slots (the trampoline
    // unavoidably clobbers R0 to hold the save-slot VA and R1 across
    // the SVC bounce). R12 goes through TPIDR_EL0 (= AArch32
    // TPIDRURW), which the trampoline writes with `MCR p15,0,r12,...`
    // as its very first instruction before clobbering R12 to hold the
    // save-slot base. TPIDRURW is ARMv6+ state the SA-1100-era Newton
    // ROM never touches, so using it as the R12 save slot is safe.
    //
    // Restoring R12 matters for the shadow-byte-access UDF-trap path,
    // where the faulting instruction can legitimately use R12 as base
    // / data / offset. The tracer's function-entry UDF sites don't
    // need R12 (every Newton 2.x prologue begins `MOV R12, R13`), but
    // doing the restore unconditionally is cheaper than branching on
    // the UDF kind.
    ctx.x[0] = read_guest_word_pa(UND_SAVE_R0_IPA).unwrap_or(ctx.x[0] as u32) as u64;
    ctx.x[1] = read_guest_word_pa(UND_SAVE_R1_IPA).unwrap_or(ctx.x[1] as u32) as u64;
    ctx.x[12] = read_sysreg!("tpidr_el0");

    // DIAG: prove handle_und is being reached at all. Single-shot log.
    static mut UND_ENTRY_LOGGED: bool = false;
    // SAFETY: single-threaded.
    let first = unsafe {
        let was = UND_ENTRY_LOGGED;
        UND_ENTRY_LOGGED = true;
        !was
    };
    if first {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        let far = read_sysreg!("far_el1");
        // ctx.x[13] is SP_usr, ctx.x[14] is LR_usr per Table D1-79 —
        // *not* the source mode's banked SP/LR. The trampoline HVCs
        // from UND mode, so SP_und/LR_und are in ctx.x[23]/ctx.x[22].
        kprintln!(
            "und: handle_und first entry, ELR_EL2={:#x} SPSR_EL2={:#x} FAR_EL1={:#x}",
            elr, spsr, far
        );
        kprintln!(
            "und:   SP_und=ctx.x[23]={:#x}  LR_und=ctx.x[22]={:#x} — LR_und-4 is the faulting PC",
            ctx.x[23] as u32, ctx.x[22] as u32
        );
        kprintln!(
            "und:   r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
        );
    }

    let lr_und = match read_guest_word_pa(UND_SAVE_LR_IPA) {
        Some(v) => v,
        None => {
            kprintln!("*** handle_und: UND_SAVE_LR slot unreadable");
            cpu::halt();
        }
    };
    let spsr_und = read_guest_word_pa(UND_SAVE_SPSR_IPA).unwrap_or(0) as u64;
    let faulting_pc = lr_und.wrapping_sub(4);

    // The faulting PC is a kernel VA (post-MMU); for non-identity-mapped
    // VAs (e.g. the gROMPublicJumpTable aliased at 0x01E00000) the IPA
    // differs from the VA. Try a PA-direct read first, then fall through
    // to a stage-1-walked VA read so the decoder picks up bytes from
    // the actual backing PA when the kernel has set up an aliasing
    // L2 entry.
    let insn = match read_guest_word_pa(faulting_pc)
        .or_else(|| crate::guest_endian::guest_read_u32_va(faulting_pc))
    {
        Some(w) => w,
        None => {
            kprintln!(
                "*** handle_und: faulting PC {:#x} is outside mapped guest memory",
                faulting_pc
            );
            guest_mem::dump_stage1_walk(faulting_pc);
            cpu::halt();
        }
    };

    record_und_history(faulting_pc, insn, spsr_und as u32, ctx);

    // iter-105: catch the first UND that came from a Thumb-mode source.
    // Newton 2.x doesn't use Thumb anywhere, so SPSR_und.T=1 means an
    // interworking branch (BX, BLX, ARMv7+ `mov pc, Rm`, ldr pc / pop pc)
    // landed with bit 0 set. Logging the first such UND pins the
    // wild-branch event close to its source — and more importantly, dumps
    // every register at the moment the CPU vectored to UND, so we can
    // see what r0..r12, SP_usr, LR_usr looked like.
    if (spsr_und as u32) & 0x20 != 0 {
        static mut THUMB_UND_LOGGED: bool = false;
        // SAFETY: single-threaded.
        let first = unsafe {
            let was = THUMB_UND_LOGGED;
            THUMB_UND_LOGGED = true;
            !was
        };
        if first {
            kprintln!(
                "thumb-und: first UND with SPSR_und.T=1 — \
                 PC={:#x} insn={:#010x} SPSR_und={:#010x} mode={:#x}",
                faulting_pc, insn, spsr_und as u32, (spsr_und as u32) & 0x1F,
            );
            kprintln!(
                "thumb-und:   r0..r7:   {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
                ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32,
            );
            kprintln!(
                "thumb-und:   r8..r15:  {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32,
                ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32, ctx.x[15] as u32,
            );
            // x[18..23] are AArch32 banked SP/LR/SPSR per Table D1-79.
            // Also dump the SPSR_svc / SPSR_und register file so we can
            // tell whether a recent SWI tail's `movs pc, lr` was the
            // wild-branch source.
            let sp_usr = ctx.x[13] as u32;
            let lr_usr = ctx.x[14] as u32;
            let sp_svc = ctx.x[19] as u32;
            let lr_svc = ctx.x[18] as u32;
            let sp_und = ctx.x[23] as u32;
            let lr_und_reg = ctx.x[22] as u32;
            let spsr_svc = read_sysreg!("spsr_el1") as u32;
            kprintln!(
                "thumb-und:   SP_usr={:#010x} LR_usr={:#010x}  SP_svc={:#010x} LR_svc={:#010x} SPSR_svc={:#010x}",
                sp_usr, lr_usr, sp_svc, lr_svc, spsr_svc,
            );
            kprintln!(
                "thumb-und:   SP_und={:#010x} LR_und={:#010x}",
                sp_und, lr_und_reg,
            );
        }
    }

    // StrongARM CP15 clock-control write (MCR p15, 0, Rt, c15, c1, 2).
    // ARMv8 doesn't define that register, so the instruction raises UND
    // locally at EL1 rather than trapping via HCR_EL2.TIDCP — which is
    // why we handle it here and not in handle_cp15_trap. Fires exactly
    // once during 717006 boot (probe/FINDINGS.md §16.4); treat as a
    // no-op and advance past it. Mask clears cond (31:28) and Rt
    // (15:12); target encoding is MCR p15,0,Rt,c15,c1,2 (0x_E0F_0F51).
    // The ROM's StrongARM-detect sequence at 0x186a8 uses cond=EQ; the
    // UND only fires when the condition already passed, so any cond
    // is valid here.
    if (insn & 0x0FFF_0FFF) == 0x0E0F_0F51 {
        log_cp15_strongarm_clock(faulting_pc);
        return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Deprecated ARMv4 "Invalidate Unified Cache" encoding
    // `MCR p15, 0, Rt, c7, c7, 0` — ARMv7+/A53 UND this, but the ROM
    // emits it from FlushTheCache at 0x18924 (see the 717006 BootOS
    // flow; Einstein treats this as a valid deprecated cache op and
    // no-ops it). On A53 the JIT probe showed this opcode firing
    // exactly once at boot, from inside FlushTheCache. Emulate as a
    // cache-clean-all via `dsb ish; ic ialluis; isb` and advance past
    // it. Mask clears Rt (15:12).
    if (insn & 0xFFFF_0FFF) == 0xEE07_0F17 {
        log_cp15_deprecated_cache_all(faulting_pc);
        cp15::invalidate_icache_all();
        return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Deprecated ARMv4 "Invalidate Entire Data Cache" encoding
    // `MCR p15, 0, Rt, c7, c6, 0` — same family as c7,c7,0 above
    // (which the kernel emits from FlushTheCache); A53 also UNDs
    // this one. Seen at PC=0x189C0 in the 717006 boot path during
    // FlushDCache. Emulate as a no-op (A53 maintains coherency
    // natively for our config) and advance past it.
    if (insn & 0xFFFF_0FFF) == 0xEE07_0F16 {
        log_cp15_deprecated_cache_all(faulting_pc);
        return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Einstein's JIT (TJITGenericPage.cpp) advances PC by 8 past each
    // of these three UNDs — opcode + a 4-byte payload slot. We mirror
    // that; the payload interpretation varies per UND (debugger logs
    // a string, TapFileCntl takes a command word in R0) but early-boot
    // just needs the PC advance + budgeted visibility.
    match insn {
        0xE6000010 => {
            log_und_budgeted("SystemBootUND", faulting_pc, None);
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        0xE6000510 => {
            // DebuggerUND: opcode followed by a null-terminated ASCII
            // string (typically the debug-log message), padded to the
            // next 4-byte boundary. Einstein's TEmulator::DebuggerUND
            // reads the string byte-by-byte starting at inPAddr+4
            // until it hits a null. We do the same and advance PC past
            // the final word containing the null. If we got this wrong
            // (advance only by 8), the CPU would fall into the middle
            // of the ASCII payload and UND on a random "instruction"
            // (what we saw as insn=0x2d757365 at 0x3ae1ac — "esu-" bytes
            // of "non-user mode.").
            let msg_start = faulting_pc + 4;
            let msg_end = scan_to_null_word_aligned(msg_start, 256);
            log_debugger_und(faulting_pc, msg_start, msg_end);
            return_to_guest_from_und(ctx, msg_end as u64, spsr_und);
        }
        // Newton DDK debug-primitive function-entry UNDs. Each
        // `0xE60000XX10` opcode sits at a symbol in the ROM (ExitToShell,
        // Debugger, DebugStr, SendTestResult, TapFileCntl, RawDebugStr,
        // RawDebugger — see 0x38ce6c..0x38ce84 in rom.dis) and is called
        // via `BL <symbol>`. Einstein's JIT (TJITGeneric_Other.cpp)
        // emulates TapFileCntl with `POPNIL(); SETPC(GETCALLER() + 4)` —
        // i.e. return to the caller's LR. The rest fall through Einstein's
        // generic UndefinedInstruction path and take a real ARM UND
        // exception; on our guest that wedges because `gDebugger = 1`
        // makes the ROM's 0x38ce88 handler jump to ReportException →
        // StopImage. So every one of these must be emulated in EL2 as
        // a "log and return to caller" NOP.
        //
        // The caller's LR is captured by the UND trampoline into the
        // `UND_SAVE_BANKED_LR_IPA` RAM slot (the trampoline briefly
        // switches to the faulting mode — SYS for USR — and stores that
        // mode's banked LR there). ERETing to that address resumes the
        // caller's instruction stream after the BL.
        0xE6000110 | 0xE6000210 | 0xE6000310 | 0xE6000710 | 0xE6000810 => {
            let name = match insn {
                0xE6000110 => "ExitToShell",
                0xE6000210 => "Debugger",
                0xE6000310 => "DebugStr",
                0xE6000710 => "SendTestResult",
                0xE6000810 => "TapFileCntl",
                _ => "DDK-UND",
            };
            let r0 = ctx.x[0] as u32;
            log_und_budgeted(name, faulting_pc, Some(r0));
            // Each of these UND opcodes is a Newton-DDK function entry,
            // called from ROM code via `BL <symbol>` (see rom.dis around
            // 0x38ce6c..0x38ce84). Einstein's JIT returns to the caller
            // via `POPNIL; SETPC(GETCALLER()+4)` for TapFileCntl and the
            // same shape applies to the rest. The UND trampoline
            // captures the faulting mode's banked LR (via its mode-
            // switch dance — see `patch_und_vector` in `guest_mem.rs`)
            // into the `UND_SAVE_BANKED_LR_IPA` RAM slot so we can ERET
            // there.
            let banked_lr = read_guest_word_pa(UND_SAVE_BANKED_LR_IPA).unwrap_or(0);
            if banked_lr == 0 {
                kprintln!(
                    "*** {} @PC={:#x}: banked LR slot @{:#x} is 0 — UND trampoline must \
                     stage the faulting mode's LR before HVC (see ROM trampoline mode-\
                     switch dance in patch_und_vector). Halting.",
                    name, faulting_pc, UND_SAVE_BANKED_LR_IPA,
                );
                cpu::halt();
            }
            // TapFileCntl has an Einstein-modelled dispatch table
            // (do_sys_open / read / write / …) — we don't implement the
            // file protocol, so write -1 to R0 as a "call failed" result
            // that the caller can observe. The other primitives leave R0
            // alone.
            if insn == 0xE6000810 {
                ctx.x[0] = 0xFFFF_FFFFu32 as u64;
            }
            return_to_guest_from_und(ctx, banked_lr as u64, spsr_und);
        }
        _ if is_swp_encoding(insn) => {
            emulate_swp(ctx, insn, faulting_pc);
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        // `MRS Rd, SPSR` executed in USR mode. On ARMv4 / SA-1100 this
        // returns the CPSR (no SPSR exists for USR); the A53 UNDs it.
        // Einstein models this at `TARMProcessor::GetSPSR()`
        // (TARMProcessor.cpp:774-781): "At MonitorEntryGlue and
        // elsewhere, the OS accesses SPSR in User mode and apparently
        // gets CPSR." Emulate by writing the pre-UND CPSR (i.e.
        // `spsr_und`, which the UND trampoline captured from the
        // hardware-saved SPSR_und) into Rd and advancing PC by 4. Rd
        // is extracted from bits[15:12]; per the MRS encoding, r15 is
        // UNPREDICTABLE here, so bail if the guest asked for it.
        _ if (insn & 0x0FFF_0FFF) == 0x014F_0000
            && (spsr_und & 0x1F) == 0x10 =>
        {
            let rd = ((insn >> 12) & 0xF) as usize;
            if rd == 15 {
                kprintln!(
                    "*** MRS R15, SPSR (USR): UNPREDICTABLE at PC={:#x}",
                    faulting_pc
                );
                cpu::halt();
            }
            ctx.x[rd] = spsr_und;
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        // Tracer trampoline slot[0] executed in USR mode. HVC is
        // UNDEFINED at EL0, so the trampoline's `hvc #TRACE_TAG`
        // raises an UND exception instead of entering EL2 directly.
        // Log the entry (same content as the normal HVC path) and
        // resume at slot[1] — the original first instruction copy —
        // restoring the USR-mode CPSR. Without this, any traced
        // function the Newton kernel calls in user mode (e.g. OsBoot
        // per the `code-symbols.txt` classification) halts here.
        #[cfg(feature = "trace")]
        _ if insn == crate::tracer::TRACE_HVC_INSN
            && crate::tracer::in_trampoline_pool(faulting_pc) =>
        {
            crate::tracer::log_trace_at(ctx, faulting_pc, spsr_und as u32);
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        // Reboot canary (rom_patches::REBOOT_PC = 0x000D_9884). The
        // kernel calls `Reboot(long, ...)` from USR mode on
        // UnhandledException; HVC at EL0 is UNDEFINED, so our
        // patched `HVC #REBOOT_HVC_IMM` lands here. Route into the
        // same halt handler the HVC path uses.
        _ if insn == rom_patches_hvc_insn(crate::rom_patches::REBOOT_HVC_IMM)
            && faulting_pc == crate::rom_patches::REBOOT_PC =>
        {
            handle_reboot(ctx);
        }
        // PowerOffAndReboot canary, symmetric case for the USR-mode
        // call path.
        _ if insn == rom_patches_hvc_insn(crate::rom_patches::POWEROFF_REBOOT_HVC_IMM)
            && faulting_pc == crate::rom_patches::POWEROFF_REBOOT_PC =>
        {
            handle_poweroff_reboot(ctx);
        }
        // BootOS / ROMBoot canary (rom_patches::BOOTOS_PC = 0x0001_8688).
        // The initial hypervisor-ERET lands here in SVC mode (HVC traps
        // normally to EL2). Any later entry from USR mode is a software
        // reset reached via a task branching to the reset vector — HVC
        // from EL0 is UNDEFINED and arrives here instead of handle_hvc.
        // Route into the same handler so the canary's "2nd+ entry →
        // halt" logic applies regardless of the source mode.
        _ if insn == rom_patches_hvc_insn(crate::rom_patches::BOOTOS_HVC_IMM)
            && faulting_pc == crate::rom_patches::BOOTOS_PC =>
        {
            handle_bootos_canary(ctx);
            return;
        }
        // L1[0xCD] investigation probes: the patched HVC instructions
        // sit inside `Remember` and `AllocatePageTable`, which the
        // kernel calls from both SVC (kernel-side fault chain) and USR
        // (user-mode wrappers like the post-ship patch table). HVC
        // from USR is UNDEFINED, so those calls land here. Pass the
        // trampoline-saved `spsr_und` (= the original USR-caller CPSR)
        // directly to the inner probe so its SP/LR lookups land on the
        // right banked register, then advance ELR via the UND-return
        // stub since UND entry doesn't auto-advance.
        _ if insn == rom_patches_hvc_insn(crate::rom_patches::REMEMBER_SWIRET_HVC_IMM) => {
            handle_remember_swiret_probe_with(ctx);
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
            return;
        }
        _ if insn == rom_patches_hvc_insn(crate::rom_patches::UNHANDLED_EXCEPTION_HVC_IMM) => {
            handle_unhandled_exception(ctx, false);
            // Never returns: handle_unhandled_exception halts.
        }
        _ if insn == rom_patches_hvc_insn(crate::rom_patches::UNHANDLED_NUM_EXCEPTION_HVC_IMM) => {
            handle_unhandled_exception(ctx, true);
            // Never returns: handle_unhandled_exception halts.
        }
        // User-driven guest software breakpoint — must be checked
        // before the tracer path because the marker encoding
        // (UDF #0xFFFE) is also a UDF-shape instruction. See
        // `src/guest_bp.rs`.
        _ if insn == crate::guest_bp::BP_UDF_INSN => {
            if !crate::guest_bp::handle_user_bp_und(ctx, faulting_pc, spsr_und, insn) {
                kprintln!(
                    "*** guest_bp: marker at PC={:#x} with no matching table entry — halting",
                    faulting_pc
                );
                cpu::halt();
            }
        }
        // Shadow-byte-access UDF — the patched byte/halfword-access
        // site raises UND, this arm emulates the access in EL2 Rust.
        // See `src/shadow_stub.rs`.
        _ if shadow_stub::is_sba_udf_insn(insn) => {
            if !shadow_stub::handle_sba_udf(ctx, faulting_pc, spsr_und, insn) {
                kprintln!(
                    "*** shadow_stub: SBA UDF at PC={:#x} insn={:#010x} failed — halting",
                    faulting_pc, insn
                );
                cpu::halt();
            }
        }
        // FPA control/status register access: RFS / WFS / RFC / WFC.
        // These UND on A53 (no FPA coprocessor) and — per ARMv8 B2.2.4 —
        // may UND even when their condition is false. Emulate as a NOP:
        // reads return 0 in Rt, writes are discarded. Nothing Newton boot
        // actually runs exercises the FPA control/status registers —
        // FPE_Install's helper at 0x392704 uses `rfceq`/`wfceq` to init
        // the emulator state, and the context-word semantic (rounding
        // mode, trap enables) is never consulted by integer-math boot
        // code. See INVESTIGATION.md for the full FPE_Install analysis.
        _ if is_fpa_ctrl_reg_insn(insn) => {
            emulate_fpa_ctrl_reg(ctx, insn, faulting_pc, spsr_und);
        }
        // FPA load/store/arithmetic UNDs are bypassed at the UND-vector
        // level by `FPA_BYPASS_STUB_OFFSET` (see guest_mem.rs) — the
        // stub at IPA 0x00FF_FEC0 detects FPA-class insns and branches
        // directly to `FP_UndefHandlers_Start_JT` at 0x38d874 without
        // entering EL2. SA-110 hardware delivered the UND to the
        // kernel's FPE this way; our previous EL2 round-trip path
        // (`forward_und_to_guest_fpe`) leaked enough state corruption
        // to wedge the FPE's IP-preservation invariant on certain
        // dispatch arms (iter-83/84/85). Reaching this arm now means
        // the bypass stub didn't recognise the opcode as FPA — halt
        // loudly so we can investigate.
        _ if is_fpa_insn(insn) => {
            kprintln!(
                "*** FPA UND reached EL2 — bypass stub at 0x00FF_FEC0 \
                 should have routed this directly to FPE_JT. \
                 insn={:#010x} PC={:#x}",
                insn, faulting_pc,
            );
            cpu::halt();
        }
        _ => {
            // Stop the tarmac window before any further EL2 work runs
            // (the diagnostic kprintln!'s below would otherwise appear
            // in the trace and bloat it). The window was opened by the
            // FPE-entry probe at 0x38d918 on the third FPE entry —
            // exactly the call that wedges on the IP-corruption trap.
            crate::tarmac::emit_stop();
            kprintln!(
                "*** unrecognised UND: insn={:#010x} at PC={:#x} SPSR_und={:#x}",
                insn, faulting_pc, spsr_und
            );
            kprintln!(
                "  src_mode={:#x} ({})  r0..r7:   {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                (spsr_und as u32) & 0x1F,
                describe_aarch32_mode((spsr_und as u32) & 0x1F),
                ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
                ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32,
            );
            kprintln!(
                "                       r8..r15:  {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32,
                ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32, ctx.x[15] as u32,
            );
            kprintln!(
                "                       SP_und=ctx.x[23]={:#x} LR_und=ctx.x[22]={:#x}",
                ctx.x[23] as u32, ctx.x[22] as u32,
            );
            kprintln!(
                "    (extend handle_und in trap.rs to handle this opcode)"
            );
            dump_und_history();
            // iter-87 diag: dump the USR stack near SP_usr (via stage-1
            // walk) — if USR reached PC=0xffff54 via POP {pc} or LDM,
            // the popped value should still be visible just below SP_usr.
            let sp_usr = ctx.x[13] as u32;
            let read_va = |va: u32| -> Option<u32> {
                let pa = guest_mem::translate_va(va)?;
                read_guest_word_pa(pa)
            };
            kprintln!("USR stack (SP_usr={:#010x}, words at sp-32..sp+96):", sp_usr);
            for i in 0..32i32 {
                let addr = sp_usr.wrapping_add((i.wrapping_sub(8) * 4) as u32);
                let v = read_va(addr)
                    .map(|w| w as i64)
                    .unwrap_or(-1);
                if v < 0 {
                    kprintln!("  [{:#010x}] = (unmapped)", addr);
                } else {
                    kprintln!("  [{:#010x}] = {:#010x}", addr, v as u32);
                }
            }
            // Also resolve the BL target chain to spot a corrupt JT thunk:
            // the most-recent BL was at LR_usr-4. Show its insn and decoded
            // target, then the word at the target (the JT thunk's `b`).
            let lr_usr = ctx.x[14] as u32;
            let bl_pc = lr_usr.wrapping_sub(4);
            kprintln!("BL site (LR_usr-4 = {:#010x}):", bl_pc);
            let bl_insn = read_va(bl_pc).unwrap_or(0xDEAD_BEEF);
            kprintln!("  insn = {:#010x}", bl_insn);
            if (bl_insn & 0xFF00_0000) == 0xEB00_0000 {
                let imm24 = bl_insn & 0x00FF_FFFF;
                let signed = ((imm24 << 8) as i32) >> 8;
                let target =
                    bl_pc.wrapping_add(8).wrapping_add((signed as u32).wrapping_shl(2));
                kprintln!("  decoded BL target = {:#010x}", target);
                let target_insn = read_va(target).unwrap_or(0xDEAD_BEEF);
                kprintln!("  insn at target = {:#010x}", target_insn);
                // If the target is a `b imm24` (JT thunk), follow it.
                if (target_insn & 0xFF00_0000) == 0xEA00_0000 {
                    let imm24b = target_insn & 0x00FF_FFFF;
                    let signedb = ((imm24b << 8) as i32) >> 8;
                    let target2 = target
                        .wrapping_add(8)
                        .wrapping_add((signedb as u32).wrapping_shl(2));
                    kprintln!("  jt target follows-> {:#010x}", target2);
                    let target2_insn = read_va(target2).unwrap_or(0xDEAD_BEEF);
                    kprintln!("  insn at jt target = {:#010x}", target2_insn);
                    // And the next 3 insns of the function body.
                    for off in [4u32, 8, 12, 16] {
                        let v = read_va(target2.wrapping_add(off))
                            .unwrap_or(0xDEAD_BEEF);
                        kprintln!("  insn at {:#010x} = {:#010x}",
                                  target2.wrapping_add(off), v);
                    }
                }
            }
            // Also dump the trampoline area so we can verify the HVC
            // is at 0xffff54.
            kprintln!("trampoline area:");
            for off in [0u32, 4, 8, 0x40, 0x44, 0x50, 0x54, 0x58, 0x5C].iter() {
                let addr = 0x00FF_FF00u32.wrapping_add(*off);
                let v = read_va(addr).unwrap_or(0xDEAD_BEEF);
                kprintln!("  insn at {:#010x} = {:#010x}", addr, v);
            }
            cpu::halt();
        }
    }
}

/// Does `insn` match one of the four FPA control/status register
/// encodings — RFS, WFS, RFC, WFC — targeting CP1?
///
///   RFS: cccc 1110 0011 0000 Rt 0001 0001 0000  (MRC p1, 1, Rt, c0, c0, 0)
///   WFS: cccc 1110 0010 0000 Rt 0001 0001 0000  (MCR p1, 1, Rt, c0, c0, 0)
///   RFC: cccc 1110 0101 0000 Rt 0001 0001 0000  (MRC p1, 2, Rt, c0, c0, 0)
///   WFC: cccc 1110 0100 0000 Rt 0001 0001 0000  (MCR p1, 2, Rt, c0, c0, 0)
///
/// The common bits fix the shape as `0x?E00_?110` with bits 23:20 ∈
/// {2, 3, 4, 5}. Mask 0x0F0F_0FFF preserves everything except cond
/// (31:28), opc1/L (23:20), and Rt (15:12); the fixed pattern is
/// 0x0E00_0110. We then require bits 23:20 to be one of the four
/// control/status register values — this leaves FPA data ops (ADF, LDF,
/// …) and non-CP1 accesses to halt loudly, which is the right Phase-A
/// trip-wire behaviour.
fn is_fpa_ctrl_reg_insn(insn: u32) -> bool {
    if (insn & 0x0F0F_0FFF) != 0x0E00_0110 {
        return false;
    }
    matches!((insn >> 20) & 0xF, 2 | 3 | 4 | 5)
}

/// Does `insn` match an FPA-class encoding targeting cp1 or cp2?
///
/// Covers: LDF/STF (LDC/STC, bits[27:24]=0xC,0xD with the N bit selecting
/// the LFM/SFM multi-register variants), CDP (FPA arithmetic — ADF, MUF,
/// MVF, CMF, …; bits[27:24]=0xE, bit[4]=0), and MCR/MRC (FIX/FLT/etc.;
/// bits[27:24]=0xE, bit[4]=1). The Newton kernel's FPA emulator at ROM
/// 0x38d8dc handles every shape in this family.
///
/// `cond == 0xF` (unconditional) is excluded — that encoding is reserved
/// for VFP/Advanced SIMD on ARMv5+ and never appears in 717006 ROM. The
/// existing `is_fpa_ctrl_reg_insn` arm runs first and catches RFS/WFS/RFC/
/// WFC as in-EL2 NOPs, so this returns true for those too but is harmless
/// (the ctrl-reg arm matches earlier in the dispatch chain).
fn is_fpa_insn(insn: u32) -> bool {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        return false;
    }
    let coproc = (insn >> 8) & 0xF;
    if coproc != 1 && coproc != 2 {
        return false;
    }
    matches!((insn >> 24) & 0xF, 0xC | 0xD | 0xE)
}

/// Emulate an FPA control/status register access (RFS / WFS / RFC /
/// WFC) as a NOP: reads return 0 in Rt, writes are discarded, PC
/// advances by 4. Respects the ARM condition code — an FVP-taken UND
/// on a false-condition `rfceq` etc. leaves Rt alone, matching the
/// architecturally-specified NOP behaviour (ARMv8 B2.2.4).
fn emulate_fpa_ctrl_reg(
    ctx: &mut TrapContext,
    insn: u32,
    faulting_pc: u32,
    spsr_und: u64,
) {
    let cond = (insn >> 28) & 0xF;
    let passed = arm_condition_passed(cond, spsr_und as u32);
    if passed {
        let is_read = ((insn >> 20) & 1) != 0;
        let rt = ((insn >> 12) & 0xF) as usize;
        // Rt == r15 is UNPREDICTABLE for RFS/RFC on FPA; ignore the
        // write rather than scribble on the AArch64 context's x15.
        if is_read && rt < 15 {
            ctx.x[rt] = 0;
        }
        // Write path: discard the source value. The FPA control word
        // holds rounding mode + trap enables, neither observable under
        // our emulation.
    }
    log_fpa_ctrl_reg(faulting_pc, insn, passed);
    return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
}

/// Evaluate an ARM A1 condition field against NZCV flags from a CPSR
/// snapshot. Cond == 0xF (unconditional) is not reachable here because
/// the FPA control/status encodings always have a real condition in
/// bits 31:28; defensively we treat it as AL.
fn arm_condition_passed(cond: u32, cpsr: u32) -> bool {
    let n = (cpsr >> 31) & 1;
    let z = (cpsr >> 30) & 1;
    let c = (cpsr >> 29) & 1;
    let v = (cpsr >> 28) & 1;
    match cond & 0xF {
        0x0 => z == 1,                  // EQ
        0x1 => z == 0,                  // NE
        0x2 => c == 1,                  // CS / HS
        0x3 => c == 0,                  // CC / LO
        0x4 => n == 1,                  // MI
        0x5 => n == 0,                  // PL
        0x6 => v == 1,                  // VS
        0x7 => v == 0,                  // VC
        0x8 => c == 1 && z == 0,        // HI
        0x9 => c == 0 || z == 1,        // LS
        0xA => n == v,                  // GE
        0xB => n != v,                  // LT
        0xC => z == 0 && n == v,        // GT
        0xD => z == 1 || n != v,        // LE
        0xE => true,                    // AL
        _ => true,                      // 0xF: defensive
    }
}

fn log_fpa_ctrl_reg(pc: u32, insn: u32, cond_passed: bool) {
    const SEEN_CAP: usize = 16;
    static mut SEEN: [u32; SEEN_CAP] = [0; SEEN_CAP];
    static mut SEEN_N: usize = 0;
    // SAFETY: single-threaded EL2.
    let first = unsafe {
        let mut found = false;
        for i in 0..SEEN_N { if SEEN[i] == pc { found = true; break; } }
        if !found && SEEN_N < SEEN_CAP {
            SEEN[SEEN_N] = pc;
            SEEN_N += 1;
            true
        } else {
            false
        }
    };
    if first {
        let name = match (insn >> 20) & 0xF {
            2 => "WFS",
            3 => "RFS",
            4 => "WFC",
            5 => "RFC",
            _ => "FPA-CR?",
        };
        let rt = (insn >> 12) & 0xF;
        kprintln!(
            "und: FPA {} r{} @PC={:#x} — NOP (cond {})",
            name,
            rt,
            pc,
            if cond_passed { "passed" } else { "failed" },
        );
    }
}

/// Generic "inspect-then-halt" diagnostic HVC handler.
///
/// Invoked when a vector (typically 0x10 DABT or 0x0C PABT) has been
/// patched to `HVC #DIAG_TAG` during Phase B debugging. Dumps:
/// - ELR_EL2 (PC after HVC), SPSR_EL2, ESR (via caller's trap path)
/// - FAR_EL1 (original faulting VA, preserved across HVC)
/// - Banked SPSR_<mode> for all non-current exception modes
/// - Guest x0..x14 (= AArch32 R0..R14 of the mode that executed HVC,
///   where R13/R14 are banked)
/// - Guest stage-1 translation walk for FAR_EL1
///
/// Then halts loudly. Useful for any abort we don't see at EL2 because
/// the guest handles it at EL1; patching the vector and running lets
/// us catch the abort context once before the guest's own handler
/// clobbers it.

// ---

/// Canary handler: the guest hit `PowerOffAndReboot` (ROM PC 0x000E_6BBC).
/// rom_patches patched the first word to `HVC #POWEROFF_REBOOT_HVC_IMM`,
/// so we land here BEFORE the function's prologue runs — ctx.x[0..14]
/// alias the caller's AArch32 R0..R14, and ELR_EL2 == PowerOffAndReboot
/// entry PC.
///
/// The kernel calls this whenever some fatal init-time check fails
/// (flash identification, memory test, ROM checksum, etc.). Rebooting
/// is the kernel's response; under the hypervisor the same failure
/// will recur indefinitely, so we dump state and halt loudly on the
/// first hit — no ERET.
fn handle_poweroff_reboot(ctx: &TrapContext) -> ! {
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    let elr_el2 = read_sysreg!("elr_el2");
    let mode = spsr_el2 & 0x1F;
    let caller_lr = crate::banked::lr_for_mode(ctx, spsr_el2);
    kprintln!();
    kprintln!("*** PowerOffAndReboot canary fired — guest is giving up ***");
    kprintln!(
        "  ELR_EL2  = {:#010x}  (= PowerOffAndReboot entry PC)",
        elr_el2
    );
    kprintln!(
        "  SPSR_EL2 = {:#010x}  mode={} ({:#x})",
        spsr_el2, describe_aarch32_mode(mode), mode
    );
    kprintln!(
        "  R0 = {:#010x}  (reboot reason code, passed in by caller)",
        ctx.x[0] as u32
    );
    kprintln!(
        "  R1 = {:#010x}  R2 = {:#010x}  R3 = {:#010x}",
        ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
    );
    kprintln!(
        "  R12={:#010x}  R14_{}={:#010x}  (caller LR via Table D1-79)",
        ctx.x[12] as u32, describe_aarch32_mode(mode), caller_lr
    );
    kprintln!();
    kprintln!("  Flash bank 0 host bytes [0..0x40]:");
    dump_flash_bytes(0x0200_0000, 0x40);
    kprintln!("  Flash bank 0 host bytes [0x40..0x90]:");
    dump_flash_bytes(0x0200_0040, 0x50);
    kprintln!();
    kprintln!("  RAM read-back buffer 0x034af38c..0x034af3ac (LE host bytes):");
    dump_ram_bytes(0x034a_f38c, 0x20);
    kprintln!("  RAM expected buffer  0x034af228..0x034af2b0 (LE host bytes):");
    dump_ram_bytes(0x034a_f228, 0x90);
    kprintln!();
    kprintln!(
        "  (See the tracer entries immediately preceding this line for the"
    );
    kprintln!(
        "   caller chain. The check that failed is the last non-trivial"
    );
    kprintln!(
        "   function called before PowerOffAndReboot.)"
    );
    cpu::halt();
}

/// Canary handler for `Reboot(long, unsigned long, unsigned char)` at
/// 0x000D_9884. Symmetric with `handle_poweroff_reboot`: halt on the
/// first hit with R0..R3 (reboot reason / flags / ...) and the preceding
/// tracer line naming the caller.
/// AArch32 `HVC #imm16` encoding at unconditional (cond=AL). Mirror of
/// `rom_patches::hvc_insn` — duplicated here because that helper is
/// `const fn` and not exported.
const fn rom_patches_hvc_insn(imm: u32) -> u32 {
    0xE140_0070 | ((imm & 0xFFF0) << 4) | (imm & 0xF)
}

/// Canary handler for `BootOS` / `ROMBoot` (0x0001_8688). The AArch32
/// reset vector at VA 0 branches here, so the first entry after the
/// hypervisor ERETs the guest is legitimate — we emulate the original
/// first instruction (`mov r0, #0xb0`) and advance ELR so the kernel
/// continues. Every SUBSEQUENT entry is a software reset (watchdog,
/// `Reboot`, `PowerOffAndReboot`, or a direct jump to the reset
/// vector); we dump state and halt. Complements the already-canaried
/// `Reboot` / `PowerOffAndReboot` entry points by catching reset
/// paths that bypass them.
fn handle_bootos_canary(ctx: &mut TrapContext) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static ENTRIES: AtomicU32 = AtomicU32::new(0);
    let n = ENTRIES.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 {
        // First boot. Emulate `mov r0, #0xb0` (the word we overwrote
        // with the HVC) and ERET to BootOS + 4 so the kernel runs
        // through its normal boot sequence.
        ctx.x[0] = 0xb0;
        let next_pc = (crate::rom_patches::BOOTOS_PC + 4) as u64;
        // SAFETY: ELR_EL2 controls the post-ERET guest PC.
        unsafe {
            core::arch::asm!(
                "msr elr_el2, {}",
                in(reg) next_pc,
                options(nostack, preserves_flags),
            );
        }
        kprintln!("BootOS canary: first boot — emulated mov r0,#0xb0 and passing through");
        return;
    }

    // Second+ entry — software reset.
    // Stop the tarmac-window capture before any further EL2 work runs
    // (the halt message itself will appear in the trace if we emit the
    // stop AFTER the `*** BootOS canary fired ...` line).
    crate::tarmac::emit_stop();
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    let elr_el2 = read_sysreg!("elr_el2");
    let mode = spsr_el2 & 0x1F;
    let caller_lr = crate::banked::lr_for_mode(ctx, spsr_el2);
    kprintln!();
    kprintln!("*** BootOS canary fired on entry #{} — software reset detected ***", n);
    kprintln!(
        "  ELR_EL2  = {:#010x}  (= BootOS entry PC)",
        elr_el2
    );
    kprintln!(
        "  SPSR_EL2 = {:#010x}  mode={} ({:#x})",
        spsr_el2, describe_aarch32_mode(mode), mode
    );
    kprintln!(
        "  R0 = {:#010x}  R1 = {:#010x}  R2 = {:#010x}  R3 = {:#010x}",
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
    );
    kprintln!(
        "  R12={:#010x}  R14_{}={:#010x}  (caller LR via Table D1-79)",
        ctx.x[12] as u32, describe_aarch32_mode(mode), caller_lr
    );
    kprintln!();
    kprintln!(
        "  Preceding tracer entries show what the kernel was doing before"
    );
    kprintln!(
        "  the reset. Common triggers: watchdog timeout, Reboot() / "
    );
    kprintln!(
        "  PowerOffAndReboot (separately canaried), or a direct jump to"
    );
    kprintln!(
        "  the reset vector at VA 0."
    );
    cpu::halt();
}

/// Read up to `max` bytes of an ASCII C-string from guest VA, stopping
/// at NUL or unmapped page. Used for exception-name dumps.
fn read_cstr_at(va: u32, max: usize) -> ([u8; 128], usize) {
    let mut buf = [0u8; 128];
    let cap = max.min(128);
    let mut len = 0;
    let mut i = 0usize;
    while i < cap {
        // Read a 32-bit word at the next word-aligned position so we
        // can extract the relevant bytes — stage-1 translate is
        // word-granular in our helpers.
        let word_va = (va.wrapping_add(i as u32)) & !0x3;
        let off = ((va.wrapping_add(i as u32)) & 0x3) as usize;
        let w = match crate::guest_endian::guest_read_u32_va(word_va) {
            Some(w) => w,
            None    => break,
        };
        // Newton 2.x stores strings in BE-byte-order even in our LE-
        // word view (BE32 kernel built against SA-1100; iter-30 docs).
        // Within a word, byte k of the string is `(w >> ((3-k)*8))`.
        for j in off..4 {
            if i >= cap { break; }
            let shift = (3 - j) * 8;
            let b = ((w >> shift) & 0xFF) as u8;
            if b == 0 { return (buf, len); }
            buf[i] = b;
            len = i + 1;
            i += 1;
        }
    }
    (buf, len)
}

fn print_exception_name(label: &str, name_va: u32) {
    let (buf, len) = read_cstr_at(name_va, 128);
    let s = core::str::from_utf8(&buf[..len]).unwrap_or("<non-utf8>");
    if len == 0 {
        kprintln!("  {} @ VA={:#010x}: <unmapped or empty>", label, name_va);
    } else {
        kprintln!("  {} @ VA={:#010x}: \"{}\"", label, name_va, s);
    }
}

/// Halt-on-entry tripwire for `UnhandledException(char*, void*,
/// void(*)(void*))` (and the NonUserMode variant). The kernel calls
/// these when it can't dispatch an exception to any installed handler;
/// the caller passes the exception NAME as a C-string in r0. Catching
/// here is far cleaner than letting Reboot fire and decoding the
/// stack-passed string from a downstream caller.
///
/// `non_user` distinguishes the two variants (false ⇒ regular USR
/// Common halt path for invariant-violation tripwires (iter-30+
/// instrumentation pass). Emits a uniform header, runs the per-
/// assertion local-context dump, runs `task_dump::dump()` for
/// scheduler/task state, then halts. Use for any check that should
/// stop the boot at the first 4-KiB-hypothesis violation rather
/// than chase the symptom downstream.
#[inline(never)]
fn halt_invariant(label: &str, local_dump: impl FnOnce()) -> ! {
    let elr = read_sysreg!("elr_el2");
    let spsr = read_sysreg!("spsr_el2") as u32;
    kprintln!();
    kprintln!("*** invariant violation: {} ***", label);
    kprintln!(
        "  ELR_EL2={:#x} SPSR_EL2={:#x} src_mode={:#x}",
        elr, spsr, spsr & 0x1F,
    );
    local_dump();
    kprintln!();
    kprintln!("--- task_dump ---");
    crate::task_dump::dump();
    kprintln!("--- end task_dump ---");
    cpu::halt();
}

fn dump_flash_bytes(ipa: u32, count: u32) {
    let base = crate::peripherals::flash::host_pa() as *const u8;
    let flash_off = ipa.wrapping_sub(crate::peripherals::flash::BANK0_PA_BASE) as usize;
    for row in 0..((count + 15) / 16) {
        let off = row * 16;
        let mut hex = [0u8; 48]; // " %02x" * 16 = 48 chars + nul
        let mut i = 0;
        for j in 0..16 {
            if off + j >= count { break; }
            // SAFETY: host_pa + flash_off + off + j bounded by BANK_SIZE.
            let b = unsafe { *base.add(flash_off + (off as usize) + j as usize) };
            let hi = b >> 4;
            let lo = b & 0xF;
            hex[i] = if hi < 10 { b'0' + hi } else { b'a' + hi - 10 };
            hex[i+1] = if lo < 10 { b'0' + lo } else { b'a' + lo - 10 };
            hex[i+2] = b' ';
            i += 3;
        }
        let s = core::str::from_utf8(&hex[..i]).unwrap_or("<?>");
        kprintln!("    {:#010x}: {}", ipa + off, s);
    }
}

fn dump_ram_bytes(va: u32, count: u32) {
    // Walk guest stage-1 to resolve VA -> PA first.
    let pa = match guest_mem::translate_va(va) {
        Some(p) => p,
        None => {
            kprintln!("    {:#010x}: <no stage-1 mapping>", va);
            return;
        }
    };
    for row in 0..((count + 15) / 16) {
        let off = row * 16;
        let mut hex = [0u8; 48];
        let mut i = 0;
        for j in 0..16 {
            if off + j >= count { break; }
            let b = match guest_mem::read_byte_pa(pa + off + j) {
                Some(b) => b,
                None => { kprintln!("    {:#010x}: <pa {:#x} unmapped>", va + off + j, pa + off + j); return; }
            };
            let hi = b >> 4;
            let lo = b & 0xF;
            hex[i] = if hi < 10 { b'0' + hi } else { b'a' + hi - 10 };
            hex[i+1] = if lo < 10 { b'0' + lo } else { b'a' + lo - 10 };
            hex[i+2] = b' ';
            i += 3;
        }
        let s = core::str::from_utf8(&hex[..i]).unwrap_or("<?>");
        kprintln!("    VA {:#010x} (PA {:#010x}): {}", va + off, pa + off, s);
    }
}

/// Phase-0 stub for the kernel-intent mask tracker. The full tracker
/// (`KERNEL_INTENT_MASK` + `kim_remember` / `kim_forget`) was driven by
/// the deleted PrimRememberMapping / PrimForgetMapping probes and went
/// away with them. The pa_emulate / guest_mem callers handle `None`
/// gracefully (treat as "no kernel intent recorded → don't flag"), so
/// the stub keeps the API live without resurrecting the data path.
pub fn kernel_intent_mask_for(_pa: u32, _va: u32) -> Option<u32> {
    None
}

/// path, true ⇒ kernel/UND path). Halts via `halt_invariant`.
fn handle_unhandled_exception(ctx: &TrapContext, non_user: bool) -> ! {
    let r0 = ctx.x[0] as u32;
    let r1 = ctx.x[1] as u32;
    let r2 = ctx.x[2] as u32;
    let r3 = ctx.x[3] as u32;
    let trampoline_saved_spsr = crate::guest_endian::guest_read_u32_pa(UND_SAVE_SPSR_IPA).unwrap_or(0);
    let true_source_mode = trampoline_saved_spsr & 0x1F;
    let true_caller_lr = crate::banked::lr_for_mode(ctx, trampoline_saved_spsr);
    let true_source_sp = crate::banked::sp_for_mode(ctx, trampoline_saved_spsr);
    let label = if non_user { "UnhandledNonUserModeException" } else { "UnhandledException" };
    halt_invariant("kernel reached UnhandledException — exception had no handler", || {
        kprintln!("  variant: {}", label);
        kprintln!(
            "  r0={:#010x}  r1={:#010x}  r2={:#010x}  r3={:#010x}",
            r0, r1, r2, r3,
        );
        print_exception_name("exception name (r0)", r0);
        kprintln!(
            "  TRUE source mode={} ({:#x})  caller_lr={:#010x}  sp={:#010x}",
            describe_aarch32_mode(true_source_mode),
            true_source_mode, true_caller_lr, true_source_sp,
        );
        kprintln!("  exception data (r1) — first 8 words:");
        for i in 0..8 {
            let va = r1.wrapping_add(i * 4);
            match crate::guest_endian::guest_read_u32_va(va) {
                Some(w) => kprintln!("    [{:+3}] @{:#010x} = {:#010x}", (i * 4) as i32, va, w),
                None    => kprintln!("    [{:+3}] @{:#010x} = (unmapped)", (i * 4) as i32, va),
            }
        }
    });
}

/// FPE-entry probe: patched at `0x0038_D918` over `mov ip, sp`. Counts
/// FPE entries; on the call that's expected to wedge on the
/// IP-corruption trap (entry index 2 = forward #2 = mvfs in
/// SetSystemVolume), opens the TarmacTrace window via
/// `crate::tarmac::emit_start()`. The matching `emit_stop()` fires from
/// the unrecognised-UND halt path. The trace bracketed by these two
/// markers contains every retired instruction inside the FPE call —
/// grep for the moment R12 is written with `0x003900c8` to pin the
/// IP-clobber site.
///
/// Then emulates the original `mov ip, sp` by setting
/// `ctx.x[12] = ctx.x[23]` (= sp_und, since the FPE always runs in
/// UND mode after iter-84's bypass routes UND naturally to FPE_JT).
fn handle_fpe_entry_probe(ctx: &mut TrapContext) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static FIRED: AtomicU32 = AtomicU32::new(0);
    let n = FIRED.fetch_add(1, Ordering::Relaxed);
    let sp_und = ctx.x[23] as u32;
    if n < 8 {
        kprintln!("FPE-entry[{}]: sp_und={:#010x}", n, sp_und);
    }
    // Emulate `mov ip, sp` (= ctx.x[23] in UND-source mode).
    ctx.x[12] = (ctx.x[12] & 0xFFFF_FFFF_0000_0000) | (sp_und as u64);
}

fn handle_reboot(ctx: &TrapContext) -> ! {
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    let elr_el2 = read_sysreg!("elr_el2");
    let mode = spsr_el2 & 0x1F;
    let caller_lr = crate::banked::lr_for_mode(ctx, spsr_el2);
    kprintln!();
    kprintln!("*** Reboot canary fired — guest kernel is rebooting ***");
    kprintln!(
        "  ELR_EL2  = {:#010x}  (= Reboot entry PC)",
        elr_el2
    );
    kprintln!(
        "  SPSR_EL2 = {:#010x}  mode={} ({:#x})",
        spsr_el2, describe_aarch32_mode(mode), mode
    );
    kprintln!(
        "  R0 = {:#010x}  R1 = {:#010x}  R2 = {:#010x}  R3 = {:#010x}",
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
    );
    kprintln!(
        "  R12={:#010x}  R14_{}={:#010x}  (caller LR via Table D1-79)",
        ctx.x[12] as u32, describe_aarch32_mode(mode), caller_lr
    );

    // Iter-34: SPSR_EL2 mode=UND is the TRAMPOLINE'S mode (HVC
    // was issued from the UND-mode trampoline at 0xFFFF54), not
    // the original caller's. The trampoline saves the source CPSR
    // to UND_SAVE_SPSR_IPA. Read it to recover the actual caller
    // mode and banked LR.
    let trampoline_saved_spsr = crate::guest_endian::guest_read_u32_pa(UND_SAVE_SPSR_IPA).unwrap_or(0);
    let true_source_mode = (trampoline_saved_spsr & 0x1F) as u32;
    let true_caller_lr = crate::banked::lr_for_mode(ctx, trampoline_saved_spsr);
    kprintln!();
    kprintln!(
        "  TRUE source CPSR={:#010x} mode={} ({:#x})  TRUE caller LR={:#010x}",
        trampoline_saved_spsr,
        describe_aarch32_mode(true_source_mode),
        true_source_mode,
        true_caller_lr,
    );
    kprintln!(
        "  (LR_und=0x{:08x} above is the trampoline's bookkeeping, not the caller.)",
        ctx.x[22] as u32,
    );

    // The original-mode SP gives the call stack. For USR/SVC etc.
    // ctx.x[13] is the active SP slot per Table D1-79.
    let true_source_sp = crate::banked::sp_for_mode(ctx, trampoline_saved_spsr);
    kprintln!(
        "  TRUE source SP_{}={:#010x}",
        describe_aarch32_mode(true_source_mode),
        true_source_sp,
    );
    kprintln!();
    kprintln!("  TRUE source-mode stack (16 words from {:#010x}):", true_source_sp);
    for i in 0..16 {
        let va = true_source_sp.wrapping_add(i * 4);
        match crate::guest_endian::guest_read_u32_va(va) {
            Some(w) => kprintln!("    [{:+3}] @{:#010x} = {:#010x}", (i * 4) as i32, va, w),
            None    => kprintln!("    [{:+3}] @{:#010x} = (unmapped)", (i * 4) as i32, va),
        }
    }

    // Keep the SP_und dump (under the original `if mode == 0x1B`)
    // for backward compatibility — useful when the trampoline path
    // happens to push something there.
    if mode == 0x1B {
        let sp_und = ctx.x[23] as u32;
        kprintln!();
        kprintln!("  SP_und stack (16 words from ctx.x[23]={:#010x}):", sp_und);
        for i in 0..16 {
            let va = sp_und.wrapping_add(i * 4);
            match crate::guest_endian::guest_read_u32_va(va) {
                Some(w) => kprintln!("    [{:+3}] @{:#010x} = {:#010x}", (i * 4) as i32, va, w),
                None    => kprintln!("    [{:+3}] @{:#010x} = (unmapped)", (i * 4) as i32, va),
            }
        }
        // R0 typically points at a TException / TThrow descriptor
        // (per the existing handler comment). Dump 8 words at
        // *R0 if the address translates.
        let r0 = ctx.x[0] as u32;
        kprintln!();
        kprintln!("  Exception-descriptor candidate at R0={:#010x}:", r0);
        for i in 0..8 {
            let va = r0.wrapping_add(i * 4);
            match crate::guest_endian::guest_read_u32_va(va) {
                Some(w) => kprintln!("    [{:+3}] @{:#010x} = {:#010x}", (i * 4) as i32, va, w),
                None    => kprintln!("    [{:+3}] @{:#010x} = (unmapped)", (i * 4) as i32, va),
            }
        }
        // R3 is often the kErr_* code passed to Throw (signed int32).
        // Print both unsigned and signed interpretations so we can
        // match against kErr_- constants (typically -10000..-50000)
        // or positive object-table indices.
        let r3 = ctx.x[3] as u32;
        kprintln!();
        kprintln!(
            "  R3 decoded as error code: {:#010x} ({}, signed={})",
            r3, r3, r3 as i32
        );
    }

    kprintln!();
    kprintln!(
        "  (Preceding tracer entries show the caller chain. A typical trigger"
    );
    kprintln!(
        "   is UnhandledException → Reboot; in that case the exception name"
    );
    kprintln!(
        "   is at the r0 pointer passed to `Throw` a few entries earlier.)"
    );

    // 2026-04-26: dump kernel state to diagnose lazy-L1 wedge inside
    // TInterpreter::TInterpreter. The interesting fields:
    //   - L1[0xC0..0xD7]: which sections are coarse vs lazy 0x90 vs 0
    //   - gCurrentTask (= *(0x0c100ff8)): the running TTask*; used by
    //     DataAbortHandler at 0x393318 for the monitor lookup
    //   - That task's monitor list at +0x74,+0x78,+0x7c (read by
    //     `0x393324..0x393348` to build the dispatch bitmask)
    kprintln!();
    crate::alrt_capture::dump_counters();
    kprintln!();
    kprintln!("=== one-shot Reboot-canary kernel-state dump ===");
    {
        let ram = guest_mem::ram_host_pa() as *const u32;
        kprintln!("  L1 walk (sections 0xC0..0xD7):");
        for i in 0xC0..=0xD7 {
            // SAFETY: i < 4096; L1 lives at start of guest RAM.
            let e = unsafe { ram.add(i).read() };
            let kind = match e & 3 {
                0 => "fault",
                1 => "coarse",
                2 => "section",
                _ => "fine",
            };
            kprintln!("    L1[{:#x}] = {:#010x}  ({})", i, e, kind);
        }
        // gCurrentTask at 0x0c100ff8 → walk through stage-1 to find PA.
        let gct_va: u32 = 0x0c10_0ff8;
        if let Some(gct_pa) = guest_mem::translate_va(gct_va) {
            if let Some(gct) = crate::guest_endian::guest_read_u32_pa(gct_pa) {
                kprintln!(
                    "  gCurrentTask @VA={:#x} PA={:#x} = {:#010x}",
                    gct_va, gct_pa, gct
                );
                if let Some(gct_pa2) = guest_mem::translate_va(gct) {
                    let m74 = crate::guest_endian::guest_read_u32_pa(gct_pa2 + 0x74).unwrap_or(0xDEAD);
                    let m78 = crate::guest_endian::guest_read_u32_pa(gct_pa2 + 0x78).unwrap_or(0xDEAD);
                    let m7c = crate::guest_endian::guest_read_u32_pa(gct_pa2 + 0x7c).unwrap_or(0xDEAD);
                    kprintln!(
                        "  task @VA={:#x} PA={:#x} ->[0x74]={:#010x} ->[0x78]={:#010x} ->[0x7c]={:#010x}",
                        gct, gct_pa2, m74, m78, m7c
                    );
                    // Walk each non-null monitor's bitmask at +0x10
                    for (off, val) in [(0x74, m74), (0x78, m78), (0x7c, m7c)] {
                        if val != 0 {
                            if let Some(pa) = guest_mem::translate_va(val) {
                                let bm = crate::guest_endian::guest_read_u32_pa(pa + 0x10).unwrap_or(0xDEAD);
                                kprintln!(
                                    "    monitor[+{:#x}] @VA={:#x} PA={:#x} ->[0x10]={:#010x} (fault-bitmask)",
                                    off, val, pa, bm
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    // Step 3 (2026-04-28): full task census via task_dump::dump.
    // For the alrt-task wedge investigation, we need to see WHICH tasks
    // are RUN/RDY/BLK at wedge time and compare with Einstein
    // (NewtonProbe shows alrt as BLK; on us it's running CheckButton
    // with junk this — meaning we somehow scheduled it). The
    // census print covers run-queue traversal, all-objects walk, and
    // semaphore-wait state — enough to spot the divergence.
    kprintln!();
    kprintln!("=== Step 3: full task census ===");
    crate::task_dump::dump();
    kprintln!("=== end Step 3 dump ===");
    kprintln!("=== end Reboot-canary state dump ===");

    cpu::halt();
}

// ---- Remember post-SWI probe (surviving the iter-50..89 sweep) ----
//
// Re-establishes the kernel's `r8 = -10003` sentinel after the SWI
// return inside `TUDomainManager::Remember (static)`. The SWI dispatch
// in the host clobbers r8 in some paths; without this fixup the
// following `teq` at 0x00258E58 misbehaves and the kernel's monitor
// retry path doesn't engage. See `src/rom_patches.rs::apply_l1_cd_probes`.

fn handle_remember_swiret_probe(ctx: &mut TrapContext) {
    handle_remember_swiret_probe_with(ctx);
}

fn handle_remember_swiret_probe_with(ctx: &mut TrapContext) {
    // Emulate `mov r8, #237`. Together with the next ROM instruction
    // `sub r8, r8, #10240` this materialises r8 = -10003 (the kernel's
    // sentinel value loaded after the SWI return).
    ctx.x[8] = 237;
}

fn handle_dah_mrs_spsr_patch(ctx: &mut TrapContext) {
    let spsr_abt_save = crate::guest_endian::guest_read_u32_pa(
        guest_mem::DABT_SAVE_PA + 8,
    ).unwrap_or(0);
    let lr_abt_save = crate::guest_endian::guest_read_u32_pa(
        guest_mem::DABT_SAVE_PA,
    ).unwrap_or(0);
    let r1_in = ctx.x[1] as u32;
    let far = read_sysreg!("far_el1") as u32;
    // Cross-check: also read `mrs spsr_abt` from EL2. If it disagrees
    // with the saved slot, that's the documented QEMU staleness. We
    // always use the saved-slot value (architecturally correct on
    // every platform).
    let mrs_view = read_banked_spsr("abt") as u32;
    // Replace r1 with the trampoline-saved SPSR_abt. Natural ERET
    // resumes at the post-HVC PC (= 0x393148, the kernel's
    // `and r1, r1, #31`).
    ctx.x[1] = (ctx.x[1] & 0xFFFF_FFFF_0000_0000)
        | (spsr_abt_save as u64);
    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 16 {
        // lr_abt_save here is the original faulting PC + 8 (the slow
        // trampoline doesn't subtract; the kernel's `sub lr, lr, #8`
        // at DAH entry runs *after* the trampoline saves it). The
        // fast trampoline (iter-105) saves it at the same offset
        // pre-DAH-entry, so the value is `faulting_PC + 8` on both
        // paths.
        kprintln!(
            "DAH-mrs-patch[{}]: r1_in={:#010x} mrs={:#010x} saved-slot={:#010x} \
             (pre-abt mode={:#x} = {}) faulting_PC={:#010x} FAR={:#010x}{}",
            n, r1_in, mrs_view, spsr_abt_save, spsr_abt_save & 0x1F,
            describe_aarch32_mode(spsr_abt_save & 0x1F),
            lr_abt_save.wrapping_sub(8), far,
            if (mrs_view & 0x1F) != (spsr_abt_save & 0x1F) {
                "  *** MRS DIVERGES ***"
            } else { "" },
        );
    }
}

/// Handler for the patched `cmp r0, #0` at DAH PC `0x393984`,
/// immediately after `bl FaultMonitorEntry`. Logs `r0` (FaultMonitorEntry's
/// return value) plus the FAR (so we can correlate against the
/// dabt-forward log), then emulates the original `cmp r0, #0` by
/// updating SPSR_EL2's NZCV bits so the kernel's subsequent `beq
/// 0x393a30` at `0x393988` branches as if the cmp had run.
///
/// Useful for the layer-γ investigation: Einstein's abort #16 at
/// FAR=0x0CD07400 recovers (FaultMonitorEntry returns 0); ours doesn't
/// (returns non-zero). This probe pins the divergent return value
/// directly.
fn handle_dah_fme_ret_probe(_ctx: &mut TrapContext) {
    // r0 is in ctx.x[0] for AArch32 from EL2's view.
    let r0 = _ctx.x[0] as u32;
    let far = read_sysreg!("far_el1") as u32;
    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 24 {
        kprintln!(
            "DAH-FME-ret[{}]: r0={:#010x} far={:#010x}{}",
            n, r0, far,
            if r0 == 0 { "  (success → recovery)" } else { "  (failure → reboot)" },
        );
    }
    // Emulate `cmp r0, #0`:
    //   N = (r0 >> 31) & 1
    //   Z = (r0 == 0)
    //   C = 1  (cmp Rn, #0 = subs scratch, Rn, #0; no borrow → C=1)
    //   V = 0  (cmp with 0 cannot overflow)
    let n_bit = ((r0 >> 31) & 1) as u64;
    let z_bit = if r0 == 0 { 1u64 } else { 0u64 };
    let c_bit = 1u64;
    let v_bit = 0u64;
    let new_nzcv = (n_bit << 31) | (z_bit << 30) | (c_bit << 29) | (v_bit << 28);
    let mut spsr_el2: u64;
    // SAFETY: sysreg read/write, no side effects.
    unsafe {
        core::arch::asm!("mrs {}, spsr_el2", out(reg) spsr_el2,
            options(nomem, nostack, preserves_flags));
    }
    spsr_el2 = (spsr_el2 & !(0xF << 28)) | new_nzcv;
    unsafe {
        core::arch::asm!("msr spsr_el2, {}", in(reg) spsr_el2,
            options(nostack, preserves_flags));
        core::arch::asm!("isb", options(nostack, preserves_flags));
    }
}

/// Handler for the patched `mov ip, sp` at FaultMonitorEntry static
/// entry PC `0x0011FC60`. Logs r0 (input fault mask), the FAR, and
/// emulates the original `mov ip, sp` (writes ctx.x[12] = current SP
/// for the source AArch32 mode). Also reads `curr_task[+0x64]` (which
/// was set just before this call to scratch[4] from
/// GetDomainAndFaultMonitorFromDomainNumber) to test whether the
/// return value difference is in scratch[0] only or both scratch[0]
/// and scratch[4].
fn handle_fme_entry_probe(ctx: &mut TrapContext) {
    let r0 = ctx.x[0] as u32;
    let far = read_sysreg!("far_el1") as u32;
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    let src_mode = spsr_el2 & 0x1F;
    let sp_src = crate::banked::sp_for_mode(ctx, spsr_el2);
    let curr_task = crate::guest_endian::guest_read_u32_va(
        crate::rom_patches::G_CURRENT_TASK_VA,
    ).unwrap_or(0);
    let task_64 = if curr_task != 0 {
        crate::guest_endian::guest_read_u32_va(curr_task.wrapping_add(0x64)).unwrap_or(0)
    } else { 0 };
    let task_70 = if curr_task != 0 {
        crate::guest_endian::guest_read_u32_va(curr_task.wrapping_add(0x70)).unwrap_or(0)
    } else { 0 };
    let task_58 = if curr_task != 0 {
        crate::guest_endian::guest_read_u32_va(curr_task.wrapping_add(0x58)).unwrap_or(0)
    } else { 0 };
    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 24 {
        kprintln!(
            "FME-entry[{}]: r0(mask)={:#010x} far={:#010x} src_mode={:#x} sp={:#010x} task[+0x70]={:#010x} task[+0x64]={:#010x} task[+0x58]={:#010x}",
            n, r0, far, src_mode, sp_src, task_70, task_64, task_58,
        );
    }
    // Emulate `mov ip, sp`: ctx.x[12] = banked SP for source mode.
    ctx.x[12] = (ctx.x[12] & 0xFFFF_FFFF_0000_0000) | (sp_src as u64);
}

/// Handler for the patched `ldr r1, [pc, #1588]` at DAH PC `0x393318`
/// (just before the OR-chain that builds the fault mask). Logs the
/// curr_task pointer + its `[+0x74/+0x78/+0x7c]` TUDomainManager chain
/// + each monitor's `[+0x10]`, then emulates the original ldr by
/// writing the literal `0x0C100FF8` (the address of `gCurrentTask`)
/// into `ctx.x[1]`. ERET resumes at `0x39331c` (kernel's
/// `ldr r1, [r1]`).
fn handle_dah_or_chain_probe(ctx: &mut TrapContext) {
    let far = read_sysreg!("far_el1") as u32;
    let g_current_task_va = crate::rom_patches::G_CURRENT_TASK_VA;
    let curr_task = crate::guest_endian::guest_read_u32_va(g_current_task_va).unwrap_or(0);
    let m74 = if curr_task != 0 {
        crate::guest_endian::guest_read_u32_va(curr_task.wrapping_add(0x74)).unwrap_or(0)
    } else { 0 };
    let m78 = if curr_task != 0 {
        crate::guest_endian::guest_read_u32_va(curr_task.wrapping_add(0x78)).unwrap_or(0)
    } else { 0 };
    let m7c = if curr_task != 0 {
        crate::guest_endian::guest_read_u32_va(curr_task.wrapping_add(0x7c)).unwrap_or(0)
    } else { 0 };
    let m74_10 = if m74 != 0 {
        crate::guest_endian::guest_read_u32_va(m74.wrapping_add(0x10)).unwrap_or(0)
    } else { 0 };
    let m78_10 = if m78 != 0 {
        crate::guest_endian::guest_read_u32_va(m78.wrapping_add(0x10)).unwrap_or(0)
    } else { 0 };
    let m7c_10 = if m7c != 0 {
        crate::guest_endian::guest_read_u32_va(m7c.wrapping_add(0x10)).unwrap_or(0)
    } else { 0 };
    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 24 {
        kprintln!(
            "DAH-OR[{}]: far={:#010x} curr_task={:#010x} m74={:#010x}->{:#010x} m78={:#010x}->{:#010x} m7c={:#010x}->{:#010x}",
            n, far, curr_task, m74, m74_10, m78, m78_10, m7c, m7c_10,
        );
    }
    // Emulate `ldr r1, [pc, #1588]`: r1 = literal at (pc + 8 + 1588) =
    // 0x393954 = 0x0C100FF8 (the address of `gCurrentTask`). Write low
    // 32 bits of ctx.x[1].
    ctx.x[1] = (ctx.x[1] & 0xFFFF_FFFF_0000_0000) | (g_current_task_va as u64);
}

/// Trap-trace arming flag (iter-105). When set, every sync trap and
/// every IRQ entry forces a verbose log line — bypassing the
/// `TRAP_LOG_BUDGET` budget. Used to capture the full trap sequence
/// between an "incoming task is suspect" event (e.g. drvr being
/// scheduled in) and the wedge UND, so we can see exactly what
/// happens during the kernel's ERET to the user and any
/// immediately-following user-mode fault.
pub static TRAP_TRACE_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn trap_trace_armed() -> bool {
    TRAP_TRACE_ARMED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Task-switch save-area probe (iter-105). Fires on the kernel's
/// patched `add r0, r0, #16` at PC `0x003ad9a4`, which sits in the
/// task-restore epilog after `bl SwapInGlobals`. At this point r0 = r4
/// = the incoming `gCurrentTask` (the task whose registers and
/// `movs pc, lr` are about to run); we treat the previous fire's
/// incoming as the outgoing for this switch.
///
/// Each fire logs a one-line summary of the outgoing and incoming
/// task's `TTaskSavedContext` and flags any bit-0 in saved_pc or T-bit
/// in saved_spsr — Newton 2.x is pure ARM, so either is the iter-105
/// PC=0/T=1 corruption we're hunting. The handler then emulates
/// `add r0, r0, #16` so the natural ERET resumes the kernel at
/// 0x003ad9a8 unchanged.
///
/// Also: when the incoming task's name is "drvr", arm `TRAP_TRACE_ARMED`
/// so every subsequent sync trap and IRQ logs verbosely — pinning the
/// wedge sequence between drvr's first ERET and the PC=0/T=1 UND.
fn handle_task_switch_probe(ctx: &mut TrapContext) {
    // r4 holds the incoming task (mov r4, r0 at 0x3ad998, preserved
    // across `bl SwapInGlobals` per APCS).
    let incoming = ctx.x[4] as u32;

    // Track the previous-fire incoming as the outgoing for this switch.
    static PREV_INCOMING: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let outgoing = PREV_INCOMING.swap(
        incoming,
        core::sync::atomic::Ordering::Relaxed,
    );

    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    kprintln!("task-switch[{}]:", n);
    if outgoing != 0 {
        crate::task_dump::dump_task_save_oneline("out", outgoing);
    } else {
        kprintln!("  task[out] (first switch — no prior incoming)");
    }
    crate::task_dump::dump_task_save_oneline("in ", incoming);

    // Arm verbose trap tracing when the incoming task is named "drvr".
    // Newton's TT_GLOBALS sits at task_va + 0xa0 and points just past
    // the per-task globals struct; `find_task_name` scans the 128
    // bytes below it for a printable fourcc.
    if !trap_trace_armed() {
        let glob = crate::guest_endian::guest_read_u32_va(incoming.wrapping_add(0xa0))
            .unwrap_or(0);
        if let Some((_, v)) = crate::task_dump::find_task_name(glob) {
            let bytes = [
                (v >> 24) as u8,
                (v >> 16) as u8,
                (v >> 8)  as u8,
                v          as u8,
            ];
            if &bytes == b"drvr" {
                TRAP_TRACE_ARMED.store(true, core::sync::atomic::Ordering::Relaxed);
                kprintln!(
                    "*** trap-trace ARMED: drvr task incoming (task={:#010x}) ***",
                    incoming,
                );
            }
        }
    }

    // Emulate `add r0, r0, #16`. Preserve high 32 bits of x0 the same
    // way other AArch32-emulation handlers in this file do.
    ctx.x[0] = (ctx.x[0] & 0xFFFF_FFFF_0000_0000)
        | ((ctx.x[0] as u32).wrapping_add(0x10) as u64);
}

/// Pre-ERET probe (iter-105). Fires on the patched `pop {r0, r1, r2}`
/// at 0x003ada68 — the very last instruction before the kernel's
/// `movs pc, lr` at 0x003ada6c. At this point lr_svc holds the saved_pc
/// the kernel will jump to and SPSR_EL1 (= AArch32 SPSR_svc) holds the
/// new CPSR for the user. Logs both, plus the popped r0/r1/r2 (which
/// become the user's r0..r2 on ERET). Then emulates the pop so natural
/// ERET continues into the `movs pc, lr` at PC+4 unchanged.
fn handle_task_switch_pre_eret_probe(ctx: &mut TrapContext) {
    // SVC banked regs per Table D1-79: ctx.x[18] = LR_svc, x[19] = SP_svc.
    let lr_svc = ctx.x[18] as u32;
    let sp_svc = ctx.x[19] as u32;
    let spsr_svc = read_sysreg!("spsr_el1") as u32;

    // Read the 3 words about to be popped from the SVC stack via the
    // guest stage-1 walker — same path the diagnostic dumps use, so it
    // resolves SVC mappings even though we're at EL2.
    let read_va = |va: u32| -> u32 {
        match crate::guest_mem::translate_va(va) {
            Some(pa) => crate::guest_endian::guest_read_u32_pa(pa).unwrap_or(0xDEAD_BEEF),
            None => 0xDEAD_BEEF,
        }
    };
    let p0 = read_va(sp_svc);
    let p1 = read_va(sp_svc.wrapping_add(4));
    let p2 = read_va(sp_svc.wrapping_add(8));

    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // The bug we're chasing produces lr_svc with bit-0 set or spsr_svc
    // with the T-bit set; flag those loudly. The routine fires once per
    // task switch (~826 times before the wedge), so the flag is what
    // makes the smoking gun easy to spot in the log tail.
    let suspicious = (lr_svc & 1) != 0 || (spsr_svc & 0x20) != 0;
    kprintln!(
        "task-eret[{}]: lr_svc={:#010x} spsr_svc={:#010x} sp_svc={:#010x} pop=[{:#010x}, {:#010x}, {:#010x}]{}",
        n, lr_svc, spsr_svc, sp_svc, p0, p1, p2,
        if suspicious { "  *** CORRUPT (T-bit or bit-0) ***" } else { "" },
    );

    // Emulate `pop {r0, r1, r2}` (= `ldmfd sp!, {r0, r1, r2}`):
    //   r0 := [sp_svc + 0]
    //   r1 := [sp_svc + 4]
    //   r2 := [sp_svc + 8]
    //   sp_svc += 12
    ctx.x[0]  = (ctx.x[0] & 0xFFFF_FFFF_0000_0000)  | (p0 as u64);
    ctx.x[1]  = (ctx.x[1] & 0xFFFF_FFFF_0000_0000)  | (p1 as u64);
    ctx.x[2]  = (ctx.x[2] & 0xFFFF_FFFF_0000_0000)  | (p2 as u64);
    ctx.x[19] = (ctx.x[19] & 0xFFFF_FFFF_0000_0000) | (sp_svc.wrapping_add(12) as u64);

    // When the trap-trace is armed (drvr is incoming), also dump the
    // 4 instructions starting at the ERET target. If the bytes at
    // saved_pc don't match the disasm, that pins the bug to a runtime
    // patcher; if they match, the bug is in the ERET / fault path.
    if trap_trace_armed() {
        let target = lr_svc;
        let read_target = |va: u32| -> u32 {
            match crate::guest_mem::translate_va(va) {
                Some(pa) => crate::guest_endian::guest_read_u32_pa(pa).unwrap_or(0xDEAD_BEEF),
                None => 0xDEAD_BEEF,
            }
        };
        let w0 = read_target(target);
        let w1 = read_target(target.wrapping_add(4));
        let w2 = read_target(target.wrapping_add(8));
        let w3 = read_target(target.wrapping_add(12));
        kprintln!(
            "  insn @ saved_pc {:#010x} = {:08x} {:08x} {:08x} {:08x}",
            target, w0, w1, w2, w3,
        );
    }
}

/// Emulate the kernel's `movs pc, lr` at 0x003ada6c. We replaced the
/// native instruction with HVC; this handler performs the AArch32 EL1
/// exception-return semantics from EL2:
///
///   new_cpsr ← SPSR_svc           (= SPSR_EL1 from EL2's view)
///   new_pc   ← lr_svc             (= ctx.x[18])
///   coerce M[4]=1 if the kernel wrote a USR-26 encoding (M[4:0]=0)
///   ELR_EL2  ← new_pc
///   SPSR_EL2 ← new_cpsr           (with the coercion applied)
///
/// The natural HVC ERET in `trap_sync_lower_aarch32`'s assembly stub
/// then drops to AArch32 EL0 at new_pc with new_cpsr, just like the
/// native `movs pc, lr` would.
fn handle_task_switch_eret_emulator(ctx: &mut TrapContext) {
    let lr_svc = ctx.x[18] as u32;
    let mut spsr_svc = read_sysreg!("spsr_el1") as u32;

    // ARMv4 USR-26 → ARMv7 USR-32 coercion: real ARMv8 hardware
    // coerces M[4]=0 to USR-32 on AArch32 ERET, but the SPSR_EL2 path
    // strictly follows AArch64 rules where M[4]=0 means AArch64 EL0.
    // Force M[4]=1 with M[3:0] forced to USR=0x0 when the kernel
    // installed a 26-bit-style mode field.
    if (spsr_svc & 0x10) == 0 {
        spsr_svc = (spsr_svc & !0x1F) | 0x10;
    }

    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if trap_trace_armed() || n < 8 {
        kprintln!(
            "task-eret-emu[{}]: lr_svc={:#010x} spsr_svc={:#010x} -> ELR_EL2/SPSR_EL2",
            n, lr_svc, spsr_svc,
        );
    }

    // SAFETY: ELR_EL2 / SPSR_EL2 control the next ERET; setting them
    // here is the whole point of the handler. The natural ERET in the
    // sync-trap dispatcher will consume these values.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {pc}",
            "msr spsr_el2, {sp}",
            pc = in(reg) lr_svc as u64,
            sp = in(reg) spsr_svc as u64,
            options(nomem, nostack, preserves_flags),
        );
    }
}

fn handle_diag(ctx: &mut TrapContext) {
    let far = read_sysreg!("far_el1");
    let spsr_el2 = read_sysreg!("spsr_el2");
    let elr_el2 = read_sysreg!("elr_el2");

    // Fast path: DABT-vector trampoline → kernel DataAbortHandler.
    // For DABTs whose DFSC matches a translation / permission / access-flag
    // fault, forward to the kernel's DataAbortHandler at VA 0x0039_3114
    // (the original target of the ROM's VA 0x10 branch before we patched
    // it with our diagnostic trampoline). Lets the kernel handle routine
    // faults like stack-collision growth (USR `STMFD sp!, {…}` prologue
    // crossing into an unmapped page → TStackManager::CopyPagesAfterStack-
    // Collided) without the hypervisor needing to model on-demand paging.
    //
    // Alignment faults (DFSC=0x01) take a separate HVC #ALIGN_TAG path
    // in the DABT trampoline and never reach here. Source-mode UND (the
    // guest_bp BP path) and PABT-source (the PABT-vector intercept at
    // VA 0x0C) fall through to the loud halt below — that's the Phase-B
    // trip-wire behaviour we need for those classes.
    //
    // DFSC values (short-descriptor, ARMv7 DFSR.FS[3:0]):
    //   0x05 = translation, section (L1 entry fault)
    //   0x07 = translation, page    (L2 entry fault)
    //   0x0D = permission, section
    //   0x0F = permission, page
    //   0x03 = access flag, section
    //   0x06 = access flag, page
    //
    // R0 and R1 were clobbered by the DABT trampoline (which stashed
    // them in TPIDRURW / TPIDRRO and then loaded DFSR / SPSR_abt into
    // them); restore from those scratch slots so the kernel's handler
    // sees the pre-abort register state. Other regs and banked
    // LR_abt / SP_abt / SPSR_abt are already in their post-DABT-entry
    // values (the trampoline reads them but does not modify them).
    let hvc_src_mode = (spsr_el2 as u32) & 0x1F;
    if hvc_src_mode == crate::banked::MODE_ABT {
        let esr_el1 = read_sysreg!("esr_el1");
        let dfsc = (esr_el1 & 0x3F) as u32;
        // Iter-54: DFSC=0x01 is alignment fault. The DABT trampoline's
        // legacy `mrc p15,0,Rt,c5,c0,0` ought to route alignment faults
        // to ALIGN_TAG via the BEQ — but in practice we've seen at least
        // one site (the LDR-with-imm-offset rotate idiom in DrText at
        // 0x0035c554) where the trampoline still falls through to
        // DIAG_TAG. Cross-check ESR_EL1 here and dispatch to the
        // alignment emulator unconditionally, instead of dumping and
        // halting on a fault the hypervisor already knows how to handle.
        if dfsc == 0x01 {
            crate::unaligned::handle_align_fault(ctx);
            return;
        }
        let forwardable = matches!(dfsc, 0x03 | 0x05 | 0x06 | 0x07 | 0x0D | 0x0F);
        if forwardable {
            // ARMv7 leaves DFSR.Domain UNK for DFSC=5 (translation,
            // section) — see ARMv7 ARM B4.1.51. The 717006 kernel was
            // written for StrongARM, where the equivalent register
            // (CP15 c5,c5,0) always carried the L1 entry's domain
            // regardless of fault status. Our hypervisor rewrites
            // the kernel's `mrc c5,c5,0` to `mrc c5,c0,0` (= DFSR_EL1)
            // at ROM-load time (see guest_mem::patch_cp15_encodings),
            // so the kernel's later DAH read picks up whatever ARMv7
            // hardware put in DFSR.Domain — which is 0 for DFSC=5.
            // The kernel then computes domain := 0 and asks
            // `GetDomainAndFaultMonitorFromDomainNumber(0)`, which has
            // no monitor → returns scratch[0]=0 →
            // `FaultMonitorEntry(r0=0)` → -10015 → reboot.
            // Empirical wedge: qemu13.log fault #2 shows
            // task[+0x58]=0x05 (DFSR=0x05, domain=0) where every other
            // recovered abort had task[+0x58]=0x47 (DFSR=0x47,
            // domain=4).
            //
            // Fix: synthesise the StrongARM-style domain field by
            // reading the L1 entry for the FAR's section and writing
            // its bits[8:5] into DFSR_EL1.bits[7:4]. Idempotent for
            // valid-domain DFSCs (the bits already match).
            if dfsc == 0x05 || dfsc == 0x07 || dfsc == 0x0D || dfsc == 0x0F {
                let l1_pa = 0x0400_0000u32 + ((far as u32) >> 20) * 4;
                let l1 = crate::guest_endian::guest_read_u32_pa(l1_pa).unwrap_or(0);
                let l1_domain = (l1 >> 5) & 0xF;
                let mut dfsr_el1: u64;
                // SAFETY: sysreg read of DFSR_EL1 (= ESR_EL1's AArch32
                // alias for data aborts when EL1 is AArch32). On
                // Cortex-A53 in our config, DFSR_EL1 == ESR_EL1 for
                // AArch32 EL1 abort entries, so update both via
                // ESR_EL1.
                unsafe {
                    core::arch::asm!("mrs {}, esr_el1", out(reg) dfsr_el1,
                        options(nomem, nostack, preserves_flags));
                }
                dfsr_el1 = (dfsr_el1 & !(0xF << 4)) | ((l1_domain as u64) << 4);
                unsafe {
                    core::arch::asm!("msr esr_el1, {}", in(reg) dfsr_el1,
                        options(nostack, preserves_flags));
                    core::arch::asm!("isb", options(nostack, preserves_flags));
                }
            }
            log_dabt_forward(dfsc, far as u32, hvc_src_mode, ctx);
            // One-shot diagnostic: when the recursive-abort "newt" DABT
            // fires (FAR=0x6e657774, mode=ABT), dump the SWIBoot save
            // area of every cdsv-named task before forwarding to the
            // kernel handler — the kernel's own response is to reboot,
            // so this is our only chance to see the corrupt slot.
            if far as u32 == 0x6e65_7774 {
                static FIRED: core::sync::atomic::AtomicBool =
                    core::sync::atomic::AtomicBool::new(false);
                if !FIRED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                    kprintln!("=== one-shot newt-DABT diagnostic: cdsv save areas ===");
                    crate::task_dump::dump_save_area_for_named(b"cdsv");
                    kprintln!("=== one-shot newt-DABT diagnostic: full kernel dump ===");
                    crate::task_dump::dump_full();
                    kprintln!("=== end one-shot newt-DABT diagnostic ===");
                }
            }
            let saved_r0: u64;
            let saved_r1: u64;
            unsafe {
                core::arch::asm!(
                    "mrs {}, tpidr_el0",
                    out(reg) saved_r0,
                    options(nomem, nostack, preserves_flags),
                );
                core::arch::asm!(
                    "mrs {}, tpidrro_el0",
                    out(reg) saved_r1,
                    options(nomem, nostack, preserves_flags),
                );
            }
            ctx.x[0] = saved_r0;
            ctx.x[1] = saved_r1;
            const DATA_ABORT_HANDLER_VA: u32 = 0x0039_3114;
            unsafe {
                core::arch::asm!(
                    "msr elr_el2, {elr}",
                    "isb",
                    elr = in(reg) DATA_ABORT_HANDLER_VA as u64,
                    options(nostack, preserves_flags),
                );
            }
            return;
        }
    }

    // Banked SPSRs are AArch64-named sysregs (FVP and QEMU both honour
    // them). For SPSR_svc, the architecturally-mapped AArch64 view is
    // SPSR_EL1 (DDI 0487 D13.2 — SPSR_EL1 bits[31:0] are mapped to
    // AArch32 SPSR_svc).
    let spsr_svc = read_sysreg!("spsr_el1");
    let spsr_abt = read_banked_spsr("abt");
    let spsr_und = read_banked_spsr("und");
    let spsr_irq = read_banked_spsr("irq");
    let spsr_fiq = read_banked_spsr("fiq");

    // HVC-source mode: whichever AArch32 mode was active when HVC fired
    // (ABT for the PABT-vector intercept, UND for guest_bp). The
    // "pre-abort" / "pre-fault" mode is named by the matching banked
    // SPSR (SPSR_abt for ABT-source, SPSR_und for UND-source).
    // (`hvc_src_mode` was already computed above for the fast-path
    // gate; reused here for the diagnostic dump.)
    let mode_name = describe_aarch32_mode(hvc_src_mode);

    kprintln!();
    kprintln!("*** DIAG vector intercept (HVC #DIAG_TAG from mode {}) ***", mode_name);
    kprintln!("  ELR_EL2   = {:#010x}  (PC of insn after HVC)", elr_el2);
    kprintln!("  SPSR_EL2  = {:#010x}  (CPSR at HVC entry)", spsr_el2);
    kprintln!("  FAR_EL1   = {:#010x}  (most-recent EL1 faulting VA)", far);
    kprintln!(
        "  SPSR_svc  = {:#010x}  SPSR_abt = {:#010x}  SPSR_und = {:#010x}  SPSR_irq = {:#010x}  SPSR_fiq = {:#010x}",
        spsr_svc, spsr_abt, spsr_und, spsr_irq, spsr_fiq
    );
    let esr = read_sysreg!("esr_el2");
    let esr_el1 = read_sysreg!("esr_el1");
    let sctlr = read_sysreg!("sctlr_el1");
    let ttbr0 = read_sysreg!("ttbr0_el1");
    let ttbr1 = read_sysreg!("ttbr1_el1");
    let tcr   = read_sysreg!("tcr_el1");
    kprintln!(
        "  ESR_EL2   = {:#010x}  EC={:#x} ISS={:#x}",
        esr, (esr >> 26) & 0x3F, esr & 0x1FFFFFF
    );
    // ESR_EL1 holds the EL1 fault syndrome the CPU wrote when the
    // guest took its own DABT. For AArch32 DABT, EC=0x24 with
    // ISS[5:0] = DFSC (fault class).
    kprintln!(
        "  ESR_EL1   = {:#010x}  EC={:#x} ISS={:#x}  DFSC={:#x}",
        esr_el1, (esr_el1 >> 26) & 0x3F, esr_el1 & 0x1FFFFFF, esr_el1 & 0x3F
    );
    kprintln!(
        "  SCTLR_EL1 = {:#010x}  (M={}, C={}, I={}, V={})",
        sctlr, sctlr & 1, (sctlr >> 2) & 1, (sctlr >> 12) & 1, (sctlr >> 13) & 1
    );
    kprintln!(
        "  TTBR0_EL1 = {:#010x}  TTBR1_EL1 = {:#010x}  TCR_EL1 = {:#010x}",
        ttbr0, ttbr1, tcr
    );

    // Banked SP/LR via the X-register mapping (DDI 0487 D1.21.1
    // Table D1-79). Truncated to u32 because Table D1-85 says the
    // upper 32 bits of x16..x30 on AArch32→AArch64 entry are
    // CONSTRAINED UNPREDICTABLE.
    let sp_usr = ctx.x[13] as u32;
    let lr_usr = ctx.x[14] as u32;
    let lr_irq = ctx.x[16] as u32;
    let sp_irq = ctx.x[17] as u32;
    let lr_svc = ctx.x[18] as u32;
    let sp_svc = ctx.x[19] as u32;
    let lr_abt = ctx.x[20] as u32;
    let sp_abt = ctx.x[21] as u32;
    let lr_und = ctx.x[22] as u32;
    let sp_und = ctx.x[23] as u32;
    kprintln!(
        "  banked SP/LR (Table D1-79):  USR sp={:#010x} lr={:#010x}",
        sp_usr, lr_usr
    );
    kprintln!(
        "                               SVC sp={:#010x} lr={:#010x}",
        sp_svc, lr_svc
    );
    kprintln!(
        "                               ABT sp={:#010x} lr={:#010x}   IRQ sp={:#010x} lr={:#010x}",
        sp_abt, lr_abt, sp_irq, lr_irq
    );
    kprintln!(
        "                               UND sp={:#010x} lr={:#010x}",
        sp_und, lr_und
    );
    kprintln!("  guest regs r0..r14 (R8..R12 are USR-bank for non-FIQ source modes):");
    for chunk in 0..3 {
        let base = chunk * 5;
        kprintln!(
            "    r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x}",
            base, ctx.x[base] as u32,
            base+1, ctx.x[base+1] as u32,
            base+2, ctx.x[base+2] as u32,
            base+3, ctx.x[base+3] as u32,
            base+4, ctx.x[base+4] as u32,
        );
    }

    // Pick the source mode's LR/SP. For HVC-from-ABT (the PABT-vector
    // intercept), the pre-abort mode is named by SPSR_abt and the
    // banked LR/SP for that mode comes from its X-register slot.
    // For HVC-from-UND (guest_bp), use SPSR_und.
    let (spsr_src, lr_src) = match hvc_src_mode {
        crate::banked::MODE_UND => (spsr_und as u32, lr_und),
        crate::banked::MODE_ABT => (spsr_abt as u32, lr_abt),
        _ => (spsr_el2 as u32, ctx.x[14] as u32),
    };
    let pre_mode = spsr_src & 0x1F;
    let pre_lr = crate::banked::lr_for_mode(ctx, spsr_src);
    let pre_sp = crate::banked::sp_for_mode(ctx, spsr_src);
    let thumb = (spsr_src & (1 << 5)) != 0;
    // Faulting PC adjustment: ARM DABT = LR-8, ARM PABT = LR-4,
    // Thumb DABT = LR-4, Thumb PABT = LR-2. The DIAG path only fires
    // from PABT today (line 801 of guest_mem.rs patches PABT vector),
    // so use the PABT formula (LR-4 ARM, LR-2 Thumb).
    let faulting_pc = if thumb { lr_src.wrapping_sub(2) & !1 } else { lr_src.wrapping_sub(4) };
    let insn = crate::guest_endian::guest_read_u32_pa(faulting_pc & !3).unwrap_or(0xDEAD_BEEF);
    kprintln!(
        "  HVC source mode = {:#x} ({}); pre-fault mode (from SPSR_<src>) = {:#x} ({}), T={}",
        hvc_src_mode, mode_name,
        pre_mode, describe_aarch32_mode(pre_mode), thumb as u32
    );
    kprintln!(
        "  pre-fault SP={:#010x} LR={:#010x}  -> faulting PC {:#010x}  insn={:#010x}",
        pre_sp, pre_lr, faulting_pc, insn
    );

    // The DABT trampoline at DABT_TRAMP_OFFSET still records LR_abt /
    // SP_abt / SPSR_abt to a fixed PA slot for the alignment-fault
    // fast path. Print those too so any divergence between the
    // X-register view and the trampoline-stash view is visible at a
    // glance.
    let lr_abt_save = crate::guest_endian::guest_read_u32_pa(guest_mem::DABT_SAVE_PA).unwrap_or(0);
    let sp_abt_save = crate::guest_endian::guest_read_u32_pa(guest_mem::DABT_SAVE_PA + 4).unwrap_or(0);
    let spsr_abt_save = crate::guest_endian::guest_read_u32_pa(guest_mem::DABT_SAVE_PA + 8).unwrap_or(0);
    kprintln!(
        "  DABT-trampoline stash (cross-check):  LR_abt={:#010x} SP_abt={:#010x} SPSR_abt={:#010x}",
        lr_abt_save, sp_abt_save, spsr_abt_save
    );

    guest_mem::dump_stage1_walk(far as u32);
    // Also walk a handful of VAs that are relevant to Newton boot —
    // SVC stack, ABT stack target, REx window start, RAM base — so we
    // can tell at a glance whether the kernel's L1 table has the
    // expected mappings in place at the time of the abort.
    for va in [0x04004400u32, 0x0C004C00, 0x01000000, 0x04000000, 0x00800000,
               0x02A00000, 0x02A04000, 0x02A04AA4, 0x00FFFF00,
               0x0008EA8C, 0x0008EB00, 0x0008EB08,
               0x0100018B, 0x01000180, 0x01000190, 0x01000193,
               0x01A00000, 0x01A00004,
               0x0C100000, 0x0C100800, 0x0C104000] {
        guest_mem::dump_stage1_walk(va);
    }

    // Symbolic stack trace from SP_svc. lr_svc is the return address
    // of whoever is currently executing in SVC — i.e. the BL that led
    // us here. From SP_svc, scan upward looking for plausible saved
    // return addresses (point into ROM, aligned, and preceded by a
    // BL/BLX). Cheap substitute for an fp-chain walk when fp=0 (which
    // BootOS deliberately sets at 0x187d4).
    kprintln!("  symbolic stack trace (SVC):");
    kprintln!(
        "    #0  {:#010x}  ({})   <- faulting PC",
        faulting_pc, if thumb { "Thumb" } else { "ARM" }
    );
    kprintln!(
        "    #1  {:#010x}  ARM    <- LR_svc (caller of faulting fn)",
        lr_svc & !1
    );
    let mut frame = 2usize;
    for i in 0..64u32 {
        let va = sp_svc.wrapping_add(i * 4);
        let pa_opt = guest_translate_va(va);
        if pa_opt.is_none() { continue; }
        let pa = pa_opt.unwrap();
        let w = match crate::guest_endian::guest_read_u32_pa(pa) {
            Some(x) => x, None => continue,
        };
        let tgt = w & !1;
        if tgt == 0 || tgt >= 0x0100_0000 { continue; }
        if tgt & 3 != 0 { continue; }
        if let Some(prev) = crate::guest_endian::guest_read_u32_pa(tgt.wrapping_sub(4)) {
            let is_bl = ((prev >> 24) & 0xF) == 0xB;       // BL (unconditional)
            let is_blx_imm = ((prev >> 25) & 0x7F) == 0x7D; // BLX imm (v5+)
            if is_bl || is_blx_imm {
                kprintln!(
                    "    #{}  {:#010x}  (called via {:#010x} @ {:#x})",
                    frame, tgt, prev, tgt - 4
                );
                frame += 1;
                if frame >= 8 { break; }
            }
        }
    }
    kprintln!("  (end of trace — cross-reference PCs against _Data_/symbols.txt)");
    cpu::halt();
}

/// Translate a guest VA to its guest PA via the current stage-1
/// tables. Returns None on a fault (unmapped / wrong descriptor type).
/// Uses the same logic as `guest_mem::dump_stage1_walk` but returns
/// the PA instead of printing.
pub fn guest_translate_va(va: u32) -> Option<u32> {
    // Assume TTBR0 = 0x04000000 (per probe findings) and walk the
    // short-descriptor tables via guest_mem's PA accessors.
    let l1_idx = (va >> 20) as usize;
    let l1_entry = crate::guest_endian::guest_read_u32_pa(0x0400_0000 + (l1_idx as u32) * 4)?;
    let ty = l1_entry & 3;
    match ty {
        2 => Some((l1_entry & 0xFFF0_0000) | (va & 0x000F_FFFF)),
        1 => {
            let l2_pa = l1_entry & 0xFFFF_FC00;
            let l2_idx = (va >> 12) & 0xFF;
            let l2_entry = crate::guest_endian::guest_read_u32_pa(l2_pa + l2_idx * 4)?;
            match l2_entry & 3 {
                1 => Some((l2_entry & 0xFFFF_0000) | (va & 0x0000_FFFF)),
                2 | 3 => Some((l2_entry & 0xFFFF_F000) | (va & 0x0000_0FFF)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn read_banked_spsr(which: &'static str) -> u64 {
    // SAFETY: these are defined AArch64 system registers at EL2.
    unsafe {
        let v: u64;
        match which {
            "abt" => core::arch::asm!("mrs {}, spsr_abt", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            "und" => core::arch::asm!("mrs {}, spsr_und", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            "irq" => core::arch::asm!("mrs {}, spsr_irq", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            "fiq" => core::arch::asm!("mrs {}, spsr_fiq", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            _ => { v = 0; }
        }
        v
    }
}

fn describe_aarch32_mode(mode: u32) -> &'static str {
    match mode & 0x1F {
        0x10 => "USR",
        0x11 => "FIQ",
        0x12 => "IRQ",
        0x13 => "SVC",
        0x16 => "MON",
        0x17 => "ABT",
        0x1A => "HYP",
        0x1B => "UND",
        0x1F => "SYS",
        _    => "?",
    }
}

fn is_swp_encoding(insn: u32) -> bool {
    // ARMv7 A8.8.229: SWP  cond 0001_0000 Rn Rd SBZ 1001 Rm  (word)
    //                 SWPB cond 0001_0100 Rn Rd SBZ 1001 Rm  (byte)
    // Mask zeros cond (bits 31:28), Rn (19:16), Rd (15:12), SBZ (11:8),
    // Rm (3:0). Leaves bits [27:20] + [7:4] for the opcode check.
    (insn & 0x0FB0_0FF0) == 0x0100_0090
}

/// Emulate a SWP / SWPB instruction. The CPU took UND on A53 (SCTLR.SW
/// = 0 by default on ARMv8). AArch32 R0..R12 are non-banked for the
/// USR/SYS/SVC/ABT/UND/IRQ modes the Newton kernel actually uses and
/// map directly to ctx.x[0..12], so we can read/write the operand regs
/// through the saved context.
fn emulate_swp(ctx: &mut TrapContext, insn: u32, faulting_pc: u32) {
    let is_byte = (insn & 0x0040_0000) != 0;
    let rn = ((insn >> 16) & 0xF) as usize;
    let rd = ((insn >> 12) & 0xF) as usize;
    let rm = (insn & 0xF) as usize;

    // FIQ-mode and banked-SP/LR operands would need the banked-register
    // machinery. The Newton kernel's one SWP site (probe/FINDINGS.md
    // §16.5) uses low regs, and our tests stay below r13.
    if rn >= 13 || rd >= 13 || rm >= 13 {
        kprintln!(
            "*** SWP with banked reg operand: insn={:#010x} PC={:#x} Rn=r{} Rd=r{} Rm=r{}",
            insn, faulting_pc, rn, rd, rm
        );
        cpu::halt();
    }

    let va = ctx.x[rn] as u32;
    let new_value = ctx.x[rm] as u32;

    // The SWP target is a VA when the guest stage-1 MMU is on — the only
    // in-ROM SWP site is `Swap` at PC 0x3ae204, reached from kernel code
    // that hands us user/kernel VAs (e.g. 0x0c1xxxxx, which stage-1
    // remaps into RAM per TMemoryConsts). Pre-MMU it's identity and we
    // can feed `va` straight through.
    let addr = match resolve_guest_pa(va) {
        Some(pa) => pa,
        None => {
            kprintln!(
                "*** SWP{} [r{}]={:#x} — stage-1 translation failed at PC={:#x}",
                if is_byte { "B" } else { "" }, rn, va, faulting_pc
            );
            cpu::halt();
        }
    };

    if is_byte {
        let old = match read_guest_byte_pa(addr) {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** SWPB [r{}]={:#x} (PA={:#x}) — address not readable",
                    rn, va, addr
                );
                cpu::halt();
            }
        };
        if !write_guest_byte_pa(addr, new_value as u8) {
            kprintln!(
                "*** SWPB [r{}]={:#x} (PA={:#x}) — address not writable",
                rn, va, addr
            );
            cpu::halt();
        }
        ctx.x[rd] = old as u64;
    } else {
        if addr & 3 != 0 {
            kprintln!(
                "*** SWP with unaligned address r{}={:#x} (ignored, guest may fault)",
                rn, va
            );
        }
        let old = match read_guest_word_pa(addr) {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** SWP [r{}]={:#x} (PA={:#x}) — address not readable",
                    rn, va, addr
                );
                cpu::halt();
            }
        };
        if !write_guest_word_pa(addr, new_value) {
            kprintln!(
                "*** SWP [r{}]={:#x} (PA={:#x}) — address not writable",
                rn, va, addr
            );
            cpu::halt();
        }
        ctx.x[rd] = old as u64;
    }

    log_swp_budgeted(faulting_pc, is_byte, rn, rd, rm, addr);
}

/// Resolve a guest address as seen by an AArch32 load/store instruction
/// into a guest PA. Identity when the stage-1 MMU is off (SCTLR_EL1.M=0);
/// stage-1 walk otherwise. Returns `None` only when the MMU is on and
/// the VA is unmapped.
fn resolve_guest_pa(addr: u32) -> Option<u32> {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, sctlr_el1",
            out(reg) sctlr,
            options(nomem, nostack, preserves_flags),
        );
    }
    if sctlr & 1 == 0 {
        Some(addr)
    } else {
        guest_mem::translate_va(addr)
    }
}

/// UND-path return. Must NOT use `return_to_guest` — that calls
/// `msr spsr_el2, <val>`, which on QEMU raspi3b has a documented side
/// effect: it clobbers SPSR_EL1 (= AArch32 SPSR_svc) with the value
/// being written. Since the UND trampoline HVCs from UND mode, `<val>`
/// is the pre-UND CPSR (e.g. 0x1D3 for SVC mode); that pollutes the
/// guest's live SPSR_svc from USR → SVC, and the kernel's subsequent
/// `movs pc, lr` at SWIBoot's epilog stays in SVC instead of returning
/// to USR. Stalls Phase B at DFAR=0x0c001000 in SVC on `pop {r4, r5}`
/// at PC 0x3ae3ec.
///
/// Workaround (suggested by the verification agent on 2026-04-23):
/// don't write SPSR_EL2 at all. Instead, ERET into a `ldr lr, [pc,
/// #0]; movs pc, lr` stub at `UND_RETURN_STUB_VA`. SPSR_EL2 stays as
/// the CPU's auto-saved value from HVC entry (= UND, mode 0x1B), so
/// the ERET lands in UND mode. The stub loads the target PC from a
/// post-LDR literal we write to the ROM backing, then `movs pc, lr`
/// architecturally — the CPU copies SPSR_und (still the pre-UND
/// CPSR, preserved since UND entry) into CPSR, and R14_und into PC.
/// No `msr spsr_el2`, no SPSR_EL1 side-effect.
pub(crate) fn return_to_guest_from_und(_ctx: &mut TrapContext, elr: u64, _spsr: u64) {
    // iter-87 diag: catch the case where we're about to ERET to USR
    // mode at a PC inside our own trampoline window. That's never
    // legitimate; the only trampoline-internal ERET target is the
    // UND_RETURN_STUB which lives outside this range.
    // iter-87 diag: only flag ERET to the trampoline body proper —
    // ranges 0xffff00..0xffff60 (UND_TRAMP) and 0xffec0..0xffefc
    // (FPA bypass). SBA_POST_TRAMP at 0xffff80 and UND_RETURN_STUB
    // at 0xffffe4 are legitimate ERET targets.
    let mode = (_spsr as u32) & 0x1F;
    let elr32 = elr as u32;
    let in_und_tramp = elr32 >= 0x00FF_FF00 && elr32 < 0x00FF_FF60;
    let in_fpa_bypass = elr32 >= guest_mem::FPA_BYPASS_STUB_OFFSET as u32
        && elr32 < (guest_mem::FPA_BYPASS_STUB_OFFSET as u32 + 0x40);
    if mode == 0x10 && (in_und_tramp || in_fpa_bypass) {
        kprintln!(
            "*** return_to_guest_from_und: USR target inside trampoline body! \
             elr={:#x} spsr={:#x} — about to wedge",
            elr, _spsr,
        );
        dump_und_history();
        kprintln!(
            "  elr_el2={:#x} caller-trace below; halting before ERET",
            read_sysreg!("elr_el2"),
        );
        cpu::halt();
    }
    // Write target PC to the stub's literal slot, then ERET into the
    // stub in UND mode (by leaving SPSR_EL2 alone). The stub does
    // `ldr lr, [pc, #0]; movs pc, lr` — CPU restores CPSR from SPSR_und
    // (preserved since UND entry) and PC from the literal.
    //
    // Using a literal in the stub (rather than staging the return PC
    // into LR_und = ctx.x[22] per Table D1-79) is the simpler and
    // platform-portable choice: `ic ivau` on the literal address is
    // a single barrier-coupled instruction, whereas the X22 path
    // would require relying on AArch64-ERET-to-AArch32 to faithfully
    // route x[22] into R14_und across both QEMU raspi3b and FVP, and
    // the ROM-backing flush is needed regardless.
    let literal_host =
        guest_mem::rom_host_pa() as usize + guest_mem::UND_RETURN_STUB_LITERAL_OFFSET;
    // The UND_RETURN_STUB does `ldr lr, [pc, #0]` to load this literal,
    // running under BE-8 with CPSR.E=1. Host bytes must be BE-encoded
    // so the guest's LDR returns `elr` numerically — write swap of elr.
    // Guest-test mode doesn't run BE-8; identity write.
    #[cfg(not(nh_guest_test))]
    let literal_value = (elr as u32).swap_bytes();
    #[cfg(nh_guest_test)]
    let literal_value = elr as u32;
    // SAFETY: bounded write in ROM backing; EL2-owned. Flush via D-cache
    // clean + I-cache invalidate so the guest fetch path sees the new
    // literal.
    unsafe {
        core::ptr::write_volatile(literal_host as *mut u32, literal_value);
        core::arch::asm!(
            "dc cvau, {0}",
            "dsb ish",
            "ic ivau, {0}",
            "dsb ish",
            "isb",
            in(reg) literal_host as u64,
            options(nostack, preserves_flags),
        );
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "isb",
            elr = in(reg) guest_mem::UND_RETURN_STUB_VA as u64,
            options(nostack, preserves_flags),
        );
    }
}

// Guest-PA memory accessors. Word/halfword reads go through
// `guest_endian` so the BE-8 byte-order swap happens in one place.
// Byte accessors keep the legacy `guest_mem::*_byte_pa` paths because
// the existing call sites supply already-XOR-3-transformed addresses
// (under BE-32 word-invariant). Phase 4 simplifies the byte sites.
use crate::guest_endian::{guest_read_u32_pa as read_guest_word_pa,
                          guest_write_u32_pa as write_guest_word_pa};
use guest_mem::{read_byte_pa as read_guest_byte_pa,
                write_byte_pa as write_guest_byte_pa};

/// Budgeted log for the DABT→kernel forward path. Prints once per unique
/// (FAR, hvc_src_mode, pre_abt_mode) tuple so we see each distinct fault
/// site without flooding on tight-loop faults (e.g. a page-table walk
/// the kernel is filling in one entry at a time).
///
/// Including `pre_abt_mode` (`SPSR_abt & 0x1F`) in the dedup key
/// distinguishes a USR-pre-abt fault from an SVC-pre-abt fault at the
/// same FAR — needed for the L1[0xCD] investigation, where we suspect
/// a silently-handled USR-pre-abt fault precedes the visible SVC-pre-abt
/// one. See `docs/plans/l1-cd-lazy-investigation.md` Step 6a.
fn log_dabt_forward(dfsc: u32, far: u32, mode: u32, ctx: &TrapContext) {
    let spsr_abt = read_banked_spsr("abt") as u32;
    let pre_abt_mode = spsr_abt & 0x1F;
    // Cross-check `mrs spsr_abt` against the trampoline-saved SPSR_abt
    // (docs/QEMU_BUGS.md Bug #1: QEMU raspi3b returns stale spsr_abt
    // for `mrs` from EL2). The trampoline writes the slot before any
    // kernel code runs, so the slot is the architecturally-correct
    // pre-abt CPSR.
    let spsr_abt_save = crate::guest_endian::guest_read_u32_pa(guest_mem::DABT_SAVE_PA + 8).unwrap_or(0);
    let pre_abt_mode_save = spsr_abt_save & 0x1F;
    const SEEN_CAP: usize = 16;
    static mut SEEN: [(u32, u32, u32); SEEN_CAP] = [(0, 0, 0); SEEN_CAP];
    static mut SEEN_N: usize = 0;
    // Dedup on the saved-slot mode (architecturally correct) so a single
    // physical fault doesn't double-print just because `mrs spsr_abt`
    // reads a different (stale) value than the saved slot.
    let dedup_mode = pre_abt_mode_save;
    // SAFETY: single-threaded EL2.
    let first = unsafe {
        let mut found = false;
        for i in 0..SEEN_N {
            if SEEN[i] == (far, mode, dedup_mode) { found = true; break; }
        }
        if !found && SEEN_N < SEEN_CAP {
            SEEN[SEEN_N] = (far, mode, dedup_mode);
            SEEN_N += 1;
            true
        } else {
            false
        }
    };
    if first {
        // Capture more context: LR_abt (faulting PC + 8) tells us *where*
        // the kernel was when the abort happened — critical when
        // mode=ABT (recursive abort) because the FAR alone doesn't
        // identify the kernel-side instruction that wandered into the
        // unmapped VA. SPSR_abt names the mode the abort was taken from
        // (i.e. the mode that was running before this abort). For mode=ABT
        // (recursive) SPSR_abt also reads ABT — confirming the
        // double-fault.
        let lr_abt = ctx.x[20] as u32;
        let sp_abt = ctx.x[21] as u32;
        let lr_usr = ctx.x[14] as u32;
        let sp_usr = ctx.x[13] as u32;
        let lr_svc = ctx.x[18] as u32;
        let sp_svc = ctx.x[19] as u32;
        // For ARM-mode DABT, faulting_pc = LR_abt - 8.
        let faulting_pc = lr_abt.wrapping_sub(8);
        kprintln!(
            "dabt: forwarding to kernel DataAbortHandler — DFSC={:#x} FAR={:#010x} mode={:#x}",
            dfsc, far, mode
        );
        kprintln!(
            "  LR_abt={:#010x} (faulting PC={:#010x}) SP_abt={:#010x} SPSR_abt={:#010x} (pre-abt mode={:#x}){}",
            lr_abt, faulting_pc, sp_abt, spsr_abt, spsr_abt & 0x1F,
            if pre_abt_mode_save != pre_abt_mode {
                "  [mrs] -- mrs DIVERGES FROM SAVED SLOT --"
            } else { "" },
        );
        kprintln!(
            "  saved-slot SPSR_abt={:#010x} (pre-abt mode={:#x} = {})",
            spsr_abt_save, pre_abt_mode_save, describe_aarch32_mode(pre_abt_mode_save),
        );
        kprintln!(
            "  USR sp={:#010x} lr={:#010x}   SVC sp={:#010x} lr={:#010x}",
            sp_usr, lr_usr, sp_svc, lr_svc
        );
        kprintln!(
            "  r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} r12={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32, ctx.x[12] as u32
        );
        // If the faulting USR PC is in the SBA stub pool, decode slot
        // 14's back-branch to recover the original ROM PC the stub
        // emulates. Same trick as the dabt-trip handler above; lets us
        // identify the kernel/user instruction that took the
        // unresolvable fault even when it's a hot SBA-instrumented
        // store.
        if (crate::shadow_stub::SBA_STUB_POOL_IPA
            ..crate::shadow_stub::SBA_STUB_POOL_END)
            .contains(&faulting_pc)
        {
            let off = faulting_pc.wrapping_sub(crate::shadow_stub::SBA_STUB_POOL_IPA);
            let slot_idx = off / crate::shadow_stub::SBA_STUB_BYTES;
            let slot_base = crate::shadow_stub::SBA_STUB_POOL_IPA
                + slot_idx * crate::shadow_stub::SBA_STUB_BYTES;
            let back_branch_pa = slot_base.wrapping_add(14 * 4);
            if let Some(insn) = crate::guest_endian::guest_read_u32_pa(back_branch_pa) {
                if (insn & 0x0F00_0000) == 0x0A00_0000 {
                    let imm24 = insn & 0x00FF_FFFF;
                    let signed = if imm24 & 0x0080_0000 != 0 {
                        imm24 | 0xFF00_0000
                    } else {
                        imm24
                    };
                    let target = back_branch_pa
                        .wrapping_add(8)
                        .wrapping_add((signed as i32 as u32) << 2);
                    let orig_pc = target.wrapping_sub(4);
                    kprintln!(
                        "  sba-stub: slot {} (base {:#010x}) emulates ROM PC {:#010x} (back-branch {:#010x} -> {:#010x})",
                        slot_idx, slot_base, orig_pc, back_branch_pa, target,
                    );
                }
            }
        }
        // Dump the stage-1 walk for the FAR. Crucial for distinguishing
        // "L1 entry missing" (DFSC=5) from "L2 entry missing"
        // (DFSC=7) — both would otherwise look the same in a brief log.
        guest_mem::dump_stage1_walk(far);
        // For DFSC=5 (section fault), also show the neighbouring L1
        // entries so we can see whether this section was an isolated
        // hole vs. a wider gap. Lazy "non-zero fault" descriptors
        // (e.g. 0x90 — type=00 with bit-7/bit-4 set) are a kernel
        // bookkeeping shape worth eyeballing across a window.
        if dfsc == 5 {
            guest_mem::dump_l1_neighbourhood(far);
        }
    }
}

fn log_und_budgeted(name: &str, pc: u32, payload: Option<u32>) {
    // Dedup SystemBootUND / TapFileCntlUND by PC — only 6 sites in ROM
    // total. Same rationale as log_debugger_und: one log per site gives
    // us clear bring-up breadcrumbs without flooding on tight loops.
    const SEEN_CAP: usize = 16;
    static mut SEEN: [u32; SEEN_CAP] = [0; SEEN_CAP];
    static mut SEEN_N: usize = 0;
    // SAFETY: single-threaded.
    let first = unsafe {
        let mut found = false;
        for i in 0..SEEN_N { if SEEN[i] == pc { found = true; break; } }
        if !found && SEEN_N < SEEN_CAP {
            SEEN[SEEN_N] = pc;
            SEEN_N += 1;
            true
        } else {
            false
        }
    };
    if first {
        match payload {
            Some(p) => kprintln!("und: {} @PC={:#x} payload={:#010x}", name, pc, p),
            None => kprintln!("und: {} @PC={:#x}", name, pc),
        }
    }
}

fn log_cp15_strongarm_clock(pc: u32) {
    static mut LOG_BUDGET: usize = 2;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if LOG_BUDGET > 0 {
            LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!("und: MCR p15,0,Rt,c15,c1,2 (StrongARM clock) @PC={:#x} — no-op", pc);
    }
}

/// Scan guest memory from `start` word-by-word for a null byte in
/// any of the bytes of each word, and return the VA one past the end
/// of the word that contains the null (aligned, since words are
/// 4-byte aligned). `max_words` bounds the search so a missing null
/// doesn't infinite-loop.
/// Log a guest C string pointed to by `addr`.
///
/// The Newton 717006 ROM is stored big-endian in the image file and
/// byteswapped per word at load time so LDR in our LE guest returns
/// the u32 the original BE CPU saw (see `guest_mem::load_newton_rom`).
/// Bytes within each 4-byte word end up reversed in host memory: a
/// word originally `0x48 0x65 0x6C 0x6C` ("Hell" in BE) is laid out
/// as `0x6C 0x6C 0x65 0x48` in host LE memory. To recover the
/// original byte sequence we re-swap each loaded word via
/// `to_be_bytes()`.
///
/// Guest-test binaries are LE-native (no ROM byteswap on load), so
/// the bytes in host memory are already in natural order — use
/// `to_le_bytes()`. We pick at compile time via `nh_guest_test`.
fn log_guest_string(prefix: &'static str, addr: u32) {
    const CAP: usize = 256;
    let mut buf = [0u8; CAP];
    let mut len = 0usize;
    let mut va = addr;
    'outer: while len < CAP {
        let w = match read_guest_word_pa(va & !0x3) {
            Some(v) => v,
            None => break,
        };
        #[cfg(nh_guest_test)]
        let bytes = w.to_le_bytes();
        #[cfg(not(nh_guest_test))]
        let bytes = w.to_be_bytes();
        let first = (va & 0x3) as usize;
        for i in first..4 {
            let b = bytes[i];
            if b == 0 { break 'outer; }
            buf[len] = b;
            len += 1;
            if len == CAP { break 'outer; }
        }
        va = (va & !0x3).wrapping_add(4);
    }
    match core::str::from_utf8(&buf[..len]) {
        Ok(s) => kprintln!("{}: {:?}", prefix, s),
        Err(_) => kprintln!("{}: <{} non-utf8 bytes @ {:#x}>", prefix, len, addr),
    }
}

fn scan_to_null_word_aligned(start: u32, max_words: u32) -> u32 {
    let mut va = start & !0x3;
    for _ in 0..max_words {
        let w = read_guest_word_pa(va).unwrap_or(0);
        // The ROM is stored big-endian (original 1990s Newton bytes)
        // and our load_rom byteswaps each word so LDR in our LE guest
        // returns the same u32 the original BE CPU saw. That means a
        // byte-level string search has to examine the word in BE byte
        // order — the null terminator is *BE-byte-order* inside a
        // word, which is why we use to_be_bytes here, not to_le_bytes.
        let bytes = w.to_be_bytes();
        if bytes[0] == 0 || bytes[1] == 0 || bytes[2] == 0 || bytes[3] == 0 {
            return va.wrapping_add(4);
        }
        va = va.wrapping_add(4);
    }
    // No null found within bound — return (start + max_words*4) as a
    // best-effort stop. Caller will log + the guest may fault on the
    // next fetch, which makes the miss visible.
    va
}

fn log_debugger_und(pc: u32, msg_start: u32, msg_end: u32) {
    // Dedup by PC: each DebuggerUND site in the ROM is a distinct panic
    // message (e.g. "_stack_overflow called - panic!", "Undefined SWI",
    // "SWI from non-user mode (rebooting)"), and the first time the guest
    // hits any one of them tells us something specific about where we've
    // diverged. There are ~22 sites across ROM + REx, so an unfiltered
    // log of first-hits isn't noisy. Repeated hits at the same PC are
    // suppressed.
    const SEEN_CAP: usize = 32;
    static mut SEEN: [u32; SEEN_CAP] = [0; SEEN_CAP];
    static mut SEEN_N: usize = 0;
    // SAFETY: single-threaded.
    let first = unsafe {
        let mut found = false;
        for i in 0..SEEN_N { if SEEN[i] == pc { found = true; break; } }
        if !found && SEEN_N < SEEN_CAP {
            SEEN[SEEN_N] = pc;
            SEEN_N += 1;
            true
        } else {
            false
        }
    };
    if first {
        // Extract the string (first up to 120 bytes) for the log.
        // See scan_to_null_word_aligned for why we iterate bytes in
        // BE order — the ROM's strings are laid out that way within
        // each 32-bit word on an LE host.
        let mut buf = [0u8; 120];
        let mut n = 0;
        let mut va = msg_start;
        'outer: while n < buf.len() && va < msg_end {
            let w = match read_guest_word_pa(va) {
                Some(v) => v,
                None => break,
            };
            for byte in w.to_be_bytes() {
                if byte == 0 { break 'outer; }
                buf[n] = byte;
                n += 1;
                if n >= buf.len() { break 'outer; }
            }
            va = va.wrapping_add(4);
        }
        let s = core::str::from_utf8(&buf[..n]).unwrap_or("<bad utf-8>");
        kprintln!(
            "und: DebuggerUND @PC={:#x} msg={:?} (resume at PC={:#x})",
            pc, s, msg_end
        );
    }
}

fn log_cp15_deprecated_cache_all(pc: u32) {
    static mut LOG_BUDGET: usize = 2;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if LOG_BUDGET > 0 {
            LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!(
            "und: MCR p15,0,Rt,c7,c7,0 (deprecated invalidate-unified-cache) @PC={:#x} — emulated as ICIALLU",
            pc
        );
    }
}

fn log_swp_budgeted(pc: u32, is_byte: bool, rn: usize, rd: usize, rm: usize, addr: u32) {
    static mut SWP_LOG_BUDGET: usize = 8;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if SWP_LOG_BUDGET > 0 {
            SWP_LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!(
            "und: SWP{} @PC={:#x} r{} <- [r{}={:#x}] <- r{}",
            if is_byte { "B" } else { "" }, pc, rd, rn, addr, rm
        );
    }
}

// ISS layout for EC=0x03 (trapped MCR/MRC to CP15):
//   [19:17]  Opc2
//   [16:14]  Opc1
//   [13:10]  CRn
//   [9:5]    Rt   (guest register operand)
//   [4:1]    CRm
//   [0]      Direction: 0 = write (MCR), 1 = read (MRC)
fn handle_cp15_trap(ctx: &mut TrapContext, iss: u32) {
    let is_read = (iss & 1) != 0;
    let _crm = ((iss >> 1) & 0xF) as u32;
    let rt = ((iss >> 5) & 0x1F) as usize;
    let crn = ((iss >> 10) & 0xF) as u32;
    let opc1 = ((iss >> 14) & 0x7) as u32;
    let opc2 = ((iss >> 17) & 0x7) as u32;
    let crm = _crm;

    // Budget-limited CP15 logging for bring-up diagnostics. Prints only the
    // first N unique (CRn, CRm, Opc1, Opc2, dir) tuples.
    static mut CP15_SEEN: [u32; 32] = [0; 32];
    static mut CP15_N: usize = 0;
    let key = ((is_read as u32) << 13)
        | (crn << 9)
        | (crm << 5)
        | (opc1 << 2)
        | opc2;
    // SAFETY: single-threaded.
    let should_log = unsafe {
        let mut found = false;
        for i in 0..CP15_N {
            if CP15_SEEN[i] == key { found = true; break; }
        }
        if !found && CP15_N < 32 {
            CP15_SEEN[CP15_N] = key;
            CP15_N += 1;
            true
        } else {
            false
        }
    };
    if should_log {
        let value_log = if is_read { 0 } else { ctx.x[rt] as u32 };
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "cp15: {} p15,{},Rt=r{},c{},c{},{{{}}} val={:#010x} @ELR={:#x}",
            if is_read { "MRC" } else { "MCR" },
            opc1, rt, crn, crm, opc2, value_log, elr
        );
    }

    // Dispatch on the full (opc1, CRn, CRm, opc2, dir) tuple. The
    // surface is fixed at 15 tuples for the 717006 ROM (see
    // probe/FINDINGS.md §16.4). The load-time CP15 patcher in
    // guest_mem.rs rewrites the StrongARM lax CRm=CRn encodings for
    // CRn ∈ {1,2,3,5,6} to the ARMv7 canonical CRm=0 form before the
    // guest runs, so we only see the ARMv7 encodings here; the three
    // cache and TLB groups (CRn=7, CRn=8) and the one-off StrongARM
    // clock-control write (CRn=15, CRm=1, opc2=2) keep their native
    // encodings.
    // Writes to virtual-memory CP15 regs (SCTLR/TTBR/DACR/FSR/FAR)
    // trap via HCR_EL2.TVM. Reads of the same registers are NOT
    // trapped (we don't set TRVM): the hardware already holds the
    // right values — for SCTLR/TTBR/DACR because we synced them on
    // the trapped write, for DFSR/DFAR because the CPU writes them
    // when it takes an EL1 stage-1 abort. Guest MRC reads go straight
    // to hardware and return the real values.
    //
    // Cache-maintenance (CRn=7) and TLB invalidation (CRn=8) are not
    // covered by TVM; they trap via HCR_EL2.TIDCP / TSW.
    let tuple = (opc1, crn, crm, opc2, is_read);
    match tuple {
        // --- writes to virtual-memory CP15 regs ---
        (0, 1, 0, 0, false) => {
            let value = ctx.x[rt] as u32;
            // Detect M=0→M=1 transitions and re-walk the stage-1 tables
            // then. The TTBR-write pass catches what was reachable at
            // that moment but misses coarse L1 entries populated after.
            // (ARMv4 small-page descriptors use bits[11:4] as four
            // subpage AP fields; ARMv7 reinterprets bit 9 as AP[2] and
            // bits[5:4] as AP[1:0], so entries like 0x04007F0E read as
            // AP[2:0]=100 (reserved) = no-access on A53 and writes
            // permission-fault.) Running fix on every M=1 write would
            // cost ~60k calls/sec under task switching, so we gate it
            // on the rising edge only. The rewrite is idempotent.
            let prev_sctlr = cp15::read_sctlr_el1() as u32;
            let was_off = (prev_sctlr & 1) == 0;
            let now_on = (value & 1) != 0;
            // Force SCTLR.A=1 on the guest so unaligned LDR/STR raises
            // an alignment fault at EL1. The DABT-vector trampoline
            // routes alignment faults to unaligned::handle_align_fault.
            //
            // Under BE-8 also force EE (bit 25) and E0E (bit 24) so the
            // kernel's SCTLR writes (which never set EE) don't drop us
            // back into LE data mode mid-boot. Guest-test builds keep
            // the kernel's value verbatim so LE flat-binary tests work.
            #[cfg(not(nh_guest_test))]
            let value_with_a = value | 0x2 | (1u32 << 25) | (1u32 << 24);
            #[cfg(nh_guest_test)]
            let value_with_a = value | 0x2;
            cp15::write_sctlr_el1(value_with_a as u64);
            // One-time cross-check: read SCTLR back to verify A-bit stuck,
            // and emit <<TRM_START>> so the tarmac-window capture begins
            // at the moment A=1 becomes live. Paired with emit_stop()
            // in handle_align_fault so we get the exact window from
            // "A=1 applied" through "first alignment fault decoded".
            static LOGGED_SCTLR_A_ONCE: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);
            if !LOGGED_SCTLR_A_ONCE.swap(true, core::sync::atomic::Ordering::Relaxed) {
                let readback = cp15::read_sctlr_el1() as u32;
                kprintln!(
                    "sctlr: first guest write {:#010x} → hw {:#010x} (A={}, M={}, V={})",
                    value, readback, (readback >> 1) & 1, readback & 1,
                    (readback >> 13) & 1,
                );
                // iter-85: tarmac window now opens from the FPE-entry
                // probe at 0x38d918 on entry #2 (= forward #2 = mvfs in
                // SetSystemVolume that wedges the FPE on IP corruption).
                // The SCTLR-A=1 trigger from the iter-78 alignment-fault
                // investigation is left disabled so it doesn't preempt
                // the FPE window. Re-enable when the active investigation
                // changes.
                let _ = ();  // tarmac::emit_start() suppressed for iter-85
            }
            log_sctlr_write(value);
            if was_off && now_on {
                // Drop HCR_EL2.DC. While the guest ran with stage-1
                // off, DC=1 gave its data accesses Normal-WB semantics
                // so they hit the same cache lines the hypervisor
                // writes. But DC=1 also suppresses the guest's stage-1
                // translation from EL2's side (DDI 0487 D13.2.50):
                // leaving it set past this point means every non-
                // identity VA → IPA mapping the guest sets up (the
                // UND trampoline's save slot being the first one we
                // hit) falls through as VA=IPA and stage-2-faults.
                crate::guest::set_dc_for_stage1_off(false);
                // The XN-rewrite walks RAM[0..0x4000] interpreting it
                // as the L1 table — that's correct only when the
                // guest's TTBR0 actually points there. Guest tests
                // that pick a different L1 base (e.g. their own table
                // at 0x04004000) would otherwise have RAM[0..0x4000]
                // (stack / scratch) corrupted by the walker. Gate on
                // the live TTBR0 value.
                if (cp15::read_ttbr0_el1() as u32 & 0xFFFF_C000) == 0x0400_0000 {
                    let rom_dirty = guest_mem::fix_stage1_xn_bits();
                    guest_mem::install_scratch_pool_l1_section();
                    if rom_dirty {
                        reseed_flash_checksums_if_needed();
                    }
                }
                // No cache maintenance here: the TTBR0 write handler
                // below OR's Inner/Outer-WB cacheability bits into
                // every guest TTBR0 write, so stage-1 walks share the
                // D-cache view of the producer (kernel's own page-
                // table writes, and our in-place rewrites in
                // fix_stage1_xn_bits). Producer + walker matched-
                // attributes keeps them coherent per ARM ARM §B2.8
                // without any DC CVAC burst. See the comment block at
                // the (0, 2, 0, 0) CP15-write case for the full
                // rationale.
                maybe_dump_l1_once();
                // Swap the UND trampoline's save-slot literal to the
                // kernel VA that L1[0xC0] maps to the RAM slot. Done
                // outside `enable_patches()` so a soft-reboot that
                // cycles M=1→0→1 re-applies the swap (the tracer
                // gates its UDF install on a one-shot flag, but the
                // literal needs to track every MMU transition).
                // SAFETY: single-word ROM-backing write under the
                // paused-guest invariant.
                unsafe { guest_mem::install_und_vector_swap_post_mmu(); }
            }
            // M=1→M=0: the guest is turning its stage-1 MMU off
            // (typically the SWIBoot→ROMBoot soft-reset path). Revert
            // the UND trampoline's save-slot literal to the pre-MMU
            // RAM IPA so any UND taken before MMU re-enable lands in
            // a stage-2-mapped IPA. Without this, the first trace-UDF
            // after a soft reboot stores to VA 0x0C00_4F0C with MMU
            // off, which faults at an unmapped IPA.
            if !was_off && !now_on {
                // Soft reboot: the guest turned its stage-1 MMU off.
                // Re-enable HCR_EL2.DC so data accesses stay Normal-WB
                // cacheable while we're back in the "MMU off" regime.
                crate::guest::set_dc_for_stage1_off(true);
                // SAFETY: single-word ROM-backing write under the
                // same paused-guest invariant as the original patch.
                unsafe { guest_mem::install_und_vector_swap_pre_mmu(); }
            }
        }
        (0, 2, 0, 0, false) => {
            // The 717006 kernel writes TTBR0 = 0x0400_0000 with the
            // low 14 bits cleared — IRGN = RGN = S = 0, so stage-1
            // page-table walks are Normal **Non-cacheable**
            // Non-shareable (DDI 0406C §B4.1.154 TTBR0 layout +
            // Table B3-17 attribute encodings). The kernel populates
            // its page tables via cacheable Normal-WB mappings (DC=1
            // while MMU is off, normal sections once MMU is on);
            // fix_stage1_xn_bits also edits L2 entries in place
            // through the hypervisor's Normal-WB view. Cacheable
            // producer + Non-cacheable walker is an ARM ARM §B2.8
            // mismatched-memory-attributes case: the walker bypasses
            // the D-cache and reads stale DRAM until a DC CVAC to
            // PoC. StrongARM's simpler cache model let the Newton ROM
            // get away without any table-side cache maintenance; on
            // Cortex-A53 under FVP the first walk after SCTLR.M=1
            // fetches pre-guest zeros (or post-rewrite L2 bytes that
            // never made it to DRAM) and prefetch-aborts on whatever
            // VA it tries, wedging the guest in a PABT-vector loop.
            //
            // Rather than bursting DC CVAC over all of RAM + ROM on
            // every M=0→M=1 (which costs ~64 Ki maintenance ops and
            // still leaves later, untraceable kernel-side table
            // updates to hit the same trap), rewrite the walker
            // attributes to match the producer: OR Inner WB/WA and
            // Outer WB/WA cacheability into every guest TTBR0 write.
            // Walker and producer then share the D-cache and stay
            // coherent without any explicit maintenance — including
            // for the kernel's runtime page-table updates we don't
            // trap.
            //
            // Fields set:
            //   bit 0 IRGN[1] = 0, bit 6 IRGN[0] = 1 → IRGN = 0b01 = WB/WA
            //   bits[4:3] RGN[1:0] = 0b01           → ORGN = 0b01 = WB/WA
            // (Encoding per DDI 0406C Table B3-17.)
            // S (bit 1) left as the guest wrote it: the boot kernel
            // leaves it 0 = Non-shareable, which matches our single-
            // core stage-2 RAM mapping's effective shareability.
            //
            // Guest MRC reads of TTBR0 aren't trapped (HCR_EL2.TRVM
            // is off) so they go through to hardware and see the
            // modified value. The Newton kernel writes TTBR0 during
            // MMU setup and doesn't read it back in a way that
            // inspects cacheability bits, so the asymmetry is
            // invisible to it in practice.
            const TTBR_WB_WA: u32 = (1 << 6) | (1 << 3);
            let raw = ctx.x[rt] as u32;
            let value = raw | TTBR_WB_WA;
            cp15::write_ttbr0_el1(value as u64);
            // First TTBR write locks in the guest's stage-1 table
            // location. Walk it once and normalise the XN / SBZ bits
            // before the guest turns stage-1 on.
            static mut TTBR_FIXED: bool = false;
            // SAFETY: single-threaded.
            let already = unsafe {
                let v = TTBR_FIXED;
                TTBR_FIXED = true;
                v
            };
            if !already && (raw & 0xFFFF_C000) == 0x0400_0000 {
                let rom_dirty = guest_mem::fix_stage1_xn_bits();
                guest_mem::install_scratch_pool_l1_section();
                if rom_dirty {
                    reseed_flash_checksums_if_needed();
                }
            }
        }
        (0, 3, 0, 0, false) => cp15::write_dacr32(ctx.x[rt]),
        (0, 5, 0, 0, false) => {
            // Guest writes to DFSR — pass through to hardware so
            // subsequent guest reads see the intended value.
            cp15::write_dfsr32(ctx.x[rt]);
        }
        (0, 6, 0, 0, false) => {
            // Guest writes to DFAR — pass through to FAR_EL1.
            cp15::write_far_el1(ctx.x[rt]);
        }

        // Cache maintenance (CRn=7). Per probe/FINDINGS.md §16.7:
        //   c7, c6, op2=0  Invalidate entire data cache
        //   c7, c6, op2=1  Clean+invalidate DC line (MVA)
        //   c7, c7, op2=0  Invalidate unified cache
        //   c7, c10, op2=1 Clean DC line (MVA)
        //   c7, c10, op2=4 Drain write buffer / DSB
        // A53 handles coherency natively for our config, so a DSB is
        // the only operation we actually need to preserve ordering
        // the guest expects. The other c7 ops are no-ops.
        (0, 7, _, _, false) => cp15::cache_maintenance_barrier(),

        // TLB invalidation (CRn=8):
        //   c8, c5, op2=0  ITLB invalidate all
        //   c8, c6, op2=1  DTLB invalidate by MVA
        //   c8, c7, op2=0  TLB invalidate all
        (0, 8, _, _, false) => cp15::invalidate_tlb(),

        // StrongARM-specific clock-control write (c15, c1, op1=0, op2=2).
        // Fires exactly once at boot; no observable effect from EL2.
        (0, 15, 1, 2, false) => { /* nop */ }

        // Guest VBAR_EL1 write (CP15 c12, c0, opc1=0, opc2=0). Needed
        // so tests that want a non-default exception-vector table can
        // install one; the real Newton ROM never writes VBAR (it uses
        // low vectors at 0), but the shadow_stub abort-transparency
        // test does.
        (0, 12, 0, 0, false) => {
            let value = ctx.x[rt] as u64;
            // SAFETY: VBAR_EL1 is writable at EL2; on ERET the guest
            // sees it as its own CP15 VBAR.
            unsafe {
                core::arch::asm!(
                    "msr vbar_el1, {}",
                    "isb",
                    in(reg) value,
                    options(nostack, preserves_flags),
                );
            }
        }

        _ => {
            // Unrecognised tuple — Phase A contract: halt loudly so
            // we model it here rather than silently returning zero /
            // dropping the write. probe/FINDINGS.md §16.4 enumerates
            // the 15 tuples 717006 uses.
            halt_unknown_cp15(is_read, opc1, crn, crm, opc2, rt, ctx);
        }
    }

    advance_elr(4);
}

/// FP / SIMD access trap from a lower EL (EC=0x07), routed to EL2 by
/// CPTR_EL2.TFP. On Newton this is how native-primitive calls arrive:
/// the guest executes `MCR p10, 0, Rd, cN, cM, {opc2}` and Einstein's
/// convention is that the CPU register Rd holds the "native call code"
/// (driver ID << 8 | sub-function). We decode the faulting instruction
/// from guest memory, read the named register, and hand it to
/// peripherals::native_primitives::execute.
///
/// MRC reads from CP10/CP11 (and any other FP/SIMD shape we don't
/// expect from Newton OS) halt loudly — extend the handler when a
/// ROM boot trips one.
fn handle_fp_simd(ctx: &mut TrapContext, _iss: u32) {
    let elr = read_sysreg!("elr_el2") as u32;
    let insn = match read_guest_word_pa(elr) {
        Some(w) => w,
        None => {
            kprintln!(
                "*** fp_simd: faulting PC {:#x} unreadable from EL2 backing stores",
                elr
            );
            cpu::halt();
        }
    };

    // Decode ARMv7 MCR / MRC (load/store to coprocessor, single
    // register). Encoding: cond 1110 opc1 L CRn Rd coproc opc2 1 CRm
    // Mask for (MCR or MRC) with bit 4 = 1 and the fixed 1110 prefix
    // is (insn & 0x0F00_0010) == 0x0E00_0010.
    let is_mcr_mrc = (insn & 0x0F00_0010) == 0x0E00_0010;
    let cop = (insn >> 8) & 0xF;
    let l_bit = (insn >> 20) & 1; // 0 = MCR, 1 = MRC

    if !(is_mcr_mrc && (cop == 10 || cop == 11)) {
        kprintln!(
            "*** fp_simd trap on unexpected instruction {:#010x} @PC={:#x}, halting",
            insn, elr
        );
        cpu::halt();
    }

    let rd = ((insn >> 12) & 0xF) as usize;
    let crn = (insn >> 16) & 0xF;
    let crm = insn & 0xF;
    let opc1 = (insn >> 21) & 0x7;
    let opc2 = (insn >> 5) & 0x7;

    if l_bit != 0 {
        kprintln!(
            "*** MRC from CP{} not supported: insn={:#010x} @PC={:#x} (opc1={} Rd=r{} CRn=c{} CRm=c{} opc2={})",
            cop, insn, elr, opc1, rd, crn, crm, opc2
        );
        cpu::halt();
    }

    // Einstein's NativeCoprocRegisterTransfer reads CPU register Rd as
    // the "native call" code. ARMv4 MCR with Rd=PC reads PC+12, but
    // the Newton kernel never uses PC there; flag it if we ever see
    // one so we can match Einstein's quirk.
    if rd == 15 {
        kprintln!(
            "*** MCR p{}: Rd=PC is an Einstein quirk (mCurrentRegisters[15]+4); halting to investigate",
            cop
        );
        cpu::halt();
    }

    let native_insn = ctx.x[rd] as u32;
    native_primitives::execute(ctx, native_insn, elr);

    advance_elr(4);
}

fn log_sctlr_write(value: u32) {
    static mut SCTLR_N: usize = 0;
    // SAFETY: single-threaded.
    let n = unsafe { let v = SCTLR_N; SCTLR_N += 1; v };
    if n < 6 {
        let sctlr_now = cp15::read_sctlr_el1();
        kprintln!(
            "cp15.sctlr[{}] wrote {:#010x} (M={} V={} C={} I={})",
            n, value,
            value & 1,
            (value >> 13) & 1,
            (value >> 2) & 1,
            (value >> 12) & 1,
        );
        kprintln!("   SCTLR_EL1 after write = {:#018x}", sctlr_now);
    }
}

fn maybe_dump_l1_once() {
    static mut L1_DUMPS: usize = 0;
    // SAFETY: single-threaded.
    let n = unsafe { let v = L1_DUMPS; L1_DUMPS += 1; v };
    if n < 10 {
        guest_mem::dump_guest_l1_table();
    }
}

/// Re-seed the flash ROM/REx checksums after `fix_stage1_xn_bits` has
/// modified ROM-resident L2 page tables. The original boot-time seed
/// (in main.rs) computed checksums over the unpatched ROM bytes; once
/// the kernel writes TTBR0 we walk its L1 table, find L2 tables that
/// live in ROM, and rewrite them in place to ARMv7-compatible form
/// (XN/AP/CB normalisation). That mutation invalidates the seeded
/// checksums — the kernel then sees flash[0x64..0x8C] mismatch its own
/// runtime CalculateROMREXCheckSums result and takes the heavyweight
/// `UpdateBlock0FromBlock1 → erase → rewrite` recovery path, which
/// diverges heap state and feeds the downstream "newt" UnhandledException.
/// Re-running the seed function recomputes over the post-mutation ROM
/// and overwrites flash[0x64..0x8C] so the kernel's later comparison
/// passes. Idempotent: subsequent calls (the kernel re-enables MMU on
/// every task switch) recompute the same value.
fn reseed_flash_checksums_if_needed() {
    // Idempotent: each call recomputes from the current ROM bytes and
    // writes flash[0x64..0x8C]. Subsequent calls (the kernel re-enables
    // MMU on every task switch) recompute, and any further L2-table
    // mutations in ROM get reflected before the kernel reaches the
    // checksum comparison in TReservedBlockAccessor::CheckIfRecoveryIsNeeded.
    crate::peripherals::flash::seed_rom_rex_checksums(
        guest_mem::rom_host_pa() as *const u32,
        guest_mem::ROM_SIZE,
    );
}

fn halt_unknown_cp15(is_read: bool, opc1: u32, crn: u32, crm: u32, opc2: u32, rt: usize, ctx: &TrapContext) -> ! {
    let value = if is_read { 0 } else { ctx.x[rt] as u32 };
    let elr = read_sysreg!("elr_el2");
    kprintln!();
    kprintln!("*** unhandled CP15 access halted (no silent stub per Phase A) ***");
    kprintln!(
        "  {} p15,{},Rt=r{},c{},c{},{{{}}}  val={:#010x}  @ELR={:#x}",
        if is_read { "MRC" } else { "MCR" },
        opc1, rt, crn, crm, opc2, value, elr
    );
    kprintln!(
        "  (extend handle_cp15_trap in trap.rs to service this tuple; cross-reference"
    );
    kprintln!(
        "   probe/FINDINGS.md §16.4 for the 15 tuples the 717006 ROM exercises.)"
    );
    cpu::halt();
}

// Small inline module with the raw sysreg touches, kept close to the
// dispatch above so the trap handler stays readable.
mod cp15 {
    // Only the write paths are used by the hypervisor: we intercept
    // guest MCRs to these CP15 registers via HCR_EL2.TVM and mirror
    // the value into the corresponding EL2 sysreg. Guest reads are
    // not trapped (we don't set TRVM) so they go straight to hardware
    // and return the current value, which is either what we synced
    // on the last trapped write (SCTLR/TTBR/DACR) or what the CPU
    // wrote on the last EL1 abort (DFSR/DFAR).

    pub fn write_sctlr_el1(v: u64) { sysreg_write!("sctlr_el1", v); sync(); }
    pub fn write_ttbr0_el1(v: u64) { sysreg_write!("ttbr0_el1", v); sync(); }
    pub fn write_dacr32(v: u64) { sysreg_write!("dacr32_el2", v); sync(); }

    /// AArch32 DFSR via DFSR32_EL2 (op0=3 op1=4 CRn=5 CRm=0 op2=0,
    /// ARM ARM D10.2.32). Both MRS and MSR to this register take an
    /// EC=0 (UNDEFINED) exception at EL2 on Cortex-A53 under QEMU
    /// raspi3b, despite the ARM ARM saying it should be accessible
    /// from EL2 AArch64 when a lower EL supports AArch32 (which
    /// ID_AA64PFR0_EL1.EL1=0x2 confirms it does). So `write_dfsr32`
    /// is a no-op — we swallow the write. The hardware maintains
    /// DFSR correctly at EL1 when it takes an abort, which is what
    /// a kernel's abort handler needs. Guest writes are rare and
    /// typically just attempts to clear the register; losing them
    /// has no functional impact since the next abort will overwrite.
    pub fn write_dfsr32(_v: u64) { /* DFSR32_EL2 MSR UNDEFs on A53 */ }

    pub fn write_far_el1(v: u64) { sysreg_write!("far_el1", v); sync(); }

    pub fn read_sctlr_el1() -> u64 { sysreg_read!("sctlr_el1") }
    pub fn read_ttbr0_el1() -> u64 { sysreg_read!("ttbr0_el1") }

    pub fn cache_maintenance_barrier() {
        // StrongARM c7 cache ops don't all map cleanly to A53 encodings
        // and A53 handles coherency natively for our configuration. A
        // `dsb ish` covers the write-buffer-drain encoding the guest
        // issues most often; the rest are no-ops.
        sync();
    }

    pub fn invalidate_tlb() {
        // SAFETY: TLBI variants are defined sysreg writes.
        unsafe {
            core::arch::asm!(
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    pub fn invalidate_icache_all() {
        // ARMv8 equivalent of ARMv4 `MCR p15, 0, Rt, c7, c7, 0`
        // (invalidate unified cache). A53 has split I/D caches with
        // broadcast; `IC IALLUIS` covers the inner-shareable domain.
        // The D-cache is handled by A53's native coherency for our
        // config, so no explicit DCCISW loop is needed here.
        // SAFETY: cache maintenance sysreg writes.
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "ic ialluis",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    fn sync() {
        // SAFETY: barrier instructions only.
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    macro_rules! sysreg_read {
        ($reg:literal) => {{
            let v: u64;
            unsafe {
                core::arch::asm!(
                    concat!("mrs {}, ", $reg),
                    out(reg) v,
                    options(nomem, nostack, preserves_flags),
                );
            }
            v
        }};
    }
    macro_rules! sysreg_write {
        ($reg:literal, $val:expr) => {{
            unsafe {
                core::arch::asm!(
                    concat!("msr ", $reg, ", {}"),
                    in(reg) $val,
                    options(nostack, preserves_flags),
                );
            }
        }};
    }
    pub(crate) use {sysreg_read, sysreg_write};
}

fn handle_unknown(iss: u32) -> ! {
    let elr = read_sysreg!("elr_el2");
    let spsr = read_sysreg!("spsr_el2");
    // EC=0 "unknown reason" — an illegal / undefined AArch32 instruction.
    // Phase A contract: halt loudly with the faulting PC so we can see
    // what instruction the guest tried to execute and add handling for
    // it. No silent skip.
    kprintln!();
    kprintln!("*** EC=0 'unknown' trap halted (no silent skip per Phase A) ***");
    kprintln!("  ELR={:#x}  SPSR={:#x}  ISS={:#x}", elr, spsr, iss);
    if let Some(w) = crate::guest_endian::guest_read_u32_pa(elr as u32) {
        kprintln!("  insn at ELR = {:#010x}", w);
    }
    cpu::halt();
}

// ----------------- helpers -----------------

fn advance_elr(bytes: u64) {
    let elr = read_sysreg!("elr_el2");
    // SAFETY: single-word write to EL2 sysreg; next ERET uses the new value.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {}",
            "isb",
            in(reg) elr + bytes,
            options(nostack, preserves_flags),
        );
    }
}

pub fn describe_ec(ec: u32) -> &'static str {
    match ec {
        0x00 => "Unknown reason",
        0x07 => "SIMD/FP access trap (CPTR_EL2.TFP)",
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
