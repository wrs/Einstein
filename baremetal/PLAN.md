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
- All 37 guest tests must pass on every commit that touches hypervisor
  functionality (not merely diagnostics):
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current state (2026-06-10):** The goal of this plan is reached. The
717006 ROM boots to the Welcome UI and the builtin apps work
interactively — on QEMU raspi3b, on ARM FVP, and on a real
Pi Zero 2 W with HDMI display, USB touch, HDMI audio, and SD-backed
flash persistence (non-blocking DMA autosave). All 37 guest tests are
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
3. **Debug-scaffolding teardown** — done. The one-off Phase-B residue
   has been deleted: the write-capture/`newt`/`cdsv` tripwires, the
   subpage-AP and alias-onset audit inside `fix_stage1_xn_bits`, the
   parked-PC wedge probe (audio-null now arms its own DMA-completion
   IRQ), the never-installed stack-pad/lock-heap wrappers, and the
   dead `shadow_pool` / `usb_probe` modules. What remains is listed
   under "Diagnostic scaffolding" below, kept deliberately as
   tripwires and debugging tooling.
4. **Targeted guest-TLB maintenance.** The hypervisor rewrites guest
   stage-1 PTEs behind the guest's back (`fix_stage1_xn_bits`,
   scratch-pool L1-section install) with no TLBI at the rewrite sites;
   today a blanket `vmalle1` per 16 ms heartbeat (`timer::on_irq`)
   bounds stale-entry lifetime. Replace the blanket flush with
   targeted TLBIs at each rewrite site, then drop it.
5. **M7 — performance and polish** (HIGHLEVEL.md §12): measurement vs
   the real 162 MHz StrongARM; display-scaling quality on real hw.

## Workflow per stop

1. Reproduce the stall on QEMU and capture the loud-halt context dump
   (or the last snapshot before the wedge — see CLAUDE.md).
2. Identify the kernel-side code at the wedge PC from the disasm
   (`scripts/disasm-out/rom.dis`) and instrument the relevant entry
   point with an HVC probe if more detail is needed.
3. Cross-reference with Einstein (`build/NewtonProbe baremetal/roms/
   newton.rom _Data_/Einstein.rex 30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — `src/peripherals/*.rs`, `src/trap/`.
   - **Einstein behavioural quirk** — port the matching logic.
   - **ROM patch** — `src/rom_patches.rs`. Only when no other layer can
     host the fix.
5. Re-run, confirm the wedge is gone, repeat for the next stop.

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

`baremetal/guest-tests/scripts/run-all.sh` runs the 37 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green. Set
`CHECK_MATRIX=1` to also run `scripts/check-matrix.sh` (10 feature
combos) at the top of the run.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011, clears XN, rewrites fine-table
  L1 placeholders to fault; CP15-encoding rewrites.
- `src/guest_trampolines.rs` — UND/DABT/PABT vector trampolines + the
  hypervisor-code range predicate.
- `src/guest_regions.rs` — the single region manifest (ipa/size/
  host_pa/perms/snapshot) driving stage2, host_addr_for, and snapshot.
- `src/trap/` — `mod.rs` (sync-trap + IRQ dispatch, same-EL slim ISR),
  `dabt.rs` (`handle_data_abort` with kernel-DABT forwarding for lazy
  stack growth), `und.rs`, `cp15.rs` (CP15 shim), `hvc.rs` (tag
  dispatch); `src/probes.rs` for the Newton-ROM probe handler bodies.
- `src/host_dma.rs` — host-side BCM2835 DMA driver (UART TX, MAI, SD).
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
- `guest-tests/tests/` — 37 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 37 tests must pass.

## Non-goals

- Multi-ROM switching, JIT, software CPU emulation, Pi 4/5 support
  (HIGHLEVEL.md §15).

## Diagnostic scaffolding (active)

The one-off Phase-B probes are gone (the write-capture tripwires,
the subpage-AP/alias audit inside `fix_stage1_xn_bits`, the parked-PC
wedge probe, `shadow_pool`, `usb_probe`); these stay as permanent
tripwires and debugging tooling.

- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries and the
  BUS_ERROR_THROW loud-halt capture in `rom_patches.rs`, gated on
  `cfg(nh_loud_halt_canaries)` (semihost/dev builds only — off on
  real hardware so a user reset doesn't halt the hypervisor).
- The function-level tracer (`--features trace`) and `guest_bp`
  one-shot software breakpoints for the gdb workflow.
