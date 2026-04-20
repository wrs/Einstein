# Newton Hypervisor — baremetal

Pure-Rust Type-1 hypervisor that runs an unmodified Apple Newton OS 2.x
ROM natively on a Cortex-A53. Design and rationale live in
[`HIGHLEVEL.md`](HIGHLEVEL.md); the implementation plan and why we
don't link Einstein's C++ live in [`IMPLEMENTATION.md`](IMPLEMENTATION.md);
the Newton peripheral spec (with Einstein cross-references) is in
[`docs/peripherals.md`](docs/peripherals.md).

**Current status:** M5 — hypervisor boots at EL2 under QEMU `raspi3b`,
installs stage-2 maps for ROM / RAM / flash / framebuffer, drops to
EL1 AArch32 at the ROM reset vector, and runs Newton 717006 natively.
The guest progresses through CP15 init, SCTLR toggles, TTBR/DACR
install, PCMCIA probe, and SVC↔FIQ mode switches before stalling in a
pre-scheduler wait loop (expected until more peripherals are ported).

What works end-to-end:

- **EL2 stage-1 MMU** with a 1 GiB Normal-WB region for the image and
  the lower 1 GiB MMIO window, plus a Device-nGnRE block covering the
  BCM2836 per-core peripheral at `0x4000_0000`.
- **Stage-2 trap dispatch** with decoded `ESR_EL2.EC` handlers for
  data aborts, instruction aborts, HVCs, and trapped CP15 (`TVM` /
  `TRVM` / `TIDCP`).
- **CP15 shim** covering the 15 tuples Newton 717006 actually issues,
  rewriting the StrongARM lax encoding (`MCR p15,0,Rn,cN,cN,0`) at
  ROM load so ARMv7's `CRm=0` form is what the hardware sees.
- **Ticks clock** at 3.6864 MHz, scaled from `CNTPCT_EL0` with
  `CNTFRQ_EL0`, exposed at `0x0F18_1800`.
- **VIC state** (`int_present`, `int_ctrl`, `fiq_mask`, edge registers,
  four match registers) with gating matching `TInterruptManager` and
  rising-edge latching for timer matches.
- **Async timer delivery** via CNTHP (`src/timer.rs`): each match-reg
  write reprograms `CNTHP_CVAL_EL2`; the EL2 physical timer raises
  CNTHPIRQ; the BCM2836 local peripheral routes it to core 0's IRQ;
  `trap_irq` latches the fired bits and sets `HCR_EL2.VI`. A guest
  parked in `wfi` wakes on real time.
- **Guest-test tier** (`guest-tests/`) with its own AArch32 runtime, an
  HVC protocol for pass/fail/print, and four tests today: `test_hello`
  (HVC sanity), `test_vic` (async timer delivery + IRQ handler
  round-trip via WFI), `test_flash`, `test_dma`. All pass on QEMU
  raspi3b.

What's still scaffolding (see HIGHLEVEL.md §16 and
`docs/peripherals.md` for the target shape):

- `mmio.rs` returns ad-hoc constants for most registers outside the
  VIC/ticks window. The plan is to replace each stub with a module
  under `src/peripherals/`, porting Einstein's C++ state machine into
  Rust.
- `guest_mem.rs::load_newton_rom` patches ROM words 1..=6 to
  `movs pc, lr` so early exceptions don't fall into the unmapped ROM
  jump-table VAs. This bring-up cheat comes off once the full
  peripheral / interrupt stack runs.
- CP15 encoding rewrite (`guest_mem.rs::patch_cp15_encodings`) is a
  static in-place patch at ROM load. Runtime translation of the
  StrongARM variants is a follow-on.

## Prerequisites

- `rustc` via `rustup`; the pinned toolchain and target come from
  `rust-toolchain.toml` (currently stable 1.94.1, target
  `aarch64-unknown-none-softfloat`).
