# Current-stop handoff

Live notes for the next iteration. Replace this file's body when the
current stop is fixed and a new one takes over — git history is the
archive of past investigations.

## NewtonProbe cross-check: Einstein keeps RelocHeap valid throughout boot (2026-04-28)

Wired the cross-check from the previous iteration's PLAN.md: added
`heap_header_dump` to `baremetal/probe/probe.cpp`, dumping
`heap[0x0ca6b010..+0x80]` every 2 s alongside the existing
`task_dump`. Built with `cmake --build build --target NewtonProbe`,
ran `build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex
60` and captured `/tmp/probe-heap-dump.log`.

Result: Einstein's RelocHeap header is **stable and valid for the
entire 60 s window**. Identical bytes captured at t=2 s, 4 s, ..,
30 s+ (one ephemeral difference at t=2 s — heap[+0x1c]=0x124 vs.
0x90 thereafter — consistent with normal allocator activity, not
corruption). The invariants hold:

```
heap[+0x00]  0x0ca6b000 0x0ca6cc00 0x736b6961 0x0c600c10
heap[+0x10]  0x0ca6b010 0x00000000 0x00000000 0x00000090
heap[+0x20]  0x0ca6cb60 0x0ca6cb60 0x00200000 0x00001c00
heap[+0x30]  0x00003000 0x00001c00 0x00000400 0x00000001
heap[+0x40]  0x00000040 0x0c984314 0x0ca6cb60 0x00000000
heap[+0x50]  0x00000000 0x00000000 0x00000000 0x00000000
heap[+0x60]  0x00000000 0x0c116e7c 0xfffffdec 0xfffffde8
heap[+0x70]  0x00000000 0x00000000 0x00000000 0x00000000
```

- `heap[+0x00]` = 0x0ca6b000 = heap-16 (base) — invariant ✓
- `heap[+0x08]` = 0x736b6961 = 'skia' magic — invariant ✓
- `heap[+0x10]` = 0x0ca6b010 — self-pointer, also valid
- `heap[+0x18]` = 0x00200000 = 2 MiB (heap size) — matches NewHeap #3 args ✓
- `heap[+0x40]` = 0x40 = 64 (constant from NewHeap init) — invariant ✓
- `heap[+0x44]` = 0x0c984314 — pointer into NewHeap #2's region ✓
- `heap[+0x64]` = 0x0c116e7c — TULockingSemaphore wrapper ✓
- VA→PA: VA 0x0ca6b010 → PA 0x040a6010, **never rebound** across
  ~30 dumps (in our hypervisor the same VA hops PA 0x0401f000 →
  0x04032000 partway through boot — that's already a divergence).

**Direct comparison against our wedge-time corrupted dump** (from
the SearchFreeList wild-r0 halt, recorded earlier in this file):

| offset | Einstein     | Hypervisor    | divergence |
|--------|--------------|---------------|------------|
| +0x00  | 0x0ca6b000   | 0x002dd804    | ROM PC into TStoreObjectWriter |
| +0x04  | 0x0ca6cc00   | 0x001a48f0    | ROM PC into TRefStack ctor |
| +0x08  | 0x736b6961   | 0x0c645f0c    | 'skia' magic clobbered |
| +0x0c  | 0x0c600c10   | 0x0cc825d8    | stack-pointer-shaped value |
| +0x10  | 0x0ca6b010   | 0x00000000    | self-pointer cleared |
| +0x14  | 0x00000000   | 0x0cc825e0    | stack ptr |
| +0x18  | (2 MiB size) | 0x0cc82510    | stack ptr |
| +0x1c  | 0x00001c00   | 0x0cc82038    | stack ptr |
| +0x20  | 0x0ca6cb60   | 0x002dfa20    | ROM PC into MakeStoreObject |
| +0x24  | 0x0ca6cb60   | 0x002dd7c4    | ROM PC into TStoreObjectWriter |
| +0x44  | 0x0c984314   | 0x0c9842b4    | similar pointer, different value |
| +0x64  | 0x0c116e7c   | 0x0c116e7c    | match (TULockingSemaphore wrapper) |

**The bug is hypervisor-side.** The kernel correctly creates and
maintains the RelocHeap on Einstein; our hypervisor is overwriting
its header bytes with what looks like a saved exception frame
(saved-LRs + stack pointers). The corrupted +0x00..+0x14 region
matches the layout of an ARM stack frame `{r4, r5, r6, r7, fp, ip,
lr, pc}` from an exception unwind — exactly the pattern visible
in the corrupted bytes (ROM PCs at +0x00/+0x04/+0x20/+0x24, stack
ptrs at +0x0c/+0x10/+0x14/+0x18).

Hypothesis space (per PLAN.md option 2 candidates):

1. **Stage-2 mapping issue** — already extensively investigated.
   The carve-out shows pre-rebind perm faults work but
   post-rebind silence. We know writes are landing through a path
   that misses our perm-fault trap.
2. **CP15 / cache-op handler corrupting RAM through a side
   channel** — possible if a `dccmvac` / `dccsw` is operating
   on the wrong target. Worth listing every CP15 handler that
   touches RAM during the trap window.
3. **shadow_stub byte-access patcher mis-emulating** — the wedge
   PC `0x00f76368` is inside the SBA inline-stub pool, and we
   know shadow_stub rewrites byte/halfword accesses ROM-side. If
   one of those rewrites targets `[r4, #imm]` where r4 happens
   to be the heap base on the failing call, the rewrite would
   corrupt the header.
4. **Banked-register / mode confusion** — when an exception fires
   during heap-allocator code, we save banked regs to a region
   that overlaps the heap header. If task[-16] points into the
   heap header range, our banked-reg save path would clobber
   it. Worth checking what `task[-16]` is at the moment of the
   first heap-watch[3] transition.

The most directly testable hypothesis is #4: cross-check whether
the corrupted bytes look like banked regs we save during exception
entry. heap[+0x00..+0x14] containing ROM PCs + stack pointers
matches that hypothesis well — it IS what an SVC/IRQ frame would
write to a stack region. If task[-16] for the running task aliases
the heap base, every exception entry corrupts the header.

Diagnostic scaffolding kept intact (heap-watch sentinel, stage-2
RO carve-out, sanity check w/ halt re-enabled, stub-orig-PC
decoder).

## Reverted iteration-6/10 symptom workarounds; refocus on root cause (2026-04-28)

Iterations 6 (SearchFreeList wild-r0 → no-fit ELR redirect) and
10 (gFallbackHeap substitution at SetCurrentHeap entry) were
symptom workarounds that let the boot walk past wedges without
fixing the underlying corruption. Both reverted.

## (Earlier) gFallbackHeap substitution gets boot past SetBlockSize, lands on CompactHeap→LockedBlock translation fault (2026-04-28)

Wired option 1 from the previous PLAN.md: in the SetCurrentHeap probe
arm at ROM `0x00142df0`, when `r0 == 0x0ca6b010`, read `gFallbackHeap`
from VA `0x0c101080` and substitute it into `ctx.x[0]` before the
function runs. The kernel's bracketing pattern (NewHeap, NewHandle,
CompactHeap, HUnlock save→use→restore) sees fallback in / fallback
out, so it doesn't get confused.

Result on cold boot:
- Two `SetCurrentHeap: substituted r0=0x0ca6b010 -> gFallbackHeap=
  0x0c111000` events fire (the NewHeap save/restore bracketing).
