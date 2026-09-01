# Debugging a wedge

The recipes behind the triage doctrine in [`../CLAUDE.md`](../CLAUDE.md):
where a wedge PC comes from, how to break on it, and which layer owns
the fix. The per-stop workflow (reproduce → triage → disasm → Einstein
oracle → fix → rerun) is in [`../PLAN.md`](../PLAN.md).

## Bitmap-first triage

Whenever a wedge names a specific guest PC — UND at `PC=X`, PABT at
`X`, "wild branch to X", an instruction at `X` decoding as garbage —
check **first** whether `X` is marked as code in the classifier's reach
bitmap, before digging into trap state, banked registers, or the ERET
path:

```bash
grep -E "^  $(printf '%08x' $X | cut -c1-6)" \
  baremetal/classify/*/code-regions.txt
```

If `X` isn't marked, the loader didn't byteswap that word at load time,
the guest fetches BE bytes as LE, and the decode IS garbage — nothing
to debug at the runtime layer. The fix is a new seeder in
`tools/classify-rom/src/main.rs` for the structure that contains `X`,
not a change under `src/hv/trap/`.

Regenerate with `scripts/regen-classify.sh [ver]` (default 717006; also
refreshes `code-symbols.txt`), then `scripts/dump-data-regions.py` to
refresh `code-regions.txt` so the same grep verifies the fix.

The doctrine does **not** apply to a wedge PC in RAM — see
[`PACKAGE_NATIVE_CODE.md`](PACKAGE_NATIVE_CODE.md).

## Loud halts are the trip-wire

Every handler in `src/hv/trap/` and `src/peripherals/*` halts loudly on
unknown input with a context dump. When a ROM boot trips one, the halt
message names exactly the table entry that needs adding. **Never** add
a silent default to quiet it.

Before extending a handler, cross-check Einstein's behaviour — it is
the oracle: `Emulator/TNativePrimitives.cpp`, `Emulator/Serial/*`,
`Emulator/TEmulator.cpp`. `probe/FINDINGS.md` is the captured record of
what a fully-booted Newton actually does; consult it before guessing.
Regenerate it with `cmake --build build --target NewtonProbe` and
`build/NewtonProbe baremetal/roms/newton.rom - 90`.

## gdb on QEMU

```bash
# term 1
DEBUG=1 cargo run --release

# term 2 (Linux: gdb-multiarch; macOS: aarch64-elf-gdb)
aarch64-elf-gdb -x scripts/gdb-init \
  target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

EL2 breakpoints (`break kmain`, `break trap_sync_lower_aarch32`,
source-line, `stepi`, `bt`, locals) all work. Stack unwinding is
reliable within Rust frames; it degrades across the EL2 exception
vector boundary because the asm stubs have no DWARF.

FVP side: `scripts/fvp --gdb <elf>` exposes an Iris debug server on
host port 7100.

### Guest AArch32 breakpoints

`qemu-system-aarch64`'s gdbstub is aarch64-only and drops the mode
switch, so guest breakpoints need the helpers in `scripts/gdb-init`:

- **`bg <addr>`** — conditional stop at `trap_sync_lower_aarch32` when
  `$ELR_EL2 == <addr>`. Fires only at naturally-trapping guest
  instructions (data/insn abort, SVC/HVC, CP15). Does **not** catch
  UND-class traps: the UND trampoline HVCs into EL2, so by the time
  we're at trap_sync entry `ELR_EL2` points at the trampoline, not the
  original PC.
- **`bp <addr>`** — one-shot guest software breakpoint
  (`src/diag/guest_bp.rs`). Patches the ROM word with `UDF #0xFF0E` and
  stops in `handle_user_bp_und` with `faulting_pc` = the guest PC.
  Works for any ROM-range PC whether or not it naturally traps. `bp
  <addr>` again to re-arm. Snapshot autosaves are gated while any BP is
  live, so a debug session never corrupts a persisted snapshot.
- `tt N`, `guest-state`, `bp-clear`, `bp-list` — convenience.

### Typical recipe

```
(gdb) break trap_sync_lower_aarch32     # land anywhere in EL2 context
(gdb) c                                  # stop at first guest sync-trap
(gdb) bp 0x<guest_pc_of_interest>        # install sw BP + arm stop
(gdb) delete 1                           # remove the trap_sync bp
(gdb) c                                  # run until guest hits your BP
(gdb) p/x faulting_pc                    # which BP fired
(gdb) guest-state                        # ELR/ESR/FAR/CPSR at trap
(gdb) c                                  # resume (handler restores word)
```

For a guest PC that naturally traps (e.g. an MMIO access you already
saw in a log), skip the install: `bg <addr>` then `c`.

### gdb-callable dump shims

`diag` builds export four `#[no_mangle]` C-ABI entry points in
`src/diag/task_dump.rs` (pinned with `#[used]` so LTO keeps them).
They read guest memory through the live stage-1 walker, which gdb
itself cannot do from EL2, and print to the kernel log. Stop at any
EL2 trap first (`break trap_sync_lower_aarch32`, `continue`) so the
walker sees a consistent TTBR, then `set language c` and:

```
call ((void(*)())diag_dump_current_task_chain)()          # gCurrentTask save area + APCS chain (dump() skips the current task)
call ((void(*)(unsigned))diag_dump_task_chain)(0xc124c98)  # same for one TTask VA
call ((void(*)(unsigned,unsigned))diag_dump_words)(0xc10155c, 8)   # hex-dump guest VAs
call ((void(*)(unsigned,unsigned))diag_dump_refvar)(ctx->x[1], 24) # follow a RefVar* (RC6RefVar ABI) to its object
call ((void(*)(unsigned,unsigned))diag_dump_refvar_deep)(ctx->x[1], 24) # ...and dump any large-object VAddr it holds
```

