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

/// `gObjectTable` is a TObjectTable instance at this VA. We saw it as
/// `r0=0x0c10fc34` in the trace for `TObjectTable::Get`.
///
/// Layout (from `TObjectTable::Init` + `Get`):
///   +0x00       static handler / vtable ptr (set in Init)
///   +0x0C       zeroed
///   +0x10..+0x10+127*4   hash bucket heads (128 buckets of TKernelObject*)
///
/// TKernelObject node layout (from `Get`):
///   +0x00       id (lookup key, also the task/object ID)
///   +0x04       next-in-hash-chain pointer
///   ... rest depends on KernelType
///
/// ID encoding (from `NewId`):
///   bits[3:0]   KernelType — full kernel-side mapping for 717006:
///                 0x2 = Port,  0x3 = Task,    0x4 = Env,    0x5 = Domain,
///                 0x6 = SemL,  0x7 = SemG,    0x8 = SMem,   0x9 = SMsg,
///                 0xa = Mon,   0xb = Phys.
///               Note: this is the user-side ObjectTypes enum (DDK
///               `KernelTypes.h`) plus 2 — see docs/STRUCTURES.md
///               "Kernel object IDs" for citations.
///   bits[31:4]  per-type sequence number (NextGlobalUniqueId)
/// Hash bucket index = (id >> 4) & 0x7F
const G_OBJECT_TABLE:  u32 = 0x0c10_fc34;
const OT_BUCKETS_BASE: u32 = 0x10;
const OT_NUM_BUCKETS:  u32 = 128;
const OBJ_TYPE_TASK:   u32 = 3;
const OBJ_TYPE_PORT:   u32 = 2;
const OBJ_TYPE_MONITOR:u32 = 0xa;

/// 4-byte name for each KernelType bucket. Indexed by `id & 0xF`.
/// Buckets we haven't seen used (0, 1, 12..15) are blank.
pub const KIND_NAMES: [&str; 16] = [
    "----", "----", "Port", "Task", "Env ", "Dom ", "SemL", "SemG",
    "SMem", "SMsg", "Mon ", "Phys", "----", "----", "----", "----",
];

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

/// Determine task state from the (gCurrentTask, run-queue presence,
/// wait-queue link presence) signals.
///
/// - "RUN"   task is gCurrentTask
/// - "RDY"   task has run-queue links set (TTaskQItem.next/prev non-zero)
/// - "WAIT"  task has either of the embedded TDoubleQItem links set
///           (offsets +0xbc, +0xc8)
/// - "BLK"   none of the above — task is alive but not in any link we
///           understand. Most likely waiting on a message-port or
///           shared-mem queue tracked elsewhere (e.g. waiter list owned
///           by the port). To distinguish from "freshly-Suspended"
///           we'd need to identify the per-port waiter-list field —
///           TODO.
fn task_state(task_va: u32, current: u32) -> &'static str {
    if task_va == current {
        return "RUN";
    }
    let qnext = rd(task_va + TT_QITEM).unwrap_or(0);
    let qprev = rd(task_va + TT_QITEM + 4).unwrap_or(0);
    let wq1n  = rd(task_va + 0xbc).unwrap_or(0);
    let wq2n  = rd(task_va + 0xc8).unwrap_or(0);
    if wq1n != 0 || wq2n != 0 { return "WAIT"; }
    if qnext != 0 || qprev != 0 { return "RDY"; }
    "BLK"
}

