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
~27 live tasks (newt, OBJM, scrn, pckm, cmgr, …), draws the boot
splash, and starts rendering small UI overlays. Past the gLocaleCache
wedge, past the StorePermObject / TUnicodeCompressor abort loop, past
the StackManager bus-error stall.

The current ceiling is a **wild data-abort at FAR=0x0cce4400** — 508
KiB above the topOfStack of a 2 MiB stack (slot 125..181 in the main
heap-domain pool). The wrapped ResolveFault correctly returns -10204
"FAR ≥ topOfStack", `TStackManager::Fault` then throws `exBusError`,
no handler is installed, and the kernel hits `UnhandledException`.
Captured via the new BusErrorThrow LoudHalt at 0x001F_8534 + the
TStack invariant walker (`dump_tstacks_and_check_invariants`).

It's **not** a stack-overflow into a guard — the offending VA is half
a megabyte above the top, suggesting a corrupt SP or wild pointer in
some new code path the boot reached for the first time.

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

### Wrapper-policy attempts (worth keeping for next time)

The wrapper went through three iterations before settling. The
intermediate one is interesting because **it was the only state in
which the splash actually rendered** — and the reason it rendered
is informative.

**Attempt A — `bne` propagates any non-zero.**
First version. Halted early at `evt.ex.abt.bus` because iter 0 of a
bottom-page commit (FAR = `slot_base + 0`) is *always* below
`info[+24] = slot_base + 1 KiB` and returns -10203, which the
wrapper then treated as a bus error. False-positive on every
legitimate stack-grow into the bottom 4 KiB page.

**Attempt B — `bgt` propagates only positive errors; negatives
silently return success.**
Reached and *rendered the boot splash*: `screen.blit ENTER
@PC=0x801bd4 pixmap=0xc107d8c ... copied=19200` (full 480×320
paint) plus the 32×32 icon blit from ROM `0x37ab5c`. 26M+ traps of
post-splash kernel idle. **But** when *every* iter returned
negative (the FAR's whole 4 KiB page was out of range — i.e. a
wild access, not a stack grow), the wrapper still returned 0 and
the kernel re-faulted forever (busy loop, no progress). The splash
rendered specifically *because* this bug masked the wild-FAR cases
that would otherwise have thrown busError.

**Attempt C (current) — `bgt` + per-iter success tracking.**
Cleared `r9` on any success; after the loop, return 0 only if at
least one iter succeeded, else propagate the negative error. Wild
FARs now correctly throw busError. The cost is that the Phase-B
ceiling moved from "post-splash idle" to "BusErrorThrow at
FAR=0x0cce4400, before splash render completes" — i.e. we
exchanged a silently-masked correctness bug for a loud halt at a
real defect.

**Lesson:** rendering the splash under Attempt B did not mean the
boot was healthy; it meant the wrapper was hiding the wild-FAR
defect that's still there. Fixing whatever produces FAR=0x0cce4400
(a corrupt SP or wild pointer 508 KiB above the marker stack's
top) is the real path forward, not relaxing the wrapper.

### Diagnostics added

- `BUS_ERROR_THROW_PC` (0x001F_8534) — `bl Throw` inside
  `TStackManager::Fault`, patched with `HVC #LoudHalt`. Captures
  R0..R14 + banked SP/LR for every mode + FAR_EL1 + the
  ResolveFault return code at the moment the kernel decides to
  throw busError. `handle_loud_halt` now matches the site via
  `caller_lr - 4` so user-mode HVCs (routed via the UND trampoline)
  resolve to the patched site and not the trampoline PC.
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

1. **Identify the wild FAR=0x0cce4400 access**. Marker stack is the
   2 MiB pool at `[0x0ca65000, 0x0cc65400)`. The FAR is 508 KiB
   above its top. Likely a corrupt SP or pointer arithmetic gone
   wrong. The `BusErrorThrow` LoudHalt already captures the banked
   USR LR; needs a probe at the actual data-abort entry to recover
   the original faulting PC (the `dabt:` log dedupes by FAR so this
   one isn't logged at the moment).
2. **Confirm the rest of the TStack landscape is clean.** Walker
   shows clean 1 KiB guards everywhere except in-flight allocations
   in the highest pool. Once the wild-FAR is fixed, re-run the
   walker and confirm everything's tidy.
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
