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

**Current goal (iter-68):** iter-67 narrowed the wedge mechanism
further with two probes (both reverted before commit): a `bp` at
PrefetchAbortHandler entry (0x393b84) and an unsuppressed-repeat
counter in `log_dabt_forward`. Findings below; **the wedge is not
PABT-recovery looping — it's the kernel's `DataAbortHandler`
(invoked once by a recursive DABT at FAR=0x0cd2d000, mode=0x17)
running emulated byte accesses indefinitely without making
forward progress**.

iter-66's correction is itself partially wrong: there's no
permanent PABT loop. The actual mechanism is:

1. USR took a DABT, kernel `DataAbortHandler` ran in ABT mode.
2. While in ABT mode, a *recursive* DABT fired at
   FAR=0x0cd2d000 (DFSC=0x05 = translation fault, section).
3. EL2's `handle_diag` forwarded that to `DataAbortHandler` at
   0x393114 (the standard DABT-fast-path forward).
4. Since then: zero further DIAG-path aborts (the iter-67
   REPEAT counter logged 0 events across 4.6M+ beacons). All
   the `ELR=0xffffe4 SPSR=0x40000197` (mode=ABT) traps are SBA
   UDF emulations, not abort recursions.

So the kernel's `DataAbortHandler` is in a tight loop *inside
its own body* — running through emulated byte accesses without
returning. Most plausible: a string scan / list walk / pointer
chase against corrupt state from the original DABT.

**Probe results that ruled things out:**

- `bp 0x00393b84` (PrefetchAbortHandler entry) installed via
  `install_guest_bp` in `kmain` — **never fired** during 4.6M+
  wedge beacons. The kernel's PABT path doesn't reach 0x393b84.
  So the ELR=0xffffe4 wedge isn't PABT-driven.
- `log_dabt_forward` repeat-counter (logging up to 16 repeats
  per tuple plus every 64th up to 1024) — also fired 0 times.
  Confirms the DIAG-path is quiescent after the initial
  recursive-DABT forward.

**iter-68 next steps:**

1. **Identify what `DataAbortHandler` is doing.** A `bp` at
   `0x00393114` (DataAbortHandler entry) will fire on the
   first invocation; the dump-and-continue tail logs r0..r12 +
   banked LR/SP. From there, bisect forward to find the loop.
2. **Or instrument the SBA UDF emulator** to count emulations
   per source mode and per faulting_pc. If most ABT-mode SBA
   UDFs cluster at one PC range, that's the loop body.
3. **Cross-check FAR=0x0cd2d000.** That VA is in the kernel's
   global-data window (0x0c100000+). What kernel structure lives
   at 0x0cd2d000? probe/FINDINGS.md or the L1 page-table walker
   should resolve it. If the structure is corrupt at boot
   (e.g. stack collision, uninitialised pointer), the wedge is
   in the kernel's stage-1 mapping setup, not in shadow_stub at
   all.

**Background (unchanged from iter-61):** boot reaches a quiescent
idle at the Newton splash. The framebuffer renders correctly
(`/tmp/newton-fb/00000.png`). All 26 expected tasks alive;
`newt`=RUN, `scrn`=RDY blocked on its event-signal sema-group,
all 24 others BLK. The residual `evt.ex.fr.store` throws are
benign soup-probe misses caught by NewtonScript.

### Iteration 67: PABT-recovery hypothesis falsified; wedge is in DataAbortHandler

#### Method

Two probes added (both reverted before commit):

- `install_guest_bp(0x0039_3b84)` in `kmain`: patches the first
  instruction of `PrefetchAbortHandler` with the marker UDF. The
  default dump-and-continue tail in `handle_user_bp_und` would
  log r0..r12 + banked LR/SP for every kernel-side PABT entry.
- Unsuppressed-repeat counter in `log_dabt_forward`
  (`src/trap.rs` ~6822): logs the first 16 repeat hits per
  (FAR, mode, dedup_mode) tuple plus every 64th up to 1024,
  bypassing the existing dedup so the wedge's dominant fault
  isn't silenced.

Cold-boot, no debugger.

#### Result

Boot reached the wedge state (`ELR=0xffffe4 SPSR=0x40000197
mode=ABT`) at ~3M beacons. Kept running to 4.6M+ beacons.

- **PrefetchAbortHandler bp: 0 hits.** No "guest_bp: HIT at
  0x00393b84" line appeared. The kernel's PABT path doesn't
  reach 0x393b84 in the wedge state. So the wedge isn't
  driven by repeated PABTs.
- **DABT-forward repeat counter: 0 events.** The single
  pre-existing forward (`DFSC=0x5 FAR=0x0cd2d000 mode=0x17`)
  is the only `handle_diag` ABT-source dispatch in the entire
  run — no further aborts go through DIAG.

#### Implication

iter-66's "PABT recovery loop on permanently-unmapped VA"
hypothesis is **falsified**. The trap-rate signature at
`ELR=0xffffe4` (UND_RETURN_STUB) is pure SBA UDF emulation
inside the kernel's `DataAbortHandler` — invoked once for the
recursive DABT at FAR=0x0cd2d000 (mode=0x17), and stuck
running *inside its own body* without making forward progress
on a tight loop of byte accesses.

The wedge is therefore *upstream* of any USR-mode "indirect
call to high VA" bug. The kernel's DataAbortHandler entered with
corrupt state from the original DABT and is now grinding through
emulated byte accesses that never converge to a return.

`LR_abt = 0xe7f842f4` in periodic dumps is just a register value
left over from inside the handler's body; not a hardware abort
artifact.

### Iteration 66: slot 0x424 LDRB hypothesis falsified

#### Method

Added a one-line probe in `shadow_stub::emulate_sba_site` that
logs every SBA UDF firing at slot index `0x424`, including a flag
for "EA inside DrTextChunk" (the false-positive case the iter-65
hypothesis would have produced). Cold-boot run, no debugger.

#### Result

Beacons advanced to ~3M traps at `ELR=0xffffe4 SPSR=0x40000197`,
periodic dumps showed the same wedge state as iter-65 (`current
task 0xc12391c (newt) … pc=0xffffe4 lr=0xe7f842f4 mode=0x17`), and
**zero probe hits** logged. The LDRB at `0x35d110` is not being
executed during the wedge.

#### Implication

The match between `LR_abt = 0xe7f842f4` and the slot-0x424 UDF
encoding is coincidental — slot allocation order happened to give
slot `0x424` to the LDRB at that address, and the UDF byte pattern
`enc_udf(0x8000 | 0x424) = 0xe7f842f4` matches an unrelated
unmapped VA the kernel is retrying. The probe was reverted before
commit (one-line removal); the negative result is the deliverable.

iter-65's hypothesis ("DrTextChunk LDRB scanner reads its own
patched code as data and uses the resulting UDF marker as a
function pointer") is **falsified**.

#### Verification

- Probe was active for >3M traps, never fired.
- chain dump shows DrawSplashScreen → MeasureGlyphWidths → ...
  exactly as in iter-65, but with mode=ABT confirming the wedge
  is *inside the abort handler's retry loop*, not in the USR
  scanner code DrTextChunk represents.

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
