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

**Current goal (iter-74):** boot now stops in a stage-2 permission-
fault loop at guest PC=0x19a84 / 0x19ac0 (`stmia r0!, {r2,r3,r4}` /
`stmia r0!, {r2,r3,r4,r5}` inside `DiagBootStub` at 0x1955c).
ESR=0x9200004f ⇒ EC=0x24 (DABT from lower EL) WnR=1, DFSC=0x0F
(permission fault, level 3). Repeats forever — the kernel's
DataAbortHandler isn't clearing the fault, suggesting r0 points at
a guest VA the kernel believes is writable but stage-2 has marked
RO (most likely the ROM aperture or a kernel-globals page). Next:
capture FAR_EL2 + the running task / guest-mode CPSR to identify
the destination PA, then decide whether stage-2 needs to grow a
new mapping or the kernel patch list needs to redirect the write.

**Background (unchanged from iter-61):** boot used to reach a
quiescent idle at the Newton splash with `newt`=RUN wedged in
`InitTextWalker`. iter-70 cleared the splash wedge; iter-71's
classifier added idiom recognizers but introduced a new wedge
that iter-72 root-caused and fixed; iter-73 then forwarded FPA
UNDs to the kernel's FPE emulator at 0x38d8dc, unblocking the
TFrameSoundChannel codec path. Boot now drives most of the ROM
init / sound subsystem before hitting the DiagBootStub stage-2
perm fault.

### Iteration 73: forward FPA UND to the guest's kernel FPE emulator

#### Method

iter-72 cleared the iter-71 abort wedge, exposing the iter-70
stall: `*** unrecognised UND: insn=0xed2dc203 at PC=0xd2780
SPSR_und=0x60000110`. `0xed2dc203` is the FPA `SFM f4, 1, [sp,
#-12]!` opcode (Store FPA Multiple) — the compiler-emitted prologue
of `Convert__18TFrameSoundChannelFRC6RefVarP10SoundBlock`.
`scripts/disasm-out/rom.dis` shows ~80 SFM/LFM call sites in the
ROM, so per-opcode in-EL2 NOPs would be a maintenance liability.

Newton's UND vector at PA=0x4 originally branches `b 0x1a031f4`
(`FP_UndefHandlers_Start_JT` in the post-ship JT region) which
thunks to `FP_UndefHandlers_Start` at ROM 0x38d8dc — a complete
FPA emulator covering LDF/STF/LFM/SFM, the CDP arithmetic family
(MUFD/ADFD/SUFD/CMF/CMFE/MVF/…), MCR/MRC (FIX/FLT), and the
control/status register accesses. The whole family already has
an in-ROM home; we just need to route there.

`patch_und_vector` (in `guest_mem.rs`) preempts the original
branch with our HVC trampoline, so unhandled FPA opcodes wedge
in `handle_und` instead of reaching the kernel FPE. The fix:
when the faulting insn is FPA-class (cp1/cp2 LDC/STC/CDP/MCR/MRC,
cond ≠ 0xF) and not the existing in-EL2 RFS/WFS/RFC/WFC ctrl-reg
arm, ERET into 0x38d8dc directly — staying in UND mode (SPSR_EL2
unchanged, since the trampoline ended in `msr cpsr_c, #0xdb`).

The trampoline already preserves everything the FPE emulator
expects on entry: orig R0..R12 (R0/R1/R12 reloaded from stash
slots / TPIDR_EL0 at `handle_und` entry; R2 reloaded by the
trampoline itself before HVC; R3..R11 untouched), SP_und (never
written), hardware-saved LR_und = `faulting_pc + 4`, and SPSR_und
= pre-UND CPSR. The FPE emulator's first instruction reads
`LR - 4` to recover the faulting PC, so the trampoline's
LR_und write is consumed correctly.

The FPE emulator forces I=1, F=1 on entry and restores both from
SPSR_und on `ldm sp!, {pc}^`, so the trampoline's pre-HVC
`msr cpsr_c, #0xdb` (which forces F=1 even when the original
mode had F=0) is invisible to the guest after FPE return —
SPSR_und holds the original F bit. Existing in-EL2 ctrl-reg
NOP path is left intact; it runs first and never reaches the
forward arm.

#### Result

- `und: forwarding FPA insn 0xed2dc203 @PC=0xd2780 → kernel FPE
  @0x38d8dc` (SFM at `Convert__18TFrameSoundChannelFRC6RefVarP10SoundBlock`)
