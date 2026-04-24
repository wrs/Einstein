# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at — unrecognised Einstein UND 0xe6000210 at PC 0x38ce84 (FVP, 2026-04-24)

Boot now advances past the BootOS-canary-entry-2 stall (resolved by
hypervisor-wide rotate-LDR emulation — see below). The new frontier:

```
und: TapFileCntlUND @PC=0x38ce7c payload=0xe6000310
*** unrecognised UND: insn=0xe6000210 at PC=0x38ce84 SPSR_und=0x20000110
    (extend handle_und in trap.rs to handle this opcode)
```

The preceding TapFileCntlUND at `0x38ce7c` dispatches cleanly. The
instruction at `0x38ce84` is `0xe6000210` — another Einstein UND
opcode (all `0xe60000x0` forms are Einstein / Newton-specific UND
markers per `Emulator/JIT/Generic/TJITGenericPatchManager.cpp` and
`Emulator/TARMProcessor::DoUND`). This one isn't in our current
`handle_und` dispatch table (see `src/trap.rs`). Next-session work:
grep the Einstein source for `0xe6000210` / `0xe6_00_02_10`, decode
the semantic (payload width, PC advance, side effect), add it to
the handler. The payload `0xe6000310` captured in the previous
TapFileCntlUND log-line suggests this region of the ROM is heavy
on Newton-specific UND markers — several more variants may follow.

Reproduce:

```
rm -f /tmp/newton-snapshot-*.bin
cargo build --release --no-default-features --features "platform-fvp-base quiet"
scripts/fvp --timeout=90 target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

## Resolved — hypervisor-wide rotate-LDR emulation via SCTLR.A + alignment fault (2026-04-24)

The BootOS-canary-entry-2 stall (documented below) was caused by
ARMv4 rotate-LDR semantics. SA-1100 (BE-32 + SCTLR.U=0) on an
unaligned `LDR` aligns the address down and rotates right by
`(addr & 3) * 8`; A53 AArch32 forces `SCTLR.U=1` and does a true
unaligned load (four contiguous bytes, no rotate). The 717006 ROM
has ~1300 sites depending on rotate semantics — patching each is
not viable.

**Fix**: hypervisor-wide alignment-fault emulation. The CP15 shim
ORs `SCTLR.A=1` into every guest SCTLR write, so unaligned LDR/STR
raise a stage-1 alignment fault at EL1. The DABT trampoline at
VA 0x10 detects `DFSR.FS[3:0]==1` (unique to alignment in the
short-descriptor FS encoding) and fast-paths to `HVC #ALIGN_TAG`;
`src/unaligned.rs::handle_align_fault` decodes the faulting LDR/
STR and performs the aligned word load + ROR in EL2 Rust,
advancing ELR_EL2 past the faulting insn.

Notable subtleties:

- **Banked-register access at HVC from ABT mode.** Per ARM ARM
  DDI 0487 D1.21.1 Table D1-79, the AArch32→AArch64 exception-
  entry register map is by bank name, not by active mode. So
  `ctx.x[14]` is `LR_usr`, `LR_abt` lives in `ctx.x[20]`, etc.
  This had been misdiagnosed as a "QEMU banked-reg bug" more
  than once — see `docs/QEMU_BUGS.md` for the full mapping. The
  emulator uses `ctx_slot_for_reg(reg, pre_mode)` to read/write
  Rn/Rt/Rm correctly for any pre-abt AArch32 mode.
- **R0/R1 recovery.** The DABT stub uses R0/R1 as scratch; it
  saves them to `TPIDRURW` / `TPIDRRO` before clobbering. The
  handler reads them back via `tpidr_el0` / `tpidrro_el0`.
- **SPSR_abt read.** QEMU raspi3b's named `mrs spsr_abt` is
  still flaky (returns 0), so the stub persists SPSR_abt to the
  `DABT_SAVE_PA` RAM slot and the handler reads it from there.
  LR_abt comes from `ctx.x[20]` directly (reliable on FVP).
- **Boot progress.** Reaches the same ~620K-trap horizon that
  the earlier hand-patched CountMatches + ResolveFault commits
  landed at. Those targeted patches are superseded and removed
  — the trap-based emulator covers every static unaligned
  immediate-offset LDR and every `[Rn, Rm, LSL #1]` register-
  offset site without a ROM-patch whitelist.

New guest test: `test_rotate_ldr` verifies offsets +1/+2/+3
produce SA-1100 ROR-by-8/16/24 results plus the aligned control
case. All 23 guest tests pass.

