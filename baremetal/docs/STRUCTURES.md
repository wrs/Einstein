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
    ULong      flags;           // +0x6c   KernelObjectState bits — see
                                //         TTaskQueue table below.
                                //         bit 0x4000     = on a TUCTTable
                                //         bit 0x20000    = on a TScheduler
                                //                          run queue
                                //         bit 0x100000   = on a TSemaphore
                                //                          wait queue
                                //         bit 0x02000000 = paged stack
    // ...
    Priority   priority;        // +0x80   used as run-queue bucket index
    ULong      stack_pages;     // +0x88   freed via FreePagedMem(pages-1)
    void*      stack_base;      // +0x8c   freed via free() if not paged
    TTaskContainer* container;  // +0x90   ptr to the queue container we're
                                //         currently in (TScheduler / TSema /
                                //         TUCTTable). Written by
                                //         TTaskQueue::Add (`str r6, [r4, #36]!`
                                //         after `str r0, [r4, #108]!` ⇒
                                //         base=task+0x6c+0x24=task+0x90 —
                                //         NOT task+0x24).
    TTask*     q_next;          // +0x94   TTaskQueue.next link
    TTask*     q_prev_or_queue; // +0x98   TTaskQueue.prev — points to either
                                //         the previous task in the queue OR
                                //         to the queue head (when this is
                                //         the queue's only/first element)
    // +0x9c          ULong  per-queue payload (TUCTTable stores held-id here:
                                //         `str r4, [r5, #156]` at ROM 0x256330)
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
    void*   next;               // +0x00   next entry's TDoubleQItem (or 0)
    void*   prev;               // +0x04   prev entry's qitem (or container itself
                                //         when this is the head — Add stores the
                                //         container ptr there as a sentinel)
    void*   container;          // +0x08   back-ptr to TDoubleQContainer (set by Add)
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
type bits[3:0] — full mapping for 717006 (kernel side, the value stored in ID):
   0x2 = TPort
   0x3 = TTask
   0x4 = TEnvironment
   0x5 = TDomain          (TKDomain shares this type; TKDomain : TDomain)
   0x6 = TSemaphoreList   (DDK calls it SemList)
   0x7 = TSemaphoreGroup
   0x8 = TSharedMem
   0x9 = TSharedMemMsg
   0xa = TMonitor
   0xb = TPhys
   (0x0, 0x1, 0xc..0xf — unused by kernel objects in this build)
sequence bits[31:4] — global counter from NextGlobalUniqueId()
                      (advances monotonically; resets some flag at 256
                      but the counter keeps going)
```

**The kernel KernelTypes enum is the user ObjectTypes enum + 2.** The
public DDK header `KernelTypes.h` defines `{Port=0, Task=1, Env=2,
Domain=3, SemList=4, SemGroup=5, SharedMem=6, SharedMemMsg=7, Monitor=8,
Phys=9}` — those are the values user-mode code passes to
`MakeObject__8TUObjectF11ObjectTypesP13ObjectMessageUl` (e.g. r1=8
inside `Init__9TUMonitor` at ROM 0x2594f8). The kernel's
`MonitorDispatchSWI` translates them to its internal KernelTypes by
adding 2 before constructing the kernel object and calling
`RegisterObject__FP13TKernelObject11KernelTypesUl` (the public kernel
helper that wraps `Add__12TObjectTable`).

**Citations for individual mappings:**
- `Init__9TUMonitor` ROM 0x2594f0..2594f8: `mov r1, #8; ... bl
  MakeObject` (user side passes 8, kernel registers as 0xa).
- `InitObjectManager` ROM 0x1495a0..1495b8: `bl __ct__8TMonitor; ...
  mov r1, #10; bl RegisterObject` — proves Monitor = kernel type 10.
- `OsBoot` ROM 0x14817c..148194: `bl __ct__5TTask; ... mov r1, #3; bl
  RegisterObject` — proves Task = kernel type 3.
- `InitKernelDomainAndEnvironment` ROM 0xe90e8..e911c: registers
  TEnvironment with r1=4 (kernel type 4) and TKDomain with r1=5
  (kernel type 5).
- `Init__5TTask` ROM 0x252468 + 0x2524b0: registers a TSharedMem at
  TTask+0xf0 with r1=8 (kernel type 8) and a TSharedMemMsg at
  TTask+0xf4 with r1=9 (kernel type 9). (Every task ends up owning
  one of each as embedded fields.)
- `InitTPhysAndAddToObjectTable` ROM 0x148f28 (path inside an
  unnamed function, post-`Init__5TPhys`): `mov r1, #11` then `b
  0x1490ec` falls through to `bl RegisterObject`. Proves Phys =
  kernel type 11 (0xb).

