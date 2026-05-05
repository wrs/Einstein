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
- All 36 guest tests must pass on every commit that touches hypervisor
  functionality (not merely diagnostics):
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-99):** with iter-98's classifier refinements in
place (data-stop ranges + alt-entry collector + ROM-soup logging),
boot reaches PC=0x7a56e4 — TMain… driver init — but stalls on a UND
trap whose root cause is the kernel reading its own instruction as
data via `LDR`. Two confirmed sites:

- **DataAbortHandler 0x003931e4:** `ldr r0, [lr]` — loads the
  faulting word so the kernel can decode the abort.
- **UndefinedInstruction 0x0038ce9c:** `ldr r1, [lr, #-4]` — loads
  the faulting word to compare against UDF marker patterns.

Both run with `CPSR.E=1` (kernel BE data mode). The walker-marked
words are byteswapped on disk → LE byte order in memory at load
time so the LE instruction-fetch decodes them correctly. A BE
`LDR` of the same address returns the LE-stored bytes interpreted
as BE → the original word with bytes reversed, i.e. the
byteswapped encoding the kernel cannot recognize.

First-attempt fix: patch the instruction-as-data load sites to
swap the loaded value back to BE before the kernel uses it. Either
inline `REV` after each `LDR` (rom_patches.rs word-write) or HVC-
trap-and-emulate the load with byteswap. Inline REV is the smaller
change — the two known PCs are both `LDR Rd, [lr…]` whose Rd is
known statically; emit `REV Rd, Rd` in the slot that follows.

Remaining diagnostic noise: 35 ROM-soup walk entries, all
legitimate ROM-driver TClassInfo trampolines at 0x7a5xxx
(TMainDisplayDriver, TScreenDriver, "four"-named driver) plus a
handful of B-AL run dispatch tables. The user-defined ROM-soup
range (0x3afda8..0x800000) intentionally over-reaches; the
logging is left enabled as a tripwire.

### Iteration 99 (planned): patch fault-handler instruction loads to return BE

Goal: clear the PC=0x7a56e4 stall by making the kernel's
instruction-as-data `LDR`s in the fault handlers return the
encoding the kernel was compiled to recognise, despite our load-
time byteswap of code-marked words.

The two sites currently known:

| PC          | Insn     | Reads from | Why                  |
|-------------|----------|------------|----------------------|
| `0x003931e4`| `ldr r0, [lr]`     | faulting PC | DABT decode |
| `0x0038ce9c`| `ldr r1, [lr, #-4]`| faulting PC | UND marker compare |

Approach (first attempt — inline REV patch):

1. In `src/rom_patches.rs`, add a word-write pair for each site:
   - Replace the `LDR` with itself (no-op write — keeps the patch
     table aligned for the follow-up word).
   - Replace the immediately-following instruction with
     `REV Rd, Rd` (`0xE6BF0F30 | (Rd<<12) | Rd`), where `Rd`
     matches the LDR's destination register (`r0` for DABT,
     `r1` for UND).
   - Save the original next-instruction value in a small inline
     trampoline that the REV's fall-through reaches, OR use a
     2-word patch that emits `REV; B back_to_next_real_insn`.

   Actual encoding plan: pick whichever of the two patterns fits
   without re-shuffling the surrounding code. The handler bodies
   in the disasm have predictable shape (`ldr` then a use of the
   loaded word in the very next instruction or two), so a single-
   word REV insertion may require relocating one trailing insn
   into a hypervisor-managed thunk.

2. Confirm via guest tests that the REV doesn't perturb non-fault
   paths. Both handlers are entry points reached only on actual
   abort/UND traps — the only baseline-affecting change is `r0` /
   `r1` carrying a different (correct) value past the LDR.

3. Re-run cold boot, expect the UND at 0x7a56e4 to either resolve
   correctly (the kernel decodes its own UDF / branch / mov
   without surprise) or surface a different root cause that lets
   us advance further.

