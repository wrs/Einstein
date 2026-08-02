# Newton Hypervisor — baremetal

Pure-Rust Type-1 hypervisor that runs an unmodified Apple Newton OS 2.x
ROM natively on a Cortex-A53 guest. Architecture and rationale live in
[`HIGHLEVEL.md`](HIGHLEVEL.md); the implementation plan in
[`IMPLEMENTATION.md`](IMPLEMENTATION.md); the iteration log and current
goal in [`PLAN.md`](PLAN.md); the Newton peripheral spec (with Einstein
cross-references) in [`docs/peripherals.md`](docs/peripherals.md).

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
- 37 guest tests exercise the handler surface in isolation; all green
  on both QEMU and FVP.

The known functional gap is **add-on app packages** — the ROM and
builtin apps run, but Newton's `.pkg` installation flow (which can
carry native code) is not yet supported.

What's working end-to-end:

- **Two host platforms.** `platform-raspi3b` (default) under QEMU
  `raspi3b` — fast iteration, BCM2835 VIC, AArch64↔AArch32 banking
  quirks documented in [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md).
  `platform-fvp-base` under `FVP_Base_RevC-2xAEMvA` — accurate
  reference: GICv3 (the hypervisor brings it up through an EL3 stub),
  exact generic-timer + cache model. Both must stay green on every
  commit.
- **EL2 stage-1 MMU and stage-2 trap dispatch.** Decoded `ESR_EL2.EC`
  handlers for data/instruction aborts, HVCs, trapped CP15
  (`TVM`/`TRVM`/`TIDCP`), and undefined instructions. CP15 shim
  covers every tuple Newton 717006 issues; the StrongARM lax encoding
  (`MCR p15,0,Rn,cN,cN,0`) is rewritten to the ARMv7 `CRm=0` form at
  ROM load.
- **Newton peripheral surface.** Modules under `src/peripherals/`
  (battery, DMA, flash, host-call, native-primitives, network,
  PCMCIA, platform, printer, screen, serial, sound, tablet, VIC,
  in/out translators) port Einstein's C++ state machines into Rust.
  See [`docs/peripherals.md`](docs/peripherals.md).
- **BE-8 mode + selective ROM byteswap.** The guest runs with
  `CPSR.E=1` and `SCTLR_EL1.EE=1`. `src/guest_mem.rs::load_newton_rom`
  consults the classifier `reach.bitmap` per word: code → byteswap
  to LE on load (so AArch32 fetch works); data → leave BE-natural
  (so a CPSR.E=1 LDR returns the kernel's intended numerical
  value). `src/guest_endian.rs` is the EL2-side bottleneck for
  reads/writes of guest data.
- **Stage-1 normalisation.** The kernel uses ARMv4 subpage-AP
  semantics that ARMv8 doesn't natively support.
  `fix_stage1_xn_bits` in `src/guest_mem.rs`, run on every guest
  TTBR0 install, edits the guest's own L1/L2 descriptors in place:
  it flattens subpage-AP to AP=011, clears the XN bits ARMv7
  reinterprets from ARMv4 SBZ, and rewrites the ROM's fine-table L1
  placeholders to fault. No parallel shadow page-table tree.
- **Async timer delivery.** CNTHP rearms on every match-reg write;
  the EL2 physical timer raises CNTHPIRQ, the BCM2836 local
  peripheral routes it to core 0, and `trap_irq` latches and sets
  `HCR_EL2.VI`. WFI wakes on real wall time.
- **Snapshot ring** at `/tmp/newton-snapshot-{0..3}.bin`, autosaved
  every 2 s of wall time from the timer-IRQ hook. Saves guest-visible
  state (RAM, FB, SCRATCH_POOL, EL1 CP15 regs, all 31 AArch64 GPRs —
  which alias every AArch32 banked R0..R14) with ROM + flash
  fingerprints so resume rejects mismatched binaries or diverged
  flash. `HVC #0x18` from the guest forces an immediate save. Lets
  you edit hypervisor code, `cargo run` again, and ERET back into the
  failure point in the time it takes the loader to read ~6 MiB — the
  foundation of the per-iteration debug loop.
