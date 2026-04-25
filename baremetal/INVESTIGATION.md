# Phase B boot-stall investigation

Live notes. Update as we learn more; remove old updates as we move on to
new stalls.

## Currently at — pckm task at sp_usr=0x0cc7a248 reads TAEventHandler bytes instead of stack frame (QEMU + FVP, 2026-04-27)

**Root divergence narrowed**: the recursive "newt" DABT
(FAR=0x6e657774) is caused by the pckm task (id=0x1753, struct at
0x0c118dd8) resuming with sp_usr=0x0cc7a248 and reading user RAM at
sp+8 / sp+12 that contains the literal ASCII fourccs `'newt'` and
`'cdsv'` instead of the stack pointers TUPort::Receive's prologue
(0x259d2c) should have pushed there.

### Evidence (one-shot diagnostic dump in DABT-fast-path)

`src/task_dump.rs::dump_save_area_for_named` fires once at the FAR=
0x6e657774 forward and prints the SWIBoot context-save area
(task+0x10..0x54) plus a ±0x80 user-stack window plus a stage-1 walk.

For task `0x0c118dd8` (id=0x1753, named `cdsv` in our run, named
`pckm` in Einstein's run — same struct slot, same task throughout
the boot, just different `find_task_name` heuristic hits as the
globals area gets repopulated by the AppWorld over time):

```
Our hypervisor                          Einstein (NewtonProbe)
sp_usr  = 0x0cc7a248                    sp_usr  = 0x0cc7a248        (SAME)
saved-PC = 0x003ae230                   saved-PC = 0x003ae230        (SAME)
lr_usr  = 0x00259d48                    lr_usr  = 0x00259d48         (SAME)
fp/ip   = 0x0cc7a29c / 0x0cc7a2b0       fp/ip   = 0x0cc7a29c / 0x0cc7a2b0  (SAME)

stage-1 walk: VA 0x0cc7a248 → PA 0x0402a248
                                stage-1 walk: VA 0x0cc7a248 → PA 0x0402a248  (SAME PA)

user-stack window @ sp_usr:
  [+0]=0  [+4]=0                        [+0]=0x0c600d2c  [+4]=0x0c600d1c  (pushed r4,r5 from PortReceiveSWI)
  [+8]=0x6e657774  ("newt")             [+8]=0x0cc7a270   (push of r0=sp+16 from 259d2c)
  [+12]=0x63647376 ("cdsv")             [+12]=0x0cc7a26c  (push of r1=sp+12)
  [+16..+20]=0,0                        [+16]=0x0cc7a264  [+20]=0x0cc7a268  (push of r2,r3)
```

Both implementations pick the same VA→PA, save the same context.
Only the contents at PA 0x0402a248..0x0402a25f differ. Einstein has
the four valid stack pointers from TUPort::Receive's `push {r0..r3}`;
ours has the literal pattern of a `TAEventHandler{ signal='newt',
class='cdsv', ...}` (signal at +0x08, class at +0x0c — see
`docs/STRUCTURES.md` "TAEventHandler"). Trace 183155 in our run is
the only `TAEventHandler::Init(handler, 'cdsv', 'newt')` call, but
its handler address was `0x0c602e2c`, not `0x0cc7a248` — so the
pattern at PA 0x0402a248 came from somewhere else.

### Faulting site

When pckm resumes at PC=0x3ae230 (= post-`svc #2` in `PortReceiveSWI`
at 0x3ae228):

```
003ae228 <PortReceiveSWI>:
  3ae228: push {r4, r5}
  3ae22c: svc  #2
  3ae230: ldr  r5, [sp, #8]    ; r5 ← 0x6e657774 ("newt")
  3ae234: cmp  r5, #0
  3ae238: strne r1, [r5]       ; ← DABT here, FAR=0x6e657774, DFSC=0x05
                                 ;   (translation, section — no L1 entry
                                 ;    for the 0x6e000000..0x70000000 range)
```

The L1 fault recurses through DataAbortHandler → ConvertIdToObj →
Throw → UnhandledException → "Unhandled exception evt.ex.abt.bus,
warm reboot!".

### Root cause confirmed: stage-1 page-table aliasing

Per-trace-event tripwire (`src/tracer.rs::log_trace_at`) bisected the
write to **trace 180652** (= `TCardMessage::Clear` entry — but the
write actually happened in the prior trace event):

```
trace 180650 0x0004ed10 TCardMessage::TCardMessage(void) (usr) r0=0x0cc82250 ...
trace 180651 0x00025d1c TAEvent::TAEvent(void)         (usr) r0=0x0cc82250 ...
trace 180652 0x0004ed84 TCardMessage::Clear(void)      (usr) r0=0x0cc82250 r1=0x6e657774 ...
*** newt-tripwire fired AT trace 180652 (PA 0x0402a250=0x6e657774 0x0402a254=0x63647376)
```

`TCardMessage::TCardMessage` at 0x0004ed10 explicitly stores
"newt"+"cdsv" into its `self`:

```
4ed3c: ldr r0, [pc, #44]    @ 0x4ed70 = 0x6e657774 ('newt')
4ed40: str r0, [r4]          ; *(self+0) = 'newt'
4ed44: ldr r0, [pc, #40]    @ 0x4ed74 = 0x63647376 ('cdsv')
4ed48: str r0, [r4, #4]      ; *(self+4) = 'cdsv'
```

with `self = r4 = 0x0cc82250` for this allocation. The two literals
are the magic class IDs used to identify TCardMessage in untyped
buffers (the constructor calls them after the TAEvent base ctor and
before its own `Clear`).

**The kicker — page-table alias:**

```
*** stage-1 walk for VA 0x0cc82250 (TCardMessage write target):
  L1[0xcc] = 0x04023481  (coarse, L2 @ PA 0x04023400)
  L2[0x82] = 0x0402a03e  (small)
  → PA 0x0402a250

*** stage-1 walk for VA 0x0cc7a250 (pckm sp_usr+8 read site):
  L1[0xcc] = 0x04023481  (same coarse table)
  L2[0x7a] = 0x0402a03e  (small) ← same PA
  → PA 0x0402a250
```

`L2[0x7a]` and `L2[0x82]` of the same kernel L2 table both map to PA
0x0402a000. So a write through VA 0x0cc82250 lands at the same
physical page that backs pckm's user-stack VA 0x0cc7a000. When pckm
next resumes and `PortReceiveSWI` reads `[sp_usr+8]`, it reads the
"newt"/"cdsv" magic from the TCardMessage instead of the stack
pointer that `TUPort::Receive` 0x259d2c pushed there.

This is a *kernel-side* divergence — the kernel's heap/page allocator
picked PA 0x0402a000 for the new TCardMessage even though that page
was already mapped at VA 0x0cc7a000 as pckm's stack. Einstein doesn't
do this, so its L2 entries don't alias.

### Open next steps

1. **Find the diverging allocation.** Walk back from the TCardMessage
   alloc (`__nw__FUi(184)` at trace 180650) and identify why the
   kernel's TPageManager / heap chose PA 0x0402a000. Compare against
   Einstein's allocation order.
