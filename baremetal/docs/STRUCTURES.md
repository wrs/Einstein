# Newton 717006 ROM kernel data structures

Working catalog of kernel data structure layouts inferred from the
ROM disassembly during Phase B debugging. All offsets are bytes
unless otherwise noted. Values prefixed with `0x` are absolute VAs in
the kernel's address space (typically the `0x0c000000+` window).

> **Convention.** Where a header or DDK reference exists, we cite it.
> Where we inferred from the binary, we cite the ROM PC + assembly
> snippet that established the offset. **Do not trust an offset
> without a citation; the kernel was built with a private build of
> CFront/MPW C++ in 1996 and offsets often disagree with the public
> DDK headers (e.g., the OS600 `KernelTypes.h` enum order is wrong
> for 717006 — see "Kernel object IDs" below).**
>
> Layouts are written in C-header form for clarity; treat them as
> commented annotations, not authoritative `#include`-able sources.

---

## Kernel globals (at fixed VAs)

```c
0x0c100fc4   TTask*           gPriorScheduledTask;   // *r5 in Schedule
0x0c100fd0   TScheduler*      gScheduler;            // ptr to TScheduler
0x0c100fd4   ULong            gWantSchedule;         // bool: scheduler tick pending
0x0c100fd8   ULong            gHoldScheduleCount;    // depth of HoldSchedule
0x0c100fe4   ULong            gWantScheduleAfter;    // ?
0x0c100ff8   TTask*           gNewlyScheduledTask;   // ?
0x0c100ffc   TTask*           gIdleTask;             // ?
0x0c101000   TTask*           gCurrentTask;          // currently running
0x0c101008   ULong            gSomeBool;             // ?
0x0c101054   ULong            gCurrentTaskId;        // copied from task->[0]
0x0c101058   void*            gCurrentDomainSomething; // copied from task->[0xd8]
0x0c10105c   void*            gCurrentGlobals;       // copied from task->[0xa0]
                                                     // STaskSwitchedGlobals lives just below
0x0c101980   void*            gAccountingPage;       // accumulated CPU times etc.
0x0c10fc34   TObjectTable     gObjectTable;          // INSTANCE (not a ptr)
```

Citations: `Scheduler` at ROM `0x1cc1ec`; `TScheduler::Schedule` at
`0x1cc780`; `WantSchedule__Fv` at `0x1cc7f4`; `SwapInGlobals` at
`0x25215c`; `TObjectTable::Get` first arg in trace
(`r0=0x0c10fc34`).

---

## TScheduler (ROM `Add`/`Schedule`/`RemoveHighestPriority`)

```c
struct TScheduler {                 // total ~0x120
    void*       vtable;             // +0x00
    // ... (unknown)
    ULong       highest_priority;   // +0x14   max of any non-empty queue
    ULong       priority_bitmap;    // +0x18   bit p set ⇒ queue[p] non-empty
    TTaskQueue  queue[32];          // +0x1c   per-priority run queues, 8 B each
    // ... (unknown padding)
    TTask*      last_removed;       // +0x11c  cache from RemoveHighestPriority
};
struct TTaskQueue {                 // 8 bytes — at offset +0x1c+prio*8
    TTask*  head;                   // +0x00   first-task pointer
    TTask*  tail;                   // +0x04   last-task pointer (probably)
};
```

Citations: `TScheduler::Add` at `0x1cc564`:
```
ldr r4, [r1, #128]    ; r4 = task->priority
ldr r0, [r3, #24]     ; r0 = sched->bitmap   ; CITES +0x18
orr r0, r0, r5, lsl r4
str r0, [r3, #24]
ldr r0, [r3, #20]     ; sched->highest_pri   ; CITES +0x14
cmp r0, r4
movcc r0, r4
str r0, [r3, #20]
add r0, r3, r4, lsl #3
add r0, r0, #28       ; queue[prio] = sched + 0x1c + prio*8 ; CITES +0x1c stride 8
```
`RemoveHighestPriority` at `0x1cc690`:
```
ldr r5, [r0, #284]    ; sched->last_removed                ; CITES +0x11c
```

