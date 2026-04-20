# Newton Hypervisor — Implementation Plan

**Scope:** language choice, build system, FFI, tooling, and testing strategy for the bare-metal Pi Zero 2 W port described in [`HIGHLEVEL.md`](./HIGHLEVEL.md). This doc does not re-state the architecture; read HIGHLEVEL.md first.

**Status:** draft, pre-M1.

## 1. Language split

Rust (`no_std`, `aarch64-unknown-none-softfloat`) for the novel code; C++ for the Einstein peripheral classes that are reused verbatim. The two sides communicate via a hand-rolled C ABI at each peripheral.

```
  Rust (no_std)                           C++ (reused Einstein)
  +-------------------------------+       +-----------------------+
  | EL2 init, vectors, ERET       |       | TInterruptManager     |
  | Stage-2 page tables           |       | TDMAManager           |
  | Trap dispatch                 | --C-> | TFlash                |
  | CP15 shim                     |       | TScreenManager        |
  | vIRQ / vFIQ injection         |       | TSoundManager         |
  | Pi drivers: mini-UART,        |       | TSerialChip*          |
  |   mailbox, framebuffer, SD,   |       | TPCMCIAController     |
  |   USB HID, I2S                |       |                       |
  | Config/boot loader            |       |                       |
  +-------------------------------+       +-----------------------+
```

### 1.1 Why Rust for the new code

- Memory safety at compile time is disproportionately valuable in a hypervisor: one MMIO or page-table bug = guest escape.
- `no_std` + `aarch64-unknown-none-softfloat` is mature; bare-metal Pi in Rust is well-trodden (see §8.1).
- Stable inline assembly (since 1.59) handles `MSR`/`MRS`/`ERET`/vector prologues cleanly.
- System registers and MMIO have high-quality typed wrappers (`aarch64-cpu`, `tock-registers`) that eliminate the category of bugs C historically hosts.
- Enum + exhaustive `match` is an ideal fit for the CP15 shim's `(op1, CRn, CRm, op2, direction)` decode: the compiler fails the build when a new trap site lacks a handler.
- Stage-2 descriptor layouts as `bitflags!` + `repr(C)` structs are harder to silently corrupt than C bitfields.

### 1.2 Why keep Einstein peripherals in C++

- They already work. Rewriting them in Rust is weeks of effort and risk for zero correctness or performance gain.
- Their interface surface (per-peripheral register read/write + occasional callbacks) is narrow enough that a C shim is cheap.
- Translation to Rust is a reasonable v2 cleanup, not a v1 requirement.

## 2. Rust side

### 2.1 Target and toolchain

- Target: `aarch64-unknown-none-softfloat` (no FP/SIMD at EL2; guest gets its own VFP via CP10/CP11 trap-or-passthrough, TBD).
- Toolchain: pinned `rust-toolchain.toml` on a stable release; `rustup target add` covered by the toolchain file.
- Build: `cargo build --release`, emit a flat binary with `rust-objcopy -O binary target/aarch64-unknown-none-softfloat/release/newton-hypervisor kernel8.img`.
- Lints: `#![deny(unsafe_op_in_unsafe_fn)]`, `#![warn(clippy::pedantic)]` at crate level; per-module relaxations as needed. `rustfmt` on commit.

### 2.2 Crates (candidate set, to be validated)

| Purpose | Crate | Notes |
|---|---|---|
| Core CPU access | `aarch64-cpu` | system registers, barriers, core ID |
| Typed MMIO | `tock-registers` | volatile + field accessors |
| Bit flags | `bitflags` | stage-2 descriptors, HCR_EL2 bits |
| Compile-time register layout | `register` or hand-rolled | choose one, avoid both |
| Panic handling | hand-rolled | prints to mini-UART, halts |

Avoid: anything that pulls in `alloc` or `std` transitively. No global allocator in v1. Use static arenas and fixed-size buffers.

### 2.3 Project layout

```
hypervisor/
  Cargo.toml
  rust-toolchain.toml
  build.rs               # invokes CMake for the C++ core, emits link flags
  linker.ld              # EL2 layout: text, rodata, bss, stacks, vectors
  src/
    main.rs              # EL2 entry, parks extra cores, hands off to init
    arch/                # aarch64/aarch32 helpers, system-reg wrappers
    boot/                # firmware handoff shim, early UART
    mmu/                 # stage-2 tables, TLB ops
    trap/                # vector table, ESR_EL2 dispatch
    cp15/                # CP15 shim, trap handlers per (op1,CRn,CRm,op2)
    vic/                 # Newton VIC bridge, vIRQ/vFIQ injection
    drivers/             # mini-UART, mailbox, framebuffer, SD, USB, I2S
    peripherals/         # thin Rust wrappers over C++ shim handles
    panic.rs
  tests/                 # host-side unit tests (see §6)
cxx-core/                # C++ peripheral core (reused Einstein)
  CMakeLists.txt
  shim/                  # C ABI exposed to Rust
```

