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

Three probes installed; the third caught the aliases.

1. **`TUDomainManager::Get` page-allocator** (HVC #0x53 on `0x00258EFC`).
   28 Get calls in baseline boot, all from `caller_lr=0x001F87C0`
   (AllocNewPage), all count=2, all distinct PageIds. **0
   duplicates.** Get is NOT recycling PAs at the bookkeeping level.

2. **`Remember (static)`** at `0x00258E0C` (added per-PA → first-VA
   tracker to `handle_remember_entry_probe_with`). 7 ENTER lines,
   **0 `Remember ALIAS:` lines** — but the underlying alias detector
   was mis-decoded (treated r3 as a PA when r3 is the TPhys-pointer
   passed unchanged to `GenericSWI`). Even with correct decoding it
   would still miss the kernel-internal paths.

3. **`PrimRememberMapping` at `0x00163480`** — HVC #0x54. **Catches
   all 12 Group-2 aliases.** The signature was originally documented
   as `(env, va, &TPhys, perm)` but per disasm is actually
   `(va=r0, mask=r1, &TPhys=r2, perm=r3)`; the first-iteration probe
   miscoded this, registering false positives on incremental-subpage
   widening (mask=0x3 → 0xf → 0x3f → 0xff for the same VA). Fixed.

   The probe also walks RememberMapping's APCS frame (`fp - 4`) to
   capture the upstream caller of `RememberMapping__FUlN31Uc` (the
   call site that issued the BL into RememberMapping itself). The
   distribution across 13 unique aliased PAs:

   - `0x000d8e3c` (GenericSWIHandler, SWI #12 dispatch): 13 PAs (all)
   - `0x001f775c` (CopyPagesAfterStackCollided #2): 9 PAs
   - `0x001f76bc` (CopyPagesAfterStackCollided #1): 2 PAs

   Group-1 aliases (PA=0x04004000-0x04006000, kernel-globals
   self-mapping) do NOT pass through Prim — they're created by
   direct kernel L2 writes during TTBR0 setup.

### Next iteration — narrow Group-2 + stage-2 trap for Group-1

**Group-2 (Prim catches these — narrow further):**

a. Probe `ForgetMapping` (called from `CopyPagesAfterStackCollided`
   immediately before `RememberMapping`) at `0x001f75f4` to confirm
   whether the OLD VA→PA mapping is actually cleared from L2 before
   the NEW mapping installs. If ForgetMapping leaves stale entries,
   that's the bug; if not, the alias originates from a SWI #12 path
   that doesn't pair with a ForgetMapping.

b. For the GenericSWIHandler/SWI #12 path, walk the SWIBoot save
   area at the kernel-side dispatch entry to recover the user-mode
   caller PC (above the SWI boundary). The likely callers are
   `FMNewStack`, `LockHeapRange`, `UnlockHeapRange` — already
   touched by existing per-allocator patches, so we may need to
   ratchet those (e.g. extend the 4-KiB chunk-size patch to a
   per-allocator exclusivity guarantee).

c. Cross-check Newton's `TUPageManager::Get` PageId encoding —
   `count=2` from the page-allocator means two physical pages per
   PageId. If only ONE is owned by the callee and the other ends
   up unclaimed, a later allocation may re-claim that PA from
   elsewhere → alias.

**Group-1 (stage-2 trap):**

The 3 kernel-globals self-mapping aliases at PA=0x04004000-0x04006000
are written by direct kernel store instructions during TTBR0 setup,
bypassing the entire Remember/Prim layer. Plan: install a stage-2 RO
trap on those PAs, decode each AArch32 store fault, log
`(PC, L2-entry-index, value)`, then commit the write so the kernel
proceeds. Once the (PC, entry, value) triples are captured, decide
between (a) Einstein-port behaviour, (b) ROM patch that splits the
self-map, or (c) hypervisor-synthesised second mapping.

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