/// Walk the object table and dump every TASK entry. Use when we want
/// to see the population of tasks beyond the run queue.
fn dump_object_table_tasks(current: u32) {
    let mut total: u32 = 0;
    let mut tasks: u32 = 0;
    let mut by_type: [u32; 16] = [0; 16];
    for bucket in 0..OT_NUM_BUCKETS {
        let head_va = G_OBJECT_TABLE + OT_BUCKETS_BASE + bucket * 4;
        let mut node = match rd(head_va) {
            Some(v) => v,
            None => continue,
        };
        let mut steps = 0u32;
        while node != 0 && steps < 128 {
            total += 1;
            let id = match rd(node) {
                Some(v) => v,
                None => break,
            };
            let kind = id & 0xF;
            by_type[kind as usize] += 1;
            if kind == OBJ_TYPE_TASK {
                tasks += 1;
                let state = task_state(node, current);
                let prio = rd(node + TT_PRIORITY).unwrap_or(u32::MAX);
                let globals = rd(node + TT_GLOBALS).unwrap_or(u32::MAX);
                let qnext = rd(node + TT_QITEM).unwrap_or(0);
                let qprev = rd(node + TT_QITEM + 4).unwrap_or(0);
                let wq1n  = rd(node + 0xbc).unwrap_or(0);
                let wq1p  = rd(node + 0xc0).unwrap_or(0);
                let wq2n  = rd(node + 0xc8).unwrap_or(0);
                let wq2p  = rd(node + 0xcc).unwrap_or(0);
                let name  = find_task_name(globals);
                let (n0, n1, n2, n3) = match name {
                    Some((_, v)) => ((v>>24) as u8, (v>>16) as u8, (v>>8) as u8, v as u8),
                    None         => (b'?', b'?', b'?', b'?'),
                };
                kprintln!(
                    "  [{}] task {:#010x} id={:#x} prio={} name='{}{}{}{}' q={:#010x}/{:#010x} wq1={:#010x}/{:#010x} wq2={:#010x}/{:#010x}",
                    state, node, id, prio,
                    n0 as char, n1 as char, n2 as char, n3 as char,
                    qnext, qprev, wq1n, wq1p, wq2n, wq2p,
                );
            }
            node = match rd(node + 4) {
                Some(v) => v,
                None => break,
            };
            steps += 1;
        }
    }
    kprintln!(
        "  object table: {} tasks (of {} kernel objects)",
        tasks, total,
    );
    kprintln!(
        "    Port={} Task={} Env={} Dom={} SemL={} SemG={} SMem={} SMsg={} Mon={} Phys={} (others: t0={} t1={} t12={} t13={} t14={} t15={})",
        by_type[2], by_type[3], by_type[4], by_type[5],
        by_type[6], by_type[7], by_type[8], by_type[9],
        by_type[10], by_type[11],
        by_type[0], by_type[1], by_type[12], by_type[13], by_type[14], by_type[15],
    );
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
        // Print the first word at the task pointer — TKernelObject::id
        // (per TObjectTable::Add `str r0, [r4]`). Tells us the ID and
        // therefore the encoded KernelType in bits[3:0].
        let id_word = rd(curr).unwrap_or(u32::MAX);
        kprintln!("  curr task->[0] = {:#x}  (low nibble = type, high = seq)", id_word);
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

    kprintln!("  all tasks (object table walk):");
    dump_object_table_tasks(curr);
}

