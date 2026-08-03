# Newton Hypervisor — baremetal

Pure-Rust Type-1 hypervisor that runs an unmodified Apple Newton OS 2.x
ROM natively on a Cortex-A53 guest. Architecture and rationale live in
[`HIGHLEVEL.md`](HIGHLEVEL.md); language, build-system and testing
decisions in [`IMPLEMENTATION.md`](IMPLEMENTATION.md); current state
and remaining work in [`PLAN.md`](PLAN.md); the Newton peripheral spec
(with Einstein cross-references) in
[`docs/peripherals.md`](docs/peripherals.md).

## Status

**The 717006 ROM boots to the Welcome UI and the builtin apps work —
on QEMU, on FVP, and on a real Pi Zero 2 W.**

- The hypervisor boots, drops to AArch32 EL1 at the ROM reset vector,
  and the Newton kernel reaches steady-state interactive operation:
  ~27 tasks running, the NewtonScript interpreter executing bytecode,
  pen input driving the UI.
- On QEMU/FVP the live host viewer (`tools/host-viewer/`, built with
  `--features host-io-semihost`) renders the display and injects
  mouse clicks as pen taps.
- On real hardware (Pi Zero 2 W, `pi-bare-metal-input` aggregate
  feature) the full stack runs natively: HDMI display (`host-io-pi-fb`),
  USB touchscreen input (`input-mtouch`), HDMI audio (`audio-pi-hdmi`),
  and flash persistence to SD card (`flash-persist-sd`) with
  non-blocking DMA autosave. See
  [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md).
- 38 guest tests exercise the handler surface in isolation; all green
  on both QEMU and FVP. `scripts/check-matrix.sh` keeps all 18
  supported build combinations compiling.

Known gaps, in [`PLAN.md`](PLAN.md): **add-on app packages** (the
`.pkg` installation flow, which can carry native code), **snapshot
resume** (saves work, resuming the ROM wedges), the guest **serial
port** and **PCMCIA card images** on real hardware, and **ROM versions
other than 717006**.

What's working end-to-end:

- **Three hosts.** `platform-raspi3b` (default) under QEMU `raspi3b` —
  fast iteration, BCM2835 VIC, AArch64↔AArch32 banking quirks
  documented in [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md).
  `platform-fvp-base` under `FVP_Base_RevC-2xAEMvA` — accurate
  reference: GICv3 (brought up through an EL3 stub), exact
  generic-timer + cache model. The same `platform-raspi3b` image runs
  on a real Pi Zero 2 W. Both emulated hosts must stay green on every
  commit.
- **EL2 stage-1 MMU and stage-2 trap dispatch.** Decoded `ESR_EL2.EC`
  handlers for data/instruction aborts, HVCs, trapped CP15
  (`TVM`/`TRVM`/`TIDCP`), and undefined instructions. The CP15 shim
  covers every tuple Newton 717006 issues; the StrongARM lax encoding
  (`MCR p15,0,Rn,cN,cN,0`) is rewritten to the ARMv7 `CRm=0` form at
  ROM load.
- **Newton peripheral surface.** Modules under `src/peripherals/`
  (ASIC, battery, DMA, flash, host-call, native-primitives, network,
  PCMCIA, platform, printer, screen, serial, sound, tablet, VIC,
  in/out translators) port Einstein's C++ state machines into Rust.
  See [`docs/peripherals.md`](docs/peripherals.md).
- **BE-8 mode + selective ROM byteswap.** The guest runs with
  `CPSR.E=1` and `SCTLR_EL1.EE=1`. `load_newton_rom` in
  `src/newton/loader.rs` consults the classifier `reach.bitmap` per
  word: code → byteswap to LE on load (so AArch32 fetch works);
  data → leave BE-natural (so a `CPSR.E=1` `LDR` returns the kernel's
  intended numerical value). `src/hv/guest_endian.rs` is the EL2-side
  bottleneck for reads/writes of guest data.
