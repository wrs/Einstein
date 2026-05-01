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

**Current goal (iter-65):** iter-64 used the existing function
tracer (`--features trace_once,quiet` — first-touch only, low
overhead) to locate where newt actually is post-splash. **It is
inside RunInitScripts → DoBlock running NewtonScript, doing text
drawing and polling flash erases.** Concrete findings below; next
steps:

1. **Identify which NewtonScript boot block is running.** The
   `DoBlock(refHandle, *0x00680388)` call inside `RunInitScripts`
   at ROM 0x1f1b18 picks up a NewtonScript frame from the symbol
   table at `0x00680388`. Read that NS object out of the ROM
   to identify the boot block, then trace what it does. The new
   `IsInternalFlashEraseActive` and `CheckEraseCompletion`
   first-touches at the trace tail strongly suggest the block
   is provisioning the PSS store (formatting blocks via erase).
2. **Cross-check against Einstein** — still outstanding from
   iter-61/62/63. The clean oracle would be a small companion
   to `NewtonProbe` that calls into `TEmulator` for the same
   wall-clock window and dumps `gObjectTable` + run-queue head,
   mirroring our `task_dump`. Tells us whether Einstein at the
   same point is still in this NS block or has progressed
   further.
3. **Optional perf (deferred):** ScratchVA fallback for the
   rotate-LDR `no_dead_scratches` rejection (98 % of inline-
   stub misses). Trap rate at splash idle is ~400 K/s, dominated
   by `ELR=0xffffe4`. Fine for development unless it
   bottlenecks diagnostics.

**Background (unchanged from iter-61):** boot reaches a quiescent
idle at the Newton splash. The framebuffer renders correctly
(`/tmp/newton-fb/00000.png`). All 26 expected tasks alive;
`newt`=RUN, `scrn`=RDY blocked on its event-signal sema-group,
all 24 others BLK. The residual `evt.ex.fr.store` throws are
benign soup-probe misses caught by NewtonScript.

### Iteration 64: function tracer locates newt — past splash, inside RunInitScripts/DoBlock

No code changes. Used the existing `--features trace_once,quiet`
build (first-touch only, ~3% overhead vs full `trace`) to discover
where newt has actually been post-splash. The "must be in
TNotebook::InitToolbox somewhere" guess from iter-63 was right
about the function but wrong about the step.

#### Method

`trace_once` patches every code-symbol entry with a HVC trampoline
that fires once per function (per-fn `INITIALISED` bitmap in
`tracer.rs`). Cold boot for 90 s captured first-entries from
trace 1 to ~4.25M; the system reached steady state with the
expected `ELR=0xffffe4 SPSR=0x40000197` rotate-LDR trap signature
at ~33M traps total.

#### Findings

Newt's progression through user-mode boot, by trace number:

```
   16514  TAppWorld::TheMain                  (boot main)
   18066  TLoader::TheMain                    (TLoader at 0x11401c)
   71308  TSoundServer::TheMain               (separate task)
  100494  TCardProcessor::TheMain             (separate task)
 4113325  TPSSManager::TheMain                (separate task; lasts 4M+ ticks)
 4171906  TNotebook::InitToolbox              (newt enters InitToolbox)
 4171907  TApplication::InitToolbox           (parent — step 1)
 4172358  DoBlock                             (RunInitScripts NewtonScript)
 4240890  TNotebook::DrawSplashScreen         (step 6 — splash logo)
 4241030  UpdateHardwareScreen
 4241034  BlitToScreens                       (the actual blit)
   ...
 4244865  InitTextWalker
 4244866  ResetTextWalker                     (NS drawing text)
 4245205  IsInternalFlashEraseActive          (NS provisioning flash)
 4245206  TNewInternalFlash::CheckEraseCompletion
 4245210  TNewInternalFlash::InternalCheckEraseCompletion
```

After trace 4245210 no new functions enter for the rest of the
~33M-trap run. So newt is in a tight loop calling only previously-
seen functions, dominated by `IsInternalFlashEraseActive` polls,
text drawing, and the rotate-LDR alignment-fault path.

What this tells us:
- **Newt got past splash.** The "what wakes scrn" line of inquiry
  was a dead end — scrn is quiet by design (only QD-primitive
  drawing without an enclosing StartDrawing/StopDrawing wakes it).
- **Newt is inside `DoBlock` in `RunInitScripts`** (step 10 of
  TNotebook::InitToolbox), executing a NewtonScript boot block
  loaded from `*0x00680388`. The NS block is actively drawing
  text and waiting on flash erases — almost certainly
  provisioning the PSS store on first boot (filling in the
  Formatted-but-empty data area whose layout iter-58/63 showed
  the kernel scanning).
- **Trace overhead matters.** Full `--features trace,quiet`
  slows boot enough that in 60 s of wall time the system
  hadn't even cleared `InitPSSManager`'s flash log scan. Every
  Newton function call becomes an EL2 round-trip; `trace_once`
  amortises it to one round-trip per function.

#### Verification

- All 36 guest tests pass on QEMU (no code changes; ran the
  suite to confirm baseline still green).
- Two cold-boot runs:
  - `trace_once,quiet` 90 s — log captured the boot waypoints
    above; FB at `/tmp/newton-fb/00000.png` matches iter-61.
  - `trace,quiet` 60 s — too slow to clear flash scan; abandoned.

#### Out of scope (deferred to iter-65)

- Decoding the NewtonScript block at `*0x00680388`. This is
  what iter-65 should do — extract the boot-block frame and
  identify which NS function is running.
- ScratchVA fallback for rotate-LDR `no_dead_scratches`. 4634
  rejections out of 4923 attempts (94 %) is a perf bottleneck
  that would shrink the trap rate by an order of magnitude.

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
