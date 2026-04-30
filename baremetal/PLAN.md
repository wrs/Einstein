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

**Current goal (iter-52):** iter-51 root-caused the
`evt.ex.abt.bus` wedge in `TStackManager::Fault`: shadow_stub's
liveness analyser had a memoization bug that caused R0 to be
mis-classified dead at WriteChunk's LDRB stub @0x00257080. The fix
landed; boot progresses past WriteRun and hits a NEW wedge in
`FindSuperceeder` → `Lookup` with wild OUT-param r3=0x80000110:

```
*** invariant violation: TFlashStore::Lookup called with wild
    &TObjRef OUT param (bit-31 r3) ***
WILD CALL: seq=#19 r0=0x0c604c04 r1=0x00000027 r2=0x0000000d
           r3=0x80000110 caller_lr=0x000c96cc
```

caller_lr=0x000c96cc is right after `bl FindSuperceeder` at
0x000c96c8. The wild value 0x80000110 is a CPSR (USR mode + N
flag), the same shape as the iter-49 `MRS scratch_fl, CPSR`
clobber pattern but at a different stub site. Likely another
shadow_stub mis-pick where the picker chose R3 (the OUT-param
register) as scratch_fl.

iter-52 should:
1. Identify which shadow_stub site clobbers R3 between Lookup
   call #18 (sane r3=0x0c328eac) and #19 (wild r3=0x80000110).
2. Decide whether the bug is another picker miss (the iter-51 fix
   is general but doesn't cover every site automatically) or a
   site-specific liveness-analysis edge case.

### Iteration 51: WriteRun bus-abort root-caused — visited-set leak in liveness analyser

iter-50's wedge (Throw `evt.ex.abt.bus` from
`TStackManager::Fault @0x001f8534`) traced to a corrupted
`TUnicodeCompressor::count` field (this[+0x9c]=0x20000111 instead
of expected 0..0xff). WriteRun then iterated that runaway count
and walked off the end of the heap region at byte VA=0x0c647003,
triggering a stack-grow fault that ResolveFault refused (FAR was
1 byte past the registered upper bound).

#### Mechanism

Working backwards from the corruption:

1. `WriteChunk @0x00257080` is `ldrb r1, [r4, #160]` — read the
   compressor's flag byte. shadow_stub patches this to `B stub`.
2. The stub's pre/post `MRS R0, CPSR` / `MSR cpsr_f, R0` pair
   uses R0 as `scratch_fl` (where the picker stored "save NZCV
   here") because the picker's liveness analyser misclassified
   R0 as dead.
3. The `MRS R0, CPSR` overwrote R0 with 0x20000110 (the saved
   condition flags + USR mode bits).
4. The next ROM instruction `add r1, r0, #1 @0x0025708c` then
   computed r1 = 0x20000110 + 1 = 0x20000111 and stored it as
   the new `count`.
5. WriteRun later loaded that wild count and looped 0x353
   iterations, eventually reading byte at this+0x3F4 = 0x0c647000
   which is past the stack/heap upper bound → bus abort →
   throw → unhandled.

#### Root cause: visited-set conflated cycle-break with memoization

`live_at_recursive` used a single `Visited` flat list that served
two purposes incompatibly:

- **Cycle break**: revisit-on-current-call-stack returns 0 (no
  new reads from this back-edge). Correct.
- **Memoization**: revisit-of-fully-walked-block returns 0 (the
  first walk already counted the reads). **WRONG** — the caller
  applies `live |= sub & !written` with the CALLER'S local
  `written`. If the original walk visited the block deep inside
  a sub-tree where the caller's `written` masked out R0, that
  R0-read DIDN'T propagate up to the OUTER caller's frame. When
  the outer caller's direct path then tried to walk the same
  block, the visited check returned 0 instead of recomputing
  with the outer's empty `written`.

Concrete failure for `0x00257080`:

```
outer (0x257084)
├── teq r1, sl (R1, R10 read)
└── BNE 0x2570c0 → recurse:
    ├── taken: 0x2570c0 (mov r0, r4 → written |= R0)
    │         → bl WriteRun (R0 caller-saved → written |= R0)
    │         → ... deep walk eventually back-edges to 0x25705c
    │         → 0x25705c walks → ldrb r1, [r4, #160] @ 0x257080
    │           → BNE 0x2570c0 (visited!) →
    │           → fall=0x25708c FIRST visited HERE, deep,
    │             where written includes R0 → R0 read masked
    │             out before bubbling up
    │ This frame returns its live (no R0).
    └── fall: 0x25708c (visited!) → returns 0 ← BUG
   
outer's live |= (taken_no_R0 | 0) & !0 = no R0
```

The OUTER's direct fall path has empty `written`, so a fresh
walk would have correctly reported R0 live. The visited cache
robbed it of that walk.

#### Fix

`Visited` now carries per-block analysis state (`InProgress` →
`Finished(live_in)`). `live_at_recursive` returns:

- `0` when revisiting a block currently being walked (cycle break).
- The cached `live_in` when revisiting a block already fully
  analysed (proper memoization — `live_in(B)` is independent of
  the caller's local `written` because the caller applies its
  mask AFTER receiving the result).

`nzcv_dead_recursive` got the same treatment (encodes its bool
result as 0/1 in the same `Visited::live` array; values never
collide with the `LIVE_IN_PROGRESS = u16::MAX` sentinel).

#### Verification

- `shadow_stub pick @0x00257080` now reports
  `DeadReg sea=R12 sfl=None` (single-scratch with NZCV-dead);
  R0 no longer chosen.
- All 36 shadow_stub unit tests pass, including 2 new iter-51
  regression tests:
  - `liveness_shared_block_does_not_drop_reads_iter_51` — the
    shared-block-via-two-paths pattern that exposed the bug.
  - `liveness_tight_cycle_terminates_iter_51` — confirms the
    in-progress sentinel still breaks tight self-loops.
- All 36 guest tests pass.
- Cold boot no longer Throws `evt.ex.abt.bus`. Boot reaches
  Tmux task and a different wedge (the iter-52 starting point).

#### Why this didn't surface earlier

The existing `pick_scratch_at_rom_0x257080_does_not_pick_r0`
test correctly exercised the simplified pattern but didn't
include the deep BL/back-edge structure that triggers the
visited-leak. The test uses a 16-instruction stream where
the BNE target is a self-contained block; the ROM has the
back-edge to 0x25705c that walks BACK into 0x25708c via a
different path. iter-51 added the missing pattern.

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