- **Stage-1 normalisation.** The kernel uses ARMv4 subpage-AP
  semantics that ARMv8 doesn't support. `fix_stage1_xn_bits` in
  `src/newton/os.rs`, run on every guest TTBR0 install, edits the
  guest's own L1/L2 descriptors in place: it flattens subpage-AP to
  AP=011, clears the XN bits ARMv7 reinterprets from ARMv4 SBZ, and
  rewrites the ROM's fine-table L1 placeholders to fault. No parallel
  shadow page-table tree.
- **SA-1100 unaligned-LDR semantics.** `SCTLR_EL1.A` is forced on, and
  every alignment fault is emulated with rotate-LDR semantics
  (`src/newton/unaligned.rs`). Hot sites get a per-PC in-ROM stub
  installed on first fault (`src/newton/unaligned_inline.rs`) so they
  stop trapping.
- **Async timer delivery.** CNTHP rearms on every match-reg write;
  the EL2 physical timer raises its IRQ, the platform layer routes it
  to core 0, and `trap_irq` latches and sets `HCR_EL2.VI`. WFI wakes
  on real wall time.
- **Function-level execution tracer.** `--features trace,quiet`
  patches every entry in the curated `code-symbols.txt` with an
  HVC trampoline and logs `seq PC name (mode) r0..r3` on every
  call. Variant `trace_once` gates the line on a per-function
  fired-bitmap if you want first-touch only.
- **Live display + pen input.** Each `screen::blit` is forwarded
  through `src/host/host_io/` to a companion viewer at
  `tools/host-viewer/`, which opens a window via softbuffer + winit,
  applies the blit stream to its own 320×480 2 bpp backing store, and
  posts mouse events back as Newton pen samples. Selected with
  `--features host-io-semihost`; the default `host-io-null` backend
  is a no-op (used by guest-tests and CI).
- **Guest-test tier** under `guest-tests/` — 38 small AArch32
  binaries against a shared runtime, with an HVC protocol for
  pass/fail/print. Cover every handler (CP15, VIC, DMA, flash,
  serial, screen blit, native primitives, tablet, PCMCIA, snapshot
  round-trip, banked-reg paths, `LDR` rotate, SWP, …).
- **Diagnostic scaffolding.** DABT/PABT DIAG vectors at ROM offsets
  `0x10` / `0x0C`, BootOS / PowerOff / Reboot canaries (semihost/dev
  builds only, gated on `cfg(nh_loud_halt_canaries)`), trap
  histograms, and kernel struct dumps (`src/diag/task_dump.rs` walks
  `TScheduler` / `TTask`).

## Prerequisites

- `rustc` via `rustup`. Pinned toolchain and target come from
  `rust-toolchain.toml` (target `aarch64-unknown-none-softfloat`).
- `qemu-system-aarch64` with `raspi3b` support.
- For FVP runs: OrbStack (or Docker) to host the
  `armswdev/aemfvp-cca-v2-image` container. `scripts/fvp` wraps
  the dockerised `FVP_Base_RevC-2xAEMvA`.
- `arm-none-eabi-gcc` + `arm-none-eabi-objcopy` to cross-compile the
  guest-test images.
- Cross-aarch64 gdb for source-level debugging: `gdb-multiarch` on
  Linux, `aarch64-elf-gdb` (Homebrew) on macOS.
- A Newton 2.x ROM at `roms/newton.rom` (8 MiB, byteswapped on load;
  gitignored — **never commit the ROM**).

`apt install qemu-system-arm gcc-arm-none-eabi gdb-multiarch` plus
`rustup target add aarch64-unknown-none-softfloat` covers a fresh box
for the QEMU path.

## Build and run

QEMU `raspi3b` (default):

```
cargo build --release          # just build
cargo run --release            # build + boot the Newton ROM in QEMU
```

`cargo run` invokes [`scripts/run-qemu.sh`](scripts/run-qemu.sh): the
ELF is `objcopy`'d to `kernel8.img` and QEMU is launched with PL011 on
stdio, `-no-reboot`, and semihosting enabled (the hypervisor uses
`HLT #0xF000` for host file access).

ARM FVP `FVP_Base_RevC-2xAEMvA` (accurate reference model):

