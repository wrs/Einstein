# Phase B boot-stall investigation

Live notes. Update as we learn more; remove old updates as we move on to
new stalls.

## Currently at — sound subfn map known; wedge in StackManager page-copy persists (QEMU, 2026-04-25 late)

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


