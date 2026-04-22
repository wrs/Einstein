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

`cargo run --release --features trace,quiet` produces a "first-touch"
chronological log of every recognised Newton function as the guest
enters it for the first time:

```
trace     1 PC=0x000188f8 LR=0x0001889c (svc) FlushTheCache
trace     2 PC=0x00045b78 LR=0x000188a4 (svc) HandleDebugCard
...
```

Useful for Phase-B bisection: a stall at some PC is immediately
readable as "N functions deep into the boot, right after `FooBar`".

### Mechanism (`src/tracer.rs`)

1. `build.rs` parses `../_Data_/demangled_symbols.txt` when the
   `trace` feature is on, keeps word-aligned ROM-range entries whose
   names look like functions (uppercase-leading, C++ `::` / `(`),
   drops linker markers (`Image$$…`, `…$Size`), and emits three
   blobs into `OUT_DIR`: `fn_addrs.bin`, `fn_name_offs.bin`,
   `fn_names.bin`.
2. At ROM load time `tracer::init()` registers the table but does
   **not** patch yet — the UND trampoline's save slot at VA
   `0x0C00_4F00` only translates to the correct RAM IPA once the
   guest's own stage-1 L1 is in place.
3. On the first `SCTLR_EL1.M = 0 → 1` transition (intercepted via
   `HCR_EL2.TVM`), `tracer::enable_patches()` walks `FN_ADDRS`,
   reads each entry's current first word, and if it matches a
   known ARM function-start allowlist (PUSH/STMFD sp!, SUB sp,
   ADD ip-sp, STR lr / MOV ip-sp, MOV Rd-imm / MVN / MOV reg, LDR
   pc-relative, MRC/MCR p15, B-cond-AL), stashes the original and
   overwrites with `UDF #index` (A1 encoding `0xE7F0_00F0` with a
   16-bit imm split). After the loop it does `dsb ish; ic ialluis;
   dsb ish; isb` to publish the new instructions to the guest's
   fetch path.
4. On each trace UND the handler logs, restores the original word
   in the ROM backing, invalidates the icache line via
   `cpu::ic_ivau`, and rewinds `ELR_EL2` to the faulting PC.

### UND trampoline extension

To capture the caller's LR, the UND-vector trampoline
(`patch_und_vector` in `src/guest_mem.rs`) briefly switches to SVC
mode (`msr cpsr_c, #0xd3`), snapshots `R14_svc` into
`UND_SAVE_LR_SVC_IPA = 0x0400_5F08`, and switches back before HVC.
Reason: `MRS X, ELR_EL1` from AArch64 EL2 returns 0 under QEMU
raspi3b for AArch32 banked state — same plumbing limitation that
forces `LR_und` / `SPSR_und` to be persisted via RAM. The trampoline
body is now 13 words at `0x00FFFF00..0x00FFFF34`; extend the
reserved range in `tracer::in_reserved_range` if you grow it
further.

### Logging budget

- `quiet` feature silences `fix_stage1_xn_bits:` summaries via
  `dprintln!` in `src/uart.rs`. Route further recurring diagnostic
  logs through `dprintln!` (not `kprintln!`) to keep trace output
  readable. `dprintln!` is a no-op under `quiet`; `kprintln!` is
  always emitted.

### Gotchas

- `trace` mutates many ROM words, so snapshots saved pre-patch are
  rejected on load. Tracing runs are cold-boot runs — clear
  `/tmp/newton-snapshot-*.bin` before the first boot.
- Functions called before the guest's stage-1 MMU comes on
  (`Reset` → early `ROMBoot` up through the first SCTLR.M=1 write)
  are not in the trace. The first trace line today is
  `FlushTheCache` at `0x000188f8`, which is just after the kernel
  installs its initial L1 tables.
- The `(svc)` mode label on each trace line is authoritative; the
  LR value is only reliable when mode == svc. For other modes the
  slot still holds the last stored `R14_svc` from the SVC bounce.