- **Function-level execution tracer.** `--features trace,quiet`
  patches every entry in the curated `code-symbols.txt` with an
  HVC trampoline and logs `seq PC name (mode) r0..r3` on every
  call. Variant `trace_once` gates the line on a per-function
  fired-bitmap if you want first-touch only. Used end-to-end to
  bisect boot stalls against Einstein.
- **Live display + pen input.** Each `screen::blit` is forwarded
  through `src/host_io/` to a companion viewer at
  `tools/host-viewer/`, which opens a window via softbuffer + winit,
  applies the blit stream to its own 320×480 2 bpp backing store, and
  posts mouse events back as Newton pen samples. Selected with
  `--features host-io-semihost`; the default `host-io-null` backend
  is a no-op (used by guest-tests and CI).
- **Guest-test tier** under `guest-tests/` — 37 small AArch32
  binaries against a shared runtime, with an HVC protocol for
  pass/fail/print. Cover every handler (CP15, VIC, DMA, flash,
  serial, screen blit, native primitives, tablet, PCMCIA, snapshot
  round-trip, banked-reg paths, `LDR` rotate, SWP, …).
- **Diagnostic scaffolding.** DABT/PABT DIAG vectors at ROM offsets
  `0x10` / `0x0C`, BootOS / PowerOff / Reboot canaries (semihost/dev
  builds only, gated on `cfg(nh_loud_halt_canaries)`), kernel struct
  dumps (`task_dump.rs` walks `TScheduler` / `TTask`).

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
`HLT #0xF000` to read/write snapshot files).

ARM FVP `FVP_Base_RevC-2xAEMvA` (accurate reference model):

```
rm -f /tmp/newton-snapshot-*.bin
cargo build --release --no-default-features \
    --features "platform-fvp-base quiet"
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
# Always cold-boot the first time you switch in/out of semihost;
# old snapshots from the host-io-null build are version-tagged
# and will be rejected, but cleaning explicitly makes the first
# boot faster.
cd /path/to/baremetal
rm -f /tmp/newton-snapshot-*.bin
cargo run --release --no-default-features \
    --features 'platform-raspi3b host-io-semihost'

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
state (matches real hardware; equivalent to Einstein's `SendPowerSwitchEvent`).

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

### Snapshot resume — the debug inner loop

`/tmp/newton-snapshot-{0..3}.bin` rolls four guest-state snapshots on
disk. On every startup the hypervisor tries to resume from the newest
valid slot; missing or mismatched files fall through to a cold boot.

```
# Cold boot (ignore any existing snapshots).
rm -f /tmp/newton-snapshot-*.bin
cargo run --release

# Normal run — loads newest valid slot if any, else cold-boots.
cargo run --release

# Pin an older slot to the top of the stack.
cp /tmp/newton-snapshot-2.bin /tmp/newton-snapshot-0.bin
```

Save triggers: every 2 s of wall time from `trap_irq`, plus
`HVC #0x18` from the guest for explicit checkpoints. Only guest-visible
state is persisted; hypervisor-side EL2 code, trap tables, and timer
deadlines are fresh each boot — the whole point: edit hypervisor code,
rebuild, resume mid-failure. See the "Snapshot / resume workflow"
section in `CLAUDE.md` for the full procedure.

## Cargo features

Features come in independent axes — a platform, plus one backend per
I/O seam. `build.rs` validates the combination at build time (exactly
one platform, at most one backend per axis) and falls back to the
`null` backend for any axis left unspecified.