Commit: (this commit).

## Previously at — BootOS canary entry #2 → soft-reset via stale task frame (FVP + QEMU, 2026-04-23)

Boot advances past the SWP-VA-translation and FPA fixes (see below)
and reaches beacon trap ~620 000 before the canary fires. The canary
halts on a second entry to `BootOS` from **USR mode** (HVC #0x44 at
PC 0x18688 → UND since HVC is UNDEFINED at EL0 — we now route the USR-
mode canary path through `handle_bootos_canary` via `handle_und`).

Root-cause chain (reconstructed from FVP TarmacTrace window around
the reset):

1. User task calls `TUMonitor::Init` at 0x2594b4. Local stack frame
   set up at `sp = 0x0c31030c`; `MakeObject(8, sp, 36, …)` invoked
   via jump-table `BL 0x1bd6b64 → B 0x2595b4`.

2. `MakeObject` prologue pushes `{r4, r5, r6, fp, ip, lr, pc}` at
   0x2595b8. Trace confirms `R r14_usr = 0x2594fc` at BL time; the
   push stores it at `[0x0c310304]` (saved LR slot). The body runs
   through shadow-stub emulation of `LDRB r0, [r0, #4]` at 0x2595c8,
   calls `MonitorDispatchSWI` (SVC #0x1b) at 0x2595fc.

3. Deep inside the SWI handler, the kernel executes
   `StoreToPhysAddress` at 0x18ce0 to rewrite an L2 page-table entry
   *with stage-1 MMU disabled*:
   - 0x18d0c: `MCR p15, 0, r2, c1, c1, 0` (SCTLR ← 0x1100|0xb0, M=0)
   - 0x18d10: `STR r1, [r0]` writes `r1=0x0401b0ce` to `r0=0x04023840`
     (the L2 entry covering VA `0x0c310000..0x0c311000`)
   - 0x18d14: `MCR p15, 0, r3, c1, c1, 0` (SCTLR ← with M=1 again)

   This **remaps VA 0x0c310xxx from PA 0x04026000 → PA 0x0401b000**.
   The old PA still holds the task's stack contents (saved LR=
   0x2594fc at offset 0x304) but is no longer mapped. The new PA is
   a fresh page that's zero at offset 0x304.

4. Kernel returns via SWIBoot epilog `MOVS pc, lr` at 0x3ada6c to
   USR mode, resuming inside `MakeObject` at 0x259600 with the
   task's saved registers (`r11=0x0c310308`, `sp_usr=0x0c3102f0`).

5. `MakeObject`'s epilogue at 0x25961c executes
   `LDMDB r11, {r4-r6, r11, sp, pc}`: tarmac shows
   `MR4 0c310304 00000000` (reads 0 because VA→PA now hits the fresh
   new page), `R pc 00000000`. Guest executes the reset vector
   `B 0x18688` at VA 0, lands in USR mode at `BootOS` — canary #2.

Einstein's probe hits the same SCTLR-off/on pattern 56 165 times
(`MCR p15, 0, r0, c1, c1, 0` at first_pc=0x18690), so the kernel's
`StoreToPhysAddress` path is normal behaviour, **not** a hypervisor-
induced stall. Einstein boots through it; we don't. The divergence
is somewhere else — either:

- The task being resumed is *not* the same task that entered
  `MakeObject` (a context switch happened mid-SWI). Our hypervisor
  restores the registers correctly but the VA→PA mapping under
  those register values changed, so the resume reads garbage. This
  matches what tarmac shows.
- Or the kernel expected to initialise the new page at PA 0x0401b000
  with valid stack contents (copy from old PA?) before the remap,
  and our hypervisor silently broke that sequence. But no direct
  writes to 0x0401b[0-3]xx appear in the window after the remap.

### Update (2026-04-23 follow-up) — root cause is `CopyPhysicalPage` never runs its inner copy

A fresh tarmac-window walk at `/tmp/guest-trace.log` (the one
referenced above) shows the remap path is driven by
`TStackManager::CopyPagesAfterStackCollided` at `0x1f7540`, which
SVC-enters at tarmac time 9823635900000 and calls
`CopyPhysicalPage` at `0x15b8a4` at time 9828946380000 — well before
the kernel's `StoreToPhysAddress` write of `0x0401b0ce` at 9840502600000.
The design is:

1. Allocate new PA (here `0x0401b000`).
2. `CopyPhysicalPage` copies the OLD PA's contents into the NEW PA
   subpage-by-subpage (`PhysSubPageCopy` at `0x18df4`, which toggles
   SCTLR.M off, does 4× `LDM/STM` of 128 bytes via direct PA, toggles
   SCTLR.M on).
3. Kernel rewrites the L2 entry to point at the new PA
   (`StoreToPhysAddress`).
4. Kernel `MCR p15,0,r0,c8,c6,1 / c8,c5,0` — DTLB-by-MVA + ITLB-all
   — at `0x3ad538/0x3ad53c`, invalidating the old VA mapping.
5. Task resumes; LDMDB sees the new PA, which now holds the copied
   stack.

**Tarmac on our hypervisor shows step 2 is architecturally silent:**

```
$ grep -c "00018df4:" /tmp/guest-trace.log   # PhysSubPageCopy entry
0
$ grep -cE "0015b8e[048]" /tmp/guest-trace.log  # inner loop body
0
```

Instead, `CopyPhysicalPage` enters at `0x15b8a4`, runs its outer
loop 4×, and each iteration falls through via:

```
0x15b8c0: LDR r2, [r11, #-0x2c]      ; r2 = subpage-to-copy bitmap
0x15b8c4: LSR r0, r2, r7             ; shift by subpage index r7
0x15b8c8: TST r0, #1                 ; test bit 0
0x15b8cc: BEQ 0x15b8fc               ; ← ALWAYS TAKEN
0x15b8fc: ADD r7, r7, #1             ; next iter without copying
```

The bitmap at `[r11, #-0x2c]` is the saved-r2 slot from
`CopyPhysicalPage`'s own prologue; r2 was the 3rd parameter supplied
by `CopyPagesAfterStackCollided` via:

```
0x1f7658: ldr r2, [sp, #0x10]   ; bitmap from the params struct
0x1f7668: bl 0x1af5ac8          ; → CopyPhysicalPage
```

The value that `0x1f7658` loads traces back to the
`TCopyPageAfterStackCollisionParams` struct's fields 0x1c / 0x14
copied to sp by the prologue at `0x1f7568..0x1f7578`.

Tarmac confirms `0x0401b03e` appears at L2[0x10] of table
`0x04023800` (= VA `0x0c310xxx`) AND at L2[0x18] (= VA `0x0c318xxx`)
— both aliasing to the same PA. So the kernel DID set up two VA
windows into PA `0x0401b000` (the "new" page aliased via a scratch
VA before the remap), but no data ever arrived there because the
subpage bitmap is zero.

**Next-session concrete leads:**

1. Dump the `TCopyPageAfterStackCollisionParams` struct at entry to
   `CopyPagesAfterStackCollided` — log `[r1+0x00..0x28]` from EL2
   on the first SVC-entry at `0x1f7540`. The field at 0x1c is
   the observed-zero bitmap.
2. Who populates that field? Trace back to `CopyPageAfterCollisionSWI`
   at `0x1f796c` (the SWI trampoline) and its caller —
   `SafeUserRequestEntry__13TStackManager` at `0x1f779c`, request
   code dispatch at `0x1f77e8`. Candidate request slots branch to
   `FMCopyPagesAfterStackCollided__13TStackManager` (not in the
   dumped table above; grep `classify-out/code-symbols.txt`).
3. If the request dispatch is emitting a zero bitmap, check whether
   a preceding shadow-stub-emulated byte access (e.g., the
   `ldrb r0, [r4, #0x26]` at `0x1f7578` or `[r4, #0x24]` at
   `0x1f75a8`) is returning 0 where it should return non-zero —
   that would gate whether the kernel goes into the copy-needed or
   copy-not-needed branch.
4. Less likely but possible: the bitmap is legitimately zero because
   the kernel tracks sub-page dirtiness, and on Einstein the dirty
   tracking gets set by some path we don't run. Compare to Einstein
   via probe — e.g., instrument the probe to log `CopyPhysicalPage`
   entries with (r0, r1, r2) and see whether Einstein's bitmap is
   non-zero at the equivalent call.

Original prior-session candidates retained below (largely superseded
by the above, but kept for completeness):

- `fix_stage1_xn_bits` L2-rewrite mask — ruled out: we strip TEX bits
  but nG is 0 in both kernel's and our rewrite, so TLB semantics are
  unaffected. The L2 rewrite happens correctly (`0x0401b0ce` →
  `0x0401b03e`, PA preserved).
