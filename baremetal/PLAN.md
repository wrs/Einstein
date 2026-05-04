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

**Current goal (iter-97 follow-up):** with the iter-97 framework in
place — symbols.txt-driven seeding, DFS provenance trace, JT-thunk
pre-mark, suspicion tagging — the eyeball cycle is the next step.
The 488 SUSPICIOUS code regions in `classify/<hash>/code-regions.txt`
(>25% non-AL or any NV-cond) are the queue: each is either a real
walker drift to fix at the seed/pass level, or a legitimate
mixed-cond function (rare in this ROM). Use `WALK_TRACE_ADDRS` to
get the DFS path back to the originating Seed when investigating.
The VA-aware walker is still the long-term goal but requires
isolating the FnPtrLiteral seed-into-REx-data path that drove the
~3.3M-word reach explosion when previously attempted.

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

### Iteration 96: pre-mark patch-table + gROMPublicJumpTable thunks via L2 walk

Goal was to extend classify-rom so the BE-8 atomic flip's load-time
byteswap covers every B-thunk the kernel branches through, including
gROMPublicJumpTable (PA 0x13000..0x15FFF) and its sibling thunk
pages (PA 0x1B000..0x21FFF) that share gROMPublicJumpTablePageTable's
L2 (PA 0x18000) with a few pages of kernel-managed page-table data.

Approach: pre-walk pass walks the in-ROM L2 page tables, filters
target pages by shape (top byte 0xEA on the first 16 words), pre-
marks the leading B-AL run inside each thunk page.

Result:
- 16919 patch-table thunk words + 12433 gROMPublicJumpTable family
  thunk words pre-marked.
- `byte-access-static` 27750. Boot reaches PC=0x7a56e4; 36/36 guest
  tests pass.

Limitation: pre-marking blocked the walker from following B-AL
entries in pages that the L2 also mapped as normal vtables (0x1b000,
0x1d000, 0x21000), losing real coverage downstream — fixed in
iter-97 by switching to PA-range-targeted pre-marking.

<!-- Older iteration retrospectives (iter-95 and earlier) live in
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