```
cargo build --release --no-default-features \
    --features "platform-fvp-base rom-717006 quiet diag"
scripts/fvp --timeout=90 \
    target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

FVP runs the timer and cache model accurately, so wall-clock is much
slower than QEMU TCG — use longer timeouts. Add `--gdb` for an Iris
debug server on host port 7100; add `--features trace` for the
function-level tracer. See the comments at the top of `scripts/fvp`.

### Live display + pen input

By default the hypervisor builds with the `host-io-null` backend:
blits are computed but not forwarded, and pen input is always
"no sample". To get a real window with mouse-driven pen events,
build the hypervisor with `host-io-semihost` and run the companion
viewer in `tools/host-viewer/` in a second terminal.

**One-time setup** — the viewer pulls `softbuffer` + `winit` from
crates.io, so you need network on first build:

```
( cd tools/host-viewer && cargo build --release )
```

**Each session**, two terminals:

```
# term 1 — hypervisor with the semihost backend.
cd /path/to/baremetal
rm -f /tmp/newton-snapshot-*.bin
cargo run --release --no-default-features \
    --features 'platform-raspi3b rom-717006 diag host-io-semihost'

# term 2 — companion viewer. Start it after term 1 prints
#   "host_io: outbound /tmp/newton-host-io/out fh=…"
#   "host_io: inbound  /tmp/newton-host-io/in  fh=…"
# (those mean the IPC files exist).
cd /path/to/baremetal/tools/host-viewer
cargo run --release
```

A 640×960 window appears (the panel is 320×480, scaled 2×). The
Newton boot UI paints incrementally as blits arrive. Left-click to
tap the panel; drag to drag. Mouse position is mapped 1:1 into the
panel coord space, so taps land where you expect. Press `P` to send
a power-switch press — the only way to wake the guest after it has
slept into PowerOff, since Newton OS masks the tablet IRQ in that
state (matches real hardware; equivalent to Einstein's
`SendPowerSwitchEvent`).

**IPC details, if you need to debug.** The hypervisor opens these
two files via Arm semihosting on init:

- `/tmp/newton-host-io/out` — hypervisor → viewer. 24-byte
  `BlitEvent` headers followed by 2 bpp packed payloads (MSB-first,
  4 px/byte; 0 = white, 3 = black). One stream of variable-size
  records.
- `/tmp/newton-host-io/in`  — viewer → hypervisor. 8-byte
  `PenEvent` records (`kind` byte + le16 `x`, `y`, `pressure`;
  `kind` = 1 down / 2 move / 3 up / 4 power-switch). Hypervisor seeks
  to its last read position every 16 ms and drains new bytes.

The hypervisor creates the directory and `touch`es both files on
boot, so first-time startup needs no manual prep. Restarting the
hypervisor truncates `out`; the viewer detects "file shrunk" and
resets its read position. Restarting just the viewer truncates `in`,
which the hypervisor handles the same way.

**FVP path.** The dockerised FVP wraps `/tmp` into the container, so
the same paths work — start the FVP run with `scripts/fvp` from
term 1, viewer from term 2 on the host. Path resolution goes
through semihosting in both cases.

### Snapshots

`/tmp/newton-snapshot-{0..3}.bin` holds a rolling ring of four
guest-state snapshots, autosaved every 2 s of wall time from the
timer-IRQ hook (and on demand from the guest via `HVC #0x18`). Each
save carries guest-visible state only — RAM, framebuffer, the
inline-stub scratch pool, the EL1 CP15 registers reachable from EL2,
and all 31 AArch64 GPRs (which alias every AArch32 banked R0..R14 per
ARM ARM Table D1-79) — plus ROM and flash fingerprints so a mismatched
binary or a diverged flash image is rejected.

**Resuming a Newton-ROM snapshot is currently broken**: the guest
ERETs to the saved PC and immediately wedges in a prefetch-abort loop
at the vector page. Cold-boot for any run whose result you intend to
trust:

```
rm -f /tmp/newton-snapshot-*.bin && cargo run --release
```

