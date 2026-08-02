# Newton Hypervisor — Implementation Notes

**Scope:** language, build system, source structure, tooling and
testing. The architecture is in [`HIGHLEVEL.md`](./HIGHLEVEL.md); read
that first. Build/run instructions for a user are in
[`README.md`](./README.md); current state and remaining work in
[`PLAN.md`](./PLAN.md).

## 1. Language and dependencies

Pure Rust, `no_std`, target `aarch64-unknown-none-softfloat`. The
hypervisor, the Newton peripheral models and the bare-metal Pi drivers
are one crate. Einstein's C++ is a *reading reference* — it is neither
compiled nor linked.

Why Rust for all of it:

- Memory safety matters disproportionately in a hypervisor: one MMIO
  or page-table bug is a guest escape.
- Enum + exhaustive `match` fits the CP15 shim's
  `(op1, CRn, CRm, op2, direction)` decode and the VIC / DMA register
  dispatch, so an unhandled tuple is a compile error rather than a
  silent default.
- Stage-2 descriptor layouts as `repr(C)` structs and typed constants
  are harder to silently corrupt than C bitfields.
- Stable inline assembly covers `MSR`/`MRS`/`ERET` and the vector
  prologues without a separate assembler step (the two `.s` files in
  `src/arch/` are `global_asm!`-included).

Why Einstein is not linked: the simple peripherals (`TFlash`,
`TDMAManager`) are 30–60 lines of real logic once the save/restore and
stdio plumbing is stripped, and the one with mass
(`TInterruptManager`) is mostly a `TThread` / `clock_gettime`
scheduling wrapper around a small state machine — none of which
applies to a trap-driven hypervisor that pumps from trap handlers.
Linking would have meant stubbing pthread, stdio, exceptions, RTTI and
mmap, and maintaining an FFI boundary, to import code that gets
rewritten anyway. Einstein stays authoritative on register bit
semantics; [`docs/peripherals.md`](docs/peripherals.md) records what
each peripheral does with pointers into Einstein's files, and when the
doc and Einstein's C++ disagree, Einstein wins and the doc is
corrected.

**Dependencies.** No helper crates for system registers, MMIO, bit
flags or panic handling — all hand-rolled with inline `asm!` and plain
constants, which keeps the unsafe surface visible at each use site.
The only path dependencies are `newton-objects` (in-tree; NS `Ref` tag
decoding, pulled in by the `diag` feature) and a vendored
`embedded-sdmmc` (`vendor/`, local changes listed in `VENDOR.md`).
Nothing pulls in `alloc` or `std`; there is no global allocator, and
peripheral state lives in static storage constructed once at startup.

## 2. Build

