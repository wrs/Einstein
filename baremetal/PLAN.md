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

**Current state:** Boot reaches the kernel's task-scheduler running
~27 live tasks (newt, OBJM, scrn, pckm, cmgr, …), renders the boot
splash + 32×32 icon (4 `screen.blit` calls), runs ~268 ResolveFault
commits, then halts at a *new* ceiling — a DABT at FAR=0x0cdb2ffc
where the `Fault` matcher passes a TStackInfo whose range doesn't
contain the FAR (info top=0x0cd78000, FAR=0x0cdb2ffc — exactly 4 B
below the next stack at 0x0cdb3000). The previous ceiling at
FAR=0x0cce4400 inside `TIntrpStack::NewState` is **resolved** by
the path-A wrapper rewrite (see below).

The current ceiling: a real matcher mismatch (different problem from
the one the FAR=0x0cce4400 probe ruled out for that earlier site).
Captured by RF-wrap[267]: `r0=-10204 FAR=0x0cdb2ffc info=0x0c124254
norm=0x0cd74000 hard=0x0cd74400 curr=0x0cd75000 top=0x0cd78000
pos=ABOVE-top`. Adjacent stack at 0x0cdb3000 was being initialized;
some near-stack-base pointer arithmetic underflowed by 4 B and the
matcher selected a non-adjacent stack. Likely a `push` past the base
or an `STR Rd, [Rn, #-4]!` whose Rn is the just-allocated stack's
norm.

This is **not** a wild pointer — `0x001a4708` is the very first store
in `NewState__11TIntrpStackFv`:

```
0x001a46f0 NewState:
   push {r4, fp, ip, lr, pc}
   mov  r4, r0              ; self
   ldr  r0, [r0]            ; r0 = self->buf  (= 0x0cce4400)
   mov  r1, #2
   str  r1, [r0]            ; <- DABT here
```

The TStackInfo walker shows `slot[196..197]` covering
`[0x0cce4000, 0x0ccf6000)` with `norm=0x0cce4000 hard=0x0cce4400
curr=0x0cce4400 top=0x0ccf6000`. So the FAR is exactly at the
freshly-allocated stack's `curr`/`hard` boundary — the
NewtonScript interpreter is initializing the bottom of a brand-new
TStack, the access is at the lowest committed subpage, and the
ResolveFault path is somehow returning -10204 anyway. The earlier
"508 KiB above top" framing was off: the FAR fell in the *next*
stack region (slot 196..197), not slot 125..181.

`DAH-FME-ret[2]: r0=0` says the FaultMonitorEntry path *did* report
recovery for FAR=0x0cce4400, but the `bl ResolveFault` at
0x001F_84E0 returned -10204 anyway (R5 in the BusErrorThrow dump =
`0xffffd824` = -10204).

**Probe finding (2026-05-07).** Added `HVC #ResolveFaultRet` at the
exit of `apply_resolve_fault_wrapper` (one word before the final
`pop`). The probe captures `r0`, `r4`=manager, `r5`=info, info bounds,
`r8`=original FAR. For the busError-bound call
at FAR=0x0cce4400 it logs:

```
RF-wrap[2]: r0=0xffffd824 (-10204) FAR=0x0cce4400 mgr=0x0c112cb8
            info=0x0c121b90  norm=0x0cce4000 hard=0x0cce4400
            curr=0x0cce4400 top=0x0ccf6000  pos=in-range
```

So:

- **The matcher passes the correct info\*** (slot[196..197], `top=
  0x0ccf6000`) — *not* the previous slot's. PLAN's earlier theory #1
  is ruled out.
- **FAR=0x0cce4400 is exactly `info[+4]`** (the lowest 1-KiB subpage
  *above* the guard). With base=0x0cce4000 and the wrapper aligning
  FAR to the page boundary, iter 0 fires for FAR=0x0cce4000 (subpage
  0 = guard region) and ResolveFault correctly returns -10203 (FAR <
  info[+4]).