- Two more forwards in the same function:
  `0xed9fc108 @PC=0xd2a40` (LDF) and `0xed1bc20c @PC=0xd2cfc`
  (LFM). All three resolve cleanly through the kernel FPE.
- 36/36 guest tests pass.
- Cold boot: progresses well past iter-70/iter-72's stall point.
  The early `und: handle_und first entry` log fires (StrongARM
  CP15 c15 c1 op2=2 NOP), then SystemBootUND, TapFileCntl,
  multiple sound-driver subfunctions (subfn 0x1f, 5, 6, 0xa,
  0xc, 4, 0x13, 0x17, 9, 7, 0x11, 0xd) — TSoundServer comes up.
  The full TScheduler / task table is set up: TNewtWorld /
  TPSSManager / TPCKM / TCommManager / TNameServer / TSoundServer
  / TAlertEventHandler / TScreenDriver / TAppWorld(s) all
  populated. `TNotebook::InitToolbox` runs before the new stall.
- New stall: stage-2 permission-fault loop at guest PC=0x19a84
  / 0x19ac0 inside `DiagBootStub` (memory-fill loops,
  `stmia r0!, {r2,r3,r4}` / `stmia r0!, {r2,r3,r4,r5}`). ESR
  ISS bit[6] WnR=1, DFSC=0x0F = level-3 permission fault.
  Iter-74 territory — no FAR_EL2 captured yet.

### Iteration 72: classify-rom — fn-range clamp on unbounded PC-rel switch

#### Method

Cold-boot wedged at `ELR=0xffffe4 SPSR=0x80000197` (ABT mode),
~18 M HVC #DIAG_TAG firings/run. `FAR_EL1` upper half (=IFAR)
held `0xe7f848f4` — an SBA UDF-marker encoding (slot 0x484), not
an address. Lower half (DFAR) `0x0c10fc2e`. `LR_abt =
0xe7f848f8` = UDF marker for slot 0x488. Suggested an iter-69-
class regression: a literal-pool / function-pointer slot was
being patched as code.

Diagnostic: dumped `SBA_ORIG_PC[0x484..=0x488]`. Slot 0x484/0x485
were real STRBs at 0x3ad370/0x3ad374 (legitimate). Slots
0x486/0x487/0x488 were at 0x3ad580/0x3ad584/0x3ad58c, with
`orig_insn` values `0x003adbb4 / 0x003adedc / 0x003adcb0` — i.e.
*ROM addresses*, not instructions. Cross-reference against
`rom.dis` showed 0x3ad568..0x3ad5f4 is the SWIBoot handler-pointer
table (35 entries, plus a few preceding-tail words). The
classifier was walking those data words as code.

Trace: in `tools/classify-rom/src/main.rs`,
`enumerate_pc_rel_jump_table` (iter-71) seeds 64 worklist roots
when no CMP bound is present. The dispatch in `DynArrayLeaf` at
0x3ad4e4 (`add pc, pc, r1, lsl #2`) is unbounded; only ~14 case-
body slots are real, so seeding 64 starting at 0x3ad4ec swept past
the function's `mov pc, lr` at 0x3ad520 and into the data table at
0x3ad568+. Three table entries decoded as byte-access shapes
(LDRH / LDRSB), so `shadow_stub` patched them with UDF markers,
corrupting the SWIBoot dispatch.

Fix in `enumerate_pc_rel_jump_table`: thread `fn_ranges` through
and clamp seeded slots to the containing function's end via
`find_fn_range`. The cond-code emulator at 0x3add80 (the case
iter-71 was added for) is inside SWIBoot (0x3ad698..0x3ae158);
its 64-slot table at 0x3add88..0x3ade88 stays inside the fn
range and continues to be enumerated correctly.

#### Result

- `byte-access-static.bitmap` popcount: 27913 → 27906 (-7,
  removing the false-positive bits at 0x3ad580/0x3ad584/
  0x3ad58c/0x3ad590/0x3ad5b4/0x3ad5bc/0x3ad5e8 inside the
  SWIBoot pointer table).
- `reach.bitmap` popcount: 2654975 → 2654939 (-36 words: the
  data-table region 0x3ad568..0x3ad5f4 plus a few adjacent slots
  inside DynArrayLeaf that were over-seeded).