`dump_current_chain(ctx)` / `dump_chain_at(ctx, pc)` (existing) render
the *trap* context; the `diag_dump_*task_chain` pair render the
*kernel save area*, which is what you want for a task that is parked
in a SWI. `scripts/gdb-init`'s `bp`/`bp-clear` call the unmangled
`install_guest_bp` / `remove_guest_bp` symbols; run `set language c`
before `source scripts/gdb-init` in a batch script.

A minimal batch session that arms a guest BP on a ROM function and
dumps the chain when it fires (used to find the `-48421` throw site):

```
set language c
source scripts/gdb-init
break trap_sync_lower_aarch32
continue
delete
bp 0x31a0dc                      # ThrowBadTypeWithFrameData
continue
frame 1
call ((void(*)(void*,unsigned))dump_chain_at)(ctx, faulting_pc)
call ((void(*)(unsigned,unsigned))diag_dump_refvar)((unsigned)ctx->x[1], 24)
detach
```

### Parked newt task ("the REPL / UI went dead")

Symptom: `scripts/newton-repl.py` gets no replies, the UI ignores
taps, the guest idles in `pause_system`. `task_dump` shows the newt
task as current with `[pc=MonitorDispatchSWI+4 lr=LockHeapRange+0x34]`
and nothing runnable — it is waiting on the STKU monitor for a heap
range lock that never completes. Known trigger: an exception escaping
`SuckPackageFromBinary` (a rejected package) leaks the source
binary's `TObjectPtr` lock; the next heap growth (`MakeBinary` of a
few KiB) parks the task. Plain NS exceptions and successful sucks do
not trigger it. Only a reboot recovers. Note the monitor tasks'
(`PTBL`, `STKF`, `STKU`) save-area chains look identical in a healthy
census (stale idle signatures) — only the newt task's state is
evidence.

### Package install without the Dock

`scripts/pkg-repl-install.py MyApp.pkg` (test packages:
`tools/test-packages/`, `./build.sh` there) uploads a package through the
REPL (`MakeBinary` + `StuffByte` chunks, byte-sum verified) and calls
`GetDefaultStore():SuckPackageFromBinary(pkg, nil)` — the same call the
ROM's restore path makes — reporting the package count before/after
and any exception (name + errorCode, via `platform.Log`). It isolates
the store/pager/RegisterNewPackage half of an install from the
serial/MNP/Dock half. For the real path, build UnixNPI from
github.com/chuma/unixnpi (`command git clone`, `make`) and follow
README "External serial port"; the Dock's Connect button gives up
after ~20 s, so start UnixNPI first. REP quirks it works around: the REP's parser
does not know the `nil` keyword (`{}.x` yields nil), a `for` loop
counter is undefined inside a REP-compiled function (use `foreach`
or `while`), a top-level `x := ...` inside `begin/end` is local (use
`DefGlobalVar`/`DefGlobalFn`), and `Print` output lands in the
kernel log as `platform.Log`, not on the REP echo line.

## Headless boot verification

```bash
scripts/boot-check.sh --cold              # forces a cold boot first
scripts/boot-check.sh --marker 'some other log line'
```

It redirects the QEMU log, polls for the Welcome-UI markers, and
SIGKILLs QEMU the moment they appear (exit 0), or exits 1 on
`--timeout` with the log tail. Use it instead of hand-rolled
`timeout N cargo run … ; pkill` recipes — QEMU defers SIGTERM while the
guest is busy ([`QEMU_BUGS.md`](QEMU_BUGS.md)).

## Regression coverage before committing

`guest-tests/tests/` exercises each handler in isolation; a regression
in handler code shows up as a failing test. Run
`guest-tests/scripts/run-all.sh` before committing.

`scripts/check-matrix.sh` runs the two structure lints
(`check-layering.sh` import discipline, `check-rom-addrs.sh` ROM-address
containment) and then `cargo check`s all 19 supported build
combinations in one shared target dir (~10 s warm). Run it after
touching `build.rs`, feature gates, or any cfg-dispatched backend; it is
also available as `CHECK_MATRIX=1 guest-tests/scripts/run-all.sh`.
Forbidden `--features` sets fail with build.rs's named
platform-mutual-exclusion message rather than a deep compile error,
because the cross-axis constraints are expressed as Cargo feature
dependencies (hardware backends imply `platform-raspi3b`; `sd-probe`
implies `no-semihost`).

**Probe-only iterations may skip the guest-test run.** A Newton-ROM
probe is a new HVC immediate at a Newton-ROM PC in
`src/newton/rom_patches.rs`, a dispatch arm in `src/hv/trap/hvc.rs`,
and a handler body in `src/newton/probes.rs` that emulates the original
instruction. The guest tests run isolated test ELFs that don't include
the Newton ROM, so probe-only changes can't regress them. Run the
tests when changes touch `src/newton/inline_patch.rs`,
`src/newton/unaligned.rs`, `src/peripherals/*`, `src/arch/banked.rs`,
`src/hv/stage2.rs`, `src/hv/guest.rs`, the generic UND/DABT/IRQ
paths in `src/hv/trap/` (`mod.rs`, `dabt.rs`, `und.rs`, `cp15.rs`), or
`guest-tests/` itself.
