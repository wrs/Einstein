# Phase B boot-stall investigation

Live notes. Update as we learn more; remove old updates as we move on to
new stalls.

## Currently at — kernel page-mapping loop, PC=0x3ae1bc (FVP/QEMU, 2026-04-24)

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


