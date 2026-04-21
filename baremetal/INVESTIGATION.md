# Phase B boot-stall investigation

Live notes on the current post-MMU-on data-abort. Update as we learn
more; archive to a dated file when we move past the stall.

## Current stall

Post Phase-A silent-default cleanup, the guest data-aborts shortly
after SCTLR M=1 writes. Our DABT-vector HVC patch at VA 0x10
intercepts it; the two-stage DIAG handler in `src/trap.rs`
(`handle_diag` + `handle_diag_lr`) dumps state via a RAM-based
banked-register stub (`src/trap.rs` near `LR_STUB_PA`) that bypasses
QEMU raspi3b's unreliable AArch32→AArch64 banked-reg plumbing.

## Observed state at the abort

```
pre-DABT mode:   UND   (SPSR_abt = 0x1DB)
pre-UND mode:    SVC   (SPSR_und = 0x1D3)
DFSR = 0x00000801   — FS[4:0] = 0x01 → Alignment fault, WnR = 1
DFAR = 0x0100018B   — unaligned (bits[1:0] = 11)
FAR_EL1 = 0x0100018B (matches DFAR)
SCTLR_EL1 = 0x11BD  — MMU on, C=1, I=1, V=0 (low vectors), A=0
TTBR0_EL1 = 0x04000000, TTBR1 = 0, TCR = 0
L1[0x10] = 0          (the VA 0x0100018B gap between Opt. ROM and REx window)
```

Banked state (recovered via RAM-dump stub):

```
SP_abt = 0x000908A0  LR_abt = 0x02A04000  SPSR_abt = 0x1DB
SP_und = 0x0008EA8C  LR_und = 0x0008EB08  SPSR_und = 0x1D3
SP_svc = 0x0C004C00  LR_svc = 0x0001889C  (stale, pre-MMU-on)
```

## Interpretation

`LR_und = 0x0008EB08` → faulting PC (from UND perspective) = 0x0008EB00
in ARM mode. That instruction is `ldmdb fp, {r4, fp, sp, pc}` — a
function epilogue at the end of `GetScriptDictRef__FRC6RefVar`
(symbol in `_Data_/symbols.txt`).

`DFAR = 0x0100018B` and the `fp` used for the LDMDB:
LDMDB reads four words at fp-16, fp-12, fp-8, fp-4 (into r4, fp, sp,
pc respectively). If `fp = 0x01000193` (unaligned, bits[1:0] = 11),
the access at fp-8 = 0x0100018B triggers the alignment fault — LDMDB
requires 4-byte alignment regardless of SCTLR.A. Matches.

`LR_svc = 0x0001889C` is the static return address from the initial
`BL 0x188F8` (FlushTheCache) at BootOS PC 0x18898. Meaning: from the
moment SVC returned from FlushTheCache until now, SVC made **zero**
further BLs. The kernel's control flow between FlushTheCache return
and the faulting LDMDB therefore uses only non-BL branches, voluntary
mode switches (MSR CPSR_c), and mode-bank swapping.

`handle_und` never fires during the whole run (its one-shot
`"und: handle_und first entry"` log never appears in the trap log).
So the guest entered UND mode **voluntarily** via `MSR CPSR_c, #0xDB`,
not through a UND exception. Candidate MSR sites from the ROM scan:
0x18BE4 (inside `SetUpStacks`), 0x18F78 (inside StrongARM branch of
`SaveCPUStateAndStopSystem`, not taken on A53), 0x19334 and 0x193E0
(inside the non-StrongARM branch of `SaveCPUStateAndStopSystem`).

`LR_abt = 0x02A04000` is the odd anomaly: `faulting_pc + 8 = 0x0008EB08`
matches `LR_und`, not `LR_abt`. If the CPU genuinely took a DABT
from UND at PC 0x0008EB00, we'd expect `LR_abt = 0x0008EB08`.
Candidate explanations (pick based on evidence in follow-up runs):

- **QEMU banked-LR plumbing bug for ABT.** The stub reads `LR_abt`
  correctly via `mov r1, lr` while in ABT mode, but QEMU might have
  failed to update R14_abt on ABT exception entry for certain DABT
  paths. Historically we've already seen QEMU return 0 for `SPSR_abt`
  via the AArch64 MRS; similar plumbing issues aren't ruled out.
- **Imprecise abort.** On A53 some DABT classes are imprecise; the
  CPU may have written `LR_abt` to an arbitrary later PC. For an
  alignment fault on a multi-word LDMDB though the ARM ARM says the
  abort is precise.
- **The abort fired from a different mode than UND, via a subsequent
  exception taken after the LDMDB.** For example: LDMDB at 0x8EB00
  in UND faults → CPU enters ABT with LR_abt = 0x8EB08, but a later
  ABT or some kernel handler clobbers our ERET / re-enters ABT before
  our HVC latches. Our HVC at VA 0x10 should be the first insn after
  the vector jump though, so this is unlikely.

## Trap timeline (from the 500-entry trap log)