The remaining types (Port=2, SemList=6, SemGroup=7) are confirmed by
the +2 pattern but not yet by direct disasm observation of their
RegisterObject call (`__ct__5TPortFv` is not exported as a separate
symbol in this ROM; ports are constructed inline through the
`MonitorDispatchSWI` path and we haven't traced that kernel side).

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

`TPort::Receive` at ROM `0x192330`, `Send` at `0x19211c`, dtor at
`0x191b40`, and `PortReceiveKernelGlue` at `0x192224` together
establish the port layout. Type code: **0x2** (KernelType_Port,
confirmed by `PortResetKernelGlue` ROM `0x1924a0`: `mov r0, #2; bl
ConvertIdToObj`).

```c
struct TPort /* : TKernelObject */ {           // ~56 bytes
    // +0x00 / +0x04   id, hash-chain next  (TKernelObject base)
    // +0x08..+0x0F    unknown — ctor not exported as a separate symbol
                       //        in this ROM; ports are constructed inline
                       //        from MonitorDispatchSWI handlers we
                       //        haven't yet traced kernel-side
    TDoubleQContainer  pending_messages;   // +0x10  20 B — msgs already sent,
                                           //        not yet received. Walked
                                           //        by Receive (0x192378) and
                                           //        drained by dtor (0x191b58).
    TDoubleQContainer  waiting_receivers;  // +0x24  20 B — TSharedMemMsg*'s
                                           //        of blocked receivers.
                                           //        Walked by Send (0x192170);
                                           //        drained by dtor (0x191ba8).
};
```

Citations: `Send__5TPort` at `0x19211c`:
```
add r0, r6, #16    ; r0 = port + 0x10 = pending-messages queue
bl  Peek__17TDoubleQContainer
...
add r0, r6, #36    ; r0 = port + 0x24 = receivers queue
mov r1, r4         ; r1 = msg
bl  Add__17TDoubleQContainerFPv
```
`Receive__5TPort` at `0x192330` does the symmetric walk: it walks
+0x10 looking for a matching pending msg, and if none is found and
the caller is willing to block, it adds the receiver-msg to +0x24.

**Identifying who is blocked on a port:** walk `port+0x24` (or use
`walk_dqc` in `src/task_dump.rs`); each entry is a TSharedMemMsg
whose `+0x70` field is the receiving task's ID. Resolve via
`gObjectTable[bucket((id>>4)&0x7F)]` to the TTask\*. See the
TSharedMemMsg layout above for the full field list.

Same pattern for monitors (TMonitor +0x24 is also a
TDoubleQContainer — see "TMonitor" below).

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

## TMonitor (ROM `__ct__8TMonitorFv` at `0x11fb60`)

The monitor is the kernel's mutual-exclusion + serialised-message
primitive — every `OBJM`, `PMGR`, `PTBL`, `STK*`, `ROM*` task in the
boot trace is the helper-task spawned by some monitor's `Init` to run
its body. Type code: **0xa** (KernelType_Monitor, confirmed by
`MonitorDispatchKernelGlue` ROM `0x11fc30`: `mov r0, #10; bl
ConvertIdToObj`).

```c
struct TMonitor /* : TKernelObject */ {        // 72 bytes
    // +0x00 / +0x04   id, hash-chain next  (TKernelObject base)
    // +0x08          ULong owner-or-env id (compared with gCurrentTask+8 in
                     //        Aquire ROM 0x120278; possibly env id used for
                     //        access checks)
    // +0x0c          unknown
    ULong  depth;             // +0x10  re-acquire depth — incremented by
                              //        Aquire (ROM 0x120298), decremented by
                              //        FlushTasksOnMonitor (ROM 0x11fbf0)
    ULong  state_flags;       // +0x14  bits 0..1 checked at start of Aquire
                              //        (ROM 0x12024c) — entry guard / aborted?
    // +0x18          unknown (zeroed in ctor)
    // +0x1c          unknown (zeroed)
    // +0x20          unknown (zeroed)
    TDoubleQContainer waiters;// +0x24  20 B — TDoubleQContainer of blocked
                              //        TTasks. link_offset = 0xc8, so each
                              //        entry's qitem is task->wq_link_2 (the
                              //        second TDoubleQItem embedded in TTask).
                              //        Constructed with a CheckBeforeAdd
                              //        callback (ROM 0x11fb8c..fb94 passes
                              //        a function ptr in r2 and self in r3).
    // +0x38          ptr (zeroed in ctor)
    // +0x40          ptr (zeroed)
    // +0x45          byte (set in Release ROM 0x1202fc — likely a "released"
                     //        sentinel or "monitor-is-suspended" flag)
    // +0x46          byte (zeroed)
};
```

Citations: ctor at `0x11fb60` (`mov r0, #72`):
```
add r0, r4, #36         ; +0x24
mov r3, r4              ; CheckBeforeAdd ctx = self
ldr r2, [pc, #48]       ; CheckBeforeAdd fn ptr
mov r1, #200            ; link_offset = 0xc8 (TTask.wq_link_2)
bl  __ct__17TDoubleQContainerFPcPFPvT1_vPv
```
`Aquire__8TMonitor` at `0x12022c` (waiter add):
```
add r0, r4, #36         ; monitor +0x24
mov r1, r5              ; r5 = gCurrentTask
bl  Add__17TDoubleQContainerFPv
```
`FlushTasksOnMonitor` at `0x11fbc8` (waiter drain → `ScheduleTask`):
```
add r0, r0, #36         ; +0x24
bl  Remove__17TDoubleQContainerFv   ; returns entry = qitem - 0xc8 = TTask*
ldr r1, [r4, #16]       ; depth
sub r1, r1, #1
str r1, [r4, #16]
bl  ScheduleTask__FP5TTask          ; reschedule the unblocked task
```

**Diagnostic implication:** unlike a TPort whose waiters are
TSharedMemMsg tokens, a TMonitor's waiters are TTasks **directly**.
So enumerating "who is blocked on which monitor" is one walk shorter:
walk `monitor+0x24`, each entry IS a TTask, read `task+0x00` for the
id and resolve its name via `STaskSwitchedGlobals`.

## TSharedMemMsg (ROM `__ct__13TSharedMemMsgFv` at `0x1e017c`)

The kernel-side message-passing token. Every cross-task `TUPort::Send`,
`Receive`, `SendRPC` and similar is mediated by one of these. Each
TTask owns one as an embedded sub-object at `task+0xf4` (registered
with KernelType=9 by `Init__5TTask` at ROM `0x2524b0`). Type code:
**0x9** (KernelType_SharedMemMsg).

```c
struct TSharedMemMsg /* : TKernelObject */ {       // 168 bytes
    // +0x00 / +0x04   id, hash-chain next  (TKernelObject base)
    // +0x08..+0x23    inherited from TSharedMem (28 B);
    //                 see Init__10TSharedMem ctor — buffer base/limit, env,
    //                 flags. Detailed offsets TBD.
    TDoubleQItem  q1;          // +0x30   12 B — qitem used when msg is parked
                               //         on a TPort or TMonitor waiter/pending
                               //         queue. Confirmed ctor at 0x1e01a4.
    // +0x3c             cleared by Init / PortReceiveKernelGlue
    // +0x40             cleared
    ULong         state_or_obj;// +0x44   "in-use" sentinel during PortReceive
                               //         (set to 1 at ROM 0x1922b8); later set
                               //         to a TKernelObject* by CompleteMsg
                               //         (ROM 0x1e05e8) when the msg parks on
                               //         a Port (type 2) or Task (type 3).
    // +0x48             user ref-con (returned by SMemMsgGetUserRefConKernelGlue
                               //         ROM 0x1e01f0)
    // +0x4c             buffer-related (see CompleteReceiver ROM 0x1e0494)
    ULong         flags;       // +0x50   bit 0x01000000 = msg is a sub-message
                               //         (CompleteReceiver checks this);
                               //         bits 0x02000000/0x03000000 control
                               //         lifecycle (CompleteMsg ROM 0x1e0508)
    ULong         filter;      // +0x54   recv filter (set by PortReceiveKernelGlue
                               //         ROM 0x1e0863); compared against sender's
                               //         flags in Send/Receive's iteration loop
    // +0x58             ULong  result1   (returned to caller via
                               //         SMemMsgCheckForDoneKernelGlue ROM 0x1e02d0)
    // +0x5c             ULong  result2   (same path, ROM 0x1e02e0)
    // +0x60             ULong  result3
    // +0x64             ULong  result4
    // +0x68             ULong  recv_override_id (used at ROM 0x1922f8 when
                               //         flag bit 0 of incoming `r4` is set)
    // +0x6c             ULong  parked_on_id    (set at 0x1922fc to either
                               //         current-task id at +0x70 or override
                               //         at +0x68; its low 4 bits identify the
                               //         owner's KernelType — Port=2 / Task=3)
    ULong         receiver_id; // +0x70   id of the task that issued Receive
                               //         (= gCurrentTask->id at ROM 0x1922e0)
    // +0x74             ULong (initialised to 0; reset to msg+0x78 by
                               //         CompleteReceiver ROM 0x1e04dc)
    // +0x78             ULong (initialised to 1)
    ULong         sender_id;   // +0x7c   sender task id (resolved as Type=3
                               //         in CompleteMsg ROM 0x1e0580 to find
                               //         the sender's TTask*)
    TDoubleQItem  q2;          // +0x80   12 B — qitem used when msg is linked
                               //         from a parent msg's child-completion
                               //         container at +0x8c (see below).
                               //         ctor at 0x1e01ac.
    TDoubleQContainer children;// +0x8c   20 B — child-msgs awaiting completion;
                               //         link_offset = 128 (= 0x80, i.e. uses
                               //         each child's q2). ctor at 0x1e01b8.
};
```

Citations: ctor at `0x1e017c` allocates 168 bytes:
```
mov r0, #168
bl  __nw__FUi
add r0, r4, #48     ; +0x30 q1 ctor
bl  __ct__12TDoubleQItemFv
add r0, r4, #128    ; +0x80 q2 ctor
bl  __ct__12TDoubleQItemFv
add r0, r4, #140    ; +0x8c container ctor
mov r1, #128        ; link_offset = 0x80 (uses q2 of each child)
bl  __ct__17TDoubleQContainerFPc
```
`PortReceiveKernelGlue` at `0x192224` writes the receiver-task id +0x70:
```
ldr r0, [r7]        ; r0 = gCurrentTask
ldr r0, [r0]        ; r0 = curtask->id
ldr r1, [sp]        ; r1 = msg
str r0, [r1, #112]! ; msg->[0x70] = receiver id
```
`CompleteMsg` at `0x1e0524` reads sender id from +0x7c:
```
ldr r1, [r4, #124]    ; +0x7c = sender id
mov r0, #3            ; KernelType_Task
bl  ConvertIdToObj    ; resolve to TTask*
```

**Diagnostic implication:** to identify "which task is blocked on a
port", walk the port's waiter queue (`port+0x24` TDoubleQContainer)
with link_offset = whatever the container reports (typically 0x30 for
`q1`), then for each entry read `msg+0x70` (receiver id) and look it
up in `gObjectTable` to find the TTask*. Cross-check against
`msg+0x6c` whose low nibble must equal 2 (Port) when the msg is
currently parked on a port.

## TDoubleQContainer (ROM `__ct__17TDoubleQContainerFv` at `0x9c8d0`)

The kernel's primary intrusive doubly-linked-list primitive. Used by
ports (pending msgs, waiters), monitors (waiters), the timer engine,
and several other kernel subsystems. **Not a kernel object** — it has
no ID and isn't registered in `gObjectTable`; it's embedded inside
other structures as a field.

```c
struct TDoubleQContainer {       // 20 bytes (mov r0, #20; bl __nw__ in ctor)
    void*  head;                 // +0x00   first entry's TDoubleQItem (or 0)
    void*  tail;                 // +0x04   last entry's TDoubleQItem (or 0)
    ULong  link_offset;          // +0x08   offset within each entry where the
                                 //         embedded TDoubleQItem sits — Add()
                                 //         computes &entry.qitem = entry + link_offset
    void*  check_before_add_fn;  // +0x0c   optional CheckBeforeAdd callback
                                 //         (set only by ctor variant
                                 //          __ct__17TDoubleQContainerFPcPFPvT1_vPv)
    void*  client_data;          // +0x10   opaque ptr passed to the callback
};
```

Citations: `Init__17TDoubleQContainerFPc` at `0x9c990` (5-word zero-out,
stores r1 to +0x08 — the link-offset arg):
```
mov r2, #0
str r2, [r0]        ; +0x00 head = 0
str r1, [r0, #8]    ; +0x08 link_offset = arg
str r2, [r0, #4]    ; +0x04 tail  = 0
str r2, [r0, #12]   ; +0x0c check_before_add_fn = 0
str r2, [r0, #16]!  ; +0x10 client_data = 0
```

`Add__17TDoubleQContainerFPv` at `0x9c9b0`:
```
ldr r0, [r4, #8]   ; link_offset
add r5, r0, r1     ; &qitem = link_offset + entry
str 0, [r5]        ; qitem.next = 0
ldr r0, [r4]       ; head
teq r0, #0
streq r5, [r4]     ; head = &qitem
streq r4, [r5, #4] ; qitem.prev = container (sentinel for head)
ldrne r0, [r4, #4] ; tail
strne r5, [r0]     ; old_tail.next = &qitem
strne r0, [r5, #4] ; qitem.prev = old_tail
str r5, [r4, #4]   ; tail = &qitem
str r4, [r5, #8]!  ; qitem.container = self
```

**Walking a TDoubleQContainer for diagnostics:**
1. Read `head` (+0x00). If 0, queue is empty.
2. Read `link_offset` (+0x08). Each entry pointer is reached by
   `entry = qitem - link_offset`.
3. From the head qitem, walk `qitem.next` until 0; the very last
   `qitem.prev` should be `container` itself (sentinel) but the
   chain terminates on `next == 0`.

The link-offset varies per use:
- TPort pending-msgs queue (+0x10): entries are `TSharedMemMsg*`, qitem
  embedded within the msg (offset TBD in TSharedMemMsg layout).
- TPort waiters queue (+0x24): entries are `TSharedMemMsg*` as well.
- TMonitor waiters queue (+0x24): entries TBD.

## TTaskQueue (ROM `__ct__10TTaskQueueFv` at `0x359a74`)

Two-pointer head/tail queue used by both the scheduler (per-priority
run buckets at `gScheduler + 0x1c + prio*8`) and TSemaphore (the
BlockOnZero / BlockOnInc wait queues at `sema+0x18` / `sema+0x20`).
Tasks are linked by `task[+0x94]` (next) and `task[+0x98]` (prev or
queue-back-pointer when head/tail).

```c
struct TTaskQueue {     // 8 bytes (mov r0, #8 in ctor)
    TTask*  head;       // +0x00  first task in queue (or 0 if empty)
    TTask*  tail;       // +0x04  last task in queue
};
```

Add/Remove citations:

`Add__10TTaskQueueFP5TTask17KernelObjectStateP14TTaskContainer` at
ROM `0x359aac`:

```
str r0, [r1, #148]    ; task[+0x94] (next) = 0   ; CITES +0x94 next
ldr r0, [r5]          ; queue.head
teq r0, #0
streq r4, [r5]        ; if empty, queue.head = task
streq r5, [r4, #152]  ; task[+0x98] = queue
ldrne r0, [r5, #4]    ; queue.tail
strne r4, [r0, #148]  ; old_tail.next = task     ; CITES +0x94
strne r0, [r4, #152]  ; task[+0x98] = old_tail   ; CITES +0x98 prev
str r4, [r5, #4]!     ; queue.tail = task
ldr r0, [r4, #108]    ; flags[+0x6c]
orr r0, r0, r7        ; |= state
str r0, [r4, #108]!   ; r4 ← task+0x6c
str r6, [r4, #36]!    ; task[+0x6c+0x24] = task[+0x90] = container
                       ; CITES +0x90 container (NOT +0x24)
```

**KernelObjectState bits passed as `r2` to `Add__10TTaskQueue`:**

| bit          | symbol             | added by                                   |
|--------------|--------------------|--------------------------------------------|
| `0x4000`     | (TUCT)             | `TUCTTable::Add` (ROM 0x256300, r2=0x4000) |
| `0x20000`    | (run queue)        | `TScheduler::Add` (ROM 0x1cc564, r2=0x20000) |
| `0x100000`   | (TSemaphore wait)  | `TSemaphore::BlockOnInc/BlockOnZero` (ROM 0x1d4d98 / 0x1d5264, r2=0x100000) |

These bits are stored in `task[+0x6c]` (flags). Reading
`task[+0x6c]` tells you which queue mechanism currently owns the
task. Cleared on the matching Remove path (`bic` in
`Remove__10TTaskQueue` ROM 0x359b48 / `RemoveFromQueue` 0x359bcc).

## TSemaphore (ROM `__ct__10TSemaphoreFv` at `0x1d5100`)

The kernel's binary-or-counted-semaphore primitive. **Not a kernel
object** (no ID, not in `gObjectTable`); allocated as a 40-byte
internal struct, typically as the contents of a TSemaphoreGroup's
sema array.

```c
struct TSemaphore {           // 40 bytes (mov r0, #40 at ROM 0x1d5114)
    // +0x00 / +0x04   unused (zeroed via __nw_v)
    // +0x08           unknown (zeroed)
    // +0x0c           unknown (zeroed)
    void*       vtable;       // +0x10  set to 0x0001ae40 by __ct__
                               //        (the real vtable; 0x0001dbc4 is
                               //        the early "init in progress" stub
                               //        written first then overwritten)
    Long        count;        // +0x14  current count value (signed; <0
                               //        means waiters present in BlockOnInc)
    TTaskQueue  block_zero;   // +0x18  8 B — tasks waiting for count==0
    TTaskQueue  block_inc;    // +0x20  8 B — tasks waiting for count to inc
};                             // total 0x28
```

Critical methods:

- `BlockOnInc__10TSemaphoreFP5TTask8SemFlags` ROM `0x1d4d98`:
  ```
  tst r2, #1              ; SemFlags & 1 = "non-blocking" → return early
  bl  UnScheduleTask      ; remove caller from run queue
  add r0, r5, #32         ; r0 = sema + 0x20 = block_inc queue
  mov r3, r5              ; container = sema
  mov r2, #0x100000       ; state bit
  b   Add__10TTaskQueue
  ```
- `WakeTasksOnInc__10TSemaphoreFv` ROM `0x1d4e18` walks `sema+0x20`,
  calling `Remove__10TTaskQueue(state=0x100000)` and
  `ScheduleTask` for each waiter, then `WantSchedule`.
- `Remove__10TSemaphoreFP5TTask` ROM `0x1d5230` calls
  `RemoveFromQueue` for whichever of the two queues the task is on.

## TSemaphoreGroup (ROM `Init__15TSemaphoreGroupFUl` at `0x1d4e5c`)

Kernel-side wrapper around an array of TSemaphores. Registered in
`gObjectTable` as KernelType=7 ("SemG"). The TUSemaphoreGroup user
wrapper adds a +0x00..+0x18 prefix; the kernel half lives at
TUSemaphoreGroup+0x18 (verified by TUSemaphoreGroup::Init at ROM
0x25a270 calling MakeObject which fills `this[0]` with the kernel id,
plus TULockingSemaphore::Init at 0x25a504 calling
TUSemaphoreGroup::Init on `this`).

```c
struct TSemaphoreGroup /* : TKernelObject */ {     // ~24 bytes
    // +0x00 / +0x04   id, hash-chain next (TKernelObject base)
    // +0x08           unknown (zeroed)
    // +0x0c           unknown (zeroed)
    TSemaphore*  sema_array;       // +0x10  malloc'd via __nw_v of
                                   //        sizeof(TSemaphore)*count;
                                   //        each slot ctor'd by __ct__
                                   //        (ROM 0x1d4e84 / 0x1d4e88).
    ULong        count;            // +0x14  number of semaphores
                                   //        (ROM 0x1d4e9c stores r5)
};
```

**Diagnostic implication:** to map a TSemaphore back to its owning
group, walk every TSemaphoreGroup in `gObjectTable` and check
whether the sema's address falls within `[arr_base, arr_base +
40*count)` and is 40-byte aligned from `arr_base`. See
`task_dump::find_semaphore_owner` for the reference walker.

## TUSemaphoreGroup / TULockingSemaphore (user-mode wrappers)

```c
struct TUSemaphoreGroup {           // user-side wrapper
    TObjectId    sem_group_id;      // +0x00  kernel id, set by
                                    //        MakeObject(ObjectTypes=5)
                                    //        at ROM 0x25a290.
    // +0x04            unknown
    void*        refcon;            // +0x08  opaque, set by SetRefCon
                                    //        (TUSemaphoreGroup +0x08 in
                                    //        TULockingSemaphore::Init).
    // +0x0c..+0x17     unknown
    // (TSemaphoreGroup kernel half lives at +0x18 in TULockingSemaphore;
    //  TUSemaphoreGroup standalone is just the +0x00..+0x14 prefix.)
};