- `qemu-system-aarch64` with `raspi3b` support.
- `arm-none-eabi-gcc` + `arm-none-eabi-objcopy` for cross-compiling the
  guest-test images.
- `gdb-multiarch` for source-level debugging.
- A Newton 2.x ROM at `roms/newton.rom` (8 MiB, byteswapped inside the
  hypervisor on load; gitignored — **never commit the ROM**).

`apt install qemu-system-arm gcc-arm-none-eabi gdb-multiarch` plus
`rustup target add aarch64-unknown-none-softfloat` covers a fresh box.

## Build and run

```
cargo build --release          # just build
cargo run --release            # build + boot the Newton ROM in QEMU
```

`cargo run` invokes [`scripts/run-qemu.sh`](scripts/run-qemu.sh), which
objcopies the ELF to `kernel8.img` and launches

```
qemu-system-aarch64 -M raspi3b -kernel kernel8.img \
    -serial stdio -display none -no-reboot
```

Expected output (abridged):

```
===============================================
 Newton Hypervisor v0.0.1  (baremetal, M0)
 Target: Cortex-A53 / BCM2837 (Pi 3B, Zero 2 W)
===============================================
Current EL: 2
Core ID:    0
...
MMU: EL2 stage-1 enabled (identity map 0..1 GiB, MMIO as Device)
VBAR_EL2 = 0x0000000000080800
guest_mem: loading 8388608 bytes of ROM (byteswap big-endian -> little-endian)
stage2: ROM @ IPA 0x0..0x1000000  ... (RO)
stage2: RAM @ IPA 0x4000000..0x4400000 ... (RW)
stage2: flash @ IPA 0x2000000..0x2800000 ... (RW, 8 MiB)
stage2: framebuffer @ IPA 0xe000000..0xe200000 ... (RW, 2 MiB)
vic: timer epoch = ... CNTFRQ_EL0 = 62500000 Hz  (Newton tick = 3686400 Hz)
timer: CNTHP armed, CNTFRQ=62500000 Hz, CNTHPIRQ -> core0 IRQ
Entering Newton ROM...
Dropping to EL1 AArch32 at guest IPA 0x00000000 (ROM reset vector)
cp15: MCR p15,0,...   (many lines)
beacon: 10000 traps, ELR=..., int_present=0x0
...
```

The guest eventually stalls in a pre-scheduler wait loop; the `beacon:`
lines keep printing every 10 000 sync traps so you can see progress.
Stop QEMU with `Ctrl-A X`.

## Guest-test tier

An ARM-guest test framework lives in [`guest-tests/`](guest-tests/) —
each test is a small AArch32 binary linked to a shared runtime
(`common/test_runtime.S`) that sets up SVC / IRQ / FIQ stacks, installs
an IRQ handler that bumps a counter and snapshots `IntPresent`, and
exposes an HVC protocol the hypervisor understands (`HVC #1` = putchar,
`HVC #3` = PASS, `HVC #4` = FAIL, `HVC #5` = mark/progress). The
hypervisor builds the image with `NH_GUEST_TEST=<bin>` set in the
environment so the guest memory is populated with the test instead of
the ROM.

```
# Build every test listed in guest-tests/tests/MANIFEST
guest-tests/scripts/build-tests.sh

# Build + run a single test, report PASS/FAIL/TIMEOUT
guest-tests/scripts/run-test.sh test_vic

# Run all tests
guest-tests/scripts/run-all.sh
```

Add a new test by dropping `tests/<name>.S` in place, appending its
name to `tests/MANIFEST`, and rerunning `build-tests.sh`. See
`guest-tests/README.md` for the full HVC protocol.

## Debug with gdb

```
DEBUG=1 cargo run --release
```

The runner adds `-s -S` so QEMU exposes a gdb stub on `:1234` and
pauses at the reset vector. In another terminal:

```
gdb-multiarch target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
  -ex 'target remote :1234' \
  -ex 'break kmain' \
  -ex 'continue'
```