If inline REV proves intractable (e.g. the LDR's caller already
uses Rd in the very next cycle), fall back to:
- HVC-trap the LDR via `UDF #imm` injection in rom_patches.rs and
  emulate the load + byteswap from EL2.
- Or repurpose the existing shadow_stub byte-access path —
  add a "code-region word LDR" form that returns the un-byteswapped
  word, gated on the LDR's PA being inside the byteswapped reach
  set.

Open question: are there OTHER kernel sites that read instruction
encodings as data? Candidates beyond fault handlers: any ROM-patch
mechanism, any breakpoint/inspection tooling embedded in the
kernel (debugger-int handler at 0x38cec8 looks suspicious), any
vtable / classinfo decoder that walks B-AL chains. Trace the
remaining suspicious code regions in iter-97/iter-98's queue
once the fault-handler fix unblocks the boot.

### Iteration 98: classifier refinement — data-stop ranges, alt-entry, ROM-soup log

Goal: drive the classifier's false-positive rate down by patching
three observed misclassifications (3861e4–e8 missing as code,
3948e8–39965c spurious as code, 7a0dbc–7a11fc spurious as code)
and add diagnostic logging for any walk that crosses into the
post-code ROM data region.

Major changes:
- `DATA_STOP_RANGES` — half-open `[start, end)` ranges the walker
  refuses to enter and `load_symbol_roots` refuses to seed.
  Mirrors `classify-symbols.py`'s `DATA_RANGES`. Stops the cascade
  where data symbols (e.g. `PublicFiller` at 0x003948e4 with first
  word `0xE6000410`) seed the walker, who then walks linearly
  through bp-weight data and pushes misdecoded `bne` targets into
  NSRuntime / package data at 0x7a0dbc, 0x7ed138, 0x7ed2ec.
- `collect_alt_entry_roots` — new indirect-pass collector for the
  `mov r0, pc; mov pc, lr` micro-trampoline that follows
  `add/sub pc, ip, #N` in Newton's class-info dispatch. The pair
  is a "get class-name string pointer" alt entry; without this
  collector the alt entry at 0x3861e4 (TClassInfoRegistryImpl::
  ClassInfo dispatch helper) was unmarked because no static caller
  exists.
- `ROM_SOUP_RANGE = 0x3afda8..0x800000` walk-entry log: per popped
  walk, the first word inside the range produces a stderr line
  with the full origin trace stack. Diagnostic only — does not
  drop bits.
- `SeedSource::Symbol` now carries the symbol name as
  `&'static str` (leaked at parse time). `Seed(Symbol "PublicFiller_1")`
  vs the prior useless `Seed(Symbol)`.

Result:
- `byte-access-static` 28291 → 27769 (-522 false positives).
- 1879 symbol roots dropped via data-stop-range; 2 alt-entry
  roots added.
- Boot reaches PC=0x7a56e4 (TMain… driver init); 35 remaining
  ROM-soup walk-entries, all legitimate ROM-driver class info.
- Invariant (oracle ⊆ static) still holds.

### Iteration 97: classify-rom symbols.txt + DFS provenance trace + JT chain mark

Goal: rebuild classify-rom around the raw symbol list and add a
trace facility so we can answer "why was 0x… marked code?" for any
mystery bit.

Major changes:
- `load_symbol_roots` reads `_Data_/symbols.txt` (was
  `code-symbols.txt`) and applies its own filters: linker markers,
  `^[gk][A-Z0-9]` data prefixes, NV-cond skip, prologue-shape gate.
  Per-filter drop counts in `summary.txt`.
- `collect_classinfo_roots` extended to seed `MOV PC,LR` Branch
  slots (default-empty methods) plus the `fBTableDelta` and
  `fEntryProcDelta` SROs, picking up the entire B-table and the
  monitor entry-proc target. Branch slot offsets cover the full
  `{0x18, 0x1C, 0x20, 0x24, 0x28, 0x34, 0x38}` set per
  OS600/Protocols.h.