**Newton priority convention** — confirmed from `Add`'s
`cmp r0, r4 / movcc r0, r4` against `highest_priority` (movcc = "if
unsigned r0 < r4"): **higher number = higher priority**. So
priority 20 > priority 10. The bitmap's "highest non-empty bucket" is
the bucket whose tasks should run next.

---

## TTask (ROM `__ct__5TTaskFv` at `0x252190`)

```c
struct TTask /* : TKernelObject */ {           // total = 0x104 (260 B)
    TObjectId  id;              // +0x00   set by TObjectTable::Add
                                //         (low nibble = type=3 for tasks)
    TKernelObject* next;        // +0x04   next-in-hash-chain in TObjectTable
    // ... (unknown +0x08..+0x6b)
    ULong      flags;           // +0x6c   bit 0x02000000 = paged stack
                                //         (FreeStack tests this)
    // ...
    ULong      stack_pages;     // +0x88   freed via FreePagedMem(pages-1)
    void*      stack_base;      // +0x8c   freed via free() if not paged
    // ...
    Priority   priority;        // +0x80   used as run-queue bucket index
    TTaskQItem run_queue_link;  // +0x94   embedded — 40 B
                                //         +0x00 next_task_ptr
                                //         +0x04 prev_task_ptr (probably)
    void*      globals;         // +0xa0   per-task switched-globals ptr
                                //         (STaskSwitchedGlobals lives below)
    // +0xa4..+0xa8 zeroed in ctor
    // +0xac..+0xb8 zeroed in ctor
    TDoubleQItem wq_link_1;     // +0xbc   12 B — wait-queue link
    TDoubleQItem wq_link_2;     // +0xc8   12 B — wait-queue link
    // +0xd4..+0xf4 zeroed in ctor (saved registers? FP / NEON?)
    void*      something_d8;    // +0xd8   copied to *(0x0c101058) by SwapInGlobals
    TObjectId  bequeath_id;     // +0xfc   set by SetBequeathId
    void*      bequeath_obj;    // +0x100  set by SetBequeathId
};

// SWIBoot context-save area (offsets relative to TTask base).
// Citations: SWIBoot save side at 0x3ad864..0x3ad8dc, restore side
// at 0x3ad9a4..0x3ad9c4 + final `movs pc, lr` at 0x3ada6c.
struct TTaskSavedContext {  // sits at &TTask + 0x10, 84 bytes (21 words)
    ULong r0;        // +0x10   popped pre-restore at 0x3ad9f8 ldm r0,{r0..r12}
    ULong r1;        // +0x14
    ULong r2;        // +0x18   stmiane r1!,{r2..r7} writes r2..r7 at +0x18..+0x2c
    ULong r3;        // +0x1c
    ULong r4;        // +0x20
    ULong r5;        // +0x24
    ULong r6;        // +0x28
    ULong r7;        // +0x2c
    // For USR-mode resume (SPSR.mode==0x10): the next 7 words come
    // from a STM with `^` and so are the user-mode banked R8..R12,
    // SP_usr, LR_usr (not the active mode's banked regs).
    ULong r8_usr;    // +0x30
    ULong r9_usr;    // +0x34
    ULong sl_usr;    // +0x38
    ULong fp_usr;    // +0x3c
    ULong ip_usr;    // +0x40
    ULong sp_usr;    // +0x44   ← what `^`-banked STM at 0x3ad880 stores
    ULong lr_usr;    // +0x48
    // For UND-mode resume (SPSR.mode==0x1B): the kernel skips the
    // `^`-LDM and instead at 0x3ad9d0..3ad9e8 reads sp_und from +0x44
    // and lr_und from +0x48, then runs a non-banked LDM r0..r12.
    ULong saved_pc;  // +0x4c   target of `movs pc, lr` at 0x3ada6c
    ULong saved_spsr;// +0x50   `msr SPSR_fc, r1` then movs restores CPSR
};
// Total TTask size from ctor (at 0x252190) is 0x104 bytes; the save
// area lives in the gap between fields explicitly written by the ctor.
// The ctor itself does not zero the save area — it's overwritten on
// the task's first scheduler-save.
struct TTaskQItem {             // 40 bytes
    TTask*  next;               // +0x00
    TTask*  prev;               // +0x04   (queue head goes here for tail; verify)
    // +0x08..+0x27 unknown
};
struct TDoubleQItem {           // 12 bytes
    void*   next;               // +0x00
    void*   prev;               // +0x04
    void*   container;          // +0x08?  (TDoubleQContainer ptr — verify)
};
```

Citations: `TTask::TTask(void)` at `0x252190` (size = `mov r0, #260`,
field-zero pattern); `TScheduler::Add` at `0x1cc564` (`ldr r4, [r1, #128]`
for priority); `SwapInGlobals` at `0x25215c` (reads +0x00, +0xa0, +0xd8);
`FreeStack__5TTaskFv` at `0x252250` (+0x6c flags, +0x88 pages, +0x8c base);
`SetBequeathId__5TTaskFUl` at `0x252278` (+0xfc bequeath_id, +0x100 obj).

---

## TKernelObject / TObjectTable

```c
// gObjectTable is an INSTANCE at fixed VA 0x0c10fc34.
struct TObjectTable {
    void*  scavenge_proc;       // +0x00   set by Init to 0x319a74 (Scavenge)
    // +0x04..+0x0b unknown
    ULong  some_count;          // +0x0c   zeroed by Init
    // +0x10..+0x20c   bucket[128] — TKernelObject* hash chain heads
    TKernelObject* bucket[128]; // bucket = (id >> 4) & 0x7F
};
struct TKernelObject {
    TObjectId       id;         // +0x00
    TKernelObject*  next;       // +0x04   next-in-hash-chain
    // +0x08+ depends on subclass
};
```

Citations: `TObjectTable::Init` at `0x319df4`; `Get` at `0x319f14`;
`Add` at `0x319f9c` (`str r0, [r4]` writes new id into kernelObj+0).

### Kernel object IDs

```c
ID = (sequence << 4) | type;
type bits[3:0] — empirically derived from 717006:
   0x3 = Task         (e.g. STKU id=0x12e3, drvl id=0x1803)
   0x8 = Monitor      (high count in trace queries)
   0x9 = Phys         (high count in trace queries)
sequence bits[31:4] — global counter from NextGlobalUniqueId()
                      (advances monotonically; resets some flag at 256
                      but the counter keeps going)
```

**Warning:** the OS600 DDK `KernelTypes.h` lists the enum as
`{Port, Task, Env, Domain, SemList, SemGroup, SharedMem, SharedMemMsg,
Monitor, Phys}` — the order does **not** match 717006 (where Task=3,
not 1). The actual 717006 mapping for non-confirmed values is still
unknown — we observe types 0x2, 0x4, 0x5, 0x7, 0xa, 0xb in trace
without yet attributing them.

Hash bucket index = `(id >> 4) & 0x7F` ⇒ 128 buckets.

Citations: `NewId` at `0x319e30`:
```
orr r6, r4, r0, lsl #4  ; id = type | (seq << 4)
```
`Get` at `0x319f14`:
```
mov r2, #127
and r2, r2, r1, lsr #4   ; bucket = (id >> 4) & 0x7F
add r0, r0, r2, lsl #2
ldr r0, [r0, #16]        ; bucket head at table+16+bucket*4
```

### How tasks change state

The kernel uses two non-virtual helpers (`ScheduleTask` /
`UnScheduleTask`) to add and remove a task from the scheduler's
per-priority run queues:

```
0x1918e8 <ScheduleTask(TTask*)>:
  ldr r0, [&gScheduler]; r0 = *gScheduler
  b   TScheduler::AddWhenNotCurrent(scheduler, task)

0x1918fc <UnScheduleTask(TTask*)>:
  ldr r2, [&gScheduler]
  ldr pc, [r2, #16]   ; vtable[+16] tail-call (a "Remove"-style virtual)
```

Implication for state inference:

- **A "blocked" task has *empty* run-queue links** (`q.next=0,
  q.prev=0`) because `UnScheduleTask` → `TScheduler::Remove` calls
  `RemoveFromQueue` on the priority bucket. The task is *not* moved
  to any kernel-owned "blocked list" — it's just dangling from the
  scheduler's perspective.
- The blocked task is **reachable from whatever it's waiting on**
  (a port's waiter queue, a semaphore's waiter queue, etc.) — that
  object holds a TDoubleQContainer linking the task in via one of
  the embedded `TDoubleQItem` slots at `task+0xbc` or `task+0xc8`.
  *But* in the current Phase B wedge, every BLK task has both wq
  links zeroed. Either:
  - the per-port waiter mechanism lives at a *different* offset
    (TODO: trace `TUPort::Receive` blocking flow to find it), or
  - those tasks are waiting via a kernel mechanism that doesn't
    use TDoubleQContainer at all (e.g., a state field at
    `task+0xd4..+0xf4`, all zeroed in the ctor — TODO inspect).

So our dump's "BLK" classification means **alive, not in any run
queue, not in either of the two TDoubleQItem wait-links we know
about**. The task is waiting on the *blocking object* — see "How
ports track waiters" below.

### How ports track waiters

`TPort::Receive` at ROM `0x192330` reveals the port layout. When a
port has no message ready and the receiver is allowed to block:

```c
struct TPort /* : TKernelObject */ {
    // +0x00 / +0x04   id, hash-chain next  (TKernelObject base)
    // +0x08..+0x0F    unknown
    TDoubleQContainer  pending_messages;   // +0x10  msgs already sent
                                           //        (length 20 B)
    TDoubleQContainer  waiting_receivers;  // +0x24  TSharedMemMsg*'s
                                           //        of blocked receivers
};
```

Citations: `TPort::Receive` at `0x192330`:
```
add r0, r6, #16    ; r0 = port + 16 = pending-messages queue
bl Peek/GetNext on TDoubleQContainer
...
add r0, r6, #36    ; r0 = port + 36 (=0x24) = receivers/waiters queue
mov r1, r4         ; r1 = TSharedMemMsg* msg (the receiver token)
bl Add__17TDoubleQContainerFPv
```

**Crucial:** the link in the waiter queue points to a
`TSharedMemMsg`, **not** to the `TTask` itself. The msg presumably
records the requesting task (some field within — TODO map). So
"who's blocked on what" must be enumerated by walking ports and
their waiter queues, then chasing each msg back to its owner task.

To enumerate blocked tasks comprehensively from the hypervisor:
1. Walk `gObjectTable` for `type=Port` (TODO: identify Port type bits
   — it's neither 3, 8, nor 9; observe more types in trace).
2. For each port, walk `port+0x24`'s TDoubleQContainer.
3. Each entry is a `TSharedMemMsg` — read the owner-task field
   (TODO: identify offset).

Same pattern likely for semaphores / shared-mem.

### Observed task IDs and names (this run)

| id     | prio | name | role (inferred from name) |
|--------|------|------|---------------------------|
| 0x1043 | 20   | OBJM | Object Manager monitor    |
| 0x1093 | 0    | idle | idle task                 |
| 0x1183 | 20   | PMGR | Page Manager monitor      |
| 0x11d3 | 20   | PTBL | Page Table monitor        |
| 0x1223 | 20   | STKF | Stack-Free monitor        |
| 0x1283 | 20   | STKP | Stack-Page monitor        |
| 0x12e3 | 20   | STKU | Stack-User monitor (this is gCurrentTask at the wedge) |
| 0x1523 | 20   | ROMF | ROM Filesystem monitor    |
| 0x1583 | 20   | ROMP | ROM Pages monitor         |
| 0x1653 | 20   | ???? | unnamed (likely boot init or TUTaskWorld parent) |
| 0x16c3 | 10   | name | NameServer task           |
| 0x1753 | 10   | pckm | Package Manager           |
| 0x1803 | 10   | drvl | driver loader             |
| 0x1cb3 | 20   | drvr | driver root               |
| 0x1d23 | 10   | alrt | alert                     |
| 0x1dc3 | 12   | sndm | Sound Manager             |

---

## STaskSwitchedGlobals (DDK `UserTasks.h`)

```c
struct STaskSwitchedGlobals {       // total > 0x50, variable
    SKernelParams      fKernelParams;       // +0x00   12 ULongs (48 B)
    int                fErrNo;              // +0x30
    TObjectId          fDefaultHeapDomainId;// +0x34
    void*              fStackTop;           // +0x38
    void*              fStackBottom;        // +0x3c
    TObjectId          fTaskId;             // +0x40
    void*              fCurrentHeap;        // +0x44
    NewtonErr          fMemErr;             // +0x48
    ULong              fTaskName;           // +0x4c   four-char-code (e.g. 'STKU')
    ExceptionGlobals   fExceptionGlobals;   // +0x50   variable size
};
```

`TaskSwitchedGlobals()` returns
`((STaskSwitchedGlobals*)gCurrentGlobals) − 1`. So the struct *base*
is `gCurrentGlobals − sizeof(STaskSwitchedGlobals)` and `fTaskName`
sits at `(gCurrentGlobals − sizeof) + 0x4c`. Empirically, on
heuristic backwards-search, fourcc names show up at
`gCurrentGlobals − 8` for tasks observed so far — implying the
struct is exactly 0x54 bytes for this build (so name @ 0x54−8=0x4c).

---

## TAEventHandler (ROM `__24TAEventHandler` / `Init` at `0x25628`)

`TAEventHandler::Init(self, class, signal)` writes (citations from
the disassembly at 0x25628..0x25650):

```c
struct TAEventHandler {       // size unknown, observed +0x08..+0x0c
    void*  vtable;            // +0x00
    // +0x04   unknown
    ULong  signal;            // +0x08   ← `str r2, [r0, #8]`  (signal arg)
    ULong  class;             // +0x0c   ← `str r1, [r0, #12]` (class arg)
    // ... rest unknown
};
```

Note the parameter order to `Init`: r1 = class fourcc, r2 = signal
fourcc. Storage order in the object: signal at +0x08, class at +0x0c
— **not the same order as the C++ argument list**.

Used by `TAppWorld::AEInstallHandler` (called immediately after Init
at 0x25648) to register the handler for an (class, signal) pair.

Diagnostic relevance: on the Phase B "newt" DABT, our hypervisor's
pckm task sees `0x6e657774` ("newt") at `sp_usr+0x08` and
`0x63647376` ("cdsv") at `sp_usr+0x0c`. That's exactly the layout of
a `TAEventHandler` with `class='cdsv', signal='newt'` placed at
`sp_usr`. Cross-check: trace 183155 in our run has
`Init(handler=0x0c602e2c, class='cdsv', signal='newt')` — same fourcc
pair, but installed at a *different* handler address. The pckm
divergence appears to be that our kernel state ends up with this
fourcc pair sitting on top of pckm's user stack frame, which Einstein
never produces (in Einstein the stack at `sp_usr+0x08` holds a normal
stack-pointer pushed by `TUPort::Receive` 0x259d2c).

---

## See also

- `INVESTIGATION.md` — live wedge debugging notes
- `src/task_dump.rs` — runtime walker that materializes the above
- `docs/DISASM.md` — how to use `scripts/disasm-out/rom.dis`
- `/Users/walter/Projects/newton/ghidra/DDKIncludes/OS600/` — public
  DDK headers (Apple, 1995). Useful for class names and high-level
  shape; **field offsets must be verified against 717006 binary.**
