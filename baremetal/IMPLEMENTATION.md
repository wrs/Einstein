# Newton Hypervisor — Implementation Plan

**Scope:** language choice, build system, tooling, and testing strategy for the bare-metal Cortex-A53 port described in [`HIGHLEVEL.md`](./HIGHLEVEL.md). This doc does not re-state the architecture; read HIGHLEVEL.md first.

**Status:** Bring-up complete; the 717006 ROM boots through to the Welcome UI. Most of the plan below is realized — the surviving sections capture both the rationale that shaped the codebase and current-state pointers into the tree. The iteration log lives in [`PLAN.md`](./PLAN.md); the user-facing project overview is in [`README.md`](./README.md).

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

Each peripheral is one Rust module under `src/peripherals/`. State-machine code is platform-neutral and separated from the MMIO routing in `src/hv/mmio.rs`. The test tier is the AArch32 guest-test suite in `guest-tests/`, which exercises the full hypervisor stack against the same handlers the ROM hits.

### 1.4 Concrete scope from the probe runs

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

### 2.2 Crates (candidate set, as evaluated)

| Purpose | Crate | Notes |
|---|---|---|
| Core CPU access | `aarch64-cpu` | system registers, barriers, core ID |
| Typed MMIO | `tock-registers` | volatile + field accessors |
| Bit flags | `bitflags` | stage-2 descriptors, HCR_EL2 bits |
| Compile-time register layout | `register` or hand-rolled | choose one, avoid both |
| Panic handling | hand-rolled | prints to mini-UART, halts |

**None of these helper crates were adopted.** System-register access,
MMIO, bit-flag encoding, and panic handling are all hand-rolled with
inline `asm!` and plain constants; the only path dependencies are
`newton-objects` (in-tree) and the vendored `embedded-sdmmc` (see
`Cargo.toml`). The hand-rolled forms kept the unsafe surface visible at
each use site and avoided an external API to track.

Avoid: anything that pulls in `alloc` or `std` transitively. No global allocator in v1. Use static arenas and fixed-size buffers.

### 2.3 Project layout

All hypervisor code lives under `baremetal/` at the repo root so
upstream Einstein stays untouched and porting this work is a
subdirectory merge. Paths below are relative to `baremetal/`. For
the user-facing layout (peripheral modules enumerated, scripts and
docs indexed), see [`README.md`](./README.md) — this section sticks
to the build / language / structural shape.

