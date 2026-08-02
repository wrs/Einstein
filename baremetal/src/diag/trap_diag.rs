//! Diagnostics with behaviour: the trap-progress beacon, the
//! heartbeat / TStack-invariant dump, the budgeted loggers, the
//! loud-halt rendering, and `handle_diag`.

use crate::{arch::cpu, hv::guest_mem};
use crate::arch::trap_context::{read_sysreg, TrapContext};
use crate::kprintln;
use crate::hv::trap::UND_SAVE_SPSR_IPA;
use crate::hv::trap::und::read_banked_spsr;
use crate::hv::guest_mem::read_cstr_at;

/// Budget-limited "progress beacon": print PC every 10k traps so we
/// can see if the guest is making forward progress or looping in one
/// place. Doesn't halt — lets boot continue. Called once per sync
/// trap from `trap_sync_lower_aarch32`'s exit tail; also drives the
/// FVP tarmac window-open check off the same counter.
pub fn sync_trap_beacon() {
    static mut TRAP_COUNTER: u64 = 0;
    // SAFETY: single-threaded.
    let n = unsafe { TRAP_COUNTER += 1; TRAP_COUNTER };
    if n % 10_000 == 0 {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        crate::log_traps!(
            "beacon: {} traps, ELR={:#x} SPSR={:#x} int_present={:#x}",
            n, elr, spsr, crate::peripherals::vic::int_present_raw()
        );
    }
    #[cfg(feature = "platform-fvp-base")]
    crate::diag::tarmac::maybe_emit_start(n);
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

/// Canary handler shared by `Reboot`, `PowerOffAndReboot`, and
/// `StopImage`. Each site is patched with `HVC #LoudHalt` over its
/// first instruction, so we land here BEFORE the function's prologue
/// runs — ctx.x[0..14] alias the caller's AArch32 R0..R14, and
/// ELR_EL2 == the patched function's entry PC.
///
/// All three sites are end-of-the-line for the kernel: it's either
/// rebooting after a fatal check or going idle. Dump state, halt the
/// host. Distinguish sites by ELR_EL2 in the log line.
/// Walk the kernel's `TStackManager` and dump every `TStackInfo` it
/// owns, checking invariants along the way:
///
///  - guard size (`info[+4] - info[+20]`) should be exactly 4 KiB
///    (our patched value; original kernel was 1 KiB)
///  - data range (`info[+28] - info[+4]`) should be a multiple of 4 KiB
///  - current bound (`info[+24]`) should be in `[info[+4], info[+28]]`
///  - `info[+0] == info[+28]` (top is stored twice at init)
///  - `info[+4]` should be in `[info[+20], info[+28]]`
///  - per-stack VA range `[info[+20], info[+28])` should not overlap
///    any other stack's range
///
/// Layout source: `Init__10TStackInfoFUlN51` at ROM 0x001f6700 (we read
/// these field offsets directly from the disassembly there).
///
/// Manager lookup: the kernel has the global `gStackManagerHeap` at
/// VA 0x0c104c08 (the *literal* loaded by NewStack et al.). The actual
/// TStackManager pointer is held at `*(gStackManagerHeap + 4)` per the
/// ROM pattern `ldr r0, [r0, #4]` after loading the literal. The domain
/// queue lives at `TStackManager + 208` (`+0xD0`, see
/// `GetDomainForAddress__13TStackManager` at 0x001f8e48).
///
/// `marker_far` is highlighted in the output if any TStackInfo's range
/// covers it, so the busError-causing FAR is easy to correlate.
fn dump_tstacks_and_check_invariants(marker_far: u32) {
    use crate::hv::guest_endian::guest_read_u32_va as rd;

    const G_STACK_MGR_HEAP_LITERAL: u32 = 0x0c10_4c08;
    let tsm = rd(G_STACK_MGR_HEAP_LITERAL.wrapping_add(4)).unwrap_or(0);
    if tsm == 0 || tsm < 0x0c00_0000 || tsm >= 0x0d00_0000 {
        kprintln!(
            "tstack-dump: gStackManagerHeap[+4]={:#010x} doesn't look like a heap pointer; skipping",
            tsm
        );
        return;
    }
    kprintln!(
        "tstack-dump: TStackManager @ {:#010x}  (marker FAR={:#010x})",
        tsm, marker_far
    );

    // Domain queue lives at TStackManager + 0xD0 (verified via
    // GetDomainForAddress at ROM 0x001f8e48: `add r0, r0, #208`).
    //
    // TDoubleQContainer layout (from Peek/GetNext at 0x0009c884/0x0009c89c):
    //   +0  head_item_ptr  (NULL when empty; otherwise points at the
    //                       TDoubleQItem inside the first element)
    //   +4  tail_item_ptr
    //   +8  item_offset    (offset of the embedded TDoubleQItem within
    //                       each element, i.e. element + item_offset
    //                       == item_ptr; THeapDomain's TDoubleQItem
    //                       lives at +4 per its ctor)
    //
    // TDoubleQItem layout (from __ct__12TDoubleQItemFv at 0x0009c6dc):
    //   +0  next_item_ptr (NULL = end of queue)
    //   +4  prev_item_ptr
    //   +8  back-pointer to the owning container
    //
    // Walking:
    //   element = Peek(container) = container[+0] != 0 ?
    //             container[+0] - container[+8] : NULL
    //   next    = GetNext(container, element):
    //             item = element + container[+8];
    //             item[+8] must equal container (sanity);
    //             next_item = item[+0];
    //             return next_item != 0 ? next_item - container[+8] : NULL
    let container = tsm.wrapping_add(0xD0);
    let head_item   = rd(container.wrapping_add(0)).unwrap_or(0);
    let item_offset = rd(container.wrapping_add(8)).unwrap_or(0);
    kprintln!(
        "  domain queue @ {:#010x}: head_item={:#010x} item_offset={:#x}",
        container, head_item, item_offset
    );
    if item_offset > 0x100 {
        kprintln!("  (item_offset suspicious; aborting walk)");
        return;
    }
    let mut domain = if head_item == 0 { 0 } else { head_item.wrapping_sub(item_offset) };

    // Collect ranges to check overlap.
    let mut ranges: [(u32, u32); 64] = [(0, 0); 64];
    let mut nranges = 0usize;
    let mut total_stacks = 0usize;
    let mut errors = 0usize;

    // Print one TStackInfo run (slots `run_first..=run_first+run_count-1`
    // all pointing at the same `info`), check its invariants, and record
    // its VA range for the overlap pass. Used both when a run ends mid-
    // iteration and for the trailing run.
    let flush_run = |info: u32,
                     run_first: u32,
                     run_count: u32,
                     total_stacks: &mut usize,
                     errors: &mut usize,
                     ranges: &mut [(u32, u32); 64],
                     nranges: &mut usize| {
        if info == 0 || run_count == 0 {
            return;
        }
        let i_hard  = rd(info.wrapping_add(4)).unwrap_or(0);
        let i_norm  = rd(info.wrapping_add(20)).unwrap_or(0);
        let i_curr  = rd(info.wrapping_add(24)).unwrap_or(0);
        let i_end   = rd(info.wrapping_add(28)).unwrap_or(0);
        let i_field0= rd(info.wrapping_add(0)).unwrap_or(0);
        let i_n     = rd(info.wrapping_add(8)).unwrap_or(0);
        let guard   = i_hard.wrapping_sub(i_norm);
        let range   = i_end.wrapping_sub(i_hard);
        let covers_marker = marker_far >= i_norm && marker_far < i_end;
        kprintln!(
            "    slot[{:3}..{:3}] info @ {:#010x}: norm={:#010x} hard(+4)={:#010x} curr(+24)={:#010x} top(+28)={:#010x} +0={:#010x} +8(n)={:#x} guard={:#x} range={:#x}{}",
            run_first, run_first + run_count - 1, info,
            i_norm, i_hard, i_curr, i_end, i_field0, i_n, guard, range,
            if covers_marker { "  ***MARKER***" } else { "" },
        );
        *total_stacks += 1;
        if guard != 0x1000 {
            kprintln!("      [INV] guard != 4 KiB: {:#x}", guard);
            *errors += 1;
        }
        if i_curr < i_hard || i_curr > i_end {
            kprintln!("      [INV] info[+24]={:#010x} not in [hard..top]", i_curr);
            *errors += 1;
        }
        if i_hard < i_norm || i_hard > i_end {
            kprintln!("      [INV] info[+4]={:#010x} not in [norm..top]", i_hard);
            *errors += 1;
        }
        if *nranges < ranges.len() {
            ranges[*nranges] = (i_norm, i_end);
            *nranges += 1;
        }
    };

    for _d_iter in 0..16 {
        if domain == 0 { break; }
        if domain < 0x0c00_0000 || domain >= 0x0d00_0000 {
            kprintln!("  domain @ {:#010x} not heap-shaped; stopping walk", domain);
            break;
        }
        let pool_start = rd(domain.wrapping_add(16)).unwrap_or(0);
        let pool_end   = rd(domain.wrapping_add(20)).unwrap_or(0);
        let num_slots  = rd(domain.wrapping_add(24)).unwrap_or(0);
        let slots_ptr  = rd(domain.wrapping_add(28)).unwrap_or(0);
        kprintln!(
            "  THeapDomain @ {:#010x}: pool=[{:#010x}..{:#010x}) num_slots={} slots@={:#010x}",
            domain, pool_start, pool_end, num_slots, slots_ptr,
        );
        if num_slots > 1024 || slots_ptr == 0
            || slots_ptr < 0x0c00_0000 || slots_ptr >= 0x0d00_0000 {
            kprintln!("    (suspect domain layout — skipping slot iteration)");
        } else {
            // Each TStackInfo can be referenced from multiple
            // consecutive entries in slot_array (FMNewStack fills
            // slot_array[r6..sl] = same info* for a stack spanning
            // multiple slot indices). Dedup by tracking the most
            // recently-printed info pointer and the run length, then
            // print once per distinct info with a slot-range.
            let mut last_info: u32 = 0;
            let mut run_first: u32 = 0;
            let mut run_count: u32 = 0;
            for s in 0..num_slots {
                let info = rd(slots_ptr.wrapping_add(s.wrapping_mul(4))).unwrap_or(0);
                if info == last_info && info != 0 {
                    run_count += 1;
                    continue;
                }
                // Flush previous run, then start a new one.
                flush_run(last_info, run_first, run_count,
                    &mut total_stacks, &mut errors, &mut ranges, &mut nranges);
                last_info = info;
                run_first = s;
                run_count = if info == 0 { 0 } else { 1 };
            }
            // Flush trailing run.
            flush_run(last_info, run_first, run_count,
                &mut total_stacks, &mut errors, &mut ranges, &mut nranges);
        }

        // GetNext: read next_item from item[+0], subtract item_offset.
        let item = domain.wrapping_add(item_offset);
        let next_item = rd(item.wrapping_add(0)).unwrap_or(0);
        if next_item == 0 { break; }
        domain = next_item.wrapping_sub(item_offset);
    }

    // Pairwise overlap check for VA ranges.
    for i in 0..nranges {
        let (a_lo, a_hi) = ranges[i];
        for j in (i + 1)..nranges {
            let (b_lo, b_hi) = ranges[j];
            if a_lo < b_hi && b_lo < a_hi {
                kprintln!(
                    "      [INV] VA overlap: [{:#010x}..{:#010x}) overlaps [{:#010x}..{:#010x})",
                    a_lo, a_hi, b_lo, b_hi
                );
                errors += 1;
            }
        }
    }

    kprintln!(
        "tstack-dump: walked {} TStackInfo(s); {} invariant violations.",
        total_stacks, errors
    );
}

/// Loud-halt canary dispatcher. The `apply_loud_halt_traps` ROM patches
/// rewrite the first instruction of the kernel reset / fault sinks
/// (`Reboot`, `PowerOffAndReboot`, `StopImage`, the bus-error `Throw`)
/// to an `HVC` that routes here, so the boot stops at the first hit with
/// a full context dump instead of silently rebooting. Identifies which
/// site fired (priv-mode HVCs land ELR_EL2 just past the patched insn;
/// USR-mode HVCs route through the UND trampoline so the real site is
/// `LR_<mode> - 4`), prints the canary, and halts.
pub(crate) fn handle_loud_halt(ctx: &TrapContext) -> ! {
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    let elr_el2 = read_sysreg!("elr_el2") as u32;
    let mode = spsr_el2 & 0x1F;
    let caller_lr = crate::arch::banked::lr_for_mode(ctx, spsr_el2);
    // ELR_EL2 captures the post-HVC PC (= patched-site PC + 4) for
    // priv-mode HVCs, so subtract 4 to get the patched site itself.
    // For USR-mode (HVC routed through UND_TRAMP) the offsets work
    // out the same way.
    // For priv-mode HVCs ELR_EL2 points just past the patched insn, but
    // for USR-mode (routed via the UND trampoline) ELR_EL2 lands inside
    // the trampoline at 0xFFFFxx — the real patched site is then
    // caller_lr - 4 (since `bl Throw` saves PC+4 in LR_UND before the
    // trampoline emits its HVC). Pick whichever matches a known site.
    let pc_from_elr = elr_el2.wrapping_sub(4);
    let pc_from_lr = caller_lr.wrapping_sub(4);
    let known = |pc: u32| matches!(pc,
        crate::newton::rom_patches::REBOOT_PC
        | crate::newton::rom_patches::POWEROFF_REBOOT_PC
        | crate::newton::rom_patches::STOP_IMAGE_PC
        | crate::newton::rom_patches::BUS_ERROR_THROW_PC);
    let site_pc = if known(pc_from_elr) { pc_from_elr }
                  else if known(pc_from_lr) { pc_from_lr }
                  else { pc_from_elr };
    let site = match site_pc {
        crate::newton::rom_patches::REBOOT_PC => "Reboot",
        crate::newton::rom_patches::POWEROFF_REBOOT_PC => "PowerOffAndReboot",
        crate::newton::rom_patches::STOP_IMAGE_PC => "StopImage",
        crate::newton::rom_patches::BUS_ERROR_THROW_PC => "BusErrorThrow",
        _ => "LoudHalt",
    };
    kprintln!();
    kprintln!(
        "*** LoudHalt canary fired at {} (PC={:#010x}, ELR={:#010x}) ***",
        site, site_pc, elr_el2,
    );
    kprintln!(
        "  SPSR_EL2 = {:#010x}  mode={} ({:#x})",
        spsr_el2, crate::arch::arm_decode::aarch32_mode_name(mode), mode
    );
    kprintln!(
        "  R0 = {:#010x}  R1 = {:#010x}  R2 = {:#010x}  R3 = {:#010x}",
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
    );
    kprintln!(
        "  R12={:#010x}  R14_{}={:#010x}  (caller LR via Table D1-79)",
        ctx.x[12] as u32, crate::arch::arm_decode::aarch32_mode_name(mode), caller_lr
    );
    // BusErrorThrow site: also dump R4 (= TStackManager*), R5 (= the
    // ResolveFault return value, e.g. -10203/-10204), the FAR_EL1
    // (= the original fault VA), and the relevant TStackInfo bounds
    // so we can identify which stack overflowed.
    if site_pc == crate::newton::rom_patches::BUS_ERROR_THROW_PC {
        let far = read_sysreg!("far_el1") as u32;
        // Walk all TStacks and check invariants — output goes BEFORE
        // the per-register dump so the structural picture is visible.
        dump_tstacks_and_check_invariants(far);
        let r4 = ctx.x[4] as u32;
        let r5 = ctx.x[5] as u32;
        kprintln!(
            "  R4 = {:#010x} (TStackManager*)  R5 = {:#010x} ({} signed)",
            r4, r5, r5 as i32
        );
        kprintln!("  FAR_EL1 = {:#010x}  (the faulting VA)", far);
        // Dump the most-recent AArch32 DABT context, captured by the
        // DABT trampoline (slow + fast paths both store to
        // DABT_SAVE_PA before branching). For wild FARs the busError
        // path forwards through the fast trampoline straight to the
        // kernel's DataAbortHandler, never entering EL2 — so the
        // `dabt:` log never fires and `log_dabt_forward` can't see
        // the original faulting PC. Reading the slot here recovers
        // it. Caveat: if the kernel's DAH itself faults again before
        // reaching `Throw`, the slot would have been overwritten by
        // the recursive abort. In practice DAH's TStackInfo walk
        // touches only mapped memory, so the slot is the original.
        let dabt_lr_abt   = crate::hv::guest_endian::guest_read_u32_pa(crate::newton::guest_trampolines::DABT_SAVE_PA).unwrap_or(0);
        let dabt_sp_abt   = crate::hv::guest_endian::guest_read_u32_pa(crate::newton::guest_trampolines::DABT_SAVE_PA + 4).unwrap_or(0);
        let dabt_spsr_abt = crate::hv::guest_endian::guest_read_u32_pa(crate::newton::guest_trampolines::DABT_SAVE_PA + 8).unwrap_or(0);
        let dabt_pre_mode = dabt_spsr_abt & 0x1F;
        let dabt_thumb    = (dabt_spsr_abt & (1 << 5)) != 0;
        let dabt_faulting_pc = if dabt_thumb {
            dabt_lr_abt.wrapping_sub(4)
        } else {
            dabt_lr_abt.wrapping_sub(8)
        };
        kprintln!(
            "  DABT-save: LR_abt={:#010x}  SP_abt={:#010x}  SPSR_abt={:#010x} (pre-abt mode={} {:#x}{})",
            dabt_lr_abt, dabt_sp_abt, dabt_spsr_abt,
            crate::arch::arm_decode::aarch32_mode_name(dabt_pre_mode), dabt_pre_mode,
            if dabt_thumb { ", T" } else { "" },
        );
        kprintln!(
            "  DABT-save: faulting_PC = {:#010x}  (= LR_abt - {})",
            dabt_faulting_pc, if dabt_thumb { 4 } else { 8 },
        );
        kprintln!(
            "  R6 = {:#010x}  R7 = {:#010x}  R8 = {:#010x}  R9 = {:#010x}",
            ctx.x[6] as u32, ctx.x[7] as u32, ctx.x[8] as u32, ctx.x[9] as u32
        );
        // Banked SP/LR for each AArch32 mode — `ctx.x` indices follow
        // ARM ARM Table D1-79 (AArch64 EL2 view of AArch32 banked regs).
        kprintln!(
            "  banked: USR sp={:#010x} lr={:#010x}  SVC sp={:#010x} lr={:#010x}",
            ctx.x[13] as u32, ctx.x[14] as u32, ctx.x[19] as u32, ctx.x[18] as u32
        );
        kprintln!(
            "          ABT sp={:#010x} lr={:#010x}  IRQ sp={:#010x} lr={:#010x}",
            ctx.x[21] as u32, ctx.x[20] as u32, ctx.x[17] as u32, ctx.x[16] as u32
        );
        kprintln!(
            "          UND sp={:#010x} lr={:#010x}",
            ctx.x[23] as u32, ctx.x[22] as u32
        );
        // Walk the failing task's APCS call chain. R1 is the user-mode
        // SP at fault time (= second arg to Throw). The trapping insn
        // was a PUSH that did not complete, so the topmost frame on the
        // user stack is the CALLER of the function whose prologue
        // faulted (here `Lookup`). With the APCS prologue
        //   mov ip, sp
        //   stmfd sp!, {r4..rN, fp, ip, lr, pc}
        //   sub fp, ip, #4
        // each frame stores saved-PC at the highest address of the
        // frame, with saved-LR one word below, saved-IP one word below
        // that, and saved-FP one word below that. The current-frame FP
        // points at the saved-PC slot. Walking by `*(fp - 12)` recovers
        // the chain.
        //
        // We can't read R11 of the failing task directly here (the
        // kernel handlers between the data abort and our HVC have
        // clobbered the GPRs we see in `ctx`). But the caller's
        // saved-FP IS in stack memory, written by the caller's
        // prologue. The caller's FP value points at the saved-PC slot
        // of the caller's frame; that slot's address equals
        // `pre_prologue_sp_of_caller - 4`. Because BL doesn't change
        // SP, the caller's pre-prologue SP equals the SP at fault =
        // R1. So caller-FP candidate = SP - 4 + caller_frame_size.
        //
        // We don't know caller_frame_size. Scan upward from SP for
        // the first word that is itself a plausible same-stack
        // pointer (i.e. value in [SP, SP+0x100) with low bits clear)
        // and points one frame deeper into the chain — that's the
        // caller's saved-FP. Then the slot just before it
        // (pointed-at - 4) holds saved-LR; pointed-at + 0 holds
        // saved-PC.
        let sp_fail = ctx.x[1] as u32;
        kprintln!("  stack-trace: fault-SP={:#010x}", sp_fail);
        let mut start_fp: u32 = 0;
        for i in 0..32 {
            let slot_va = sp_fail.wrapping_add(i * 4);
            let cand = match crate::hv::guest_endian::guest_read_u32_va(slot_va) {
                Some(v) => v,
                None => continue,
            };
            // Plausible saved-FP: aligned, points to a slot above us
            // but still on the same stack page.
            if (cand & 3) != 0 { continue; }
            if cand <= sp_fail || cand > sp_fail.wrapping_add(0x800) { continue; }
            // The pointed-at word should look like a saved-PC (ROM
            // text). Saved PC for ARM = entry+8 due to prefetch.
            let pc_at_cand = match crate::hv::guest_endian::guest_read_u32_va(cand) {
                Some(v) => v,
                None => continue,
            };
            if pc_at_cand >= 0x0080_0000 { continue; }
            start_fp = cand;
            kprintln!(
                "    seed-FP = {:#010x} found in stack slot {:#010x}",
                start_fp, slot_va
            );
            break;
        }
        if start_fp != 0 {
            // Print the topmost (incomplete) frame ourselves: the
            // function whose prologue faulted, i.e. PC = the
            // faulting PC.
            let mut depth = 0usize;
            let frame_va_top = sp_fail; // the prologue hadn't pushed
            let pc_top = dabt_faulting_pc;
            let (n0, l0) = crate::diag::task_dump::fmt_pc_name(pc_top);
            kprintln!(
                "    #{:<2} frame={:#010x}  pc={:#010x}  {}",
                depth, frame_va_top, pc_top,
                core::str::from_utf8(&n0[..l0]).unwrap_or("?"),
            );
            depth = 1;
            crate::diag::task_dump::walk_apcs_frames(start_fp, 1024, |frame_lr, frame_fp| {
                let (n, l) = crate::diag::task_dump::fmt_pc_name(frame_lr);
                kprintln!(
                    "    #{:<2} frame={:#010x}  pc={:#010x}  {}",
                    depth, frame_fp, frame_lr,
                    core::str::from_utf8(&n[..l]).unwrap_or("?"),
                );
                depth += 1;
            });
        } else {
            kprintln!("    (could not locate a saved-FP near fault SP; chain unrecovered)");
        }
    }
    cpu::halt();
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

/// Common halt path for invariant-violation tripwires (iter-30+
/// instrumentation pass). Emits a uniform header, runs the per-
/// assertion local-context dump, runs `task_dump::dump()` for
/// scheduler/task state, then halts. Use for any check that should
/// stop the boot at the first 4-KiB-hypothesis violation rather
/// than chase the symptom downstream.
#[inline(never)]
fn halt_invariant(label: &str, local_dump: impl FnOnce()) -> ! {
    // A corrupted EL2 stack guard often *causes* the invariant we're
    // about to report (overflow clobbers state, that state then trips a
    // check). Surface it first so the root cause isn't buried under a
    // downstream symptom. `check_stack_guard` itself halts on mismatch.
    cpu::check_stack_guard();
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
    crate::diag::task_dump::dump();
    kprintln!("--- end task_dump ---");
    cpu::halt();
}

/// Halt-on-entry tripwire for `UnhandledException(char*, void*,
/// void(*)(void*))` (and the NonUserMode variant). The kernel calls
/// these when it can't dispatch an exception to any installed handler;
/// the caller passes the exception NAME as a C-string in r0. Catching
/// here is far cleaner than letting Reboot fire and decoding the
/// stack-passed string from a downstream caller.
///
/// `non_user` distinguishes the two variants (false ⇒ regular USR
/// path, true ⇒ kernel/UND path). Halts via `halt_invariant`.
pub(crate) fn handle_unhandled_exception(ctx: &TrapContext, non_user: bool) -> ! {
    let r0 = ctx.x[0] as u32;
    let r1 = ctx.x[1] as u32;
    let r2 = ctx.x[2] as u32;
    let r3 = ctx.x[3] as u32;
    let trampoline_saved_spsr = crate::hv::guest_endian::guest_read_u32_pa(UND_SAVE_SPSR_IPA).unwrap_or(0);
    let true_source_mode = trampoline_saved_spsr & 0x1F;
    let true_caller_lr = crate::arch::banked::lr_for_mode(ctx, trampoline_saved_spsr);
    let true_source_sp = crate::arch::banked::sp_for_mode(ctx, trampoline_saved_spsr);
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
            crate::arch::arm_decode::aarch32_mode_name(true_source_mode),
            true_source_mode, true_caller_lr, true_source_sp,
        );
        kprintln!("  exception data (r1) — first 8 words:");
        for i in 0..8 {
            let va = r1.wrapping_add(i * 4);
            match crate::hv::guest_endian::guest_read_u32_va(va) {
                Some(w) => kprintln!("    [{:+3}] @{:#010x} = {:#010x}", (i * 4) as i32, va, w),
                None    => kprintln!("    [{:+3}] @{:#010x} = (unmapped)", (i * 4) as i32, va),
            }
        }
    });
}

