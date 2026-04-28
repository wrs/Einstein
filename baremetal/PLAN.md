# Plan — Drive Newton OS to interactive use

## Status

**Current goal: eliminate ALL RAM PA aliases**
User directive (2026-04-28): if any RAM physical page is mapped by two
distinct VAs, things break randomly under our flat AP=011. No other
debugging until aliasing is zero.

For the prior history (Phase B per-stall fixes, FMNewStack 33→36 KiB
patch attempt and revert, deeper alrt-task DABT analysis, RelocHeap
corruption fix, etc.) see git log up to commit
`83634659 baremetal: Remember (static) is also NOT the aliasing
source — pivot to PrimRemember*` and `INVESTIGATION.md` at that
commit. The current file is intentionally pruned to the live task.

**IMPORTANT:** Run the *original ROM code*. Don't introduce patches or
workarounds just to get the run further. Diagnose and fix the actual
problem. *No workarounds, no deferrals, no shortcuts.* No silencing
warnings. Fix all warnings before each commit.

When complete, next goal will be to resume per-stall debugging.

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

## Aliasing elimination — current state

### Inventory at the wedge — 12 RAM aliases in two groups

```
PA=0x04004000  VA=0x0c000000 (L1[0xc0],L2[0x0]) ↔ VA=0x0c002000 (L1[0xc0],L2[0x2])
PA=0x04005000  VA=0x0c003000 (L1[0xc0],L2[0x3]) ↔ VA=0x0c004000 (L1[0xc0],L2[0x4])
PA=0x04006000  VA=0x0c007000 (L1[0xc0],L2[0x7]) ↔ VA=0x0c008000 (L1[0xc0],L2[0x8])
PA=0x04028000  VA=0x0c310000 ↔ VA=0x0c318000  (last pages of stacks #10, #11)
PA=0x0402c000  VA=0x0cc7a000 ↔ VA=0x0cc82000  (8 KiB apart)
PA=0x0402e000  VA=0x0cc9b000 ↔ VA=0x0cca3000
PA=0x0402f000  VA=0x0c318000 ↔ VA=0x0cc7a000
PA=0x04033000  VA=0x0cc82000 ↔ VA=0x0ccad000
PA=0x04034000  VA=0x0cc7f000 ↔ VA=0x0cc82000
PA=0x04035000  VA=0x0c603000 ↔ VA=0x0ccc4000
PA=0x0403a000  VA=0x0ccc4000 ↔ VA=0x0ccca000
PA=0x0403b000  VA=0x0ccc4000 ↔ VA=0x0cccb000
```

(Reported by `verify-mmu` in `src/guest_mem.rs::fix_stage1_xn_bits`,
ratchet-logged with `(PA, VA1, VA2)` per alias-onset.)

**Group 1 — kernel-globals self-mapping** (PAs 0x04004-0x04006).
Created at TTBR0 setup time. The kernel maps its own L1/L2 backing
pages into VA 0x0c000000+ at two offsets each. Kernel-only by intent.

**Group 2 — stack-guard sharing** (the rest). Adjacent stack slots at
33-KiB intervals straddle a 4-KiB boundary; the kernel relied on
ARMv4 subpage AP to sub-divide ownership. ARMv7 has no subpage AP →
both VAs end up RW pointing to the same PA after we flatten to AP=011.

### Investigation progress

Four probes installed; the third+fourth narrowed Group-2 to
deliberate stack-guard sharing.