/// Dump the SWIBoot context-save area of `task_va` — the 21-word
/// region at +0x10..+0x54 the SVC scheduler reads at 0x3ad9a4..0x3ad9c4
/// to ERET back into the task. Layout (citations: 0x3ad8cc..0x3ad8dc
/// for the save side, 0x3ad9a4..0x3ad9c4 + 0x3ada6c for the restore /
/// movs-pc-lr side):
///
///   +0x10 r0  +0x14 r1  +0x18 r2  +0x1c r3
///   +0x20 r4  +0x24 r5  +0x28 r6  +0x2c r7
///   +0x30 r8  +0x34 r9  +0x38 sl  +0x3c fp
///   +0x40 ip
///   +0x44 sp_usr   +0x48 lr_usr
///   +0x4c saved-pc (LR_svc at SWI tail; becomes target of `movs pc, lr`)
///   +0x50 saved-SPSR (CPSR to restore via `msr SPSR_fc` then `movs`)
pub fn dump_save_area(label: &str, task_va: u32) {
    let id   = rd(task_va).unwrap_or(u32::MAX);
    let glob = rd(task_va + TT_GLOBALS).unwrap_or(u32::MAX);
    let name = find_task_name(glob);
    let (n0, n1, n2, n3) = match name {
        Some((_, v)) => ((v>>24) as u8, (v>>16) as u8, (v>>8) as u8, v as u8),
        None         => (b'?', b'?', b'?', b'?'),
    };
    kprintln!(
        "  save-area [{}] task={:#010x} id={:#x} name='{}{}{}{}':",
        label, task_va, id, n0 as char, n1 as char, n2 as char, n3 as char,
    );
    let names = [
        "r0 ", "r1 ", "r2 ", "r3 ",
        "r4 ", "r5 ", "r6 ", "r7 ",
        "r8 ", "r9 ", "sl ", "fp ",
        "ip ",
        "sp_usr", "lr_usr",
        "PC ", "SPSR",
    ];
    for (i, lab) in names.iter().enumerate() {
        let off = 0x10 + (i as u32) * 4;
        let va  = task_va + off;
        let v   = rd(va).unwrap_or(u32::MAX);
        kprintln!("    +{:#04x} {:6} = {:#010x}", off, lab, v);
    }
    // Also dump 64 words spanning sp_usr ± 32. The faulting site is
    // typically a stack load just past sp_usr; corruption can extend
    // beyond the immediate window, so widen to see the boundary.
    let sp_usr = rd(task_va + 0x44).unwrap_or(0);
    if sp_usr != 0 && sp_usr != u32::MAX {
        kprintln!("    user stack window @ sp_usr={:#010x} (±0x80):", sp_usr);
        for i in 0..32i32 {
            let off = (i - 8) * 4;
            let va = sp_usr.wrapping_add(off as u32);
            let v  = rd(va).unwrap_or(u32::MAX);
            let mark = if off == 0 { " <- sp" } else { "" };
            kprintln!("      [{:+4}] {:#010x} = {:#010x}{}", off, va, v, mark);
        }
        kprintln!("    stage-1 walk for sp_usr:");
        crate::guest_mem::dump_stage1_walk(sp_usr);
        // Also walk a few aliasing-suspect VAs: any AEInstallHandler
        // we registered with class/signal pairs lands signal at +8 and
        // class at +12 of its TAEventHandler. If our user stack at
        // sp_usr+8/+12 contains 'newt'/'cdsv', the suspect handler is
        // at VA 0x0c602e2c (per trace 183155). If those VAs walk to
        // the same PA as sp_usr → confirmed stage-1 alias.
        kprintln!("    stage-1 walk for 0x0c602e2c (suspected alias):");
        crate::guest_mem::dump_stage1_walk(0x0c602e2c);
    }
}

/// One-shot diagnostic: dump the SWIBoot save area for every task
/// in the object table whose fTaskName matches `name_match` (4-char
/// ASCII; `?` = wildcard byte). Plus the current task. Useful when
/// chasing per-task corruption: at the moment the "newt" DABT fires
/// we want to see all tasks named 'cdsv'.
pub fn dump_save_area_for_named(name_match: &[u8; 4]) {
    let curr = rd(G_CURRENT_TASK).unwrap_or(0);
    if curr != 0 {
        dump_save_area("CURR", curr);
    }
    for bucket in 0..OT_NUM_BUCKETS {
        let head_va = G_OBJECT_TABLE + OT_BUCKETS_BASE + bucket * 4;
        let mut node = match rd(head_va) {
            Some(v) => v,
            None => continue,
        };
        let mut steps = 0u32;
        while node != 0 && steps < 128 {
            let id = match rd(node) { Some(v) => v, None => break };
            if (id & 0xF) == OBJ_TYPE_TASK {
                let glob = rd(node + TT_GLOBALS).unwrap_or(u32::MAX);
                if let Some((_, v)) = find_task_name(glob) {
                    let bytes = [(v>>24) as u8, (v>>16) as u8, (v>>8) as u8, v as u8];
                    let mut hit = true;
                    for i in 0..4 {
                        if name_match[i] != b'?' && name_match[i] != bytes[i] {
                            hit = false;
                            break;
                        }
                    }
                    if hit && node != curr {
                        dump_save_area("OBJ ", node);
                    }
                }
            }
            node = match rd(node + 4) { Some(v) => v, None => break };
            steps += 1;
        }
    }
}