```
Cargo.toml             # crate manifest (no_std, panic=abort)
Cargo.lock
rust-toolchain.toml    # pinned toolchain + target
build.rs               # platform + ROM-version resolution, linker-script
                       # templating, classify-bitmap stage, symbol blob
                       # (NH_GUEST_TEST switches to guest-test mode)
.cargo/config.toml     # target, rustflags, cargo-run runner
linker.ld.in           # image-layout template; build.rs substitutes the
                       # per-platform load address into OUT_DIR/linker.ld
scripts/
  run-qemu.sh          # cargo runner: ELF → kernel8.img → QEMU
  fvp                  # cargo-runner-equivalent for FVP (dockerised)
  check-matrix.sh      # cargo-check all feature combos + the two lints
  check-layering.sh    # import-discipline lint for the src/ layers
  check-rom-addrs.sh   # ROM-address containment lint (rom_ver/)
  classify-symbols.py  # ROM symbol partitioner (code/data/drop)
  regen-classify.sh    # code-symbols.txt + reach.bitmap regeneration
  gdb-init             # gdb helpers (bg, bp, tt, guest-state, …)
src/                   # one crate, six layer directories; import
                       # direction low→high: arch ← hv ← newton (see
                       # scripts/check-layering.sh for the full rules)
  main.rs              # no_std entry, kmain boot narrative, ERET handoff
  panic.rs             # panic handler → loud halt
  arch/                # AArch64/AArch32 mechanism: boot.s, vectors.s,
                       # trap_context, mmu, cpu, banked, arm_decode,
                       # aarch32_emit, slim_isr
  hv/                  # generic hypervisor core: stage2, guest,
                       # guest_mem, guest_endian, be8, layout (single
                       # region/MMIO-window manifest), mmio router,
                       # timer, snapshot, hvc_imm, hooks (GuestOs
                       # seam), trap/{mod,dabt,und,cp15,hvc}
  newton/              # Newton-specific: os (GuestOs impls), loader
                       # (ROM load + selective byteswap, consumes
                       # reach.bitmap), rom_patches, probes,
                       # shadow_stub, guest_trampolines,
                       # unaligned[_inline], rom_ver/ (per-version
                       # constants: r717006, r710031 skeleton)
  peripherals/         # Rust ports of Einstein's peripheral models
  host/                # host drivers + backends: console/macros,
                       # platform/ (raspi3b, fvp_base, gicv3), mailbox,
                       # host_dma, sd/, usb/, display/, audio/, input/,
                       # host_io/, flash_persist/
  diag/                # diagnostics layer (feature `diag`): trap_diag,
                       # trap_hist, task_dump, heap_check, rep_print,
                       # symbols, guest_bp, tracer, tarmac
docs/
  peripherals.md       # peripheral spec + Einstein cross-references
  DISASM.md / NEWTON_INTERNALS.md / STRUCTURES.md / WORKFLOW.md
  QEMU_BUGS.md         # raspi3b AArch64↔AArch32 bug catalog
  ARM_Reference.txt    # ARMv7 reference (consult before re-deriving)
roms/                  # .gitignore'd — developer-provided Newton ROM
probe/                 # headless Einstein harness (C++, host build only)
classify/              # per-ROM-hash classifier outputs (reach.bitmap)
tools/classify-rom     # ROM code/data classifier (Rust, host build)
tools/romdump          # symbol-aware ROM hex dumper
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

### 3.1 Realised peripherals

Every peripheral the 717006 boot path exercises has a Rust module
under `src/peripherals/`. Each is paired with at least one entry in
`guest-tests/tests/` that exercises the handler surface in
isolation. See `docs/peripherals.md` for the per-peripheral spec
plus Einstein cross-references.

- **VIC** (`vic.rs`) — interrupt controller; CNTHP edge-trigger
  delivery via `trap_irq`; `HCR_EL2.VI` virtual-IRQ injection.
- **DMA** (`dma.rs`) — chip-wide assignment register; per-channel
  state for channels 0/1 (serial 0 RX/TX) mirroring Einstein's
  `TBasicSerialPortManager`; channels 2-7 still log+drop. Enable
  writes do not synthesise IRQs — completion fires only when bytes
  actually move (TX drain on enable, RX poll from `trap_irq`).
- **Flash** (`flash.rs`, `flash_driver.rs`) — bank0/bank1
  byte-addressable backing; seeded ROM+REx checksum table for
  `TReservedBlockAccessor`.
- **PCMCIA** (`pcmcia.rs`) — "no card" probe responses; absorbs
  enable/disable writes.
- **Serial** (`serial.rs`, `serial_driver.rs`) — TSerialChipVoyager
  MMIO surface + TSerialChipEinstein native-primitive subfns.
  Channels 0/1 of the external-serial port ("extr") are wired
  through `dma.rs` to the hypervisor's host PL011 so the guest can
  actually push and receive bytes.
- **Native primitives** (`native_primitives.rs`) — CP10/CP11
  gateway routing screen / battery / tablet / sound / printer /
  network / host-call / in/out-translator.
- **Screen** (`screen.rs`) — Blit intercept; each blit is forwarded
  through `host_io` to a paired host viewer for live display.
- **Battery / tablet / sound / printer / network / platform**
  (`battery.rs`, `tablet.rs`, `sound.rs`, `printer.rs`,
  `network.rs`, `platform.rs`) — minimal stubs sufficient for
  boot.
- **Translators** (`in_translator.rs`, `out_translator.rs`) —
  endpoint thunks for the abstract POutTranslator vtable;
  `ns_trace` feature uses these to capture kernel REP-printf
  output through `rep_print.rs`.
- **Host call** (`host_call.rs`) — semihosting bridge for
  guest-test diagnostics.

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

The MMIO dispatcher in `src/hv/mmio.rs` routes the IPA + size +
value to the right peripheral's `read` / `write`. No FFI, no opaque
handles — the Rust type system is the contract.

## 5. Build system

Single-crate Cargo build targeting `aarch64-unknown-none-softfloat`,
with a host-platform select at compile time:

- `cargo build --release` (default features) → QEMU `raspi3b` image.
  `cargo run --release` invokes `scripts/run-qemu.sh`, which
  `objcopy`'s the ELF into `kernel8.img` and boots QEMU.
- `cargo build --release --no-default-features --features
  "platform-fvp-base quiet"` → ARM FVP `FVP_Base_RevC-2xAEMvA`
  image. `scripts/fvp <elf>` wraps the dockerised model.
- The two platforms differ in load address, UART/GIC addresses,
  MMU memory map, and timer-IRQ routing — selected via the
  `platform-raspi3b` / `platform-fvp-base` mutually-exclusive
  features. The AArch32 guest ISA and the simulated Newton
  hardware are unaffected.
- `build.rs` does four things at compile time:
  (1) instantiates `linker.ld.in` with the chosen platform's load
  address and links against the result in `OUT_DIR`;
  (2) resolves the `rom-*` feature to its build inputs (ROM/REx
  paths, symbol tables, flash filename) via `resolve_rom_version()`;
  (3) stages the per-ROM-hash `reach.bitmap` from
  `classify/<hash>/` into `OUT_DIR` so the loader can
  `include_bytes!` it;
  (4) reads `NH_GUEST_TEST` and, if set, builds in guest-test mode
  (the test binary is semihost-loaded, or embedded when the var
  names a path) instead of booting the Newton ROM.

No CMake, no external C toolchain, no linker gymnastics. Output:
a single flat `kernel8.img` plus an ELF with DWARF for gdb.

## 6. Testing strategy

Two tiers — behavioural (guest tests) and structural (build-matrix
plus lints) — with the ROM boot itself as the end-to-end canary:

### 6.1 Structural tier: check-matrix + lints

`scripts/check-matrix.sh` runs `cargo check` over every supported
feature combination (platforms, real-hw aggregates, trace, probes,
guest-test cfg) in a shared target dir, after first running the two
structure lints: `scripts/check-layering.sh` (import discipline
between the six `src/` layer directories) and
`scripts/check-rom-addrs.sh` (ROM-space hex literals confined to
`src/newton/rom_ver/` + allowlist). There is no host-side
`cargo test` tier — all runtime verification happens in the guest
tests and the ROM boot.

### 6.2 Guest-test tier

`guest-tests/` holds 38 small AArch32 binaries linked against a
shared runtime (`common/test_runtime.S`) that sets up SVC / IRQ /
FIQ stacks, installs an IRQ handler, and exposes an HVC protocol
the hypervisor understands (`HVC #0x10` putchar, `HVC #0x12` PASS,
`HVC #0x13` FAIL, `HVC #0x14` mark; see `guest-tests/README.md`).
The hypervisor is built with `NH_GUEST_TEST` set; guest memory is
populated with the test instead of the ROM. Each test exercises one
trap path or peripheral surface and reports pass/fail on the UART.