Fixing or removing the resume path is tracked in
[`PLAN.md`](PLAN.md); what a save does and does not restore is
specified in
[`docs/SNAPSHOT_RESUME_CONTRACT.md`](docs/SNAPSHOT_RESUME_CONTRACT.md).

## Cargo features

Features come in independent axes — a platform, a ROM version, plus one
backend per I/O seam. `build.rs` validates the combination at build
time and falls back to the `null` backend for any axis left
unspecified.

| Axis / feature         | Default | Purpose                                                                              |
|------------------------|---------|--------------------------------------------------------------------------------------|
| `platform-raspi3b`     | yes     | QEMU raspi3b host (and real Pi Zero 2 W). BCM2835 VIC, PL011 at 0x3F201000.          |
| `platform-fvp-base`    | no      | FVP `FVP_Base_RevC-2xAEMvA` host. GICv3 brought up through an EL3 stub.              |
| `rom-{717006,710031}`  | 717006  | Guest-ROM version: selects the `src/newton/rom_ver/` constants module + build inputs. Exactly one required. |
| `host-io-{null,semihost,pi-fb}` | null | Display + pen seam: no-op, semihost viewer IPC, or real VC4 framebuffer.      |
| `flash-persist-{null,semihost,sd}` | semihost | Flash persistence: volatile, `$HOME/.newton/flash.bin` via semihosting, or FAT32 SD card. |
| `input-{null,mtouch}`  | null    | Pen-input seam: no-op or TSTP MTouch USB touchscreen (real hw).                      |
| `audio-{null,pi-hdmi}` | null    | Sound seam: null (no output, but arms timer-paced DMA-completion IRQs) or VC4 HDMI MAI audio (real hw). |
| `no-semihost`          | no      | No semihosting host is listening: no `HLT #0xF000` calls anywhere. Negative because Cargo features only add; `build.rs` inverts it to `cfg(nh_semihost)` for source to read. |
| `trace`                | no      | Function-level execution trace via per-entry HVC trampolines.                        |
| `trace_once`           | no      | First-touch variant of `trace`. Trampolines still fire; only the SEQ line is gated.  |
| `quiet`                | no      | Silence recurring diag log lines (`fix_stage1_xn_bits:` summaries, etc.).            |
| `diag`                 | yes     | Diagnostics layer (`src/diag/`): trap histograms, task/heap dumps, guest BPs, the ~743 KiB symbol tables. Off swaps in no-op stubs with the same surface. |
| `log_*`                | partial | Per-subsystem diagnostic logging. Default carries the low-volume tier (`log_traps`, `log_irqs`, `log_host_io`); the investigation tiers (`log_mmu`, `log_unaligned`, `log_tasks`, `log_store`) are opt-in. |
| `ns_trace`             | no      | Open the kernel's TInterpreter trace gates (NS-level DoSend/DoCall logging).         |
| `sd-probe`, `fb-probe` | no      | Standalone real-hw bring-up probes (boot, test one peripheral, halt).                |

Aggregates for real hardware (`pi-bare-metal`, `pi-bare-metal-sd`,
`pi-bare-metal-display`, `pi-bare-metal-input`) roll up
`platform-raspi3b + rom-717006 + no-semihost` plus progressively more
backends; `pi-bare-metal-input` is the full stack (SD flash, HDMI
display + audio, USB touch). The authoritative list is `Cargo.toml`.

Common combinations:

```
cargo run --release                                    # default: QEMU, full diag logs
cargo run --release --features quiet                   # QEMU, no diag noise
cargo run --release --features trace,quiet             # QEMU, clean function trace
cargo build --release --no-default-features \
    --features "platform-fvp-base rom-717006 quiet diag"   # FVP build (then scripts/fvp)
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh /Volumes/PIBOOT
                                                       # bootable SD for the Pi Zero 2 W
```

`trace`, `log_store` and `ns_trace` mutate ROM words, which changes the
snapshot ROM fingerprint — clear `/tmp/newton-snapshot-*.bin` when
toggling them.

## Function-level execution trace

Sample (abridged):