- Boot progresses past the old SetBlockSize wedge and through five
  successful kernel-DABT recoveries (`DAH-OR[0..4]` for FARs
  `0x0cc79ff4`, `0x0cc7fcc8`, `0x0cc80001`, `0x0cc81000`, `0x0ccc9ffc`).
- Halts at a new `DIAG vector intercept` (HVC #DIAG_TAG from mode
  ABT) with:
  - `pre-fault PC` in shadow_stub's stub pool (slot starting at
    `0xf76940`, fault at `0xf76968` = slot 10 = the access slot).
  - `FAR=0x0c22cba3`, stage-1 walk: L1[0xc2]=0x0401cc61 (coarse),
    L2[0x2c]=0 (fault).
  - `USR lr=0x0031326c` = return into `CompactHeap` after
    `bl LockedBlock` at `0x00313268`.
  - `r4=r9=0x0c111000` = gFallbackHeap (substitution working).
  - `ESR_EL1=0x37` — non-standard DFSC, so handle_diag took the
    "not-forwardable" loud-halt path rather than passing it to the
    kernel's DAH.

So substitution is good enough to keep the boot walking but the
resulting kernel state diverges from a real Newton boot path —
the kernel reaches CompactHeap on the fallback heap, calls
LockedBlock, and that ends up doing a shadow_stub-emulated
byte/halfword access on a VA whose page isn't mapped. That's a
different class of fault than the DAH-handled "lazy-grow" DABTs.

Two lanes for the next iteration:

1. **Decode the new wedge.** The dabt-trip-style stub-orig-PC
   decoder doesn't run on the DIAG_TAG path (different log
   format). Add the same decode in handle_diag's loud-halt arm
   so the new wedge tells us which ROM PC it emulates.
2. **Reconsider the substitution.** Possibly the kernel
   genuinely needs the RelocHeap-specific behaviour (the heap
   has different attribute fields than fallback) and substituting
   `gFallbackHeap` lands us in code paths that expect
   relocatability semantics fallback doesn't provide. Cross-check
   Einstein's RelocHeap usage at this boot offset to see what
   the heap is actually used for.

## SBA-stub wedge decodes to `SetBlockSize`'s `strb r0, [r9]` (2026-04-27, deep night)

Added a stub-orig-PC decoder in `handle_data_abort`'s `dabt-trip`
log: when the faulting ELR is in `shadow_stub`'s SBA inline-stub
pool (`SBA_STUB_POOL_IPA..SBA_STUB_POOL_END`), the dump now reads
slot 14 of the containing 16-word stub (= the back-branch
`B orig_pc + 4`) and decodes the imm24 to recover the original
ROM PC the stub emulates.

For the cold-boot wedge at PC=`0x00f76368`:

```
sba-stub: slot 23949 (base 0x00f76340) emulates ROM PC 0x00312a18
                     (back-branch 0x00f76378 -> 0x00312a1c)
```

ROM PC `0x00312a18` is **inside `SetBlockSize`** (at `0x0031266c`).
The instruction is `strb r0, [r9]` (`0xe5c90000`):

```
312a04: bl 0x1b78c60 <NewBlock>      ; r0 = block ptr
312a08: mov r9, r0                   ; r9 = NewBlock return
312a0c: ldr r4, [sp, #8]
312a10: ldrb r0, [r4]
312a14: bic r0, r0, #4
312a18: strb r0, [r9]                ← wedge (now b 0xf76340 stub)
```

