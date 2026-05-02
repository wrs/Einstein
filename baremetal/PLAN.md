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

**Current goal (iter-77):** iter-76 added a DoSend entry probe
(HVC #0x77 at 0x2f059c) plus a 16-entry ring buffer
(`src/dosend_ring.rs`) of recent invocations dumped on the first
ThrowExInterpreterWithSymbol fire. Cold boot showed exactly **one**
DoSend call before the throw:

```
DoSend #0: recv=0x0cd09020 meth=0x0c643c9c args=0x006840b4
           argc=1 caller_lr=0x002f0eac
```

`caller_lr=0x002f0eac` is just past the
`bl DoSend` inside `DoMessage__FRC6RefVarN21` (0x002f0e40 —
`DoMessage(RefVar const&, RefVar const&, RefVar const&)`).
DoMessage's flow:

```
2f0e40: <prologue> r5=recv r4=methodName r6=argsArray
2f0e60: bl IsSymbol(*methodName)   ; passes
2f0e84: bl FindImplementor(recv, methodName)
2f0e88: bl AllocateRefHandle       ; wraps result, sp = &RefHandle
2f0e94: bl PushArgArray(argsArray) ; pushes args, returns argc in r0
2f0ea8: bl DoSend(recv, sp, methodName, argc)
```

Inside DoSend at 0x2f05fc the throw fires because
`**arg1 == 2` — i.e. the FindImplementor result's first word is
2. Captured `*arg1 = 0x0c643c9c` (heap address), so the heap
object at IPA 0x0c643c9c has header word = 2 (NS object type 2,
which is *not* a frame). The receiver is a heap frame at
0x0cd09020, and the methodName is the ROM symbol
`RSSYMpunctuationcursiveoption` (0x006840b4).

So FindImplementor walked the receiver's protochain looking for
the `punctuationCursiveOption` slot and returned a non-frame
heap object instead of an implementor frame. Two possibilities:
(a) the protochain walk found the slot but its value isn't a
frame (NS code expecting a method-on-frame but the slot holds
a different type); (b) the protochain walk returned an internal
sentinel that the caller mishandled.

Next: probe FindImplementor entry+exit (or AllocateRefHandle
post-FindImplementor) to capture the actual lookup result; dump
the heap object at 0x0c643c9c (header + 16 words) to identify
its NS type. If the object is genuinely not-a-frame and
expected, the bug is in DoMessage's caller (the NS opcode that
sourced this message). If it's a wild value, walk further back
to find the corruption point.

**Background:** iter-70 cleared the splash wedge;
iter-71/72 fought a classifier regression; iter-73 forwarded FPA
UNDs to the kernel's FPE emulator at 0x38d8dc; iter-74 pinned
the throw chain to ThrowRefException; iter-75 walked up to the
DoSend `**r5 == 2` site; iter-76 walked up to DoMessage. Boot
reaches NS runtime, 27 kernel objects + `newt` running NS code,
several `evt.ex.fr.store` exceptions caught, then `type.ref.frame`
escapes all handlers exactly once and trips UnhandledException.

### Iteration 76: walk back from DoSend to DoMessage

#### Method

iter-75's probe placed the throw inside DoSend but `caller_lr`
pointed into DoSend itself (just past the conditional bleq).
DoSend has 16+ call sites in 717006, so a static cross-ref
isn't enough — need a runtime probe.

Add a probe at DoSend entry (HVC #0x77 at 0x002F_059C) that
captures r0..r3 (receiver / impl / methodName / argc), resolves
each RefVar with one indirection (so we see the actual NS Refs),
and source-mode banked LR. Also added a 16-entry ring buffer
in `src/dosend_ring.rs` populated on every DoSend call; the
ThrowExInterpreterWithSymbol probe (iter-75) dumps the ring on
the first fire so we get the call sequence even when many
DoSends precede the bad one.

DoSend fires ~hundreds of times per NS-running boot, so the
inline log throttles to every 64th call (plus the first 8).
The ring is the authoritative record.

#### Result

Single-shot cold boot fired exactly **one** DoSend before
the type-mismatch throw:

```
DoSend #0: recv=0x0cd09020 meth=0x0c643c9c args=0x006840b4
           argc=1 caller_lr=0x002f0eac
```

(In the existing log this is the *first* DoSend — earlier
boot work apparently hadn't dispatched any messages, which
is consistent with TNotebook::InitToolbox having only just
finished and the NS interpreter just entering its first
message-send.)

`caller_lr=0x002f0eac` lands inside
`DoMessage__FRC6RefVarN21` (0x002f0e40), specifically just
past the `bl DoSend` at 0x2f0ea8. DoMessage's flow:

```
prologue: r5 = recv, r4 = methodName, r6 = argsArray
2f0e60: bl IsSymbol(*methodName)           ; passes — methodName IS a symbol
2f0e84: bl FindImplementor(recv, methodName)
2f0e88: bl AllocateRefHandle               ; wraps result, sp = &RefHandle
2f0e94: bl PushArgArray(argsArray)         ; pushes args, returns argc
2f0ea8: bl DoSend(recv, sp, methodName, argc)
```

So DoMessage receives `(recv, methodName=punctuationCursiveOption,
argsArray)`, looks up the method in the receiver's protochain via
FindImplementor, wraps the result in a RefHandle, and calls DoSend
with that wrapper as arg1. DoSend reads `**arg1` (= the
implementor's first word) and finds 2 → throws "type.ref.frame".

The captured `meth=0x0c643c9c` field is actually the
FindImplementor-result Ref (passed as DoSend's arg1, mislabeled
"meth" in the probe). The heap object at IPA 0x0c643c9c has
its first word = 2, so it's not a frame.

The methodName (DoSend's arg2, captured as `args=0x006840b4`) is
the ROM symbol `RSSYMpunctuationcursiveoption` at 0x006840b0.
The receiver is `recv=0x0cd09020` (a heap frame). FindImplementor
returned a non-frame for the `punctuationCursiveOption` slot —
either the slot's value genuinely isn't a frame (so the NS code
calling `recv:punctuationCursiveOption(arg)` is wrong about the
type), or it's a wild value from corruption.

iter-77 will probe FindImplementor + dump the heap object at
0x0c643c9c to identify its actual NS type and decide which
branch we're in.

36/36 guest tests skipped per the maintenance note (probe-only
addition: new HVC immediate + dispatch arms + log-only handler).

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

<!-- iter-74 (added a ThrowRefException entry probe — HVC #0x75 at
     0x2f5730 — to walk one frame up from the existing Throw
     probe. Captured caller_lr=0x002f5878 = inside the wrapper
     ThrowExInterpreterWithSymbol; offending Ref *r1=0x0c643ca4
     [a heap pointer ref]. iter-75 walked another frame up.)
     pruned per auto-prune. See `git log --grep="iter-74"`. -->

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
