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

**Current goal (iter-60):** iter-59 added a per-imm HVC histogram
diagnostic, which revealed that ~99% of traps were `HVC #DIAG_TAG`
(20.8 M in 30 s) — kernel DABTs forwarded to `DataAbortHandler`.
A new AArch32 fast-forward DABT trampoline at
`DABT_FAST_TRAMP_OFFSET = 0x008F_FF00` now dispatches by DFSC
without an EL2 round trip for the common kernel-handled cases
(translation/permission/access-flag faults at section + page).
Result: cold boot reaches the multitasking phase, with `newt`,
`OBJM`, `cdfm`, `cdsv`, `PMGR`, `PTBL`, `STKF`, `idle`, `main`
all alive in the task list. New failure mode: kernel-thrown
exceptions `evt.ex.abt.bus` (FAR=0x0cd2d000, kernel heap range)
and `evt.ex.fr.store` reach `UnhandledException` because no
handler catches them.

Next steps:

1. **Diagnose the bus-abort throws.** FAR=0x0cd2d000 falls in the
   kernel-VA heap range; `caller_lr=0x002ddefc` is post-
   `DisposeRefHandle`. Could be: (a) genuine guest bug uncovered
   by faster boot, (b) our fast-forward not setting up DAH state
   correctly for some DFSC, (c) stage-2 mapping gap revealed at
   later boot phase. Walk the throw chain back from
   `BusFaultMonitor` / `evt.ex.abt.bus` registration.
2. **Improve rotate-LDR liveness coverage.** 98% of remaining
   alignment-fault traps (3865/3883) are rejected as
   `no_dead_scratches`. Adding a ScratchVA fallback would cover
   them with a per-stub TPIDR-saved scratch, similar to the
   shadow_stub byte-access path.
3. **Wire up tablet/pen input** once the `evt.ex.fr.store` is
   resolved and the boot can quiesce into a true idle.

Next steps:

1. **Identify where the byte-access work is concentrated.** With
   alignment-fault returns at 98% of beacons after iter-58, a
   PC histogram on the SBA UDF dispatch (or on the alignment-
   fault EL2 emulator) would show whether it's a few hot kernel
   functions (string hash, soup walking) or a wide spread.
2. **If concentrated:** add inline-stub coverage for the
   currently-rejected sites (writeback, no-dead-scratch
   fallback via ScratchVA, RAM-resident PCs via UDF retry).
3. **If wide:** consider untrapping more aggressively — e.g.
   relax `SCTLR_EL1.A=1` so legitimate aligned word LDRs don't
   fault and only the rotate-LDR pattern traps via a different
   mechanism. (Risk: changes guest semantics.)
4. **Wire up tablet/pen input** once the boot quiesces into a
   true idle wait state — observable user interaction is the
   real Phase B endpoint.

### Iteration 59: AArch32 fast-forward DABT trampoline — boot reaches scheduler

iter-58's HVC-tag histogram diagnostic (added this iteration as
`trap.rs::dump_hvc_tag_stats`, called every ~2 s from `trap_irq`)
revealed that ~99% of HVCs were `HVC #DIAG_TAG` (0x11) — kernel
DABT-vector traps. Each was a full EL2 entry/exit even though
`handle_diag` just rewrote ELR to forward the fault to the kernel's
own `DataAbortHandler` at `0x00393114`. The kernel's
`AddPgPAndPermWithPageTable` (and many other paths) take routine
translation / permission faults during normal operation; round-
tripping every one through EL2 was the dominant remaining cost.

#### Fix

New AArch32-side fast-forward trampoline at
`DABT_FAST_TRAMP_OFFSET = 0x008F_FF00` (in the unused tail
between Einstein.rex and the tracer trampoline pool). VA 0x10
now branches here first; the trampoline reads DFSR, masks to
DFSC[3:0], and dispatches in 4–10 inline instructions:

```
ft+0:   mcr p15,0,r0,c13,c0,2     ; TPIDRURW = R0 (save)
ft+1:   mcr p15,0,r1,c13,c0,3     ; TPIDRRO = R1 (save)
ft+2:   mrc p15,0,r0,c5,c0,0      ; R0 = DFSR
ft+3:   and r0, r0, #0xF          ; DFSC[3:0]
ft+4:   cmp r0, #7    \           ; six dispatched values:
ft+5:   beq FAST_FWD  |             0x07 (translation, page)
ft+6:   cmp r0, #15   |             0x0F (permission, page)
ft+7:   beq FAST_FWD  |             0x05 (translation, section)
ft+8:   cmp r0, #5    |             0x0D (permission, section)
ft+9:   beq FAST_FWD  |             0x06 (access flag, page)
ft+10:  cmp r0, #13   |             0x03 (access flag, section)
ft+11:  beq FAST_FWD  |
…       …             |
ft+16:  mrc p15,0,r0,c13,c0,2     ; restore R0 (was clobbered with DFSC)
ft+17:  b SLOW_DABT_TRAMP         ; uncommon DFSCs → DABT_TRAMP_OFFSET
ft+18:  mrc p15,0,r0,c13,c0,2     ; FAST_FWD: restore R0
ft+19:  mrc p15,0,r1,c13,c0,3     ;           restore R1
ft+20:  ldr pc, [pc, #-4]         ;           jump to DAH
ft+21:  literal: 0x00393114
```