So the wedge isn't a wild branch — the kernel is in `SetBlockSize`
naturally, executing the strb via shadow_stub's emulation stub.
The fault is because `r9` (NewBlock's return value) is a small
constant (`0x3` per the trap dump's `r12=0x3`, which the stub
mirrors from `r9`) rather than a valid block pointer.

The corruption flow is therefore deeper than just "wild branch
into stub pool":

1. Bad RelocHeap stays installed (cascade not stopped at
   SearchFreeList no-fit — task[-16] clear didn't unwind the
   caller's stack copies of the bad heap).
2. NewBlock is called against the bad heap and returns a
   corrupted block pointer (sourced from `heap[+0x48]` = freelist
   position, which on the bad heap holds garbage).
3. SetBlockSize uses the corrupted return value as `r9` and
   does `strb r0, [r9]` → fault.

Strategic implication: the right intervention point is upstream of
NewBlock, not at the SBA-stub wedge. Two candidates:

a. **Reject the bad heap at SetCurrentHeap entry.** The
   SetCurrentHeap probe at ROM `0x00142df0` already detects
   `r0=0x0ca6b010`. Substitute a known-good heap (e.g. set
   `r0 = gFallbackHeap` before letting the function run) so the
   bad heap never gets installed in `task[-16]`.
b. **Validate NewBlock's return.** Hook NewBlock's return path
   (after `ldmdb fp, …, pc`) to validate the returned block
   pointer is in a valid heap range before SetBlockSize uses it.

(a) is upstream and prevents the entire cascade. (b) is downstream
but more surgical — only triggers when the bad heap actually
produces a bad return.

Diagnostic scaffolding from this iteration (kept armed):
- `handle_data_abort` dabt-trip dump now decodes any SBA-stub-pool
  wedge to its original ROM PC.
- `heap_watch::sample` sanity halt downgraded to log-only so the
  boot continues into the SBA wedge for diagnosis.

## Heap sanity checker added; halts cleanly at first sign of corruption (2026-04-27, late late)

`heap_watch::check_heap_sanity` validates two invariants the kernel
never legitimately mutates:

- `heap[+0x00]` = `heap - 16` (block-management prefix sits 16
  bytes before the header).
- `heap[+0x08]` = `0x736b_6961` ("skia" magic literal from ROM
  `0x00310f34`, set by NewHeap at `0x00310e80`).

Other fields (`heap[+0x0C]`, `heap[+0x10]`) start as self-pointers
but the kernel re-uses them for "next heap" / "free-list owner"
links during normal heap-chain management — checking them caused
false positives.

Wired into `heap_watch::sample` after the transition logic. Skipped
when ELR is inside known heap-allocator PC ranges (`0x140000..
0x148000`, `0x310000..0x320000`) so partial updates don't trip
the check. On first trip-wire, dumps the heap header + ring buffer
of recent trap ELRs and halts.

Result on cold boot: sanity check fires at the same ELR=0xffffd8
as `heap-watch[3]` (DABT-trampoline DIAG return), with heap[+0x00]
=`0x002dd804`. This is functionally equivalent to the earlier
heap-watch[3] detection but cheaper and clearer — boot halts on
first corruption rather than running on to the SBA-stub wedge,
and the diagnostic dump shows the corrupted header + ring buffer
in one place.

Useful new context from the ring buffer: the two ELRs immediately
before the wedge are `0x00e4f168` (in `shadow_stub`'s SBA inline-
stub pool — analogous to the 0xf76368 wedge from iteration 6) and
`0xffffd8` itself (recurring DABT-trampoline returns). So the
allocator is repeatedly dispatching through corrupted vtables
into shadow_stub stubs in a tight loop, with each iteration
mutating more of the heap header. The sanity check freezes the
process at the first observable corruption.

For the next iteration, the strategic move is one of:

1. Hook the entry path into the SBA inline-stub pool — when
   `handle_data_abort` sees a fault at IPA in
   `SBA_STUB_POOL_IPA..SBA_STUB_POOL_END` AND the previous-fault
   record shows a different originating PC than the stub's
   intended caller, halt with the wild-branch source PC.
2. Pre-empt the cascade by detecting a corrupted vtable load —
   probe the well-known dispatch sites after `__nw__FUi`'s no-fit
   recovery and validate the loaded function pointer before the
   `bl`/`blx`.
3. Step back: accept that the corruption is downstream of a
   Newton-OS state divergence we don't yet model; cross-check
   Einstein's `__nw__FUi` recovery path to identify what state
   we're missing.

## Active stop: wild jump into SBA inline-stub pool — 2026-04-27 (updated)

`PC=0x00f76368` is **inside `shadow_stub`'s SBA inline-stub pool**
(`0x00E00000..0x00FFFF00`), confirmed via paired `kmain` dumps: the
ROM word reads zero post-`load_rom` and `0xe5cc0000` post-
`patch_rom_from_bitmap`. The bytes form the body of one
shadow_stub inline emulation stub:

```
+0x00 (0xf76368): e5cc0000  strb r0, [r12, #0]     ← faulting (r12=3)
+0x04           : e320f000  nop
-0x04           : e128f001  msr cpsr_c, r1
```

The stub assumes a specific calling convention. Our wild jump
arrives with `r12=0x3` (a stray byte from the corrupted heap), so
the `strb r0, [r12]` writes to IPA 3 → unmapped → halt.

So this is **downstream blast radius** from the original RelocHeap
corruption: the kernel does an indirect call through a clobbered
function pointer that resolves into the stub pool. The user stack
still carries the bad heap (`stack[sp]=0x0ca6b010`), so the call
chain is operating on the corrupted RelocHeap.

The fix lane is now clearer:

- `__nw__FUi`'s no-fit recovery (after our SearchFreeList
  intercept returns r0=0) still has the bad heap in
  `currentTask->globals[-16]`. Subsequent allocator calls keep
  reading from it — eventually one returns a corrupted function
  pointer, and we end up here.
- Extending the SearchFreeList no-fit arm to also clear
  `gCurrentHeap` (set `task[-16]` to 0 or to `gFallbackHeap`)
  would short-circuit the cascade.

Diagnostic scaffolding installed during this iteration:
- `main.rs` post-`load_rom` and post-`patch_rom_from_bitmap` dumps
  at `0x00f76368` to identify which patcher writes there (one-shot,
  cheap, leave armed).
- `dabt-trip` log in `handle_data_abort` now also dumps four
  instruction words (`pc-0x4`, `pc`, `pc+0x4`, `pc+0x8`) via
  both VA and PA — handy for any future wedge whose PC sits past
  the disassembly's coverage.

## Earlier active stop: USR write to IPA=0x3 from PC=0x00f76368 — 2026-04-27

The RelocHeap-header corruption wedge (long history below) is now
side-stepped via a `guest_bp` arm at ROM `0x00313308` that detects
a wild freelist node and ELRs to the function's no-fit exit with
`r0 = 0`. The kernel's `__nw__FUi` then takes its existing
out-of-memory path. Boot progresses ~2400 trace lines past the
old halt and lands on:

```
dabt-trip: PC=0x00f76368 mode=usr writing 0x00000082 -> IPA=0x3
*** unknown MMIO write halted ***
  IPA = 0x00000000  B  value=0x00000082  @ELR=0xf76368
```

Notes:
- IPA=0x3 = NULL+3 byte address (`r12=0x3` is the base reg).
- ELR `0x00f76368` sits in the ROM gap past REx tail and past the
  function-tracer pool (so not in `rom.dis`). Likely an indirect
  dispatch through a corrupted vtable / function-pointer table on
  the bad heap (the user stack still holds `0x0ca6b010` and r4 =
  heap+0x11c).
- `lr_usr=0x00311e1c` is the return into `__nw__FUi`'s caller —
  still operating on the corrupted RelocHeap.

Concrete next steps:
1. Read the four ROM words at `0x00f76368` and surrounding to
   identify what the address actually is. Likely a tracer
   trampoline-style stub OR a region zero-filled at boot that's
   been written to.
2. Trace `lr_usr=0x00311e1c` back to figure out which `__nw__FUi`
   recovery path led us here.
3. Likely upstream fix: extend the SearchFreeList no-fit arm to
   also clear `gCurrentHeap`'s reference to the bad heap so
   future allocations avoid it.

## Earlier stop: SearchFreeList wild-r0 (RelocHeap header corruption) — RESOLVED 2026-04-27

Full chronology kept below for reference. The current stop above
sits downstream of the (now side-stepped) wedge.

## No active stop. Steady-state idle reached (2026-04-27).

A 90 s cold boot with `cargo run --release` (default features, no
`trace*`) reaches the idle pause loop and stays there cleanly. The
last stop — `Swap(NULL, 1)` at ROM `0x3ae204` — was resolved by
mirroring Einstein's `TMemory::WriteP` silent-drop for the ROM
aperture; see the resolved-stops table in `PLAN.md`.

## What "steady-state idle" actually means here

It's the **kernel's** idle pause loop, not the user-facing idle:

- `idle` task RUN at prio 0
- `newt` task RDY at prio 10, queued on `q=0x00000000/0x0c116ed8`
  (some functions/wait queue, not the run queue)
- everything else BLK
- timer IRQ + beacon trap cycle through PCs `0x800a0c` /
  `0x3adb0c` / `0x3ad6f4`

`peripherals/screen.rs::blit` never fires, so `/tmp/newton-fb/`
stays empty. Cross-checked against the existing pre-fix
`trace_once` log at `/tmp/run-trace-once.log` (1477 unique first-
calls, 4147578 total trace events, ending at the SWP-NULL stop):

```sh
awk '/^trace / && !seen[$4]++' /tmp/run-trace-once.log \
  | grep -iE "Screen|Blit|Display|TPlatform|TBits"
```

returns `TPlatformDriver::Init`, `PowerOffSubsystem`,
`PowerOnSubsystem`, `RegisterPowerSwitchInterrupt`,
`EnableSysPowerInterrupt`, `ResetZAPStoreCheck` — but no
`TScreenDriver::*`, `TMainDisplay*`, or `TBlit*`. The display driver
was never instantiated before the wedge, and post-fix the kernel
quiesces on the same path without ever getting there.

## Pending follow-ups

### FVP cross-check: same wedge, same corruption, no QEMU bug. Real cause: writes during the RW window. (2026-04-27, very late)

Built `--no-default-features --features platform-fvp-base quiet`,
ran under FVP for 240 s. The boot progresses (slower wall-clock,
same logical behaviour) and reaches the same SearchFreeList wild-r0
halt with **byte-for-byte identical** corruption in the heap header
(0x002dd804 / 0x001a48f0 / 0x0c645f0c / ...). So the bug is not
platform-specific and not a QEMU stage-2 enforcement issue.

Two real findings from FVP:

1. **prev-trap-elr=0x2dd7bc** on FVP for transition #3 (vs. QEMU's
   0xe4f168). PC 0x2dd7bc is `push {r4, r5, r6, r7, fp, ip, lr, pc}`
   in `__ct__18TStoreObjectWriter`. So the trap immediately
   preceding the wedge-value observation is TStoreObjectWriter's
   prologue push. The push doesn't itself land at the heap (saved-r4
   would be the caller's r4 = `inWrapper`, not 0x002dd804) but it's
   the very last guest activity before the corruption is observed.
