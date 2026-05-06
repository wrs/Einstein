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

**Current goal (iter-105):** iter-104 fixed the "Undefined SWI"
wedge: the iter-102 `mov r1, r0` patch at SWIBoot's dispatch site
(0x003a_d738) only worked for unconditional SVCs. For conditional
SVCs (cond != 0xE), the conditional dispatcher at 0x003a_dd7c
does `mrs r0, SPSR`, clobbering the byteswap-corrected SVC word in
r0 with the caller's CPSR. Replacing the patched instruction with
a proper LDR-byteswap stub (4th member of the family in
`apply_fault_handler_ldr_byteswap_patches`) re-reads the SVC word
from `[r1, #-4]` (with r1=lr from the preceding `mov r1, lr`) and
REVs the result, mirroring the existing iter-101 site at
0x003a_d69c.

Boot now reaches a new (unrelated) wedge:

```
<<TRM_STOP>>
*** unrecognised UND: insn=0xea0061a0 at PC=0x0 SPSR_und=0x330
  src_mode=0x10 (USR)  LR_und=ctx.x[22]=0x4  LR_usr=0x01bdde84
```

(`<<TRM_STOP>>` is just the tarmac-window terminator emitted from
the unrecognised-UND log path itself — no separate kernel
"emergency stop". `0xEA0061A0` is the reset vector's
`B BootOS` word.)

Notes from the diagnostic dump:

- `LR_usr=0x01bdde84` is the post-ship patch-table entry for
  `TaskKillSelf` (`0x003943ac`). Some USR-mode caller did
  `bl 0x01bdde80`, which sets LR=patch_entry+4 and B's into the
  TaskKillSelf body via the patch slot.
- `SPSR_und=0x330` decodes to **USR mode + T=1 (Thumb) + E=1**.
  Newton 2.x is pure ARM — no Thumb code anywhere. Either the
  kernel set up a context with the T-bit accidentally set, or the
  trampoline / banked-state restore path leaked a stale CPSR.
- BootOS canary fired only once (legitimate first boot at line
  163 of the run log). A second fire would have produced
  `BootOS canary fired on entry #2 — software reset detected`,
  but no such line exists. So either (a) the UND fires *before*
  the `B BootOS` at PC=0 actually reaches PC=0x18688, or (b) the
  guest never actually executed PC=0 at all and the UND
  trampoline got tricked into firing through a wild branch
  to VA 0x4 (the UND vector).

Diagnostic progress so far:

- `handle_und` now logs every UND whose source CPSR has T=1
  ("thumb-und" prefix), with full register file. The first such
  UND IS the wedge; nothing earlier flips T.
- `SPSR_svc=0x310` (T=0) at the wedge → the kernel's last
  `movs pc, lr` from the SWI tail returned to ARM-mode user code.
  The interworking happened *afterwards*, in user code itself —
  not in the SVC return path.
- `MonitorEntryGlue` (0x394318)'s `mov pc, r3` is NOT the source.
  A removed-since probe at 0x00394360 logged every indirect call
  for ~345 invocations; r3 was always a valid even-aligned
  function pointer (0x01b15ae4, 0x01b05288, 0x003860fc, 0x01afffb0).
- LR_usr=0x01bdde84 at the wedge matches the patch-table entry
  for TaskKillSelf, but `bl 0x01bdde84` would set
  LR_usr=0x01bdde88. The actual BL was at PC=0x01bdde80
  (TaskGiveObject's patch entry → main-ROM B 0x002596f0 →
  GenericSWI svc 5 path). Whatever wild-branched is much later
  than that BL — LR_usr just hasn't been overwritten since.
- Function tracer (`--features trace`) doesn't reach this wedge:
  it trips an earlier instruction-abort wedge at PC=0x6c343100
  (FIQ mode) caused by the trampoline pool layout.

Diagnostic progress added by the FVP DABT-trampoline fix work:

- A single-site HVC probe at `0x003a_dbb0` (one of the SWI tail's
  three `movs pc, lr`) captured ~225 user-mode resumes before the
  wedge. The LAST one had `lr_svc=0x003a_e3ec, lr_usr=0x0025a644,
  sp_usr=0x0c108ce4`. Wedge state has `lr_usr=0x01bdde84,
  sp_usr=0x0cc89fa8` — different stack and LR, meaning a TASK
  SWITCH happened between that swi-tail and the wedge through a
  *different* `movs pc, lr` site (one of the IRQ tail's at
  `0x0039_2cd0 / 0x0039_2fd4 / 0x0039_306c / 0x0039_3110`, or
  the other two SWI tails at `0x003a_da6c / 0x003a_db10`).
- Multi-site probe attempts (patching all 7 `movs pc, lr` with
  one shared HVC) ran into an emulation gotcha: the kernel
  legitimately writes `mode=0` to SPSR_svc (ARMv4 USR-26
  encoding) and natively the ARMv8 hardware coerces to USR-32
  on AArch32 ERET. From EL2, ERETing via SPSR_EL2 strictly
  follows AArch64 rules (M[4]=0 → AArch64 EL0), so simple
  read-and-replay broke the kernel. Forcing `M[4]=1 | M[3:0]
  = USR(0x10)` made the first `movs pc, lr` ERET correctly
  but the user code subsequently wild-branched to PC=0x2c910e00
  (different from the iter-105 PC=0 wedge), suggesting either
  (a) the multi-site probe perturbs CPSR flags subtly, or
  (b) the wild-branch landing is run-to-run variant. Backed
  out — the right tool for catching the precise interworking
  instruction is whole-execution tracing, not per-ERET probes.

Diagnostic progress from the iter-105 task-switch + pre-ERET probes:

- New HVC probe at `0x003ad9a4` (`add r0, r0, #16` in the kernel's
  task-restore epilog) logs the incoming + outgoing TTask save area
  on every task switch. Compact one-line dump per task:
  `task[in/out] <task_va> id=… '<name>' saved_pc saved_spsr sp_usr lr_usr` plus saved r0..r8 and r12.
- Companion probe at `0x003ada68` (`pop {r0, r1, r2}` immediately
  before the actual `movs pc, lr` at `0x003ada6c`) captures
  `lr_svc` and `spsr_svc` — the EXACT values the hardware will
  consume on ERET — plus the popped r0..r2 that become the user's
  on resume.

Cold-boot run reveals the wedge-causing task is `'drvr'` (id=0x1d03,
TTask at `0x0c113ee0`) being scheduled in for the FIRST TIME. There
is no prior `task[out]` for it, so its save area was populated by
task-creation code (in REx, `0x008006xx` region — the function that
loads the literal `0x64727672` = "drvr" at `0x800740`).

Smoking-gun events #825:

```
task-switch[825]:
  task[in ] drvr saved_pc=0x00800968 saved_spsr=0x00000010
            saved r0..r7 = 0cc89ffc 4 1d03 0 0 0c113ee0 0 0
            saved r8=0 r12=0  sp_usr=0x0cc89fa8 lr_usr=0x01bdde84
task-eret[825]: lr_svc=0x00800968 spsr_svc=0x00000310
                pop=[0x0cc89ffc, 0x4, 0x1d03]
thumb-und: PC=0x0 SPSR_und=0x330 mode=0x10
           r0..r12 IDENTICAL to drvr's saved (zero user instructions ran)
```

Both probes confirm:
- The save area is syntactically correct (saved_pc=0x800968 is a
  valid REx address; saved_spsr=0x10 is clean USR-32 with no flags).