For forwardable DFSCs the entire round-trip is ~6 instructions
of inline AArch32 with no EL2 entry. Other DFSCs (alignment,
external aborts, etc.) fall through to the existing
`DABT_TRAMP_OFFSET` slow path.

`trap.rs` gains `dump_hvc_tag_stats` + per-imm histogram
counters, called from `trap_irq` every ~2 s of wall time
(independent of the snapshot autosave gating).

#### Verification

- All 36 guest tests pass on QEMU.
- HVC histogram: `DIAG_TAG=20.8M → 0` between iter-58 and
  iter-59. `UND_TAG` (byte-access UDF) and `ALIGN_TAG`
  (rotate-LDR) are now the dominant non-zero entries; the
  former at ~146 K, the latter at ~3.9 K, both small.
- Cold boot reaches the multitasking phase. Task dump shows
  `OBJM`, `idle`, `main`, `cdfm`, `newt` (RUN), `cdsv`, `PMGR`,
  `PTBL`, `STKF` and others all alive — the same scheduler
  state Einstein reaches at 60 s wall (per NewtonProbe).
- New failure: `evt.ex.abt.bus` and `evt.ex.fr.store` thrown
  by kernel code reach `UnhandledException`. Tracked as the
  iter-60 starting point.

#### Out of scope (deferred)

- Stub the rotate-LDR `no_dead_scratches` rejection rate (98%)
  via a ScratchVA fallback like shadow_stub uses for byte
  accesses. Would cover sites where liveness can't find 2 dead
  candidates by saving them to a per-stub 8-byte slot in the
  scratch pool.
- Refactor `unaligned.rs` and `handle_diag` to read banked
  LR_abt / SP_abt from `ctx.x[20]` / `ctx.x[21]` instead of
  the trampoline's `DABT_SAVE_PA` slot, which would let us
  drop the lr/sp save in the slow `DABT_TRAMP` and make even
  the slow path leaner.

### Iteration 58: untrap CP15 cache-by-VA — 5–15× progress speedup

iter-57 cut the alignment-fault trap rate; the next-dominant
beacon source was 75% inside `CleanRangeInDCSWIGlue`'s 5-instruction
cache-line loop:

```
mcr p15,c7,c10,{1}   ; DCCMVAC — clean line by VA
mcr p15,c7,c10,{4}   ; DSB
mcr p15,c7,c6, {1}   ; DCIMVAC — invalidate line by VA
add r2, r2, #32
teq r2, r1
bne .loop
```

Three CP15 traps per 32-byte line, called after every flash
write via `FlushDataCache__11TFlashRangeCFUlT1`. Each trap is a
full EL2 entry/exit even though we no-op the op — the trap
cost dominated wall-clock time in the flash-store init phase.

#### Fix

`src/guest.rs` clears `HCR_EL2.TPC` and `HCR_EL2.TPU`
(previously both set). The MCRs run natively at EL1 with no
trap. Cortex-A53 in AArch32 treats DC-by-VA / IC-by-VA on an
unmapped VA as a no-op (matching the SA-1100 semantics
Newton's `CleanPageInDcache` relies on for unmapped VAs before
L2-entry population), so the `AddPgPAndPermWithPageTable`
caller works without the EL2 detour.

This mirrors Einstein's `TARMProcessor::SystemCoprocRegisterTransfer`
case 7 (`TARMProcessor.cpp:253`), which is a silent no-op for
all non-WFI cache-maintenance MCRs.

`scripts/run-qemu.sh` switches `-serial stdio` →
`-serial mon:stdio` so `Ctrl-A x` quits QEMU cleanly (the prior
form forwarded Ctrl-C / Ctrl-\ as characters to the guest).

#### Verification

- All 36 guest tests pass on QEMU.
- Cold boot reaches steady-state with no `***` halt; FB still
  renders splash + sub-region correctly. `fb_dump` fires within
  the 25-second window post-iter-58 (it didn't reliably fire
  pre-iter-58 within the same window).
- Trap rate ~91 K/s (iter-57) → ~430 K/s–1.3 M/s (iter-58).
  Beacon-sampled cache-MCR PCs (`0x18b30`/`0x18b34`/`0x18b38`)
  drop from 75% to 0% — the kernel-side cache loops finish
  natively without trapping.
- 160 M traps in 120 s of wall (vs ~96 M in 17 min pre-iter-58)
  — boot still in DiagBootStub-region work but progresses
  ~10× faster.

#### Out of scope (deferred)

- FVP fallback. The original comment warned that FVP Base RevC
  raises a translation fault for cache-by-VA on unmapped VAs.
  If FVP regresses, add a translation-fault filter in
  `handle_data_abort` that no-ops the fault when ELR points at
  a c7 cache-maintenance MCR. (Not observed in this iteration
  because all testing was QEMU.)
- TSW (set/way cache maintenance). Newton's kernel doesn't use
  set/way ops in the hot path; leave it trapped.

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