- New `seed_vector_table_roots` parses
  `C$$ctorvec$$Base/$$Limit` (and dtorvec) pairs from symbols.txt
  and seeds every function pointer in the array.
- `clear_literal_pool_targets_from_reach` extended to all conds
  (was AL-only). Catches `LDREQ`/etc. literal-pool entries the
  kernel reads as data.
- Pre-walk `PURE_THUNK_PAGES` mark — sets reach bits on the
  specific PA ranges that are pure JT-thunk backing
  (patch-table 0x2000..0x12FFF, gROMPublicJumpTable
  0x13000..0x15FFF, secondary-jt 0x7EE000..0x7EE048). Critically
  does NOT use the L2-blanket approach from iter-96 — those L2s
  also map normal vtable pages (0x1b000, 0x1d000, 0x21000, …)
  which the walker MUST follow to discover their B-AL targets.
- `MANUAL_DATA_RANGES` for the DiagHook BEQ-into-literal-pool
  dead-code excursion at 0x18668..0x18688 (one-off, not a general
  pattern — only one such instance in the entire reached set).
- `MANUAL_CODE_ROOTS` for FP-trap-dispatched error paths in
  `sqrt` / `_ldfp` at PA 0x382418 (transitively pulls in the sqrt
  error-path entries via `BEQ` chain).
- Provenance trace facility: each worklist entry carries the full
  DFS-path stack of `WalkReason` frames (`Seed(SeedSource)`, `Jump
  from PC`, `Branch from PC`) at the moment it was pushed. Drop a
  target PA in `WALK_TRACE_ADDRS = &[0x...]`, recompile, run; the
  walker dumps the complete origin chain when about to mark `cur`.
  Each `SeedSource` variant carries an originating PC (LDR PC,
  ADR PC, trampoline PC, dispatch PC, B-run entry PA) so the
  trace points at the exact source line.
- `scripts/regen-classify.sh` switched input from
  `code-symbols.txt` to `../_Data_/symbols.txt`. The function
  tracer's `code-symbols.txt` consumer is independent.
- `scripts/dump-data-regions.py` generalised to emit both
  `data-regions.txt` and `code-regions.txt` under
  `classify/<hash>/`. New `suspicion_tag` flags code regions
  with any NV-cond or >25% non-AL — likely walker drifts. Reports
  gitignored.

Attempted but reverted: VA-aware `resolve_target_to_rom` chain
resolution returning final ROM PA (instead of thunk PA). With the
chain change, walker followed FnPtrLiteral seeds into REx data
(0x80f8b4 etc., which the user identified as gROMSoup —
NewtonScript objects, not code) and wedged the boot well before
the iter-95/96 PC 0x7a56e4 mark. Reverted to iter-94/95 behaviour:
return thunk PA, walker walks the B word, Step::Jump uses PA
decoding (which goes wrong-target for aliased pages but breaks
safely on out-of-ROM, and pre-marking covers the bytes for BE-8).

Result:
- byte-access-static 28291 (vs iter-96 27750): +541 bits, mostly
  TClassInfo MOV PC,LR slots and ctor/dtorvec function bodies.
- Reach popcount ~885K (vs ~880K).
- Boot reaches the iter-95/96 wedge at PC 0x007a56e4; 36/36 guest
  tests pass.
- 488 SUSPICIOUS code regions queued for the eyeball pass.

<!-- Older iteration retrospectives (iter-96 and earlier) live in
     `git log` per the auto-prune maintenance note. -->
<!-- iter-90 deferred shadow_stub deletion: still gated off
     (`patch_rom_from_bitmap` no longer called from `main.rs`); full
     removal + SBA dispatch arms + `unxor_sub_word` guest-test path
     is a follow-up commit. -->



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