- D-cache coherence around MMU-off STR — ruled out: HCR_EL2.DC=1 is
  set on M=1→0 and the direct-PA store of `0x0401b0ce` to
  `0x04023840` is captured by tarmac, so the write path itself is
  coherent. The *missing* path is the sub-page copy that would
  populate PA `0x0401b000`.

Repro / tracing tools:

```bash
# Windowed tarmac around the reset (1.5 GiB file, ~10M lines).
rm -f /tmp/newton-snapshot-*.bin tarmac-window.log
scripts/fvp --tarmac-window \
  target/aarch64-unknown-none-softfloat/release/newton-hypervisor

# Guest-mode only slice.
awk '!/EL2h_n/' tarmac-window.log > /tmp/guest-trace.log
# Writes to MakeObject's saved-LR slot:
awk '/MW. 0c310304/' /tmp/guest-trace.log
# L2-entry updates for VA 0x0c310xxx:
awk '/MW. 04023840/' tarmac-window.log
```

`src/tarmac.rs` emits `<<TRM_START>>` when TRAP_COUNTER crosses
`START_AT_TRAP = 619_900` and `<<TRM_STOP>>` from the canary halt
path; `scripts/fvp --tarmac-window` gates the TarmacTrace plugin on
those UART tokens via `bp.pl011_uart0.toggle_mti`.

## Resolved — FPA CP1 rfc/wfc at 0x392718 (FVP, 2026-04-23)

`FPE_Install` at `0x3928A0` calls a helper at `0x392704` that
conditionally executes `rfceq r1` at `0x392718` and `wfceq r1` at
`0x39272C`. On the A53 both UND because CP1 (FPA) is unimplemented.
Per ARMv8 A-profile B2.2.4, an UNDEFINED instruction whose condition
evaluates false is permitted to either NOP or raise the Undefined
Instruction exception — implementation-defined. FVP chooses to
exception; QEMU raspi3b would NOP. Hence the stall only surfaces on
FVP.

Fix in `src/trap.rs::handle_und`: direct NOP emulation of the four
FPA control/status-register encodings, guarded by the ARM condition
bits in `spsr_und` so false-condition UNDs don't scribble on `Rt`.

```
  RFS  cccc 1110 0011 0000 Rt 0001 0001 0000   (MRC p1, 1, Rt, c0, c0, 0)
  WFS  cccc 1110 0010 0000 Rt 0001 0001 0000   (MCR p1, 1, Rt, c0, c0, 0)
  RFC  cccc 1110 0101 0000 Rt 0001 0001 0000   (MRC p1, 2, Rt, c0, c0, 0)
  WFC  cccc 1110 0100 0000 Rt 0001 0001 0000   (MCR p1, 2, Rt, c0, c0, 0)
```

Read path returns 0 in `Rt`; write path discards the value. Nothing
Newton boot actually runs depends on real FPA state — the control
word only holds rounding mode + trap enables, which no integer-math
boot code consults. All other FPA / CP1 shapes (data ops, load/store
FP regs) still halt as Phase-A trip-wires; we'll add emulation if any
ever fire during boot.

Why not forward to the guest's own FPE emulator? Tried it — it
forwards the UND through `FP_UndefHandlers_Start`, whose dispatch
table at `0x38EA34..0x38EA70` classifies `rfc`/`wfc`/`rfs`/`wfs` as
"not a recognized FP instruction" (bits\[23:20] ∈ {2,3,4,5} all map to
`b 0x38F028`). From `0x38F028` control chains into `ReportException`,
which with our `gDebugger = 1` patch takes the `StopImage` branch and
wedges polling BIO register `0x0F183000` for a halt-acknowledge bit
that nothing will ever set. Direct EL2 emulation sidesteps the entire
chain.

Notes on the surrounding code:

- The probe at `0x392630` calls stubs at `0x39291C` (pre-probe) and
  `0x392924` (post-probe), both 2-word `push {r0,lr}; pop {r0,pc}`
  functional NOPs with no symbol in `_Data_/symbols.txt`. With
  `0x39291C` as a plain return, the `bl`→`mvn r0, #0`→`bne` sequence
  at `0x392650..0x392660` always stores `-1` as the FPA type and
  skips the `rfs r2` probe entirely. That path is consistent with the
  SA-1100 having no FPA; the `rfceq`/`wfceq` calls in the helper at
  `0x392704` are gated on `r0 == 0x81`, which is never true, so those
  instructions are architecturally NOPs and the only reason they
  matter is FVP's choice to UND on a false-condition undefined
  encoding.