/// Walk a TDoubleQContainer at `qc_va` and call `f(entry_va)` for each
/// entry, up to `max` iterations. Returns the number of entries
/// observed, or `None` if the container itself is unreadable.
///
/// Layout (see docs/STRUCTURES.md):
///   +0x00 head        — first entry's TDoubleQItem (or 0)
///   +0x04 tail        — last entry's qitem
///   +0x08 link_offset — offset of the qitem within each entry
///
/// Each TDoubleQItem is { next, prev, container } with a 4-word stride.
pub fn walk_dqc<F: FnMut(u32)>(qc_va: u32, max: usize, mut f: F) -> Option<usize> {
    let head      = rd(qc_va)?;
    let link_off  = rd(qc_va + 0x08)?;
    if head == 0 {
        return Some(0);
    }
    let mut count = 0usize;
    let mut qitem = head;
    while qitem != 0 && count < max {
        // entry pointer = qitem - link_offset
        let entry = qitem.wrapping_sub(link_off);
        f(entry);
        count += 1;
        let next = match rd(qitem) { Some(v) => v, None => break };
        if next == qitem { break; }
        qitem = next;
    }
    Some(count)
}

/// Print a one-line summary of a TDoubleQContainer at `qc_va`. Useful
/// when called against a port's pending or waiter queue.
pub fn dump_dqc_summary(label: &str, qc_va: u32) {
    let head      = rd(qc_va).unwrap_or(u32::MAX);
    let tail      = rd(qc_va + 0x04).unwrap_or(u32::MAX);
    let link_off  = rd(qc_va + 0x08).unwrap_or(u32::MAX);
    let cb        = rd(qc_va + 0x0c).unwrap_or(u32::MAX);
    let client    = rd(qc_va + 0x10).unwrap_or(u32::MAX);
    let count = walk_dqc(qc_va, 64, |_| {}).unwrap_or(usize::MAX);
    kprintln!(
        "  dqc[{}] @{:#x}: head={:#x} tail={:#x} link_off={} cb={:#x} client={:#x} count={}",
        label, qc_va, head, tail, link_off, cb, client, count
    );
}

/// Find the task with id `task_id` in `gObjectTable` and return
/// (TTask*, name fourcc) if found. Used when chasing a msg back to
/// its sender or receiver.
fn find_task_by_id(task_id: u32) -> Option<(u32, [u8; 4])> {
    if task_id == 0 || task_id == u32::MAX || (task_id & 0xF) != OBJ_TYPE_TASK {
        return None;
    }
    let bucket = (task_id >> 4) & 0x7F;
    let head_va = G_OBJECT_TABLE + OT_BUCKETS_BASE + bucket * 4;
    let mut node = rd(head_va)?;
    let mut steps = 0u32;
    while node != 0 && steps < 128 {
        let id = rd(node)?;
        if id == task_id {
            let glob = rd(node + TT_GLOBALS).unwrap_or(0);
            let name = match find_task_name(glob) {
                Some((_, v)) => [(v>>24) as u8, (v>>16) as u8, (v>>8) as u8, v as u8],
                None         => *b"????",
            };
            return Some((node, name));
        }
        node = rd(node + 4)?;
        steps += 1;
    }
    None
}

