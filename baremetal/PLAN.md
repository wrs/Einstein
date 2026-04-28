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

Two probes installed and run; both refuted hypotheses:

1. **`TUDomainManager::Get` page-allocator** — patches the `teq r0,
   #0` after `bl MonitorDispatchSWI` at `0x00258EFC` with `HVC #0x53`.
   Handler `handle_page_get_probe` in `src/trap.rs`. Logs every
   returned PageId + caller LR; tracks per-id first-caller for dup
   detection.
   - **Result:** 28 successful Get calls in baseline boot, all from
     `caller_lr=0x001F87C0` (= AllocNewPage's bl-Init return), all
     count=2, all distinct PageIds. **0 duplicates.**
   - Conclusion: Get is NOT recycling PAs. Aliasing has a different
     origin.

2. **`Remember (static)`** at `0x00258E0C` — augmented the existing
   `handle_remember_entry_probe_with` with a per-PA → first-VA
   tracker. Logs every (env, va, pa, perm) call; emits `Remember
   ALIAS:` when a PA is later seen at a different VA.
   - **Result:** 7 ENTER lines in baseline boot. **0 `Remember
     ALIAS:` lines.** The 12 verify-mmu aliases all still appeared.
   - Conclusion: the L2 writes that produce the aliases do NOT pass
     through the `Remember (static)` user-shim.

### Next iteration — probe `PrimRememberMapping` at 0x00163480

This is the lower-level L2-write primitive (called from kernel-mode
paths that bypass `Remember (static)`).

Args at entry: `(env=r0, va=r1, &TPhys=r2, perm=r3)`. PA is extracted
as `*(r2+16) >>= 12 << 12`.

Steps:

1. Patch the first word of `PrimRememberMapping` (`mov ip, sp` =
   `0xE1A0_C00D`) with `HVC #PRIM_REMEMBER_PROBE_HVC_IMM` (pick a
   fresh tag, e.g. `0x54`).
2. Wire a `handle_prim_remember_probe` in `trap.rs` that captures
   args, dereferences `&TPhys` to get PA, runs the same per-PA →
   first-VA aliasing tracker, then emulates `mov ip, sp`.
3. Cold-boot, capture `Prim ALIAS:` lines; compare against verify-mmu
   alias enumeration.

**Fallbacks if `PrimRememberMapping` doesn't catch the aliases:**

- Try `PrimRememberPhysMapping` at `0x00163708` (variant taking pre-
  resolved PA).
- Try `PrimRememberPermMapping` at `0x00163920` (perm-only updates).
- If still nothing, escalate to a **stage-2 trap on the L2 backing
  pages**: mark them RO at stage-2, decode each write fault, log
  `(L2 entry index, value)`. That catches direct kernel writes that
  don't go through any Remember-shim — most likely the source of the
  Group 1 kernel-globals self-map aliases.

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