- **Iters 1/2/3 fail because of an off-by-one in the wrapper's `blt`
  encoding** — verified with arm-none-eabi-as. The stub uses
  `0xBAFFFFF5` (imm24 = -11), which assembles to `blt 0x34` from
  PC=0x58 (the `str r0, [r6, #68]`), *not* `blt 0x30` (the `add r0,
  r7, r10, lsl #10` that recomputes FAR). So iters 2/3/4 reuse stale
  `r0` (= -10203 from iter 0, then -10204 from iter 1, …) as the FAR.
  Each subsequent ResolveFault sees garbage and returns -10204.
  `all_failed=1` propagates → busError.

Why this hasn't bitten earlier stacks: `RF-wrap[0]` at FAR=0x0cc754c8
hits `page_base = base+0x6000` (a non-bottom page). Iter 0's FAR=
page_base is in `[hard, top)`, ResolveFault commits subpage 0 → returns
0, `r9=0`. Iters 1/2/3 fail with garbage but `r9` stays 0 → wrapper
returns 0. The bug is invisible to the kernel for non-bottom pages.

The visible-bug case (FAR exactly at info[+4]) lights up specifically
because iter 0 falls into the guard region and there's no other iter
with a valid FAR.

**Why the encoding fix wasn't a one-liner.** Encoding the `blt`
correctly (`0xBAFFFFF4`) makes iter 1 fire with FAR=base+1024 =
info[+4], ResolveFault commits, and the busError goes away — *but*
boot regresses to a ~5x slowdown stuck in DiagBootStub: every fault
through `Fault` then iterates all 4 subpages and claims them, and
some downstream kernel state (per-subpage refcount, RememberMappings,
the FindOrAllocPage allocator) doesn't tolerate the change for
early-boot stacks. The buggy `blt` was effectively running iter 0
only — the wrapper has been a no-op pass-through for non-bottom-page
case all along, and a false-busError generator for the bottom-page
case.

