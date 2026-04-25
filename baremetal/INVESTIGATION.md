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
