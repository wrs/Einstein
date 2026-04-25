# Phase B boot-stall investigation

Live notes. Update as we learn more; remove old updates as we move on to
new stalls.

## Currently at — Reboot("BLTG") from BuildDomainsAndHeaps (FVP, 2026-04-24)

The 0x13c814 alignment-fault stall is **resolved**. Root cause was
in `shadow_stub::live_at_recursive`: the `BranchKind::BLink` arm
clobbered APCS_CALLER_SAVED but didn't mark R0..R3 as **read** at
the BL site. So a parameter register that was set up locally but
only consumed by the call (e.g. `mov r1, r3` followed by linear
unrelated code, then `bl callee`) appeared dead. The inline-stub
scratch picker would then claim that register as `scratch_fl`, do
`MRS scratch_fl, cpsr` and never restore — leaving CPSR (=0x000001d3
in SVC mode) in the param register at the BL. The Newton ROM
@ 0x13ca08 (LDRB) / 0x13ca20 (STRB) inside `AddFlashRange` is the
canonical site — both stubs poisoned r1, which became `&Ul` in
`AlignAndMapVMRange` and then `r5`, then `STR r2, [r5]` faulted at
0x13c814.

Fix: in the BLink walker arm, `live |= APCS_PARAMS & !written`
before the post-BL clobber. Unit tests added: `liveness_bl_param_regs_live`
(replaces the now-incorrect `liveness_bl_clobbers_caller_saved`)
and `liveness_bl_param_set_just_before_call` (the 0x13ca08 motif).

Boot now advances through `MemObjManager::PrimGetEnvDomainName`,
deep into kernel init, and reaches `BuildDomainsAndHeaps__FUl`
@ 0xe91f0 — where it hits a `Reboot__FlUlUc` call from PC 0xe9b98
with `r1 = 'BLTG'` (literal at 0xe9c24). The reboot triggers when
`[sp, #48]` is non-zero at 0xe9b7c — i.e. an exception was thrown
during one of the inner calls (`GetDomainInfo`, `GetPersistentRef`,
`__dt__8TUObject`, or `__dl__FPv`) and the unwind landed on the
BLTG-reboot escape hatch.

Next step: enable `--features trace` to identify which inner call
is throwing, then drill into that call. Note that trace mutates the
ROM and invalidates snapshots — clear `/tmp/newton-snapshot-*.bin`
before the trace boot.