## 3. C++ side

### 3.1 What's reused

The peripheral classes identified in `HIGHLEVEL.md` §8. Source files come from `Emulator/`, trimmed to remove host-OS dependencies (FLTK, SDL, pthreads, sockets, `FILE*`). The reusable subset is approximately:

- `TInterruptManager`, `TDMAManager`, `TFlash`, `TScreenManager`, `TSoundManager`, `TPCMCIAController`
- `TSerialPortManager`, `TSerialChip*` (subset — drop the TCP-backed ones)
- `TMemory`'s MMIO dispatch tables (as reference; we replace the MMU walker and RAM/ROM paths)

Explicitly not reused: `TJIT*`, `TMMU`, `TARMProcessor`, `TNetworkManager`, anything in `Emulator/Host`, `Emulator/Files`, `Emulator/Platform`, `Monitor/*`, `app/*`.

### 3.2 Trimming strategy

Rather than fork and edit in place, build a new CMake target (`newton-peripherals`) that `#include`s the selected source files and provides freestanding replacements for the handful of things the reused classes reach into (logging, time, sleep). This avoids divergence from upstream Einstein.

### 3.3 Freestanding C++

- No STL where avoidable. Where unavoidable (e.g. `std::string` somewhere inside Einstein), provide a freestanding replacement or pull in a minimal `no_std`-compatible variant.
- No exceptions (`-fno-exceptions`), no RTTI (`-fno-rtti`).
- No heap in v1. Peripheral objects are constructed once at startup into static storage.

## 4. FFI boundary

### 4.1 Shim shape

Per peripheral, a C header and a C++ implementation. Example for `TInterruptManager`:

```c
// shim/interrupt_manager.h
typedef struct nh_interrupt_manager nh_interrupt_manager_t;

nh_interrupt_manager_t* nh_im_new(void);
void     nh_im_write(nh_interrupt_manager_t*, uint32_t addr, uint32_t val);
uint32_t nh_im_read (nh_interrupt_manager_t*, uint32_t addr);
void     nh_im_tick (nh_interrupt_manager_t*);
bool     nh_im_irq_pending(const nh_interrupt_manager_t*);
bool     nh_im_fiq_pending(const nh_interrupt_manager_t*);
```

Rust side declares the same via `extern "C"` blocks plus a thin safe wrapper per peripheral. Total shim: ~200–400 lines of C++ across all peripherals, plus matching Rust declarations.

### 4.2 Callbacks from C++ into Rust

Some peripherals need to raise host events (e.g. `TScreenManager` wants to damage a framebuffer region). Pass function pointers at construction:

```c
typedef struct nh_screen_callbacks {
  void (*damage)(void* ctx, uint32_t x, uint32_t y, uint32_t w, uint32_t h);
  void* ctx;
} nh_screen_callbacks_t;

nh_screen_manager_t* nh_sm_new(const nh_screen_callbacks_t*);
```

Rust installs trampolines marked `extern "C"`.

### 4.3 What the Rust side does *not* see

C++ type layout, vtables, templates, STL. Only opaque handles and POD-typed functions. This keeps `bindgen` out of the picture (which never handles C++ well) and makes the boundary auditable by eye.

## 5. Build system

### 5.1 Cargo-driven

The top-level `Cargo.toml` is the build entry point. `build.rs`:

1. Runs CMake to configure and build `cxx-core/` as a static archive (`libnewton-peripherals.a`).
2. Emits `cargo:rustc-link-search=` and `cargo:rustc-link-lib=static=newton-peripherals`.
3. Also links `libgcc` for soft-float/builtin intrinsics (or `compiler-builtins`, TBD — see §9.2).

### 5.2 Cross-compilation

- Host: Linux or macOS dev box, any arch.
- Cross toolchain: `aarch64-none-elf-gcc` for the C++ side (for consistent libc-less build). Alternative: `clang --target=aarch64-none-elf` with `-nostdlib`.
- Rust handles its own cross with `rustup target add`.

### 5.3 Output

A single flat binary `kernel8.img` plus an ELF with debug info (`newton-hypervisor.elf`) for gdb.

## 6. Testing strategy

Bare-metal code famously resists unit tests. Counteract by layering:

### 6.1 Host-side unit tests

