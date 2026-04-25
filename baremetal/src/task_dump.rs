//! Walk the Newton kernel scheduler state and dump every task in the
//! run queues plus the currently-running task. Reads guest RAM through
//! the live stage-1 walker so the dump reflects the kernel's view at
//! the moment of the call.
//!
//! Layout discovered from the 717006 ROM disassembly (`docs/DISASM.md`):
//!
//! Globals (kernel data segment, fixed VAs):
//!   0x0c100fd0  → TScheduler*    (gScheduler)
//!   0x0c101000  → TTask*         (gCurrentTask)
//!   0x0c100fd4  → ULong          (gWantSchedule flag)
//!   0x0c100fd8  → ULong          (gHoldSchedule count)
//!   0x0c10105c  → void*          (gCurrentGlobals — see UserTasks.h)
//!
//! TScheduler (ROM `Scheduler` / `TScheduler::Schedule` / `Add`):
//!   +0x14  (20)   ULong  highest non-empty priority
//!   +0x18  (24)   ULong  priority bitmap (bit p set ⇒ queue p non-empty)
//!   +0x1c  (28)   TTaskQueue[32]  per-priority run queues, 8 bytes each
//!                                 (head = +0, tail = +4)
//!   +0x11c (284)  TTask* last RemoveHighestPriority result (one-shot)
//!
//! TTask (ROM `__ct__5TTaskFv` + `TScheduler::Add`):
//!   +0x80  (128)  ULong  priority (used as bucket index)
//!   +0x94  (148)  TTaskQItem (40 bytes) — run-queue link
//!                  +0x00 next_task_ptr
//!                  +0x04 prev_task_ptr (probably; verified empirically)
//!   +0xa0  (160)  void*  per-task globals base
//!   +0xbc  (188)  TDoubleQItem (12 bytes) — wait-queue link 1
//!   +0xc8  (200)  TDoubleQItem (12 bytes) — wait-queue link 2

use crate::guest_mem::read_word_va;
use crate::kprintln;

const G_SCHEDULER_PTR: u32 = 0x0c10_0fd0;
const G_CURRENT_TASK:  u32 = 0x0c10_1000;
const G_WANT_SCHED:    u32 = 0x0c10_0fd4;
const G_HOLD_SCHED:    u32 = 0x0c10_0fd8;
const G_CURRENT_GLOB:  u32 = 0x0c10_105c;

const TS_HIGHEST_PRI:  u32 = 0x14;
const TS_PRI_BITMAP:   u32 = 0x18;
const TS_QUEUES_BASE:  u32 = 0x1c; // 32 * 8 bytes
const TS_LAST_REMOVED: u32 = 0x11c;

const TT_PRIORITY:     u32 = 0x80;
const TT_QITEM:        u32 = 0x94; // TTaskQItem, 40 bytes
const TT_GLOBALS:      u32 = 0xa0;

/// Maximum tasks to walk per priority queue before bailing — guards
/// against loops in case our offset assumptions are wrong.
const MAX_QUEUE_WALK: usize = 32;

fn rd(va: u32) -> Option<u32> {
    read_word_va(va)
}

fn walk_queue(prio: u32, queue_va: u32) {
    let head = match rd(queue_va) {
        Some(v) => v,
        None => {
            kprintln!("  prio {} queue head VA {:#x} unreadable", prio, queue_va);
            return;
        }
    };
    if head == 0 {
        kprintln!("  prio {} queue head=NULL (bitmap inconsistency?)", prio);
        return;
    }
    let mut cur = head;
    let mut steps = 0usize;
    while cur != 0 && steps < MAX_QUEUE_WALK {
        dump_task_one_line(cur);
        let next = rd(cur + TT_QITEM).unwrap_or(0);
        if next == cur {
            break;
        }
        cur = next;
        steps += 1;
    }
    if steps >= MAX_QUEUE_WALK {
        kprintln!("  prio {} queue walk hit MAX_QUEUE_WALK at {:#x}", prio, cur);
    }
}

/// Search backward from `globals_va` for a printable fourcc tag —
/// STaskSwitchedGlobals.fTaskName lives at +76 from the struct base,
/// and TaskSwitchedGlobals() returns globals_ptr − sizeof(struct), so
/// the name is somewhere just below `globals_va`. Scan the 128 bytes
/// below it for a printable 4-char tag and return the offset + value
/// of the first hit (None if none found).
fn find_task_name(globals_va: u32) -> Option<(i32, u32)> {
    if globals_va == 0 || globals_va == u32::MAX {
        return None;
    }
    for off in (4..=128i32).step_by(4) {
        let addr = globals_va.wrapping_sub(off as u32);
        let v = match rd(addr) { Some(x) => x, None => continue };
        let bytes = [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8];
        if bytes.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            // Skip pure-space / pure-digit fourccs (likely false positives).
            let alpha = bytes.iter().filter(|&&b| b.is_ascii_alphabetic()).count();
            if alpha >= 2 {
                return Some((-off, v));
            }
        }
    }
    None
}

