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

**Current goal (iter-64):** iter-63 closed out the "what wakes
`scrn`?" question and pivoted the investigation to "what is
`newt` doing post-splash?". `scrn` is wired up correctly and
idle by design — the right next step is to find which call in
`TNotebook::InitToolbox` newt is currently inside. Concrete
findings recorded below; next steps:

1. **Probe each `TNotebook::InitToolbox` step.** The function
   at `0x146b28` makes a fixed sequence of ~13 calls (parent
   init → virtual hook → script-globals → inker → orientation
   → DrawSplashScreen → FPlaySoundIrregardless → print/font/
   intl → recognition → RunInitScripts → InitDarkStar → tail).
   Plant one HVC per call site to determine which call doesn't
   return; the high-rotate-LDR-trap signature (~400 K/s at
   `ELR=0xffffe4`) plus newt=RUN suggests we're inside something
   that iterates heavily — most likely `RunInitScripts`'s
   NewtonScript interpreter loop, but could also be
   `FPlaySoundIrregardless` waiting on the sound server.
2. **Cross-check against Einstein** — still outstanding from
   iter-61/62. The clean oracle would be a small companion to
   `NewtonProbe` that calls into `TEmulator` for 60 s and dumps
   `gObjectTable` + run-queue head, mirroring our `task_dump`.
3. **Optional perf (deferred):** ScratchVA fallback for the
   rotate-LDR `no_dead_scratches` rejection (98 % of inline-
   stub misses). Trap rate at splash idle is ~400 K/s, dominated
   by `ELR=0xffffe4`. Fine for development unless it bottlenecks
   diagnostics.

**Background (unchanged from iter-61):** boot reaches a quiescent
idle at the Newton splash. The framebuffer renders correctly
(`/tmp/newton-fb/00000.png`). All 26 expected tasks alive;
`newt`=RUN, `scrn`=RDY blocked on its event-signal sema-group,
all 24 others BLK. The residual `evt.ex.fr.store` throws are
benign soup-probe misses caught by NewtonScript.

### Iteration 63: SemOp OpList decoder; scrn wake-path mapped; InitToolbox decoded

Adds a kernel-ID-aware OpList decoder to `task_dump` so any task
parked at `SemaphoreOpGlue` (saved PC in `0x3ae1fc..0x3ae204`)
auto-dumps the semaphore ops it's blocked on, with live sema
counts and a `<-- BLOCKS` flag on the offending op. Also walks
the disasm of the screen subsystem and `TNotebook::InitToolbox`
to pin down the wake-path and the post-splash startup sequence.

Mechanism:

- New `find_object_by_id` walks `gObjectTable`'s hash chain to
  resolve a kernel object ID to a kernel-side VA. The user-side
  `SemOp` wrapper at `0x25a464` derefs the user-handle to extract
  the kernel ID (not a VA) before tail-branching into
  `SemaphoreOpGlue`'s `svc #0xb`, so the saved `r0`/`r1` in the
  task's SWIBoot save area at `+0x10`/`+0x14` are kernel IDs.
- New `dump_oplist(group_id, oplist_id)` resolves both IDs via
  the gObjectTable walk, then decodes the kernel-side
  `TSemaphoreOpList` (count at +0x14, ops array at +0x10) and
  prints each op as `sema[i] {wait_zero|inc|dec} delta=...`. The
  encoding is `(sema_idx << 16) | (signed-16-bit delta)`, derived
  from the kernel `SemOp` dispatch at ROM `0x1d4f64..0x1d4f74`.
- Live cross-check: read sema[i].count from
  `(group[+0x10] + i*40)+0x14` and flag the op that would block
  *now*. At quiescent idle this is reliable.

Findings (concrete):

- **scrn's blocking op identified.** At splash idle the dump
  prints (verbatim from cold boot):
  ```
  SemOp: group @0xc125cec (id=0x3707) arr=0xc125d10 n=3
         OpList @0xc125ec0 (id=0x3786) ops@0xc125edc count=4
    op[0]: sema[0] wait_zero delta=+0  (op=0x00000000) sema@0xc125d10 count=0
    op[1]: sema[0] inc       delta=+1  (op=0x00000001) sema@0xc125d10 count=0
    op[2]: sema[1] dec       delta=-1  (op=0x0001ffff) sema@0xc125d38 count=0 <-- BLOCKS
    op[3]: sema[2] wait_zero delta=+0  (op=0x00020000) sema@0xc125d60 count=0
  ```
  This OpList is `gScreen[+48]` per `InitScreenTask`. scrn is
  blocked at `dec sema[1]` because no producer has incremented
  sema[1].
- **The only `inc sema[1]` OpList is at `gScreen[+44]`**, used
  by exactly one producer: `QDStopDrawing` at `0x1ccf0c`. That
  routine fires the wake whenever its dirty rect is non-empty.
  `QDStopDrawing`'s 10 callers are all primitive QD operations:
  `DrawLine`, `DrawArc`, `RgnBlt`, `DrTextChunk`, `InkerLine`,
  `GrayShrink`, `StretchBits`. So scrn only fires when one of
  these primitives runs **without an enclosing `StartDrawing`/
  `StopDrawing` pair** — i.e. ad-hoc primitive drawing, not the
  normal app-driven path.
- **`StopDrawing` is NOT gated externally.** It uses
  `OpList[+56]=(2:-1, 2:0, 2:1)` as a non-blocking barrier on
  sema[2]: the first thread to acquire the barrier does
  `UpdateHardwareScreen` synchronously itself; the rest fall
  through. So scrn idle at splash is *expected* — drawing went
  through the sync path, and there's no further QD primitive
  drawing to fire scrn's wake. This kills the "scrn is stuck on
  something we haven't satisfied" hypothesis.
- **`TNotebook::InitToolbox` (0x146b28) decoded.** This is the
  master post-OS-init driver that newt runs to bring the app
  framework up. Sequence:
    1. `TApplication::InitToolbox` (parent init)
    2. virtual `this->vtable[+44]` (platform-specific hook)
    3. `InitScriptGlobals`
    4. `this->InitInker`
    5. `GetPreference` + `SetOrientation`
    6. **`this->DrawSplashScreen`** (renders the lightbulb)
    7. **`FPlaySoundIrregardless(0x00680210)`** (startup sound;
       blocks on sound server — `Schedule`/`Start` SVC paths
       can throw)
    8. `InitPrintDrivers` / `InitFontLoader` /
       `InitInternationalUtils`
    9. **`TRecognitionManager::Init`** at `0x1ab4a60`
    10. **`RunInitScripts`** at `0x1aa0074` — sets up a
        `setjmp`/`AddExceptionHandler`, then calls
        `DoBlock(refHandle, *0x00680388)` to run a NewtonScript
        block. This is the "run all the boot scripts" call.
    11. `InitDarkStar`
    12. tail `DisposeRefHandle`.
  The splash is rendered (step 6 ran to completion), so newt
  has reached at least step 7. The high rotate-LDR trap rate
  with newt=RUN is most consistent with newt being inside step
  10's NewtonScript interpreter, but FPlaySoundIrregardless
  blocking on the sound server is also plausible.

Verification:

- All 36 guest tests pass on QEMU.
- 50 s SIGKILL'd cold boot reaches the same splash-idle state.
  New OpList decode line is printed for `scrn` at every periodic
  task dump.

Out of scope (deferred):

- The actual instrumentation of `TNotebook::InitToolbox` call
  sites — that's iter-64.
- Cross-check against Einstein — still outstanding.

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
