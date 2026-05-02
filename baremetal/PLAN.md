# Plan — Drive Newton OS to interactive use

## Status

**Maintenance note (auto-prune):** Each iteration, BEFORE adding a new
iter-N section, prune the old one(s) so PLAN.md stays small. The full
history lives in `git log`. Keep only: this Status block + the most
recent 1-2 iteration sections + the reference sections at the bottom.
Bloated PLAN.md wastes context every read.

**Hard rules** (user directives still in force):

- Hypervisor-side compensation for subpage-AP incompatibility is OFF
  the table (2026-04-29). The fix MUST be a kernel patch.
- Run the *original ROM code*; no workarounds, no deferrals, no
  shortcuts; fix all warnings before each commit.
- All 36 guest tests must pass on every commit
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-76):** iter-75's probe at
`ThrowExInterpreterWithSymbol` entry (HVC #0x76 at 0x2f5810)
pinned the throw to `DoSend__FRC6RefVarN21l` at 0x2f05fc:

```
2f05e8: ldr r0, [r5]   ; r0 = *r5    (r5 = methodName RefVar)
2f05ec: ldr r0, [r0]   ; r0 = first word of object at *r5
2f05f0: teq r0, #2     ; type tag == 2?
2f05f4: moveq r1, r4   ; if so, r1 = args RefVar
2f05f8: ldreq r0, [pc, #168]  ; sym code = 0xffff4157
2f05fc: bleq ThrowExInterpreterWithSymbol  ← throw fires here
```

Probe captured `r0=0xffff4157` (sym), `r1=0x006840b8`,
`*r1=0x006840b4` — a pointer Ref to ROM 0x006840b0
(`RSSYMpunctuationcursiveoption`, a NS symbol). So a `Send`
operation is being attempted with the *symbol* as the offending
value (probably the receiver itself, dereferenced and finding
header-word 2 = some "not-a-frame" tag).

caller_lr = 0x2f0600 — that's *inside* DoSend (just past the
BL). Need next iteration's stack walk to find the *upstream*
caller of DoSend (one of: 0x2f02b8 / 0x2f0850 / 0x2f09b0 / and
the TInterpreter inline send paths). Add a probe at DoSend
entry (0x2f059c) capturing r0..r3 + caller LR; that pins which
NS-runtime wrapper is sending a method to a symbol receiver.

**Background:** iter-70 cleared the splash wedge; iter-71/72
fought a classifier regression; iter-73 forwarded FPA UNDs to
the kernel's FPE emulator at 0x38d8dc; iter-74 pinned the
unhandled-throw chain to ThrowRefException; iter-75 walked one
frame up to the DoSend `**r5 == 2` site. Boot reaches NS runtime:
27 kernel objects, all standard tasks (TSoundServer, TNotebook,
TNameServer, …), `newt` running NS code, several
`evt.ex.fr.store` exceptions caught successfully, then
`type.ref.frame` escapes all handlers.

### Iteration 75: walk back from ThrowRefException to the DoSend type check

#### Method

iter-74's probe captured `caller_lr=0x002f5878` (just past the
`bl ThrowRefException` inside `ThrowExInterpreterWithSymbol`).
But that's a wrapper above ThrowRefException — the *real* NS
runtime caller is one frame further back. Cross-ref disasm shows
12 distinct call sites for ThrowExInterpreterWithSymbol in 717006:

```
2b6170: FGetVar             2ed760: FastResend
2d2d64: FindVar              2edbec: FastFindVar
2d2ddc: SetFindVar           2f2ff8/2f31ec: SlowRun
2ed58c: FastCall             2f5874: ThrowRefException (the wrapper)
2ed614: FastSend             2f645c/2f64fc/2f6538: SetupSend / SetupResend
```

Add a probe at `ThrowExInterpreterWithSymbol` entry (HVC #0x76
at 0x2f5810) to capture `r0` (the symbol code, a `long`),
`r1`/`*r1` (the offending RefVar), and source-mode banked LR
(= return PC into the caller).

#### Result

Single-shot cold boot fired:

```
ThrowExInterpreterWithSymbol #0: sym=-48809 (r0=0xffff4157)
  r1=0x006840b8 *r1=0x006840b4 caller_lr=0x002f0600
```

`caller_lr=0x002f0600` is inside `DoSend__FRC6RefVarN21l`
(0x2f059c..0x2f068c), specifically just past the throw site
at 0x2f05fc:

```
DoSend prologue saves: r7=arg0 (recv), r5=arg1 (methodName),
                       r4=arg2 (args), r6=arg3 (argc).
2f05e8: ldr r0, [r5] ; *r5 = methodName Ref
2f05ec: ldr r0, [r0] ; first word of object pointed to
2f05f0: teq r0, #2   ; type tag == 2 ?
2f05fc: bleq ThrowExInterpreterWithSymbol(sym, r4)
```

Offending Ref `0x006840b4`: low 2 bits = 00 (pointer ref); the
target is ROM `006840b0 <RSSYMpunctuationcursiveoption>` — a
NS symbol literal embedded in the ROM. The interpreter expected
something else (frame? array?) and rejected the symbol's
header-word value of 2.

iter-76 will probe DoSend entry (0x2f059c) to capture
r0..r3 + caller LR and identify the upstream NS-runtime
wrapper (Send / Perform / NSSendProtoWithArgArray / inline
TInterpreter send paths) that's passing a symbol receiver.

36/36 guest tests skipped per the maintenance note (probe-only
addition: new HVC immediate + dispatch arms + log-only handler).

### Iteration 74: pin the type.ref.frame throw site

#### Method

Re-reading iter-73's "new stall" section showed it was wrong:
the apparent abort loop at PC=0x19a84 / 0x19ac0 inside
`DiagBootStub` is just normal demand-paging during a memory-fill
loop (each `stmia r0!,{r2,r3,r4}` iteration faults on a fresh
page, the RAM-perm-fault arm in `handle_data_abort` flips
RO→RW+XN, the STM completes, ELR advances, loop continues).
The trap log budget runs out at 500 lines and the boot keeps
walking silently; the actual stall is much further in.

Looking at the tail of `/tmp/iter73-boot.log`:

```
Throw #0..#4: name="evt.ex.fr.store" (caught somewhere)
ThrowRefException #0: name="evt.ex.fr.intrp;type.ref.frame"
                       *r1=0x0c643ca4  caller_lr=0x002f5878
Throw #5..#7: name="evt.ex.fr.intrp;type.ref.frame"  (rethrown)
*** invariant violation: kernel reached UnhandledException ***
```

Existing `Throw` probe (0x000B_00C8) only sees the throw inside
`ThrowRefException`'s own `bl Throw` site at 0x2f57f8 — it can't
walk back through the constructor's frame to identify which NS
runtime function asked for the throw. Added a new probe at
`ThrowRefException__FPcRC6RefVar` entry (0x2f5730), HVC #0x75,
that captures r0 (name C-string), r1 (RefVar const&), and the
banked LR (= return PC into the caller) at entry — plus
dereferences `*r1` so we can see the offending Ref value.

#### Result

- Probe added (`THROW_REF_EXCEPTION_PROBE_HVC_IMM`,
  `THROW_REF_EXCEPTION_PROBE_PC = 0x002F_5730`); single-shot
  cold boot fires it once before the wedge.
- `ThrowRefException #0: name="evt.ex.fr.intrp;type.ref.frame"
  (r0=0x000afed8) r1=0x0cc77b50 *r1=0x0c643ca4
  caller_lr=0x002f5878 sp=0x0cc77b4c mode=0x10`.
- Caller PC 0x002f5878 = first insn after `bl ThrowRefException`
  inside `ThrowExInterpreterWithSymbol__FlRC6RefVar` at 0x002f5810.
  Disasm confirms: `2f586c..2f5874` is
  `ldr r0,[pc,#24]; ldr r0,[r0]; bl ThrowRefException`.
- Offending ref `*r1 = 0x0c643ca4`: low 2 bits = 00 ⇒ pointer
  ref (`PtrRef`); the object lives in RAM at IPA 0x0c643ca4.
  The interpreter expected a frame; the heap object isn't one.
- 36/36 guest tests skipped per the maintenance note (probe-
  only addition: new HVC immediate + dispatch + log-only handler,
  no SBA/UND/DABT-path changes).

<!-- iter-73 (forward FPA UND to the guest's kernel FPE emulator
     at ROM 0x38d8dc. Added `is_fpa_insn` (cp1/cp2 LDC/STC/CDP/
     MCR/MRC, cond ≠ 0xF) and `forward_und_to_guest_fpe` in
     `src/trap.rs` that ERETs to 0x38d8dc with SPSR_EL2 unchanged
     so we stay in UND mode. Cleared the iter-70 SFM wedge at
     0xd2780 and let boot walk through TFrameSoundChannel codec,
     TSoundServer init, full kernel-task census, into NS runtime.
     Subsequently revealed the iter-74 type.ref.frame throw stall.)
     pruned per auto-prune. See `git log --grep="iter-73"`. -->