2. **Bisect the earlier divergence.** The two implementations agree
   on L1/L2 layout for many earlier pages. The first L2 entry that
   diverges between Einstein and our hypervisor is the clue. Add a
   diagnostic that dumps both L1 + L2 contents at periodic
   intervals and diff against Einstein's NewtonProbe.
3. **Investigate likely peripheral-state-driven divergence.** The
   `TNewCardAsyncMsg` chain is in the PCMCIA card-insertion path
   (`TCardSocket::~TCardSocket`, `TCardAlertEvent`, `TCardPart-
   Handler` were already traced as new-territory functions before
   the fault). Our PCMCIA driver returns different state than
   Einstein's, plausibly steering the heap allocator down a path
   that reuses pckm's stack page.

---

## Earlier — kernel-mode "newt" UnhandledException (QEMU + FVP, 2026-04-26)

After resolving the STKU wedge (see "Resolved — STKU wedge: QEMU
Bug #1 leak from unaligned `msr spsr_el2`" below), QEMU now reaches
the same `0x6e657774` ("newt" ASCII) recursive kernel-mode DABT that
FVP has hit since Apr 24 — both platforms now agree.

A fresh cold-boot QEMU trace runs to ~213k entries with **1262
unique functions** (vs. ~1087 / 156k pre-fix), advances `gCurrentTask`
past STKU → cdsv → and into `Throw`/`UnhandledException` /
`Subexception` / `__vfprintf` user-mode reporting code. The trace
ends with the Reboot canary firing at IPA 0x00FFFF58, mode UND:

```
trace 213650 0x00393114 DataAbortHandler (abt) ... lr=0x003ae240
trace 213652 0x0011fc60 FaultMonitorEntry(unsigned long) (abt) ...
trace 213657 0x00250864 RebootIfFaultWasInStack (abt) r0=0x6e657774 ...
trace 213658 0x000b00c8 Throw (usr) r0=0x000afda0 r1=0x6e657774 ...
trace 213663 0x000b0220 UnhandledException(char *, ...) (usr) ...
putc 213671..213722: "Unhandled exception evt.ex.abt.bus, warm reboot!"
```

Decode (from the prior FVP-side finding still applies verbatim):

- Faulting PC = `LR_abt - 8 = 0x259d40` = `ldr r0, [r0]` in
  `TUPort::Receive` (just before `bl PortReceiveSWI` at `0x259d44`).
- Faulting VA = `0x6e657774` = "newt" ASCII — `r0` was loaded from
  `[fp, #4]` (caller's saved arg0 = `self`) and dereferenced. The
  TUPort `self` pointer is occupying "newt" ASCII bytes.
- mode=0x17 (ABT) → recursive abort: kernel was inside its DABT
  handler when the next access faulted.

Pre-failure path now passes through `cdsv` task initialisation
(`SwapInGlobals 0xc118dd8 → 0x00393114 DataAbortHandler` at trace
213649/213650), which means the cdsv task struct itself contains
"newt" ASCII at the offset that `TUPort::Receive` dereferences. This
is the same Apr 24 finding from the QEMU run that briefly
"transient-cleared" before the regression we just resolved hid it.

### Open next steps

1. Read the saved `cdsv` task struct at `0xc118dd8` — specifically
   the per-task-globals area at `task+0xa0` and the TUPort field that
   `TUPort::Receive` reads — to identify which slot holds the "newt"
   ASCII bytes.
2. Find which symbol-table entry holds the literal `0x6e657774`
   pattern (per Apr 24 hypothesis: a runtime symbol-name lookup
   returned a name string instead of a code/data pointer). The
   symbol prefixes `newtConnects`, `SYMnewtaboutview`, `SYMnewtinfobox`
   are candidates.
3. Walk back from the SwapInGlobals at trace 213649 in the function
   trace to find what *created* the cdsv task and what it intended to
   pass as the TUPort `self`. The corruption could be the kernel
   storing the symbol name into a pointer slot (off-by-one in a
   shared-memory layout), or our hypervisor mishandling a prior
   write to that slot (symbol-table region or RAM page alias).

## Resolved — STKU wedge: QEMU Bug #1 leak from unaligned `msr spsr_el2` (2026-04-26)

The STKU page-copy SWI wedge (PC=0x3ae1bc / SVC mode, persistent for
several minutes) was caused by **QEMU Bug #1** triggered from
`unaligned::set_return`. The fix is in `src/unaligned.rs::set_return`:
delegate to `trap::return_to_guest_from_und`, which ERETs into the
existing `UND_RETURN_STUB` at IPA `0x00FFFFE4` while leaving SPSR_EL2
untouched. The mode switch happens AArch32-side via `movs pc, lr` and
never goes through QEMU's buggy MSR helper.

### Root cause

The Newton ROM has ~1300 sites that depend on SA-1100 rotate-LDR
semantics for unaligned word loads (`UstrlenPrivate` at 0x1944b8
alone fires a fault on every other call — UTF-16 strings are 2-byte
aligned). With `SCTLR_EL1.A` forced on, each unaligned LDR raises an
alignment fault that the DABT-vector trampoline forwards to EL2 via
`HVC #ALIGN_TAG`. `handle_align_fault` decodes and emulates the
load, then called `set_return` to `msr elr_el2 / msr spsr_el2` and
ERET back to the pre-fault mode.

Per `docs/QEMU_BUGS.md` Bug #1, **`msr spsr_el2, x` from EL2 leaks
`x` into AArch32 SPSR_svc (banked_spsr[1])**. A direct probe
(`mrs spsr_el1` before/after the buggy write) confirmed the leak:

```
qemu-clobber-probe[4]: SPSR_EL1 pre=0x000001d3 post=0x000001d3 (wrote spsr_el2=0x200001d3)
qemu-clobber-probe[5]: SPSR_EL1 pre=0x200001d3 post=0x200001d3 (wrote spsr_el2=0x600001d3)
qemu-clobber-probe[6]: SPSR_EL1 pre=0x600001d3 post=0x600001d3 (wrote spsr_el2=0x800001d3)
```

(Each pre value matches the previous probe's wrote-value — the leak
is exact.) Pre-fault mode at every observed alignment fault was
0x1d3 = SVC, so SPSR_svc was being clobbered to a SVC-mode value
during the kernel's SVC handler. When the SVC handler eventually ran
its `movs pc, lr` epilogue (at 0x3ada6c / 0x3adb10 in `SWIBoot`),
CPSR was restored from the corrupted SPSR_svc → CPSR=SVC instead of
USR. The post-`svc #5` `mov pc, lr` at GenericSWI 0x3ae1bc then
self-looped because LR_svc = 0x3ae1bc and the instruction is the
non-mode-restoring form (no `s` suffix).

### Why FVP got past it before

FVP doesn't have Bug #1 — the AArch64 banked-SPSR helper handles
SPSR_EL2 writes correctly. So FVP's STKU iteration completed
normally and it advanced to cdsv → newt-exception. QEMU stuck at
STKU because every unaligned access during the SVC handler corrupted
SPSR_svc.

### Verification

- All 23 guest tests pass (no regression).
- Cold-boot QEMU trace: 213k entries / 1262 unique functions,
  task_dump shows `curr=0xc11b2c0` (cdsv) past the STKU state.
- Trajectory now matches FVP (both reach the "newt"
  UnhandledException as the next stall).

## Resolved (was) — wedge isolated to STKU monitor task body (QEMU, 2026-04-25 night)

### Pre-flight: restored DABT→kernel forward fast-path

The merge resolution dropped the DABT-forward fast-path from
`handle_diag` ("keep mz banked-register fixes, drop mn DABT-forward
fast-path"). On a fresh boot that drop wedges on the **first** non-
alignment DABT — the SetFreeChain APCS prologue's `STMFD sp!,
{...,fp,ip,lr,pc}` crossing into an unmapped page below `SP_usr=0x0cc7a010`
(FAR=0x0cc79ff4, DFSC=0x07, page-translation fault). Newton's own
`DataAbortHandler` at `0x0039_3114` is the legitimate handler for that
class of fault; the hypervisor's DIAG halt was a Phase-B trip-wire,
not the right behaviour for routine on-demand paging.

Restored the fast-path in `trap.rs::handle_diag`:

- Source-mode gate: only forward when HVC source mode is `MODE_ABT`.
  guest_bp UND-source hits and PABT-vector hits still take the loud
  halt.
- DFSC gate: `0x03 | 0x05 | 0x06 | 0x07 | 0x0D | 0x0F` (translation /
  permission / access-flag for both section + page).
- R0/R1 restored from TPIDR_EL0 / TPIDRRO_EL0 (the DABT trampoline
  stashed them there before clobbering with DFSR / SPSR_abt).
- `ELR_EL2 = 0x0039_3114`, then ERET; SPSR_EL2 stays as captured (mode
  ABT). LR_abt / SP_abt / SPSR_abt remain hardware-populated.
- Budgeted `dabt:` log dedups by (FAR, mode), 16 unique-pair cap.

After restore: a 90-s cold boot logs **one** DABT forward
(`DFSC=0x7 FAR=0x0cc79ff4 mode=0x17` — the SetFreeChain stack-extension)
and otherwise progresses through the same trajectory as the prior
investigation: trace ~156k entries, last unique user-mode call
`PSoundDriver::SoundOutputIH` (sound IRQ injection probe), wedged at
`PC=0x3ae1bc CPSR=SVC SP_svc=0x0c000400 LR_svc=0x3ae1bc`.

### New observation: LR_svc readback now reliable, and reads PC

The previous heartbeat used `MRS x, sp_el1` / `MRS x, elr_el1` and
returned `0` from EL2 IRQ context — flagged in `docs/QEMU_BUGS.md`.
The banked-reg overhaul replaced those with `ctx.x[19]` / `ctx.x[18]`
per ARM ARM Table D1-79, which gives architecturally-defined values
on both QEMU and FVP.

The reliable readback shows:

```
timer_irq[late]: ELR=0x3ae1bc SPSR=0x60000113 SP_svc=0x0c000400
                 LR_svc=0x3ae1bc FAR_EL1=0x0c116e66
                 intid=0 VI=0 ipres=0x40 ictrl=0xc401420 pend=false
```

`LR_svc == ELR == 0x3ae1bc`. That's the address of `mov pc, lr` (note:
no `s`, not `movs`) at the end of `GenericSWI`:

```
003ae174 <GenericSWI>:
  ...
  3ae1b8: ef000005   svc #5
  3ae1bc: e1a0f00e   mov pc, lr
```

Architecturally, when `svc #5` at `0x3ae1b8` fires, hardware sets
`LR_svc = 0x3ae1bc` and switches to SVC mode. Normal exit through
`GenericSWIHandler` does an `LDM SP!, {..., PC}^` that restores CPSR
from `SPSR_svc` (= the saved USR CPSR) and PC from the saved LR. After
that, `mov pc, lr` at `0x3ae1bc` runs **in USR mode** and falls back
to the user-mode caller via `LR_usr`.

The wedge state — `PC=0x3ae1bc, mode=SVC, LR_svc=0x3ae1bc,
SP_svc=0x0c000400` — is the smoking gun for one of:

1. The SWI epilogue used `LDM SP!, {..., PC}` (no `^`) or `MOV PC, LR`
   (no `s`), so CPSR is not restored and we stay in SVC. `mov pc, lr`
   at `0x3ae1bc` then jumps to `LR_svc=0x3ae1bc` — **infinite loop in
   SVC mode**.
2. A re-entrant `svc` somewhere in the SVC handler clobbered `LR_svc`
   to `0x3ae1bc`, and the outer return drops us at `0x3ae1bc` in SVC
   mode where `mov pc, lr` self-loops.

Either way the kernel is sitting in a tight `mov pc, lr` self-jump
in SVC mode, with sound DMA IRQs preempting the loop on each
heartbeat (no progress is made).

`SP_svc = 0x0c000400` is the BootOS-set initial SVC stack base —
matches "SVC stack frame fully unwound", so the handler did get to its
final pop before the issue.

### Why FVP got past it before, why QEMU doesn't

Per the prior FVP cross-check (180 s wall, run `mn` bad09ce3): on FVP
the STKU dump appears once (during the page-copy SWI) and then
`gCurrentTask` advances to `cdsv` (CardServer). On QEMU the wedge is
permanent. The recent banked-reg work did not change that — confirming
the wedge is a QEMU TCG behaviour at the AArch32 SVC return path,
specifically around how `LDM ... {pc}^` restores SPSR_svc to CPSR
when control re-enters AArch32 from EL2 IRQ-trap context.

`docs/QEMU_BUGS.md` Bug #1 (SPSR_svc clobber via `msr spsr_el2, x`)
is *not* the cause here: HVC and DABT round-trips are documented to
use the auto-saved SPSR_EL2 unchanged, and the SVC handler's
`LDM ... {pc}^` reads `banked_spsr[1]` directly. But the same QEMU
sub-system (banked SPSR plumbing across the AArch32↔AArch64 boundary)
is what's faulty.

### Open next steps

1. **Tarmac trace on FVP across one STKU iteration** (the prior plan
   from `mn` bad09ce3 — still pending). Capture the exact instruction
   sequence STKU executes after PhysSubPageCopy returns, so we know
   what the "correct" path looks like and can compare against QEMU.
   Specifically: does FVP also see `LR_svc = 0x3ae1bc` momentarily
   and recover, or does the kernel's SVC return path go somewhere
   different on FVP?
2. **Inspect `GenericSWIHandler` (0x000d8a64) tail** in ghidra MCP to
   find the SWI return idiom. If it uses `LDM SP!, {..., PC}^` and
   the wedge is QEMU's `^` plumbing dropping SPSR_svc on the floor,
   we have the bug isolated.
3. **Test on QEMU**: replace the SVC-handler return idiom in ROM
   patches with a hypervisor-mediated path (HVC → EL2 → re-construct
   correct CPSR + ELR → ERET). If that fixes the QEMU wedge, the
   bug is QEMU's `LDM {pc}^` semantics in TCG.

## Resolved (was) — wedge isolated to STKU monitor task body (QEMU, 2026-04-25 night)

Added `src/task_dump.rs`: walks the scheduler at `*0x0c100fd0`,
gCurrentTask at `*0x0c101000`, the per-priority TTaskQueues at
`gScheduler+0x1c+prio*8`, and decodes the task fourcc name from
STaskSwitchedGlobals.fTaskName (heuristic search a few words below
each task's `globals` pointer at `task+0xa0`).

At the wedge state, the dump consistently reports:

```
task_dump: gSched=0xc1084b4 curr=0xc113dd8 highest_pri=10 bitmap=0x400
           last_rem=0x0 want=0 hold=0 curr_glob=0xc11446c
  current:
  task 0x0c113dd8 prio=20 name=STKU globals=0x0c11446c q=0/0 stk_bot=0x0c114030
  prio 10 queue@0xc108520:
  task 0x0c119c74 prio=10 name=name globals=0x0c320a58 ...   (NameServer task)
  task 0x0c1180a8 prio=10 name=drvl globals=0x0cc82790 ...   (driver loader)
```

Key facts the dump establishes:

1. **Scheduler state is healthy**: `gWantSchedule=0`, `gHoldSchedule=0`,
   highest occupied priority = 10, bitmap=0x400 (only bucket 10 set).
   `gCurrentTask` = STKU at priority 20 with `q.next=0 q.prev=0` —
   correctly removed from the run queue while running.
2. **Newton priority convention** (verified from `TScheduler::Add`'s
   `cmp r0, r4 / movcc r0, r4` against `highest_pri`): higher number =
   higher priority. So STKU (prio 20) > drvl/name (prio 10) — the
   scheduler is right not to preempt with the lower-priority ready
   tasks.
3. **Only TWO ready tasks in the system** (drvl, name). Sound server,
   pkg, the TStackManager user etc. are all blocked on
   semaphores/ports — they don't appear in any per-priority run
   queue. (Walking blocked-task lists from semaphore wait queues
   would need the gObjectTable scan; not yet wired.)
4. **STKU's wq1/wq2 links are 0** at task+0xbc/0xc8 — STKU isn't
   waiting on a semaphore-queue or port-queue we know to look at.

So the wedge is **inside the STKU monitor task's execution body**,
not a scheduler/dispatcher bug. From the snapshot at PC=0x3ae1bc
LR_svc=0x1f7cc4 the call frame is `TStackManager::ResolveFault →
CopyPageAfterCollisionSWI → GenericSWI tail` — i.e., the SWI
returned (heartbeat fires *post*-svc-ret). The next instructions to
execute would be `add sp, sp, #40` then `b 0x1f7ab0`
(`Release(semaphore); ldrb [r4,#192]; …`) which loops back into
ResolveFault. None of that shows in the function tracer — the
loop body is either doing all of it inside the same already-traced
functions OR genuinely not executing.

QEMU SP_EL1 / ELR_EL1 readback from EL2 IRQ context returns 0
(documented QEMU bug — see `docs/QEMU_BUGS.md`). So the dump's
"SP_EL1=0 ELR_EL1=0" line is not informative on QEMU; FVP is the
only way to verify SP_svc/LR_svc directly.

Open next steps:

1. **Run on FVP** to (a) confirm the wedge reproduces, (b) read
   SP_EL1/ELR_EL1 reliably, (c) get a bounded tarmac trace across a
   single iteration of the supposed STKU loop body.
2. **Identify what makes STKU return to its idle/Receive loop** in
   Einstein. The smoking gun is below: in Einstein STKU is BLK
   (blocked), our hypervisor it's RUN forever. Find the SVC return
   path or unscheduling that we're missing.

### Einstein-vs-hypervisor task census (Phase B oracle, 2026-04-25)

`baremetal/probe/probe.cpp::task_dump` dumps the same scheduler
state on the Einstein side (every 2s). Diffing at matching boot
phases:

| field            | hypervisor (wedge) | Einstein (t=12s) |
|------------------|--------------------|------------------|
| total tasks      | 16                 | 29               |
| total kernel obj | 119                | 404              |
| gCurrentTask     | STKU id=0x12e3     | fser id=0x4793   |
| highest_pri      | 10                 | 12               |
| ready tasks      | 1 (drvl)           | 4 (Tmux, cdsv, scpl, codc) |
| STKU state       | **RUN** (stuck)    | **BLK** (idle waiting for next msg) |
| OBJM/PMGR/PTBL/STKF/STKP/STKU/ROMF/ROMP | all BLK (q=0/0 wq=0/0) | all BLK (same pattern) |

So **the wedge is STKU failing to return to its idle blocked state
after the CopyPageAfterCollisionSWI completes**. Einstein's STKU
finishes the same SVC, returns to its TUMonitor main loop, calls
some Receive() that blocks, and `fser` / `Tmux` / etc. take over.
Our hypervisor's STKU never reaches that block — it's stuck at
PC=0x3ae1bc in SVC mode, the post-svc-#5 `mov pc, lr` of GenericSWI.

**The empty-link `q=0/0 wq1=0/0 wq2=0/0` pattern IS the normal
blocked state in Newton**: blocked tasks have empty task-side
links and live only on the blocking object's (port/sem/etc.)
waiter queue. So our 14 BLK tasks are correctly blocked — STKU is
the one anomaly.

Tasks Einstein has that we don't (at this boot phase): Tmux, cdsv,
codc, cdfm, cdpr, pg&e, newt, pssm, scrn, inkr, cmgr, scpl, fser.
These are post-monitor-init tasks (GUI / ink / file server / power
mgmt) — boot can't reach them while STKU holds whatever resource
they're transitively waiting on.

### Investigation plan from here

The SVC handler ran ~110 traced functions inside CopyPagesAfter-
StackCollided and then stopped emitting traces after `_ExitFIQAtomic`
at trace 154686. The handler's return-to-user path normally:
1. Restores user-mode CPSR (USER) from SPSR_svc.
2. ERETs back to PC after the `svc 0x05` (= 0x3ae1bc).
3. Executes `mov pc, lr` → resumes user-mode caller at LR_usr.
4. Caller (TStackManager::ResolveFault @0x1f7cc4) cleans stack +
   loops back to Release semaphore + check for more work.
5. Eventually returns to TUMonitor::Main which calls Receive() to
   block until next request.

We're observing CPSR=SVC at 0x3ae1bc with LR_svc apparently
(via QEMU snapshot) = 0x1f7cc4. But the task-dump comparison says
this should ultimately end in STKU being BLK. So somewhere between
trace 154686 (last svc trace) and the would-be Receive() block,
control is lost.

Likely culprits to check next on FVP:
- `ldmdb fp, {…, pc}`-style multi-register restore in SVC handler
  exits — if the saved registers on the kernel stack are corrupted
  (bad page-copy interaction?) the wrong PC is restored.
- A `subs pc, lr, #4` from IRQ context that maps SPSR back to SVC
  mode by accident (we set HCR_EL2.IMO so EL2 takes IRQs — does
  the AArch32→AArch64 SPSR plumbing on QEMU corrupt the SPSR?).
- Re-entrant `svc` from SVC mode somewhere in the SVC handler
  itself, clobbering LR_svc — would make `mov pc, lr` self-loop.

FVP tarmac trace across the suspected wedge window would tell us
which.

### FVP cross-check (180s wall, 2026-04-25)

```
scripts/fvp --timeout=180 \
    target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

Periodic task_dump output:

```
task_dump: gSched=0xc1084b4 curr=0xc108624  highest_pri=0  bitmap=0x0     # OBJM, idle setup
task_dump: gSched=0xc1084b4 curr=0xc113dd8  highest_pri=10 bitmap=0x400   # STKU, same as QEMU wedge state
task_dump: gSched=0xc1084b4 curr=0xc11b2c0  highest_pri=0  bitmap=0x0     # cdsv (CardServer)
task_dump: gSched=0xc1084b4 curr=0xc11b2c0  highest_pri=0  bitmap=0x0
task_dump: gSched=0xc1084b4 curr=0xc11b2c0  highest_pri=0  bitmap=0x0
```

**FVP gets past STKU.** The STKU dump appears once (transient,
probably during page-collision handling) and then the scheduler
moves on to `cdsv` (CardServer). On QEMU, STKU stays as
gCurrentTask forever. So **the STKU wedge is QEMU-specific.**

After ~180s on FVP, boot crashes with a different failure: the
"newt" (`0x6e657774`) exception in `UnhandledException` —
matches the Apr 24 finding (kernel-mode DABT with FAR ASCII =
"newt") that was previously seen on QEMU. So FVP doesn't deadlock
on STKU but DOES hit a separate kernel-state corruption later.

**Most likely culprit for the QEMU-specific STKU wedge** (per
`docs/QEMU_BUGS.md` "AArch64↔AArch32 boundary"): the EL2 trap entry
or ERET path mishandles the SVC-mode banked LR (`R14_svc`). When an
IRQ is taken from SVC-mode AArch32 to AArch64 EL2, ELR_EL2 holds
the trap PC (= 0x3ae1bc) and SPSR_EL2 holds the AArch32 CPSR
(= 0x60000113 SVC). On ERET back, `R14_svc` should be unchanged
from before the trap. If QEMU is corrupting it (or our trap stub
inadvertently writes through `LR_svc`), `mov pc, lr` at 0x3ae1bc
would jump to a wrong PC.

Two falsifiable next steps to confirm:

1. **Save+restore SP_EL1 / ELR_EL1 explicitly** in the EL2 IRQ
   trap stub to bypass any QEMU bug. If the QEMU wedge clears,
   the bug is in QEMU's banked-reg plumbing.
2. **Tarmac trace on FVP across one STKU iteration** — capture the
   exact instruction sequence STKU executes after SVC return so
   we know what the "correct" path looks like and can compare
   against QEMU's stuck state.

Lower priority once the wedge is QEMU-side: the FVP "newt"
exception (kernel state corruption around `gCurrentGlobals`-relative
addressing) — that was previously chased on QEMU and presumably
masked when the STKU wedge started covering it. We'll see it again
when the wedge is fixed.

## Resolved (was) — sound subfn map known; wedge in StackManager page-copy persists (QEMU, 2026-04-25 late)

Captured the actual native-primitive subfn sequence the Newton kernel
exercises during sound init by adding "first-occurrence" logging in
`peripherals/sound.rs::handle`:

```
sound: first subfn 0x1f @PC=0x8013f8 r1=0x400 r2=0x1000 r3=0xc401420
sound: first subfn 0x5  @PC=0x8011f0 r1=0xcc84140 r2=0xea0 r3=0xcc85030
sound: first subfn 0x6  @PC=0x801204 r1=0xcc86030 r2=0xea0 r3=0xcc87030
sound: first subfn 0xa  @PC=0x801254 (PowerOutputOff)
sound: first subfn 0xc  @PC=0x80127c (PowerInputOff)
sound: first subfn 0x1e @PC=0x8013e4 (InputIntHandler  — only after our injection fires INT_DMA3)
sound: first subfn 0x1d @PC=0x8013d0 (OutputIntHandler — only after our injection fires INT_DMA5)
```

So the kernel's sound init goes:
1. `NativeSetInterruptMask(input=INT_DMA3=0x400, output=INT_DMA5=0x1000)`
2. `SetOutputBuffers(0xcc84140, 0xea0, 0xcc85030, 0xea0)` — two 0xea0-byte
   output buffers in RAM.
3. `SetInputBuffers(0xcc86030, 0xea0, 0xcc87030, 0xea0)` — likewise input.
4. `PowerOutputOff` / `PowerInputOff`.
5. End of sound init — kernel proceeds, never calls subfn 0x07
   (ScheduleOutputBuffer), 0x09 (PowerOutputOn), or 0x0d (StartOutput).
   So the sound subsystem is configured but parked.

`GetSoundHardwareInfo` (subfn 0x04) is NOT called during the early-boot
path — our previous suspicion that the kernel needed the 7-word info
struct written is false. We still implement Einstein's behaviour
(write the struct + return 0) so future paths that exercise it
behave the same as Einstein, but it's not load-bearing for this stall.

The subfn 0x1d / 0x1e firings only happen after our wedge probe
injects INT_DMA3 + INT_DMA5; the kernel's IRQ path runs the IH chain
and SendForInterrupt queues a deferred message. **That alone doesn't
unblock the boot**: heartbeat continues to show PC=0x3ae1bc (= post-
SVC#5 mov pc,lr in GenericSWI) with int_present=0x40 (TIMER_3 latched
but unused) and irq_pend=false.

The actual wedge: TStackManager monitor task (id 0x0c113dd8) is
processing the sound task's `LockStack` collision through
`FMLockHeapRange / ResolveFault / CopyPageAfterCollisionSWI`. Two
collision iterations get traced (155559, 155720) — the loop is real
and per-iteration work is ~270 trace lines — but no further unique
functions appear past trace ~156725. SwapInGlobals shows
~10 distinct tasks rotating through the scheduler (so it's not a
classic deadlock), but the user/svc paths only re-enter
already-traced code.

Heartbeat reads of `SP_EL1=0 ELR_EL1=0` (= AArch32 R13_svc/R14_svc
when at EL1 AArch32 SVC) from EL2 are likely unreliable on QEMU
raspi3b — see `docs/QEMU_BUGS.md`. The existing `handle_diag_lr` path
uses a guest-side stub to read banked regs into RAM precisely because
LLVM's AArch64 `MRS sp_svc` / `MRS lr_svc` plumbing on QEMU is
documented-flaky. Snapshot saves at sync-trap time read non-zero
values via the same sysregs, suggesting the readback is only
unreliable from EL2 IRQ-trap context.

Pending-work hypothesis: the StackManager monitor task is **looping
correctly** but each iteration enters only previously-traced code, so
the function tracer's "first-occurrence" view shows no progress. The
real boot may eventually complete the loop. Worth running for 5+ min
or moving the trace from "first occurrence" to "every-Nth call" to
confirm forward progress vs. true wedge.

Open next steps:
1. Use ghidra MCP to read the kernel-mode REx-side `0x1b16b6c b
   0x1f7540` chain's caller frame (`FMLockHeapRange`) and identify
   what loop bound it's iterating to — see whether the boot is
   waiting for many pages to copy or just a few.
2. Verify SP_svc/LR_svc on FVP at the wedge — if they read sane
   values there, the QEMU readback was the misdiagnosis source.
3. Switch the function tracer to "log every Nth call" or wire a
   per-call counter so we can see whether already-traced functions
   are being re-entered (real progress) or genuinely stuck.

## Resolved (was) — boot wedges inside StackManager monitor's page-copy SWI; sound IRQ injection partially unblocks (QEMU, 2026-04-25 evening)

The "kernel waiting for sound DMA IRQ" hypothesis below was tested and is
**partially correct but not the primary blocker**:

1. Added a wedge probe to `trap_irq` that injects `INT_DMA_CH3 |
   INT_DMA_CH5` (0x1400) into `vic::int_present` after the heartbeat
   detects 64+ consecutive samples at the same guest PC and the kernel
   has armed the sound IRQ enables in `int_ctrl` (mask & 0x1400 ==
   0x1400). Implementation: `peripherals/vic.rs::inject_sound_dma_irq`,
   wired into `trap.rs::trap_irq`.

2. **The injection works**: with it enabled the kernel's IRQ path runs
   `IRQHandler → DispatchIRQInterrupt → PSoundDriver::InputIntHandler →
   TSoundServer::SoundInputIH → SendForInterrupt`, then the same chain
   for OutputIntHandler / SoundOutputIH. So the kernel **does** want
   sound DMA IRQs after registering them in `int_ctrl=0xc401420`.

3. **But the SoundIH runs in IRQ context only** — it doesn't unblock
   the StackManager monitor task that's wedged in SVC mode mid-page-
   copy. After the IRQ returns, control resumes at the same idle PC=
   0x3ae1bc and no new user/svc-mode functions are entered.

4. The actual wedge: `TStackManager::FMLockHeapRange` / `BuildPerms` /
   `AddPgPAndPerm` for the sound task's stack pages stops making
   progress around trace 155832 (last `_ExitFIQAtomic`). Sync trap
   counter keeps growing (cache-flush MCRs, shadow-stub UDFs) but
   `awk '/^trace / && !seen[$4]++'` shows no new function entries past
   `PhysSubPageCopy` regardless of how long we run (180+ s).

5. Open question: where in the REx-side `0x1b16b6c
   CopyPagesAfterStackCollided` (or its callees) are we stuck? The
   user-mode wrapper is just `ldr r0, [r0]; b 0x1b16b6c`, so the
   actual loop lives in REx code that the rom.dis tooling doesn't
   cover. Probably needs ghidra MCP to inspect.

6. Heartbeat reads SP_EL1=0 LR_EL1=0 (which should alias R13_svc /
   R14_svc) at the wedged state, but the snapshot save reads non-zero
   values via the same sysregs at sync-trap time (per
   `INVESTIGATION.md` history: r13(SP_svc)=0x0c1142bc r14(LR_svc)=
   0x001f7cc4 sampled from snapshot). Either QEMU's
   AArch32↔AArch64 banked-register plumbing is unreliable for IRQ-
   trap context (see `docs/QEMU_BUGS.md`), or the kernel does
   genuinely have SP_svc=LR_svc=0 in some idle path. Worth verifying
   on FVP before assuming the kernel is at fault.

The sound DMA IRQ injection is left in place as a probe; it doesn't fix
the wedge but does extend coverage by exercising the sound IH path.

Next steps:
1. Use ghidra MCP to inspect the REx-side `0x1b16b6c
   CopyPagesAfterStackCollided` to identify the loop termination
   condition and what state the kernel is checking that doesn't
   advance.
2. Cross-check by running the same boot point on FVP — if SP_svc /
   LR_svc read coherently there, the QEMU readback is the
   misdiagnosis source; if they're also 0, the kernel really is
   parked there with SP_svc=LR_svc=0 (interesting).
3. Independent path: check whether the wedged kernel-mode task is
   waiting on a specific kernel-internal semaphore or condition
   variable that `inject_sound_dma_irq` can't unstick.

## Resolved (was) — kernel idle waiting for non-timer IRQ after stack-collision SWI (QEMU 16×+ratchet+ROM-patch, 2026-04-25)

After both the ratchet fix (hypervisor-side) and the
addls→addcc ROM patch (kernel-side) below, the timer/alarm
subsystem is fully working. In a 180-s run:

- `TTimerEngine::Alarm` fires 45× (was 1× before)
- `RestartTimerOverflowDetect` fires 45× (was 0× before — never)
- `UpdateClock` fires 46× (was 1×)
- `TTimerEngine::QueueTimer` runs 46× (was 1×)

The kernel's gClock now properly tracks tick wraps; alarm.high
matches gClock.high in snapshots; alarms queued at `gClock + delay`
fire at the right moment.

Boot reaches the same stack-collision page-copy SWI as the
shorter runs (TSoundServer::TheMain → LockStack →
CopyPageAfterCollisionSWI → CopyPagesAfterStackCollided →
PhysSubPageCopy → CleanPageInDcache → PurgePageFromTLB →
_ExitFIQAtomic, last traced call ~155350). After that, no new
unique functions appear for the remaining ~25 seconds of the run.

Heartbeats show steady-state:
- PC=0x3ae1bc CPSR=0x60000113 (SVC mode, IRQs enabled, Z=1)
- int_present=0x0 (no timer match latched at sample time)
- int_ctrl=0xc401420 (TIMER_2 + DMA3/DMA5 + power-off enabled,
  TIMER_3 / GPIO disabled)
- VI=0, irq_pend=false

The alarm engine cycles through `RestartTimerOverflowDetect`
once per ~3.7s (delay = 0x0d2f0000 ticks at 59 MHz scaled), but
no other code progresses between alarm IRQs.

Hypothesis: the kernel set DMA channel 3 (Sound input,
0x400) and DMA channel 5 (Sound output / Tablet rcv, 0x1000)
IRQ enables in `int_ctrl` during sound subsystem init, then
called a `WaitOn` that depends on sound DMA completion to
deliver an IRQ. We don't model sound DMA, so that IRQ never
fires, the kernel sits idle through alarm cycles.

Next steps:
1. Confirm by inspecting the saved task struct at the heartbeat
   PC: which task is running, what semaphore it's blocked on.
2. Either implement minimal sound DMA stubs (return
   "transfer complete" immediately) or short-circuit the
   sound subsystem entirely if it's optional for early boot.
3. Cross-check Einstein's TDMAManager / sound-driver path —
   what does it return for these channels?

## Resolved — alarm-loop wedge from spurious wrap detection (QEMU, 2026-04-25)

**Two complementary fixes** ended up needed:

1. Hypervisor-side: `peripherals/vic.rs::ticks()` now ratchets
   via `LAST_TICKS` so consecutive in-hypervisor calls return
   strictly increasing values.

2. ROM patch in `rom_patches.rs`: replace `addls` with `addcc`
   (`ls`→`cc` swap on cond field) at the three wrap-detect
   sites in the kernel — `GetClock` 0x3ad430, and
   `SetAlarm` 0x3ad46c / 0x3ad49c. The kernel reads the live
   tick register via the non-trapping `stage2::TICK_PAGE`
   mapping, which only refreshes on hypervisor heartbeat
   (~16 ms) — so the hypervisor-side ratchet doesn't help when
   the kernel reads the same page twice in quick succession.
   The ROM patch makes wrap-detect strictly less-than instead
   of less-or-equal, so equal successive reads don't fire a
   false wrap.

Without the ROM patch alone, the alarm engine still wedges
because `addls` treats `current_ticks == gClock.low` as a wrap
(see "Verified by reading guest RAM" below). Without the
ratchet, hypervisor-side `ticks()` calls (e.g. for the tick
page itself) can return equal values across two close calls,
which is harmless after the ROM patch but still violates the
"strictly monotonic" contract that other code might rely on.

QEMU boot was getting stuck in a `TTimerEngine::Alarm` →
`SetAlarm` → `SetAlarm1` → `DisableAlarm1` tight loop right after
`UserBoot`, never advancing past trace 27313. Symptom: same alarm
time (low word) being re-armed forever, with current ticks already
past it.

Root cause: the Newton kernel's `GetClock` (0x003ad41c) reads
gClock from RAM, then reads the live tick register, then bumps
the output's `high` word if `current_ticks <= gClock.low` — that
is, equality counts as "wrapped". Designed for an environment where
two consecutive ticks reads are guaranteed to differ.

In QEMU TCG, `CNTPCT_EL0` advances slowly relative to instruction
count, so two `ticks()` calls in quick succession (e.g., the
`UpdateClock` call in `StartTimerOverflowDetect` followed
immediately by `QueueTimer`'s `GetClock`) can return the same
value. That trips the equal-counts-as-wrapped path and bumps the
local TTime.high to 1, even though no wrap occurred. The freshly
queued alarm gets `alarm.high = 1` while the global `gClock.high`
in RAM stays at 0, so `CompCompare(now, alarm)` permanently
returns -1 and the alarm engine wedges.

Verified by reading guest RAM out of a snapshot: gClock at IPA
0x04008_56c (VA 0x0c10156c via stage-1 walk through L1[0xc1] →
L2[1] = 0x0400803e) was `(0, 0x1A52512C)` — exactly the ticks
value at the boot's first-and-only `UpdateClock` call. The alarm
queue head at IPA 0x040085a0 had `(1, 0x2781512C)` = gClock +
0x0d2f0000, but with the +1 in the high word — the smoking gun.

Fix in `peripherals/vic.rs::ticks()`: ratchet via static
`LAST_TICKS` so consecutive calls always return strictly
increasing values. If the raw computation lands at-or-below the
previous reading, return `last + 1` instead. Real wraps still
work because the raw value drops by ~2^32 and the ratchet steps
naturally past 0xFFFFFFFF on subsequent calls.

After fix: boot advances from trace 27313 to trace 156638, past
`UserBoot` / `InitDomainsAndEnvironments` / `BuildDomainsAndHeaps`
/ `MakeSystemStackManager` / `TPageManager::Register` / sound
hardware probe, into the page-copy SWI for stack-collision
handling.

## Resolved — kernel page-mapping loop, PC=0x3ae1bc (FVP/QEMU, 2026-04-24)

The BLTG-reboot from `BuildDomainsAndHeaps` is **resolved**. Root cause
was in `shadow_stub::analyze_insn`: a *conditional* APCS return (e.g.
`LDMDBNE fp, {…, pc}`, `MOVNE pc, lr`, `BXNE lr`, `LDRNE pc, [sp], #4`)
was reported as `BranchKind::Return` regardless of the cond, so the
liveness walker stopped there and never visited the fall-through. Newton
ROM @ `MakeObject` (0x2595c8) is the canonical site:

```
2595c8: ldrb r0, [r0, #4]      ; ← byte access patched by shadow_stub
2595cc: teq  r0, #0
2595d0: movne r0, #200
2595d4: subne r0, r0, #10240   ; conditional return setup
2595d8: ldmdbne fp, {r4..r10, fp, sp, pc}   ; *conditional* return!
2595dc: str  r1, [r4, #8]      ; reads r1 — only reached on fall-through
2595e0: str  r3, [r4]          ; reads r3 — only reached on fall-through
2595e4: …
2595fc: bl   MonitorDispatchSWI
```

Walker at 0x2595cc stopped at the conditional `ldmdbne` thinking it was
an unconditional return → reads of r1 and r3 at 2595dc/2595e0 were
missed → `pick_scratch_regs` saw r1 and r12 as dead → inline stub
clobbered r1 with CPSR. Downstream `str r1, [r4, #8]` then put garbage
into the `ObjectMessage` op-code field; `ObjectAlloc`'s op-dispatch took
the default arm and returned -10006; `Init__9TUMonitorFPFPv…` propagated
that out of `Init__13TStackManagerFv`; `MakeSystemStackManager` ran the
TStackManager destructor and left `*(0x0c104c08+4) = NULL`; later
`BuildDomainsAndHeaps → NewHeapDomain` dereferenced the null pointer to
read the monitor id, dispatched on monitor 0, and the cumulative error
walks took the BLTG-reboot escape hatch.

Fix: new `BranchKind::CondReturn` variant emitted whenever a return
instruction has a non-AL condition. The walker merges
`APCS_RETURN_LIVE` (taken path) with the recursive walk of PC+4
(fall-through). `nzcv_dead_recursive` does the analogous merge.
Regression: `liveness_cond_return_walks_fallthrough` documents the
0x2595c8 motif.

After the fix, the trace counter advances from ~26900 entries (before
the BLTG-reboot) to ~156700+. Boot reaches deep page-mapping code
(`AddPgPAndPerm`, `LoadFromPhysAddress`, `CleanPageInDcache`,
`PurgePageFromTLB`) and then converges on a steady-state heartbeat at
guest PC=0x3ae1bc CPSR=0x60000113 (SVC mode) — that's `mov pc, lr` at
the tail of `GenericSWI` (just after `svc #5`). The kernel is
repeatedly issuing GenericSWI #5; whether this is a legitimate
busy-loop or another stall is the next question.

Next step: dump the trace tail to identify which caller is spinning on
GenericSWI #5 and what the SWI does. Likely candidates: scheduler
idle loop, timer wait, or a paging operation that never finishes.

### Snapshot inspection at the stuck point (2026-04-24)

`/tmp/run.firsts` (awk first-trace-per-function over `/tmp/run`) shows
the boot reaches `TSoundServer::TheMain` (lr=0x000cb2a8 — vtable +0x34
dispatch from the world runner), then `LockStack` triggers
`CopyPageAfterCollisionSWI` → `CopyPagesAfterStackCollided` →
`TStackManager::CopyPageState` → `CopyPhysicalPage` → `PhysSubPageCopy`.
The kernel completes the stack-collision page-copy work (last traced
function: `_ExitFIQAtomic` at trace #148096) and then issues no more
traced calls for the remaining 16+ s of the run.

Snapshot 3 (the last save before the run was killed) has guest GPRs:

```
PC=0x3ae1bc  CPSR=0x60000113  (mode=SVC, I=0 IRQs ENABLED, Z=1, C=1)
r0=0x00000000   r1=0x00000005
r2=0x0c114250   r3=0x0c113f88   r4=0x0c112cb8   r5=0x0c115fa4
r6=0x0c116e44   r7=0x00000008   r8=0x00000001   r9=0x0c1181b0
r10=0x00000000  r11=0x0c114334  r12=0x00000010
r13(SP_svc)=0x0c1142bc
r14(LR_svc)=0x001f7cc4
spsr_svc=0x60000113   spsr_und=0x20000110   spsr_abt=0x110
sctlr_el1=0x11b7      ttbr0_el1=0x4000048
```

`r14=0x1f7cc4` is the post-`bl CopyPageAfterCollisionSWI` PC inside
`TStackManager::FindCollidedPage`. So `mov pc, lr` at 0x3ae1bc would
branch back into the kernel-mode body of that function (which is
exactly what we'd see if the SVC handler exited correctly — the trace
shows ~150 ROM functions called inside this SWI before the heartbeat
takes over).

Critical CPSR bit: **I=0**, IRQs enabled. So a pending vIRQ would fire
immediately on ERET. The fact that snapshots 24..31 (~16 s of wall time)
all capture *exactly* this state — same PC, same regs — means the
guest is making zero forward progress. Either:

1. A pending vIRQ keeps firing each cycle, the guest's IRQ handler
   doesn't clear `vic::int_present`, and we re-trap immediately on
   ERET. (HCR_EL2.VI sampled at heartbeat would tell us.)
2. EL2 is stuck in an IRQ storm of its own — but heartbeat fires at
   the expected ~1/64 cadence, so this isn't an EL2 storm.
3. Something at PC=0x3ae1bc takes a sync trap (DABT? PABT? alignment?)
   on the `mov pc, lr` itself. But this insn is 0xe1a0f00e — no memory
   access, no shift on PC, should never fault.

Heartbeat anomaly: `intid=0` for every IRQ this run. CNTHP is wired as
PPI INTID 26 (`gicv3.rs:146`); the GIC's IAR returning 0 (= SGI 0)
means either the priority mask is rejecting the CNTHP priority, or
ICC_IGRPEN1_EL1 isn't taking effect. We then EOI intid=0, which on
GICv3 deactivates SGI 0 — leaving CNTHP-26 active forever. That
matches "physical IRQ keeps firing, EL2 never deasserts at the GIC
level." Worth a focused look at why IAR1 reads 0 on FVP.

Plan:
- Add a heartbeat-time dump of `HCR_EL2.VI`, `vic::int_present`,
  `vic::int_ctrl`, and `irq_pending()` so we can tell whether case (1)
  is happening.
- Investigate the GICv3 intid=0 puzzle — verify ICC_PMR_EL1 and
  ICC_IGRPEN1_EL1 are sticky after EL3-to-EL2 handoff on FVP, and
  whether IAR is actually returning 0 or whether ack() is reading a
  stale register.

### Update — heartbeat diagnostics added; boot now reaches scheduler activation, then a kernel-mode DABT (2026-04-24)

After adding `VI / int_present / int_ctrl / irq_pending` to the
heartbeat log (`trap.rs::trap_irq`), a fresh cold-boot run reaches
**trace 230925** (vs. the previous 148096), so the previous PC=0x3ae1bc
heartbeat-only state appears to have been a transient/stale snapshot
rather than a true hang — this run flies past the stack-collision SWI
into multitasking. New territory:

- `TCardReinsertAlertDialog::Init`, `TCardPositionAlertDialog::Init`
- `TPartHandler::Init`, `TPartHandler::Register`, `TPartEventHandler`
- `TPkRegisterEvent::TPkRegisterEvent`
- `Sleep(0x8ffc)` from `TPartHandler::Register` (caller `0x18233c`)
- `InitVppManager` (Vpp = high-voltage flash supply driver)

The heartbeat now shows correct intid=26, `int_present=0x60`,
`int_ctrl=0xc400000`, `irq_pend=false`, `VI=0` — no missed-IRQ
storm. (intid=0 in the prior /tmp/run was either a stale binary or a
resume-from-snapshot artefact; still worth noting if it recurs.)

The boot exits via a recursive kernel-mode DABT:

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x6e657774 mode=0x17
trace 230833 0x00393114 DataAbortHandler (abt) ... lr=0x00259d48
... kernel calls FaultMonitorEntry, ConvertIdToObj, RebootIfFaultWasInStack, Throw
putc 230872..230923: "Unhandled exception evt.ex.abt.bus, warm reboot!"
```

Decoded:

- **Faulting PC = LR_abt - 8 = 0x259d40** = `ldr r0, [r0]` in
  `TUPort::Receive` (just before `bl PortReceiveSWI` at 0x259d44).
- **Faulting VA = 0x6e657774 = "newt" ASCII** — `r0` was loaded from
  `[fp, #4]` (caller's saved arg0 = `self`) and dereferenced. So the
  TUPort `self` pointer is "newt" string bytes.
- **mode=0x17 (ABT)** — fault taken from ABT mode → **recursive
  abort**. The kernel was already inside its DABT handler when the
  next access faulted.
- The first DABT (preceded by trace 230832 `SwapInGlobals`) corresponds
  to the scheduler picking task `0x0c118dd8` and ERETing back into its
  saved PC. That saved PC must already have been 0x259d40 with a
  corrupt FP frame, or the scheduler is loading a wrong task struct.
- 5 prior DABTs in this run all had DFSC=0x7 (translation level-2,
  legit page-in faults at `0x0cc7xxxx` heap pages); the last one is
  DFSC=0x5 (translation level-1) on a wild VA — distinctly different.

Pre-failure trace shows `InitVppManager` (Vpp = high-voltage flash
supply driver, called from `0x00054aa4` inside the platform driver
init loop) working through a normal `operator new(12)` allocation
chain (NewPtr → NewDirectBlock → NewBlock → MoveFreeBlock →
SetFreeChain) and then `TUSemaphoreGroup::GetRefCon`. The DABT comes
~1000 trace events later, after the kernel scheduler has ticked
several times.

Next investigation steps:
- Find where the TUPort::Receive task was created and what arg0
  should be. Scan back through `/tmp/run3.log` for `TUPort::Receive`
  entries to see the legitimate caller's `r0` value.
- Inspect the saved task struct at 0x0c118dd8 (offsets 0x00 / 0xa0 /
  0xd8 are what `SwapInGlobals` loads). If we can dump RAM at that
  address from a snapshot, we can verify whether the kernel's view
  of the task is corrupt or whether our hypervisor mishandled a
  save/restore at task switch.
- The "newt" byte pattern (0x6e657774) doesn't appear as a literal in
  the disassembly but does prefix several symbol names like
  `newtConnects`, `SYMnewtaboutview`, `SYMnewtinfobox` — symbol-name
  data lives in the runtime symbol table, suggesting the corrupt
  pointer came from a symbol table lookup that returned a name string
  instead of a code/data address.
- Confirm reproducibility — re-run cold boot without snapshots and
  verify the DABT site / FAR are stable.