**Path A (this branch's fix, 2026-05-07).** Rewrote the wrapper as
44 instructions (172 B):

  1. Fast-fail to -10203 if orig_FAR < info[+4] (kernel busError on
     guard violation; without this, iters past the guard would
     succeed and the wrapper would return 0 → infinite loop).
  2. Fast-fail to -10204 if orig_FAR >= info[+28].
  3. Otherwise iterate sub_idx 0..4, but skip any far_iter outside
     [info[+4], info[+28]) — only legitimate subpages reach
     ResolveFault.
  4. Propagate any non-zero return immediately (no "all_failed"
     tracking needed; the iter-skip in step 3 means the kernel never
     gets a -10203/-10204 from us, so any non-zero is real).
  5. HvcImm::ResolveFaultRet probe at the exit (still armed).

Result: FAR=0x0cce4400 case resolves (iter 0 = guard, skipped; iter 1
commits the page → r0=0). Splash renders (480×320 + 32×32 icon
blits + assorted overlays). 268 wrapper calls before the next ceiling
(unrelated matcher mismatch at FAR=0x0cdb2ffc). All earlier RF-wrap
calls succeed.

The probe stays armed.

### Stack-VM patches that got us here

Three coordinated fixes around the kernel's TStackInfo / ResolveFault
path, all rooted in the `kSubPagesPerPage=4`, `kSubPageSize=1024`
assumption baked into the original kernel and the way we'd patched
`FMNewStack` for ARMv7's no-subpage-AP world:

1. **`0x001F_9060` / `0x001F_9064` NOPs** (the `stackNormalization`
   formula fix). FMNewStack computes
   `stackNormalization = fBase + (firstFree/kSubPagesPerPage)*kPageSize
                                + firstFree*(kStackSize - kSubPageSize)`.
   With `kStackSize=33792`, `kSubPageSize=1024`, `kSubPagesPerPage=4`,
   the expression simplifies to `fBase + firstFree*kStackSize`. We'd
   patched `kStackSize` to 36864 in FMNewStack but left the divide-by-
   `kSubPagesPerPage` intact — so for non-multiple-of-4 `firstFree` the
   sum was off by `r * kSubPageSize` (up to 3 KiB). NOPing the
   `addmi r0, r0, #3` and `asr r0, r0, #2` makes it
   `fBase + firstFree*kPageSize + firstFree*32768 = fBase + firstFree*36864`
   exact. This single change carried the boot from the gLocaleCache
   wedge through `StartScheduler` and into the boot splash.
2. **`0x001F_9038` reverted from `+4096` → `+1024`** (info[+4] /
   info[+24] geometry). Initially we'd set the hard lower-bound
   `info[+4] = slot_base + 4 KiB` to match the new 4 KiB guard size.
   That made user-mode code expecting the original 1 KiB-guard layout
   trip exBusError on every push 12 bytes below the original bottom-
   of-data: `ResolveFault` saw `FAR < info[+24]` and returned -10203.
   Reverting to `+1 KiB` keeps our `info[+24]` matching Einstein's,
   so the same SP excursion that worked there works here. The
   "guard" can't be hardware-enforced sub-page anyway — the bottom
   4 KiB page commits as a whole on first access — so this is a
   logical guard size, identical to the original kernel's view.
3. **`apply_resolve_fault_wrapper` rewritten with success-tracking.**
   The wrapper iterates 4× over the 1 KiB subpages of a faulting
   4 KiB page so the kernel's per-subpage bookkeeping stays
   consistent. Old policy `bne done` propagated any non-zero
   `ResolveFault` return — so iter 0 of a bottom-page commit (FAR =
   `slot_base + 0`, always below `info[+24] = slot_base + 1 KiB`)
   false-positived as -10203 → busError. New policy: `bgt done`
   propagates real positive errors (e.g. `r0=4` FindOrAllocPage
   failure) immediately, but treats negative returns (-10203 /
   -10204 "subpage out of range") as "this iter doesn't apply,
   skip". A new `r9` flag tracks "any iter succeeded"; on loop
   exit, `movne r0, #0` returns success only if at least one iter
   succeeded — else propagates the error so a wild FAR (whose
   entire 4 KiB page is out of range) actually throws busError
   instead of silently returning 0 and busy-looping.

### Wrapper policy (current)

`bgt` propagates positive errors (e.g. `r0=4` FindOrAllocPage
failure) immediately. Negative returns (-10203 / -10204) are
treated per-iter as "this subpage isn't ours, skip"; an `r9`
flag tracks whether any iter succeeded. On loop exit, return 0 if
any iter cleared `r9`; else propagate the last negative so a truly
wild FAR (all four iters out of range) throws busError instead of
silently returning 0 and busy-looping. Required because under the
1 KiB-guard geometry, iter 0 of every legitimate bottom-page commit
sits below `info[+24]` and naturally returns -10203 — but the
wrapper *must* still distinguish that from "all iters failed". (See
git log for the bne→bgt→bgt+success-track history.)

### Diagnostics added

- `BUS_ERROR_THROW_PC` (0x001F_8534) — `bl Throw` inside
  `TStackManager::Fault`, patched with `HVC #LoudHalt`. Captures
  R0..R14 + banked SP/LR for every mode + FAR_EL1 + the
  ResolveFault return code at the moment the kernel decides to
  throw busError. `handle_loud_halt` now matches the site via
  `caller_lr - 4` so user-mode HVCs (routed via the UND trampoline)
  resolve to the patched site and not the trampoline PC. Also
  reads `DABT_SAVE_PA` to recover the original abort's
  `LR_abt`/`SP_abt`/`SPSR_abt` (the fast DABT trampoline forwards
  to kernel DAH without entering EL2, so the regular `dabt:` log
  never fires for these — but DABT_SAVE_PA gets written by both
  fast and slow trampolines before the kernel handler runs).
  Yields `faulting_PC = LR_abt - 8` (or `-4` for Thumb).
- `dump_tstacks_and_check_invariants` (in `src/trap.rs`) — walks
  `gStackManagerHeap[+4] → TStackManager → domain queue (+0xD0) →
  THeapDomain → slot_array → TStackInfo`, dedupes consecutive
  same-`info` slot entries, prints per-stack `norm/hard/curr/top/
  guard/range`, and flags violations: `guard != 1 KiB`, `info[+24]`
  outside `[hard..top]`, `info[+4]` outside `[norm..top]`,
  pairwise VA-range overlaps. TDoubleQContainer layout decoded
  from `Peek__17TDoubleQContainerFv` / `GetNext` — head_item at
  +0, item_offset at +8, items linked via TDoubleQItem (next at
  +0, prev at +4, container back-ptr at +8).
- DAH-FME-ret probe extended to log every failure (`r0 != 0`)
  plus the first 24 successes — was capped at 24, missing the
  late ones.
- FME-entry probe extended to sample at 100 K-call intervals
  after the first 24, so a long boot still gives a FAR
  distribution.

### Next

1. **Run down the FAR=0x0cdb2ffc matcher mismatch — slots_array
   bookkeeping.** Disasm of `Fault` at 0x001F_84E0 confirms it's
   *not* iterating candidates; it calls `GetStackInfo(manager,
   FAR)` exactly once. The inner walker at
   `GetStackInfo__11THeapDomainFUlPP10TStackInfo` (0x001F_8DE0)
   computes `slot_index = (FAR - pool_start) / 33792` and returns
   `slots_array[slot_index]` directly. So the matcher does what
   it's told; the question is whether the slots_array bookkeeping
   is correct for the FAR.

   For FAR=0x0cdb2ffc with pool_start=0x0c600000:
   slot_index = (0x0cdb2ffc - 0x0c600000) / 33792 = 238. The
   wrapper observed `info=0x0c124254` at this FAR, so the kernel
   returned `slots_array[238] = 0x0c124254`. But that info has
   `norm=0x0cd74000 top=0x0cd78000` and the FAR is way above top.

   The TStack walker (`dump_tstacks_and_check_invariants`)
   reports `slot[212..218]` as the last `info=0x0c124254` run,
   then `slot[219..219]` for a different info, with the last
   non-NULL run at `slot[228..229]` — slots 230..495 are
   NULL by the walker. That contradicts the empirical observation
   that `slots_array[238]=0x0c124254`.

   Two sub-questions to answer next:
   - Is the walker really showing the right state? Add a probe
     that reads `slots_array[238]` directly at
     BusErrorThrow time and compares to the walker's view.
   - If the walker is right and slots_array[238] is actually
     non-NULL between wrapper fire and BusError fire, what
     writes it? The Fault path between wrapper return and
     BusErrorThrow is only register compares (no memory writes
     to slots_array). Either there's a write we're missing, or
     the slot-allocation logic in `FMNewStack` left
     slots_array[238] stale (= 0x0c124254 from a previous
     allocation that overlaps slot 238 in slots_array but
     doesn't logically own that pool VA range).

   The "info[+0]" field for slot[212..218] is `0x0cdb3000` (=
   norm of slot[219]), strongly suggesting `info[+0]` is "max
   reserved end" and this stack has reserved [0x0cd74000,
   0x0cdb3000) of pool VA but only grown its top to 0x0cd78000.
   FMNewStack might be writing slots_array entries for the *full
   reserved range* (slots 212..238 covering [0x0ccd5400,
   0x0cdb9800) in pool VA = 7×33792-strided slot indices that
   "logically" map to the reserved [0x0cd74000, 0x0cdb3000)
   stack-VA range — but the walker only sees the first 7 of those
   slots filled, suggesting there's an off-by-one or a stride
   confusion between `FMNewStack`'s slot writes and `GetStackInfo`'s
   slot reads.

   **Next probe:** add `HVC #SlotsArrayProbe` at FMNewStack's
   slots_array fill loop (around 0x001F_91xx) so we capture
   exactly which slot indices it writes for each new stack, with
   what info pointer. Compare to GetStackInfo's expected indices.
2. **Confirm the rest of the TStack landscape is clean.** Walker
   shows clean 1 KiB guards everywhere except in-flight allocations
   in the highest pool, plus one invariant violation in the
   `[0x0de00000..0x0e600000)` pool's slot[0]
   (`info[+24]=0x0de08000 > top=0x0de07fff`). Investigate whether
   that's a transient initialization state or a real bug.
3. **Long-tail.** Reach NewtonScript's `TInterpreter` boot, full UI
   render. EinsteinProbe is the visual oracle.

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
