# Hypervisor notes for Claude

Newton Hypervisor — pure-Rust Type-1 hypervisor running an unmodified
Newton OS 2.x ROM on Cortex-A53 under QEMU `raspi3b`. See
`README.md` for the user-facing project overview and
`PLAN.md` / `HIGHLEVEL.md` / `IMPLEMENTATION.md` for the phasing.

Phase A is done. Phase B's goal is booting the 717006 ROM through to
`TInterpreter::TInterpreter` at `0x002F40E0`, one stall at a time.
Every iteration is "run, see where it stops, fix, rerun" — which
means the snapshot workflow below matters a lot.

## Snapshot / resume workflow (Phase B)

`src/snapshot.rs` rolls four guest-state snapshots on disk at
`/tmp/newton-snapshot-{0..3}.bin`. On every hypervisor startup we
try to resume from the newest valid slot; missing or mismatched
files fall through to a cold boot.

### Commands

```bash
# Cold boot (fresh, ignore any existing snapshots).
rm -f /tmp/newton-snapshot-*.bin
cargo run --release

# Normal run — loads the newest slot if any exist, else cold-boots.
cargo run --release

# Force cold boot without deleting, by making slots unreadable:
chmod 000 /tmp/newton-snapshot-*.bin && cargo run --release
# (then chmod 644 to restore)

# Inspect slots (size + mtime).
ls -la /tmp/newton-snapshot-*.bin
```

### Save triggers

- **Periodic (default):** every `AUTOSAVE_INTERVAL_MS = 2000` ms of
  wall time, hooked into `trap_irq` (timer IRQ) in `src/trap.rs`.
  Wall-clock pacing, not trap count — a pathological abort loop
  won't thrash saves.
- **Guest-triggered:** `HVC #0x20` from the guest issues an
  immediate save. Handy for guest tests that want to snapshot at
  a specific PC.

### How to use this during debugging

1. `cargo run` — hypervisor boots, saves every 2s, eventually
   wedges (current Phase B starting point: guest stuck at
   PC=0x10 / PC=0xC depending on which abort-loop branch).
2. Notice the failure. The newest slot holds the state at the
   moment the timer last fired — usually already inside the
   failure, but the older slots cover the preceding 2 / 4 / 6
   seconds. Four slots = ~8 seconds of rewindable history.
3. Edit hypervisor code. `cargo run` again.
4. On startup the hypervisor loads the newest slot and ERETs to
   its saved PC, bypassing the entire ROM boot-up. You see
   "Resuming guest from snapshot at PC=…" instead of "Entering
   Newton ROM…".
5. Observe whether the fix changed behaviour past the saved point.

If the newest slot is already past where you want to land, copy an
older slot on top:

```bash
cp /tmp/newton-snapshot-2.bin /tmp/newton-snapshot-0.bin  # pin slot 0 to the older state
```

(The loader picks the file with the highest `seq`. Copying an
older file into a newer-seq slot puts it at the top of the stack
because the copied file's header carries its own seq — so the
loader sees two slots with the same high seq, picks one
deterministically, and restores the older state.)

### What survives a rebuild

Only guest-visible state: GUEST_RAM, GUEST_FB, flash, the EL1
CP15 regs we can reach from AArch64 EL2, and x0..x14 of the
currently-active guest AArch32 mode. Hypervisor-side EL2 code
addresses, trap tables, VIC state, timer deadlines, and so on
are fresh each boot. That's the point: edit hypervisor code,
rebuild, resume.

### ROM fingerprint

The snapshot header embeds a FNV-1a hash of the first 1 KiB of
GUEST_ROM after load-time patches. If you swap a guest-test
binary for the ROM (or vice versa), the loader notices the
mismatch and cold-boots instead of ERET-ing into someone else's
code. Same applies across Einstein.rex changes that shift the
early ROM bytes.

### Known limitations

- Banked `SP_` / `LR_` for non-active AArch32 modes are not saved
  (LLVM's AArch64 assembler doesn't expose the banked mnemonics
  for those). The Newton kernel initialises SP per-mode on mode
  entry, so this matches observed behaviour. If a future
  snapshot resumes inside an exception handler that was taken
  through a banked SP that we didn't restore, we'll need an
  AArch32 stub to widen coverage.
- Each save is ~14 MiB (RAM + FB + flash + header) through
  semihosting SYS_WRITE. Fast enough at 2 s cadence but will
  become painful if the cadence tightens.
- The autosave hook runs from `trap_irq`, so a guest that never
  takes a timer IRQ won't produce fresh snapshots. In practice
  the Newton kernel arms its match registers very early and
  CNTHP fires steadily; this hasn't been an issue.

## General Phase-B debugging guidance

- Every handler in `src/trap.rs` / `src/peripherals/*` halts
  loudly on unknown inputs with a context dump. When a ROM boot
  trips one, the halt message points at exactly the table entry
  that needs adding. **Don't paper over it** by adding a silent
  default — the loud halt is the trip-wire.
- Before extending a handler, cross-check Einstein's behaviour
  at `Emulator/TNativePrimitives.cpp`, `Emulator/Serial/*`,
  `Emulator/TEmulator.cpp`, etc. — those are the oracles.
- `probe/FINDINGS.md` is the capture of what a fully-booted
  Newton actually does; consult it before guessing. Regenerate
  with `cmake --build build --target NewtonProbe` and
  `build/NewtonProbe baremetal/roms/newton.rom - 90`.
- Guest tests in `guest-tests/tests/` exercise each handler in
  isolation. A Phase B regression in handler code should show
  up as a failing test; run `guest-tests/scripts/run-all.sh`
  before committing.