```
EC=0x03 ELR=0x18690  SCTLR write #1 (M=0)
EC=0x24 ELR=0x186B4..0x18710  — BootOS early MMIO init (VIC + misc)
EC=0x24 ELR=0x313470..0x313568 — sub-function (via BL 0x313468 in BootOS)
EC=0x24 ELR=0x18748..0x1875C  — more inline MMIO
EC=0x24 ELR=0x18F10..0x18F24  — SafeShortTimerDelay loop
EC=0x24 ELR=0x1A00C, 0x1A01C    — ??
EC=0x24 ELR=0x19FE8, 0x19FF4    — ??
EC=0x24 ELR=0x19810..0x19940  — inside 0x1955C setup sweep
EC=0x24 ELR=0x19A24            — RAM probe (0x19988)
EC=0x24 ELR=0x19Cxx..0x19D8C  — later 0x1955C stores
EC=0x24 ELR=0x19960, 0x19970  — trailing 0x1955C stores
EC=0x03 ELR=0x1879C  SCTLR write #2 (still M=0; deep in BootOS)
EC=0x24 ELR=0x3136C8..0x3137E0 — TestForREx probes (0x3137DC)
EC=0x03 ELR=0x18944  DFSR write (inside FlushTheMMU)
EC=0x03 ELR=0x18850  DACR write (inline)
EC=0x03 ELR=0x18864  TTBR0 write (inline; triggers fix_stage1_xn_bits)
EC=0x03 ELR=0x18894  SCTLR write #3 (M=1 — MMU comes up)
EC=0x12 ELR=0x00014 — our DIAG HVC at VA 0x10 (DABT intercept)
```

Between SCTLR M=1 (0x18894) and the DABT we record **zero** sync
traps. Whatever executes in that window (FlushTheCache cache ops,
`MOV sp, r4`, `BL 0x45B78`, `BL 0x11EFB4` SetUpStacks, etc.) runs
without any CP15 / stage-2 trap. So the fault path is purely CPU-
internal between those two points.

## What we've ruled out

- **Silent MMIO drops.** Phase A converted every unknown MMIO to a
  loud halt. None have fired before the DIAG intercept, so there's
  no leftover silent-drop corrupting pointer loads or vtable entries
  that the kernel later dereferences.
- **`handle_und` trampoline failing.** Its one-shot entry log never
  fires, so no UND exception has been taken end-to-end during this
  run (if the trampoline's own push to SP_und were aborting, we'd
  see an ABT-from-UND with `LR_abt` pointing at 0x00FFFF00-ish, not
  at 0x0008EB00 / 0x02A04000).
- **UND trampoline VA unreachable.** L1[0x0F] maps VA 0x00F00000-
  0x00FFFFFF as a section to PA 0x00F00000 (identity), so
  `0x00FFFF00` is valid executable memory in the guest's stage-1.
- **CPSR.T = 1 Thumb run-away.** The earlier-observed Thumb-bit-set
  LR readings were symptoms of QEMU's AArch32 MRS SPSR plumbing,
  not actual Thumb execution (the RAM-dump path shows T=0 throughout
  the relevant banked SPSRs).

## Reproduction

```bash
rm -f /tmp/newton-snapshot-*.bin
cd baremetal && cargo run --release
```

The DIAG intercept is installed at boot (see `src/guest_mem.rs`
`rom_ptr.add(4).write(0xE140_0171)` for the single-word DABT-vector
HVC patch; `src/trap.rs` `handle_diag` / `handle_diag_lr` for the
two-stage handler; stub at guest VA 0x0C004F00 / PA 0x04005F00).

All 13 `guest-tests/scripts/run-all.sh` tests pass with the current
state.

## Open questions / next hypotheses

1. **Where does the kernel's voluntary MSR-to-UND happen between
   0x18898 (post-MMU BL FlushTheCache) and the faulting LDMDB at
   0x8EB00?** Candidates: 0x18BE4 (inside SetUpStacks — but SetUpStacks
   hasn't run; SVC didn't BL it), 0x19334 / 0x193E0 (non-StrongARM
   path of SaveCPUStateAndStopSystem — also reached via BL which we
   don't see). Check: could the kernel be reached via a non-BL tail
   branch to one of these, or via `mov pc, <reg>` from FlushTheCache's
   return?

2. **How does `fp` become 0x01000193 in UND mode?** If the kernel
   moves to UND mode and then calls `GetScriptDictRef` (which has a
   prologue `mov ip, sp; push {r4, r5, fp, ip, lr, pc}; sub fp, ip, #4`),
   fp = caller's sp - 4. If caller's SP_und = 0x01000197, fp = 0x01000193.
   So: who sets SP_und to 0x01000197? Not the UND-stack-init at
   0x18BE4 (it sets SP_und = 0x0C006000 per TMemoryConsts).

3. **Why does `LR_abt` disagree with `LR_und` + 8?** See interpretation
   above; likely QEMU plumbing. If critical, add a third-stage stub
   that does `stmfd r0, {lr}^` or similar to cross-check.

## Diagnostic scaffolding to remove once the stall is past

- Single-word HVC patch at ROM offset 0x10 (`guest_mem.rs`
  `rom_ptr.add(4).write(0xE140_0171)`).
- `handle_diag`, `handle_diag_lr`, the `LR_STUB_*` / `LR_SAVE_*`
  constants, `LR_SAVE_PA_RECORD`, `DIAG_TAG`, `DIAG_LR_TAG` and
  `handle_und`'s one-shot log in `src/trap.rs`.
- 500-entry trap log budget at the top of `trap_sync_lower_aarch32`.
- `guest_mem::dump_stage1_walk` stays — it's useful for future
  vector-intercept diagnostics.
