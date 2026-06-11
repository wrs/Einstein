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
- All 35 guest tests must pass on every commit that touches hypervisor
  functionality (not merely diagnostics):
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current state (2026-06-10):** The goal of this plan is reached. The
717006 ROM boots to the Welcome UI and the builtin apps work
interactively — on QEMU raspi3b, on ARM FVP, and on a real
Pi Zero 2 W with HDMI display, USB touch, HDMI audio, and SD-backed
flash persistence (non-blocking DMA autosave). All 35 guest tests are
green. The Phase-B debugging diary that used to live here (stack-VM
patches, ResolveFault wrapper, matcher-mismatch hunts) is archived in
git history; the durable findings are in `docs/STRUCTURES.md` and
`docs/NEWTON_INTERNALS.md`. Real-hardware bring-up is tracked in
`docs/REAL_HW_BRINGUP.md`.

## Current goals

1. **Add-on app packages** — the known functional gap. The `.pkg`
   installation flow (soup storage, package loader, possibly native
   code inside packages) is unexercised. Needs an investigation pass:
   load a known-simple package, see where it stops, fix, repeat.
2. **Phase 6 remainder** — serial port and PCMCIA images on real
   hardware (audio is done). See `docs/REAL_HW_BRINGUP.md` §Phase 6.
3. **Debug-scaffolding teardown** — boot has quiesced; the
   Phase-B probes listed under "Diagnostic scaffolding" below are now
   removal candidates.
4. **M7 — performance and polish** (HIGHLEVEL.md §12): measurement vs
   the real 162 MHz StrongARM; display-scaling quality on real hw.

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
- **Pi Zero 2 W** — real hardware.
  `PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest>`
  assembles the boot partition. See `docs/REAL_HW_BRINGUP.md`.

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`. Patches every
  `scripts/classify-out/code-symbols.txt` entry with HVC trampoline.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor function-
  entry traces.
- **`build/NewtonProbe`** — Einstein-as-oracle.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s from `trap_irq` (QEMU/FVP; deferred on real hw).
- **Live display + pen input** — `src/host_io/` forwards each
  `screen::blit` to a companion viewer at `tools/host-viewer/`
  via `/tmp/newton-host-io/` (semihosting files); the viewer posts
  mouse events back as Newton pen samples. Enabled via
  `--features host-io-semihost`.

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

`baremetal/guest-tests/scripts/run-all.sh` runs the 35 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011 and runs the verify-mmu
  alias detector; UND-vector trampoline; DABT/PABT DIAG patches.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / SBA /
  tracer / canary / probe tags); `handle_data_abort` with kernel-DABT
  forwarding for lazy stack growth; `trap_irq` + the same-EL slim ISR.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, TPC, TPU, IMO, FMO, AMO,
  DC); CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3.
- `src/banked.rs` — AArch32 banked-register access from EL2 (Table
  D1-79).
- `src/rom_patches.rs` — Einstein word-write patches; HVC injection
  helpers; canaries; ResolveFault wrapper.
- `src/peripherals/*` — Newton driver / native-primitive surface.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-*.bin`.
- `src/flash_persist/` + `src/sd/` — SD-backed flash persistence with
  DMA autosave (`docs/SD_DMA_AUTOSAVE.md`).
- `src/usb/` + `src/input/` — DWC2 USB host + MTouch pen input.
- `src/audio/` — VC4 HDMI MAI sound output.
- `src/tracer.rs` — function-level tracer.
- `src/guest_bp.rs` — `bp <addr>` for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `guest-tests/tests/` — 35 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 35 tests must pass.

## Non-goals

- Multi-ROM switching, JIT, software CPU emulation, Pi 4/5 support
  (HIGHLEVEL.md §15).

## Diagnostic scaffolding (active)

Boot has quiesced; these are now teardown candidates (goal 3 above).

- `verify-mmu` in `fix_stage1_xn_bits` — ratchet-logs subpage-AP
  heterogeneity and per-alias-onset `(PA, VA1, VA2)` tuples.
- `handle_page_get_probe` (PAGE_GET_PROBE_HVC_IMM=0x53) on
  `0x00258EFC` — page-allocator return logger + dup detector.
- `handle_remember_entry_probe_with` (REMEMBER_PROBE_HVC_IMM=0x46)
  on `0x00258E0C` — Remember-side per-PA → first-VA aliasing tracker.
- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.
- `alrt_capture` / `g1_capture` stage-2 write captures, armed at boot.