- The kernel's ERET intent is correct: `lr_svc=0x00800968` (bit-0
  clear) and `spsr_svc=0x00000310` (T-bit clear, mode=USR, A=1, E=1
  preserved from the prior SVC entry's CPSR via `msr SPSR_fc`).
- `lr_usr=0x01bdde84` is the standard TaskKillSelf trampoline,
  which Newton uses as the per-task "fall-off" return.

Yet the wedge state has every register matching the save area
EXACTLY — proving the `drvr` user task ran ZERO instructions.
PC=0 with T=1 cannot be produced by any user instruction starting
from PC=0x00800968 with CPSR=0x310 (the LDR at 0x800968 would
change r0; nothing changed).

This rules out task-creation corruption and rules out the kernel
mis-installing the ERET state. The bug must be in the ERET path
itself. Three remaining hypotheses:

1. **Bytes at 0x800968 in the running guest are not `0xe5900000`
   anymore.** Some patcher (NATIVE_PRIM rewriter, shadow_stub,
   unaligned, etc.) rewrote them, and the new instruction takes a
   trap whose handler corrupts CPSR.T and PC. Quick check: dump
   the word at IPA 0x800968 in the wedge handler.
2. **A trap fires immediately on the ERET** — e.g., a vIRQ
   pending, a stage-2 fault on the user's first instruction fetch
   at 0x800968, or a stage-1 access-flag fault — and our trap
   handler's response leaves CPSR with T=1 and PC=0.
3. **Hardware ERET edge case on Cortex-A53** — `movs pc, lr` with
   `spsr_svc=0x310` (E=1 preserved through `msr SPSR_fc`) and
   `lr_svc=0x00800968` somehow doesn't land at PC=0x800968.

Next probe to discriminate: dump `[0x800968]` in the wedge handler
(or as part of the pre-ERET probe), AND check what the actual
post-ERET PC is by adding a UND/DABT probe at a *user-mode* PC
near 0x800968 and seeing whether it ever fires.

The iter-99/101/104 byteswap stubs, the iter-103 VA-space
classifier rework, the iter-105 snapshot-revival fix, and the
iter-105 FVP DABT-trampoline fix (SPSR_abt save + c7-MCR
filter) all remain in place; everything is working as intended
up to the wild-branch wedge.

### Iteration 103: VA-space classifier walker

Goal: rebuild the classify-rom walker to operate on virtual
addresses end-to-end so aliased thunk pages (patch-table,
public-jt, secondary-jt) decode their B-AL targets against the
runtime VA the kernel will actually branch to. Previously the
walker pre-resolved JT VAs to their backing PAs, then decoded the
thunk's B against the PA — wrong destination on every aliased page
where many VAs share one PA.

Major changes (`tools/classify-rom/src/main.rs`):
- New `va_to_pa(words, va) -> Option<u32>`: identity for main ROM
  / REx, L2 walk for patch-table / gROMPublicJumpTable /
  secondary-JT, silent `None` otherwise. Used by indirect-pass
  collectors that scan literal-pool words heuristically.
- New `va_to_pa_loud`: same translation, eprintln-once-per-unique
  on miss. The walker's hot path uses this so any unmapped VA the
  walker reaches surfaces immediately as a missing JT window or a
  misdecoded data branch.
- Walker inner loop: `cur` is a VA throughout; `cur_pa =
  va_to_pa_loud(cur)` for bitmap and `words` access only;
  `Step::Continue / Step::Jump` targets stay as VAs.
- `load_symbol_roots` keeps the symbol's VA (was: resolved to
  thunk PA). The walker pops the JT VA itself; `va_to_pa`
  translates; the walker reads the thunk's B word and `Step::Jump`
  dispatches to the next VA along the chain.
- Drop `resolve_target_to_rom`, `resolve_jt_chain`,
  `PURE_THUNK_PAGES` pre-marking, the post-walk chain-thunk-mark
  pass, and `collect_apcs_prologue_scan_roots` (an iter-100
  workaround for functions the broken walker missed — the rebuilt
  walker reaches them naturally).
- Indirect-pass collectors (vtable, fnptr-literal,
  indexed-dispatch, classinfo, vector-table, FDRV) now seed VAs
  via the silent `va_to_pa` + first-word shape gate.

Result:
- 36/36 guest tests pass.
- `byte-access-static` popcount 27786 → 27790 (essentially
  unchanged — the gain is correctness, not coverage).
