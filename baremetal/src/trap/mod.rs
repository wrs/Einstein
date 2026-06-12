//! EL2 synchronous trap dispatcher.
//!
//! The vector at offset 0x600 (lower-EL AArch32 sync) saves the full x0..x30
//! context, hands us a `*mut TrapContext`, and we dispatch based on ESR_EL2.EC.
//!
//! Handlers that emulate a guest instruction and want to resume mutate the
//! context in place, advance ELR_EL2 past the faulting instruction, then
//! return — the vector trailer restores the context and ERETs. Handlers that
//! don't want to resume never return (they call `cpu::halt`).

use crate::{cpu, guest_mem, kprintln, peripherals::{native_primitives, vic}, platform, timer};
use crate::trap_context::{advance_elr, describe_ec, read_sysreg, TrapContext};

mod dabt;
mod diag;
mod hvc;
pub(crate) mod cp15;
pub(crate) mod und;

use dabt::{handle_data_abort, resolve_ipa};
use cp15::handle_cp15_trap;
use hvc::handle_hvc;
pub(crate) use und::return_to_guest_from_und;
use crate::guest_endian::guest_read_u32_pa as read_guest_word_pa;

const EC_UNKNOWN: u32 = 0x00;
// EC=0x03: trapped MCR/MRC access to CP15 with opc1==0 (and some other
// combinations). This is what we see when HCR_EL2.TVM/TRVM/TIDCP steer a
// guest CP15 access to EL2 instead of letting it go through on real CP15.
const EC_TRAPPED_CP15: u32 = 0x03;
const EC_FP_SIMD: u32 = 0x07;
const EC_HVC_A32: u32 = 0x12;
const EC_INSN_ABORT_LOWER: u32 = 0x20;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

// (UND / ALIGN / GPIO_TRIGGER / DIAG immediates live in
//  `crate::hvc_imm::HvcImm` — see that module for descriptions.)

// Per-EC / per-HVC-imm / per-DABT-(PC,IPA) histograms live in
// `crate::trap_hist`. The sync dispatcher below calls
// `trap_hist::record_sync(ec)` for every sync trap and the HVC + DABT
// handlers feed in their own sub-bucket records; `trap_irq` calls
// `trap_hist::dump_and_reset()` every ~2 s to print and zero the window.


/// Synchronous exception from a lower EL running AArch32.
#[no_mangle]
pub extern "C" fn trap_sync_lower_aarch32(ctx: &mut TrapContext) {
    let esr = read_sysreg!("esr_el2");
    let ec = ((esr >> 26) & 0x3f) as u32;
    let iss = (esr & 0x01ff_ffff) as u32;

    crate::trap_hist::record_sync(ec);

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

    // Trap-exit tail. This sync-trap tail and the `irq_from_guest` tail
    // below share the input-pump → update_virq core (drain pen events so
    // a freshly raised INT_TABLET reflects into HCR_EL2.VI on THIS exit,
    // not the next), but the two are NOT a single sequence: the IRQ tail
    // additionally services the timer, DMA pumps, audio tick, heartbeat
    // sampling, autosave, and the histogram dump — work that must not run
    // on every sync trap. They are kept separate by design and
    // co-located here so the shared core stays visible.
    //
    // Drain any pen events from the host viewer before update_virq,
    // so a freshly raised INT_TABLET gets reflected into HCR_EL2.VI
    // on this trap exit instead of waiting for the next CNTHP
    // heartbeat. Cheap: backend self-throttles to 16 ms wall.
    crate::host_io::pump_input();
    crate::input::pump();
    // (audio used to be pumped here from the trap tail. With cyclic
    // DMA driving MAI from a hardware-paced CB chain, audio refills
    // happen from `audio::on_mai_dma_done` — the DMA
    // period-completion IRQ — and from `schedule_output` when the
    // kernel queues a new buffer. Liveness no longer depends on
    // trap rate, which is something the rest of the hypervisor is
    // trying to reduce.)

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
    // divergence at TStackInfo::Init #12.
    crate::stage2::tick_page::update_from_sync_trap();

    // Budget-limited "progress beacon": print PC every 10k traps so we
    // can see if the guest is making forward progress or looping in one
    // place. Doesn't halt — lets boot continue.
    static mut TRAP_COUNTER: u64 = 0;
    // SAFETY: single-threaded.
    let n = unsafe { TRAP_COUNTER += 1; TRAP_COUNTER };
    if n % 10_000 == 0 {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        crate::log_traps!(
            "beacon: {} traps, ELR={:#x} SPSR={:#x} int_present={:#x}",
            n, elr, spsr, vic::int_present_raw()
        );
    }
    #[cfg(feature = "platform-fvp-base")]
    crate::tarmac::maybe_emit_start(n);
}

