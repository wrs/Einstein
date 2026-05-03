# Plan — Drive Newton OS to interactive use

## Status

**Maintenance note (auto-prune):** Each iteration, BEFORE adding a new
iter-N section, prune the old one(s) so PLAN.md stays small. The full
history lives in `git log`. Keep only: this Status block + the most
recent 1-2 iteration sections + the reference sections at the bottom.
Bloated PLAN.md wastes context every read.

**Hard rules** (user directives still in force):

- Hypervisor-side compensation for subpage-AP incompatibility is OFF
  the table (2026-04-29). The fix MUST be a kernel patch.
- Run the *original ROM code*; no workarounds, no deferrals, no
  shortcuts; fix all warnings before each commit.
- All 36 guest tests must pass on every commit that touches hypervisor
  functionality (not merely diagnostics):
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-88):** chase the new wedge after iter-87. Boot
now reaches REP user-space queries (many `REP> (#…). := #…` lines),
then trips an `evt.ex.abt.bus` Throw and the kernel falls through to
`UnhandledException`:

```
Throw #2: name="evt.ex.abt.bus" (r0=0x000afda0) r1=0x0cc6ed0c r2=0x00000000
          caller_lr=0x001f8538 sp=0x0c113388 mode=0x10
*** invariant violation: kernel reached UnhandledException ***
```

A scheduled task census shows several tasks blocked on `PortReceiveSWI`
(NameServer, PSSManager, ChannelMgr) plus pckm spinning in
`GetPartInfoDesc → BeginLoadPackage` and a `scrn` task waiting on a
3-element semaphore-op group whose middle op (`sema[1] dec`) blocks.
Investigation will need to follow the bus-abort path —
`evt.ex.abt.bus` typically means the kernel saw a DABT it couldn't
satisfy, so the ResolveFault/Remember chain is the natural starting
point.

### Iteration 87: relocate kernel-patch stubs out of the UND trampoline window

#### Symptom

After iter-86, boot reaches REP `TimeInSeconds()` then wedges:

```
*** unrecognised UND: insn=0xe1400170 at PC=0xffff54 SPSR_und=0x80000110
  src_mode=0x10 (USR) … SP_und=0xc006000 LR_und=0xffff58
```

`0xffff54` is the UND trampoline's `hvc #UND_TAG`. handle_und's
catch-all fires because USR mode itself ran the HVC (HVC at PL0 → UND).

#### Root cause

The kernel-patch native-primitive stubs `DEBUG_STR_STUB_PC=0xffff30`,
`DEBUGGER_STUB_PC=0xffff38`, `FTIME_STUB_PC=0xffff40`, and
`FDATE_STUB_PC=0xffff60` (in `src/rom_patches.rs`) lived **inside**
the region `patch_und_vector` writes (UND trampoline at
`0xffff00..0xffff60`, SBA pre-fault stub at `0xffff60..0xffff80`).

Install order: `apply_717006_patches` writes the stubs first, then
`patch_und_vector` overwrites them. The kernel-patched BL/B sites
at `0x89b80` (FTimeInSeconds), `0x8A8A8` (FDate), `0x38ce6c/70`
(DebugStr/Debugger) still pointed at the now-clobbered stub
addresses. When REP eventually called `TimeInSeconds()` →
`FTimeInSeconds` → patched `b 0xffff40`, USR jumped into the middle
of the trampoline body (base+16: `ldr r2, [r12, #0x14]`) and ran
forward through `mov r0, lr` (writing LR_usr=0x89b74 to
`[r12+8]`=`0x0cd7c954` — visible in the wedge stack dump), the
two `msr cpsr_c` insns (no-op from USR), and finally the trampoline's
`hvc #0x10` at `0xffff54` — UND from USR, trampoline runs, HVC fires,
handle_und's catch-all halts.

The `LR_usr=0x89b74` and `R0=0x89b74` in the wedge-time register
dump are the smoking gun: that's `mov r0, lr` from trampoline
base+18 having executed in USR, with LR still set by FTimeInSeconds's
`bl 0x1c094b0` at `0x89b70` because the patched stub never ran the
real FTimeInSeconds work and never returned through any other BL.

