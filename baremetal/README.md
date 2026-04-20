# Newton Hypervisor — baremetal

Bare-metal Rust skeleton for the Newton 2.x hypervisor described in
[`../HIGHLEVEL.md`](../HIGHLEVEL.md) and [`../IMPLEMENTATION.md`](../IMPLEMENTATION.md).

**Current status:** M0. The image boots on QEMU `raspi3b`, confirms it is
running at EL2 on core 0 (cores 1–3 parked in WFE), prints a banner over the
PL011 UART, and halts. No MMU, no stage-2, no traps, no guest. Everything
from here builds on this scaffold.

## Prerequisites

- Rust toolchain via `rustup`. The pinned version is declared in
  `rust-toolchain.toml`; it is fetched automatically on first `cargo` invocation.
- QEMU with `qemu-system-aarch64` (Debian/Ubuntu: `apt install qemu-system-arm`).
- `gdb-multiarch` for debugging (`apt install gdb-multiarch`).

## Build

```
cargo build --release
```

This produces an ELF at `target/aarch64-unknown-none-softfloat/release/newton-hypervisor`.

## Run under QEMU

```
cargo run --release
```

`cargo run` invokes [`scripts/run-qemu.sh`](scripts/run-qemu.sh) which:

1. Runs `llvm-objcopy -O binary` on the ELF to produce a flat `kernel8.img`.
2. Launches `qemu-system-aarch64 -M raspi3b -kernel <img> -serial stdio -display none`.

Expected output:

```
===============================================
 Newton Hypervisor v0.0.1  (baremetal, M0)
 Target: Cortex-A53 / BCM2837 (Pi 3B, Zero 2 W)
===============================================
Current EL: 2
Core ID:    0
Halted on core 0. Cores 1-3 parked in WFE.
Connect gdb via `target remote :1234` when running with `-s -S`.
```

The image halts in a WFE loop. Stop QEMU with Ctrl-A X, or kill the process.

## Debug with gdb

In one terminal:

```
DEBUG=1 cargo run --release
```

The script adds `-s -S` to QEMU so it exposes a gdb stub on `:1234` and
pauses at the reset vector.

In another terminal:

```
gdb-multiarch target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
  -ex 'target remote :1234' \
  -ex 'break kmain' \
  -ex 'continue'
```

DWARF debug info is enabled in both `dev` and `release` profiles, so source-level
breakpoints (`break src/main.rs:20`), `backtrace`, `info registers`, and
`stepi`/`next` work normally.

## Layout

```
baremetal/
  Cargo.toml             crate manifest (no_std, panic=abort)
  rust-toolchain.toml    pinned toolchain + target
  .cargo/config.toml     build target, rustflags, cargo-run QEMU runner
  linker.ld              image layout: load at 0x80000, 16 KiB stack
  scripts/run-qemu.sh    cargo runner; converts ELF -> kernel8.img; runs QEMU
  src/
    main.rs              no_std entry; kmain() prints banner and halts
    boot.s               _start: park non-zero cores, set SP, zero BSS, call kmain
    cpu.rs               CurrentEL, MPIDR_EL1, halt()
    uart.rs              PL011 driver (MMIO at 0x3F201000) + kprint!/kprintln!
    panic.rs             panic handler: print location + message, WFE forever
```

## What happens on boot

1. QEMU loads `kernel8.img` at `0x80000` and starts all four A53 cores at `_start`.
2. `_start` reads `MPIDR_EL1 & 0xff`. Cores 1–3 fall through to a WFE loop.
3. Core 0 loads `__stack_top` into SP, zeros BSS, calls `kmain`.
4. `kmain` initialises the PL011 (disable → set baud 115200 → 8N1 → enable),
   prints the banner and environment info (EL, core ID), and calls `cpu::halt`.
5. `cpu::halt` spins on `wfe` forever. The machine is now debuggable via gdb.

Boot runs with the MMU off, caches off, in AArch64 EL2. Enabling them will be
the next milestone.

## Running on real hardware

Not yet. The image uses hard-coded BCM2837 MMIO addresses, so it should work
on a real Pi 3B / Zero 2 W once we:

- Wire GPIO 14/15 to their PL011 alt function (currently assumed configured by
  firmware when `enable_uart=1` is set in `config.txt`).
- Confirm firmware hands off at EL2 — see `HIGHLEVEL.md` §16.1.

Defer until there's more in the image worth testing on hardware.

## Cheatsheet

```
cargo build --release                           # just build
cargo run --release                             # build + boot in QEMU
DEBUG=1 cargo run --release                     # same, paused with gdb stub

# Raw QEMU invocation if you want to add flags:
qemu-system-aarch64 -M raspi3b -kernel kernel8.img -serial stdio -display none

# Useful QEMU debug flags:
#   -d int,mmu,cpu_reset,guest_errors    instruction/MMU/error trace to stderr
#   -d in_asm                            every instruction executed
#   -singlestep                          slower but more deterministic
```