fn dump_task_one_line(task_va: u32) {
    let prio_field = rd(task_va + TT_PRIORITY).unwrap_or(u32::MAX);
    let globals    = rd(task_va + TT_GLOBALS).unwrap_or(u32::MAX);
    let qnext      = rd(task_va + TT_QITEM).unwrap_or(u32::MAX);
    let qprev      = rd(task_va + TT_QITEM + 4).unwrap_or(u32::MAX);
    let wq1_next   = rd(task_va + 0xbc).unwrap_or(u32::MAX);
    let wq1_prev   = rd(task_va + 0xc0).unwrap_or(u32::MAX);
    let wq2_next   = rd(task_va + 0xc8).unwrap_or(u32::MAX);
    let wq2_prev   = rd(task_va + 0xcc).unwrap_or(u32::MAX);
    let stack_bot  = rd(task_va + 0x8c).unwrap_or(u32::MAX);
    let saved_pc   = rd(task_va + 0x70).unwrap_or(u32::MAX); // first guess: saved PC
    let name_info  = find_task_name(globals);
    let name_str = match name_info {
        Some((off, val)) => {
            let bytes = [(val >> 24) as u8, (val >> 16) as u8, (val >> 8) as u8, val as u8];
            // Print prefix + name + offset
            kprintln!(
                "  task {:#010x} prio={} name={}{}{}{}{} (glob{:+}) globals={:#010x} q={:#010x}/{:#010x} stk_bot={:#010x} savedPC?={:#010x} wq1={:#010x}/{:#010x} wq2={:#010x}/{:#010x}",
                task_va, prio_field,
                bytes[0] as char, bytes[1] as char, bytes[2] as char, bytes[3] as char, "",
                off, globals, qnext, qprev, stack_bot, saved_pc,
                wq1_next, wq1_prev, wq2_next, wq2_prev
            );
            return;
        }
        None => "?",
    };
    kprintln!(
        "  task {:#010x} prio={} name={} globals={:#010x} q={:#010x}/{:#010x} stk_bot={:#010x} savedPC?={:#010x} wq1={:#010x}/{:#010x} wq2={:#010x}/{:#010x}",
        task_va, prio_field, name_str, globals, qnext, qprev, stack_bot, saved_pc,
        wq1_next, wq1_prev, wq2_next, wq2_prev
    );
}

/// Peek the top-of-SVC-stack words at the wedge — useful to see the
/// most-recently-pushed return addresses when we suspect a tight
/// loop. Reads through the guest stage-1 walker so it resolves the
/// VA to the actual physical kernel-stack page.
fn dump_svc_stack(sp_va: u32, words: usize) {
    if sp_va == 0 || sp_va == u32::MAX { return; }
    kprintln!("  SVC stack @ {:#x} (top {} words):", sp_va, words);
    for i in 0..words {
        let va = sp_va.wrapping_add((i * 4) as u32);
        match rd(va) {
            Some(v) => kprintln!("    [{:+3}] {:#010x} = {:#010x}", i*4, va, v),
            None    => kprintln!("    [{:+3}] {:#010x} = <unmapped>", i*4, va),
        }
    }
}

/// Read SP_EL1 / ELR_EL1 (= AArch32 SP_svc / LR_svc) directly. These
/// are flaky on QEMU raspi3b from EL2 IRQ context but reliable on
/// FVP — useful as a cross-check against the snapshot save's
/// ctx.x[13] / x[14] (which read whatever active mode's banked
/// register we trapped from).
fn read_sp_lr_svc() -> (u32, u32) {
    let sp: u64;
    let lr: u64;
    unsafe {
        core::arch::asm!("mrs {}, sp_el1", out(reg) sp,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, elr_el1", out(reg) lr,
            options(nomem, nostack, preserves_flags));
    }
    (sp as u32, lr as u32)
}

/// Top-level dump entry. Called from `trap_irq` periodically.
pub fn dump() {
    let sched = match rd(G_SCHEDULER_PTR) {
        Some(v) if v != 0 => v,
        _ => {
            kprintln!("task_dump: gScheduler unset");
            return;
        }
    };
    let curr = rd(G_CURRENT_TASK).unwrap_or(0);
    let want = rd(G_WANT_SCHED).unwrap_or(u32::MAX);
    let hold = rd(G_HOLD_SCHED).unwrap_or(u32::MAX);
    let glob = rd(G_CURRENT_GLOB).unwrap_or(u32::MAX);
    let highest = rd(sched + TS_HIGHEST_PRI).unwrap_or(u32::MAX);
    let bitmap  = rd(sched + TS_PRI_BITMAP).unwrap_or(u32::MAX);
    let last    = rd(sched + TS_LAST_REMOVED).unwrap_or(u32::MAX);

    kprintln!(
        "task_dump: gSched={:#x} curr={:#x} highest_pri={} bitmap={:#x} last_rem={:#x} want={} hold={} curr_glob={:#x}",
        sched, curr, highest, bitmap, last, want, hold, glob
    );

    if curr != 0 {
        kprintln!("  current:");
        dump_task_one_line(curr);
        // Read SP_EL1 / ELR_EL1 directly from EL2 — flaky on QEMU but
        // useful when not. If non-zero, dump the top of the SVC stack
        // so we can see the recent call-frame chain.
        let (sp_svc, lr_svc) = read_sp_lr_svc();
        kprintln!("  SP_EL1={:#x} ELR_EL1={:#x}", sp_svc, lr_svc);
        if sp_svc != 0 {
            dump_svc_stack(sp_svc, 12);
        }
    }

    if bitmap != u32::MAX {
        for p in 0..32u32 {
            if (bitmap >> p) & 1 != 0 {
                let qva = sched + TS_QUEUES_BASE + p * 8;
                kprintln!("  prio {} queue@{:#x}:", p, qva);
                walk_queue(p, qva);
            }
        }
    }
}

/// Heartbeat-rate dump trigger. Returns true on the firing iterations.
pub fn periodic() -> bool {
    static mut COUNT: u64 = 0;
    let n = unsafe {
        COUNT = COUNT.wrapping_add(1);
        COUNT
    };
    // Roughly every 64 heartbeats × 16 ms ≈ 1 s.
    if n % 64 == 0 {
        dump();
        true
    } else {
        false
    }
}