The bug was latent before iter-86 because earlier ROM init avoided
calling `FTimeInSeconds` / native-primitive paths; REP user-space
boot is the first call site that exercises them.

#### Fix

Relocate all four stub PCs to the gap between
`RESOLVE_FAULT_WRAPPER` (ends at `0x00FF_FE5C`) and `FPA_BYPASS_STUB`
(starts at `0x00FF_FEC0`):

```
DEBUG_STR_STUB_PC = 0x00FF_FE60   // 2 words / 8 B
DEBUGGER_STUB_PC  = 0x00FF_FE68   // 2 words / 8 B
FTIME_STUB_PC     = 0x00FF_FE70   // 5 words / 20 B
FDATE_STUB_PC     = 0x00FF_FE84   // 5 words / 20 B
```

56 B used, well clear of the 64 B FPA bypass and trampoline ranges.

#### Verification

- Boot now sails past `TimeInSeconds()` through REP user-space (many
  `REP>` query lines) before tripping a separate `evt.ex.abt.bus`
  bus-fault wedge — that's iter-88's territory.
- 36/36 guest tests pass.

#### Diagnostics added (kept)

- `record_und_history` / `dump_und_history` in `src/trap.rs` — a
  32-entry rolling buffer of recent UND faults (PC, insn, mode, sp,
  lr_usr). Dumped on the catch-all halt and instrumental in finding
  this bug.
- `return_to_guest_from_und` halts loudly if `elr` lands inside
  `0xffff00..0xffff60` (UND trampoline body) or `0xffec0..0xffefc`
  (FPA bypass) with USR-mode SPSR — those are never legitimate
  ERET targets. Caught by exclusion: SBA_POST_TRAMP at `0xffff80`
  and UND_RETURN_STUB at `0xffffe4` are intentionally allowed.
- USR-stack and JT-thunk dump in handle_und's catch-all — reads via
  stage-1 walk (`guest_mem::translate_va`) so kernel VAs resolve.

---

### Iteration 86: skip the per-test rebuild via semihost-load

#### Problem

`run-all.sh` ran `cargo build --release` once per test (36 times)
because each test's `.bin` was embedded into the hypervisor via
`include_bytes!(env!("NH_GUEST_TEST_PATH"))`. Each rebuild was a
relink (LTO) — ~10s each, ~5 min total wall.

#### Fix

Two delivery modes for the test binary, selected by the value of
`NH_GUEST_TEST`:

- **embed** (`NH_GUEST_TEST=path/to/test.bin`): compile-time
  `include_bytes!` — current behavior, fast for iterating on a
  fixed test where cargo's incremental build only re-emits one
  object + relinks.

- **semihost-load** (`NH_GUEST_TEST=1`): build the hypervisor as
  a generic test image with no embedded bin; load the test
  binary at boot via Arm semihosting. The path is passed in
  QEMU's `-semihosting-config arg=<path>`. iter-86 added
  `load_test_bin_via_semihosting` in `src/guest_mem.rs` that
  calls `SYS_GET_CMDLINE` → `SYS_OPEN` → `SYS_FLEN` → `SYS_READ`
  to fill `GUEST_TEST_BIN_BUF` before stage-2 setup.

`build.rs` sets the `nh_guest_test_embed` / `nh_guest_test_semihost`
sub-cfgs (both also set `nh_guest_test`); `guest_mem.rs` and the
loader pick the right path.

`run-test.sh` and `run-all.sh` default to semihost-load. Set
`NH_GUEST_TEST_EMBED=1` to opt into the legacy embed mode.

#### Result

`run-all.sh` wall time: **~5 minutes → 6.7 seconds**. 36/36 tests
pass under both modes.

---


<!-- Older iteration retrospectives (iter-77 and earlier) live in
     `git log` per the auto-prune maintenance note. -->


## Workflow per stop

1. Capture verify-mmu output (`fix_stage1_xn_bits` ratchets per
   alias-onset). Each alias is a `(PA, VA1, VA2)` tuple.
2. Identify the kernel-side write that creates each alias by
   instrumenting the relevant L2-write entry point with an HVC probe.
