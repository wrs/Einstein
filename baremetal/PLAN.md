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

**Current goal (iter-71):** boot now wedges on an unrecognised
AArch32 coprocessor instruction:

```
*** unrecognised UND: insn=0xed2dc203 at PC=0xd2780 SPSR_und=0x60000110
    (extend handle_und in trap.rs to handle this opcode)
```

`0xed2dc203` decodes as `STC2 p2, c12, [sp, #-12]!` (or similar
coprocessor 2 store) — VFP/floating-point register save state
emitted as part of the kernel's FPU context-switch path that the
ROM 717006 expects to be handled by an FPU emulator. Newton ROM's
`FP_UndefHandlers_*` (now reachable, see iter-70) intercepts FPU
instructions; this opcode either falls through that handler or is
emitted by code outside the FPU emulation entry point.

iter-71 plan: identify the call chain landing at PC=0xd2780,
determine which FPU/coprocessor opcode family the ROM uses for
context save/restore, and either widen the trap.rs UND classifier
to forward the matching opcodes into the ROM's FP handler, or
implement a hypervisor-side stub if the ROM doesn't have one.

**iter-70 result:** classify-rom walker bugs fixed; the iter-69
wedge at `InitTextWalker → 0x35c49c` is gone. Boot progresses far
past the splash — NewBlock #793 allocations, `ExtendVMHeap`
firing, sound driver initialising at `PC=0x8011dc` — before
hitting the new VFP-stub stall.

**Background (unchanged from iter-61):** boot used to reach a
quiescent idle at the Newton splash with `newt`=RUN wedged in
`InitTextWalker`. Now boot pushes well into kernel-driver init
before stalling on the FP coprocessor opcode above.

### Iteration 70: classify-rom walker fixes — wedge cleared

#### Method

Two fix paths from iter-69's plan: (1) fix `tools/classify-rom`
or (2) runtime guard in `shadow_stub::patch_one_site`. Walter's
note "the classifier bitmap needs to be perfect, so let's at
least fix all known bugs" extended the scope to the full set of
walker reachability gaps revealed by the oracle ⊆ static
invariant check.

Four walker fixes in `tools/classify-rom/src/main.rs`:

1. **Manual-BL `in_table` bug (root cause for iter-69).** When the
   walker saw `mov lr, pc; ldr pc, [...]` (Newton's manual-BL
   idiom), `step()` correctly returned `Continue` but the
   `in_table` update flagged the LDR-PC as a jump-table dispatch.
   The next instruction — typically the function's own `ldmdb fp,
   {…pc}` epilogue — was then misinterpreted as a "default-case
   return" inside an imaginary table, so the walker walked past
   the epilogue and into the literal pool. Words there that
   happened to decode as byte-access shapes (e.g. `0x01b494f4`
   ≅ LDRSH) got marked, then patched with UDF stubs, then read
   *as data* by the LDR-pc-rel that originally loaded the
   constant. Fix: gate `in_table = true` on `!prev_sets_lr`.

2. **PC-relative jump-table dispatch.** `add pc, pc, Rn[, shift]`
   patterns (FPU undef-handler dispatchers and similar) had
   `step()` returning `Stop` because no fall-through target was
   computable; the walker never enumerated the B-AL run that
   followed. Fix: `is_pc_rel_pc_dispatch` + explicit
   `enumerate_pc_rel_jump_table` that pushes each `B target` to
   the worklist starting at `pc + 8`.

3. **Function-pointer literal harvest.** Functions reached only
   through indirect calls where a function pointer is loaded by
   `LDR Rt, [pc, #±imm]` and *passed by reference* (not stored
   into `[r0, #0]`, so `collect_vtable_roots` misses) — e.g. the
   constructor-pointer arg to `__vc__FPvT1iPFPv_v` — were
   unreachable. Fix: `collect_fnptr_literal_roots` scans every
   reached PC-rel LDR, reads the literal, and seeds it as a
   worklist root if the target word is prologue-shaped.

4. **Consecutive-B-AL dispatch table harvest.** REX `FDRV` /
   `pkgl` class-info structures embed N≥3 adjacent unconditional
   `B`s as method dispatch stubs (e.g. PA 0x800460..0x800530,
   17 entries). The walker can't reach them through any
   recognised symbol; `rex_header_roots`'s function-pointer
   filter rejects them because their value (`0xEAxxxxxx`) lies
   outside ROM. Fix: `collect_b_run_roots` scans the full ROM+REX
   for runs of ≥3 consecutive B-AL words and seeds each entry's
   branch target. Threshold of 3 keeps accidental top-byte-0xEA
   data words from generating false positives.

#### Result

- `byte-access-static.bitmap` popcount: 27799 → 27818 (+19 net;
  +35 added by reaching new code, –35 removed by the literal-pool
  fix — coincidental wash; popcount is not a useful signal for
  this fix).
- 0x35c49c bit cleared — iter-69's corruption site is no longer
  in the bitmap.
- All 12 oracle ⊆ static invariant violations resolved (oracle
  popcount = static popcount intersection = 2155, missing = 0).
- 36/36 guest tests pass.
- Cold boot: wedge at 0x35c49c is **gone**. Boot advances through
  many `NewBlock` allocations, `ExtendVMHeap` firings, and the
  sound driver's first subfn dispatch (PC=0x8011dc — REX code
  reachable via fix 4). New stall is `unrecognised UND
  insn=0xed2dc203 at PC=0xd2780` — a VFP coprocessor opcode the
  hypervisor's UND classifier doesn't know about.