1. **`TUDomainManager::Get` page-allocator** (HVC #0x53 on `0x00258EFC`).
   28 Get calls; 0 duplicates. Get is NOT recycling PAs.

2. **`Remember (static)`** at `0x00258E0C` (HVC #0x46, augmented
   per-PA tracker). 0 `Remember ALIAS:` lines, but the alias detector
   mis-decoded the args (treated r3 as a PA when r3 is the TPhys-
   pointer passed through to `GenericSWI`). Still wouldn't have
   caught the kernel-internal paths.

3. **`PrimRememberMapping` at `0x00163480`** (HVC #0x54). Caught
   all 12 Group-2 aliases. Signature is
   `(va=r0, mask=r1, &TPhys=r2, perm=r3)`; mask in r1 is the
   incremental-subpage activation mask (same va called repeatedly
   with widening 0x3 → 0xff). Probe walks RememberMapping's APCS
   frame to capture the upstream caller LR. Distribution across
   13 unique aliased PAs:
   - `0x000d8e3c` (GenericSWIHandler / SWI #12 dispatch): 13 (all)
   - `0x001f775c` (CopyPagesAfterStackCollided 2nd RM call): 9
   - `0x001f76bc` (CopyPagesAfterStackCollided 1st RM call): 2

4. **`PrimForgetMapping` at `0x00163514`** (HVC #0x55). Hoisted the
   per-PA → first-VA tracker into module-level statics so both
   probes manipulate the same arrays. A matched forget clears the
   slot; mismatched ones log `FORGET MISMATCH:`. Cold-boot deltas:

   | metric              | iter 1 (Remember) | iter 2 (+Forget) |
   |---------------------|------------------:|-----------------:|
   | `Prim ALIAS:` lines |               106 |               55 |
   | unique aliased PAs  |                13 |               12 |
   | `FORGET MISMATCH:`  |               n/a |                8 |

   The 12 surviving PAs are **real aliases**: kernel installed PA
   at VA1, then at VA2, with no intervening forget. **All 12 come
   through `0x000d8e3c` (GenericSWIHandler, SWI #12)** — i.e.
   user-mode `Remember (static)` calls. The aliased VAs land on
   the 32-KiB stack-stride pattern (e.g. PA=0x04028000 mapped at
   0xc310000, 0xc318000, 0xc320000), confirming **Group-2 stack-
   guard sharing**: the kernel deliberately makes the LAST page of
   stack N the FIRST page of stack N+1 (ARMv4 subpage AP gave each
   stack its own half; ARMv7 collapses to AP=011 → real alias).

   Group-1 aliases (PA=0x04004000-0x04006000) still don't pass
   through Prim — they remain direct kernel L2 writes during TTBR0
   setup.

### Investigation progress (continued)

5. **SWI save-area walk** for user-mode caller identification.
   New helper `read_swi_caller()` reads `(saved_pc, lr_usr,
   user_caller)` from `curr_task + {0x4c, 0x48, 0x3c-walk}`.
   Prim ALIAS lines now log all three. Result across 12 aliased
   PAs:

   | user_caller | function | aliased PAs |
   |---|---|---:|
   | `0x002523bc` / `0x002523d4` | **`TTask::Init`** post-LockHeapRange BL sites | **11** |
   | `0x00124280` | `TMuxStoreMonitor::Init` | 2 |
   | `0x003109e4` | `ExtendVMHeap` | 2 |
   | `0x0c1118c8` | RAM (REx-resident shim) | 2 |
   | + 7 more, 1 PA each | `NewVMHeap` / `LockStack` / `NewDirectBlock` / `TheMain::TLoader` / `TCardAsyncMsg` / 1 RAM | 7 |

   `user_lr=0x00258efc` (inside `TUDomainManager::Get`) for ALL
   aliases — the SWI is dispatched through Get's
   `bl MonitorDispatchSWI` site as part of LockHeapRange's
   per-page resolve-fault path.

   Root cause confirmed: stack allocations via `TTask::Init →
   NewStack → LockHeapRange` deliberately share 4 KiB boundary
   pages between adjacent stacks (33-KiB usable on a 32-KiB VA
   stride). ARMv4 subpage AP let each stack own 1 KiB of the
   shared boundary; ARMv7 has no subpage AP, AP=011 makes both
   stacks' VAs alias the same PA.

6. **Option A (call-site +4 KiB pad) attempt** — implemented as a
   2-word wrapper at `0x00FFFE80` (`add r1, r1, #4096; b NewStack
   thunk`); BL at `0x0025238C` redirected through it. **Result:
   boot wedges in an infinite ResolveFault loop at
   FAR=0xc647003** (3 bytes past `info_bounds.end=0xc647000`).
   The pad bumped the size requested of NewStack but did NOT
   change the kernel's stack-pool slot stride; padded stacks
   overflow into the (N+1)-th slot, exhausting the pool one
   stack early. ResolveFault returns "success" via the existing
   wrapper but the underlying VA is unmapped → abort re-fires
   forever. Patch reverted; wrapper code retained as
   `apply_new_stack_pad_wrapper` (not installed) for future use.
   Baseline restored.

   **Insight:** The call-site pad cannot work alone — pad and
   stride must move together (= the prior 20-patch 36-KiB
   attempt). Resurrecting that attempt with our current Get
   probe (which proved Get returns unique PageIds, contradicting
   the prior "PA recycling" diagnosis) is plausible but a
   substantial undertaking.

7. **Group-1 stage-2 RO trap probe** — implemented `g1_capture`
   module marking PA=0x04004000, 0x04005000, 0x04006000 RO+XN
   at boot, captures every guest write with (PC, offset, value).
   IRQ-only rearm (sync-trap rearm caused an infinite STMIA-retry
   loop). Cold-boot run: 186 captures across 25 writer PCs,
   exit=1 reboot canary, 15 verify-mmu aliases unchanged, 36/36
   guest tests pass.

   **Captures don't reveal alias-creating writes.** The 3 armed
   PAs are *target* pages of the duplicate L2 entries — what
   gets mapped at two VAs — not the L2 PT pages where the
   duplicate L2 descriptors live. Per the prior task-census
   `L1[0xc0]=0x00001401`, the L2 PT for L1[0xc0] sits at
   PA=`0x00001400` in **ROM**; the duplicate descriptors at
   L2[0x0] / L2[0x2] / etc. are pre-baked at ROM build time and
   never dynamically written. Group-1 aliases are static ROM
   artifacts, not runtime kernel decisions.

   Hypervisor self-noise observed: 56 captures at PC=0x00FFFF08
   (UND_TRAMP) writing PA=0x04005000+0xf0c, plus 5 at
   PC=0x00FFFFB4 (DABT_TRAMP) writing +0xfa0 — these are our
   own UND/DABT scratch-slot writes (UND_SAVE_R0_IPA=0x04005F0C,
   DABT_SAVE_PA=0x04005FA0) trapping at stage-2.

### Next iteration — confirm ROM-baked L2 PT, then choose fix layer

Step 1: Add a one-shot dump at end of `stage2::init()` (or at
first verify-mmu fire) reading PA=0x00001400..0x00001500. Log
the first 64 L2 entries. Confirm L2[0x0] and L2[0x2] both
contain PA=0x04004000-derived descriptors; repeat for L2[0x3]/
L2[0x4] and L2[0x7]/L2[0x8].

Step 2: Choose fix layer:
- (a) ROM-byte patches in `apply_717006_patches` overwriting the
  duplicate L2 entries (cleanest if duplicate access isn't
  actually used).
- (b) Stage-2 PA splitting at the duplicate VA: detect the
  duplicate at MMU-enable time, allocate a hypervisor backing,
  copy contents, modify the *guest's* L2 entry at the alias VA
  to point at the new PA. Both VAs remain RW; they no longer
  alias.
- (c) Investigate first: stage-2 trap on the duplicate VAs
  (not target PAs) to enumerate read/write patterns. If both
  VAs are used for distinct data, neither (a) nor (b) works.

Recommend the order (c) → (a)|(b) once access patterns are
characterised.

Group-2's 12 aliases remain parked until Group-1 is zero.
Group-2 will then be revisited with **Option C: stage-2 PA
splitting** — the hypervisor-level approach. When the guest's
L2 maps two VAs to the same PA, transparently allocate a
duplicate stage-2 backing page and re-route one of the VAs.
This avoids fighting the kernel's deliberate boundary-sharing
design entirely; complexity is in COW-style write shadowing.

Until aliases are zero, the alrt-task DABT and any other later wedge
stays **deliberately not investigated**.

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