```
guest-tests/scripts/build-tests.sh                # build everything
guest-tests/scripts/run-test.sh test_vic          # one test
guest-tests/scripts/run-all.sh                    # all 38 on QEMU
guest-tests/scripts/run-all.sh --platform fvp     # all 38 on FVP
```

Both QEMU and FVP must stay green on every commit. (Probe
iterations that touch only `src/newton/rom_patches.rs`,
`src/hv/trap/hvc.rs`, and `src/newton/probes.rs` can skip the
run — see `CLAUDE.md`.)

### 6.3 ROM-boot canary

The Newton ROM boot itself acts as a regression target: the
hypervisor must reach the current boot ceiling (see
[`PLAN.md`](./PLAN.md)) without an unexpected fault.
`scripts/boot-check.sh` automates the check — it boots the ROM
under QEMU and kills it once the expected milestone marker appears
in the log (`--cold` clears snapshots first for a full cold boot).

### 6.4 What isn't testable without hardware

USB, real SD timing, display, audio. These land on real Pi during
the relevant milestone and have no equivalent CI coverage until we
invest in hardware-in-loop.

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

### 8.2 Dev loop

Two host platforms, both green on every commit:

1. **QEMU `raspi3b`** (default, fast): `cargo run --release` boots
   the Newton ROM. `DEBUG=1 cargo run --release` pauses with a
   gdb stub on `:1234` for `aarch64-elf-gdb -x scripts/gdb-init`
   to attach.
2. **ARM FVP `FVP_Base_RevC-2xAEMvA`** (accurate reference):
   ```
   cargo build --release --no-default-features \
       --features "platform-fvp-base quiet"
   scripts/fvp --timeout=90 \
       target/aarch64-unknown-none-softfloat/release/newton-hypervisor
   ```
   GICv3, accurate generic-timer + cache model. `--gdb` exposes
   an Iris debug server on host port 7100. Wall-clock is much
   slower than QEMU TCG; use longer timeouts.

Real Pi (3B / Zero 2 W) is deferred — no live workflow today (see
`HIGHLEVEL.md` §16.1 on EL2 firmware handoff).

### 8.3 Debugger notes

- `aarch64-elf-gdb` (macOS, Homebrew) / `gdb-multiarch` (Linux)
  both handle Rust DWARF. Set `set print asm-demangle on`.
- EL2 hypervisor breakpoints work directly. EL1 guest (AArch32)
  breakpoints go through two helpers (`bg <addr>` /
  `bp <addr>`) because qemu-system-aarch64's gdbstub is
  aarch64-only and drops the AArch32 mode switch. See `README.md`
  for the recipe.

### 8.4 BE-8 mode + classifier-driven selective ROM byteswap

The guest runs with `CPSR.E=1` and `SCTLR_EL1.EE=1` forced by the
CP15 shim. ARMv7-A always fetches instructions in LE byte order
(per `DDI 0406C.d` §A3.3.1), so code words must be byteswapped at
load time — a host-LE read of the host backing then returns the
original BE numerical instruction encoding. Data words are stored
on host in BE-natural byte order (matching the on-disk ROM); a
guest LDR with `CPSR.E=1` reads them back as the BE numerical
value directly.