/// Asynchronous IRQ taken at EL2. Dispatched by interruptee: an IRQ
/// taken while the AArch32 guest was running needs the full guest-path
/// servicing (`irq_from_guest`); an IRQ taken while EL2 itself was
/// running (boot, or a long operation inside an `with_irqs_unmasked`
/// window) gets the slim, interruptee-agnostic `irq_from_el2`.
///
/// The interruptee is identified from SPSR_EL2. SPSR_EL2.M[4]==1 means
/// the previous PSTATE was AArch32 — that's always the guest at EL1
/// (the hypervisor never executes AArch32), so it takes the guest path.
/// With M[4]==0 (AArch64) the level is M[3:2]: 0b10 is EL2 (mode 0x8
/// EL2t / 0x9 EL2h), i.e. we interrupted hypervisor code.
#[no_mangle]
pub extern "C" fn trap_irq(ctx: &mut TrapContext) {
    // Cheap EL2 stack-overflow tripwire: if a nested-IRQ / deep-frame
    // path has descended into the stack's guard canary, halt here
    // rather than let the corruption propagate. Runs on every timer/USB
    // IRQ, which is the steady cadence this guard relies on.
    cpu::check_stack_guard();

    // SAFETY: this is the EL2 IRQ vector entry — the one place permitted
    // to mint a slim-ISR capability. See `slim_isr` for the contract.
    let cap = unsafe { crate::slim_isr::IrqCap::mint() };

    let spsr = read_sysreg!("spsr_el2");
    let aarch32 = (spsr & (1 << 4)) != 0;
    let el2 = !aarch32 && ((spsr & 0b1100) == 0b1000);

    // Slim USB interrupt-IN fast path (real-hw touchscreen) — the
    // platform layer owns the BCM2835 pending-register decode and the
    // pen harvest. When USB was the sole pending source we skip the
    // heavy body; if a sample was enqueued and we're returning to the
    // guest, reflect INT_TABLET into HCR_EL2.VI now so the pen event is
    // delivered on this exit, not the next one. Co-pending sources fall
    // through to the normal path, whose tail `update_virq` picks up any
    // harvested sample.
    if let platform::UsbFastPath::UsbOnly { enqueued } = platform::poll_usb_fast_path() {
        if !el2 && enqueued {
            update_virq();
        }
        return;
    }

    if el2 {
        irq_from_el2(cap);
    } else {
        irq_from_guest(ctx, cap);
    }
}

