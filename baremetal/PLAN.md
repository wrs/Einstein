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

**Current goal (iter-70):** **iter-69 found the wedge root cause.**
Walter's original "shadow_stub is broken" hunch was correct.
classify-rom's `BYTE_ACCESS_STATIC_BITMAP` falsely classifies a
function-pointer literal-pool entry inside InitTextWalker as a
sub-word access instruction; shadow_stub patches it; the corrupted
value gets read as data, used as a function pointer, and the
indirect call branches to high-VA garbage.

Concrete chain:

1. `InitTextWalker` (ROM 0x35c41c..0x35c49c) ends with a literal
   pool. The last word at **0x35c49c** is the function-pointer
   constant **0x01b494f4** (a JT thunk address; the disasm shows
   "<UNDEFINED> instruction: 0x01b494f4" — that's a literal, not
   code).
2. classify-rom marks 0x35c49c as a sub-word access instruction
   (false positive). NEXT_SITE allocation gave it slot **0x420**.
3. shadow_stub patches 0x35c49c to `enc_udf(0x8000 | 0x420)` =
   **0xe7f842f0**. SBA_ORIG_INSN[0x420] = 0x01b494f4 (the original
   pointer value, decoded as if it were a byte access, which it
   isn't); SBA_ORIG_PC[0x420] = 0x35c49c.
4. At runtime, `InitTextWalker` does
   `ldr r0, [pc, #52] @ 0x35c49c` (instruction at 0x35c460) — it
   reads the literal *as a 32-bit word*, getting 0xe7f842f0
   instead of 0x01b494f4.
5. `str r0, [r4, #8]` saves the corrupt value as the TextWalker's
   "scanner" function pointer.
6. Later: `ldr pc, [r4, #8]` (at 0x35c494, paired with `mov lr,
   pc` at 0x35c490) — PC := 0xe7f842f0.
7. Guest fetches at high VA 0xe7f842f0 → AArch32 PABT (no stage-1
   mapping). Kernel handler runs. The retry path keeps re-fetching
   the same bad PC → permanent loop.

The key periodic dump that revealed it (one out of dozens that
caught newt in USR mode rather than ABT):

```
current task 0xc12391c (newt) id=0x3113 mode=0x10
  [pc=0xe7f842f0 lr=0x35c498 sp=0xcc7787c fp=0xcc77894]
```

`LR_usr = 0x35c498` is the return address from the indirect call
pair `mov lr, pc; ldr pc, [r4, #8]` at 0x35c490–0x35c494. PC is the
loaded value.

**Why iter-66/67/68 hypotheses all missed this:**

- iter-66 looked at slot 0x424 (LDRB at 0x35d110). Coincidence:
  slot 0x420 — *adjacent* slot, different PC — was the actual
  culprit, and 0x424 just happens to be in the same neighbour
  cluster of slots from the same DrTextChunk family.
- iter-67/68 looked for the wedge inside the abort handler. The
  abort handler IS running, but it's reacting to USR's bad
  branch, not generating the wedge itself.
- iter-69's "literal poisoning of UND_RETURN_STUB" probe didn't
  fire because the literal was being set to legitimate USR PCs
  outside the trampoline region — the wedge is *upstream* of the
  return-stub literal write.

**iter-70 plan: fix classify-rom (or add a shadow_stub guard).**

Two fix paths, in order of preference:

1. **Detect literal pools in classify-rom.** Newton's compiler
   emits literal pools right after function bodies (after `ldmdb
   fp, {…pc}` epilogues). Words between the epilogue and the
   next function symbol are constants, not instructions. Update
   `tools/classify-rom` to suppress sub-word-access marks for
   any address that's:
   - within `[function_end, next_function_start)`, AND
   - the target of a `ldr Rd, [pc, #imm]` from inside the
     enclosing function.
2. **Runtime guard in shadow_stub.** Before patching, scan the
   surrounding 32 instructions for an `ldr Rd, [pc, #imm]`
   whose computed literal address equals the patch site. If
   found, skip the patch. Cheaper and isolates the fix to the
   hypervisor side.

Once the fix lands, the boot should advance past
`InitTextWalker → DrTextChunk` and reach the actual
post-splash NewtonScript boot block iter-64 was trying to identify.

**Background (unchanged from iter-61):** boot reaches a quiescent
idle at the Newton splash. The framebuffer renders correctly
(`/tmp/newton-fb/00000.png`). All 26 expected tasks alive;
`newt`=RUN, `scrn`=RDY blocked on its event-signal sema-group,
all 24 others BLK. The residual `evt.ex.fr.store` throws are
benign soup-probe misses caught by NewtonScript.

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

### Iteration 68: DataAbortHandler-internal hypothesis falsified; SBA UDFs are silent in the wedge

#### Method

Instrumented `shadow_stub::emulate_sba_site` with a histogram
(reverted before commit):

- Per-mode counter (`MODE_COUNT[0..32]`) over the SPSR_und
  source-mode bits.
- Per-mode top-N (8 slots) faulting_pc histogram for USR / SVC
  / ABT — first-fit on empty slots, evict-smallest otherwise.
- Dump every 2^17 (≈131 K) hits — roughly one per periodic
  heartbeat at the wedge's trap rate.

Cold boot, no debugger, ran past the wedge.

#### Result

**Exactly one histogram dump fired** (at total = 131 072 hits).
After that, total never reached the next 131 K threshold despite
22 M+ trap beacons accumulating at `ELR=0xffffe4`.

```
=== iter-68 SBA mode/pc histogram (total=131072) ===
  by-mode usr=0 svc=131073 fiq=0 irq=0 abt=0 und=0 sys=0
  top 4 pcs (mode SVC):
    pc=0x001a7ca8 count=65489
    pc=0x001a7cac count=65488
    pc=0x000bd6a0 count=48
    pc=0x000bd6a4 count=48
=== iter-68 end ===
```

All 131 K SBA UDFs were SVC-mode boot-time activity; ABT-mode
count is **zero**.

#### Implication

iter-67's "DataAbortHandler stuck running emulated byte
accesses" hypothesis is **falsified**. SBA UDFs are silent in
the wedge state. The kernel's DataAbortHandler is *not* doing
emulated byte access work — there's no SBA traffic to drive.

Combined with iter-67's findings (no DIAG-path aborts, no
PrefetchAbortHandler hits), the wedge is taking some path that
*doesn't* generate any EL2 traps until the next timer IRQ.

The most plausible mechanism is a tight 2-instruction loop at
the `UND_RETURN_STUB` itself (`ldr lr, [pc, #0]; movs pc, lr`),
where the literal at `0xFFFFEC` last got written to a value
that re-enters the stub on every iteration. Native-speed loop,
no traps, just timer IRQs catching the guest there.

iter-69 starts with a one-shot probe in
`trap::return_to_guest_from_und` that logs the first time the
literal is set to a self-referential value
(`target ∈ 0xFFFFE0..0xFFFFF0`).

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