struct TULockingSemaphore : TUSemaphoreGroup {     // 40+ bytes
    // inherits sem_group_id at +0x00, refcon at +0x08
    ULong*       lock_state;        // +0x08 alias — `refcon` is
                                    //        SET to lock_state by ctor.
                                    //        Points to a 4-byte malloc'd
                                    //        word (ROM 0x25a4dc malloc(4)).
                                    //        Lock value: 0=free, otherwise
                                    //        the holder's gCurrentTaskId.
    // +0x18..       embedded TSemaphoreGroup (see above)
};
```

`Acquire__18TULockingSemaphoreF8SemFlags` ROM `0x25a298`:

```
mov r4, r0                   ; r4 = this
ldr r0, [r4, #8]             ; r0 = lock_state ptr
ldr r1, [r9]                 ; r1 = gCurrentTaskId (= *0x0c101054)
bl  Swap                     ; *r0 ↔ r1 ; r0 ← old(*r0) (atomic)
teq r0, #0
beq acquired                 ; if old == 0, lock was free
mov r0, r4
mov r1, r8                   ; r1 = gAcquireOps (0x0c104f14)
mov r2, r5                   ; r2 = SemFlags arg
bl  SemOp__16TUSemaphoreGroupFP17TUSemaphoreOpList8SemFlags
                             ; → SVC 0xb (SemaphoreOpGlue ROM 0x3ae1fc)
                             ; → kernel-side TSemaphoreGroup::SemOp
                             ; → BlockOnInc on sema[0]
beq retry                    ; loop back to outer Swap on wakeup
```

**Note:** TULockingSemaphore is **not** recursive. A second `Acquire`
by the same task that already holds the lock will see
`*lock_state == gCurrentTaskId` (non-zero), Swap puts the same id
back, and Acquire blocks on `BlockOnInc` of a count that no one
will increment (since the holder is itself the blocked task). This
is a self-deadlock — any caller of `Acquire` must guarantee a
matching `Release` on **all** exit paths, including the C++
exception unwind chain. (See `INVESTIGATION.md` for a worked
example: `MakeStoreObject`'s catch handler calls
`TStoreWrapper::Abort` but **not** `UnlockStore`, so a Throw inside
the locked region leaves the heap-store TULockingSemaphore held by
the throwing task.)

`Release__18TULockingSemaphoreFv` ROM `0x25a31c`:

```
ldr r0, [r0, #8]             ; lock_state ptr
mov r1, #0
bl  Swap                     ; old = swap(*lock_state, 0)
ldr r1, [&gCurrentTaskId]
teq r0, r1
moveq r0, #0
ldmdbeq fp, ... (return)     ; if old == self, simple release done
mov r0, r4                   ; else need to wake waiters via SemOp inc
mov r1, &gReleaseOps         ; (0x0c104f0c)
mov r2, #1
b   SemOp                    ; → BlockOnInc complement (count++)
```

`gAcquireOps` (TUSemaphoreOpList @ 0x0c104f14) and `gReleaseOps`
(@ 0x0c104f0c) are statically initialised in
`TULockingSemaphore::StaticInit` (ROM 0x25a480, called via
`TUSemaphoreOpList::Init`). Acquire is `subtract 1` (block on
count<0); Release is `add 1` (wake on count≥0).

## TStackManager — heap / stack page allocator

**Why this matters for Phase B.** The 717006 kernel was built for
ARMv4 and uses ARMv4 *subpage-AP* (per-1-KiB AP encoding within a 4-KiB
page) to put up to four 1-KiB-owned objects on a single physical page,
relying on hardware to fault on cross-subpage user writes. ARMv7 has no
subpage-AP — once `fix_stage1_xn_bits` flattens every L2 entry to
AP=011, all four subpages become RW for the same user mode and any
cross-subpage write silently corrupts the neighbour. Forcing every
allocator to chunk in whole 4-KiB pages restores isolation. This
section catalogues the lock/unlock ABI, the kernel structures it
mutates, and every allocator we've audited for 1-KiB chunking.

### Locking ABI — `LockHeapRange` / `UnlockHeapRange`

```c
// Caller-side glue (jump-table-aliased at 0x01BD6B54 / 0x01BDDEA0)
ULong LockHeapRange(void* base, void* limit, UByte lock_id);   // 0x001F8AB4
ULong UnlockHeapRange(void* base, void* limit);                // 0x001F8B88
```

Both pack the args into a parms struct on the stack and hand off via
`MonitorDispatchSWI` to a privileged TStackManager method:

| caller | req-id | callee | parms shape |
|--------|--------|--------|-------------|
| `LockHeapRange`   | 6 | `FMLockHeapRange__13TStackManager` (`0x001F6B24`) | `{base, end_inclusive=limit-1, lock_id_byte}` (12 B) |
| `UnlockHeapRange` | 7 | `FMUnlockHeapRange__13TStackManager` (`0x001F6C24`) | `{base, end_inclusive=limit-1}` (8 B) |

Citations: `LockHeapRange` body `1f8ac0..1f8acc` (str r0; sub r0,r1,#1;
str r0,[sp,#4]; strb r2,[sp,#8]); `UnlockHeapRange` body `1f8b94..1f8b9c`
(same minus the strb).

### TStackInfo (per-stack / per-heap allocator descriptor)

The `r5` argument to FMLockHeapRange's loop and the `r5` argument to
ResolveFault are both `TStackInfo*`. Decoded fields:

```c
struct TStackInfo {                  // total size unknown
    // +0x00..+0x0f  unknown
    TStackPage**  page_table;        // +0x10  array of TStackPage*, indexed by
                                     //        page_idx = (FAR - base_va) / 4 KiB
    ULong         base_va;           // +0x14  base VA of the stack/heap region
    ULong         lower_bound;       // +0x18  inclusive lower-bound VA
    ULong         upper_bound;       // +0x1c  exclusive upper-bound VA
    // +0x20..+0x23  unknown
    void*         domain_ptr;        // +0x24  THeapDomain* (used by RememberMappings)
    // ...
};
```

Citations:
- `+0x10`: ResolveFault `1f79fc: ldr r0, [r5, #16]` then
  `1f7a00: ldr r6, [r0, r7, lsl #2]` — load page_table base, then index
  by `page_idx*4`.
- `+0x14`: ResolveFault `1f79d8: ldr r1, [r5, #20]` then
  `1f79dc: sub r0, r0, r1` — subtract base_va from FAR to get offset.
- `+0x18`: ResolveFault `1f79c4: ldr r2, [r5, #24]` — lower-bound check.
  Returns -10203 (`r0=37; subcc r0, r0, #10240`) if FAR < lower.
- `+0x1c`: ResolveFault `1f79b0: ldr r2, [r5, #28]` — upper-bound check.
  Returns -10204 if FAR ≥ upper.
- `+0x24`: RememberMappings `1f8588: ldr r0, [r4, #36]!` (with r4 =
  TStackInfo*) — Remember/Forget calls receive this as their domain.

### TStackPage (per-physical-page bookkeeping)

A `TStackPage*` lives in `TStackInfo->page_table[page_idx]` and tracks
the four 1-KiB subpages of one 4-KiB physical page.

```c
struct TStackPage {                  // total size unknown (≥48 B)
    // +0x00..+0x0f  unknown (page state / phys backing)
    TStackInfo*   subpage_owner[4];  // +0x10  stride 4   (16 B)
    UShort        subpage_info[4];   // +0x20  stride 2   (8 B)  — high byte = page_idx
    UByte         subpage_lockcount[4]; // +0x28 stride 1 (4 B)  — refcount
    UByte         subpage_flag[4];   // +0x2c  stride 1   (4 B)  — "don't page out"
    // ...
};
```

Citations:
- `+0x10` (subpage_owner): `SetSubPageInfo` `1f85f8: add lr, r3, ip,
  lsl #2; str r1, [lr, #16]!` (r3=page, ip=subpage_idx, r1=info).
  ResolveFault `1f7a3c..40: add r0, r6, r8, lsl #2; ldr r9, [r0, #16]!`
  reads back into r9.
- `+0x20` (subpage_info): SetSubPageInfo `1f8600: add r1, r3, ip, lsl
  #1` then `strb` at `+33` and `+32`. ResolveFault `1f7a48: ldr r2,
  [r0, #32]; lsr r2, r2, #16` extracts the high half = page_idx.
- `+0x28` (subpage_lockcount): UnlockSubPagesBetween `1f7100: ldrb
  sl, [r6, #40]; sub sl, sl, #1; strb sl, [r6, #40]` — decrement
  on unlock; ResolveFault `1f7ac4..d0: ldrb r1, [r0, #40]; add r1,
  r1, #1; strb r1, [r0, #40]` increments on resume.
- `+0x2c` (subpage_flag): FMLockHeapRange flag-set `1f6c04: strb r1,
  [r0, #44]` writes 1; UnlockSubPagesBetween `1f7110: streq r0, [r6,
  #44]` clears to 0 once lockcount hits 0. Semantic: subpage is pinned
  in memory while non-zero.

### FMLockHeapRange iteration (ROM `0x001F6B24`)

```
1f6b58..6c  lsr/lsl #10 — align r9=base, r8=end_inclusive DOWN to 1 KiB
1f6b88..c8  main loop:                       ; per-1-KiB step over [r9, r8]
              ResolveFault(this, info)        ; FAR set via [info[+64]][+68]
              on first-iter failure → return
              on later failure → UnlockSubPagesBetween(prefix); return
              advance r6 += 1024
1f6bcc..14  if (parms[+8] /* lock_id */ != 0):
              for (subpage in [r9, r8] step 1024)
                offset = subpage - info->base_va
                page_idx = (offset >> 10) >> 2
                page = info->page_table[page_idx]
                page[+44 + (subpage_idx & 3)] = 1   ; pin subpage
```

The main loop allocates / claims subpages via ResolveFault; the
flag-set loop pins them against paging. Both consume the same
1-KiB-aligned (r9, r8) range — which is why widening at the
LockHeapRange entry point is unsafe (the flag loop would pin subpages
owned by other allocations).

### ResolveFault page-sharing logic (ROM `0x001F7978`)

```
read FAR from this->[+64]->[+68]
range-check vs info->[+1c] (upper) and info->[+18] (lower)
offset       = FAR - info->[+14]
page_idx     = (offset >> 10) >> 2
subpage_idx  = (offset >> 10) & 3
page         = info->[+10][page_idx]
if (page == NULL):
    page = FindOrAllocPage_ReturnUnLockedOnNoPage(this, info, page_idx, mask=1<<subpage_idx)
    info->[+10][page_idx] = page
else:
    if (page->subpage_owner[subpage_idx] == info
        && high(page->subpage_info[subpage_idx]) == page_idx):
        // fast path
    else if (page->subpage_owner[subpage_idx] == NULL):
        SetSubPageInfo(this, info, page_idx, page, subpage_idx)
    else:
        CountMatches / collision-handling (subpage owned by *other* stack)
SetRestrictedPage; RememberMappings; release semaphore
if (this->[+0xC0] != 0): page[+40 + subpage_idx]++   ; lockcount on resume
```

Two existing ROM patches close the cross-stack sharing path: 
`apply_resolve_fault_wrapper` runs ResolveFault 4× per stack fault
(one per subpage), and `GetMatchingPage→0` short-circuits the
page-sharing search inside `FindOrAllocPage` so every fresh allocation
returns a brand-new page. Together these guarantee that any page
backing a stack-fault is owned by ONE stack across all 4 subpages.

### LockHeapRange call-site catalog

29 distinct `bl/b 0x01BD6B54` sites in the ROM. Grouped by intent:

**Pattern A — Lock my object's data area** (15 callers).
Driver / kernel objects calling `LockHeapRange(this, this+sizeof, lock_id)`
to pin their own footprint against paging:

| ROM PC | Owner | size (B) | lock_id |
|--------|-------|---------:|---------|
| `0x021B88` | `Init__4TADCFv` | 372 | 0 |
| `0x0388E0` | `New__15TAsyncDebugLinkFv` | 32 | 1 |
| `0x058018` | `Allocate__10TCircleBufFUliUcT3` | 40 | derived |
| `0x058F48` | `Init__20PCirrusBatteryDriverFv` | 156 | 0 |
| `0x05B1E4` | `Init__16TResistiveTabletFRC4Rect` | 180 | 0 |
| `0x0B1DEC` | `Init__9TFIQTimerFv` | 96 | 1 |
| `0x0DB20C` | `New__17TGeoPortDebugLinkFv` | 72 | 1 |
| `0x0E8D38` | `Init__9TIRQTimerFv` | 216 | 0 |
| `0x0EB70C` | `Init__10TICHandlerFUl` | 60 | 0 |
| `0x1D5D6C` | `Init__16TSerialChip16450FP11TCardSocketP12TCardHandlerPUc` | 92 | 1 |
| `0x1D63C0` | `Init__19PTheSerChipRegistryFv` | 92 | 1 |
| `0x1D69C0` | `InitByOption__18TSerialChipVoyagerFP7TOption` | 160 | 0 |
| `0x1D95EC` | `Init__16TSerialDMAEngineFP21TDMAChannelDiscriptorPvUc` | 64 | 1 |
| `0x26C5F4` | `New__20TVoyagerMiscIntfImplFv` | 36 | 0 |
| `0x26CEB4` | `Init__16TVoyagerPlatformFv` | 260 | 0 |

These objects live ON HEAP MEMORY (allocated via operator new). With
the existing NewHeap chunk_size=4096 patch, each heap exclusively owns
its 4-KiB pages, so multiple driver objects on one page are all
"owned" by the same heap → no subpage-AP boundary crossing. **Not
1-KiB allocators.**

**Pattern B — Lock a stack range** (5 callers):

| ROM PC | Owner | size | lock_id |
|--------|-------|------|---------|
| `0x12427C` | `Init__16TMuxStoreMonitorFP6TStore` | 2 KiB straddling SP | 0 |
| `0x1B940C` | `TaskConstructor__8TSerToolFv` | virtual-call return | 1 |
| `0x2523B8` | `Init__5TTaskFPFPvUlT2_vUlPvN32P12TEnvironment` (full stack) | stack span | 0 |
| `0x2523D0` | same TTask::Init (first 48 B of stack) | 48 | 0 |
| `0x1F8B30` | `LockStack` (wrapper) | from kernel globals | 0 |

The stack allocator (TStackManager via the NewStack SWI →
FMNewStack) is what determines stack page granularity, not these lock
callers. The existing `apply_resolve_fault_wrapper` already handles
stack-fault pages at 4-KiB.

**Pattern C — Lock a heap range** (5 callers):

| ROM PC | Owner | size | notes |
|--------|-------|------|-------|
| `0x1423D4` | `NewVMHeap` | r5 | r5=4096 after our existing patch |
| `0x3109E0` | `ExtendVMHeap` | round_up(req, heap[+0x38]) | heap[+0x38]=4096 after our patch |
| `0x1C5E7C` | `GrowByOnePage__15SWiredHeapDescrFv` | 4096 | always 4 KiB |
| `0x143068` | `LockPtr` | `GetPtrSize()` | caller-determined |
| **`0x1428DC`** | **`ZapHeap`** | r4 (1024 OR 4096) | **PATCHED — see audit below** |

**Pattern D — Lock a domain region** (2 callers): `BuildDomainsAndHeaps`
at `0xE9400` and `0xE9688`. Domain Base/Size are 1 MiB-aligned (ROM
enforces via `lsls #12` at `0xe9264`); 4-KiB-aligned by construction.

**Pattern E — Lock malloc'd buffer + object** (2 callers):
`Init__17THistoryCollectorFUiPcT2iT4` at `0x2DC57C` (locks the malloc'd
buffer of variable size) and `0x2DC598` (locks the 108-byte object).

### 1-KiB allocator audit

Every code path in the ROM that `mov`s a literal 1-KiB to a register
used in a heap/stack allocation, with status:

| ROM PC | Owner | What | Status |
|--------|-------|------|--------|
| `0x142398` | `NewVMHeap` | `mov r5, #1024` default chunk size | **handled** by `0x1423A0 beq→nop` patch (forces 4-KiB arm) |
| `0x142828` | `NewHeapAt` | `mov r2, #1024` chunk_size for NewHeap call | **handled** indirectly — NewHeap chunk_size patch overrides r2 to 4096 |
| `0x1428B8` | `ZapHeap` | `moveq r4, #1024` (chunk + lock size when caller flag=0) | **PATCHED THIS ITERATION** — force `mov r4, #4096` |
| `0x1F6E78` / `0x1F6E7C` | `Init__13TStackManagerFv` | `mov r3,#1024; mov r2,#1024` passed to TUDomainManager::Init | NOT an allocation — domain-manager configuration thresholds |
| `0x1F6BA8`, `0x1F6BC0`, `0x1F6C08` | `FMLockHeapRange` | `add/sub r6/r9, #1024` per-subpage step | per-subpage iteration internals; not a 1-KiB allocation request |
| `0x1F6A7C`, `0x1F6AA8`, `0x1F6FD0`, `0x1F7030`, `0x1F72F4`, `0x1F7D6C`, `0x1F7E4C`, `0x1F7E84` | `FMSetHeapLimits` / `FMGetSystemReleaseable` / `FreeSubPagesBetween` / `ReleasePagesInOneStack` | per-subpage iteration internals | same — not allocation requests |

Allocators audited and confirmed NOT to use 1-KiB chunking:
`SSafeHeapPage`, `SWiredHeapPage`, `NewBlock`, `SetBlockSize`, `malloc`,
`NewPtr`, `NewWiredPtr`, `AllocNewPage`, `TUPageManager::Get`. All
allocate at 4-byte (object) or 4-KiB (page) granularity.

## End-to-end page allocation — `AllocNewPage` → `TUDomainManager::Get`

This decodes the call chain that produces a fresh `TStackPage*`
when `ResolveFault` discovers `page_table[page_idx] == NULL`.
Useful for understanding **where the kernel's "free physical
page" pool lives** — the place that, if it returns the same PA
twice, produces our verify-mmu aliases.

### Pseudocode (decoded from ROM)

```c
// TStackManager::AllocNewPage  — 0x001F8788
TStackPage* AllocNewPage(TStackManager* this, ULong domain_id) {
    TStackPage* p = __ct__10TStackPageFv(/*r0=*/0);   // calloc'd ctor
    if (!p) return NULL;
    if (Init__10TStackPage(p, this, domain_id) != 0) {
        // Init failed → destruct and return NULL.
        __dt__10TStackPageFv(p, /*r1=*/1);
        return NULL;
    }
    return p;
}

// Init__10TStackPageFP15TUDomainManagerUl  — 0x001F9524
//   r0 = TStackPage* (this)
//   r1 = TUDomainManager* (or TStackManager*?  argument named "P15TUDomainManager")
//   r2 = page_id (when caller already has a PA, e.g. for boot setup);
//        when r2 == 0, request a FRESH page from the domain manager.
ULong Init__10TStackPage(TStackPage* this, void* mgr, ULong page_id) {
    if (page_id != 0) {
        // Pre-existing PA: just record it and clear the "fresh-allocation"
        // flag at this[+0x30] bit 0x04000000.
        this[+0]   = page_id;
        this[+0x30] &= ~0x04000000;
        return 0;
    } else {
        // Set the "fresh-allocation" flag so cleanup knows to free the page.
        this[+0x30] |= 0x04000000;
        // Tail-call into the domain manager's free-page allocator.
        // Args: (mgr, &this, 2)  — 2 means "give me a page".
        return TUDomainManager::Get(mgr, &this, /*r2=*/2);
    }
}
```

`TUDomainManager::Get`'s body is at base ROM `0x00258EC0`
(NOT in REx — the `0x01BD2974` reference is the post-ship
patch-table thunk; see `docs/DISASM.md` "Jump-table aliasing").
On return, `*&this == new_PA`.

The body is a thin SWI shim:

```c
ULong Get(TUDomainManager* this, ULong& out_pa, int count) {
    ULong msgbuf[3] = {
        /*[0]=*/ *(ULong*)0x0c101054,    // gPagePoolHandle
        /*[1]=*/ (ULong)count,           // = 2 in NewStack path
        /*[2]=*/ (ULong)(this + 24),     // domain field pointer
    };
    if (MonitorDispatchSWI(/*r0=*/ *(void**)0x0c104eec,
                           /*r1=*/ 5,            // msg = "Get a page"
                           /*r2=*/ msgbuf) == 0) {
        out_pa = msgbuf[0];               // return value in-place
        return 0;
    }
    return error_code;
}
```

`MonitorDispatchSWI` is `svc #0x1B`. `*0x0c104eec` is a
**kernel monitor handle** (an integer ID, not a pointer);
`StaticInit__15TUDomainManager` copies it in via
`CopyObject__9TUMonitor`, which writes the handle directly to
`this[0]`. The kernel SWI handler at `svc #0x1B` looks up the
proc registered for that handle and calls it.

`PageMonProc__15TUDomainManagerFlPv` at `0x0025925C` is **NOT**
the allocator-side handler. It's the LOCAL `TUDomainManager`'s
own monitor-procedure stub, run when this domain is itself
called *as* a fault monitor:

```
PageMonProc(this, msg, args_buf):
  if msg == 0x7FFFFFFF:               // init message
      jump *(*this + 8)               // = vtable[2] handler
  else:                                // normal call
      jump *(*this + 4)               // = vtable[1] handler
```

The page-allocator handler that produces a fresh PA on the OTHER
side of the SWI is registered into the global page-monitor at
runtime by `RegisterPageMonitor__15TUDomainManagerSFv` (at
`0x00259094`) via `MonitorDispatchSWI(*0x0c104eec, msg=3, ...)`.
The actual allocator proc lives in kernel-mode code reached via
that registration; it is not directly visible in `rom.dis` (kernel
runtime registration, not a static constant).

To inspect it, add a hypervisor-side `svc #0x1B` trap that
filters on `r0 == *0x0c104eec && r1 == 5` and logs the (caller,
args_in, returned_PA) tuple per call. See PLAN.md "Static
analysis dead end — proceed via runtime probe" for the full
plan.

### Where aliasing actually originates

After the existing patches (`apply_resolve_fault_wrapper` claims
all 4 subpages atomically; `GetMatchingPage→0` short-circuits the
page-sharing search), every call to ResolveFault that hits
`page_table[idx] == NULL` flows: `FindOrAllocPage` →
`AllocNewPage` → `Init__10TStackPage(mgr, /*page_id=*/0)` →
`TUDomainManager::Get(mgr, &out, 2)`.

If `TUDomainManager::Get` ever returns the same PA twice across
different `TStackInfo*` consumers, the result is an alias: two
different L2 entries (in different page_table arrays) both
naming the same PA.

**Observed in the FMNewStack 33→36 KiB patch attempt
(2026-04-28):** with the patch, fresh stack allocations
naturally consume 4-KiB-aligned 9-page slots (no stack-stack
guard sharing — verified from NewStack POST-SWI traces). But
the kernel's later heap activity (`ExtendVMHeap` faulting on a
new heap page → ResolveFault → AllocNewPage → Get) **gets the
same PA back that an earlier stack allocation had received**.
The trace correlation shows the alias appears immediately
after `ExtendVMHeap` for an 8-KiB heap, with `info_bounds=
[0x0c201000, 0x0c203000)`.

The only way one PA gets handed out twice is if
`TUDomainManager::Get` either:

a) Has a free-list that recycles pages (and our patch causes a
   page to enter that free-list when the kernel believes its
   slot was vacated).
b) Has internal bookkeeping that desyncs from our slot resize
   — for example, a slot-count or per-slot-page-budget value
   that we didn't audit.