- Invariant `oracle ⊆ static` still holds (oracle 2155, static
  27790, 0 missing).
- 70 ROM-soup walk-entries (was 35) — all legitimate ROM-driver
  TClassInfo trampolines at 0x7a5xxx; the user-defined ROM-soup
  range is intentionally over-reaching.
- Boot advances from PC=0x7a56e4 to the iter-104 wedge.

### Iteration 104: SWIBoot dispatch LDR byteswap stub

Goal: clear the "Undefined SWI" cascade by fixing the assumption
behind the iter-102 patch at SWIBoot's dispatch site
(0x003a_d738).

The kernel's SWIBoot does:

1. `ldr r0, [lr, #-4]` at 0x003a_d69c — read the SVC word.
   Iter-101 replaced this with a B-to-stub that LDRs + REVs so r0
   ends up with the original BE-32 SVC word.
2. Check `bits[27:24] == 0xF` (SVC opcode); if cond != 0xE, branch
   to the conditional dispatcher at 0x003a_dd7c.
3. Conditional dispatcher: `mrs r0, SPSR` (clobbers r0), then a
   per-cond table of `tst SPSR_flags / b 0x003a_d6b8 (continue) /
   b 0x003a_dd70 (return)`.
4. Continue dispatch path: `mov r1, lr; ldr r1, [r1, #-4]` —
   re-read the SVC word so we can mask to the imm24.
5. `bic r1, r1, #0xFF000000; cmp r1, #0x23; bge 0x003a_dd50` —
   range-check; out-of-range falls into the "Undefined SWI" debug
   stub.

Iter-102 patched step 4's LDR to `mov r1, r0`, betting that r0
still carried the byteswap-corrected SVC word from step 1. That's
true on the unconditional path but false on the conditional path,
where step 3's `mrs r0, SPSR` overwrites r0 with the caller's
CPSR. Subsequent `bic r1, r1, #0xFF000000` then keeps the low 24
bits of the CPSR — including the mode field — and the cmp+bge
fires against garbage like `0x310` (= USR mode + flags).

Diagnostic probe (HVC at 0x003a_d740 capturing r0/r1/lr_svc/
svc_word) showed the clobber concretely on a `svceq #0x1A` from
USR mode at 0x003940ac:

```
swiboot-dispatch[9]: imm24=0x0310 r0=0x60000310 r1=0x00000310
  lr_svc=0x003940b0 svc_word@0x003940ac=0x0f00001a
```

Fix: replace the iter-102 `mov r1, r0` with a proper LDR-byteswap
stub mirroring the existing three sites (DAH, UND, SWIBoot first
LDR). The stub:

```
ldr r1, [r1, #-4]   ; r1 was lr from the preceding mov r1, lr
rev r1, r1
b   0x003a_d73c     ; resume at original cmp setup
```

After the fix, the same probe shows `imm24=0x001a r1=0x0000001a`
— the imm24 dispatch is correct on conditional SVCs too. Boot
advances through hundreds more SVCs, reaches the kernel's
`<<TRM_STOP>>` marker, and trips a new wedge at PC=0x0 (see
Status above).

Implementation lives in `src/rom_patches.rs`:
- Removed the iter-102 `RomPatch { offset: 0x003A_D738, value:
  0xE1A0_1000, ... }` entry from `PATCHES_717006` (replaced with
  an explanatory comment pointing at the new stub).
- Added `SWIBOOT_DISPATCH_LDR_PC / _ORIG_INSN / _RESUME_PC`
  constants alongside the existing iter-99/101 ones.
- Extended `apply_fault_handler_ldr_byteswap_patches` to allocate
  a 4th 3-word stub and replace the original `ldr r1, [r1, #-4]`
  with a B to it.

36/36 guest tests pass; iteration is hypervisor-functional, not
probe-only, so the run is mandatory per the workflow rule.

<!-- Older iteration retrospectives (iter-98 through iter-102) live
     in `git log` per the auto-prune maintenance note. -->



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