| Axis / feature         | Default | Purpose                                                                              |
|------------------------|---------|--------------------------------------------------------------------------------------|
| `platform-raspi3b`     | yes     | QEMU raspi3b host (and real Pi Zero 2 W). BCM2835 VIC, PL011 at 0x3F201000.          |
| `platform-fvp-base`    | no      | FVP `FVP_Base_RevC-2xAEMvA` host. GICv3 brought up through an EL3 stub.              |
| `host-io-{null,semihost,pi-fb}` | null | Display + pen seam: no-op, semihost viewer IPC, or real VC4 framebuffer.      |
| `flash-persist-{null,semihost,sd}` | semihost | Flash persistence: volatile, `/tmp` file via semihosting, or FAT32 SD card. |
| `input-{null,mtouch}`  | null    | Pen-input seam: no-op or TSTP MTouch USB touchscreen (real hw).                      |
| `audio-{null,pi-hdmi}` | null    | Sound seam: null (no output, but arms timer-paced DMA-completion IRQs) or VC4 HDMI MAI audio (real hw). |
| `no-semihost`          | no      | Real hardware: no `HLT #0xF000` semihosting calls anywhere.                          |
| `trace`                | no      | Function-level execution trace via per-entry HVC trampolines.                        |
| `trace_once`           | no      | First-touch variant of `trace`. Trampolines still fire; only the SEQ line is gated.  |
| `quiet`                | no      | Silence recurring diag log lines (`fix_stage1_xn_bits:` summaries, etc.).            |
| `log_*`                | partial | Per-subsystem diagnostic logging. Default carries the low-volume tier (`log_traps`, `log_irqs`, `log_host_io`); the investigation tiers (`log_mmu`, `log_unaligned`, `log_tasks`, `log_store`) are opt-in. |
| `sd-probe`, `fb-probe`, `usb-probe` | no | Standalone real-hw bring-up probes (boot, test one peripheral, exit).      |

Aggregates for real hardware (`pi-bare-metal`, `pi-bare-metal-sd`,
`pi-bare-metal-display`, `pi-bare-metal-input`) roll up
`platform-raspi3b + no-semihost` plus progressively more backends;
`pi-bare-metal-input` is the full stack (SD flash, HDMI display +
audio, USB touch). The authoritative list is `Cargo.toml`.

Common combinations:

```
cargo run --release                                    # default: QEMU, full diag logs
cargo run --release --features quiet                   # QEMU, no diag noise
cargo run --release --features trace,quiet             # QEMU, clean function trace
cargo build --release --no-default-features \
    --features "platform-fvp-base quiet"               # FVP build (then scripts/fvp)
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh /Volumes/PIBOOT
                                                       # bootable SD for the Pi Zero 2 W
```

`trace` mutates ROM words at install time, so existing snapshots are
rejected on load — clear `/tmp/newton-snapshot-*.bin` before turning
tracing on or off.

## Function-level execution trace

Sample (abridged):

```
trace     1 0x000188f8 FlushTheCache (svc) r0=0x... r1=0x... r2=0x... r3=0x...
trace     2 0x00045b78 HandleDebugCard (svc) r0=0x... r1=0x... r2=0x... r3=0x...
trace     3 0x0011efb4 InitSpecialStacks (svc) r0=0x... r1=0x... r2=0x... r3=0x...
...
```

Every call — not first-touch — so a function called ten times produces
ten trace lines. Address list comes from
`scripts/classify-out/code-symbols.txt`, the curated code-only set
produced by `scripts/classify-symbols.py`. Mechanism (5-word in-ROM
trampoline per function, original first instruction copied or rewritten
in slot[1], branch-back at slot[4]) is in `src/tracer.rs`. To diff a
hypervisor trace against an Einstein trace of the same boot, use
`scripts/trace-diff.sh`.

## Guest-test tier

An ARM-guest test framework lives in [`guest-tests/`](guest-tests/) —
each test is a small AArch32 binary linked to a shared runtime
(`common/test_runtime.S`) that sets up SVC / IRQ / FIQ stacks, installs
an IRQ handler, and exposes an HVC protocol the hypervisor understands
(`HVC #1` = putchar, `HVC #3` = PASS, `HVC #4` = FAIL, `HVC #5` =
mark/progress). The hypervisor is built with `NH_GUEST_TEST=<bin>` set
in the environment so guest memory is populated with the test instead
of the ROM.

```
guest-tests/scripts/build-tests.sh                # build everything in MANIFEST
guest-tests/scripts/run-test.sh test_vic          # build + run one test
guest-tests/scripts/run-all.sh                    # run all 37 tests on QEMU
guest-tests/scripts/run-all.sh --platform fvp     # run all 37 tests on FVP
```

Add a new test by dropping `tests/<name>.S` in place, appending the
name to `tests/MANIFEST`, and rerunning. See `guest-tests/README.md`
for the full HVC protocol.