<!-- iter-72 (classify-rom — fn-range clamp on unbounded PC-rel
     switch in `enumerate_pc_rel_jump_table`; cleared the
     0xffffe4 / SBA-UDF-marker-as-FAR wedge by stopping
     iter-71's 64-slot enumeration from sweeping past
     DynArrayLeaf's `mov pc, lr` into the SWIBoot pointer
     table at 0x3ad568+. Bitmap deltas + walker clamp diff.)
     pruned per auto-prune. See `git log --grep="iter-72"`. -->

<!-- iter-71 (classify-rom — five idiom recognizers: TClassInfo
     trampoline walker, SVC fall-through, multi-insn case bodies
     in unbounded PC-rel switches, SWIBoot-style indexed
     dispatch, PC-rel function-pointer construction. Bitmap
     deltas + walker stats.) pruned per auto-prune. See
     `git log --grep="iter-71"`. iter-72 superseded its
     regression (the unbounded-switch fix swept past
     `DynArrayLeaf`'s fn end into the SWIBoot pointer table). -->

<!-- iter-70 (classify-rom walker fixes that cleared the iter-69
     literal-pool corruption; bitmap deltas / four walker fixes
     in tools/classify-rom/src/main.rs) pruned per auto-prune.
     See `git log --grep="iter-70"`. iter-71 superseded its
     surface (added more idiom recognizers); iter-72 superseded
     iter-71's regression. -->

<!-- iter-69 (ROOT CAUSE: classify-rom + shadow_stub corrupted the
     literal-pool function pointer at 0x35c49c, slot 0x420
     overwriting `0x01b494f4` with SBA UDF marker `0xe7f842f0`.
     Discovered via a USR-mode periodic-dump capture showing
     `pc=0xe7f842f0 lr=0x35c498` — first-PABT moment of
     `InitTextWalker`. iter-70 fixed the underlying classify-rom
     walker bug.) pruned per auto-prune. See
     `git log --grep="iter-69"`. -->

<!-- iter-68 (DataAbortHandler-internal hypothesis falsified — SBA
     UDFs silent in the wedge) pruned per auto-prune. iter-69
     superseded its self-loop hypothesis with the literal-pool
     corruption finding; iter-70 then fixed the underlying
     classify-rom bug. See `git log --grep="iter-68"`. -->

<!-- iter-67 (PABT-recovery hypothesis falsified — bp at
     PrefetchAbortHandler 0x393b84 fired 0 times, dabt-forward
     repeat counter logged 0 events) pruned per auto-prune. See
     `git log --grep="iter-67"`. Both falsified hypotheses
     (iter-66 LDRB-loop, iter-67 PABT-recovery) are superseded
     by iter-69's actual root cause: shadow_stub corrupted a
     literal-pool function pointer at 0x35c49c. -->

<!-- iter-66 (slot 0x424 LDRB hypothesis falsified — the LDRB at
     0x35d110 is never executed during the wedge despite the UDF
     marker `enc_udf(0x8000|0x424) = 0xe7f842f4` matching the
     wedge's `LR_abt`. Coincidence, not causation.) pruned per
     auto-prune. See `git log --grep="iter-66"`. -->

<!-- iter-65 (per-task call-chain tools + splash wedge
     characterised) pruned per the auto-prune maintenance note —
     iter-66 + iter-67 both refer to its `LR_abt = 0xe7f842f4`
     finding and the `MeasureGlyphWidths → DrTextChunk` chain it
     surfaced via `dump_current_chain` / `ctt`. Both hypotheses
     drawn from iter-65 (the LDRB-loop and the PABT-recovery
     loop) are now superseded by iter-67's "DataAbortHandler
     stuck inside its own body" mechanism. See
     `git log --grep="iter-65"` for the full retrospective. -->

<!-- iter-64 (function tracer locates newt past splash, inside
     RunInitScripts/DoBlock) pruned per the auto-prune
     maintenance note. See `git log --grep="iter-64"`. The iter-64
     conclusion that "newt is in DoBlock running NewtonScript" was
     based on first-touch traces; iter-65's live periodic dump
     supersedes it — newt is wedged in DrawSplashScreen, well
     before the post-splash NS block ever runs. -->


<!-- iter-63 (SemOp OpList decoder + scrn wake mapping +
     InitToolbox decode) pruned per the auto-prune maintenance
     note. See `git log --grep="iter-63"` for the full
     retrospective. -->

<!-- iter-62 (per-task APCS stack tracer) pruned per the auto-prune
     maintenance note. See `git log --grep="iter-62"` for the full
     retrospective. -->


<!-- Older iteration retrospectives (iter-61 and earlier) live in
     `git log` per the auto-prune maintenance note. -->


## Workflow per stop

1. Capture verify-mmu output (`fix_stage1_xn_bits` ratchets per
   alias-onset). Each alias is a `(PA, VA1, VA2)` tuple.
2. Identify the kernel-side write that creates each alias by
   instrumenting the relevant L2-write entry point with an HVC probe.
3. Cross-reference with Einstein (`build/NewtonProbe baremetal/roms/
   newton.rom _Data_/Einstein.rex 30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — `src/peripherals/*.rs`, `src/trap.rs`.
   - **Einstein behavioural quirk** — port the matching logic.
   - **ROM patch** — `src/rom_patches.rs`. Only when no other layer can
     host the fix.
5. Re-run, observe alias count, repeat until zero.

## Tools

### Hosts

- **QEMU raspi3b** (default; `cargo run --release`) — fast, BCM2835
  VIC, AArch32↔AArch64 banking quirks documented in `docs/QEMU_BUGS.md`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — `scripts/fvp <elf>`. Accurate
  reference: GICv3, generic timer + cache model exact. Build with
  `--no-default-features --features platform-fvp-base`.

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`. Patches every
  `scripts/classify-out/code-symbols.txt` entry with HVC trampoline.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor function-
  entry traces.
- **`build/NewtonProbe`** — Einstein-as-oracle.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s from `trap_irq`.
- **Framebuffer PNG dumps** — `/tmp/newton-fb/NNNNN.png` after
  `screen::blit`.

### Debugging

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). Helpers `bg
  <addr>`, `bp <addr>`, `tt N`, `guest-state`.
- **DABT/PABT DIAG HVCs** at ROM offsets `0x10` / `0x0C`.
- **Software-reset canaries** — BootOS / PowerOffAndReboot / Reboot.

### Reference

- `scripts/disasm-out/rom.dis` — symbol-annotated ROM+REx disassembly.
- `docs/DISASM.md` (incl. "Jump-table aliasing — DON'T mistake the
  thunk for the body").
- `docs/NEWTON_INTERNALS.md` — APCS, ClassInfo dispatch, ROM patch
  table 0x01A00000..0x01C20000.
- `docs/QEMU_BUGS.md` — raspi3b AArch64↔AArch32 quirks.
- `docs/STRUCTURES.md` — kernel struct layouts (TScheduler, TTask,
  TStackManager, end-to-end page allocation).
- `docs/peripherals.md` — peripheral implementations.
- `probe/FINDINGS.md` — golden record from a fully-booted Newton.

### Tests

`baremetal/guest-tests/scripts/run-all.sh` runs the 36 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011 and runs the verify-mmu
  alias detector; UND-vector trampoline; DABT/PABT DIAG patches.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / SBA /
  tracer / canary / probe tags); `handle_page_get_probe`,
  `handle_remember_entry_probe_with` (with the new aliasing tracker);
  `handle_data_abort` with kernel-DABT forwarding for lazy stack
  growth.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, TPC, TPU, IMO, FMO, AMO,
  DC); CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3.
- `src/banked.rs` — AArch32 banked-register access from EL2 (Table
  D1-79).
- `src/rom_patches.rs` — Einstein word-write patches; HVC injection
  helpers; canaries; ResolveFault wrapper; `PAGE_GET_PROBE` patch.
- `src/peripherals/*` — Newton driver / native-primitive surface.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-*.bin`.
- `src/tracer.rs` — function-level tracer.
- `src/guest_bp.rs` — `bp <addr>` for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `guest-tests/tests/` — 36 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 36 tests must pass.

## Non-goals

- Real screen emulation beyond the framebuffer dump — no compositor,
  no pen input.
- Package loading — needs a solution for embedded native code.

## Diagnostic scaffolding (active)

- `verify-mmu` in `fix_stage1_xn_bits` — ratchet-logs subpage-AP
  heterogeneity and per-alias-onset `(PA, VA1, VA2)` tuples.
- `handle_page_get_probe` (PAGE_GET_PROBE_HVC_IMM=0x53) on
  `0x00258EFC` — page-allocator return logger + dup detector.
- `handle_remember_entry_probe_with` (REMEMBER_PROBE_HVC_IMM=0x46)
  on `0x00258E0C` — Remember-side per-PA → first-VA aliasing tracker
  (added to the existing L1-lazy-grow probe).
- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.

Pull these once the boot quiesces.