`load_newton_rom` consults the classifier `reach.bitmap` per word:
bit set → reachable code → byteswap on load; bit clear → data /
padding → byte-copy verbatim. Same logic for Einstein.rex. ROM
patches that write into the host backing go through
`guest_mem::write_rom_code_word` (verbatim, for instruction
encodings) or `write_rom_data_word` (swap, so a BE-8 LDR returns
the kernel's intended numerical value). `apply_rom_patches`
dispatches on the bitmap so the version's patch table
(`newton::rom_ver::PATCHES`) can mix code overrides (`MOV PC, LR`)
and data overrides (`gDebugger`, time-base constants) cleanly.

EL2 reads of guest data go through `crate::guest_endian`. Helpers
byteswap on read/write for data PAs and pass-through for ROM-code
PAs — so `handle_und` decoding the faulting instruction at PC
reads the encoding directly, while reads of kernel structs in RAM
swap to recover the kernel's intended numerical value.

The classifier's exact code/data partition is built by two
host-side tools in `tools/` and `scripts/`:

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
   into string tables, vtable data, or literal pools. Writes
   `classify/<hash>/reach.bitmap` — one bit per 32-bit word across
   the 16 MiB ROM+REX aperture, set on every reached PC.

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

`build.rs` selects the bitmap directory by FNV-1a-32 hash of
`rom_bytes || rex_bytes` and stages it into `OUT_DIR`; `guest_mem`
embeds it via `include_bytes!`. A stale bitmap fails the build
rather than booting against the wrong ROM. `scripts/regen-classify.sh`
is the one-stop regen: it runs `classify-symbols.py` if needed,
rebuilds the classifier, and runs it with the curated inputs.

### 8.5 Fault handling

**UND trampoline** (`guest_mem::patch_und_vector`):

- Saves R12 via `MCR p15,0,r12,c13,c0,2` (TPIDRURW) as its first
  instruction, before the `LDR r12, [pc, ...]` that loads the
  save-slot base. `handle_und` then reads `tpidr_el0` into
  `ctx.x[12]`.
- After saving R0 / R1 / LR_und / SPSR_und, executes a short mode
  dance to capture the faulting mode's banked SP / LR: extracts
  `SPSR.M`, ORs with `#0xC0` to keep I/F masked, converts USR
  (0x10) to SYS (0x1F) so the switch-back stays within PL1, does
  `MSR CPSR_c, r1`, `STR sp / lr, [r12, #0x18 / 0x1C]`, then `MSR
  CPSR_c, #0xdb` back to UND. R2 is the scratch register for the
  mode compute; the trampoline saves it to slot +0x14 before use
  and restores it before the HVC.
- Finishes with an SVC-bounce to capture `LR_svc` (for the
  tracer's caller printout) and `HVC #0x10` into EL2.

The trampoline body fits in the reserved
`0x00FFFF00..0x00FFFF60` window. `tracer::in_reserved_range`
covers this region so function-tracer trampoline installation
skips it.

If a fault handler encounters an unresolvable guest address
(VA→PA walk fails, or the computed PA is outside every backed
region and every MMIO window), it halts with full context.

## 9. Open questions (implementation-specific)

Design-level open questions are in `HIGHLEVEL.md` §16. The
implementation-specific items below are mostly resolved; kept here
as a record of how each was answered.

1. **Soft-float target vs hard-float.** Resolved: EL2 builds against
   `aarch64-unknown-none-softfloat`. Guest VFP (CP10/CP11) is
   trapped via `CPTR_EL2.TFP`; the FPA bypass stub
   (`guest_mem.rs::FPA_BYPASS_STUB_OFFSET`) routes FPA-class UNDs
   straight to the kernel's FPE handler.
2. **`compiler-builtins` vs `libgcc`.** Resolved: Rust's
   `compiler-builtins` covers everything we need; no `libgcc`
   dependency.
3. **Panic behavior.** Resolved: `-C panic=abort`, `src/panic.rs`
   prints location + CPU state on mini-UART and halts.
4. **Static stacks.** Resolved: sizes set in `boot.s` per
   exception level / per core; no panics from exhaustion to
   date.
5. **MMU-on handoff.** Resolved: `src/arch/mmu.rs` identity-maps the
   currently-executing code region before enabling stage-1 at
   EL2.
6. **Build reproducibility.** `rust-toolchain.toml` is pinned;
   crate set is small and locked.
7. **License headers.** Resolved: GPL-2.0-or-later (per
   `Cargo.toml`), matching Einstein.

## 10. What this doc does not cover

Deployment workflow beyond the dev loop, production image signing, OTA update, debug/release feature flags, non-Pi ports. All are post-v1.