```
trace     1 0x000188f8 FlushTheCache (svc) r0=0x... r1=0x... r2=0x... r3=0x...
trace     2 0x00045b78 HandleDebugCard (svc) r0=0x... r1=0x... r2=0x... r3=0x...
trace     3 0x0011efb4 InitSpecialStacks (svc) r0=0x... r1=0x... r2=0x... r3=0x...
...
```

Every call — not first-touch — so a function called ten times produces
ten trace lines. The address list comes from
`scripts/classify-out/code-symbols.txt`, the curated code-only set
produced by `scripts/classify-symbols.py`. The mechanism (a 5-word
in-ROM trampoline per function, original first instruction copied or
rewritten in slot[1], branch-back at slot[4]) is in
`src/diag/tracer.rs`. To diff a hypervisor trace against an Einstein
trace of the same boot, use `scripts/trace-diff.sh`.

Every call fires an HVC, so a long boot can saturate the console UART —
pair `trace` with `quiet` and grep. Entries whose first word is a
PC-relative form the rewriter can't handle are counted in the
`rewrite-skip` column at install time and left untraced; the function
still runs correctly.

## Guest-test tier

An ARM-guest test framework lives in [`guest-tests/`](guest-tests/) —
each test is a small AArch32 binary linked to a shared runtime
(`common/test_runtime.S`) that sets up SVC / IRQ / FIQ stacks, installs
an IRQ handler, and exposes an HVC protocol the hypervisor understands
(`HVC #0x10` = putchar, `HVC #0x12` = PASS, `HVC #0x13` = FAIL,
`HVC #0x14` = mark/progress; full ABI in `common/hvc_abi.S`). The
hypervisor is built with `NH_GUEST_TEST` set in the environment so
guest memory is populated with the test instead of the ROM.

```
guest-tests/scripts/build-tests.sh                # build everything in MANIFEST
guest-tests/scripts/run-test.sh test_vic          # build + run one test
guest-tests/scripts/run-all.sh                    # run all 38 tests on QEMU
guest-tests/scripts/run-all.sh --platform fvp     # run all 38 tests on FVP
CHECK_MATRIX=1 guest-tests/scripts/run-all.sh     # also run the 18-combo build matrix
```

Add a new test by dropping `tests/<name>.S` in place, appending the
name to `tests/MANIFEST`, and rerunning. See `guest-tests/README.md`
for the full HVC protocol.

**Every commit must pass `guest-tests/scripts/run-all.sh`.** All 38
tests must stay green. (Probe-only iterations that touch nothing
outside `src/newton/rom_patches.rs`, `src/hv/trap/hvc.rs`, and
`src/newton/probes.rs` can skip the run — see the note in
[`docs/DEBUGGING.md`](docs/DEBUGGING.md).)

## Debug with gdb

QEMU side:

```
DEBUG=1 cargo run --release             # QEMU paused with gdb stub on :1234

# Term 2:
aarch64-elf-gdb -x scripts/gdb-init \
  target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

FVP side: `scripts/fvp --gdb <elf>` exposes an Iris debug server on host
port 7100.

`scripts/gdb-init` connects, sets sane defaults, and defines helpers
(`guest-state`, `bg`, `bp`, `bp-clear`, `bp-list`, `tt`).

### EL2 hypervisor (AArch64) breakpoints — fully work

DWARF is on in both `dev` and `release`. Source breakpoints,
backtraces, `info locals`, `stepi`, `next` all work against the
hypervisor:

```
(gdb) break kmain
(gdb) break trap_sync_lower_aarch32
(gdb) break src/hv/trap/mod.rs:103
(gdb) continue
```

### EL1 guest (AArch32) breakpoints — work via two helpers

`qemu-system-aarch64`'s gdbstub is aarch64-only and drops the AArch32
mode switch (see [qemu-arm 2020-07](https://lists.gnu.org/archive/html/qemu-arm/2020-07/msg00122.html)).
The hypervisor side fills the gap:

- **`bg <addr>`** — gdb-side conditional break at
  `trap_sync_lower_aarch32` when `$ELR_EL2 == addr`. Fires only on
  naturally-trapping guest instructions (data/insn abort, SVC/HVC,
  trapped CP15). Does not catch UND-class traps because the UND
  trampoline HVCs into EL2 first.
- **`bp <addr>`** — installs a one-shot guest software breakpoint
  (see `src/diag/guest_bp.rs`). Patches the ROM word with `UDF #0xFF0E`
  and stops in `handle_user_bp_und` with `faulting_pc` set to the
  guest PC. Works for any ROM-range PC. Snapshot autosaves are
  gated while any BP is live, so a debug session never corrupts a
  persisted snapshot.