2. The earlier "post-rebind perm faults missing" was a counter-cap
   artefact, not a bug. Resetting the dabt-on-carve cap on every
   rebind reveals **256 post-rebind perm faults** correctly logged
   on PA=0x04032xxx in QEMU. None of them target heap[+0x10..+0x18]
   (the corrupted bytes) and none carry value 0x002dd804.

Why the wedge writes escape the trap log: after each perm fault,
`handle_data_abort` line 406 flips the page to RW so the guest's
retry succeeds. Until the next trap fires (and `heap_watch::sample`
re-arms RO), the entire 4 KiB page is RW. The kernel writes many
words in that window — including any subsequent writes to
heap[+0x10..+0x18] — silently. We only capture the *first* write
of each RO→RW cycle. The wedge write to heap[+0x10] sits inside
one of these RW windows.

To capture every write would require either:

- A stage-2 invalid-entry trap (no auto-flip; reads need EL2
  emulation to return the host-PA value).
- An ARM-store decoder that emulates each write in EL2 and keeps
  the page RO (extending `try_emulate_isv0_dabt` to handle ALL
  STR/STM/STRB forms — significant implementation effort).
- Single-step support via `MDCR_EL2.SS` so the auto-flip RW lasts
  exactly one instruction.

For a stop-fixing context, the better lane is PLAN.md option 2 —
make `SearchFreeList` fail gracefully on a wild freelist node so
the kernel takes its existing out-of-memory path and the boot
keeps walking. The corruption itself is a Newton-OS allocator
divergence we can investigate further once we're past this stall.

### Defensive RO-state poll confirms the post-rebind page is RO yet still mutated (2026-04-27, night)

Added `heap_watch::sample` defensive RO-state check at the top of
every trap entry: read the L3 entry for the armed PA, log if its
S2_AP bits aren't `0b01` (RO). Logged 64 events.

**All 64 events are pre-rebind (armed PA = 0x0401f000). Zero are
post-rebind (armed PA = 0x04032000).**

What it means:

- Pre-rebind: the page IS in RW state at most trap entries (the
  existing `handle_data_abort` line 406 path keeps flipping it
  RW+XN after every perm fault; our re-arm flips it RO again on
  the next trap). Captured 64 such RW snapshots — consistent with
  the 256 perm-faults logged previously.