/// Diagnostic halt + register dump. Reached two ways:
///   1. The PABT vector slot (VA 0x0C) — patched to `HVC #Diag`
///      because the stock ROM's branch target is a missing REx
///      address. Any prefetch abort halts the host cleanly with a
///      full banked-register dump and stage-1 walk.
///   2. As the fallthrough from `handle_dabt_dispatch` for DABTs
///      with a non-forwardable DFSC.
///
/// Also available as an ad-hoc debugging facility: hand-patch
/// `HVC #Diag` into any guest code site to get a halt-with-dump
/// there.
pub(crate) fn handle_diag(ctx: &mut TrapContext) {
    let far = read_sysreg!("far_el1");
    let spsr_el2 = read_sysreg!("spsr_el2");
    let elr_el2 = read_sysreg!("elr_el2");
    let hvc_src_mode = (spsr_el2 as u32) & 0x1F;

    // Banked SPSRs are AArch64-named sysregs (FVP and QEMU both honour
    // them). For SPSR_svc, the architecturally-mapped AArch64 view is
    // SPSR_EL1 (DDI 0487 D13.2 — SPSR_EL1 bits[31:0] are mapped to
    // AArch32 SPSR_svc).
    let spsr_svc = read_sysreg!("spsr_el1");
    let spsr_abt = read_banked_spsr("abt");
    let spsr_und = read_banked_spsr("und");
    let spsr_irq = read_banked_spsr("irq");
    let spsr_fiq = read_banked_spsr("fiq");

    // HVC-source mode: whichever AArch32 mode was active when HVC
    // fired (typically ABT for the PABT-vector intercept and the
    // DABT-dispatch fallthrough). The "pre-abort" / "pre-fault" mode
    // is named by the matching banked SPSR (SPSR_abt for ABT-source).
    let mode_name = crate::arch::arm_decode::aarch32_mode_name(hvc_src_mode);

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
    // intercept and the DABT-dispatch fallthrough), the pre-abort mode
    // is named by SPSR_abt and the banked LR/SP for that mode comes
    // from its X-register slot. Hand-patched diagnostic sites in other
    // modes use the matching SPSR.
    let (spsr_src, lr_src) = match hvc_src_mode {
        crate::arch::banked::MODE_UND => (spsr_und as u32, lr_und),
        crate::arch::banked::MODE_ABT => (spsr_abt as u32, lr_abt),
        _ => (spsr_el2 as u32, ctx.x[14] as u32),
    };
    let pre_mode = spsr_src & 0x1F;
    let pre_lr = crate::arch::banked::lr_for_mode(ctx, spsr_src);
    let pre_sp = crate::arch::banked::sp_for_mode(ctx, spsr_src);
    let thumb = (spsr_src & (1 << 5)) != 0;
    // Faulting PC adjustment: ARM DABT = LR-8, ARM PABT = LR-4,
    // Thumb DABT = LR-4, Thumb PABT = LR-2. Assume PABT-source — true
    // for the PABT vector intercept (patched in
    // `guest_mem::patch_dabt_vector`) and for hand-patched diagnostic
    // sites. When `handle_dabt_dispatch` delegates here for a non-
    // forwardable DABT the formula underestimates the faulting PC by
    // 4 bytes (ARM) or 2 bytes (Thumb); the FAR / ESR / banked
    // register dump still pins the fault location.
    let faulting_pc = if thumb { lr_src.wrapping_sub(2) & !1 } else { lr_src.wrapping_sub(4) };
    let insn = crate::hv::guest_endian::guest_read_u32_pa(faulting_pc & !3).unwrap_or(0xDEAD_BEEF);
    kprintln!(
        "  HVC source mode = {:#x} ({}); pre-fault mode (from SPSR_<src>) = {:#x} ({}), T={}",
        hvc_src_mode, mode_name,
        pre_mode, crate::arch::arm_decode::aarch32_mode_name(pre_mode), thumb as u32
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
    let lr_abt_save = crate::hv::guest_endian::guest_read_u32_pa(crate::newton::guest_trampolines::DABT_SAVE_PA).unwrap_or(0);
    let sp_abt_save = crate::hv::guest_endian::guest_read_u32_pa(crate::newton::guest_trampolines::DABT_SAVE_PA + 4).unwrap_or(0);
    let spsr_abt_save = crate::hv::guest_endian::guest_read_u32_pa(crate::newton::guest_trampolines::DABT_SAVE_PA + 8).unwrap_or(0);
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
        let pa_opt = guest_mem::translate_va(va);
        if pa_opt.is_none() { continue; }
        let pa = pa_opt.unwrap();
        let w = match crate::hv::guest_endian::guest_read_u32_pa(pa) {
            Some(x) => x, None => continue,
        };
        let tgt = w & !1;
        if tgt == 0 || tgt >= 0x0100_0000 { continue; }
        if tgt & 3 != 0 { continue; }
        if let Some(prev) = crate::hv::guest_endian::guest_read_u32_pa(tgt.wrapping_sub(4)) {
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
