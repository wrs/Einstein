# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at (2026-04-22, post-shadow-stub-in-ROM)

**Boot runs to ~345,000 function-tracer entries in 90 s, then spins in
`TPrivatePackageIterator` indefinitely.** No PABT, no halt; just an
apparently infinite iteration. 127 unique functions traced; the top
callers are:

```
  11,888  TPrivatePackageIterator::NumberOfParts
   6,988  SBankInfo::NormalRAMSize
   4,216  StoreToPhysAddress
   3,439  TRAMTable::GetPPage
   3,411  TPrivatePackageIterator::DisposeDirectory
   3,192  TPrivatePackageIterator::Init
   3,192  TPrivatePackageIterator::CheckHeader
   3,192  TPrivatePackageIterator::TPrivatePackageIterator
   3,191  TPrivatePackageIterator::~TPrivatePackageIterator
   2,972  TPrivatePackageIterator::VerifyPackage
   2,972  TPrivatePackageIterator::SetupRelocationData
   2,972  TPrivatePackageIterator::ComputeSizeOfEntriesAndData
   2,972  TPrivatePackageIterator::GetPartInfo
   2,972  TPrivatePackageIterator::GetPartInfoDesc
   2,972  TPrivatePackageIterator::GetPartDataOffset
   2,972  TPrivatePackageIterator::PackageName
   2,972  UstrlenPrivate
   2,862  TPrivatePackageIterator::PackageSize
   2,653  PrimRExConfigEntry
   2,442  LoadFromPhysAddress
```

The ratio — `PrimRExConfigEntry` (the REx entry iterator's "next"
primitive) is called ~2,650 times but the per-package methods are
called 2,972 each — hints the iterator isn't advancing past a
malformed or never-terminating entry. Likely causes, in order of
plausibility:

1. Einstein REx's `fdrv` / `FDRV` / `pkgl` entry structure has a
   field (size, offset, or terminator) the kernel interprets
   differently than Einstein does, so the iterator loops over the
   same entries.
2. The kernel's package-iterator state lives in RAM addresses that
   our stage-1-normalised L1 walks mishandle (domain bits, XN bits),
   causing `NumberOfParts` to re-read zero-initialised storage and
   restart.
3. `PrimRExConfigEntry` returns the wrong "next" pointer because of
   a byte-access pattern that slipped past shadow-stub's static
   scan (e.g., a lazy-RAM-resident copy of the iterator the kernel
   populates post-MMU — pool B coverage TBD).

Next step: enable `trace,quiet` + a short `cargo run` wall clock,
then script a classifier over the trace: find the first iteration
cycle whose per-method arguments match an earlier iteration's
verbatim. That identifies the cycle length and the fixed arguments
the iterator keeps re-feeding itself.

## Resolved — post-MMU PABT on shadow-stub pool A (2026-04-22)

Boot previously wedged at trace 244 (deep in `MapTable(3, 0)`) with
a PABT at vector 0x0C caused by a patched LDRB in ROM branching to
`B 0x0181C180` — a shadow-stub slot in pool A at IPA 0x01800000.
The guest kernel's stage-1 didn't map VA 0x01800000+ once the MMU
was on (Einstein's MMU dump from `probe/results-717006-30s.txt`
shows `VA 0x01810000 to 0x01900000 (960 kB): page fault` and
`VA 0x01900000 to 0x01A00000 (1024 kB): fault`), so any post-MMU
dispatched stub PABT'd on fetch.

Fix: moved pool A into the ROM aperture at IPA 0x00E00000..0x01000000.
The guest kernel's own stage-1 L1[0xE..0xF] section descriptors map
the entire ROM aperture identity both pre- and post-MMU (Einstein's
same MMU dump shows `VA 0x00100000 to 0x01000000 (15360 kB): section`).
Stubs are reachable from every patched site regardless of MMU state.

Stub layout changed with the move: scratch save/restore uses
TPIDR_EL0 (MCR/MRC p15,0,Rt,c13,c0,2) instead of an in-slot
PC-relative STR — ROM stubs are read-only, so self-write wasn't an
option. TPIDR_EL0 is ARMv6+ per-CPU architecture the SA-1100
doesn't implement, and our HCR_EL2 doesn't trap CP15 c13, so the
save/restore executes natively. Nested-exception caveat: if a
higher-priority exception handler itself fires a shadow-stub the
saved value is clobbered; hasn't surfaced in practice.

Return-to-caller is a direct unconditional `B orig_pc+4` (±32 MiB
reach covers any patched ROM site from the pool).

Commit: `baremetal: shadow_stub pool A in ROM aperture — fixes
post-MMU dispatch`.

Pool B (lazy-RAM-resident patches, IPA 0x03000000) still exists
with the old layout — only `test_shadow_stub` exercises it, and
that path stays pre-MMU. Revisiting pool B's placement waits for
the real Newton boot to reach `UseROMJumpTables` (the first
post-MMU lazy-RAM consumer).

