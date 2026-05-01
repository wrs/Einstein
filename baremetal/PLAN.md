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

**Current goal (iter-69):** iter-68 instrumented the SBA UDF
emulator with a per-mode + per-faulting_pc histogram (reverted
before commit). The result is a third hypothesis falsification:
**the wedge is not driven by SBA UDF emulation at all.** SBA UDFs
fired exactly 131 K times during early boot — one histogram
dump, dominated by SVC mode (sites 0x001a7ca8 / 0x001a7cac at
~50 % each) — then went silent for the rest of a 22 M+ beacon
run.

The ABT-mode bucket is ZERO. iter-67's "DataAbortHandler stuck
running emulated byte accesses" mechanism is also wrong. The
wedge is happening **without any SBA UDF traffic at all**.

**Updated mechanism (most plausible):** The CPU is spinning
natively in a tight loop at PC ≈ `0xFFFFE4`, the
`UND_RETURN_STUB`. Two-instruction body:

```
0xFFFFE4: ldr lr, [pc, #0]    ; lr = *(0xFFFFEC)
0xFFFFE8: movs pc, lr         ; PC = lr, CPSR = SPSR_<mode>
0xFFFFEC: <literal>           ; written by EL2 before each ERET
```

If the literal at `0xFFFFEC` was last written to `0xFFFFE4`
(or any PC that immediately re-enters the stub), the stub
loops at 2 cycles/iteration with no traps. Timer IRQs fire ~16
ms apart (or the kernel re-asserts the timer aggressively),
catching the guest at PC=0xFFFFE4 every time. That matches the
~100 K-1 M trap/s "beacon" rate without the ETx2 SBA UDF
volume that iter-67 expected.

**Open questions for iter-69:**

1. **Read the literal at `0xFFFFEC`.** If it's `0xFFFFE4`,
   that's the wedge — a self-referential UND_RETURN_STUB. If
   it's some other PC, follow the chain from there.
2. **What writes the literal?** `trap::return_to_guest_from_und`
   in `src/trap.rs` writes the literal slot before each ERET
   to UND_RETURN_STUB. If the SBA emulator is fed
   `faulting_pc = 0xFFFFE0` (or any address that yields
   `target = 0xFFFFE4` after `+4`), the stub becomes
   self-referential.
3. **What earlier event poisons the literal?** A USR fault
   inside the trampoline region (`0xFFFFXX`) would do it.
   Could also come from a kernel-side `bx LR` where LR holds
   a stale UND_RETURN_STUB target.

**iter-69 plan:**

- One-shot kprintln in `trap::return_to_guest_from_und`
  recording the first time the literal is written with
  `target ∈ 0xFFFFE0..0xFFFFF0` (= self-referential into the
  trampoline region). Logs `(faulting_pc, target, source mode,
  caller LR)`.
- If that fires, `bp` the captured `faulting_pc` to inspect
  USR-side state at the moment the bad target was computed.

**Background (unchanged from iter-61):** boot reaches a quiescent
idle at the Newton splash. The framebuffer renders correctly
(`/tmp/newton-fb/00000.png`). All 26 expected tasks alive;
`newt`=RUN, `scrn`=RDY blocked on its event-signal sema-group,
all 24 others BLK. The residual `evt.ex.fr.store` throws are
benign soup-probe misses caught by NewtonScript.

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