/// Print a TSharedMemMsg at `msg_va`. Resolves the receiver and
/// sender tasks if their IDs are populated. See docs/STRUCTURES.md
/// "TSharedMemMsg" for the field layout.
pub fn dump_msg(label: &str, msg_va: u32) {
    let id          = rd(msg_va).unwrap_or(u32::MAX);
    let state44     = rd(msg_va + 0x44).unwrap_or(u32::MAX);
    let flags50     = rd(msg_va + 0x50).unwrap_or(u32::MAX);
    let filter54    = rd(msg_va + 0x54).unwrap_or(u32::MAX);
    let parked6c    = rd(msg_va + 0x6c).unwrap_or(u32::MAX);
    let recv_id     = rd(msg_va + 0x70).unwrap_or(u32::MAX);
    let sender_id   = rd(msg_va + 0x7c).unwrap_or(u32::MAX);
    let kind = (id & 0xF) as usize;

    let recv_str = match find_task_by_id(recv_id) {
        Some((_, n)) => n,
        None => *b"----",
    };
    let send_str = match find_task_by_id(sender_id) {
        Some((_, n)) => n,
        None => *b"----",
    };
    kprintln!(
        "  msg[{}] @{:#x} id={:#x}({}) state={:#x} flags={:#x} filter={:#x} parked_on={:#x}({}) recv={:#x}({}{}{}{}) send={:#x}({}{}{}{})",
        label, msg_va, id, KIND_NAMES[kind], state44, flags50, filter54,
        parked6c, KIND_NAMES[(parked6c & 0xF) as usize],
        recv_id, recv_str[0] as char, recv_str[1] as char, recv_str[2] as char, recv_str[3] as char,
        sender_id, send_str[0] as char, send_str[1] as char, send_str[2] as char, send_str[3] as char,
    );
}

/// Dump a TPort: its id, both queue summaries, and resolve each
/// waiter back to its receiving task. See docs/STRUCTURES.md
/// "How ports track waiters" for the layout citations.
pub fn dump_port(port_va: u32) {
    let id = rd(port_va).unwrap_or(u32::MAX);
    kprintln!("port @{:#x} id={:#x}({})", port_va, id, KIND_NAMES[(id & 0xF) as usize]);
    dump_dqc_summary("pending", port_va + 0x10);
    dump_dqc_summary("waiters", port_va + 0x24);

    // Resolve each waiter to a (msg, receiving-task) pair.
    let _ = walk_dqc(port_va + 0x24, 32, |msg_va| {
        dump_msg("waiter", msg_va);
    });
}

/// Walk gObjectTable and call `f(obj_va, id)` for each entry whose
/// `(id & 0xF) == kind`. Stops after `MAX_PER_BUCKET` per bucket as a
/// safety net against corrupted hash chains.
fn for_each_object_of_kind<F: FnMut(u32, u32)>(kind: u32, mut f: F) {
    const MAX_PER_BUCKET: u32 = 128;
    for bucket in 0..OT_NUM_BUCKETS {
        let head_va = G_OBJECT_TABLE + OT_BUCKETS_BASE + bucket * 4;
        let mut node = match rd(head_va) { Some(v) => v, None => continue };
        let mut steps = 0u32;
        while node != 0 && steps < MAX_PER_BUCKET {
            let id = match rd(node) { Some(v) => v, None => break };
            if (id & 0xF) == kind {
                f(node, id);
            }
            node = match rd(node + 4) { Some(v) => v, None => break };
            steps += 1;
        }
    }
}