c) Is implemented in REx (RAM-resident patch) where our base-
   ROM patches don't apply.

**To investigate next:** disassemble TUDomainManager::Get's
real body (probably REx-side; check `_Data_/Einstein.rex` byte
range for the function's actual address). Look for any
slot-size constant or page-counting logic that we might be
desyncing with.

## TProcessorState (DABT-time saved-context struct)

Built by `DataAbortHandler` (ROM `0x00393114`) and `PrefetchAbort-
Handler`. Total length is 100 bytes (`0x64`) — confirmed by
`GetFaultState__FP15TProcessorState` at ROM `0x0011fe3c` calling
`SMemCopyFromSharedSWI` with `r2 = 100`. Passed by reference into
`TStackManager::Fault(TProcessorState&)` (ROM `0x001F83E4`) and the
peer `TROMDomainManager1K::Fault` (ROM `0x001AEEDC`).

### Confirmed fields (Phase B iter 15, hypervisor `Fault(stackmgr)` probe)

```c
struct TProcessorState {            // 100 bytes total (0x64)
    // 0x00..0x3F: presumed user-banked register file (16 words).
    //              Not yet observed at probe time — fault path saves
    //              registers before invoking Fault, so the slots
    //              are populated by the time the dispatcher runs.
    //              (Open: confirm whether r15/PC is at +0x3C.)
    u32         saved_cpsr;         // +0x40   pre-abort CPSR (NZCV..mode bits)
    u32         far;                // +0x44   FAR_EL1 captured at abort
    u32         dfsr;               // +0x48   DFSR (e.g. 0x47 = write,
                                    //         status=0b00111 → page L2 fault)
    u32         _4c;                // +0x4c   small constant (observed = 4)
    u32         saved_sp_usr;       // +0x50   user-mode SP at fault time
    u32         env_id;             // +0x54   gCurrentTaskEnv (e.g. 0x13a5)
    u32         _58;                // +0x58   task-id-ish (observed 0x30f3 / 0x1843)
    u32         status;             // +0x5c   abort-source flags;
                                    //         bit 25 (0x2000000) tested by
                                    //         Fault @ 0x1F8420 to discriminate
                                    //         instruction- vs data-abort
};
```

Citations:
- `Fault @ 0x1F8418`: `str r5, [r4, #0x40]` saves the procst pointer
  in the manager (manager+0x40, NOT procst+0x40).
- `Fault @ 0x1F841C`: `ldr r1, [r5, #0x5c]` reads procst→status.
- `Fault @ 0x1F8438`: `ldr r1, [r0, #0x58]!` (then implicit
  pre-update `[r0, #0x50]` via writeback chain).
- `Fault @ 0x1F846C`: `ldr r1, [r0, #0x44]!` reads procst→FAR (this
  is the canonical FAR access used downstream by `GetStackInfo`).

### Probe output reference

A live capture from `handle_stack_mgr_fault_probe_with` looks like:

```
Fault(stackmgr) probe ENTER: this=0x0c112cb8 procst=0x0c1133a4 \
  pc=0x20000110 far=0x0c647003 status=0x02800000 \
  saved_sp=0x0cc77700 caller_lr=0x00259230 src_mode=0x10 (USR) sp=0x0c1133a4
Fault(stackmgr) procst[+0x40..+0x60]: 20000110 0c647003 00000047 \
  00000004 0cc77700 000013a5 000030f3 02800000
```

`pc=0x20000110` here is the *saved CPSR*, not a PC — `0x20000110`
decodes to N=0 Z=0 C=1 V=0, mode=0x10 (USR). To recover the actual
faulting USR PC from EL2, use `lr_abt - 8` (or, when the fault came
through the SBA stub pool, decode slot 14 of the containing stub for
the original ROM PC; see `src/trap.rs::handle_data_abort` forwarding
path).

## TUnicodeCompressor (ROM `Sizeof` at `0x00256C74`)

Total size **420 bytes** (`mov r0, #420` at `0x00256C74`). Used by
the kernel's Unicode-string compression path (`WriteRun` at
`0x00256EEC`, `WriteChunk` at `0x0025700C`, `Flush` at
`0x0025719C`).