/// Slim same-EL ISR: services an IRQ taken while EL2 hypervisor code
/// was running (boot before guest entry, or inside an
/// `cpu::with_irqs_unmasked` window in a trap handler).
///
/// ## Contract
///
/// May run nested inside *any* other EL2 handler (or unmasked boot
/// code). It must therefore touch no `ctx`-derived guest state and
/// nothing that interprets ELR_EL2 / SPSR_EL2 as the guest's, and it
/// owns a bounded set of mutable state that code in a
/// `cpu::with_irqs_unmasked` window must not touch. That state set, and
/// the compiler-enforced `IrqCap` gate on its two dispatch entry points
/// (`timer::on_irq`, `platform::dispatch_dma_completions`), are
/// documented once in [`crate::slim_isr`]. One subtlety worth keeping
/// local: `flash_persist::on_sd_dma_done`'s CMD12 busy-wait briefly
/// unmasks IRQs, so a nested IRQ re-enters this slim path — which does
/// not start saves, so the SD controller is never re-entered.
///
/// Deliberately absent vs. the guest path: no `ctx` access, no
/// heartbeat / wedge / task_dump / heap_check / tripwire sampling, no
/// host_io / input pumps, no `update_virq` (the guest is not running
/// while EL2 executes on this single core, so vIRQ delivery correctly
/// waits for the next guest trap exit), no snapshot autosave, no splash
/// progress, no g1/alrt capture rearm.
fn irq_from_el2(cap: crate::slim_isr::IrqCap) {
    // Acknowledge on the host CPU-interface (GICv3 on FVP, no-op on
    // BCM2836). A spurious ACK means nothing is pending and we skip
    // timer::on_irq, mirroring the guest path.
    let intid = platform::irq_ack();
    let spurious = intid == platform::irq_spurious();

    // BCM2835 DMA channel dispatch: channel N raises GPU IRQ source
    // 16+N. UART-TX owns ch 5, MAI-TX owns ch 4. Platform-owned; a
    // no-op on FVP and on QEMU raspi3b (no BCM2835 DMA engine there).
    platform::dispatch_dma_completions(cap);

    // CNTHP is level-triggered; not rearming it would storm. Calling
    // it when the real source was a DMA channel is harmless — it is
    // wall-clock-paced — and matches the guest path's behavior on BCM
    // where the ack is a no-op.
    if !spurious {
        timer::on_irq(cap);
    }

    // EOI last so the GIC is ready to deliver the next interrupt.
    // No-op on BCM2836.
    platform::irq_eoi(intid);
}

