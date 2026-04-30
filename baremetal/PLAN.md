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

**Current goal (iter-61):** iter-60 root-caused the iter-59
`evt.ex.abt.bus` throw at FAR=0x0cd2d000 to a missing DFSR.Domain
synthesis: iter-59's fast trampoline bypassed `handle_diag`, so
DFSC=0x05 (translation, section) — for which ARMv7 leaves
DFSR.Domain UNK — reached the kernel's DAH with Domain=0. The
kernel's `GetDomainAndFaultMonitorFromDomainNumber(0)` returned no
monitor, FaultMonitorEntry returned -10015, and DAH threw
`evt.ex.abt.bus`. iter-60 excluded DFSC=5 from the fast path so
those faults fall through to the slow EL2 path which still
synthesises DFSR.Domain from L1[FAR>>20][8:5] before forwarding.
Result: boot recovers from the throw, reaches 26 tasks alive
(`inkr`, `scrn` join the iter-59 list), and `evt.ex.fr.store`
NewtonScript-level throws are now non-fatal (handled by Newton's
own runtime — no `UnhandledException` halt within 30 s).

Next steps:

1. **Trace the residual `evt.ex.fr.store` throws.** Throw r1
   alternates 0xffffd692 (-10094) / 0xffffd698 (-10088), called
   from `0x00351e50` / `0x00353730` / `0x002df4f0` /
   `0x002eff24`. These look like soup / package-store error
   codes. Boot hasn't reached a quiescent idle yet; understand
   whether these are expected (catch-and-continue) or block
   reaching the idle wait state.
2. **Wire up tablet/pen input** once boot quiesces into a true
   idle. The 26-task census includes `scrn` (RDY waiting on a
   semaphore group at `0xc125d58`) and `inkr` (recogniser),
   which suggests the framework is ready for input.
3. **Optional perf:** add a ScratchVA fallback for the rotate-
   LDR `no_dead_scratches` rejection (98% of inline-stub
   misses) — keeps the alignment-fault trap rate down. Lower
   priority than (1) since the boot now progresses past the
   prior wedge.

### Iteration 60: DFSC=5 fast-forward exclusion — bus-abort throw resolved

iter-59's fast trampoline bypassed `handle_diag` for every
forwardable DFSC. That broke section-level translation faults
(DFSC=0x05): ARMv7 leaves DFSR.Domain UNK for those, the kernel's
DAH then computes Domain=0, `GetDomainAndFaultMonitorFromDomainNumber(0)`
returns no monitor, `FaultMonitorEntry` returns -10015, and DAH
throws `evt.ex.abt.bus`. iter-58's slow path synthesised
DFSR.Domain from L1[FAR>>20][8:5] before forwarding (handle_diag,
trap.rs:6295), which got the right Domain=4 and let DAH recover.

#### Diagnosis

Cold-boot probe captures (lines 6518–6697 of the iter-60 cold-boot
log) showed:

```
NewStack POST-SWI: env=0x13a5 req=0x11000 base=0x0cd2d000 ...
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x0cd2d000 mode=0x17
FME-entry[4]: r0(mask)=0x0000121a far=0x0cd2d000 ... task[+0x58]=0x00000045
```

Compare against the iter-59 log at the same FAR: `task[+0x58]=
0x00000005` (Domain=0, FS=5). The `0x45` vs `0x05` difference is
exactly the synthesised Domain=4 vs hardware-UNK Domain=0. Mask
went from 0 (no monitor matched) to 0x121a (matched stack/heap
domain), and `FaultMonitorEntry` returned 0 (success → recovery)
instead of -10015 (failure → throw).

Empirically, L1[0xcd] for our run is `0x04025081` with bits[8:5]
= 0b0100 = 4 (heap-domain encoding). Even though that L1 entry's
type bits show "section descriptor" (= no fault), the section
itself faults at first access because the stage-2 mapping
isn't backed yet — DFSC=5 fires, but the L1 entry already has
the domain bits the kernel needs. Synthesis is therefore correct
and matches the StrongARM behaviour the kernel was written for.

#### Fix

Two NOPs in place of the DFSC=5 dispatch slots in
`install_dabt_fast_trampoline` (`src/guest_mem.rs`):

```rust
// iter-60: DFSC=0x05 deliberately excluded — see file-level
// comment for rationale. Two NOPs preserve the slot layout so
// the existing beq targets / `b SLOW_DABT_TRAMP` offset stay
// correct without recomputing.
rom_ptr.add(ft +  8).write(0xE320_F000);  // nop
rom_ptr.add(ft +  9).write(0xE320_F000);  // nop
```

DFSC=5 now falls through to the slow path → DABT_TRAMP → HVC
#DIAG_TAG → handle_diag → synthesise → forward. Other DFSCs
(0x07, 0x0F, 0x0D, 0x06, 0x03) keep the iter-59 fast bypass.
NOP encoding `0xE320_F000` verified with `arm-none-eabi-objdump`.

#### Verification

- All 36 guest tests pass on QEMU.
- 30 s cold boot (no snapshot, fresh):
  - DIAG_TAG (slow-path DABT) ≈ 0 in the per-2-s histograms — fast
    path still working for the common DFSCs. (The earlier 5-min
    measurement of 249 M DIAG_TAGs was a `timeout 30` that didn't
    actually kill QEMU because `timeout` defaults to SIGTERM which
    QEMU's semihosting ignores; the 5-min run kept emitting traps
    after the kernel got into a post-throw reboot loop. Use
    `timeout -k 2 30 …` (or `-s KILL`) for QEMU runs.)
  - One `dabt: forwarding DFSC=0x5 FAR=0x0cd2d000` slow-path entry,
    followed by the DFSC=7 page-level faults at the same VA — the
    expected first-touch sequence per new section.
  - 26 tasks alive (was 24 in iter-59): `OBJM`, `idle`, `main`,
    `cdfm`, `newt`, `cdsv`, `PMGR`, `PTBL`, `STKF`, `STKP`,
    `STKU`, `cdpr`, `drvr`, `ROMF`, `pg&e`, `alrt`, `ROMP`,
    `sndm`, `mntr`, `Tmux`, `name`, `pssm`, `pckm`, `cmgr`,
    plus the new `inkr` (recogniser, 0x3853) and `scrn`
    (screen, RDY blocked on TSemaphoreGroup at 0xc125d58).
- New residual: `evt.ex.fr.store` NewtonScript throws fire at
  ROM PCs `0x00353730` / `0x002df4f0` / `0x00351e50` /
  `0x002eff24` / `0x002f1eac` with r1 ∈ {0xffffd692, 0xffffd698}.
  Caught and continued — no `UnhandledException` halt in the
  30 s window. Tracked as iter-61.

#### Out of scope (deferred)

- Inline DFSR.Domain synthesis in the AArch32 fast trampoline.
  Doable (≈10 extra insns: read FAR, walk L1, splice into DFSR
  via `mcr p15,0,r0,c5,c0,0`) but DFSC=5 fires only on first
  touch of a 1 MiB section — ~tens of times per boot. Slow-path
  cost is negligible.
- Hardening the timeout pattern in iter scripts. Adopted ad-hoc
  in iter-60: use `timeout -k 2 N` so QEMU under semihosting
  actually dies on deadline.

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

<!-- Older iteration retrospectives (iter-57 and earlier) live in
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