DWARF is enabled in both `dev` and `release`, so `break src/main.rs:40`,
`backtrace`, `info registers`, `stepi`, `next` all work. Setting
`break trap_sync_lower_aarch32` is the easiest way to step through
guest → EL2 transitions.

## Layout

```
baremetal/
  Cargo.toml             crate manifest (no_std, panic=abort)
  rust-toolchain.toml    pinned toolchain + target
  .cargo/config.toml     build target, rustflags, cargo-run QEMU runner
  linker.ld              image layout: load at 0x80000, 16 KiB stack
  scripts/run-qemu.sh    cargo runner; ELF -> kernel8.img; launches QEMU
  HIGHLEVEL.md           architecture + phasing + open-question log
  IMPLEMENTATION.md      pure-Rust plan, language/tooling rationale
  docs/peripherals.md    Newton peripheral spec (Einstein cross-refs)
  roms/                  newton.rom goes here (gitignored)
  src/
    main.rs              kmain: init MMU, stage-2, vectors, VIC, timer; ERET
    boot.s               _start: park non-zero cores, SP, BSS, kmain
    vectors.s            EL2 exception vectors, context save/restore
    cpu.rs               CurrentEL, MPIDR_EL1, halt(), sysreg readers
    uart.rs              PL011 driver (MMIO at 0x3F201000) + kprint!/kprintln!
    panic.rs             panic handler
    mmu.rs               EL2 stage-1: identity map, MAIR/TCR/TTBR, SCTLR.M
    stage2.rs            stage-2 tables for Newton guest physical layout
    guest.rs             ERET to AArch32 EL1 SVC at guest IPA 0
    guest_mem.rs         ROM load + byteswap + CP15-encoding patches;
                         stage-1 descriptor normalisation on first TTBR write
    trap.rs              sync-trap dispatch, MMIO decoder, CP15 shim,
                         trap_irq (async CNTHP delivery), update_virq
    mmio.rs              IPA dispatch (currently mostly inline stubs;
                         slated to move into src/peripherals/)
    vic.rs               Newton VIC state + 3.6864 MHz tick clock
    timer.rs             CNTHP driver: arm/rearm + BCM2836 IRQ routing
  guest-tests/
    common/              shared runtime, linker script, HVC macros
    tests/               one .S per test + MANIFEST
    scripts/             build-tests.sh, run-test.sh, run-all.sh
    README.md            HVC protocol, how to add a test
  probe/                 instrumented-Einstein oracle build for §16 answers
```

## Running on real hardware

Not yet. The image is built for QEMU `raspi3b` today and uses hard-coded
BCM2837 addresses (PL011 at `0x3F201000`, BCM2836 local peripheral at
`0x4000_0000`). Real Pi 3B / Zero 2 W needs at minimum:

- GPIO 14/15 routed to PL011 alt function (the firmware does this when
  `enable_uart=1` is set in `config.txt`).
- Verified EL2 firmware handoff — see `HIGHLEVEL.md` §16.1.
- An SD-card image pipeline (`kernel8.img`, `config.txt`, ROM file).

Defer until there's more in the image worth testing on real hardware.

## Cheatsheet

```
cargo build --release                       # just build
cargo run --release                         # build + boot Newton ROM in QEMU
DEBUG=1 cargo run --release                 # same, paused with gdb stub
guest-tests/scripts/run-all.sh              # run every guest test
guest-tests/scripts/run-test.sh test_vic    # one test, verbose output in /tmp

# Probe real Einstein against the 717006 ROM for oracle behaviour
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom - 90

# Raw QEMU invocation
qemu-system-aarch64 -M raspi3b -kernel kernel8.img -serial stdio -display none

# Useful QEMU debug flags
#   -d int,mmu,cpu_reset,guest_errors    instruction/MMU/error trace to stderr
#   -d in_asm                            every instruction executed
#   -singlestep                          slower but more deterministic
```
