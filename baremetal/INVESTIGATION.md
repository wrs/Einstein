# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at (2026-04-22, post-PlatformDriver-gap)

**Boot reaches ~trace 198655 in ~90 s, then DABTs at FAR=0xEA3FFFC5
because `GetPlatformDriver()` returns NULL.** The kernel's
`TPlatformDriver::PauseSystem` at `0x00387EB8` starts with
`LDR R0, [R0, #4]` on a NULL `this` — R0 reads our UND-vector patch
word at VA 4 (`0xEA3FFFBD`), then `LDR R12, [R0, #8]` tries to
dereference `0xEA3FFFC5` → unmapped → DABT.

Trace tail:

```
trace 198652 SpecialCPUIntDisable (svc)
trace 198653 GetPlatformDriver    (svc)   — returns 0 (global *(0x0C101764) is NULL)
trace 198654 GetPlatformDriver    (svc)
trace 198655 TPlatformDriver::PauseSystem (svc) r0=0  →  DABT FAR=0xEA3FFFC5
```

Root cause: `LoadPlatformDriver()` at `0x0018B23C` is never called
in our boot — it does a named-class registry lookup for
`"TMainPlatformDriver"` which would populate the global at
`0x0C101764`. Einstein installs the class via its REx; in our setup
the registry entry is either missing or the init path that triggers
the lookup was skipped. `LoadPlatformDriver` is in
`scripts/classify-out/code-symbols.txt`, so if it had run the tracer
would show it.

Next step: cross-check whether Einstein's REx registers
`TMainPlatformDriver` with the ROM's class registry, and if not,
which subsystem init function in the ROM/REx is responsible for
instantiating the platform driver. The tracer's `GetPlatformDriver`
hits at trace ~19468 (the first one) are the earliest clue — trace
back from there to find the missing initialisation.

## Resolved — flash header verify, BIO registers, ROM serial chip, USR-mode tracer (2026-04-22)

Several independent Phase B blockers fixed in sequence to get from
trace 1841 to trace ~198655:

1. **SystemBootUND PC advance wrong.** `handle_und` advanced ELR by
   8 (opcode + payload word), but Einstein's JIT (TJITGeneric_Other.cpp
   +TJITGenericPage.cpp) treats 0xE6000010 as a single-instruction NOP:
   `PushUnit(SystemBootUND); ... PushUnit(inVAddr + 8)`, combined with
   `GetJITUnitForPC(pc = inPC - 4)`, resumes at `inVAddr + 4`. The only
   SystemBootUND site in 717006 is at `0x000188CC`; the word at
   `0x000188D0` is a real `LDR R0, [PC, #0xc40]` feeding the
   `LDR PC, [R0]` at `0x000188D8`. Skipping it left a stale R0
   (= 0x800001D3, a CPSR value from earlier) and DABT'd when the
   indirect LDR PC tried to dereference it.

2. **BIO interface register window (0x0F05_xxxx) reads halting.**
   Einstein's TMemory.cpp:952-959 falls through to "unknown bank #3"
   = return 0 for reads of 0x0F052C00..0x0F055000. Added explicit
   read-returns-0 entries for the four R/W registers the 717006
   kernel's TBIOInterface::BIOReadCommand accesses
   (`0x0F05_2C00`, `0x0F05_3000`, `0x0F05_3400`, `0x0F05_3800`).
   Matches Einstein behaviour; no tracked state.

3. **kROMSerialChip (0x0F24_3000) not modelled.** Einstein implements
   this as a 1-Wire serial-ROM bit stream in TMemory.cpp:984-999 /
   2723-2762. Ported verbatim: `mSerialNumber[2]` computed from
   `mNewtonID = {0, 0}` (Einstein's default) yields
   `[0x3D000000, 0x00000001]`; each read returns `(bit & 1) << 1`
   and advances an index mod 65 (with 64 as the end-marker return-0
   slot).

4. **Tracer HVC in USR mode UND'd.** The tracer trampoline's
   slot[0] `hvc #TRACE_TAG` is undefined at EL0, so any traced
   function the kernel calls in USR mode (OsBoot was the first to
   fire) raised an UND instead of entering EL2. Added a fallback in
   `handle_und`: if `insn == TRACE_HVC_INSN` (= 0xE1400570) and the
   faulting PC is in the trampoline pool, log the trace entry
   (via `log_trace_at`) and advance PC to slot[1] preserving USR
   CPSR.

5. **MRS Rd, SPSR in USR mode UND'd.** `MonitorEntryGlue` and peers
   legitimately execute `MRS R12, SPSR` in USR mode — the SA-1100
   returns the CPSR in that case (ARMv4 "UNPREDICTABLE" but Einstein
   models it explicitly at TARMProcessor.cpp:774-781: `case
   kUserMode: return GetCPSR()`). The A53 UNDs. Ported: in
   `handle_und`, detect `MRS Rd, SPSR` encoding + USR mode in SPSR_und
   and emulate by writing `spsr_und` (= pre-UND CPSR captured by the
   UND trampoline) into Rd, then advancing ELR by 4.

6. **Flash header verify (prior work at 88a5d47c).** 16-bit write
   `/2` stride bug + ROM/REx checksum seeding + erased-flash default.
   See commit message for details.

These were landed in sequence; each uncovered the next once the
previous stall cleared.

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
cd baremetal && timeout 90 cargo run --release --features trace,quiet
```

The boot reaches ~trace 198655 in ~90 s and halts in the DIAG DABT
intercept with `DFAR=0xEA3FFFC5`. The preceding trace lines show
`GetPlatformDriver` returning NULL and `TPlatformDriver::PauseSystem`
dereferencing the null `this`. All 20 guest tests pass at the current
commit (`guest-tests/scripts/run-all.sh`).
