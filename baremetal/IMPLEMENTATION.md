# Newton Hypervisor — Implementation Plan

**Scope:** language choice, build system, tooling, and testing strategy for the bare-metal Pi Zero 2 W port described in [`HIGHLEVEL.md`](./HIGHLEVEL.md). This doc does not re-state the architecture; read HIGHLEVEL.md first.

**Status:** draft, pre-M1.

## 1. Language split

**Pure Rust** (`no_std`, `aarch64-unknown-none-softfloat`). The hypervisor and every peripheral state machine live in one crate; Einstein's C++ is a *reading reference* but is not compiled or linked.

```
  baremetal/src/                               Einstein (reference only)
  +-------------------------------+            +-----------------------+
  | EL2 init, vectors, ERET       |            | TInterruptManager     |
  | Stage-2 page tables           |   read     | TDMAManager           |
  | Trap dispatch                 |  <------   | TFlash                |
  | CP15 shim                     |            | TScreenManager        |
  | vIRQ / vFIQ injection         |            | TSoundManager         |
  | Pi drivers: mini-UART,        |            | TSerialChip*          |
  |   mailbox, framebuffer, SD,   |            | TPCMCIAController     |
  |   USB HID, I2S                |            |                       |
  | Peripheral state machines     |            |                       |
  |   (Rust port of Einstein's)   |            |                       |
  | Config/boot loader            |            |                       |
  +-------------------------------+            +-----------------------+
```

### 1.1 Why Rust for everything

- Memory safety at compile time is disproportionately valuable in a hypervisor: one MMIO or page-table bug = guest escape.
- `no_std` + `aarch64-unknown-none-softfloat` is mature; bare-metal Pi in Rust is well-trodden.
- Stable inline assembly (since 1.59) handles `MSR`/`MRS`/`ERET`/vector prologues cleanly.
- System registers and MMIO have high-quality typed wrappers (`aarch64-cpu`, `tock-registers`) that eliminate the category of bugs C historically hosts.
- Enum + exhaustive `match` is an ideal fit for the CP15 shim's `(op1, CRn, CRm, op2, direction)` decode and for the VIC / DMA register dispatch.
- Stage-2 descriptor layouts as `bitflags!` + `repr(C)` structs are harder to silently corrupt than C bitfields.

### 1.2 Why not link Einstein's C++

We tried. See `cxx-core/` at commit `26c1816` (now removed):

- The simple peripherals (`TFlash`, `TDMAManager`) are 30-60 lines of actual logic once you strip Einstein's save/restore and stdio plumbing. Rust ports are comparable in size.
- The one with real mass, `TInterruptManager`, is mostly a `TThread` / `clock_gettime` scheduling wrapper around a small state machine — and none of that wrapper applies to a trap-driven hypervisor that polls from trap handlers instead of blocking a main thread on a condvar.
- Freestanding Einstein means stubbing pthread, stdio, exceptions, RTTI, mmap, and maintaining an FFI boundary on both sides. We never actually succeeded in linking the bare-metal target against any Einstein object — all cxx-core tests were host-side (glibc + pthread). The freestanding port stayed perpetually "next".
- Einstein is still the authority on register bit semantics. Instead of linking it, we keep it as a documentation source: [`docs/peripherals.md`](docs/peripherals.md) captures what we learned about each peripheral with pointers into Einstein's files.

### 1.3 Peripheral ports — where they live

Each peripheral is one Rust module under `src/peripherals/`, with `#[cfg(test)]` unit tests that run under `cargo test` on the host. They cannot import `core::arch::asm!` or other bare-metal-only features in test mode; the state machine proper is platform-neutral, separated from the MMIO trap glue in `src/mmio.rs`.

### 1.3 Concrete scope from the probe runs

Against the 717006 ROM with the Einstein REx (90 s boot; see [`probe/FINDINGS.md`](probe/FINDINGS.md) for the raw capture), the probe nailed down the implementation scope for several sections that were previously described as "to be enumerated empirically":

- **CP15 shim:** 15 unique `(opc1, CRn, CRm, opc2, dir)` tuples total. Of those, 14 have direct AArch32-on-A53 equivalents (SCTLR, TTBR0, DACR, IFSR/DFSR, IFAR/DFAR, `DCCMVAC` and friends, TLBI variants). The 15th is a StrongARM `c15, op1=0, CRm=1, op2=2` clock-control write that fires **exactly once** at boot; trap-and-no-op. The shim's dispatch table is one Rust `match` with 15 arms — the compiler can enforce exhaustiveness.

