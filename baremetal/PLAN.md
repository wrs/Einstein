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

**Current goal (iter-73):** boot now stops with `unrecognised
UND insn=0xed2dc203 at PC=0xd2780 SPSR_und=0x60000110` — a VFP
coprocessor opcode the UND classifier doesn't decode. Same stall
iter-70 reached before iter-71's classifier regression buried it
under the abort loop. Next: extend `handle_und` (or the
unaligned-fault classifier) to recognise the VFP encoding family
and either emulate, NOP, or HVC-forward as appropriate.

**Background (unchanged from iter-61):** boot used to reach a
quiescent idle at the Newton splash with `newt`=RUN wedged in
`InitTextWalker`. After iter-70 the splash wedge was cleared;
iter-71's classifier added more code-discovery idioms but
inadvertently regressed boot to an InitKernelDomainAndEnvironment-
era abort loop (iter-72 root-caused and fixed). Boot now reaches
the iter-70 stall point.

### Iteration 72: classify-rom — fn-range clamp on unbounded PC-rel switch

#### Method

Cold-boot wedged at `ELR=0xffffe4 SPSR=0x80000197` (ABT mode),
~18 M HVC #DIAG_TAG firings/run. `FAR_EL1` upper half (=IFAR)
held `0xe7f848f4` — an SBA UDF-marker encoding (slot 0x484), not
an address. Lower half (DFAR) `0x0c10fc2e`. `LR_abt =
0xe7f848f8` = UDF marker for slot 0x488. Suggested an iter-69-
class regression: a literal-pool / function-pointer slot was
being patched as code.

Diagnostic: dumped `SBA_ORIG_PC[0x484..=0x488]`. Slot 0x484/0x485
were real STRBs at 0x3ad370/0x3ad374 (legitimate). Slots
0x486/0x487/0x488 were at 0x3ad580/0x3ad584/0x3ad58c, with
`orig_insn` values `0x003adbb4 / 0x003adedc / 0x003adcb0` — i.e.
*ROM addresses*, not instructions. Cross-reference against
`rom.dis` showed 0x3ad568..0x3ad5f4 is the SWIBoot handler-pointer
table (35 entries, plus a few preceding-tail words). The
classifier was walking those data words as code.

Trace: in `tools/classify-rom/src/main.rs`,
`enumerate_pc_rel_jump_table` (iter-71) seeds 64 worklist roots
when no CMP bound is present. The dispatch in `DynArrayLeaf` at
0x3ad4e4 (`add pc, pc, r1, lsl #2`) is unbounded; only ~14 case-
body slots are real, so seeding 64 starting at 0x3ad4ec swept past
the function's `mov pc, lr` at 0x3ad520 and into the data table at
0x3ad568+. Three table entries decoded as byte-access shapes
(LDRH / LDRSB), so `shadow_stub` patched them with UDF markers,
corrupting the SWIBoot dispatch.

Fix in `enumerate_pc_rel_jump_table`: thread `fn_ranges` through
and clamp seeded slots to the containing function's end via
`find_fn_range`. The cond-code emulator at 0x3add80 (the case
iter-71 was added for) is inside SWIBoot (0x3ad698..0x3ae158);
its 64-slot table at 0x3add88..0x3ade88 stays inside the fn
range and continues to be enumerated correctly.

#### Result

- `byte-access-static.bitmap` popcount: 27913 → 27906 (-7,
  removing the false-positive bits at 0x3ad580/0x3ad584/
  0x3ad58c/0x3ad590/0x3ad5b4/0x3ad5bc/0x3ad5e8 inside the
  SWIBoot pointer table).
- `reach.bitmap` popcount: 2654975 → 2654939 (-36 words: the
  data-table region 0x3ad568..0x3ad5f4 plus a few adjacent slots
  inside DynArrayLeaf that were over-seeded).
- Oracle ⊆ static invariant: 0 missing.
- 36/36 guest tests pass.
- Cold boot: wedge at PC=0xffffe4 is **gone**. Boot reaches the
  iter-70 stall point (`unrecognised UND insn=0xed2dc203 at
  PC=0xd2780` — VFP coprocessor opcode).

### Iteration 71: classify-rom — five idiom recognizers

#### Method

Iteration over the unreached-words dump of `classify/<hash>/
reach.bitmap`. Each chunk traced back to a specific compiler-
emitted idiom the walker didn't follow. Five fixes in
`tools/classify-rom/src/main.rs`, each pinned to the idiom — no
content-shape heuristics. (A "scan for runs of code-pointer-shaped
values" attempt earlier in iter-71 contaminated the bitmap with
+22311 false-positive byte-access bits and was reverted in favour
of the SWIBoot recognizer below.)