Single-crate Cargo build with compile-time selection on independent
feature axes: exactly one `platform-*`, exactly one `rom-*`, and at
most one backend per I/O seam (`host-io-*`, `flash-persist-*`,
`input-*`, `audio-*`). The full table is in
[`README.md`](./README.md#cargo-features); `Cargo.toml` is
authoritative.

`build.rs` does the compile-time resolution:

1. **Platform.** Instantiates `linker.ld.in` with the platform's load
   address (raspi3b `0x80000`, FVP `0x80000000`) into `OUT_DIR` and
   links against the result — one script, one placeholder, no
   per-platform copies.
2. **ROM version.** `resolve_rom_version()` maps the `rom-*` feature to
   its build inputs (ROM/REx paths, symbol tables, flash filename) and
   its `src/newton/rom_ver/` constants module.
3. **Classifier bitmap.** Selects `classify/<hash>/` by FNV-1a-32 of
   `rom_bytes || rex_bytes` and stages `reach.bitmap` into `OUT_DIR`
   for `include_bytes!`. A stale bitmap fails the build rather than
   booting against the wrong ROM.
4. **Backend cfgs.** Resolves each axis to a `cfg(nh_*)` — `nh_diag`,
   `nh_semihost`, `nh_real_hw`, `nh_host_io_*`, `nh_flash_persist_*`,
   `nh_input_*`, `nh_audio_*` — and registers them with
   `cargo::rustc-check-cfg`. Source reads the cfgs, never the features;
   that is what lets `no-semihost` stay a negative feature (Cargo
   features are additive) while source reads the positive
   `nh_semihost`. `validate_feature_matrix()` rejects unsupported
   combinations with a named message instead of a deep compile error.
5. **Guest-test mode.** Reads `NH_GUEST_TEST`; when set, guest memory
   is populated with a test binary instead of the Newton ROM.

Output is a single flat `kernel8.img` (via `objcopy`) plus an ELF with
DWARF for gdb. No CMake, no external C toolchain for the hypervisor
itself; `arm-none-eabi-*` is needed only to assemble the guest tests.

## 3. Source structure

`src/` is one crate in six layer directories. The dependency direction
is `arch ← hv ← newton`, with `peripherals`, `host` and `diag` as
described below; `scripts/check-layering.sh` enforces it and its header
comment is the authoritative statement of the rules and their
sanctioned exceptions.

```
src/
  main.rs        kmain: the boot narrative — MMU, backings, ROM load,
                 stage-2, vectors, peripherals, timer; ERET to guest
  panic.rs       panic handler → loud halt
  arch/          pure AArch64/AArch32 mechanism, zero upward deps:
                 boot.s, vectors.s, trap_context, mmu, cpu, banked
                 (AArch32 banked regs from EL2), arm_decode,
                 aarch32_emit, slim_isr
  hv/            generic hypervisor core: stage2, guest (ERET to
                 AArch32 EL1), guest_mem, guest_endian, be8, layout
                 (the region + MMIO-window manifest), mmio router,
                 timer, snapshot, hvc_imm, hooks (the GuestOs seam),
                 trap/{mod,dabt,und,cp15,hvc}
  newton/        Newton-specific: os (GuestOs impls, incl.
                 fix_stage1_xn_bits), loader (ROM load + selective
                 byteswap + CP15 rewrite), rom_patches, probes,
                 inline_patch (stub + scratch pools, liveness walker),
                 guest_trampolines, unaligned[_inline], rom_ver/
  peripherals/   guest device models (Rust ports of Einstein's)
  host/          host drivers + backends: console, macros, platform/
                 (raspi3b, fvp_base, gicv3), mailbox, host_dma, sd/,
                 usb/, display/, audio/, input/, host_io/,
                 flash_persist/
  diag/          diagnostics layer (feature `diag`): trap_diag,
                 trap_hist, task_dump, heap_check, rep_print, symbols,
                 guest_bp, tracer, tarmac, diag_util
```

The seams that matter:

- **hv → newton** crosses only at `src/hv/hooks.rs`: the `GuestOs`
  trait with `type ActiveGuest = newton::NewtonOs`. Guest-OS behaviour
  (SCTLR/TTBR rituals, probe HVCs, trap-tail pumps, UND resume) plugs
  in there instead of being called from generic trap code.
- **hv → peripherals** crosses only at `src/hv/mmio.rs`, through a
  closed `PeriphId` enum, so a forgotten model is a compile error
  rather than a missing registration.
- **host** is below `main.rs` and is not imported by guest-facing
  layers, except `host::platform` (the board API, importable
  everywhere) and two sanctioned upward edges: event injection into
  `peripherals::vic` / the pen queue, and reads through
  `hv::guest_mem` / `guest_endian`.
- **diag** is importable from anywhere and compiles to no-op stubs
  with the identical surface when the `diag` feature is off.

Lints: `#![deny(unsafe_op_in_unsafe_fn)]` crate-wide;
`scripts/check-rom-addrs.sh` keeps ROM-space hex literals confined to
`src/newton/rom_ver/` plus an explicit allowlist.

## 4. Peripheral module shape

Each peripheral is a module under `src/peripherals/` holding its
register state in a static, plus the dispatch functions its contract
requires — `MmioPeripheral` (`owns`, `read`, `write`, `peek_word`) for
the window-mapped devices, `NativeDriver` (`DRIVER_ID`, `handle`) for
the CP10/CP11 native primitives. Shared `halt_unknown_*` helpers give
every model the same context dump and "extend file X" hint. State
machines stay platform-neutral; routing lives in `src/hv/mmio.rs`, and
the Rust type system is the whole contract — no FFI, no opaque handles.

`peripherals::guest_access` provides the read/write-guest-memory
helpers (VA-first with PA fallback, loud halt on failure) that the
models use, so a failed guest read is treated as an emulation bug
rather than swallowed.

## 5. Classifier pipeline

The code/data partition that drives the selective byteswap, the
inline-stub "real code" definition, and the tracer's function list is
built by two host-side tools:

1. **`scripts/classify-symbols.py`** partitions every entry in the
   ROM's demangled symbol table into `code` / `data` / `drop` with an
   ordered ruleset: name prefixes (`g[A-Z]` → data, `F[A-Z]` → code,
   `::` or `(` → code, symbol/table prefixes → data), address-range
   rules (exception vectors, early-boot text), and a first-word-shape
   fallback. Outliers go in explicit exception sets rather than
   contorted rules. Outputs `classify-out/code-symbols.txt` (also the
   tracer's address list) and `classify-out/data-ranges.txt`.
2. **`tools/classify-rom`** takes the code list as walker roots and the
   data ranges as termination boundaries, and walks every basic block
   with a full ARM decoder — recognising `B`/`BL`/`Bcc`/`LDR pc`/`BX`/
   `LDM`-with-pc/`SWI`/`UDF` as terminators, `MOV LR, PC` + PC-write as
   the manual-BL idiom, and conditional data-processing writes to PC as
   jump-table dispatch. It additionally seeds from the REx header's
   `fdrv`/`FDRV`/`pkgl` entry tables (Einstein.rex has no symbol file)
   and from the constructor vtable-install pattern (`LDR Rt, [pc,#imm]`
   followed by `STR Rt, [Rn,#0]`, chased to the vtable and enumerated).
   Output is `classify/<hash>/reach.bitmap`, one bit per 32-bit word
   across the 16 MiB ROM+REx aperture.

`scripts/regen-classify.sh [ver]` is the one-stop regeneration (default
717006): it runs `classify-symbols.py`, rebuilds the classifier, and
runs it with the curated inputs. `scripts/dump-data-regions.py`
refreshes `code-regions.txt`, which is what the bitmap-first triage in
[`CLAUDE.md`](CLAUDE.md) greps.

## 6. ROM patching and fault-path stubs

All ROM-word installation goes through one API
(`rom_patches::install_patch`, on top of the encoders in
`src/arch/aarch32_emit.rs`): it verifies
the original word (loud halt on mismatch, with an explicit opt-out for
genuinely optional probes), records the original in the side table, and
publishes the line to the I-cache. Branch/literal encoders live
alongside it with compile-time asserts checking their output against
known-good encodings.

Writes into the ROM backing store pick their endianness by role:
`guest_mem::write_rom_code_word` (verbatim, for instruction encodings)
or `write_rom_data_word` (swapped, so a BE-8 `LDR` reads the intended
numerical value). The patch tables in `newton::rom_ver` therefore mix
code overrides and data overrides cleanly.

`src/newton/guest_trampolines.rs` owns the hand-assembled AArch32 that
runs guest-side before entering EL2:

- **UND trampoline.** Saves R12 via `MCR p15,0,r12,c13,c0,2`
  (TPIDRURW) as its first instruction, then R0/R1/LR_und/SPSR_und,
  then does a short mode dance — extract `SPSR.M`, keep I/F masked,
  convert USR to SYS so the switch stays within PL1 — to capture the
  faulting mode's banked SP/LR, and finally HVCs into EL2 (via an SVC
  bounce that captures `LR_svc` for the tracer's caller print).
- **DABT fast trampoline.** Checks `DFSR.FS == 0b0001` (alignment,
  unique in the FS encoding space) and HVCs straight to the unaligned
  emulator; anything else falls through to the general DABT path,
  which forwards the forwardable fault classes to the kernel's own
  `DataAbortHandler`.

If a fault handler meets an address it cannot resolve — VA→PA walk
fails, or the PA is outside every backed region and every MMIO
window — it halts with full context rather than guessing.

## 7. Testing

Two tiers, plus the ROM boot itself as the end-to-end canary. There is
no host-side `cargo test` tier: all runtime verification happens in the
guest tests and the boot.

### 7.1 Structural — build matrix and lints

`scripts/check-matrix.sh` runs the two structure lints
(`check-layering.sh` import discipline, `check-rom-addrs.sh` ROM-address
containment) and then `cargo check`s all 18 supported build
combinations — default, the no-diag variants, both platforms,
`rom-710031`, the four `pi-bare-metal*` aggregates, trace/probe/log
combos, and the guest-test cfg — in one shared target dir, printing a
PASS/FAIL line per combo (~10 s warm). Run it after touching
`build.rs`, feature gates, or any cfg-dispatched backend. It is wired
into `run-all.sh` behind `CHECK_MATRIX=1`.

Cross-axis constraints are expressed as Cargo feature dependencies
(hardware backends imply `platform-raspi3b`), so a forbidden
`--features` set fails `build.rs`'s validator with a named message.

### 7.2 Behavioural — guest tests

`guest-tests/` holds 38 small AArch32 binaries linked against a shared
runtime (`common/test_runtime.S`) that sets up SVC/IRQ/FIQ stacks,
installs an IRQ handler, and exposes the HVC protocol the hypervisor
understands (`HVC #0x10` putchar, `#0x12` PASS, `#0x13` FAIL, `#0x14`
mark; the full ABI is `common/hvc_abi.S` and
`guest-tests/README.md`). The hypervisor is built with `NH_GUEST_TEST`
set so guest memory is populated with the test instead of the ROM.
Each test drives one trap path or peripheral surface end to end — MMU,
stage-2, trap dispatch, Rust dispatcher, peripheral state machine — and
reports pass/fail on the UART.

```
guest-tests/scripts/build-tests.sh              # build everything
guest-tests/scripts/run-test.sh test_vic        # one test
guest-tests/scripts/run-all.sh                  # all 38 on QEMU
guest-tests/scripts/run-all.sh --platform fvp   # all 38 on FVP
```

Both platforms must stay green on every commit that touches hypervisor
functionality. Newton-ROM probe iterations (a new HVC immediate in
`rom_patches.rs` + a dispatch arm in `trap/hvc.rs` + a handler body in
`probes.rs`) can skip the run — the test ELFs don't contain the ROM,
so probe-only changes can't regress them.

### 7.3 ROM boot canary

The Newton boot is the regression target of last resort: the
hypervisor must reach the Welcome UI without an unexpected fault.
`scripts/boot-check.sh` automates it — boots under QEMU with the log
redirected, polls for the milestone markers, and kills QEMU the moment
they appear (`--cold` clears snapshots first). Use it rather than
hand-rolled `timeout … ; pkill` recipes: QEMU defers SIGTERM while the
guest is busy (see [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md)).

### 7.4 What has no emulated coverage

USB, real SD timing, HDMI display and audio only exist on real
hardware. They are validated by flashing a Pi Zero 2 W; there is no
hardware-in-the-loop CI.

## 8. Unsafe discipline

`unsafe` is unavoidable at the bottom (vectors, page-table descriptors,
MMIO) and is contained:

- Every `unsafe` block carries a comment stating the invariant the
  caller is asserting.
- No `unsafe` in business logic — CP15 shim, VIC state machine, trap
  dispatch — only in the modules that touch hardware or guest backing
  stores.
- `#![deny(unsafe_op_in_unsafe_fn)]` crate-wide.
- Every new `unsafe` block gets reviewed, not just changes to existing
  ones.

## 9. Resolved implementation decisions

- **Soft-float EL2.** Built against
  `aarch64-unknown-none-softfloat`; the guest's VFP/FPA accesses trap
  via `CPTR_EL2.TFP`, and FPA-class UNDs route to the kernel's own FPE
  handler through the ROM-tail bypass stub.
- **`compiler-builtins`,** not `libgcc`.
- **`panic = "abort"`;** `src/panic.rs` prints location + CPU state on
  the console and halts.
- **Static stacks** sized in `boot.s` per exception level and core,
  with a canary at the EL2 stack limit checked on the IRQ and halt
  paths.
- **MMU-on handoff:** `arch::mmu` identity-maps the executing code
  region before enabling stage-1 at EL2.
- **Toolchain** pinned in `rust-toolchain.toml`; the crate set is small
  and locked.
- **License:** GPL-2.0-or-later (per `Cargo.toml`), matching Einstein,
  whose peripheral state machines this code derives from.

## 10. Not covered here

Deployment beyond the dev loop, image signing, OTA update, non-Pi
ports.