Logic that doesn't touch hardware (CP15 decode, stage-2 table builder, VIC state machine, trap dispatch table) is compiled for the host target and tested with `cargo test`. Requires careful `#[cfg]` gating to exclude `no_std` panics and CPU intrinsics from the host build.

### 6.2 QEMU integration tests

`cargo test --target aarch64-unknown-none-softfloat` runs the image in QEMU with a test harness that exits via `psci` or a semihosting call. Each test is a `kernel8.img` variant that exercises one trap path and reports pass/fail on the mini-UART. Automatable in CI.

### 6.3 ROM-boot canary

The M2 milestone becomes a regression test: "loading ROM image X progresses past PC 0x*Y* without an unexpected fault." Checked in nightly.

### 6.4 What isn't testable without hardware

USB, real SD timing, display, audio. These land on real Pi 3B during the relevant milestone and have no equivalent CI coverage until we invest in hardware-in-loop.

## 7. Unsafe discipline

`unsafe` is unavoidable at the bottom (vectors, page-table descriptors, MMIO). Contain it:

- Every `unsafe` block has a comment stating the invariant the caller is asserting.
- No `unsafe` in business logic (CP15 shim, VIC state machine, trap dispatch) — only in the crate modules that touch hardware.
- `#![deny(unsafe_op_in_unsafe_fn)]` crate-wide.
- `miri` on the host-testable subset in CI.
- Code review for every new `unsafe` block, not just for changes to existing ones.

## 8. Tooling

### 8.1 Reference material

- `rust-raspberrypi-OS-tutorials` — walks through bare-metal Pi in Rust from first bytes through MMU, exceptions, drivers. Tutorials 10–17 cover exactly our M1–M3 needs.
- `aarch64-cpu` crate docs — system register API.
- ARM ARM DDI 0487 — the spec.
- BCM2711/2837 peripheral datasheets (Pi 3B, same SoC family as Zero 2 W).

### 8.2 Dev loop (repeated from HIGHLEVEL.md §11.5 for convenience)

1. `cargo build --release` → `kernel8.img`.
2. `qemu-system-aarch64 -M raspi3b -kernel kernel8.img -serial stdio -s -S &`
3. `gdb-multiarch target/aarch64-unknown-none-softfloat/release/newton-hypervisor` → `target remote :1234`.
4. On milestone pass, flash SD, boot real Pi 3B, confirm.
5. Pi Zero 2 W only at M6–M7.

### 8.3 Debugger notes

- `gdb-multiarch` handles Rust DWARF; symbol demangling is flakier than C++ but usable. Set `set print asm-demangle on`.
- Mixed-language stack walks work but occasionally mis-unwind across the FFI boundary; reading frames manually is a fallback.
- Consider `probe-rs` + JTAG on real Pi for M5+ when USB/display debugging over serial alone becomes painful.

## 9. Open questions (implementation-specific)

Design-level open questions are in `HIGHLEVEL.md` §16. These are narrower and implementation-only.

1. **Soft-float target vs hard-float.** EL2 doesn't need FP. Guest may use VFP (CP10/CP11). Decide: trap-and-emulate VFP, context-switch VFP on world-switch, or passthrough. `-softfloat` target for the hypervisor itself is the safe default.
2. **`compiler-builtins` vs `libgcc`.** Rust's `compiler-builtins` covers most intrinsics; gaps require `libgcc`. Determine empirically which (if any) we pull in.
3. **`bindgen` usage.** Recommendation: no. Hand-write the C ABI declarations in Rust. Small surface, clearer boundary, avoids the C++-to-C macro pain.
4. **`cxx` crate usage.** Recommendation: no for v1. Re-evaluate if the FFI shim grows past ~500 lines.
5. **Panic behavior.** v1: print location + CPU state on mini-UART, halt all cores, wait for reset. No unwinding. `-C panic=abort`.
6. **Static stacks.** Size per exception level, per core. Propose 16 KiB for EL2 main, 4 KiB for each exception stack, 4 KiB IRQ stack. Revisit after first panic due to exhaustion.
7. **MMU-on handoff.** Rust EL2 entry runs with MMU off briefly; switching it on must not invalidate the currently-executing code region. Standard bare-metal concern, standard solution (identity-map the current PC before enabling). Nothing novel, just needs to be right.
8. **Build reproducibility.** Pin `rust-toolchain.toml`; vendor or lock every crate; record C++ toolchain version. A hypervisor binary should be byte-reproducible from a given commit.
9. **License headers.** New Rust files: GPLv2 to match Einstein, or dual-license. Decide once, apply everywhere.

## 10. What this doc does not cover

Deployment workflow beyond the dev loop, production image signing, OTA update, debug/release feature flags, non-Pi ports. All are post-v1.