## Resolved — 72-function stall at `T28F016_SA_SVDriver` (pre-trace-rewrite)

Before the every-call tracer and RExScanner gGlobals fix, boot stalled
at 72 function entries deep: `T28F016_SA_SVDriver::Identify` failed
because `RExScanner` was reading poison (0xb6db6db6) from
`gGlobalsThatLiveAcrossReboot + 0x20`, diverting `ScanForREx` to base
`0x00B1FC4C` instead of `0x0071FC4C`. Fix: zero `*(r0 + 0x20)` on
`RExScanner` entry. With REx now correctly registered, the boot
progressed far enough to trigger the shadow-stub-pool PABT above.

## Earlier root-cause work (pre-trace-rewrite)

These are historical; search the git log for full context if reviving:

1. **First hard stall — post-MMU DABT at FAR=0x0100018B.** Root cause:
   `MCR p15, 0, r0, c7, c7, 0` at PC 0x18924 (deprecated ARMv4
   "invalidate unified cache" encoding, UND on A53). Cascade through
   uninitialised SP_und and UND save slot aliased with guest L1 table.
   Fix: emulate the MCR as `IC IALLUIS + DSB ISH`; rewrite UND
   trampoline to a stack-free form writing LR/SPSR via PC-rel
   literal; move save slot to 0x04005F00. See commit 5fddb693.

2. **DebuggerUND over-advanced PC.** Payload is a null-terminated
   ASCII message, not a single 4-byte word. Fix in commit 5fddb693.

3. **`fix_stage1_xn_bits` missed late L2 populations.** Only ran on
   the first TTBR write. Fixed to run on every M=0→M=1 SCTLR rising
   edge. Commit 5fddb693.

4. **Tick-register polling dominated trap time (~75 % of all traps).**
   K_HDWR_TICKS polled hot. Fix: 4 KiB L3 page at IPA 0x0F181000
   pumped from timer IRQ; see commit ebd7352b. ~13× trap reduction.

5. **UND trampoline clobbered R0 / R1 (tracer transparency).** With
   every-call tracing the extra UND round-trips surfaced an
   argument-register clobber in the trampoline. Fix: save R0 and R1
   to RAM slots, restore in `handle_und`. Commit f99b0f24.

6. **ROM DebuggerUND messages surfaced properly.** Byte-order bug
   (per-word-swapped ROM stored messages require `to_be_bytes()`
   iteration) + budget-8 cap replaced with per-PC seen set.
   Commit 534e3974.

## Diagnostic scaffolding still in place

Kept so the next stall is caught with full context:

- **DABT HVC patch at VA 0x10** (`guest_mem.rs`) → two-stage
  `handle_diag` / `handle_diag_lr` in `src/trap.rs` with a
  RAM-based banked-register dump at IPA 0x04005F00..0x04005FA7.
  Bypasses QEMU raspi3b's flaky AArch32→AArch64 banked-LR / SPSR
  plumbing. Handle_diag's stub now prefixes `msr cpsr_c, #0xd7` so
  it reads the correct mode's banked regs regardless of entry mode.
- **PABT HVC patch at VA 0x0C** (`guest_mem.rs`) — same DIAG path
  as DABT; catches any future prefetch abort. Introduced during the
  pool-A-in-ROM investigation and kept as scaffolding.
- **`handle_diag_from_bp`** (`src/trap.rs`) — lets `guest_bp`
  handlers hand off to the banked-reg dump stub without a dedicated
  vector.
- **500-entry trap log budget** at top of `trap_sync_lower_aarch32`.
  Trace-HVC (0x50) suppressed so the tracer output isn't doubled.
- **Bring-up-critical VA walks in `handle_diag`** — SVC stack, ABT
  stack target, RAM window, REx base, UND trampoline, etc.

None of the above needs to come off until we're past TInterpreter.

## QEMU banked-register caveat (CLAUDE.md already warns)

`ctx.x[13]` / `ctx.x[14]` at AArch64 EL2 entry are not reliable
aliases for the guest AArch32 mode's banked R13/R14. QEMU's
raspi3b gdb stub is aarch64-only and the AArch32→AArch64 banked-reg
plumbing is flaky. Read banked regs via the AArch32 stub
(`handle_diag_lr`) or via QEMU's `-d in_asm -accel
tcg,one-insn-per-tb=on -D <file>` trace output; don't trust the
ctx-carried values for mode-switch-sensitive reasoning.

## Reproduction

```bash
rm -f /tmp/newton-snapshot-*.bin
cd baremetal && timeout 90 cargo run --release --features trace,quiet
```

With trace enabled, the log ends with the tracer's 345k-entry
cycle through `TPrivatePackageIterator`. All 20 guest tests pass
at the current commit (`guest-tests/scripts/run-all.sh`).