Typical recipe is in [`docs/DEBUGGING.md`](docs/DEBUGGING.md).

## Reference docs

Consult these before re-deriving state from disassembly:

- [`docs/DEBUGGING.md`](docs/DEBUGGING.md) — wedge triage (bitmap-first),
  gdb and guest-breakpoint recipes, what to run before committing.
- [`docs/DISASM.md`](docs/DISASM.md) — `scripts/disasm-out/rom.dis`,
  the symbol-annotated ROM+REx disassembly.
- [`docs/NEWTON_INTERNALS.md`](docs/NEWTON_INTERNALS.md) — APCS calling
  convention, two-level object dispatch, ROM jump-table
  (`0x01A00000..0x01C20000`) as the post-ship patch mechanism, DDK
  header locations.
- [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md) — raspi3b AArch64↔AArch32
  bug catalog. **Especially the banked-register entries** — the
  apparent "flaky `ctx.x[13]` / `ctx.x[14]`" has been misdiagnosed as
  a QEMU bug multiple times; it is architected behaviour per ARM ARM
  Table D1-79.
- [`docs/STRUCTURES.md`](docs/STRUCTURES.md) — Newton kernel data
  structure layouts (TScheduler, TTask, TObjectTable, kernel ID
  encoding, observed task census).
- [`docs/peripherals.md`](docs/peripherals.md) — peripheral
  implementations (Newton-side spec + Einstein cross-references).
- [`docs/ENDIAN_FIXES.md`](docs/ENDIAN_FIXES.md) — BE-32 word-invariant
  conventions and the audit of every B-bit-visible behaviour.
- [`docs/PACKAGE_NATIVE_CODE.md`](docs/PACKAGE_NATIVE_CODE.md) —
  design note for native code inside add-on packages.
- [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md) — Pi Zero 2 W
  firmware contracts and the as-built SD / display / USB / audio
  stacks.
- [`docs/WORKFLOW.md`](docs/WORKFLOW.md) — working-style notes
  (assembler round-tripping, Einstein-port review, test-per-feature).
- [`probe/FINDINGS.md`](probe/FINDINGS.md) — golden record from a
  fully-booted Newton via the instrumented Einstein probe.

## Layout

