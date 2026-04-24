# Hypervisor notes for Claude

Newton Hypervisor — pure-Rust Type-1 hypervisor running an unmodified
Newton OS 2.x ROM on Cortex-A53. Two host platforms are supported; the
guest ISA and modelled Newton hardware are identical on both.

- **QEMU `raspi3b`** — the original target. Selected with
  `--features platform-raspi3b` (default). Runs via
  `cargo run --release` (wraps `scripts/run-qemu.sh`). Uses a legacy
  BCM2835 VIC; AArch32↔AArch64 banked-register plumbing is flaky
  (see `docs/QEMU_BUGS.md`).
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — the accurate reference model.
  Selected with `--no-default-features --features platform-fvp-base`.
  Uses GICv3 (the hypervisor brings it up through an EL3 stub). Runs
  via `scripts/fvp <elf>` — the script wraps a dockerised FVP
  (OrbStack + `armswdev/aemfvp-cca-v2-image`). Typical cold boot:
  ```bash
  rm -f /tmp/newton-snapshot-*.bin
  cargo build --release --no-default-features \
    --features "platform-fvp-base quiet"
  scripts/fvp --timeout=90 \
    target/aarch64-unknown-none-softfloat/release/newton-hypervisor
  ```
  Add `--gdb` for an Iris debug server on host port 7100; add
  `--features trace` for the function-level tracer. FVP runs the
  generic timer + cache model accurately, so wall-clock is much
  slower than QEMU TCG — use longer timeouts.

Both platforms must stay green: `guest-tests/scripts/run-all.sh` runs
the 22 guest tests on QEMU, and any new divergence should be tracked
down rather than papered over with a feature gate.

See `README.md` for the user-facing project overview and
`PLAN.md` / `HIGHLEVEL.md` / `IMPLEMENTATION.md` for the phasing.

Phase A is done. Phase B's goal is booting the 717006 ROM through to
`TInterpreter::TInterpreter` at `0x002F40E0`, one stall at a time.
Every iteration is "run, see where it stops, fix, rerun" — which
means the snapshot workflow below matters a lot.

**Important:** Do not trust your memory for details of ARM architecture,
especially EL2-related registers and coprocessor instruction encodings.
ALWAYS check against the actual ARMv7 reference, which is in
docs/ARM_Reference.txt.

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

## gdb workflow

```bash
# term 1
DEBUG=1 cargo run --release

# term 2 (Linux: gdb-multiarch; macOS: aarch64-elf-gdb)
aarch64-elf-gdb -x scripts/gdb-init \
  target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

- EL2 hypervisor breakpoints (`break kmain`, `break
  trap_sync_lower_aarch32`, source-line, `stepi`, `bt`, locals) all
  work. Stack unwinding is reliable within Rust frames; it degrades
  across the EL2 exception vector boundary because the asm stubs have
  no DWARF.
- **Guest AArch32 breakpoints don't work directly** —
  qemu-system-aarch64's gdbstub is aarch64-only and drops the mode
  switch. Use the helpers in `scripts/gdb-init`:
  - **`bg <addr>`** — conditional stop at `trap_sync_lower_aarch32`
    when `$ELR_EL2 == <addr>`. Fires only at naturally-trapping guest
    instructions (data/insn abort, SVC/HVC, CP15). Does **not** catch
    UND-class traps because the UND trampoline HVCs into EL2 — by the
    time we're at trap_sync entry, `ELR_EL2` points at the trampoline,
    not the original PC.
  - **`bp <addr>`** — install a one-shot guest software BP (see
    `src/guest_bp.rs`). Patches the ROM word with `UDF #0xFFFE` and
    stops in `handle_user_bp_und` with `faulting_pc` = the guest PC.
    Works for any ROM-range PC regardless of whether it naturally
    traps. One-shot: `bp <addr>` again to re-arm. Snapshot autosaves
    are gated while any BP is live, so a debug session never corrupts
    a persisted snapshot.
  - `tt N`, `guest-state`, `bp-clear`, `bp-list` — convenience.

### Breakpoint pattern for agents

The typical recipe:

```bash
# term 1
DEBUG=1 cargo run --release

# term 2
aarch64-elf-gdb -x scripts/gdb-init \
  target/aarch64-unknown-none-softfloat/release/newton-hypervisor
(gdb) break trap_sync_lower_aarch32     # land anywhere in EL2 context
(gdb) c                                  # stop at first guest sync-trap
(gdb) bp 0x<guest_pc_of_interest>        # install sw BP + arm stop
(gdb) delete 1                           # remove the trap_sync bp
(gdb) c                                  # run until guest hits your BP
(gdb) p/x faulting_pc                    # which BP fired
(gdb) guest-state                        # ELR/ESR/FAR/CPSR at trap
(gdb) c                                  # resume (handler restores word)
```

For a guest PC that naturally traps (e.g., the MMIO access you already
saw in a log), skip the install: `bg <addr>` and `c` is enough.

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

## Function-level execution trace

`cargo run --release --features trace,quiet` produces a chronological
log of *every* call to a recognised Newton function, with the
argument registers at the moment of entry:

```
trace     1 0x000188f8 FlushTheCache (svc) r0=0x... r1=0x... r2=0x... r3=0x...
trace     2 0x00045b78 HandleDebugCard (svc) r0=0x... r1=0x... r2=0x... r3=0x...
...
```