- **SWP:** Exactly **one** call site (`0x003AE200`) emits every SWP in the kernel. Implementation: at ROM-load time in the hypervisor, patch that site with an `LDREX`/`STREX`/branch-back sequence. No trap handler needed. (Keep one as a safety net for variants: if we ever see a SWP fault from a PC other than the patched site, trap-and-emulate and log.)

- **MMU fixup:** Guest L1 table contains three fine-table descriptors (bits `0b11`) at VA `0x78000000`, `0x90000000`, `0xAC000000`, all with fault-only L2 contents (PCMCIA window placeholders). AArch32 on A53 doesn't walk `0b11` L1 entries. Handle by trapping writes to TTBR (`HCR_EL2.TVM`) and rewriting `0b11` → `0b00` in a shadow table; point real TTBR at the shadow. Leaves the guest's view of memory unchanged because the only accessible side of those descriptors is faults either way.

- **DACR:** Always `0x00055555`. The write path in the shim can be a single-value passthrough; no per-value decoding needed.

- **Privilege:** Guest spends its time overwhelmingly in USR mode (19 310 entries vs 649 SVC over 90 s). AP enforcement stays on; no flattening.

These findings mean several items that `HIGHLEVEL.md` §6 flagged as "enumerate empirically" are now concrete tables we can hand off to Rust `match` or to a ROM-patch descriptor. Implementation is no longer blocked on instrumentation runs; it's blocked on the remaining design gate (`HIGHLEVEL.md` §16.1, EL2 handoff on real Pi firmware) and peripheral bring-up milestones.

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

All new code lives under `baremetal/` at the repo root so upstream Einstein
stays untouched and porting this work is a subdirectory merge. Paths
below are relative to `baremetal/`.

```
Cargo.toml
Cargo.lock
rust-toolchain.toml
build.rs               # NH_GUEST_TEST env var -> embed a test image
.cargo/config.toml     # target, rustflags, cargo-run QEMU runner
linker.ld              # image layout: load at 0x80000, 16 KiB stack
scripts/run-qemu.sh    # cargo runner: ELF -> kernel8.img -> QEMU
src/
  main.rs              # no_std entry, kmain, banner
  boot.s               # _start: park non-zero cores, SP, bss, call kmain
  vectors.s            # EL2 vector table
  cpu.rs               # CurrentEL, MPIDR_EL1, ID_AA64*_EL1, halt()
  uart.rs              # PL011 driver + kprint!/kprintln! macros
  panic.rs             # panic handler: print + halt
  mmu.rs               # EL2 stage-1 identity map
  stage2.rs            # guest-physical stage-2 tables
  guest_mem.rs         # ROM / RAM / flash / framebuffer backing stores
  guest.rs             # ERET to EL1 AArch32
  trap.rs              # EL2 trap dispatch (data/insn abort, CP15, HVC)
  mmio.rs              # address -> peripheral routing
  vic.rs               # Newton VIC state + match-register edge detection
  timer.rs             # CNTHP async match delivery
  peripherals/         # (coming) Rust ports of Einstein's peripherals
    flash.rs
    dma.rs
    ...
docs/
  peripherals.md       # spec capturing Einstein's observable behaviour
roms/                  # .gitignore'd — developer-provided Newton ROM dumps
probe/                 # headless Einstein harness (C++, host build only)
  probe.cpp
  FINDINGS.md
guest-tests/           # AArch32 peripheral tests loaded as the guest
  common/test_runtime.S
  common/linker.ld
  tests/*.S
  scripts/{build-tests,run-test,run-all}.sh
```

## 3. Peripheral ports

We port each peripheral's state machine directly into Rust as a module
under `src/peripherals/`. The spec for each peripheral lives
in [`docs/peripherals.md`](docs/peripherals.md), which cross-references
the Einstein C++ file + line numbers for ground truth.

### 3.1 Scope per peripheral

Targeting the Newton's boot path, in roughly the order they matter:

- `TFlash` → `peripherals/flash.rs` (byte-addressable backing + seeded Newton header + masked writes + erase).
- `TInterruptManager` → `peripherals/vic.rs` (already partly exists as `src/vic.rs`; will be moved).
- `TDMAManager` → `peripherals/dma.rs` (assignment register + log-only stubs for the rest).
- `TPCMCIAController` → `peripherals/pcmcia.rs` (return "no card" for probes, drop writes).
- `TSerialPorts` / `TSerialChip*` → `peripherals/serial.rs` (minimal for kernel's init probe).
- `TNativePrimitives` → `peripherals/native_primitives.rs` (coproc 10/11 gateway for screen / battery / tablet). Big.
- `TScreenManager` → `peripherals/screen.rs` (Blit intercept + framebuffer dump).

### 3.2 Testing

Two tiers, both cheap:

- **Rust host unit tests.** Each `peripherals/<name>.rs` includes
  `#[cfg(test)]` module. Runs under `cargo test --target x86_64-unknown-linux-gnu`
  against the same Rust code that ships in the hypervisor — the state
  machine must not depend on bare-metal-only features.
- **ARM-guest integration tests** under `guest-tests/`.
  AArch32 binaries loaded in place of the Newton ROM, poking MMIO from
  the guest side and reporting PASS/FAIL via HVC. Validates the full
  stack: MMU, stage-2, trap dispatch, Rust dispatcher, peripheral
  state machine.

### 3.3 Einstein as oracle, not dependency

- Source-level: [`docs/peripherals.md`](docs/peripherals.md) is the
  authoritative spec for peripheral behaviour; when something there
  doesn't match Einstein's C++, Einstein wins and the doc gets corrected.
- Runtime: `probe/` boots the full Einstein emulator
  against the real ROM and captures observable state (MMU tables,
  CP15 op set, SWP call sites, etc.). That's how we validate our
  assumptions about what the Newton actually does. The probe is
  unaffected by the pivot — it was always a separate host binary.
- No heap in v1. Peripheral objects are constructed once at startup into static storage.

## 4. Peripheral module shape

Each peripheral is a Rust module with:

- a `State` struct holding the register state;
- `read(&mut State, ipa: u64, size: SizeAccess) -> u32` and
  `write(&mut State, ipa: u64, size: SizeAccess, value: u32)` dispatch
  functions that take the IPA of the access; and
- peripheral-specific queries (e.g. `fn irq_pending(&State) -> bool`
  for the VIC, `fn fb_dirty_range(&State) -> Option<(u32, u32)>` for
  the screen).

Example sketch:

```rust
// src/peripherals/flash.rs
pub struct State {
    banks: [[u8; BANK_SIZE]; 2],
    fresh: bool,
}

impl State {
    pub const fn new() -> Self { ... }
    pub fn seed_newton_header(&mut self) { ... }
    pub fn read(&self, offset: u32, bank: u32) -> u32 { ... }
    pub fn write(&mut self, word: u32, mask: u32, offset: u32, bank: u32) { ... }
    pub fn erase(&mut self, block_size: u32, offset: u32, bank: u32) { ... }
}
```

The MMIO dispatcher in `src/mmio.rs` routes the IPA + size +
value to the right peripheral's `read` / `write`. No FFI, no opaque
handles — the Rust type system is the contract.

## 5. Build system

Single-crate Cargo build targeting `aarch64-unknown-none-softfloat`:

- `cargo build --release` produces the hypervisor ELF.
- `scripts/run-qemu.sh` (the cargo runner) does `llvm-objcopy -O binary`
  to produce `kernel8.img` and boots QEMU `raspi3b`.
- `build.rs` reads `NH_GUEST_TEST`; if set, it embeds the named AArch32
  guest-test binary instead of the Newton ROM.

No CMake, no external C toolchain, no linker gymnastics.

Output: a single flat `kernel8.img` plus an ELF with DWARF for gdb.

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

- `gdb-multiarch` handles Rust DWARF; demangling is flakier than C but usable. Set `set print asm-demangle on`.
- Consider `probe-rs` + JTAG on real Pi for M5+ when USB/display debugging over serial alone becomes painful.

### 8.4 Classifier-driven endianness patching

The Newton ROM is BE-32 word-invariant: aligned word accesses are
identical to the LE view after a load-time word swap, but byte and
halfword accesses target a different byte lane and must be fixed up.
`src/shadow_stub.rs` handles that by replacing each LDRB/STRB/LDRH/
STRH/LDRSB/LDRSH/SWPB in the ROM with a `Bcc` to a per-site stub
that XORs the effective address before the real load/store. For that
to be both correct (every real byte/halfword access patched) and
safe (no data bytes overwritten), the patcher needs an exact list
of byte-access PCs.

The list is built by two host-side tools in `tools/` and `scripts/`:

1. **`scripts/classify-symbols.py`** — partitions every entry in
   `_Data_/demangled_symbols.txt` into `code` / `data` / `drop` via
   an ordered ruleset. Rules are name prefixes (`g[A-Z]` → data,
   `F[A-Z]` → code, `::` or `(` → code, `SYM*` / `rat*` / `BiGS*`
   → data tables, …), address-range rules (exception vectors +
   early-boot text), and a first-word-shape fallback (cond=AL plus
   any recognised ARM encoding → code; top byte 0x00 → data).
   Outputs a curated `classify-out/code-symbols.txt` plus
   `classify-out/data-ranges.txt` (contiguous data-symbol extents
   + hand-maintained DATA_RANGES for things like the recognition-
   table block at `[0x00366f2c, 0x00382324)` and the inline-string
   run at the tail of `MonitorEntryGlue`). Weird outliers go in
   `CODE_EXCEPTIONS` / `DATA_EXCEPTIONS` sets rather than contorted
   rules.

2. **`tools/classify-rom`** — consumes the code list as walker roots
   and the data-ranges as walker termination boundaries, walks
   every basic block via a full ARM decoder (the `step()` function
   recognises B / BL / Bcc / LDR pc / BX / LDM-with-pc / SWI / UDF
   as terminators; `MOV LR, PC` followed by a PC-write as a
   manual-BL idiom; conditional DP writing PC as jump-table
   dispatch). Walker stops on entering any data range — no leakage
   into string tables, vtable data, or literal pools. Finally
   intersects reachability with an `is_byte_access` decoder
   (mirror of `shadow_stub::decode`) and writes
   `classify/<hash>/byte-access-static.bitmap` — one bit per 32-bit
   word across the 16 MiB ROM+REX aperture.

Additional seeds the walker needs:

- **REx header entry table.** `_Data_/Einstein.rex` has no symbol
  file; the walker parses the `"RExBlock"` header at PA 0x00800000
  and for each `fdrv` / `FDRV` / `pkgl` entry extracts the
  pointers inside the class-info block that point at prologue-
  shaped code, seeds those as method roots.
- **Vtable install pattern.** Constructors install a vtable with
  the two-instruction sequence `LDR Rt, [pc, #imm]` followed by
  `STR Rt, [Rn, #0]` (where Rn is whatever register holds `this`
  — often R4 after the APCS prologue, not R0). The classifier
  scans reached code for this pair, chases the literal to a
  vtable address, and enumerates pointer entries until a non-
  code-looking word.

Invariants:
- Every bit in the final bitmap decodes as a byte/halfword access
  `shadow_stub::decode` accepts, including PC-operand rejection
  (PC-as-Rn/Rt/Rm/Rt2 is filtered at the classifier level, not
  silently skipped at patch time).
- `oracle ⊆ static` when a NewtonProbe-generated oracle bitmap is
  present (execute-time set must be a subset of the static set;
  a missing bit is a classifier reachability gap, not a
  classifier false positive).

`build.rs` embeds the bitmap into the hypervisor via `include_bytes!`
and a FNV-1a-32 hash of `rom_bytes || rex_bytes` so a stale bitmap
on a newer ROM halts the hypervisor at boot rather than silently
patching wrong PCs. `scripts/regen-classify.sh` is the one-stop
regen: it runs `classify-symbols.py` if needed, rebuilds the
classifier, runs it with the curated inputs.

## 9. Open questions (implementation-specific)

Design-level open questions are in `HIGHLEVEL.md` §16. These are narrower and implementation-only.

1. **Soft-float target vs hard-float.** EL2 doesn't need FP. Guest may use VFP (CP10/CP11). Decide: trap-and-emulate VFP, context-switch VFP on world-switch, or passthrough. `-softfloat` target for the hypervisor itself is the safe default.
2. **`compiler-builtins` vs `libgcc`.** Rust's `compiler-builtins` covers most intrinsics; gaps require `libgcc`. Determine empirically which (if any) we pull in.
3. **Panic behavior.** v1: print location + CPU state on mini-UART, halt all cores, wait for reset. No unwinding. `-C panic=abort`.
4. **Static stacks.** Size per exception level, per core. Propose 16 KiB for EL2 main, 4 KiB for each exception stack, 4 KiB IRQ stack. Revisit after first panic due to exhaustion.
5. **MMU-on handoff.** Rust EL2 entry runs with MMU off briefly; switching it on must not invalidate the currently-executing code region. Standard bare-metal concern, standard solution (identity-map the current PC before enabling). Nothing novel, just needs to be right.
6. **Build reproducibility.** Pin `rust-toolchain.toml`; vendor or lock every crate. The hypervisor binary should be byte-reproducible from a given commit.
7. **License headers.** New Rust files: GPLv2 to match Einstein, or dual-license. Decide once, apply everywhere.

## 10. What this doc does not cover

Deployment workflow beyond the dev loop, production image signing, OTA update, debug/release feature flags, non-Pi ports. All are post-v1.
