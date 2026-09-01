# Hypervisor notes for Claude

Newton Hypervisor — pure-Rust Type-1 hypervisor running an unmodified
Newton OS 2.x ROM on Cortex-A53. The 717006 ROM boots to the Welcome UI
and the builtin apps work on all three hosts (QEMU `raspi3b`, ARM FVP,
real Pi Zero 2 W); add-on app packages are the known gap. Day-to-day
work is "run, see where it stops, fix, rerun".

This file is doctrine and an index only. The detail lives in the docs
below — read the relevant one before acting, don't re-derive it.

| Question | Read |
|---|---|
| What is this, how do I build/run/test it, what does each feature do | [`README.md`](README.md) |
| Architecture — memory model, traps, endianness, peripherals | [`HIGHLEVEL.md`](HIGHLEVEL.md) |
| Build system, source layout, classifier pipeline, test tiers | [`IMPLEMENTATION.md`](IMPLEMENTATION.md) |
| Current state, remaining work, per-stop workflow | [`PLAN.md`](PLAN.md) |
| Wedge triage, gdb, guest breakpoints, what to run before committing | [`docs/DEBUGGING.md`](docs/DEBUGGING.md) |
| ARM architecture facts (ARMv7 / AArch64 reference text) | `docs/ARM_Reference.txt`, `docs/ARM_aarch_Reference.txt` |
| Reading the ROM — annotated disassembly | [`docs/DISASM.md`](docs/DISASM.md) |
| Newton internals — APCS, object dispatch, ROM jump-table, DDK headers | [`docs/NEWTON_INTERNALS.md`](docs/NEWTON_INTERNALS.md) |
| Kernel struct layouts (TScheduler, TTask, TObjectTable, task census) | [`docs/STRUCTURES.md`](docs/STRUCTURES.md) |
| QEMU raspi3b quirks, especially banked registers | [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md) |
| Working style — assembler round-trips, Einstein-port review, test-per-feature | [`docs/WORKFLOW.md`](docs/WORKFLOW.md) |
| Peripheral models (Newton-side spec + Einstein cross-refs) | [`docs/peripherals.md`](docs/peripherals.md) |
| Real hardware — Pi Zero 2 W firmware, SD/display/USB/audio stacks | [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md), [`docs/MTOUCH.md`](docs/MTOUCH.md), [`docs/SD_DMA_AUTOSAVE.md`](docs/SD_DMA_AUTOSAVE.md) |
| Load a new image onto the Pi over serial, power-cycle it, capture its console (`scripts/pi-upload.py`, nhboot) | [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md) "Serial image upload" |
| Endianness — a BE-32 ROM run by a BE-8 guest | [`docs/ENDIAN_FIXES.md`](docs/ENDIAN_FIXES.md) |
| Native code in add-on packages; triaging a wedge PC in RAM | [`docs/PACKAGE_NATIVE_CODE.md`](docs/PACKAGE_NATIVE_CODE.md) |
| What a snapshot does and does not restore | [`docs/SNAPSHOT_RESUME_CONTRACT.md`](docs/SNAPSHOT_RESUME_CONTRACT.md) |
| Oracle: what a fully-booted Newton actually does | `probe/FINDINGS.md` |
| How the project was built — the historical record (the one doc allowed to narrate the past) | [`docs/project-history.md`](docs/project-history.md) |

## Rules

- **Don't trust your memory for ARM architecture details** — especially
  EL2 registers and coprocessor encodings. Check `docs/ARM_Reference.txt`.
  Round-trip every instruction encoding you write through
  `arm-none-eabi-as` + `objdump`; hand-computed imm12 rotations are
  silently wrong (`docs/WORKFLOW.md`).
- **Never silence a loud halt.** Unknown inputs on emulation paths halt
  with a context dump that names the table entry to extend. Adding a
  silent default destroys the trip-wire.
- **Bitmap-first triage.** When a wedge names a guest PC in ROM, check
  whether that address is marked as code in the classifier bitmap
  *before* investigating trap state, banked registers or the ERET path.
  If it isn't, the fix is a classifier seeder, not a runtime handler.
  Recipe in [`docs/DEBUGGING.md`](docs/DEBUGGING.md).
- **Banked registers are not a QEMU bug.** `ctx.x[14]` is `LR_usr`,
  `LR_abt` is `ctx.x[20]`, per ARM ARM Table D1-79. This has been
  misdiagnosed repeatedly — read `docs/QEMU_BUGS.md` first.
- **The snapshot ring is off by default** (behind the default-off
  `snapshot` cargo feature). A normal build never writes
  `/tmp/newton-snapshot-*.bin` and always cold-boots — no `rm -f`
  needed. Only turn it on (`--features snapshot`) if you're working on
  the ring itself; resuming a Newton-ROM snapshot still wedges the
  guest in a prefetch-abort loop (item 2 in `PLAN.md`), so with the
  feature on you're back to `rm -f /tmp/newton-snapshot-*.bin` before
  each run and must never treat a resumed run as a verification signal.
  Flash persistence (`~/.newton/flash.bin`, SD store) is independent of
  this feature and keeps working either way.
- **Both emulated platforms stay green.** `guest-tests/scripts/run-all.sh`
  before any commit that touches hypervisor functionality; track down
  QEMU/FVP divergence rather than gating it behind a feature. The one
  exception (probe-only iterations) is spelled out in
  [`docs/DEBUGGING.md`](docs/DEBUGGING.md).
- **Route recurring diagnostic logs through `dprintln!`**, not
  `kprintln!` (`src/host/macros.rs`) — `dprintln!` is a no-op under the
  `quiet` feature, which is what keeps trace runs readable.
- **Extend `docs/STRUCTURES.md`** whenever you decode another kernel
  struct from the disasm. That's how debugging stays cumulative.

## Commands

```bash
cargo run --release                                      # cold boot on QEMU (ring off by default)
scripts/boot-check.sh --cold                              # headless boot verify
guest-tests/scripts/run-all.sh                            # 39 guest tests (QEMU)
guest-tests/scripts/run-all.sh --platform fvp             # same on FVP
scripts/check-matrix.sh                                   # 19 build combos + lints

# Real Pi: build, power-cycle, upload the delta over serial, boot, capture
# The cargo build MUST be the last build before pi-upload: any
# default-features build (cargo run, boot-check.sh) replaces the same
# artifact with a semihost binary that hangs silently on hardware.
# pi-upload refuses those (no pinned ROM blob), but don't rely on it.
cargo build --release --no-default-features --features pi-bare-metal-input
scripts/pi-upload.py --kernel target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
  --until 'Welcome to NewtonScript' --timeout 120          # console → stdout + /tmp/newton-claude/serial.log
scripts/pi-upload.py --no-upload                          # power-cycle and watch until Ctrl-C

# FVP (accurate reference model; much slower than QEMU — long timeouts)
cargo build --release --no-default-features \
  --features "platform-fvp-base rom-717006 quiet diag"
scripts/fvp --timeout=90 \
  target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

`README.md` has the full cheatsheet, the feature table, and the
live-display/pen-input setup.