/// Guest-path IRQ servicing: an IRQ taken while the AArch32 guest was
/// running. Latches Newton timer-match deadlines into `vic::int_present`,
/// rearms CNTHP_CVAL_EL2, runs the diagnostic / input-pump / autosave
/// tail, and updates HCR_EL2.VI so the guest takes a virtual IRQ on ERET.
fn irq_from_guest(ctx: &mut TrapContext, cap: crate::slim_isr::IrqCap) {
    // Acknowledge the interrupt on the host CPU-interface (GICv3 on
    // FVP, no-op on BCM2836) before doing any work. On GICv3 the
    // returned INTID identifies which source fired; a spurious ACK
    // means nothing is pending and we skip timer::on_irq.
    let intid = platform::irq_ack();
    let spurious = intid == platform::irq_spurious();

    // BCM2835 IRQ controller dispatch (additive — CNTHP arrives via
    // the local-peripheral block at 0x4000_0040 and isn't reflected
    // here). DMA channel N raises GPU IRQ source 16+N (Circle's
    // ARM_IRQ_DMA0 = 16). UART-TX owns ch 5, MAI-TX owns ch 4.
    // Platform-owned; a no-op off real hardware.
    platform::dispatch_dma_completions(cap);

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
    // SAFETY: single-threaded.
    let (should_log, tag) = unsafe {
        HB_IRQ_COUNT += 1;
        if HB_FIRST_BUDGET > 0 && elr != HB_LAST_PC {
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
        let sp_svc = ctx.x[19] as u32;
        let lr_svc = ctx.x[18] as u32;
        crate::log_irqs!(
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

    // Periodically check whether the runtime heap has come up and log
    // its bounds once. Cheap idempotent — an atomic guard inside the
    // helper ensures it only does real work on the first success.
    crate::heap_check::log_heap_bounds_once();

    if !spurious {
        timer::on_irq(cap);
    }
    // Pump host PL011 -> guest extr-port RX DMA buffer. No-op when
    // DMA ch0 is not armed. See peripherals/dma.rs::poll_rx.
    crate::peripherals::dma::poll_rx();
    // Continue any in-flight guest extr-port TX DMA past the per-call
    // 4 KiB drain cap. No-op when DMA ch1 is not armed. See
    // peripherals/dma.rs::poll_tx.
    crate::peripherals::dma::poll_tx();
    // Pump the host-io backend: drain any pen events the viewer
    // posted, enqueue them, and raise INT_TABLET. Must run BEFORE
    // update_virq so the IRQ it raises lands in HCR_EL2.VI on this
    // trap exit, not the next one. `input::pump` is the parallel
    // path for real-hw pen sources (USB touchscreen) — it feeds the
    // same queue.
    crate::host_io::pump_input();
    crate::input::pump();
    // Audio tick: the null backend fires armed buffer-completion IRQs
    // here once a scheduled buffer's playback duration has elapsed,
    // raising the kernel's sound-output interrupt mask. Must run
    // BEFORE update_virq so a raised IRQ lands in HCR_EL2.VI on this
    // trap exit. The pi_hdmi backend ignores this and completes from
    // its own DMA-period IRQ (`audio::on_mai_dma_done`) instead.
    crate::audio::tick();
    update_virq();
    // Advance the boot-splash progress bar (no-op once the guest's
    // first blit has frozen the splash, and on platforms without
    // pi_fb). Driven from the timer IRQ tail so the bar grows on a
    // steady ~16 ms cadence regardless of trap-rate variation.
    #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
    crate::display::splash::update_progress(crate::trap_hist::sync_count());
    // Wall-clock-paced snapshot save. Timer IRQ is a cleaner hook
    // than sync traps: it fires regardless of whether the guest is
    // making forward progress, so we keep rolling a fresh snapshot
    // into the ring even when the guest is wedged. See
    // src/snapshot.rs.
    crate::snapshot::maybe_autosave(ctx);

    // Every ~2 s of wall, print the trap-frequency histogram so we
    // can see what dominates the residual trap rate (EC class, HVC
    // immediate, DABT PC/IPA). See `crate::trap_hist`. Independent
    // of snapshot autosave (which is gated when guest_bp is live).
    // Gated on `log_traps`: prints a multi-line histogram every 2s,
    // valuable for Phase-B but noise on a real-hardware boot.
    #[cfg(feature = "log_traps")]
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
            crate::trap_hist::dump_and_reset();
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

fn handle_instruction_abort(ctx: &TrapContext, iss: u32) {
    let far = read_sysreg!("far_el2");
    // Instruction aborts are always reads; pass wnr=false to resolve_ipa.
    // See resolve_ipa's doc for the HPFAR-vs-AT rationale.
    let ipa = resolve_ipa(iss, false);
    let elr = read_sysreg!("elr_el2");

    // RAM is mapped XN at stage-2 so the first fetch into any RAM page
    // traps here. Flip the page to RO + executable; the next write
    // stage-2-faults into the data-abort handler and re-arms it RW + XN.
    //
    // IFSC values (ISS bits [5:0]) we care about:
    //   0b001100..0b001111  permission fault levels
    let ifsc = (iss & 0x3f) as u32;
    let is_permission = (ifsc & 0b111100) == 0b001100;
    let ram_base = guest_mem::RAM_IPA_BASE as u64;
    let ram_end = ram_base + guest_mem::RAM_SIZE as u64;
    let in_ram = (ram_base..ram_end).contains(&ipa);

    if is_permission && in_ram {
        let page_start = (ipa as u32) & !0xFFFu32;
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
    let mode_name = crate::arm_decode::aarch32_mode_name(mode);
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
// SCRATCH_POOL's per-stub allocator (`NEXT_SCRATCH_SLOT`) starts past
// `RESERVED_SCRATCH_SLOTS` (32 slots = 256 B), keeping the trampoline's
// footprint (offsets 0x00..0xAC) reserved and never claimed by a stub.
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
/// faulting mode is USR) and saving its SP/LR. `handle_und` reads
/// `UND_SAVE_BANKED_LR_IPA` to recover the original LR for diagnostic
/// purposes (e.g. unhandled-exception forensics).
pub const UND_SAVE_BANKED_LR_IPA: u32 = HYP_TRAMP_SCRATCH_BASE + 0x1C;

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
    crate::trap_hist::record_fp_simd(elr);
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
