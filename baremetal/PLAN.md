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
- All 36 guest tests must pass on every commit
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-58):** iter-57 cut the steady-state trap
rate by ~37× via lazy in-ROM inline stubs for the rotate-LDR
idiom (see below). Boot still reaches steady-state, FB renders
correctly. The dominant trap is no longer the alignment-fault
return; the residue is split between SVC dispatches, cold
alignment-fault PCs we haven't installed yet, and other
peripheral activity. Next steps:

1. **Wire up tablet/pen input.** With the trap budget freed up,
   the next milestone is observable user interaction
   (`peripherals/tablet.rs` already has scaffolding; needs the
   pen-down/move/up event sequence + IRQ raise).
2. **Optional: extend the inline-stub coverage.** A few percent
   of alignment faults still trap to EL2 because they fall in
   one of the rejection paths (writeback/post-index, no-dead-
   scratch, REX or RAM PC). Most of the win is captured; only
   pursue if the residue blocks something.

### Iteration 57: lazy in-ROM inline stub for rotate-LDR — 37× trap-rate cut

iter-56 left the boot in steady-state at ~3.4M hypervisor
traps/sec, dominated 99% by alignment-fault returns at
`ELR=0xffffe4`. Each fault is a full DABT → AArch32
trampoline → HVC → EL2 emulator → ERET round-trip for an SA-
1100 rotate-LDR instruction (`LDR Rt, [Rn]` with unaligned
EA, `result = word_at(Rn & ~3) ROR ((Rn & 3) * 8)`). The ROM
has ~1300 such sites; in steady-state UI rendering a small
hot subset accounts for the bulk of the rate.

#### Fix

New `src/unaligned_inline.rs` lazy-installs an in-ROM inline
stub at each faulting PC the first time we see it, reusing the
shadow_stub mechanism (SBA stub pool at IPA
`0x00E00000..0x00FF_FF00`, B-instruction reach, liveness-aware
scratch picker, icache-flush-both-ranges install). After
install, subsequent executions of that PC run the rotate
natively in AArch32 with no trap:

```
slot 0/1: ADD/SUB sea, Rn, <off>      ; (or 2-step ADD if imm > 0xFF)
slot 2:   AND     ssh, sea, #3
slot 3:   BIC     sea, sea, #3
slot 4:   LDR{c}  Rt, [sea]
slot 5:   LSL     ssh, ssh, #3
slot 6:   MOV{c}  Rt, Rt, ROR ssh
slot 7:   B       orig_pc + 4
```

Aligned EAs see `ssh = 0` → ROR-by-0 = identity, so a single
body handles aligned and unaligned cases. Non-S forms throughout
preserve NZCV. Conditional LDR/MOV match the original cond, so
a cond-fail leaves Rt untouched (matches original LDR's
architectural behaviour).

Eligibility (anything not eligible falls through to the existing
EL2 emulator; partial coverage already wins):
- LDR only (STR-unaligned is implementation-defined; rare).
- Pre-index, no writeback (P=1, W=0).
- No PC operand for Rt, Rn, or Rm.
- Faulting PC < 0x00900000 (Newton ROM/REX, not tracer pool
  or SBA stub pool).
- Liveness analysis finds 2 dead scratches in {R0..R3, R12}
  not in the operand mask.

`shadow_stub` exposes a small public API (`live_regs_at`,
`alloc_stub_slot`, `install_inline_at`,
`read_insn_original_first`) that the new module reuses.

#### Verification

- All 36 guest tests pass on QEMU.
- Cold boot reaches steady-state with no `***` halt; FB dump
  generates correctly (logo + "Newton" caption).
- Trap rate dropped from ~3.4M/sec (iter-56) to ~91K/sec
  average over a 25-second cold boot (~37× reduction). Beacon
  ELR distribution shifted from 99% `0xffffe4` (alignment) to
  ~10% `0xffffe4` plus a spread across SVC dispatches and
  other handlers — i.e. alignment faults are no longer the
  dominant trap source.
- 56 unique PCs installed in 25 s; install rate slows as the
  hot UI-loop set is covered.

#### Out of scope (deferred)

- Inline coverage of LDR with writeback / post-index. Mostly
  unused by the rotate-LDR idiom; would expand the encoder.
- ScratchVA fallback for sites where liveness can't find 2
  dead scratches. Lazy install means partial coverage still
  wins; revisit only if a hot site has no dead scratches.
- RAM-resident faulting PCs (REX or copied code). The B from
  the SBA stub pool can't reach them; would need a parallel
  pool or an HVC-based UDF site (slower, but cheaper than the
  current EL2 emulator round-trip).

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