```
baremetal/
  Cargo.toml             crate manifest (no_std, panic=abort)
  rust-toolchain.toml    pinned toolchain + target
  build.rs               platform/ROM-version resolution, feature-matrix
                         validation, linker-script templating, symbol-blob
                         + bitmap staging
  .cargo/config.toml     build target, rustflags, cargo-run runner
  linker.ld.in           image-layout template; build.rs substitutes the
                         per-platform load address (raspi3b 0x80000,
                         FVP 0x80000000) into OUT_DIR/linker.ld
  scripts/
    run-qemu.sh          cargo runner for QEMU
    boot-check.sh        marker-based QEMU boot verifier (kills QEMU
                         once the boot milestone appears in the log)
    fvp                  cargo-runner-equivalent for FVP (dockerised)
    gdb-init             gdb helpers (bg, bp, tt, guest-state, …)
    check-matrix.sh      cargo-check every supported feature combo
                         (runs the two lints below first)
    check-layering.sh    import-discipline lint for the src/ layers
    check-rom-addrs.sh   ROM-address containment lint (hex literals
                         belong in src/newton/rom_ver/)
    check-doc-symbols.py code-reference lint (every code path cited in
                         the docs *and in source comments* resolves,
                         and to the module named)
    classify-symbols.py  ROM symbol partitioner (code/data/drop)
    regen-classify.sh    regenerate code-symbols.txt + reach.bitmap
                         for a ROM version (default 717006)
    dump-data-regions.py refresh code-regions.txt for bitmap triage
    build-rom-disasm.sh  regenerate the annotated ROM disassembly
    trace-diff.sh        diff Einstein vs hypervisor function traces
    build-sd.sh          assemble a bootable Pi SD card
    classify-out/        curated symbol lists
    disasm-out/          rom.dis + indices
  src/
    main.rs              kmain: boot narrative — MMU, backings, ROM load,
                         stage-2, vectors, peripherals, timer; ERET to guest
    panic.rs             panic handler → loud halt
    arch/                pure AArch64/AArch32 mechanism, no upward deps:
                         boot.s (_start), vectors.s (EL2 exception
                         vectors + context save/restore), trap_context.rs
                         (TrapContext + read_sysreg!), mmu.rs, cpu.rs,
                         banked.rs (AArch32 banked regs from EL2),
                         arm_decode.rs, aarch32_emit.rs (branch/literal
                         encoders + install_patch), slim_isr.rs (IrqCap)
    hv/                  generic hypervisor core: stage2.rs (stage-2
                         tables + the RW+XN ↔ RO+X page state machine),
                         guest.rs (ERET to AArch32 EL1), guest_mem.rs,
                         guest_endian.rs (BE-8 accessors), be8.rs (lane
                         math), layout.rs (single manifest of guest
                         regions + MMIO windows + hyp-code ranges),
                         mmio.rs (router into peripherals), timer.rs
                         (CNTHP), snapshot.rs (4-slot ring), hvc_imm.rs
                         (HVC tag table), hooks.rs (GuestOs trait — the
                         hv→newton seam), trap/ (mod.rs dispatch +
                         trap_irq, dabt.rs, und.rs, cp15.rs, hvc.rs)
    newton/              Newton-OS-specific logic: os.rs (GuestOs hook
                         impls, incl. fix_stage1_xn_bits + the MMU-
                         enable ritual), loader.rs (ROM load + selective
                         byteswap + CP15 rewrite), rom_patches.rs,
                         probes.rs (probe handler bodies),
                         inline_patch.rs (in-ROM stub + scratch pools,
                         APCS liveness walker), guest_trampolines.rs
                         (UND/DABT vector trampolines), unaligned.rs +
                         unaligned_inline.rs (rotate-LDR emulation and
                         its per-PC stubs), rom_ver/ (per-ROM-version
                         constants: r717006 full, r710031 skeleton)
    peripherals/         guest device models: asic, battery, console
                         (guest-serial ↔ host-console seam), dma,
                         flash + flash_driver, guest_access, host_call,
                         in/out_translator, native_primitives, network,
                         pcmcia, platform, printer, screen,
                         serial + serial_driver, sound, tablet, vic
    host/                host drivers + backends: console.rs (PL011 /
                         semihost console) + macros.rs (kprintln!,
                         log_*!), platform/ (raspi3b, fvp_base, gicv3),
                         mailbox.rs, host_dma.rs (BCM2835 DMA), sd/,
                         usb/ (DWC2 + HID), display/, audio/
                         (null / pi_hdmi), input/ (null / mtouch),
                         host_io/ (null / semihost / pi_fb),
                         flash_persist/ (null / semihost / sd)
    diag/                diagnostics layer (feature `diag`, on by
                         default; no-op stubs when off): trap_diag.rs,
                         trap_hist.rs, task_dump.rs, heap_check.rs,
                         rep_print.rs, symbols.rs (PC→name tables),
                         guest_bp.rs (gdb 'bp <addr>'), tracer.rs
                         (--features trace), tarmac.rs, diag_util.rs
  guest-tests/
    common/              shared runtime, linker script, HVC macros/ABI
    tests/               38 .S files + MANIFEST
    scripts/             build-tests.sh, run-test.sh, run-all.sh
    README.md            HVC protocol, how to add a test
  probe/                 instrumented-Einstein oracle build
  docs/                  reference docs (see above)
  classify/              cached classifier output (reach.bitmap per ROM hash)
  roms/                  ROM images (gitignored) + per-version input dirs
  tools/                 classify-rom, host-viewer, romdump
  newton-objects/        in-tree crate: NS Ref tag decoding for diag
  vendor/                vendored embedded-sdmmc 0.9.0 (path dep,
                         local changes listed in VENDOR.md)
  boot-pi/               Pi firmware config.txt for the SD boot partition
  assets/                boot splash image
  PLAN.md                current state + remaining work
  HIGHLEVEL.md           architecture
  IMPLEMENTATION.md      language, build, structure, testing
  CLAUDE.md              hypervisor notes (auto-loaded by Claude Code)
```

