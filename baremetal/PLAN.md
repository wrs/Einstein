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

**Current goal (iter-90 follow-up):** chase the cold-boot wedge in
BootOS / SafeShortTimerDelay → SaveCPUStateAndStopSystem under BE-8.
The iter-90 atomic flip (see retrospective below) replaces the
"BE-32 word-invariant via load-time word swap + UDF-trap byte-lane
emulator" architecture with "BE-8 (CPSR.E=1) data accesses +
selective code-only byteswap at load". 36/36 guest tests pass;
production cold boot reaches BootOS init code (the SCTLR write at
`0x18690` succeeds, the StrongARM CP15 clock UND at `0x186a8` is
handled, several MMIO writes proceed) but trips
`SaveCPUStateAndStopSystem +0x2bc` shortly after — so a second-tier
debug iteration is needed to pin which kernel-data read or write is
landing with the wrong byte order. The iter-89 alarm-soup
`evt.ex.fr.store (-48022)` throw is the success oracle once the
wedge is cleared; under BE-8 the underlying byte-lane bug class is
gone, so the throw should not reappear.

### Iteration 90: BE-8 atomic flip (PLAN_BE8_MIGRATION.md)

Migrated guest data accesses from "BE-32 word-invariant via load-time
word swap + UDF-trap byte-lane emulator" to "BE-8 (CPSR.E=1) data
accesses + selective code-only byteswap at load". Five commits across
mpt..pt:

1. **Phase 0 — sweep diagnostic probes.** Removed every iter-50..89
   probe HVC scaffolding (immediates 0x46, 0x48–0x4E, 0x53–0x68,
   0x6B–0x7F, 0x81–0x91) — `*_PROBE_HVC_IMM` constants in
   `rom_patches.rs`, dispatch arms in `trap.rs`, handler functions,
   ring buffers, and orphan helper modules (`dosend_ring.rs`,
   `rep_print.rs`). Kept `FPE 0x80` (load-bearing) and DAH /
   UnhandledException tripwires. ~7000 LOC removed; 36/36 guest tests
   pass.

2. **Phase 1 — `guest_endian.rs` accessors with identity behavior.**
   Added `guest_read_u32_va/pa`, `guest_write_u32_va/pa`,
   `guest_read_u8_va/pa`, `guest_read_u16_va/pa`, `guest_write_u8_pa`,
   `guest_write_u16_pa`, `guest_read_bytes_va`. Migrated ~120
   call sites across `trap.rs`, `peripherals/*`, `task_dump.rs`,
   `heap_check.rs`, etc. through the new helpers. Snapshot, ROM
   patches, and shadow_stub host-byte manipulation paths kept
   unchanged.

3. **Phase 2 — atomic flip to BE-8.** `load_newton_rom` consults the
   classifier `reach.bitmap` per-word: code → byteswap on load,
   data → byte-copy verbatim. Helpers `write_rom_code_word` /
   `write_rom_data_word` / `write_rom_word_by_kind` route every
   ROM patch through the right encoding. `eret_to_guest` SPSR sets
   E=1 (`0x000003D3`); `zero_el1_guest_state` programs SCTLR_EL1
   with `EE | E0E`. The CP15 SCTLR shim masks `EE | E0E` to `1` so
   kernel writes can't drop us back into LE mode. `guest_endian`
   helpers byteswap on read/write for data PAs and pass through for
   ROM code PAs (bitmap-aware dispatch). MMIO byte/halfword writes
   splice into the BE-8 lane (lane 0 = bits[31:24]). Snapshot
   format version bumped 3 → 4. Guest-test mode (`nh_guest_test`
   cfg) keeps LE semantics so the existing flat-binary corpus
   continues to exercise the hypervisor's other mechanisms.

4. **Phase 4 — diagnostics simplification.** `heap_check::dump_object`
   updated comment around `guest_read_bytes_va` (the byte-order
   gymnastics is now internal to the helper).

#### Result

- **36/36 guest tests pass** on QEMU (covers byte/halfword access
  emulation, unaligned LDR rotate, ROM aperture SWP, snapshot
  resume, alignment fault, etc.).
- **Cold boot reaches BootOS init code**: BootOS canary fires,
  SCTLR write succeeds (`0x000010b0` → hw `0x030010b2` with
  EE/E0E held by the shim), StrongARM CP15 clock UND at `0x186a8`
  decoded and no-op'd, several MMIO writes (`0x0F18_3800`,
  `0x0F18_3C00`, etc.) proceed, then the kernel hits a fatal init
  check and enters `SaveCPUStateAndStopSystem +0x2bc`. Full
  `evt.ex.fr.store` success-oracle path not yet exercised — wedges
  in early init. **Next iteration:** instrument the boot from the
  successful StrongARM-clock UND through the wedge to identify
  which kernel-data load or store is landing with the wrong byte
  order under BE-8. Likely candidates: a TICK_PAGE-backed access
  whose splice path needs BE-8 lane geometry; a kernel-globals
  write the `g1_capture` / `alrt_capture` perm-fault sampler
  treats as a stale value.

#### Phase 2d / 5 deferred

`shadow_stub.rs` is gated off (`patch_rom_from_bitmap` no longer
called from `main.rs`) but the module still compiles. Full deletion
+ removal of `SBA_RETRY_TAG` / SBA dispatch arms + `unxor_sub_word`
guest-test path is a follow-up commit. Phase 5 validation matrix
runs once the cold-boot wedge above is cleared.

### Iteration 89: re-baseline after iter-87/88 changes — superseded

Iter-89 chased `evt.ex.fr.store (-48022)` throws via byte-level
soup/B-tree probes; the iter-90 BE-8 migration eliminates the
underlying bug class entirely (any byte-access whose PC was missing
from the static byte-access bitmap read/wrote raw LE bytes instead
of going through XOR-2/XOR-3). The iter-89 probe fleet was deleted
in Phase 0 of iter-90; if the symptom recurs after the cold-boot
wedge is cleared, regenerate probes against the new architecture.
Full iter-89 retrospective is in `git log -- PLAN.md`.

<!-- Older iteration retrospectives (iter-77 and earlier) live in
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