- The `gDebugger = 1` ROM patch (`src/rom_patches.rs`, VA `0x13F4`)
  is still correct for the normal boot path — it's only reached from
  `ReportException` in our previous failure mode, which shouldn't
  happen any more now that we don't forward the FPA UND.

Commits: pending bundle.

## Resolved — QEMU `msr spsr_el2` clobbers SPSR_EL1 (2026-04-23)

Boot previously wedged at `DFAR=0x0c001000` in SVC mode on `pop {r4, r5}`
at PC `0x003ae3ec` (inside `SMemCopyToSharedSWI`). Einstein's trace
showed the kernel correctly transitioning SVC → USR between the last
`LocalToGlobalId (svc)` and `TEnvironment::IncrRefCount (usr)`; our
hypervisor stayed in SVC mode at the same code point and immediately
faulted on the user-space `pop` because `SP_svc` happened to land at
the kernel's stack guard.

Root cause (verified with targeted test): **QEMU raspi3b's AArch64
`msr spsr_el2, <val>` from EL2 has a side effect — it clobbers
SPSR_EL1 (= AArch32 SPSR_svc) with the value being written.** The
auto-saved SPSR_EL2 on HVC entry doesn't trigger the bug, only the
explicit `msr` write.

Our `return_to_guest_from_und` in `src/trap.rs` did
`msr spsr_el2, <pre-UND CPSR>` to set up ERET back to the faulting
mode. Since the UND trampoline HVCs from UND mode, the written value
is the pre-UND CPSR — often `0x1d3` (SVC). That pollutes the guest's
live SPSR_svc from USR (set by the user-mode SVC instruction) → SVC.
The kernel's subsequent `movs pc, lr` at the SWIBoot epilog restores
CPSR = SPSR_svc = SVC and stays in SVC at the user caller's return PC.
`SP_svc` happens to be in the stack-guard region, so the first load
faults.

Workaround: UND-path return no longer writes SPSR_EL2. Instead, the
handler writes the target PC to a ROM-resident literal and ERETs into
a small stub in AArch32 UND mode (SPSR_EL2 left at its auto-saved
value of `0x1db`). The stub is
```
  ldr lr, [pc, #0]    ; lr = target PC from literal
  movs pc, lr          ; CPSR = SPSR_und (pre-UND), PC = lr
  <literal>            ; rewritten by Rust handler each ERET
```
The architectural `movs pc, lr` copies SPSR_und (preserved since UND
entry) into CPSR, so the guest's SPSR_svc is never touched by the
hypervisor. Lives at `0x00FFFFE0` in the ROM trampoline region.

Related QEMU bug discovered along the way: AArch64 ERET to AArch32
UND doesn't reliably plumb `x14` into `R14_und`, which is why the
stub uses a literal instead of relying on `ctx.x[14]`.

Also dropped the UND trampoline's SVC-mode bounce (the `msr cpsr_c,
#0xd3 / mov r0, lr / str / msr cpsr_c, #0xdb` sequence at
`UND_TRAMP_OFFSET + 0x44`). `UND_SAVE_LR_SVC_IPA` was never read —
dead code since an earlier refactor — and the bounce would also have
tripped the QEMU bug if another code path had relied on SPSR_svc.

After the fix, cold boot advances from the old 20 k-trace stall to
400 k+ trace events in 60 s. The new loop is in scheduler/alarm land
(`WantSchedule`, `GetClock`, `SetAlarm`, `TDoubleQContainer::Peek`)
with `GetPlatformDriver` being called repeatedly — which ties back to
the original PauseSystem idle-loop stall `INVESTIGATION.md` noted
before the UDF-trap work: `gPlatformDriver` is still NULL.

All 22 guest tests pass (the verification agent added two new
`test_spsr_eret*` regression tests covering the bug).

Commits: (pending bundle).

## Currently at (2026-04-23, post-SPSR-fix + heartbeat tuning)

**Boot reaches ~trace 335 000 in 90 s** (in-USR `CArrayIterator::Init`
or similar task-init code) before non-IRQ progress stops. Two
independent changes in this session:

1. **Timer heartbeat 1 ms → 16 ms** (`src/timer.rs::rearm`). The
   CNTHP fallback deadline was firing every 1 ms to refresh the
   non-trapping tick page. Now that the guest is well past the early
   delay-loop calibration phase, 1 ms was pure IRQ noise; 60 Hz is
   plenty. Trace volume drops ~5×, and previously-hidden USR-mode
   work becomes visible in the trace.