- Post-rebind: the page is in RO state at every trap entry — the
  re-arm path is working. Yet `heap[+0]` reads change value
  (transitions #2 → #3). The writes must be landing at the host
  PA backing of `IPA=0x04032010`, but stage-2 RO is not faulting
  on them.

This rules out "page silently flipped RW by some other code path"
and pins the disagreement to QEMU's stage-2 AP enforcement on
the post-rebind codepath. Hypotheses:

- QEMU caches stage-2 translations per-VA in a way that doesn't
  fully invalidate even after `tlbi vmalls12e1is`.
- QEMU bypasses stage-2 enforcement for some specific access
  pattern (e.g. STM, SWP, or strided multi-word writes that the
  kernel uses inside the heap-allocator inner loop).
- The kernel writes through a different VA whose stage-1 IPA is
  also `0x04032xxx` but where some context (mode? domain?)
  causes QEMU to short-circuit the stage-2 walk.

Trace mode (`--features trace,quiet`) introduces ~3500× per-call
overhead and times out 240s into boot — the wedge isn't reached
in trace mode. So we can't bisect the writer through trace.

Next iteration should commit to ONE of:
1. Run on FVP for architectural ground truth. If FVP shows perm
   faults on the post-rebind PA, document the QEMU bug class
   in `docs/QEMU_BUGS.md` and proceed there.
2. Switch the carve-out from RO-page to invalid-page (clear
   `DESC_VALID`). All accesses (read AND write) fault, including
   the suspected non-faulting writes — but reads need EL2
   emulation to hand back the host-PA value. Larger code change
   but side-steps any AP-enforcement bug.
3. Move past this stop entirely: accept that we can't pinpoint
   the corrupting writer in QEMU, look at whether the wedge is
   silenceable downstream (e.g. force `SearchFreeList` to retry
   when its r0 is wild), and continue the boot.

### Stage-1 RW theory ruled out; post-rebind silence is QEMU stage-2 enforcement (2026-04-27, late evening II)

Added stage-1 walk + L3 readback at every heap_watch transition, plus
unconditional dabt-on-carve trace at handle_data_abort entry. New data:

- Stage-1 entries at every transition (including #3, the corruption):
  - `L1[0xCA] = 0x0401c081` (coarse, domain=4)
  - `L2[0x6B] = 0x0403203e` (small page, **AP=[011]**, XN=0, PA=0x04032000)

  AP=[011] is "Privileged R/W, User R/W" (ARMv7-A short-descriptor B3.7.1).
  Stage-1 grants writes from user mode, so the stage-1-RO theory is
  **ruled out**.

- L3 entry at every transition reads back as `0x000000000183277f`:
  bits[1:0]=11 (valid+page), bits[7:6]=01 (S2_AP=RO), PA=0x01832000.
  RO **is** set in the L3 table at the time of the corruption read.

- Unconditional dabt-on-carve trace (every DABT whose IPA falls on the
  armed page, regardless of class) shows **zero** hits on PA=0x04032xxx
  across the entire boot. All 64 captured hits are pre-rebind on the
  old PA=0x0401fxxx.

- Hammered TLB after rebind with `tlbi vmalls12e1is` (full stage-1+
  stage-2 EL1 flush). No change.

So the L3 entry says RO, the kernel writes RW-eligible bytes via a
stage-1-RW VA, and yet stage-2 doesn't fault. Most plausible
explanation: QEMU's stage-2 permission enforcement is missing for
AArch32-source writes after a TLBI on this codepath. Worth
verifying on FVP, where the architectural model is exact.

Even if the QEMU bug stands, the corruption itself is a Newton-OS
concern, not a hypervisor concern (Einstein boots through this
window cleanly). Better-aimed next steps:

1. **Ditch the RO carve-out for an INVALID-entry trap.** Setting
   the L3 entry to invalid (`!DESC_VALID`) makes both reads AND
   writes fault — at least we'd see one access pattern. Reads
   would need to be emulated (return the value from host PA);
   writes get the writer-PC log we're after. More setup, but
   side-steps the suspected QEMU bug class.
2. **Run on FVP with the carve-out in place.** If FVP shows the
   missing perm faults, this is firmly a QEMU bug (worth a
   `docs/QEMU_BUGS.md` entry) and we move on with the FVP data.
3. **Polling at finer granularity.** `heap_watch::sample` already
   fires every trap; the trap stream between transition #2 and
   #3 averages ~0.5 traps/instruction, but the corruption window
   sits inside a single trap-to-trap gap. Unsatisfying without
   instrumentation, but could narrow with a Tarmac slice on FVP.

### Stage-2 RO carve-out works pre-rebind, mysteriously silent post-rebind (2026-04-27, late evening)

`src/heap_watch.rs` now installs a stage-2 RO carve-out on the
4 KiB page backing `VA=0x0ca6b000` the first time SetCurrentHeap
is called with `r0=0x0ca6b010`. On every guest write to the page,
`handle_data_abort` checks `is_carved_out_ipa`, logs the writer's
ELR + IPA + value (when ISV=1), and arms a re-RO at the next trap
so subsequent writes also fault.

**Major finding: VA → PA rebind.** At boot, VA `0x0ca6b000` maps
to PA `0x0401f000`. Soon after NewHeap finishes init, the kernel
rebinds the same VA to PA `0x04032000` (different host backing).
A fixed-PA carve-out goes stale across the rebind; `maybe_rearm`
walks stage-1 every trap and re-arms on the new PA when the
rebind is detected.

**Pre-rebind sample (256 perm-faults captured, log-cap):**
The kernel writes the heap page from many sites — `0x003108d0`,
`0x00310c18`, `0x00311dec`, `0x003ae1d0`, `0x00259610`, `0x000cb5c8`,
`0x0013d180`, `0x0015e0ec`, `0x0015e228`, `0x003ae204`, `0x003940b4`,
`0x0025ba98`, `0x003ae238`, `0x003ae410`, `0x00149328`, `0x00382714`,
`0x001f8ab8`, `0x001f8b8c`, `0x003826f0`, `0x00310a30`, `0x00130644`,
`0x00259c3c`, `0x00259bbc`, `0x003ae3ac`, `0x003ae3bc`, `0x00143210`,
`0x000cb248`, `0x0038612c`, `0x000e6e68`, `0x00318eec`, `0x000e51b0`,
plus REx PCs `0x00f0ee28`, `0x00f10ee8`, `0x00f75ca8`. All are
allocator-internal — `__nw__FUi`, `NewBlock`, `Acquire`/`SemOp`,
`SetHeapInfo`, etc. — touching the block-data tail of the heap
page (offsets 0x500..0xfff), not the corrupted header window
(offsets 0x10..0x28).

**Post-rebind: zero perm faults captured on the new PA.** L3
entry readback right after the rebind confirms `0x0183277f` —
RW=0b01 (RO), AF=1, valid + page descriptor, host PA `0x01832000`.
Yet between transitions #2 and #3 (heap[+0] going `0x0ca6b000 →
0x002dd804`), no `handle_data_abort` perm-fault path fires for
PA `0x04032xxx`. The heap_watch transition itself observes the
new value with `pa_now = armed = 0x04032000`, so the read goes
through the same PA — ruling out another silent rebind.

Open question for the next iteration: why does the new PA's RO
mapping fail to trap writes? Hypotheses:

- **Stage-1 RO upstream:** the kernel's stage-1 maps `0x0ca6b000`
  RO at user mode, sending writes through stage-1 DABT (handled
  by the kernel's own DAH via the `0x10` trampoline) before
  stage-2 ever sees the access. Verify by reading the L1/L2
  entry for VA `0x0ca6b000` from `heap_watch::sample`.
- **Different VA aliasing:** the corrupting write goes through a
  different VA (e.g., `0x0c000000+` segment paged-mapped) whose
  IPA differs from `0x04032000`. A second alias would then need
  a parallel carve-out — but this would still need to land in
  the 4 MiB stage-2 RAM aperture, which the perm-fault path
  catches.
- **Stage-2 cache or AP-mismatch issue:** the descriptor reads
  back as RO but the hardware TLB is keyed differently. Adding a
  blanket `tlbi vmalls12e1` after `set_ram_page_ro_x` would
  rule it out.

The pre-rebind data already demonstrates the carve-out is sound;
the new investigation thread is "why does the rebind defeat it",
which is itself revealing about how Newton's stage-1 management
interacts with hypervisor-side stage-2.

### Source-tagged ring buffer: 0xffff58 captures are HVC returns, not IRQ noise (2026-04-27, evening)

`heap_watch::Source` (sync vs. irq) is now packed into bit 63 of each
ring slot, with the dump labelling each ELR by source kind. Re-run
shows **all** ring entries — including the four `0xffff58` captures
in transition #3's prelude — are `sync`, not `irq`. So the previous
"IRQ-during-`b .`-loop" reading was wrong.

Re-resolved: ELR_EL2 on an HVC trap from AArch32 holds the
**preferred return address** (= HVC PC + 4), not the HVC's own
address. Working through the trampoline offsets:

- UND trampoline: `HVC #UND_TAG` at IPA 0xffff54 → ELR_EL2 = 0xffff58
  on entry. The ring's `sync` 0xffff58 captures are the standard
  post-HVC sample point of every UND-class emulation.
- DABT trampoline: `HVC #DIAG_TAG` at IPA 0xffffd4 → ELR_EL2 = 0xffffd8
  on entry. The wedge's `sync` 0xffffd8 entry is `handle_diag` doing
  the kernel-DABT forward (consistent with the `dabt: forwarding…`
  log immediately below).
- DABT trampoline: `HVC #ALIGN_TAG` at IPA 0xffffd8 → ELR = 0xffffdc
  on entry. We do NOT see ELR=0xffffdc, so the ALIGN path isn't
  involved here.

So the trap stream just before transition #3 is: REx ↔ kernel cycle
through the (legitimate) `SetBankControlRegister` MMIO loop, then
two UND-class instructions emulated via the trampoline, then a
USR DABT at PC=0x001a4938 (`stmfd sp!, {r3}` — TRefStack ctor's
push) forwarded to the kernel for stack-grow recovery. The
corrupting store happens somewhere in that emulated guest run; pure
guest code, no hypervisor handler involvement.

The trap-only sampling can't narrow further. Next step is a
stage-2 RO carve-out at IPA 0x0ca6b000 so any write to the heap
header takes a stage-2 perm fault with the writing PC in ELR_EL2
and the IPA in HPFAR_EL2 — the precise tool we need.

### Heap-watch sentinel narrows the corruption window (2026-04-27, late+)

A new module `src/heap_watch.rs` samples `heap[0x0ca6b010]` from
both `trap_sync_lower_aarch32` and `trap_irq` on every guest trap,
maintains a 32-slot ring buffer of recent ELRs, and dumps the ring
on every value transition. Cold boot
(`/tmp/run-watch.log`) shows the four expected transitions:

| # | source | from        | to          | elr      | prev-elr |
|---|--------|-------------|-------------|----------|----------|
| 0 | sync   | 0x00000000  | 0x0ca6b000  | 0xffff58 | 0xffff58 |
| 1 | sync   | 0x0ca6b000  | 0x00000000  | 0x392c00 | 0x18d0c  |
| 2 | sync   | 0x00000000  | 0x0ca6b000  | 0x392c00 | 0x18d0c  |
| 3 | sync   | 0x0ca6b000  | **0x002dd804** | 0xffffd8 | 0xffff58 |

Transition #0 is NewHeap initialising the heap struct (we observe it
during a UND-trampoline IRQ). #1/#2 are short-lived zero blanks
during NewHeap init's field-store sequence. **#3 is the corruption.**

The transition-#3 ring buffer dump (newest at index 31):

```
ring[24..28]: 0x800194 / 0x3b32c / 0x3b33c / 0x3b340 / 0x8001a4   (REx ↔ kernel cycle)
ring[29]:     0xffff58   (UND-trampoline body offset +0x58)
ring[30]:     0xffff58   (same)
ring[31]:     0xffffd8   (DABT-trampoline offset +0x30 — the wedge)
```

So the corrupting store fires somewhere between the trap at ELR
0xffff58 (UND trampoline guard) and the trap at ELR 0xffffd8
(DABT trampoline path). The "kernel ↔ REx" cycle just before is:

- 0x3b32c..0x3b340 = body of `SetBankControlRegister__20TBankControlRegisterFUlT1`
  (`ldr r3, [r0]; bic; lsl; orr; str r1, [r0]; ldr r0, [r0]`) — three
  stage-2 traps on the MMIO at IPA 0x0F241000 (one bank past the
  modelled MMIO window 0x0F000000..0x0F200000).
- 0x800194 / 0x8001a4 = REx code calling `SetBankControlRegister`
  in a tight loop (caller LR after each `bl 0x3b324`).

Open questions for the next iteration:

1. Why does the guest ever hit PC=0xffff58? That is the
   `eaff_fffe (b .)` guard at `UND_TRAMP_OFFSET + 0x58` (= base+22),
   the trampoline word right after `HVC #UND_TAG` at +0x54. If
   `handle_und` correctly ERETs to `(original-UND-PC) + 4` we should
   never resume the trampoline body. Two consecutive `0xffff58`
   captures in the ring make this look like a real ERET-target bug
   in some UND emulation path, not just IRQ-during-loop noise.
2. Is the 0xffff58 observation an IRQ (asynchronous, guest stuck in
   `b .`) or a sync trap (something at offset +0x58 that traps —
   shouldn't happen)? The current ring buffer doesn't preserve the
   source label per entry. Add source tracking next.
3. Cross-check Einstein at the equivalent boot offset
   (`build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex
   30`) — does Einstein's run hit the same MMIO loop at the same
   point in time? If yes, the kernel ↔ REx cycle is normal and the
   bug is downstream; if no, the divergence pinpoints the trigger.

### `0x0ca6b010` is the legitimate RelocHeap; its header is the corruption (2026-04-27, late)

NewHeap-entry / SetCurrentHeap-entry / TRefStack-NewStack-exit probes
(`src/guest_bp.rs::handle_user_bp_und`, installed from `kmain`)
disambiguate where the bogus heap pointer enters the system. Cold boot
log `/tmp/run-probe.log`:

**NewHeap is called 7 times.** Bases and sizes:

| #  | base       | size       | caller-lr  |
|----|------------|------------|------------|
| 0  | 0x0c200c00 | 1 MiB      | 0x001423f8 |
| 1  | 0x0c600c00 | 3.5 MiB    | 0x001423f8 |
| 2  | 0x0c984000 | 896 KiB    | 0x001423f8 |
| 3  | **0x0ca6b000** | **2 MiB** | 0x001423f8 |
| 4  | 0x0d601000 | 1.003 MiB  | 0x001423f8 |
| 5  | 0x0d601000 | 1.027 MiB  | 0x00142908 |
| 6  | 0x0ccac800 | 50 KiB     | 0x001423f8 |

Heap #3's header lives at `0x0ca6b010` (= base + 16). Immediately
after creation, NewHeap calls `SetCurrentHeap(0x0ca6b010)` from
`lr=0x00310e60` (the entry switch inside NewHeap). So `0x0ca6b010`
is **the legitimate RelocHeap pointer**, not a wild value.

**SetCurrentHeap is called many times with `r0=0x0ca6b010`.** Caller
LRs (and the function each LR sits in):

- 0x00141c40 — inside `NewHandle`'s heap-switch sequence
- 0x001415d4 — `NewHandle` immediately after `bl SetHeap`
- 0x0031325c, 0x003132cc — `CompactHeap` save/restore around its body
- 0x00141ef0 — inside `HUnlock`

So the kernel actively switches to RelocHeap during normal handle
allocation and compaction. The wedge isn't "bogus pointer in
globals[-16]" — it's that the heap's content has been corrupted.

**TRefStack-NewStack-exit fires 3 times before the wedge**, each time
with `r0=0` (= success per `TRefStackFv` ctor's `teq r0,#0; beq` at
0x1a4950). So NewStack is functioning correctly — the SVC-return path
is intact and stack growth is happening.

**The corrupted heap header at 0x0ca6b010 (128 bytes):**

```
+0x00  0x002dd804 0x001a48f0 0x0c645f0c 0x0cc825d8
+0x10  0x00000000 0x0cc825e0 0x0cc82510 0x0cc82038
+0x20  0x002dfa20 0x002dd7c4 0x0c6437b4 0x0cd51800
+0x30  0x0cd51800 0x0cd51c98 0x0000012c 0x00000001
+0x40  0x00000040 0x0c9842b4 0x002dfa20 0x00000000
+0x50  0x00000000 0x00000000 0x00000000 0x00000000
+0x60  0x00000000 0x0c116e7c 0x00000000 0x00000000
+0x70  0x00000000 0x00000000 0x00000000 0x00000000
```

`+0x40 = 0x40` matches NewHeap-init's `r0 = #64; str r0, [r7, #64]`.
`+0x64 = 0x0c116e7c` is the heap-store TULockingSemaphore wrapper.
`+0x44 = 0x0c9842b4` is a pointer into NewHeap #2's region. So the
heap was correctly initialised at one point.

The corruption is concentrated in `+0x00..+0x14` (looks like saved
ROM PCs / stack pointers) and `+0x48`/`+0x60` (freelist position
clobbered to a ROM PC = `0x002dfa20`). SearchFreeList walks the
freelist starting at `heap[+72] = 0x002dfa20` and dereferences
`*0x002dfa24 = 0xe52d006c` (= encoding of `str r0, [sp, #-108]!`),
which is what causes FAR=0xe52d006c.

Newt's actual user stack is at 0x0cc82xxx (sp_usr=0x0cc81f04 at the
fault). The RelocHeap is at 0x0ca6b000..0x0cc6b000 (2 MiB). They
do **not** overlap — the corruption isn't direct stack/heap aliasing.

Diagnostic scaffolding installed (all stay armed via re-occupy-slot
in `handle_user_bp_und`; capped at 32 lines per probe but
unconditionally log on the wedge-relevant matches `r0=0x0ca6b010` for
SetCurrentHeap and `r0=0x0ca6b000` for NewHeap):

- `0x00313308` SearchFreeList wild-r0 dump (halt)
- `0x001a4948` TRefStack post-NewStack r0/r4/sp/lr
- `0x00142df0` SetCurrentHeap entry r0/lr
- `0x00310e24` NewHeap entry r0(base)/r1(size)/lr

Concrete next steps:

1. Identify the WRITE that lands `0x002dd804` at heap[+0]. Add a stage-2
   write-watch on `0x0ca6b010..0x0ca6b020` (or a guest-side gdb
   data-breakpoint via QEMU's WP) that fires on the first store.
   Cross-check the writing PC against a function that thinks it's
   pushing a stack frame.
2. Walk the heap once at NewHeap-exit (extra probe) and again at the
   first SearchFreeList hit on RelocHeap to bracket when the
   corruption appears in time.
3. Run Einstein at the same boot offset and dump the equivalent heap
   region for comparison. If Einstein's RelocHeap survives, the bug
   is hypervisor-side (stage-2 mapping / aliasing); if Einstein
   corrupts it the same way, it's a ROM-level data-flow bug we need
   to mirror or work around.

### Earlier: Bus-error origin pinned to PC=0x00313308, FAR=0xe52d006c (2026-04-27, evening)

Cold-boot-with-quiet log (`/tmp/run-quiet.log`) plus a hypervisor-side bp
at SearchFreeList's `ldr r3, [r0]` (ROM 0x00313308; install in `kmain`,
emulation+halt-on-wild-r0 in `handle_user_bp_und`) gives a single dump
when the bus-error chain fires:

- DAH-OR[8] FAR=0xe52d006c, current task = newt.
- Faulting PC 0x00313308 in `SearchFreeList` (ROM 0x003132d8).
- ctx at the fault: r0=0xe52d006c, r1=0x0ca6b010, r4=0x7c (request
  size), r9=0x0ca6b12c (= r1+0x11c), lr_usr=0x003132ec (back in the
  same fn), fp=0x0cc81f14, sp_usr=0x0cc81f04.

The "heap" at r1=0x0ca6b010 is NOT a real heap. Its 128-byte header has
ROM PCs at +0x00, +0x04, +0x20, +0x24 instead of vtable / size / start /
freelist position:

```
+0x00 0x002dd804  ; instruction inside __ct__18TStoreObjectWriter,
                  ; right after `bl __ct__9TRefStackFv` at 0x2dd800
+0x04 0x001a48f0  ; first body instruction of __ct__9TRefStackFv
+0x08 0x0c645f0c  ; RAM+0x14 0x0cc825e0  ; stack pointer
+0x18 0x0cc82510  ; stack pointer
+0x1c 0x0cc82038  ; stack pointer (read as heap[+28]="size limit")+0x20 0x002dfa20  ; instruction `mov r0, #0` at MakeStoreObject; read
                  ; as heap[+32]="freelist start sentinel"+0x24 0x002dd7c4  ; instruction inside __ct__18TStoreObjectWriter
+0x28 0x0c6437b4  ; RAM
+0x2c 0x0cd51800  ; the new TRefStack stack base from the latest
                  ; NewStack POST-SWI in this trace+0x30 0x0cd51800  ; same+0x38 0x0000012c  ; 300 = TRefStack::ctor's `mov r0, #0x12c`
+0x48 0x002dfa20  ; heap[+72]="saved freelist position" — same ROM PC
                  ; as +0x20
```

freelist walk reads node[0]@0x002dfa20 = `{size=0xe3a00000 (= mov r0,#0
insn), next=0xe52d006c (= str r0,[sp,#-108]! insn)}` — i.e. the walker
is interpreting MakeStoreObject's body as a freelist node.

So `GetCurrentHeap` (which reads `*(*0x0c10105c - 16)` = current task's
`globals[-16]`) returns a pointer to something that is **not a heap** —
likely a TStoreObjectWriter / TRefStack / CatchHeader struct that has
saved return addresses where heap header fields belong. Walking it as
a heap reads ROM bytes that look like instruction encodings (0xe52d006c
= `str r0, [sp, #-108]!` — the function-prologue stack push), and the
next-pointer dereference faults with FAR=0xe52d006c.

Einstein doesn't hit this fault (probe log: 38 unique data-abort
tuples, all FAR ≤ 0x0CDDDC00, none with FAR=0xe5xxxxxx). So between
DAH-OR[7] (FAR=0x0cc81ff8 — the last common stack-grow we both take)
and the divergence, our hypervisor either:
- writes the wrong value into newt's `globals[-16]` (our `SetCurrentHeap`
  source), or
- reads `globals[-16]` from the wrong memory backing (stage-2 bug, or
  task-globals lookup pointing at the wrong page), or
- skips a normal heap-init step that Einstein performs (page-grow
  DABTs at PC=0x002DDF2C / FAR=0x0CDDDC00 that Einstein takes 22 times
  but we don't take at all — those are exactly inside the 0x002dxxxx
  `MakeStoreObject` / `TStoreObjectWriter` neighbourhood whose code
  bytes leak into our walker).

Diagnostic scaffolding stays loaded for now (kmain installs the bp,
handle_user_bp_und emulates the LDR + halts on wild r0). The next
concrete step is to identify why we miss the 22 `0x002DDF2C / FAR=
0x0CDDDC00` page-grow faults Einstein takes — those faults come from
`str r1, [r2], #4` inside what looks like TStoreObjectWriter::Prescan
(ROM 0x002dde20 region), populating a stack-allocated buffer. If our
stage-1 page table already has those pages mapped (so no DABT fires),
the writes land but read-back later returns ROM bytes from a stale
mapping — exactly the symptom we see. Inspect newt's L1/L2 entries
covering 0x0c... in the stack-grow window before the divergence.

### Earlier: newt self-deadlocks on its own heap semaphore (2026-04-27, am)

`newt` is queued on `q=0x00000000/0x0c116ed8`. That queue address is
**TSemaphore + 0x20** (the BlockOnInc queue) of a TSemaphore at
`0x0c116eb8`. Layout citations:

- task[+0x6c] flags = `0x2100000` — bit 0x100000 ("on a TSemaphore wait
  queue", set by TSemaphore::BlockOnInc / TTaskQueue::Add at ROM
  0x1d4dc8) | bit 0x02000000 (paged stack).
- TSemaphore is 40 bytes (ROM 0x1d5114 `mov r0, #40`); BlockOnZero
  queue at +0x18, BlockOnInc queue at +0x20 (TSemaphore::TSemaphore
  ROM 0x1d512c / 0x1d5134).
- The candidate `sema+0x20 = 0x0c116eb8` has `[+0x10] = 0x1ae40` which
  matches the TSemaphore vtable initialised at ROM 0x1d513c.

The TSemaphore is sema[0] of a TSemaphoreGroup at `0x0c116e94`
(kernel id `0x13d7`, count=1). Its TUSemaphoreGroup user wrapper is
at `0x0c116e7c`. The wrapper's `+0x08` (refcon) holds
`0x0c116e8c` which is `uwrapper + 8` — the malloc'd 4-byte
lock-state word for a TULockingSemaphore (TULockingSemaphore::Init
at ROM 0x25a514: `str r0, [r4, #8]; ... bl SetRefCon`). That word
contains `0x3063` — which is **newt's own task id**.

Newt's saved PC = `0x3ae1fc` (the `svc 0xb` of `SemaphoreOpGlue`),
`SPSR=0x110` (SVC mode), `lr_usr=0x25a2e0` (= the instruction after
`bl SemOp` in `TULockingSemaphore::Acquire` at ROM 0x25a298). The user
stack just below sp_usr has saved LRs:

- `+0x20 = 0x143334` — return into `DisposPtr` after its `bl Acquire`
  at ROM 0x143330.
- `+0x60 = 0x354724` — return into `MakeStoreObject`'s exception
  handler at ROM 0x354718, the `b 0x3544f4` catch loop that calls
  `TStoreWrapper::Abort` and `NextHandler`.
- `+0x60 = 0x353af0` — return inside `TStoreWrapper::~TStoreWrapper`.

So the call chain at the wedge is:

1. Newt entered `MakeStoreObject` (ROM 0x354178) and called
   `LockStore` (which Acquires the heap-store TULockingSemaphore =
   our id 0x13d7). `Swap` returned 0 (lock free) → newt acquired it.
   `lock-word` now = `0x3063` (newt's id).
2. Newt did store work (`StorePermObject`, `TStoreObjectWriter` ctor,
   etc.).
3. Something **threw `exBusError`** (Throw at trace 4149074, r0 =
   `0x000afda0` which is the literal pool pointer to `exBusError`
   class at ROM 0x3712b8). The bus-error origin is unidentified —
   most likely an MMIO read or stage-2 fault we should turn into a
   silent-default rather than a guest-visible bus error.
4. `setjmp`/`longjmp` cleanup triggered the catch handler at ROM
   0x3544f4. **It calls `TStoreWrapper::Abort` (0x354b50) but NOT
   `UnlockStore` — Abort does not release the lock** (verified by
   reading 0x354b50: it only resets TNodeCache, calls Abort on
   TStore + the two TStoreHashTables, no UnlockStore).
5. The catch handler invokes `NextHandler` and chains to the
   destructor. `~TStoreWrapper` (ROM 0x353ae4) calls
   `DisposeRefHandle` which eventually reaches `DisposPtr` (ROM
   0x14320c). DisposPtr calls `Acquire` on the **heap semaphore at
   ROM 0x143330**.
6. That `Acquire`'s `Swap` finds `lock-word == 0x3063` (newt's own
   id, still set by step 1). Swap puts newt's id back and returns
   `0x3063 ≠ 0`, so Acquire calls `SemOp` → `BlockOnInc`. Newt is
   queued on its own held lock — self-deadlock.

The `newt`-on-`sema+0x20` linkage is therefore not a "kernel waiting
for an event" mystery; it's a **lock leak in the C++ exception
unwind path**: TStoreWrapper's catch arm doesn't unlock the store
before destroying the wrapper, and the destructor's heap free path
re-enters the same lock.

Einstein cross-check (NewtonProbe 60 s, `/tmp/probe-60s.log`): at
t=2 s Einstein already has `Tmux RUN`, `newt(3cf3) RDY`, `scrn RDY`
(prio 11), `newt(2f13) BLK`; at t=4–60 s `fser RUN` (prio 13),
plus tasks `cdsv`, `scpl`, `codc`, `scrn`, `newt(2f13)` cycling
RDY/BLK. Einstein never lands on this deadlock — most likely
because step 3 (the Bus Error) doesn't fire there. So the right
fix is to identify the Bus Error origin and make it not throw.

Investigation tools (`src/task_dump.rs`):
- `dump_semaphore_waits` — for each task with flag 0x100000 set, dump
  the queue head and probe both `sema+0x18`/`sema+0x20` candidates
  (whichever has `[+0x10]=0x1ae40` is the real TSemaphore).
- `find_semaphore_owner` — walks `gObjectTable` for KernelType=7
  (TSemaphoreGroup), matches by array-base + size.
- `dump_blocked_pcs` — prints saved PC / sp_usr / lr_usr from each
  blocked task's SWIBoot save area at task+0x10..+0x54, plus
  newt's user-stack window.

Next concrete step: re-run with `trace,quiet` (every-call trace) to
catch the exact memory access that triggers the Bus Error throw.
Compare against Einstein's run at the same offset; the divergence
will name the MMIO/DABT we need to silently default. After that,
the deadlock disappears even without changing the lock semantics.

### Feed an input (after `newt` wakes)

PLAN's stated goal is "drive forward until the boot quiesces in a
steady-state idle that **responds to** whatever tablet / serial /
network inputs we choose to feed it." Tablet is the lightest-touch
entry point — `peripherals/tablet.rs` already produces stylus-down
/ up events, and the kernel's `pckm` task is BLK on the tablet
port. Wiring a synthetic tap should exercise the dispatch path
once the scheduler is letting `newt` run.

## Resolved stop log (this session)

### `Swap(NULL, 1)` ⇒ stage-2 perm fault on ROM-aperture write (2026-04-27)

Symptom — cold boot halted with:

```
*** data abort ISV=0 at ELR=0x95c444 SPSR=0x20000110
    IPA=0 FAR=0 iss=0x4e
```

`iss=0x4e` ⇒ `WnR=1`, `DFSC=0xe` (stage-2 permission fault, level 2),
guest VA = 0.

The misleading initial read of the trace tail was that PC `0x95c444`
looked like an Einstein.rex offset (REx base `0x00800000`,
offset `0x15c444`). REx is only 0x46c50 bytes, so that offset is well
past the loaded image. The actual answer:

`0x95c444` lives inside the **tracer trampoline pool**
(`0x00900000..0x00E00000`, `src/tracer.rs`). The pool is a flat array
of 5-word slots; `0x95c444 - 0x900000 = 0x5c444 = 18 896 × 20 + 4`,
so the PC is at `slot[1]` (offset +4) of slot index 18 896. Slot
index 18 896 of `scripts/classify-out/code-symbols.txt` resolves to
function **`Swap`** at ROM `0x003ae204`, whose body is one instruction:

```
003ae204 <Swap>:
  3ae204:  e1000091   swp r0, r1, [r0]
```

`Swap` is the kernel's atomic-exchange primitive. It's reached via
`Acquire(TULockingSemaphore*, SemFlags)` (ROM `0x1bce754` →
`0x55b1c`'s `TCardSocket::VccOff` etc.). The trace tail before the
abort:

```
trace 4147559 0x00050d18 VccOff(int)              (usr) ...
trace 4147560 0x00050d28 VccOff(int, unsigned long) (usr) ...
*** data abort ISV=0 at ELR=0x95c444 ...
```

— bare-function `VccOff__Fi`/`VccOff__FiUl` (NOT
`TCardSocket::VccOff`). Inside `VccOff__FiUl` (ROM `0x50d28` —
disassembled in `scripts/disasm-out/rom.dis`) is a chain that
indexes `gPowerSemaphore[arg0]` (`g 0x0c105f54`) and passes it to
`Acquire`. On the failing path that table entry is NULL, so
`Acquire(NULL)` reaches `Swap` with `r0 = 0`. The SWP then tries to
write to VA = 0; stage-1 identity-maps to IPA = 0; stage-2 has the
ROM aperture mapped RO, so we take a stage-2 perm fault.

Einstein oracle — `Emulator/TMemory.cpp:1755-1766`:

```cpp
TMemory::WriteP(PAddr inAddress, KUInt32 inWord) {
    if (inAddress < TMemoryConsts::kRAMStart) {
        if (inAddress < TMemoryConsts::kHighROMEnd) {
            if (mLog) mLog->FLogLine(
                "Ignored write word access to ROM at P0x%.8X (%.8X)",
                ...);
            // FALL THROUGH — no fault, no write.
        }
        ...
    }
}
```

Writes to anywhere `< kHighROMEnd` (0x01000000) are silently dropped.
For SWP the read-side still runs (`TJITGeneric_SingleDataSwap_template.h`
calls `Read` then `Write`), so `r0` ends up with `ROM[0]` (the reset
vector word `0xea0061a0`) and the kernel's spin-loop sees a non-zero
value.

Fix — `src/trap.rs::try_absorb_rom_write` (called from the ISV=0 arm
of `handle_data_abort`):

- Bail unless the IPA is in the ROM aperture (`< 0x01000000`).
- Read the faulting instruction at ELR (via stage-1 if up, else PA-
  direct so the path works for the early-boot / guest-test case).
- For SWP/SWPB (`(insn & 0x0FB0_0FF0) == 0x0100_0090`): set Rd to
  `ROM[ipa]` (word or byte), drop the store, advance ELR.
- Anything else falls through to the loud halt — the absorber is
  intentionally narrow so the next novel write to ROM stays loud.

Verification:

- 90 s cold boot reaches steady-state idle (no halts; `idle` task
  RUN, `newt` task RDY, beacons cycle through ELR=`0x800a0c` /
  `0x3adb0c` / `0x3ad6f4`).
- All 36 guest tests pass (`baremetal/guest-tests/scripts/run-all.sh`).
- New regression test `guest-tests/tests/test_swp_rom_aperture.S`
  exercises word SWP, the kernel's exact `swp r0, r1, [r0]` alias
  pattern (encoded as `.word 0xe1000091` because gas rejects
  `Rn==Rd`), byte SWPB, and a non-zero ROM-aperture address.
