# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at (2026-04-22, post-flash-NATIVE_PRIM-rewrite)

**Boot reaches trace 1841 in ~6 s, then halts at the
`PowerOffAndReboot` canary because the flash header write/verify
round-trip diverges.** The canary fires at the FIRST reboot now
(see "Diagnostic scaffolding" below for the patch site).

Trace tail at the halt:

```
trace 1815  TFlashDriver::ReportWriteResult       (last write subfn)
trace 1816  TReservedBlockAccessor::CompareFlashAndMemRebootIfDifferent
trace 1818  TFlashRange::Read                      (256 bytes from 0x30000000)
trace 1826  BlockMove(src=0x30000000, dst=RAM)
trace 1833  TReservedBlockAccessor::CompareAndRebootIfDifferent
trace 1841  ConfigureFlashBankDataSize             (last before halt)
PowerOffAndReboot canary fires (R0=0xFFFFD6BC, SVC mode)
```

Mechanism: kernel erases the flash header block, programs ~256 bytes
of `DLDS`/`OSCD` header, reads it back via `TFlashRange::Read` from
the read-window VA `0x30000000`, then byte-compares against the buffer
just written. If they differ, reboot. The compare is failing.

Hypotheses (in order of plausibility):

1. **`0x30000000` flash read-window unmapped at stage-1 → reads zeros
   or stage-2 fault.** The kernel's `AddNewSecPNJT` for write at
   `VA 0x34000000 → IPA 0x02000000` is traced explicitly; the read
   window's mapping is set up elsewhere (probably during
   `TFlashRange::StartReadingArray`). Verify whether `0x30000000`
   actually translates to flash bank 0 IPA via the live stage-1.
2. **16-bit lane endianness mismatch in `flash_driver::write`.** Our
   `program_word` shifts the 16-bit half into `pa & 2 == 0 → high`
   or `pa & 2 != 0 → low`, but the BE-32 invariant view may need the
   opposite. Double-check against `TMemory::WriteToFlash16Bits` and
   the actual byte expected at LE offsets after the shadow_stub XOR.
3. **`ConfigureFlashBankDataSize` MMIO writes are no-ops** — the
   kernel toggles bank-control bits between every flash op (the
   trace shows `0x0c1008c8` writes around every Read/Write). If the
   bank control reg is supposed to gate access width, ignoring it
   could leave the next read picking up the wrong byte lanes.

Next step: dump the bytes actually written to flash bank 0 backing
after the kernel finishes the header-write loop, and the 256 bytes
the kernel reads back from `0x30000000`. If they differ, the bug is
in the read path; if they match, the kernel is reading the wrong
bytes for some other reason (alignment, bank, lane).

## Resolved — flash chip identification failure (2026-04-22)

Boot previously fail-rebooted after trace 948 because every call to
the flash chip `Identify` native primitive returned r0=0 (no driver
match), and the kernel called `PowerOffAndReboot(0xFFFFD6BF)`. The
reboot looped 361 times before the 90-s timeout, leaving 350k
identical tracer entries that masked the real failure.

Three independent fixes:

1. **`PowerOffAndReboot` canary** (`rom_patches.rs`, commit
   `baremetal: PowerOffAndReboot canary`). Patch the first word at
   `0x000E_6BBC` to `HVC #0x42`; handler in `trap.rs` dumps R0
   (reboot reason), ELR, mode, and halts. Catches every future
   "kernel gave up" failure on the first hit instead of letting it
   ring.

2. **Rewrite Einstein.rex `NATIVE_PRIM` MCR sites from Rd=LR to
   Rd=R12** (`guest_mem.rs::patch_native_prim_mcr_lr_to_r12`,
   commit `baremetal: rewrite REx NATIVE_PRIM MCR Rd=lr → r12 +
   flash driver fixes`). Einstein's `NATIVE_PRIM` macro emits:
   ```
   stmdb sp!, {lr}
   mov   lr, #id          ; or: ldr lr, [pc, #4]; .word native_insn
   [add  lr, lr, #impl*0x100]
   mcr   p10, 0, lr, c0, c0
   ldmia sp!, {pc}
   ```
   QEMU raspi3b does NOT propagate the AArch32 current-mode banked
   LR (R14_svc) into x14 on lower-EL AArch32 → AArch64 EL2 trap
   entry. ctx.x[14] reads as 0 instead of the native-call ID the
   preceding MOV wrote. The native primitive then dispatched to
   (driver=0, subfn=0) — the null-primitive test slot — for every
   call. Identify, Init, Write — all silently failed.

   The patcher walks the entire REx range at load time, recognises
   three lead-in patterns (`mov lr,#imm`; `mov lr,#imm; add lr,lr,#imm`;
   `ldr lr,[pc,#4]`), and rewrites each MCR + its producer to use
   R12 (call-clobbered per AAPCS, non-banked). 256 sites patched in
   Einstein.rex. LR is still pushed/popped by the surrounding
   `stmdb`/`ldmia` so the function returns correctly.

3. **`flash_driver::write` reads `flashRange` from ctx.x[4] instead
   of [SP+4]**. Same QEMU bug affects banked SP — none of x13,
   SP_EL0, SP_EL1 hold the SVC-mode SP at MCR trap entry. Reading
   `[SP+4]` returns poison from an unrelated stack region. All 7
   callers of `TFlashDriver::Write` (the vtable trampoline at
   `0x00384790`) are `T{8,16,32}BitFlashRange::DoWrite` whose
   prologue saves `this` (= flashRange) into r4; r4 is callee-saved
   and survives the intermediate vtable BL.

The "iterator loop" symptom turned out to be a red herring — the
350k entries were 361 identical kernel boot iterations, each ending
at the same `PowerOffAndReboot` site after the same flash-identify
failure. The iterator was working correctly; the boot was just
restarting and re-running the early-init code over and over.

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

- **`PowerOffAndReboot` HVC canary at PC 0x000E_6BBC** (`rom_patches.rs`,
  `trap.rs::handle_poweroff_reboot`). First word patched to
  `HVC #0x42`; handler dumps R0 (reboot reason) and halts. Catches
  every "kernel gave up" failure on first hit. Tracer reservation
  in `tracer::in_reserved_range` keeps it from being overwritten.
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
cd baremetal && timeout 30 cargo run --release --features trace,quiet
```

The boot reaches ~trace 1841 in a few seconds and halts at the
`PowerOffAndReboot` canary with `R0 = 0xFFFFD6BC`. The preceding
trace lines show the failed flash header write/verify round-trip.
All 20 guest tests pass at the current commit
(`guest-tests/scripts/run-all.sh`).