2. **`snapshot::restore_sysregs` now issues `TLBI alle1`** before
   the DSB/ISB. Was missing, and while the stage-1 TLB shouldn't
   carry anything useful across a fresh EL2 boot, it's the right
   shape for a complete sysreg restore.

Cold boot (`rm -f /tmp/newton-snapshot-*.bin; cargo run --release
--features trace,quiet`) now advances through:
- Early ROM init, REx scan, flash identify/init (as before)
- SWIBoot / kernel glue storms around trace 20 k (was the SPSR stall
  before 44578122)
- Deep userland: `InitDomainsAndEnvironments`, `MemObjManager::
  FindEnvironmentId`, `BuildDomainsAndHeaps`, `TUMonitor`,
  `TStackManager::Init`, `TUDomainManager::Init`,
  `TSharedMemMsg::Init`, `TObjectManager::MonitorProc`,
  `CList::Search` / `CListIterator` / `CArrayIterator::Init`
- Eventually the last task's non-IRQ activity stops, IRQ-only loop
  continues (scheduler fires alarms with no task advancing)

The run does **not** hit `PowerOffAndReboot`, `PauseSystem` on a
NULL `gPlatformDriver`, or any DIAG halt — so the old Phase B
stalls (PABT at `0x003AE3E4`, PauseSystem idle loop, `SP_svc` stack
guard) are all resolved. What's unclear is whether the new
post-trace-335k state is a genuine stall or just a long stretch of
userland code that happens to loop in regions without traced
functions. Next steps:

- Bump `HB_PRINT_BUDGET` in `trap_irq` or make heartbeat log only
  on repeated ELR, to sample guest PC during the apparent stall.
- Check whether any task ever resumes traced-function activity —
  run 5+ minutes cold-boot and count distinct traced-fn PCs over
  time.
- If the task is genuinely stuck, install a guest BP near the last
  traced PC (e.g. start of `CArrayIterator::Init`) to catch the
  faulting instruction.

## Partially resolved — snapshot resume PABT loop (2026-04-23)

Symptom was: resume ERETs to the saved PC and the guest immediately
lands at PC=0x0C (PABT vector) in ABT mode and steady-state loops
there. No sync trap ever fires (the guest stage-1 maps VA=0x0C
without execute permission, so the PABT-vector fetch itself PABTs
and the kernel never unwinds).

Three contributing causes; Header `VERSION` bumped to 2:

1. **Autosave PC inside a hypervisor-transient region.** The
   tracer trampoline pool (`0x00900000..0x00E00000`) and the
   hypervisor ROM tail (`0x00FFFF00..0x01000000`:
   UND/SBA/DABT trampolines + UND return stub) are reached via
   transient state — TPIDRURW scratch, RAM save slots, staged
   ERET PC literals, brief mode-switch dances. A snapshot taken
   mid-trampoline captures the PC but not the scaffolding; on
   resume the guest ERETs back into the stub with garbage
   scratch. Fix: `snapshot::maybe_autosave` now skips when
   `ELR_EL2` falls in those ranges.

2. **Autosave in an exception mode with banked SP/LR.** IRQ /
   FIQ / ABT / UND each have their own banked R13 / R14. LLVM
   AArch64 doesn't expose `sp_abt` / `sp_und` / `sp_irq` /
   `sp_fiq` or the matching LRs as named sysregs, and QEMU
   raspi3b's banked-reg plumbing is flaky anyway (CLAUDE.md
   "QEMU banked-register caveat"). Fix: skip autosaves taken
   with the guest CPSR mode in
   `{FIQ=0x11, IRQ=0x12, ABT=0x17, UND=0x1B}`; keep SVC/USR/SYS.

3. **Missing EL1 sysregs in the snapshot Header.** Saved:
   SCTLR, TTBR0, TTBR1, TCR, DACR32, VBAR, CPACR, SPSR_{svc,
   abt, und, irq, fiq}. NOT previously saved:
     - `SP_EL0`   — AArch32 R13_usr / R13_sys
     - `SP_EL1`   — AArch32 R13_svc
     - `ELR_EL1`  — AArch32 R14_svc
     - `MAIR_EL1` — AArch32 PRRR / NMRR (TEX remap under
       short-descriptor) or MAIR0 / MAIR1 (long-descriptor)
   AArch64 ERET to AArch32 does not propagate x13 / x14 into
   the banked R13 / R14 of the target mode, so SP_svc and
   LR_svc have to be staged via `sp_el1` / `elr_el1` before
   ERET; USR via `sp_el0`. Without this, the first SVC-mode
   instruction that touched SP faulted on garbage. MAIR_EL1
   was the silent culprit for stage-1 attribute mismatch when
   the guest had already programmed PRRR/NMRR before the save.
   Fix: save all four in Header, restore in `restore_sysregs`.