**Every commit must pass `guest-tests/scripts/run-all.sh`.** All 37
tests must stay green. (Probe-only iterations that touch nothing
outside `src/rom_patches.rs` and the dispatch in `src/trap/` can
skip the run — see the note in `CLAUDE.md`.)

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
(gdb) break src/trap/mod.rs:103
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
  (see `src/guest_bp.rs`). Patches the ROM word with `UDF #0xFFFE`
  and stops in `handle_user_bp_und` with `faulting_pc` set to the
  guest PC. Works for any ROM-range PC. Snapshot autosaves are
  gated while any BP is live, so a debug session never corrupts a
  persisted snapshot.

Typical recipe is in `CLAUDE.md` under "Breakpoint pattern for agents".

## Reference docs

Consult these before re-deriving state from disassembly:

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
- [`docs/WORKFLOW.md`](docs/WORKFLOW.md) — Einstein-port review,
  test-per-feature rule, finish-the-phase semantics.
- [`docs/ENDIAN_FIXES.md`](docs/ENDIAN_FIXES.md) — BE-32 word-invariant
  conventions (the trap that broke iter-55's blit).
- [`probe/FINDINGS.md`](probe/FINDINGS.md) — golden record from a
  fully-booted Newton via the instrumented Einstein probe.

## Layout

```
baremetal/
  Cargo.toml             crate manifest (no_std, panic=abort)
  rust-toolchain.toml    pinned toolchain + target
  build.rs               linker-script selection, symbol-blob embed
  .cargo/config.toml     build target, rustflags, cargo-run runner
  linker.ld              raspi3b image layout
  linker-fvp.ld          FVP-base image layout
  scripts/
    run-qemu.sh          cargo runner for QEMU
    boot-check.sh        marker-based QEMU boot verifier (kills QEMU
                         once the boot milestone appears in the log)
    fvp                  cargo-runner-equivalent for FVP (dockerised)
    gdb-init             gdb helpers (bg, bp, tt, guest-state, …)
    classify-symbols.py  ROM symbol partitioner (code/data/drop)
    classify-out/        curated symbol lists
    disasm-out/          rom.dis + indices
    trace-diff.sh        diff Einstein vs hypervisor function traces
    build-sd.sh          assemble a bootable Pi SD card
    regen-classify.sh    regenerate the classifier reach.bitmap
  src/
    main.rs              kmain: MMU, stage-2, vectors, VIC, timer; ERET
    boot.s               _start
    vectors.s            EL2 exception vectors, context save/restore
    cpu.rs / uart.rs / panic.rs / mmu.rs   bring-up; uart.rs holds the
                                           PL011 wire (routed to the
                                           guest's extr serial port via
                                           dma.rs) and the semihosting
                                           SYS_WRITE path for kprintln
    stage2.rs            stage-2 L1/L2/L3 tables
    guest.rs             ERET to AArch32 EL1 SVC at guest IPA 0
    guest_mem.rs         ROM load + byteswap + CP15 patch +
                         fix_stage1_xn_bits (stage-1 normalisation)
    guest_endian.rs      BE-8 byte-order-aware guest memory accessors
    guest_regions.rs     single region manifest (ipa/size/host_pa/
                         perms/snapshot) for stage2 + host_addr_for +
                         snapshot
    guest_trampolines.rs UND/DABT vector trampolines + hypervisor-code
                         range predicate
    aarch32_emit.rs      AArch32 branch/literal encoders + install_patch
    trap/                sync-trap dispatch (mod.rs), per-EC handlers
                         (dabt.rs, und.rs, cp15.rs, hvc.rs, diag.rs),
                         trap_irq + slim ISR, update_virq, HVC tag table
    trap_context.rs      TrapContext + read_sysreg! macros + describe_ec
    probes.rs            Newton-ROM probe handler bodies
    slim_isr.rs          slim-ISR state ownership (IrqCap token)
    host_dma.rs          host-side BCM2835 DMA driver (UART TX, MAI, SD)
    mmio.rs              IPA dispatch into peripherals/ (MmioPeripheral)
    peripherals/         battery, dma, flash[_driver], host_call,
                         native_primitives, network, pcmcia, platform,
                         printer, screen, serial[_driver], sound,
                         tablet, vic, in_translator, out_translator
    platform/            raspi3b.rs, fvp_base.rs, gicv3.rs (FVP)
    timer.rs             CNTHP driver
    banked.rs            AArch32 banked-reg access from EL2
    rom_patches.rs       Einstein word-write patches; HVC injection;
                         canaries; ResolveFault wrapper
    shadow_stub.rs       in-ROM stub-pool + per-stub scratch-pool
    snapshot.rs          rolling 4-slot snapshot ring (semihosting I/O)
    tracer.rs            in-ROM HVC-trampoline tracer (--features trace)
    guest_bp.rs          one-shot guest software BPs (gdb 'bp <addr>')
    host_io/             live display + pen-input plumbing
                         (null / semihost / pi-fb backends)
    flash_persist/       flash persistence backends
                         (null / semihost / sd)
    sd/                  BCM2835 SDHOST driver + block device
                         + bring-up probe
    usb/                 DWC2 USB host stack (enumeration, HID)
    input/               pen-input backends (null / mtouch) +
                         touch calibration
    audio/               sound backends (null / pi_hdmi —
                         VC4 HDMI MAI)
    display/             VC4 mailbox framebuffer + boot splash
    mailbox.rs           VC firmware mailbox interface
    task_dump.rs         TScheduler / TTask walker
    unaligned.rs         unaligned-load fixup
    tarmac.rs            FVP Tarmac plugin window markers
  guest-tests/
    common/              shared runtime, linker script, HVC macros
    tests/               35 .S files + MANIFEST
    scripts/             build-tests.sh, run-test.sh, run-all.sh
    README.md            HVC protocol, how to add a test
  probe/                 instrumented-Einstein oracle build
  docs/                  reference docs (see above)
  classify/              cached classifier output (reach.bitmap per ROM hash)
  tools/                 classify-rom, host-viewer, romdump
  vendor/                vendored embedded-sdmmc 0.9.0 (path dep,
                         local changes listed in VENDOR.md)
  boot-pi/               Pi firmware config.txt for the SD boot partition
  assets/                boot splash image
  PLAN.md                iteration log + current goal
  HIGHLEVEL.md           architecture + roadmap + open-question log
  IMPLEMENTATION.md      pure-Rust plan, language/tooling rationale
  CLAUDE.md              hypervisor notes (auto-loaded by Claude Code)
```

## Running on real hardware

**Works.** The deployment target is the **Pi Zero 2 W** (not the
Pi 3B — same SoC, same image; only the form factor differs), and the
full stack is hardware-validated: EL2 handoff, ROM boot, HDMI display,
USB touchscreen, HDMI audio, and SD-card flash persistence with
non-blocking DMA autosave.

Build a bootable SD card with:

```
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest> [sd-mount]
```

See [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md) for the
hardware specifics — `config.txt`, UART routing, the TSTP MTouch
panel, SDHOST details. The serial port and PCMCIA images are the
remaining unported peripherals.

## Cheatsheet

```
cargo build --release                         # just build (raspi3b)
cargo run --release                           # build + boot Newton ROM in QEMU
cargo run --release --features trace,quiet    # boot with function-level trace, quiet diag
DEBUG=1 cargo run --release                   # same, paused with gdb stub on :1234
guest-tests/scripts/run-all.sh                # run every guest test on QEMU
guest-tests/scripts/run-all.sh --platform fvp # run every guest test on FVP
guest-tests/scripts/run-test.sh test_vic      # one test, verbose output in /tmp

# FVP cold boot
rm -f /tmp/newton-snapshot-*.bin
cargo build --release --no-default-features --features "platform-fvp-base quiet"
scripts/fvp --timeout=90 \
    target/aarch64-unknown-none-softfloat/release/newton-hypervisor

# Force cold boot of QEMU run
rm -f /tmp/newton-snapshot-*.bin && cargo run --release

# Probe real Einstein against the 717006 ROM for oracle behaviour
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 90

# Inspect ROM bytes properly — don't hex-decode by hand
less scripts/disasm-out/rom.dis
```