1. **TClassInfo trampoline + struct walker.** Newton's class
   metadata terminates in a 4-instruction tail-stub:

   ```
   sub  r0, pc, #68      ; return struct base
   mov  pc, lr
   mov  r0, #imm         ; alt entry: bail-out returning <imm>
   mov  pc, lr
   ```

   The 60 bytes preceding the trampoline are the TClassInfo
   struct (15 longs: `fReserved1..fReserved2`). The struct's
   "Branch" fields are inline `B method` slots;
   `collect_b_run_roots` already catches dense ≥3-in-a-row runs,
   but the lone `fSelectorBranch` slot at +0x38 (a B into the
   trampoline's alt entry at `fn + 8`) falls below that threshold
   and was unreached for all 116 TClassInfo trampolines.
   `collect_classinfo_roots` recognises the trampoline pattern
   and seeds every B-AL slot in the inline struct.

2. **SVC fall-through.** `step()` treated `SVC #imm` (cond=AL)
   as `Step::Stop`, stranding everything past it. SWIs return to
   PC+4; Newton's SWI-wrapper functions (e.g.
   `SMemMsgCheckForDoneSWI` at 0x3ae458) do bookkeeping after
   the SWI before their own `mov pc, lr` epilogue. Now Continue.

3. **Multi-instruction case bodies in unbounded PC-rel
   switches.** `enumerate_pc_rel_jump_table` previously broke at
   the first non-terminal slot when no preceding `cmp Rn, #imm`
   bounded the table. Newton's cond-code emulator at 0x3add80
   dispatches `add pc, pc, r1, lsr #24` into a 16 × 16-byte case
   body table without a CMP — every case body is `nop; tst r0,
   #flag; bcc; b common`. Cap unbounded enumeration at
   MAX_UNBOUNDED = 64 slots and seed each as a worklist root;
   multi-insn bodies walk to their natural epilogues.

4. **SWIBoot-style indexed dispatch.** Newton's kernel SWI
   handler dispatch at SWIBoot+0xb4 (0x3ad74c):

   ```
   cmp r1, #35
   bge out_of_range
   ldr r0, [pc, #-488]      ; r0 = table_base
   ldr pc, [r0, r1, lsl #2]
   ```

   The 35-handler table at 0x3ad56c..0x3ad5f4 is referenced only
   through this idiom — no B-AL run, no vtable install, no
   LDR-pc-rel literal, no static 32-bit pointer to any handler.
   `collect_indexed_dispatch_roots` scans reached code for
   `ldr pc, [Rn, Rm, lsl #2]`, walks back ≤16 insns within the
   containing function for the LDR-Rn pc-rel that loaded the
   base and the CMP-Rm that bounded the index, and maps the
   conditional-branch type to an entry count (BGE/BHS/BCS → N;
   BGT/BHI → N+1).

5. **PC-rel function-pointer construction.**
   `collect_pc_relative_addr_roots` previously gated solely on
   `is_used_as_dispatch_base` — only seeded if Rd was later the
   base of a runtime PC-write dispatch. That gate was added to
   keep ASCII-string `add r1, pc, #imm` setups from contaminating
   adjacent text. New parallel gate: also seed when the target
   word itself is a function prologue. Catches cases like FPE
   init at 0x39264c — `sub r1, pc, #0x2c` to point r1 at a
   2-insn stub (`mvn r0, #0; movs pc, lr`) handed off as a
   callback. ASCII top nibbles are 0x2-0x7, never 0xE, so
   `is_known_function_start` rules string-pointer setups out
   structurally — no heuristic threshold required.

#### Result

- `byte-access-static.bitmap` popcount: 27897 → 27913 (+16: +9
  byte-class bits past SVCs, +7 halfword/signed bits in
  newly-walked switch bodies).
- `reach.bitmap` popcount: 2,653,975 → 2,654,975 (+~1000).
- Oracle ⊆ static invariant: 0 missing.
- 116/116 TClassInfo trampolines fully reached (fn_start, alt
  entry, every B-AL struct slot).
- 35/35 SWIBoot handler targets reached; SMemMsg…SWI-style
  wrappers walk to their epilogues; cond-emulator switch at
  0x3add80 reaches all 16 case bodies.
- Remaining unreached chunks (e.g. 0x39276c FPE save/restore
  handlers) have *no* static reference anywhere in reached code
  — they are runtime-installed via memory writes, which static
  analysis fundamentally can't follow. Correct behaviour, not
  a classifier gap.
- 36/36 guest tests pass.

#### Walker stats (post-fix)

```
fnptr literal roots added:      395
B-run dispatch roots added:    9379
PC-rel addr roots added:        222
TClassInfo struct roots:        460
indexed-dispatch roots:          34
total indirect roots added:   10487
reachable-code popcount:    2654975
```

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