## Layering

`src/` is one crate in six layer directories, dependency direction
enforced by `scripts/check-layering.sh` (its header comment is the
authoritative statement of the rules and the sanctioned exceptions):

- **arch** ← **hv** ← **newton** is the upward import direction;
  `main.rs` / `panic.rs` wire everything and may import all layers.
- **peripherals** (guest device models) is reached from hv only through
  the `mmio.rs` router's closed `PeriphId` enum; **newton** may use it
  freely.
- **hv → newton** crosses only at `src/hv/hooks.rs`: the `GuestOs`
  trait with `type ActiveGuest = newton::NewtonOs` — guest-OS behavior
  (SCTLR massaging, MMU-enable ritual, probe HVCs, IRQ-tail pumps)
  plugs in there rather than being called from generic trap code.
- **host** (drivers/backends) sits below `main.rs` and is not imported
  by guest-facing layers; backends are selected per axis by `build.rs`
  cfgs (`nh_host_io_*`, `nh_flash_persist_*`, `nh_input_*`,
  `nh_audio_*`). Sanctioned upward edges: event injection into
  `peripherals::vic`/`queue`, and reads through `hv::guest_mem` /
  `guest_endian`. `host/platform` is the board API, importable from
  any layer. Other seams: `peripherals/console.rs` carries guest-serial
  bytes to the host console via installed fn pointers.
- **diag** is importable from anywhere; with the `diag` feature off it
  compiles to no-op stubs with the identical surface.

Three structure lints keep this true: `scripts/check-layering.sh`
(import discipline), `scripts/check-rom-addrs.sh` (ROM addresses live
in `src/newton/rom_ver/`), and `scripts/check-matrix.sh` (every
supported feature combination builds; runs the other two first).

## Running on real hardware

The deployment target is the **Pi Zero 2 W** (not the Pi 3B — same
SoC, same image; only the form factor differs), and the full stack is
hardware-validated: EL2 handoff, ROM boot, HDMI display, USB
touchscreen, HDMI audio, and SD-card flash persistence with
non-blocking DMA autosave.

Build a bootable SD card with:

```
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest> [sd-mount]
```

See [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md) for the
hardware specifics — `config.txt`, UART routing, the TSTP MTouch
panel, SDHOST details. The guest serial port and PCMCIA card images
are the remaining unported peripherals.

## Cheatsheet

```
cargo build --release                         # just build (raspi3b)
cargo run --release                           # build + boot Newton ROM in QEMU
cargo run --release --features trace,quiet    # boot with function-level trace, quiet diag
DEBUG=1 cargo run --release                   # same, paused with gdb stub on :1234
guest-tests/scripts/run-all.sh                # run every guest test on QEMU
guest-tests/scripts/run-all.sh --platform fvp # run every guest test on FVP
guest-tests/scripts/run-test.sh test_vic      # one test, verbose output in /tmp
scripts/check-matrix.sh                       # every supported build combo + lints
scripts/boot-check.sh --cold                  # cold boot, verify the Welcome UI markers

# FVP boot
cargo build --release --no-default-features --features "platform-fvp-base rom-717006 quiet diag"
scripts/fvp --timeout=90 \
    target/aarch64-unknown-none-softfloat/release/newton-hypervisor

# Force cold boot of a QEMU run
rm -f /tmp/newton-snapshot-*.bin && cargo run --release

# Probe real Einstein against the 717006 ROM for oracle behaviour
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 90

# Inspect ROM bytes properly — don't hex-decode by hand
less scripts/disasm-out/rom.dis
```
