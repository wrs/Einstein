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

## See also

- `INVESTIGATION.md` — live wedge debugging notes
- `src/task_dump.rs` — runtime walker that materializes the above
- `docs/DISASM.md` — how to use `scripts/disasm-out/rom.dis`
- `/Users/walter/Projects/newton/ghidra/DDKIncludes/OS600/` — public
  DDK headers (Apple, 1995). Useful for class names and high-level
  shape; **field offsets must be verified against 717006 binary.**