#### Walker stats (post-fix)

```
words walked (with revisits):  851531  (was 829504 pre-fix)
fnptr literal roots added:        389
B-run dispatch roots added:       470
total indirect roots added:       859
```

### Iteration 69: ROOT CAUSE — classify-rom + shadow_stub corrupt a function-pointer literal

#### Method

iter-69 added a one-shot probe in `trap::return_to_guest_from_und`
to log the first time the UND_RETURN_STUB literal at 0xFFFFEC
was set to a value inside the trampoline region (testing the
iter-68 self-loop hypothesis). **The probe never fired** —
falsifying iter-68 too.

The breakthrough came from a different direction: cross-checking
periodic dumps that caught newt outside the wedge state. One
single dump captured newt in **USR mode** (mode=0x10) instead of
the usual ABT-mode wedge:

```
current task 0xc12391c (newt) id=0x3113 mode=0x10
  [pc=0xe7f842f0 lr=0x35c498 sp=0xcc7787c fp=0xcc77894]
```

`PC = 0xe7f842f0` and `LR_usr = 0x35c498`. This is the moment of
the *first* PABT, before the kernel handler is even reached.

#### Identifying the call site

`LR = 0x35c498` is the return address from a function call ending
just before that. Disasm of `InitTextWalker` (0x35c41c..0x35c49c):

```
  35c460:  e59f0034   ldr r0, [pc, #52]   @ literal at 0x35c49c
  35c464:  e1a01005   mov r1, r5
  35c468:  e5840008   str r0, [r4, #8]    @ TextWalker.scanner = r0
  …
  35c490:  e1a0e00f   mov lr, pc          @ LR = 0x35c498
  35c494:  e594f008   ldr pc, [r4, #8]    @ PC = TextWalker.scanner
  35c498:  e91ba830   ldmdb fp, {…, pc}   @ epilogue (return target)
  35c49c:  01b494f4                       @ literal: function pointer
```

So `*0x35c49c` is a function-pointer literal originally
`0x01b494f4` (a JT-thunk address in 0x01A00000..0x01C20000). The
disassembler misleadingly labels it "<UNDEFINED> instruction" —
it's a literal, not code.

#### Identifying the corruption

`enc_udf(0x8000 | 0x420) = 0xe7f842f0` — the SBA UDF marker for
slot 0x420. Cross-check via the slot table dump from iter-66:

```
SBA_ORIG_PC[0x420..0x428]: 0x35c49c, 0x35d078, 0x35d0ac, 0x35d0bc,
                            0x35d110, 0x35d144, 0x35d148, 0x35d1ac
```

**Slot 0x420 = orig_pc 0x35c49c.** That's the literal-pool entry.
shadow_stub::patch_rom_from_bitmap walked
BYTE_ACCESS_STATIC_BITMAP, the bit for word offset 0x35c49c was
set, so emit_udf_site overwrote `*0x35c49c` (originally the
function pointer 0x01b494f4) with the UDF marker `0xe7f842f0`.

#### Result

When `InitTextWalker` runs, the `ldr r0, [pc, #52]` reads the
patched literal — getting `0xe7f842f0` instead of `0x01b494f4`.
That value is stored as the TextWalker's scanner function
pointer and called via `ldr pc, [r4, #8]`. Branching to high VA
0xe7f842f0 traps to AArch32 PABT; the kernel's recovery path
loops on the unmapped fetch.

The persistence of the wedge across multiple periodic dumps is
because the kernel handler retries the same bad branch on every
recovery, and the `mode=ABT` snapshot captures the handler
mid-emulation rather than USR mode mid-call.

#### Why all prior hypotheses missed it

- **iter-66**: Looked at slot 0x424 (LDRB at 0x35d110). Wrong slot
  by an off-by-4 — the actual culprit is slot 0x420 in the same
  cluster. The 0x424-vs-0x420 confusion came from misreading
  `LR_abt = 0xe7f842f4` as the SBA marker (it IS slot 0x424's
  marker, but that's coincidental; LR_abt reflects later state
  inside the abort handler, not the original bad PC).
- **iter-67/68**: Looked inside the kernel abort handler. The
  handler IS running, but it's reacting to the bad branch, not
  generating it.
- **iter-69 (literal-poison probe)**: Never fired — the wedge is
  upstream of the UND_RETURN_STUB. Negative result was useful.

#### iter-70 plan: fix the bitmap classification

Two paths, in order of preference:

1. **Fix `tools/classify-rom`.** Newton's compiler emits literal
   pools right after function bodies (after `ldmdb fp, {…pc}`
   epilogues). Words between the epilogue and the next function
   symbol are constants, not instructions. Suppress sub-word-
   access marks for any address that is BOTH in such a tail
   range AND the target of an in-function `ldr Rd, [pc, #imm]`.
2. **Runtime guard in `shadow_stub::patch_one_site`.** Before
   patching, scan a small window of preceding instructions for
   any `ldr Rd, [pc, #imm]` whose computed literal address
   equals the patch site. If found, skip the patch — it's a
   data word, not code. Cheaper and isolates the fix to the
   hypervisor side.

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