/// Dump a TMonitor: id, depth, state flags, and resolve each blocked
/// task in its waiter queue (link_offset 0xc8 = TTask.wq_link_2).
/// See docs/STRUCTURES.md "TMonitor" for the layout citations.
pub fn dump_monitor(mon_va: u32) {
    let id     = rd(mon_va).unwrap_or(u32::MAX);
    let owner8 = rd(mon_va + 0x08).unwrap_or(u32::MAX);
    let depth  = rd(mon_va + 0x10).unwrap_or(u32::MAX);
    let state  = rd(mon_va + 0x14).unwrap_or(u32::MAX);
    kprintln!(
        "monitor @{:#x} id={:#x}({}) ownerOrEnv={:#x} depth={} state={:#x}",
        mon_va, id, KIND_NAMES[(id & 0xF) as usize], owner8, depth, state
    );
    dump_dqc_summary("waiters", mon_va + 0x24);

    // Each entry is a TTask*; print one line per task.
    let _ = walk_dqc(mon_va + 0x24, 32, |task_va| {
        let tid = rd(task_va).unwrap_or(u32::MAX);
        let glob = rd(task_va + TT_GLOBALS).unwrap_or(0);
        let (a, b, c, d) = match find_task_name(glob) {
            Some((_, v)) => ((v>>24) as u8, (v>>16) as u8, (v>>8) as u8, v as u8),
            None => (b'?', b'?', b'?', b'?'),
        };
        kprintln!(
            "  blocked task @{:#x} id={:#x} name={}{}{}{}",
            task_va, tid, a as char, b as char, c as char, d as char,
        );
    });
}

/// Dump every TMonitor in `gObjectTable` with its waiter list.
pub fn dump_all_monitors() {
    kprintln!("=== all monitors (KernelType=10) ===");
    let mut count = 0u32;
    for_each_object_of_kind(OBJ_TYPE_MONITOR, |va, _id| {
        dump_monitor(va);
        count += 1;
    });
    kprintln!("=== {} monitors total ===", count);
}

/// Dump a single object by id. Routes to the appropriate per-type
/// dumper based on the low 4 bits.
pub fn dump_object_by_id(id: u32) {
    let kind = (id & 0xF) as usize;
    let bucket = (id >> 4) & 0x7F;
    let head_va = G_OBJECT_TABLE + OT_BUCKETS_BASE + bucket * 4;
    let mut node = match rd(head_va) {
        Some(v) => v,
        None => { kprintln!("dump_object_by_id({:#x}): bucket head unreadable", id); return; }
    };
    let mut steps = 0u32;
    while node != 0 && steps < 128 {
        let nid = match rd(node) { Some(v) => v, None => break };
        if nid == id {
            match kind as u32 {
                OBJ_TYPE_PORT    => dump_port(node),
                OBJ_TYPE_TASK    => dump_task_one_line(node),
                OBJ_TYPE_MONITOR => dump_monitor(node),
                k => kprintln!(
                    "  obj @{:#x} id={:#x} kind={}({}) — no per-type dumper yet",
                    node, nid, k, KIND_NAMES[(k & 0xF) as usize]
                ),
            }
            return;
        }
        node = match rd(node + 4) { Some(v) => v, None => break };
        steps += 1;
    }
    kprintln!("dump_object_by_id({:#x}): id not found in bucket {}", id, bucket);
}

/// Full kernel-state dump for diagnostics. Combines the existing
/// `dump()` (scheduler + run queues + object table summary) with
/// per-port and per-monitor walks. Intended for one-shot triggers
/// (HVC, recursive-newt path), not the periodic timer.
pub fn dump_full() {
    kprintln!("=== kdump::dump_full ===");
    dump();
    dump_all_ports();
    dump_all_monitors();
    kprintln!("=== kdump::dump_full end ===");
}

/// Dump every TPort in `gObjectTable` along with its queue contents.
pub fn dump_all_ports() {
    kprintln!("=== all ports (KernelType=2) ===");
    let mut count = 0u32;
    for_each_object_of_kind(OBJ_TYPE_PORT, |va, _id| {
        dump_port(va);
        count += 1;
    });
    kprintln!("=== {} ports total ===", count);
}

/// Heartbeat-rate dump trigger. Returns true on the firing iterations.
pub fn periodic() -> bool {
    static mut COUNT: u64 = 0;
    let n = unsafe {
        COUNT = COUNT.wrapping_add(1);
        COUNT
    };
    // Roughly every 256 heartbeats × 16 ms ≈ 4 s. The object-table walk
    // touches up to 128 buckets through the stage-1 walker, so keeping
    // this slow keeps UART noise under control.
    if n % 256 == 0 {
        dump();
        true
    } else {
        false
    }
}