Every call, not first-touch — a function that's invoked ten times
produces ten trace lines. Useful for Phase-B bisection: you see not
just *which* function is at the top of the stall, but what arguments
it's being called with over time (loop counter advancing, page index,
etc.).

### Mechanism (`src/tracer.rs`)

1. `build.rs` reads `scripts/classify-out/code-symbols.txt` (the
   curated code-only symbol list produced by `classify-symbols.py`,
   i.e. the same vetted list the shadow-stub classifier's walker
   uses as its root set) and emits `fn_addrs.bin`,
   `fn_name_offs.bin`, `fn_names.bin` into OUT_DIR. The address list
   is trusted — no runtime prologue heuristic, no "does this word
   look like a function start" check.
2. At ROM load time (after rom_patches have been applied)
   `tracer::init()` installs a 5-word trampoline per function inside
   the ROM backing store at IPA 0x00900000..0x00E00000 (past the REx
   tail, before the UND-trampoline region at 0x00FFFF00), and
   rewrites each function's first word to `B trampoline_slot`.
3. Trampoline layout per slot (20 bytes):
   - slot[0]: `HVC #TRACE_TAG`  — log + args
   - slot[1]: original first insn, rewritten if PC-relative:
     `LDR Rd, [pc, #imm]` → `LDR Rd, [pc, #0]` + literal at slot[3];
     `B <label>`         → `LDR PC, [pc, #0]` + target at slot[3].
     Anything else copies verbatim.
   - slot[2]: `LDR PC, [pc, #0]`  — loads branch-back target
   - slot[3]: literal (only used by rewrite cases)
   - slot[4]: `orig_pc + 4`  — branch-back target
4. At HVC-entry time, `handle_trace_hvc` derives the slot index from
   ELR_EL2, looks up the function name, prints `seq PC name (mode)
   r0..r3`, and returns. Natural ERET resumes at slot[1] — the
   original first instruction — and the trampoline falls through to
   the branch-back at slot[4]. The trampoline never disarms itself,
   so every subsequent call retraces.

### Why the classifier list, not prologue detection

The prior implementation filtered `demangled_symbols.txt` entries by
first-word shape (must match PUSH / SUB sp / MOV imm / …). That was
a heuristic fence against mislabelled data entries. The classifier's
`code-symbols.txt` has already partitioned symbols into code / data /
drop; using it directly removes an independent heuristic and keeps
the tracer's coverage in lock-step with shadow_stub's definition of
"real code".

### Logging budget

- `quiet` feature silences `fix_stage1_xn_bits:` summaries via
  `dprintln!` in `src/uart.rs`. Route further recurring diagnostic
  logs through `dprintln!` (not `kprintln!`) to keep trace output
  readable. `dprintln!` is a no-op under `quiet`; `kprintln!` is
  always emitted.

### Gotchas

- `trace` mutates many ROM words (both the function first-word
  patches and the in-ROM trampoline slots), so snapshots saved with
  trace off are rejected on load, and vice-versa. Tracing runs are
  cold-boot runs — clear `/tmp/newton-snapshot-*.bin` before the
  first boot.
- Functions called before the hypervisor's `tracer::init()` completes
  (i.e. before the ROM is handed off to the guest) obviously can't
  fire. In practice all Newton functions are called after
  handover — the reset vector runs guest-side.
- If `code-symbols.txt` lists an address whose first word is a PC-
  relative form the rewriter can't handle (very rare, e.g. an
  indirect-to-PC via register), that entry is counted in the
  `rewrite-skip` column at install time and left unpatched. The
  function still runs correctly; it just isn't traced.
- Every call fires an HVC. On a long boot the trace volume can
  saturate the mini-UART; lean on `quiet` and/or grep.

## Reference docs

When debugging or investigating, consult these FIRST before
re-deriving state from disassembly or tool output:

- [`docs/DISASM.md`](docs/DISASM.md) — how to use
  `scripts/disasm-out/rom.dis`, the full symbol-annotated ROM+REx
  disassembly. **Don't hex-decode ROM bytes by hand; use the disasm.**
- [`docs/NEWTON_INTERNALS.md`](docs/NEWTON_INTERNALS.md) — APCS
  calling convention, two-level object dispatch, ROM jump-table
  (0x01A00000..0x01C20000) as the post-ship patch mechanism, DDK
  header locations.
- [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md) — QEMU raspi3b bugs at
  the AArch64↔AArch32 boundary. Grep this before suspecting our
  own code at that boundary. **Especially relevant for banked
  registers at AArch32 EL1 ↔ AArch64 EL2 exception entry — the
  apparent "flaky `ctx.x[13]` / `ctx.x[14]`" has been
  misdiagnosed as a QEMU bug multiple times; it is architected
  behaviour per ARM ARM Table D1-79. `ctx.x[14]` is `LR_usr`,
  `LR_abt` lives in `ctx.x[20]`, etc. Read the file before
  assuming banked-reg weirdness is a bug.**
- [`docs/WORKFLOW.md`](docs/WORKFLOW.md) — process notes: review
  Einstein-driver ports with a sub-agent, test-per-feature rule,
  finish-the-phase semantics.
- [`docs/peripherals.md`](docs/peripherals.md) — peripheral
  implementations.