3. Cross-reference with Einstein (`build/NewtonProbe baremetal/roms/
   newton.rom _Data_/Einstein.rex 30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — `src/peripherals/*.rs`, `src/trap.rs`.
   - **Einstein behavioural quirk** — port the matching logic.
   - **ROM patch** — `src/rom_patches.rs`. Only when no other layer can
     host the fix.
5. Re-run, observe alias count, repeat until zero.

## Tools

### Hosts

- **QEMU raspi3b** (default; `cargo run --release`) — fast, BCM2835
  VIC, AArch32↔AArch64 banking quirks documented in `docs/QEMU_BUGS.md`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — `scripts/fvp <elf>`. Accurate
  reference: GICv3, generic timer + cache model exact. Build with
  `--no-default-features --features platform-fvp-base`.

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`. Patches every
  `scripts/classify-out/code-symbols.txt` entry with HVC trampoline.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor function-
  entry traces.
- **`build/NewtonProbe`** — Einstein-as-oracle.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s from `trap_irq`.
- **Framebuffer PNG dumps** — `/tmp/newton-fb/NNNNN.png` after
  `screen::blit`.

### Debugging

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). Helpers `bg
  <addr>`, `bp <addr>`, `tt N`, `guest-state`.
- **DABT/PABT DIAG HVCs** at ROM offsets `0x10` / `0x0C`.
- **Software-reset canaries** — BootOS / PowerOffAndReboot / Reboot.

### Reference

- `scripts/disasm-out/rom.dis` — symbol-annotated ROM+REx disassembly.
- `docs/DISASM.md` (incl. "Jump-table aliasing — DON'T mistake the
  thunk for the body").
- `docs/NEWTON_INTERNALS.md` — APCS, ClassInfo dispatch, ROM patch
  table 0x01A00000..0x01C20000.
- `docs/QEMU_BUGS.md` — raspi3b AArch64↔AArch32 quirks.
- `docs/STRUCTURES.md` — kernel struct layouts (TScheduler, TTask,
  TStackManager, end-to-end page allocation).
- `docs/peripherals.md` — peripheral implementations.
- `probe/FINDINGS.md` — golden record from a fully-booted Newton.

### Tests

`baremetal/guest-tests/scripts/run-all.sh` runs the 36 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011 and runs the verify-mmu
  alias detector; UND-vector trampoline; DABT/PABT DIAG patches.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / SBA /
  tracer / canary / probe tags); `handle_page_get_probe`,
  `handle_remember_entry_probe_with` (with the new aliasing tracker);
  `handle_data_abort` with kernel-DABT forwarding for lazy stack
  growth.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, TPC, TPU, IMO, FMO, AMO,
  DC); CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3.
- `src/banked.rs` — AArch32 banked-register access from EL2 (Table
  D1-79).
- `src/rom_patches.rs` — Einstein word-write patches; HVC injection
  helpers; canaries; ResolveFault wrapper; `PAGE_GET_PROBE` patch.
- `src/peripherals/*` — Newton driver / native-primitive surface.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-*.bin`.
- `src/tracer.rs` — function-level tracer.
- `src/guest_bp.rs` — `bp <addr>` for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `guest-tests/tests/` — 36 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 36 tests must pass.

## Non-goals

- Real screen emulation beyond the framebuffer dump — no compositor,
  no pen input.
- Package loading — needs a solution for embedded native code.

## Diagnostic scaffolding (active)

- `verify-mmu` in `fix_stage1_xn_bits` — ratchet-logs subpage-AP
  heterogeneity and per-alias-onset `(PA, VA1, VA2)` tuples.
- `handle_page_get_probe` (PAGE_GET_PROBE_HVC_IMM=0x53) on
  `0x00258EFC` — page-allocator return logger + dup detector.
- `handle_remember_entry_probe_with` (REMEMBER_PROBE_HVC_IMM=0x46)
  on `0x00258E0C` — Remember-side per-PA → first-VA aliasing tracker
  (added to the existing L1-lazy-grow probe).
- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.

Pull these once the boot quiesces.