Post-fix status: resume works for many SVC / USR-mode snapshots
(e.g. PC=`0x18ddc` in `ZeroPhysSubPage`, PC=`0x14811c` in kernel
code — both advance into forward progress). Some resumes still
break, particularly from PCs in kernel fast paths where we may
still be missing state (FPU registers aren't saved; banked
SP/LR for non-active exception modes aren't saved either). The
workaround "cold-boot every tracer run" is no longer the
required default, but some failure modes remain.

Proper future work:
- Save / restore FPU state (Q0..Q31 + FPSCR).
- Save / restore banked SP/LR for all AArch32 modes via
  raw-encoded `msr`/`mrs` or an AArch32 stub.
- Save / restore CONTEXTIDR_EL1 (ASID) and any other CP15
  state the guest relies on (AMAIR, TPIDR_EL*, PAR_EL1).
- Detect PABT-at-VA-0xC on resume and rewrite the guest's
  stage-1 vector page to executable so at least the failure
  mode reports a usable ESR.

## Previously-current — DABT at 0x003AE3E4 (pre-SPSR-fix)

**Boot previously wedged ~trace 20 640 in 80 s, then DABT'd at guest
PC `0x003AE3E4` (inside `SWIBoot`-land) on `LDR R5, [R13, #0xC]`
with `DFAR = 0x0C001000`.** This was the symptom of the QEMU
`msr spsr_el2` SPSR_EL1 clobber (see "Resolved — QEMU `msr spsr_el2`
clobbers SPSR_EL1" above, landed in 44578122). The preceding trace
showed:

```
trace 20 632  SMemCopyToSharedSWI   (usr)
trace 20 633  SWIBoot               (svc)
trace 20 634  SMemCopyToKernelGlue  (svc)
trace 20 635  ConvertMemOrMsgIdToObj
trace 20 636  LocalToGlobalId
trace 20 637  TObjectTable::Get
trace 20 638  TDoubleQContainer::Add
...
trace 20 644  ConvertIdToObj
trace 20 645  LocalToGlobalId       → DABT on LDR [R13, #0xC]
```

The path is SMem user→kernel transition. Something in the SWI bounce
is leaving SP pointing at a VA the kernel hasn't mapped; needs
root-cause on which frame's SP is stale.

This replaced the earlier `GetPlatformDriver() returns NULL` /
`TPlatformDriver::PauseSystem` idle-loop stall at trace 63 160 — that
was downstream of the shadow-stub flag-corruption bug in
`MemObjManager::PrimGetEnvDomainName` (see resolved-fixes below).
With the UDF-trap emulator in place, PRIM returns correct results,
ksrv env has its domains, and `InitialKSRVTask` / `InitClasses` /
`TLoader::TheMain` progress — so the boot takes a different (and
shorter-through-tracer-events) path before hitting this new stall.

Next step: disassemble around `0x003AE3E4` (user has `/tmp/rom.dis`
from earlier sessions), identify which `SWIBoot` → kernel-glue path
is landing there with a stale SP, and check whether it's the
post-Init reparent → Start path for the newly-runnable
`InitialKSRVTask` (now that PrimGetEnvDomainName returns correctly).
The previous "TLoader never runs" / "Start never called" analysis
in earlier revisions of this doc was downstream of the
flag-corruption bug — superseded by the resolved entry below.

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

## Resolved — shadow-stub flag-preservation via UDF-trap (2026-04-23)

The in-guest shadow-stub approach (emit a trampoline at each
byte/halfword-access site, replace the original word with `Bcc
shadow`) survived every earlier Phase-B hurdle but finally broke on
`MemObjManager::PrimGetEnvDomainName` (ROM `0x0011D2A0..0x0011D34C`).
The PRIM walks a per-env linked list scanning each entry's byte
fields with patched `LDRB` / `STRBcond` instructions and then
dispatches on the flags left behind. Our stub's MMIO-skip gate
(`CMP <scratch>, #0x10000000; BHS skip_xor; EOR <scratch>, #3`)
clobbered NZCV, so the caller's `Bcond` immediately after stub
return took the wrong branch. The guest diverged from Einstein's
behaviour at `0x0011D2C0` (STRBEQ-patched site): Einstein's JIT sees
Z=1 and falls into the fast-match exit at `0x0011D308`; we saw the
wrong-Z path and took the end-of-list-2 exit at `0x0011D328`. The
cascade: PRIM returned "no match" → ksrv env had no domains →
`TUTask::Init` → `NewStack` failed with `-10206` → `TUTask::Start`
skipped → `InitialKSRVTask` never ran → `InitClasses` /
`TNewtWorld::MainConstructor` / `TLoader::TheMain` /
`LoadPlatformDriver` never ran → idle loop hit a NULL
`gPlatformDriver` in `PauseSystem` and DABT'd.

Attempts to fix in-place:

1. **Stack-based CPSR save** (`STMFD SP!, {scratch, flags_scratch}`
   around the CMP, `LDMFD` after the access). DABT'd immediately in
   early boot: `SafeShortTimerDelay` runs before `SetUpStacks`, so
   SP_svc isn't pointing at valid writable RAM yet. Stack-based
   save isn't a universal-mode strategy.

2. **Fixed RAM save slot at a known IPA.** Works pre-MMU (VA=PA,
   slot is in stage-2-mapped RAM) and works in kernel mode post-MMU
   if the kernel linearly maps main RAM (it does). Breaks in user
   mode: user task page tables are domain-restricted and don't
   include a slot we carved out of hypervisor-owned RAM. Broken.

3. **Single CP15 scratch register (TPIDRURW)** holds the scratch
   register OR the flags, not both. Needs two slots.

4. **Second CP15 slot via PMCCNTR** (PL1 R/W unconditionally, PL0
   R/W iff `PMUSERENR.EN=1`). Works architecturally, works in every
   mode + every MMU state. But leaks across preemption: if the
   kernel context-switches mid-stub, TPIDRURW / PMCCNTR get
   overwritten by the next task's stub entry, and when the original
   task is rescheduled the stub's saved state is gone. Newton's
   scheduler doesn't save/restore TPIDRURW or PMCCNTR (SA-1100 had
   neither, so the kernel doesn't know they exist). User-mode stubs
   can't disable IRQs to paper over this (`MSR CPSR_c` from PL0 is
   filtered, the I bit write is a no-op). For the specific
   `PrimGetEnvDomainName` bug the preemption hazard doesn't trip —
   early boot, single task, kernel mode — but it's a latent
   correctness issue for any multi-task user-mode byte-access site.

Fix: **replace the in-guest stub with a UDF-trap emulator.** Each
byte-access site becomes `UDF #(0x8000 | idx)`; the existing UND
trampoline HVCs the trap into EL2, where `shadow_stub::handle_sba_udf`
decodes the original instruction from a side table and emulates the
access in Rust. CPSR flag preservation is trivial — SPSR_EL2 carries
the pre-UDF NZCV across the trap, and EL2 code never manipulates
it. Atomic with respect to guest preemption (EL2 runs with DAIF.I
masked). Works in every mode, every MMU state, every preemption
regime.

Details: see IMPLEMENTATION.md §8.5. Key additions to the hypervisor
were the extended UND trampoline (R12 save via TPIDRURW + faulting-
mode SP/LR capture via a brief mode-switch dance) and the
post-emulation trampoline at `0x00FFFF80` that handles R13/R14
writeback (AArch64 ERET doesn't propagate `x13` / `x14` into the
target mode's banked slots).

After the fix, cold boot advances from the old PrimGetEnvDomainName
stall into `PrimGetDomainInfoByName` → `PrimGetEntryByName` →
`TUSharedMem::CopyToShared` → `SMemCopyToSharedSWI` — 20 000+ trace
events in 80 s. The next stall is a DABT at guest PC `0x003AE3E4`
(inside `SWIBoot`-land, `LDR R5, [R13, #0xC]` faulting at VA
`0x0C001000`) — a new Phase-B frontier, not a regression.

All 20 guest tests (`run-all.sh`) pass, including every subtest of
`test_shadow_stub` (reg-offset, SP-imm/neg/reg/writeback/post-index,
SWPB, LDRD-ignored, RAM-resident).

Commits: (pending bundle).

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