```c
struct TUnicodeCompressor {         // 420 bytes (0x1A4)
    u32         _vtable;            // +0x00
    // ...
    u8          buffer_a[?];        // +0x18  byte-buffer accessed via r6=this+24
    // ...
    u32         count;              // +0x9c  loop bound (validated by WriteRun)
    u8          flag_a0;            // +0xa0  byte (zeroed by Reset)
    u8          buffer_b[?];        // +0xa1  byte-buffer accessed via this+0xa1+r5
    // ...
    u8          end_marker;         // +0x121 byte set/cleared by Reset
};
```

Citations:
- `Sizeof = 420` at `0x00256C74`.
- `Reset` at `0x00256ED8` zeroes `+0x98 (4-byte)` and `+0xa0 (byte)`,
  then `str r1, [r0, #156]!` zeroes `+0x9c` (count).
- `WriteRun @ 0x00256F94..0x00256FFC` iterates the byte buffer at
  `[this+0xa1+r5]` for `r5 = 0..count-1`.

**Phase B iter 17 wedge.** A 420-byte instance was placed at
`0xc646bfc..0xc646da0` (NewBlock #656) — fully within the heap
top of `0xc647000`, NOT spanning it as iter 15 had hypothesised.
The wedge at FAR=`0xc647003` corresponds to the `WriteRun` loop
reaching iteration `r5=870`, which requires `count >= 871` at
ROM `0x256FF8`'s `bhi` check. Since `WriteChunk` caps count at
255 and `New`/`Reset` zero it, count > 870 means the compressor
was used WITHOUT calling New/Reset — count holds heap-garbage
from `NewBlock`'s freelist memory.

The `+0xa1` buffer access then reads bytes
`compressor + 0xa1 + r5` for r5=0..870. Byte 870 lands at
`0xc646bfc + 0xa1 + 870 = 0xc647003` — past the heap top.

The fault MECHANISM: the loop overruns the compressor object
(420 bytes) by hundreds of bytes, walking through whatever
follows in the heap, and eventually hits unmapped memory.

## See also

- `INVESTIGATION.md` — live wedge debugging notes
- `src/task_dump.rs` — runtime walker that materializes the above
- `docs/DISASM.md` — how to use `scripts/disasm-out/rom.dis`
- `/Users/walter/Projects/newton/ghidra/DDKIncludes/OS600/` — public
  DDK headers (Apple, 1995). Useful for class names and high-level
  shape; **field offsets must be verified against 717006 binary.**
