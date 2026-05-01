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

**Current goal (iter-66):** iter-65 shipped per-task call-chain
tools (`ctt` / `dump_current_chain` / `dump_chain_at` /
`bp_hit_anchor`) and used the live periodic dump to redirect the
wedge investigation. The "tight loop calling only previously-seen
functions" iter-64 saw is **not forward progress in DoBlock — it's
a wedge inside `DrawSplashScreen → MeasureGlyphWidths →
DrTextChunk@0x35d110`**. Mode = ABT, PC = UND_RETURN_STUB,
LR_abt = `0xe7f842f4` literally equals the SBA UDF instruction
word for slot 0x424 (which patched the LDRB at 0x35d110). That
value got into a code register somewhere; the resulting fetch at
high VA `0xe7f842f4` PABTs, the kernel handler can't recover, and
the chain of SBA UDFs in the abort handler itself produces the
~100K-1M trap/s rate at `ELR=0xffffe4`. Concrete iter-65 findings
below; next steps:

1. **Confirm the PABT trigger.** Install a one-shot HVC at the
   AArch32 PABT vector (ROM offset `0x0C`) or at the kernel
   `PrefetchAbortHandler` entry (`0x01A0_0010`) that logs IFAR,
   IFSR, LR_abt, SPSR_abt for the first hit. Hypothesis: IFAR =
   `0xe7f842f4` (or near it). The existing DABT-vector intercept
   in `guest_mem::patch_dabt_vector` is the template.
2. **Trace the LDR-as-data site.** A grep over `rom.dis` for any
   literal pool / computed reference to `0x35d100..0x35d130`
   yields nothing direct, so the bad pointer is computed at
   runtime (table-index pointer arithmetic, vtable lookup,
   glyph-cache key). Once the PABT IFAR is confirmed, set a
   guest BP a few instructions before the wedge fires and walk
   the load chain back.
3. **Decide where the fix lives.** Either (a) the classify-rom
   bitmap incorrectly marked a code/data overlap region as
   sub-word-access code; (b) shadow_stub needs an overlap-detect
   guard before patching; or (c) the LDR site itself is doing
   something un-Newtony that we should special-case. iter-64's
   "newt is in DoBlock" plan is parked until this is resolved.

**Background (unchanged from iter-61):** boot reaches a quiescent
idle at the Newton splash. The framebuffer renders correctly
(`/tmp/newton-fb/00000.png`). All 26 expected tasks alive;
`newt`=RUN, `scrn`=RDY blocked on its event-signal sema-group,
all 24 others BLK. The residual `evt.ex.fr.store` throws are
benign soup-probe misses caught by NewtonScript.

### Iteration 65: per-task call-chain tools + splash wedge characterized

#### Tooling shipped

- `task_dump::dump_current_chain(ctx)` — current-task chain from
  live banked regs (ELR_EL2 leaf, SP/LR/r11 from `ctx` per Table
  D1-79). `#[no_mangle] #[inline(never)]` + `#[used]` pin so gdb
  `call` resolves the symbol post-LTO.
- `task_dump::dump_chain_at(ctx, pc)` — explicit-PC variant for
  guest-BP stops, where ELR_EL2 holds the UND-trampoline PC, not
  the BP'd guest PC.
- `guest_bp::bp_hit_anchor(faulting_pc, ctx)` — empty
  `#[inline(never)]` `extern "C"` shim called from
  `handle_user_bp_und` after the slot lookup. Lets the gdb-init
  `bp <addr>` command set a stable conditional `tbreak
  bp_hit_anchor if ($x0 & 0xffffffff) == <addr>` (vs. the prior
  line-number anchor that drifted into a kprintln macro).
  Carries `ctx` so it's visible at the bp-stop frame after one
  `up`, allowing `ctt`-from-bp to work without walking past
  `<optimized out>` frames.
- gdb-init `ctt [pc]` and `bp <addr>` commands. Output goes to
  the hypervisor serial console, not gdb.
- Mangled C++ names in the symbol pool (build.rs reads
  `_Data_/symbols.txt` and overrides demangled names from
  `code-symbols.txt`). 18925 entries × ~32 bytes/name = 609 KB.
- One-fn-per-line stack rendering (drops the redundant `PC <-
  LR` per row that previously printed each function twice with
  different offsets).
- Periodic heartbeat dump (`trap_irq` → `task_dump::periodic`)
  now also dumps the current-task chain.

#### Key finding: the splash wedge

Cold-boot run, no snapshot, no trace. Periodic dumps land an
identical chain every ~4 s (= state stable across multiple
samples, definitively wedged):

```
current task 0xc12391c (newt) id=0x3113 mode=0x17
   [pc=0xffffe4 lr=0xe7f842f4 sp=0xc004c00 fp=0xcc77894]
        #0  <noncode 0xffffe4>                  ← UND_RETURN_STUB
        #1  <data 0xe7f842f4>                   ← UDF instruction word
        #2  MeasureGlyphWidths__Fl+0x224
        #3  UpdateLayoutState__FlN31+0x88
        #4  DrText__FlN21+0x54
        #5  DoTextOnce_...+0x110
        #6  DrawTextOnce_...+0x4c
        #7  DrawSplashScreen__9TNotebookFv+0x368
        #8  InitToolbox__9TNotebookFv+0x90
        #9..#13  ... TaskEntry → TaskKillSelf
```

Cross-checks:

- `SBA_ORIG_PC[0x424] = 0x35d110` (inside
  `DrTextChunk__FP10DrTextInfolPUsPl`).
- `SBA_ORIG_INSN[0x424] = 0xe4d61001` = `LDRB r1, [r6], #1`.
- Site is solidly inside the function body (preceded by `b
  0x35d57c`, followed by another `LDRB r2, [r6]` for halfword
  assembly).
- `enc_udf(0x8000 | 0x424) = 0xe7f842f4` exactly.

The same `0xe7f842f4` value appears as the saved `LR_abt`. The
timer-IRQ early-trap dump shows `LR_svc=0xe7f842f0` (= UDF word -
4) at the same wedge state — meaning *two* banked LRs are
contaminated by the patched-ROM-word value.

#### What this means

Some path inside `MeasureGlyphWidths` or its callees reads the
ROM word at `0x35d110` *as data* (not by executing the LDRB),
gets the patched UDF marker `0xe7f842f4`, and uses it as a code
address. The fetch at high VA `0xe7f842f4` traps to AArch32 PABT;
the kernel's PrefetchAbortHandler runs in ABT mode and can't
recover (the address is genuinely unmapped at stage-1). The
abort handler's own LDRB/LDRH sites trip SBA UDFs constantly,
producing the ~100K-1M trap/s rate at `ELR=0xffffe4` — that is
NOT forward progress, it's the abort handler grinding through
its own emulated byte accesses without ever exiting.

#### Verification

Two cold-boot runs, chain identical at +4s/+8s/+12s/+16s. SP, FP,
LR all unchanged. Beacon trap counter rises 100K-1M per beacon —
high churn, zero progress.

#### Open question for iter-66

Where is the LDR that reads `0x35d110` as data? Static grep over
`rom.dis` finds no literal pool reference. Most likely runtime-
computed: a glyph-cache pointer table, a "drawer-fn-per-glyph"
dispatch, or unrolled bitmap copy that mistakes a code page for
a font row. iter-66 starts with the PABT-vector probe to confirm
IFAR, then walks the load chain back from there.

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