- Oracle ⊆ static invariant: 0 missing.
- 36/36 guest tests pass.
- Cold boot: wedge at PC=0xffffe4 is **gone**. Boot reaches the
  iter-70 stall point (`unrecognised UND insn=0xed2dc203 at
  PC=0xd2780` — VFP coprocessor opcode).

<!-- iter-71 (classify-rom — five idiom recognizers: TClassInfo
     trampoline walker, SVC fall-through, multi-insn case bodies
     in unbounded PC-rel switches, SWIBoot-style indexed
     dispatch, PC-rel function-pointer construction. Bitmap
     deltas + walker stats.) pruned per auto-prune. See
     `git log --grep="iter-71"`. iter-72 superseded its
     regression (the unbounded-switch fix swept past
     `DynArrayLeaf`'s fn end into the SWIBoot pointer table). -->

<!-- iter-70 (classify-rom walker fixes that cleared the iter-69
     literal-pool corruption; bitmap deltas / four walker fixes
     in tools/classify-rom/src/main.rs) pruned per auto-prune.
     See `git log --grep="iter-70"`. iter-71 superseded its
     surface (added more idiom recognizers); iter-72 superseded
     iter-71's regression. -->

<!-- iter-69 (ROOT CAUSE: classify-rom + shadow_stub corrupted the
     literal-pool function pointer at 0x35c49c, slot 0x420
     overwriting `0x01b494f4` with SBA UDF marker `0xe7f842f0`.
     Discovered via a USR-mode periodic-dump capture showing
     `pc=0xe7f842f0 lr=0x35c498` — first-PABT moment of
     `InitTextWalker`. iter-70 fixed the underlying classify-rom
     walker bug.) pruned per auto-prune. See
     `git log --grep="iter-69"`. -->

<!-- iter-68 (DataAbortHandler-internal hypothesis falsified — SBA
     UDFs silent in the wedge) pruned per auto-prune. iter-69
     superseded its self-loop hypothesis with the literal-pool
     corruption finding; iter-70 then fixed the underlying
     classify-rom bug. See `git log --grep="iter-68"`. -->

<!-- iter-67 (PABT-recovery hypothesis falsified — bp at
     PrefetchAbortHandler 0x393b84 fired 0 times, dabt-forward
     repeat counter logged 0 events) pruned per auto-prune. See
     `git log --grep="iter-67"`. Both falsified hypotheses
     (iter-66 LDRB-loop, iter-67 PABT-recovery) are superseded
     by iter-69's actual root cause: shadow_stub corrupted a
     literal-pool function pointer at 0x35c49c. -->

<!-- iter-66 (slot 0x424 LDRB hypothesis falsified — the LDRB at
     0x35d110 is never executed during the wedge despite the UDF
     marker `enc_udf(0x8000|0x424) = 0xe7f842f4` matching the
     wedge's `LR_abt`. Coincidence, not causation.) pruned per
     auto-prune. See `git log --grep="iter-66"`. -->

<!-- iter-65 (per-task call-chain tools + splash wedge
     characterised) pruned per the auto-prune maintenance note —
     iter-66 + iter-67 both refer to its `LR_abt = 0xe7f842f4`
     finding and the `MeasureGlyphWidths → DrTextChunk` chain it
     surfaced via `dump_current_chain` / `ctt`. Both hypotheses
     drawn from iter-65 (the LDRB-loop and the PABT-recovery
     loop) are now superseded by iter-67's "DataAbortHandler
     stuck inside its own body" mechanism. See
     `git log --grep="iter-65"` for the full retrospective. -->

<!-- iter-64 (function tracer locates newt past splash, inside
     RunInitScripts/DoBlock) pruned per the auto-prune
     maintenance note. See `git log --grep="iter-64"`. The iter-64
     conclusion that "newt is in DoBlock running NewtonScript" was
     based on first-touch traces; iter-65's live periodic dump
     supersedes it — newt is wedged in DrawSplashScreen, well
     before the post-splash NS block ever runs. -->


<!-- iter-63 (SemOp OpList decoder + scrn wake mapping +
     InitToolbox decode) pruned per the auto-prune maintenance
     note. See `git log --grep="iter-63"` for the full
     retrospective. -->

<!-- iter-62 (per-task APCS stack tracer) pruned per the auto-prune
     maintenance note. See `git log --grep="iter-62"` for the full
     retrospective. -->


<!-- Older iteration retrospectives (iter-61 and earlier) live in
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
