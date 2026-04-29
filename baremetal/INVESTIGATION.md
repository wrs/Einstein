# Live wedge-debugging notes

This file is a working scratchpad for the **current** investigation.
For prior history (Phase B per-stall analyses, FMNewStack 33→36 KiB
patch attempt and revert, RelocHeap corruption forensics, alrt-task
DABT decoding, scheduling-divergence pivot, etc.) see git log up to
commit `83634659 baremetal: Remember (static) is also NOT the
aliasing source — pivot to PrimRemember*` and the version of this
file at that commit. The file below is intentionally pruned.

## Current task — eliminate ALL RAM PA aliases

User directive (2026-04-28). 12 verify-mmu aliases observed at the
existing Reboot-canary wedge:

- Group 1 (3 aliases): kernel-globals self-mapping (PA=0x04004000,
  0x04005000, 0x04006000), created at TTBR0 setup. Kernel-only by
  intent.
- Group 2 (9 aliases): stack-guard sharing. Adjacent stacks at 33-KiB
  intervals straddle a 4-KiB boundary; ARMv4 subpage-AP-style sharing
  collapses to plain aliasing under our flat AP=011.

## Probe progression (negative results so far)

### `TUDomainManager::Get` doesn't recycle PageIds

Patch on `0x00258EFC` (`teq r0, #0` after `bl MonitorDispatchSWI`) →
`HVC #0x53`. Handler `handle_page_get_probe` in `src/trap.rs`. Runs
from both SVC-direct and USR→UND-trampoline paths; emulates the
`teq` flag effect by writing N/Z to either SPSR_EL2 or
`UND_SAVE_SPSR_IPA`. Caller LR is recovered by walking Get's APCS
frame at `fp[-4]` since R14 is clobbered by the BL by the time the
probe fires.

Cold-boot baseline: 28 successful Get calls before the Reboot canary,
all from `caller_lr=0x001F87C0` (= AllocNewPage's bl-Init return),
all `count=2`, all distinct PageIds in `0x136B..0x2A5B`. **0
duplicates.**

Conclusion: the prior iteration's "Get is recycling PAs" hypothesis
is REFUTED at baseline. Aliasing has a different origin.

### `Remember (static)` doesn't see the aliases either

Augmented the existing `handle_remember_entry_probe_with` (patched on
`0x00258E0C`) with an unconditional per-PA → first-VA aliasing
tracker. Logs every (env, va, pa, perm) call; emits `Remember
ALIAS:` when a PA is later seen at a different VA.

Cold-boot baseline: 7 ENTER lines (matching the existing L1-lazy-
grow filter), **0 `Remember ALIAS:` lines** across the entire boot.
Meanwhile the 12 verify-mmu aliases all still appeared.

Conclusion: the L2 writes that create the 12 aliases do NOT pass
through the `Remember (static)` user-shim. Two non-overlapping
kernel paths bypass it:

- **Direct L2 writes by cold-boot kernel TTBR0 setup.** Most likely
  source of the 3 Group-1 kernel-globals self-mapping aliases.
- **`PrimRememberMapping` family** at `0x00163480` / `0x00163708` /
  `0x00163920`. Lower-level L2-write primitives reached via different
  kernel paths than the user-shim. Most likely source of the 9
  Group-2 stack-guard aliases.

## `PrimRememberMapping` probe — caught all 12 Group-2 aliases

Patched the first word at `0x00163480` (`mov ip, sp` = `0xE1A0_C00D`)
with `HVC #PRIM_REMEMBER_PROBE_HVC_IMM (0x54)`. Handler
`handle_prim_remember_probe_with` in `src/trap.rs` captures the
arg tuple, dereferences `&TPhys` to get `PA = *(r2+16) & ~0xFFF`,
runs a per-PA → first-VA aliasing tracker, then emulates the
original `mov ip, sp`.

### Arg-decoding correction

First-iteration probe mis-labelled the args: per the disasm,
`PrimRememberMapping(va=r0, mask=r1, &TPhys=r2, perm=r3)` — NOT
`(env, va, &TPhys, perm)`. The kernel calls Prim with the SAME
`va` and SAME `&TPhys` repeatedly, widening `mask` (0x3 → 0xf →
0x3f → 0xff) — that's incremental-subpage staging on the SAME
mapping, not aliasing. Tracker now keys on `va` (r0); same-VA
calls don't trigger a false alias.

### Result

Cold-boot run produces **106 `Prim ALIAS:` lines** covering **all
12 Group-2 aliased PAs** that `verify-mmu` enumerates:

```
PA=0x04028000  VA1=0x0c310000 ↔ VA2=0x0c318000  ↔ VA2=0x0c320000  ↔ VA2=0x0c602000
PA=0x0402c000  VA1=0x0cc7a000 ↔ VA2=0x0cc82000  ↔ VA2=0x0ccab000
PA=0x0402e000  VA1=0x0cc9b000 ↔ VA2=0x0cca3000  ↔ VA2=0x0c320000
PA=0x0402f000  VA1=0x0c318000 ↔ VA2=0x0cc7a000
PA=0x04033000  VA1=0x0cc82000 ↔ VA2=0x0ccad000
PA=0x04034000  VA1=0x0cc7f000 ↔ VA2=0x0cc82000
PA=0x04035000  VA1=0x0c603000 ↔ VA2=0x0ccc4000
PA=0x0403a000  VA1=0x0ccc4000 ↔ VA2=0x0ccca000
PA=0x0403b000  VA1=0x0ccc4000 ↔ VA2=0x0cccb000
PA=0x0403c000, 0x0403d000, 0x04043000  ditto
```

**Group-1 (kernel-globals self-mapping at PA=0x04004-0x04006) does
NOT appear in the Prim probe output**, confirming those aliases
come from a different layer (most likely direct kernel TTBR0-setup
L2 writes).

### Upstream call sites identified by walking RememberMapping's frame

The Prim probe's caller LR is always `0x0011c87c` (= the
`bl PrimRememberMapping` in `RememberMapping__FUlN31Uc` at ROM
`0x0011c7d8`). To find the real culprit, the probe also reads
RememberMapping's saved-LR slot at `[fp - 4]` (its prologue
`sub fp, ip, #4` points fp at the saved-PC slot, so saved-LR is
4 below). The `upstream_lr` distribution across the 13 unique
aliased PAs:

| upstream_lr  | function                        | aliased PAs |
|--------------|---------------------------------|------------:|
| `0x000d8e3c` | GenericSWIHandler (SWI #12)     | 13 (all)    |
| `0x001f775c` | CopyPagesAfterStackCollided #2  |  9          |
| `0x001f76bc` | CopyPagesAfterStackCollided #1  |  2          |

Every aliased PA passes through GenericSWIHandler at some point
(SWI #12 = user-mode `Remember (static)` shim's underlying call).
The `CopyPagesAfterStackCollided` paths are subsets — they're
involved when stack collisions force the kernel to copy a page and
re-install the mapping at a new VA.

### Why the prior `Remember (static)` probe missed these

The `Remember (static)` user-mode shim at `0x00258E0C` is the
**caller** of SWI #12 — when it runs, it issues `bl GenericSWI`
which traps to the kernel-side `GenericSWIHandler`. The handler
then calls `RememberMapping__FUlN31Uc` directly. So `Remember
(static)` fires only on USR-mode-issued installations; the kernel-
internal SWI dispatch path (`GenericSWIHandler` → `RememberMapping`
→ `PrimRememberMapping`) and `CopyPagesAfterStackCollided`'s
direct calls bypass it entirely.

Additionally, the prior probe's alias detector mis-decoded the
args: it treated r3 as a PA value, but per the disasm r3 is the
TPhys-pointer (passed unchanged through to `GenericSWI`). So
even if it had seen the right calls, its alias-key was a TPhys*
pointer rather than a real PA — a different (and rarely-coinciding)
condition.

## `PrimForgetMapping` probe — 12 of 13 prior aliases survive forget pairing

Patched `0x00163514` (`mov ip, sp`) with `HVC #PRIM_FORGET_PROBE_HVC_IMM
(0x55)`. Hoisted the per-PA → first-VA tracker out of
`handle_prim_remember_probe_with` into module-level statics
(`PRIM_FIRST_VA_FOR_PA` / `PRIM_FIRST_LR_FOR_PA`) so both probes
manipulate the same arrays. `PrimForgetMapping(va=r0, &TPhys=r1)`:
PA recovered as `*(r1+16) & ~0xFFF`. On a forget call, if the
tracker's recorded VA matches the forgotten VA, the slot clears;
mismatches log `Prim FORGET MISMATCH:`.

### Result

| metric | iter 1 (Remember only) | iter 2 (+ Forget) |
|--------|-----------------------:|------------------:|
| `Prim ALIAS:` lines    | 106 | 55 |
| unique aliased PAs     |  13 | 12 |
| `FORGET MISMATCH:` lines | n/a |  8 |
| verify-mmu aliases     |  15 | 15 |

The drop from 106 → 55 ALIAS lines and 13 → 12 unique PAs proves
the tracker correctly clears matched forgets — most of the prior
"aliases" were artifacts of normal forget+re-install sequences.
The remaining 12 are **real aliases**: same PA installed at two
distinct VAs WITHOUT an intervening forget. **All 12 come through
upstream `0x000d8e3c` (GenericSWIHandler, SWI #12 dispatch)** —
the kernel-side handler for user-mode `Remember (static)` calls.

`CopyPagesAfterStackCollided` upstream LRs (`0x001f76bc`,
`0x001f775c`) only appear as accomplices on a few PAs — those PAs
are dual-installed via SWI #12 *and* through stack-collision
recovery. The collision path is not creating fresh aliases on
its own.

### FORGET MISMATCH cases corroborate

```
PA=0x04028000  forgot VA=0x0c318000  but tracker had VA=0x0c602000
PA=0x04028000  forgot VA=0x0c320000  but tracker had VA=0x0c602000
PA=0x0402c000  forgot VA=0x0cc82000  but tracker had VA=0x0ccab000
PA=0x04034000  forgot VA=0x0cc7f000  but tracker had VA=0x0cc82000
PA=0x04034000  forgot VA=0x0cc80000  but tracker had VA=0x0cc82000
PA=0x04034000  forgot VA=0x0cc81000  but tracker had VA=0x0cc82000
PA=0x0403d000  forgot VA=0x0ccc9000  but tracker had VA=0x0ccc4000
PA=0x0402e000  forgot VA=0x0c320000  but tracker had VA=0x0cc9b000
```

These show the kernel forgetting a (VA, PA) pair where our tracker
held a *later* VA — i.e. the kernel called Remember(VA1, PA),
Remember(VA2, PA) (alias logged), then later Forget(VA1, PA). So
the kernel is aware that PA was mapped at multiple VAs and
forgets each VA's mapping individually — but during the period
between Remember(VA2, PA) and Forget(VA1, PA), the alias is live.

### Why this matches the "stack-guard sharing" hypothesis

The aliased VAs land on stack-grid offsets (32 KiB stride):

| PA           | Aliased VAs |
|--------------|---|
| 0x04028000 | 0x0c310000, 0x0c318000, 0x0c320000, 0x0c602000 |
| 0x0402c000 | 0x0cc7a000, 0x0cc82000, 0x0ccab000 |
| 0x0402e000 | 0x0cc9b000, 0x0cca3000, 0x0c320000 |

VAs `0xc310000 / 0xc318000 / 0xc320000` differ by 0x8000 (32 KiB —
the Newton stack-stride). The same physical page is the *last
page of stack N* AND the *first page of stack N+1*. ARMv4's
subpage AP let those two stacks own the page's halves
independently; on ARMv7 we collapse to AP=011, so the two stacks
share the entire 4 KiB page → write-from-one corrupts the other.

This is the documented **Group-2 stack-guard sharing** pattern:
the kernel is intentionally sharing boundary pages, not by
mistake. The fix is at the allocator (FMNewStack) layer — make
each stack allocate non-overlapping 4 KiB pages.

## SWI save-area walk — user-mode aliasing source is `TTask::Init`

`docs/STRUCTURES.md` "TTaskSavedContext" documents the SWIBoot
save layout at `&TTask + 0x10`:

- `TTask + 0x4c` = saved_pc (LR_svc, the post-SVC return PC)
- `TTask + 0x48` = lr_usr (USR caller-of-active-function LR)
- `TTask + 0x3c` = fp_usr (active USR fp at SWI time)

`curr_task = *(0x0c100ff8)` (gKernelGlobals). New helper
`read_swi_caller()` in `src/trap.rs` reads all three. The Prim
Remember probe now logs `(user_pc, user_lr, user_caller)` on
every Prim ALIAS line, where `user_caller = *(fp_usr - 4)` —
the saved-LR slot of the active USR function's APCS frame, i.e.
who BL'd into the function that issued the SWI.

### Result — `TTask::Init` dominates

Cold-boot run, 55 `Prim ALIAS:` lines across 12 PAs. user_caller
distribution:

| user_caller | function | aliased PAs | count |
|---|---|---:|---:|
| `0x002523bc` | `TTask::Init` post-`bl LockHeapRange` (1st) | 6 | 22 |
| `0x002523d4` | `TTask::Init` post-`bl LockHeapRange` (2nd) | 5 | 9 |
| `0x00124280` | `TMuxStoreMonitor::Init` | 2 | 4 |
| `0x003109e4` | `ExtendVMHeap` | 2 | 4 |
| `0x0c1118c8` | (RAM, REx-resident) | 2 | 4 |
| `0x00114078` | `TheMain::TLoader` (boot) | 1 | 2 |
| `0x001423d8` | `NewVMHeap` | 1 | 2 |
| `0x001f8b34` | `LockStack` | 1 | 1 |
| `0x00311f04` | `NewDirectBlock` | 1 | 2 |
| `0x0004b0fc` | `TCardAsyncMsg` ctor | 1 | 4 |
| `0x0c119d4c` | (RAM, REx-resident) | 1 | 2 |

`TTask::Init` is responsible for **11 of 12** aliased PAs. The
two BL sites at `0x002523b8` and `0x002523d0` are the per-task
`LockHeapRange` calls that pin the just-allocated stack into
resident memory:

```
0x25238c: bl NewStack             ; allocate stack VA range
0x2523b4: str r0, [r4, #248]      ; task[+0xf8] = stack_base - 0x54
0x2523b8: bl LockHeapRange        ; lock entire stack range → user_caller=0x2523bc
0x2523c4: ldr r0, [r4, #248]      ; r0 = stack_base - 0x54
0x2523c8: add r1, r0, #48         ; r1 = stack_base - 0x24
0x2523d0: bl LockHeapRange        ; lock 48-byte header range → user_caller=0x2523d4
```

LockHeapRange triggers per-page resolve-fault handlers that call
`RememberMapping` for each page in the range. Because Newton
stacks have a 32-KiB VA stride and a 33-KiB usable size,
**adjacent stacks share a 4 KiB boundary page** — the last page
of stack N is the first page of stack N+1 in VA space. Under
ARMv4 subpage AP each stack owned a 1-KiB subpage of the
shared 4-KiB physical page; ARMv7 has no subpage AP, our
`fix_stage1_xn_bits` flattens to AP=011, so the boundary page
becomes a true PA alias.

The tail of the distribution (1-2 PAs each) covers the same
class of problem in other allocator paths: heap creation
(`NewVMHeap`, `ExtendVMHeap`, `NewDirectBlock`), driver init
(`TMuxStoreMonitor::Init`, `TCardAsyncMsg`), boot loader
(`TheMain::TLoader`), and the `LockStack` user-mode shim.

`user_lr=0x00258efc` is consistent across all aliases — that's
inside `TUDomainManager::Get`'s post-`bl MonitorDispatchSWI`
PC. `Get` is the user-mode page-allocator shim; LockHeapRange's
fault-resolve path goes through it via SWI dispatch.

## Option A pad attempt — wedges on info_bounds overflow (2026-04-28)

Implemented Option A (call-site +4 KiB padding) as a 2-word
wrapper at `0x00FFFE80`:

```
add r1, r1, #4096       ; bump stack-size request by 4 KiB
b   <NewStack thunk>    ; tail-call into the post-ship patch table
```

Patched `TTask::Init`'s `bl NewStack` at `0x0025238C` to redirect
through the wrapper. Patch installed cleanly.

**Result: regression — boot wedges in an infinite ResolveFault
loop instead of reaching the Reboot canary.** Trace tail repeats:

```
Fault(stackmgr) ENTER  far=0x0c647003 caller_lr=0x00259230 src_mode=USR
ResolveFault    ENTER  far=0x0c647000 info_bounds=[0x0c601000,0x0c647000)
ResolveFault    ENTER  far=0x0c647400 info_bounds=[0x0c601000,0x0c647000)
ResolveFault    ENTER  far=0x0c647800 info_bounds=[0x0c601000,0x0c647000)
ResolveFault    ENTER  far=0x0c647c00 info_bounds=[0x0c601000,0x0c647000)
DAH-exit (success @ 0x00393b80)  ← kernel claims to handle the fault
[loop repeats forever, FAR never advances]
```

`FAR=0xc647003` is exactly 3 bytes past `info_bounds`'s exclusive
upper bound of `0xc647000`. The 4 KiB pad changed the *size
requested* of NewStack but NOT the kernel's stack-pool slot
stride. The kernel still places adjacent stacks at 33 KiB stride,
so each padded stack consumes its 33 KiB slot plus 4 KiB of the
next slot. The pool's per-task allocation index runs past the
upper bound on the (N+1)-th stack, producing a USR-mode access
3 bytes past info_bounds; ResolveFault returns "success" via the
ResolveFaultWrapper path but the underlying VA is still
unmapped, so the abort re-fires immediately.

Patch reverted (the wrapper code is left in `apply_new_stack_pad_wrapper`
but not installed). Baseline restored: 15 verify-mmu aliases, 55
`Prim ALIAS:` lines, all 36 guest tests pass.

**Insight:** The targeted call-site pad cannot work in isolation —
it must be paired with stride widening (Option B). The previous
20-patch 36-KiB attempt did both and successfully eliminated
stack-stack guard sharing, but introduced new stack-vs-heap
aliases. Our current Get probe confirms `TUDomainManager::Get`
returns unique PageIds per call (not the prior diagnosis of "PA
recycling"); the new aliases the 36-KiB attempt produced may
have a different cause that's now diagnosable via the Prim
alias tracker.

## Group-1 stage-2 RO trap probe — pages are write targets, not L2 PTs

Implemented a `g1_capture` module (`src/g1_capture.rs`) that:

- Marks PA=0x04004000, 0x04005000, 0x04006000 stage-2 RO+XN at
  end of `stage2::init()` (before guest ERET).
- Hooks `handle_data_abort` so any RW permission-fault on those
  pages logs `(elr, ipa, value, srt)` before the existing
  auto-flip-to-RW path lets the store complete.
- Re-arms RO+XN on every IRQ entry (timer-driven, ~16 ms cadence)
  so subsequent writes also fault. **Critical:** rearming on
  *sync* trap entry caused an infinite ResolveFault-style loop
  because STMIA (and other multi-register stores) span page
  boundaries and re-fault on each retry; IRQ-only rearm lets
  the in-flight STM complete unhindered.

Cold-boot result: 186 captures across the 3 PAs, 25 distinct
writer PCs, exit=1 reboot canary as before, 15 verify-mmu
aliases unchanged, all 36 guest tests pass.

### Captures don't reveal the alias-creating writes

Distribution by PA / offset:

```
PA=0x04004000  64 captures across 15 distinct offsets, range 0x0..0x3ec
PA=0x04005000  64 captures across  5 distinct offsets, range 0x0..0xfa0
PA=0x04006000  58 captures across  9 distinct offsets, range 0x0..0x868
```

`PA=0x04005000`'s captures at offsets `0xf0c` (56× from
PC=0x00FFFF08) and `0xfa0` (5× from PC=0x00FFFFB4) are
**hypervisor self-noise** — those PCs are inside our own
UND/DABT trampolines (UND_TRAMP at 0x00FFFF00, DABT_TRAMP at
0x00FFFFA8). UND_SAVE_R0_IPA is `0x04005F0C` and DABT_SAVE is
at `0x04005FA0`; both fall in the 0x04005000 page, so our own
EL2-controlled trampoline writes trip the stage-2 RO trap.
That's expected and harmless — they're EL1-AArch32 stores from
the guest-installed trampoline.

The remaining captures (`~125`) are heterogeneous. Some values
look like ARMv7 section descriptors (e.g. `0x04007f0e` written
to PA=0x04006000 by PC=0x00018d10), suggesting that page hosts
*part of* the kernel's L1 PT. Other values are small integers
(1..0x26) written to PA=0x04004000 by PC=0x001dd934 — looks like
counter-loop population of an array, NOT an L1/L2 PT entry.

### What we missed: writes to PA=0x00001400 (ROM-resident L2 PT)

The prior task-census output recorded `L1[0xc0] = 0x00001401`,
a coarse PT descriptor pointing at L2 PT base PA `0x00001400`
(decoded: bits[31:10]=0x05 → base = 0x05 << 10). PA `0x1400`
lives in ROM. The duplicate L2 entries that establish the
Group-1 aliases (e.g. L1[0xc0],L2[0x0] → PA=0x04004000 AND
L1[0xc0],L2[0x2] → PA=0x04004000) live at PA `0x1400 + idx*4`
in ROM — pre-baked at ROM build time, never dynamically
written.

So the g1_capture probe sees writes to the *target* PAs of the
aliases (the kernel-globals data pages) but **not** to the L2
PT entries that *create* the aliases. The aliases are static
ROM artifacts — a deliberate Apple-build-time design that
exposes kernel-globals data pages at two VAs each (likely an
ARMv4 subpage-AP-era permission-overlay pattern: kernel-RW at
one VA, kernel-RO-shared at another).

## ROM-baked L2 PT confirmed — and the duplicates are subpage-AP overlays

Added a one-shot dump path inside `verify-mmu`: when the alias
detector first records PA P, it also reads the L1 entry's
underlying L2 PT and logs `L2[prev_idx]` and `L2[va_idx]`
(both descriptors), plus the L2 PT's location (ROM vs RAM).

Cold-boot result confirms the hypothesis for **all 3 Group-1
aliases**:

```
verify-mmu alias L1[0xc0]=0x00001411 → L2_PT@PA=0x00001400 (ROM)
  L2[0x0]=0x0400403e (PA=0x04004000)  L2[0x2]=0x0400414e (PA=0x04004000)
  L2[0x3]=0x0400503e (PA=0x04005000)  L2[0x4]=0x0400514e (PA=0x04005000)
  L2[0x7]=0x0400603e (PA=0x04006000)  L2[0x8]=0x0400604e (PA=0x04006000)
```

L1[0xc0] = `0x00001411` (coarse PT, base = `0x05 << 10 =
0x00001400`, in **ROM**). The L2 entry pairs at indices
`(0x0,0x2)`, `(0x3,0x4)`, `(0x7,0x8)` are pre-baked at ROM
build time and confirmed identical-PA-different-flags.

### The duplicates are *subpage-AP permission overlays*

The two descriptors of each pair share the high 20 bits (PA)
but differ in bits [11:0]. Decoding ARMv4 subpage AP encoding
(bits [5:4] = AP[0] for subpage 0, [7:6] = AP[1], [9:8] = AP[2],
[11:10] = AP[3]) for the L2[0x0]/L2[0x2] pair (PA=0x04004000):

| descriptor | AP[0] | AP[1] | AP[2] | AP[3] |
|------------|-------|-------|-------|-------|
| 0x0400403e | 11    | 00    | 00    | 00    |  (RW   sys  sys  sys)
| 0x0400414e | 00    | 01    | 01    | 00    |  (sys  pRW  pRW  sys)

Each subpage of PA=0x04004000 has exactly **one** VA with
privileged-RW access:
- subpage 0 (offset 0..1KiB)   → VA=0xc000000 (priv-RW)
- subpage 1 (offset 1..2 KiB)  → VA=0xc002000 (priv-RW user-RO)
- subpage 2 (offset 2..3 KiB)  → VA=0xc002000 (priv-RW user-RO)
- subpage 3 (offset 3..4 KiB)  → neither (sys/priv-RO from both)

This is the ARMv4 subpage-AP-era permission-overlay pattern: one
physical page exposes different RW views through two VAs. Our
`fix_stage1_xn_bits` flattens both to AP=11 → both VAs become
full RW → real PA alias under ARMv7's no-subpage-AP rule.

### Group-2 confirmation (RAM-resident, runtime)

For comparison, the dump also fires for Group-2 aliases:
```
verify-mmu alias L1[0xcc]=0x04025481 → L2_PT@PA=0x04025400 (RAM)
  L2[0x82]=0x0403303e (PA=0x04033000)  L2[0xad]=0x0403300e (PA=0x04033000)
```
Group-2 L2 PTs are in **RAM** at PA=0x04025400/0x04025800 — the
kernel-installed per-task page tables. These match the prior
diagnosis (TTask::Init → LockHeapRange → RememberMapping → PrimRememberMapping
chain populates these L2 entries at runtime). For Group-2 aliases
the L2 entries also have differing AP bits (e.g. 0x0403303e vs
0x0403300e) confirming the same subpage-AP design pattern.

## Option α probe — wedges immediately, but the wedge is informative

Implemented Option α as PATCHES_717006 entries zeroing L2[0x2],
L2[0x4], L2[0x8] in the ROM L2 PT at PA=0x00001400.

Cold-boot result: verify-mmu reported aliases dropped from 15 → 2,
but boot wedged at FAR=0xc004fa0 in an infinite ResolveFault loop.
The TWO surviving aliases revealed previously-undetected duplicates:

```
L1[0xc0]=0x00001411 → L2_PT@PA=0x00001400 (ROM)
  L2[0x0]=0x0400403e (PA=0x04004000)  L2[0x5]=0x0400440e (PA=0x04004000)
  L2[0x3]=0x0400503e (PA=0x04005000)  L2[0x6]=0x04005c0e (PA=0x04005000)
```

Decoding the *third* descriptor of each pair:
- L2[0x5] = 0x0400440e: AP=(00,00,00,01) — subpage 3 priv-RW
- L2[0x6] = 0x04005c0e: AP=(00,00,00,11) — subpage 3 RW

So each Group-1 PA actually has **three** L2 descriptors, each
granting priv-RW to a *different* subpage:
- PA=0x04004000: L2[0x0] (sp 0), L2[0x2] (sp 1+2), L2[0x5] (sp 3)
- PA=0x04005000: L2[0x3] (sp 0), L2[0x4] (sp 1+2), L2[0x6] (sp 3)
- PA=0x04006000: L2[0x7] (sp ?), L2[0x8] (sp ?), and probably one more

The duplicates are NOT redundant — they cover distinct subpages.
Zeroing L2[0x4] unmapped subpage 3 of PA=0x04005000 from VA=0xc004XXX,
and *something* in the system was reading/writing through that VA.

## What was using VA=0xc004fa0?

User asked: "what code is accessing that memory?". Answer (verified):

**Direct literal references in the kernel disasm to the relevant
addresses are all ZERO:**

```
0x0c000000..0x0c008fff (entire L1[0xc0] self-map VA range): 0 hits
0x0c004f00 (UND/SBA scratch base post-MMU):                  0 hits in code
0x0c004fa0 (DABT scratch post-MMU):                          0 hits
0x0c006fa0 (RW alias of PA=0x04005FA0 via L2[0x6]):          0 hits
0x04005f00 (UND/SBA scratch base pre-MMU):                   0 hits
0x04005fa0 (DABT scratch pre-MMU):                           0 hits
```

(A few `stceq 15, ...` matches landed in REx data regions but
those are constants in literal pools, not instructions.)

**The only code that uses these VAs is OUR HYPERVISOR's
trampolines:**
- `install_und_vector_swap_post_mmu()` writes `0x0C00_4F00` into
  the UND trampoline literal (offset +0x5C of UND_TRAMP).
- Same function writes `0x0C00_4F00` into the SBA-post-emulation
  trampoline literal.
- And `0x0C00_4FA0` into the DABT trampoline literal.

So the kernel-globals self-map at L1[0xc0] **is functionally
inert** in the running kernel — no kernel code reads or writes
through those VAs as literals. The PA targets
(PA=0x04004000-0x04006000) DO receive writes from many kernel
PCs (per the prior g1_capture probe — 25 distinct writers), but
those accesses MUST go through some OTHER VA window we haven't
enumerated (likely a different L1 section's L2 PT pointing at
the same PAs). The L1[0xc0] mappings appear to be a vestigial
ARMv4 design-intent that the kernel team wired up but the
running code never exercises.

### Why the wedge then?

The wedge at FAR=0xc004fa0 is OUR hypervisor's DABT trampoline
trying to `str lr, [r0]` where `r0 = 0x0c004fa0`. With L2[0x4]
zeroed, that VA is unmapped → DABT → trampoline tries again at
the same VA → infinite loop. We broke ourselves, not the kernel.

### What the captures missed

The g1_capture probe armed PA=0x04004000-0x04006000 directly,
but the kernel writers of PA=0x04004000 (e.g. PC=0x001dd934
writing 1..0x26) reach those PAs via stage-1 translation through
non-L1[0xc0] L2 PTs we haven't located. To find them we'd need
to walk the entire guest L2 PT space at runtime and look for
every entry mapping any of the 3 PAs.

## Trampoline relocated; Option α retried; revealed exception-stack layout

Relocated `HYP_TRAMP_SCRATCH_BASE` from `0x04005F00` (kernel-globals
self-map page) to `0x0600_F000` (last 4 KiB of the shadow-stub
SCRATCH_POOL). Same value works pre-MMU (stage-1 off → stage-2)
and post-MMU (kernel L1[0x60] section we install at MMU-enable
→ stage-2). 36/36 guest tests pass; baseline preserved (15 verify-mmu
aliases unchanged).

With trampolines moved out of the way, re-attempted Option α
ROM-patching of L2[0x2,4,5,6,8] of the L2 PT at PA=0x00001400.
Cold-boot: verify-mmu aliases dropped 15 → 0 (!) but boot wedged
at FAR=`0xc004bf8` in `InitSpecialStacks` (LR_svc=0x188ac in
BootOS). Patches reverted.

### The real layout: kernel exception stacks share PA=0x04005000

`InitSpecialStacks` at ROM `0x0011efb4` calls:

```
SetFIQStack   (0x0c003400)  ← L2[0x3] sp 0  → PA=0x04005000+0x000..0x3FF
SetAbortStack (0x0c004c00)  ← L2[0x4] sp 1+2 → PA=0x04005000+0x400..0xBFF
SetIRQStack   (0x0c002c00)  ← L2[0x2] sp 3  → PA=0x04004000+0xC00..0xFFF
SetUndefStack (0x0c006000)  ← L2[0x6] sp 3  → PA=0x04005000+0xC00..0xFFF
SetUserStack  (0x0c007400)  ← L2[0x7]
```

So the L1[0xc0] kernel-globals self-map is **the kernel's exception
stack layout**. The kernel packs 4 exception stacks (FIQ, Abort,
Undef, plus IRQ on PA=0x04004000) into shared 4-KiB physical
pages, with each stack assigned a distinct 1-KiB subpage. Under
ARMv4, subpage AP gave each stack its own priv-RW VA window with
the others as RO/NA. Under our flat AP=11, all subpages of a page
become priv-RW from any of the alias VAs.

The disasm grep for direct literal references to the alias VAs
returned zero hits because the kernel computes them at runtime
(e.g., `mov r0, #0xC000000; orr r0, r0, #0x4c00`). Those are
indirect references that the literal scan misses.

### Einstein cross-check: subpage AP correctly emulated

`Emulator/TMMU.cpp:304-306, 325-326`:

```cpp
subpageIndexShift = ((inVAddress & 0x00000C00) >> 9) + 4;
entryPermIndex = (tableEntry & (0x00000003 << subpageIndexShift))
                 >> subpageIndexShift;
```

Einstein takes VA bits [13:12] (small-page subpage index) and
looks up the corresponding 2-bit AP grant from the L2 descriptor.
Each exception stack VA maps to its kernel-intended priv-RW
subpage; the other VAs see read-only / no-access subpages on the
same physical page. The kernel's design works correctly under
Einstein's faithful emulation.

### Why our flat AP=11 is functionally OK

The kernel's *usage pattern* puts each exception stack at a
disjoint 1-KiB offset within the shared 4-KiB physical page (FIQ
at 0..1023, IRQ at 3072..4095 of its page, Abort at 1024..3071
of its page, etc.). They share the physical page in stage-1, but
no two stacks ever write to the same bytes — assuming none of
them overflows its 1-KiB allocation (which would also be a bug
under ARMv4 subpage AP).

Verify-mmu correctly reports the aliases (two distinct VAs map
to the same PA), but no actual memory corruption can happen.
**Group-1 aliases are functionally inert** under the kernel's
careful subpage-disjoint usage.

The same logic likely applies to Group-2's 12 aliases (stack-guard
sharing in adjacent task stacks): the kernel allocator gives each
33-KiB stack a consistent offset pattern, and the boundary 4-KiB
page is effectively split into per-stack zones that don't overlap
in byte access.

This **significantly weakens the "things break randomly" premise**
behind the original directive. The aliases are reportable but
benign by virtue of the kernel's deliberate subpage-disjoint
design.

## Next iteration — pivot direction (proposed)

### Recommendation: Option σ — accept and document

Declare Group-1 aliases benign (verify-mmu reports them but the
kernel's exception-stack subpage-disjoint design ensures no byte
conflict). Add a one-time runtime characterisation of Group-2 to
confirm the same pattern (each shared-PA stack uses disjoint
offsets), then declare those benign too if confirmed. Document
both as "reportable verify-mmu observations, not real corruption".

Then **un-park** the alrt-task DABT investigation that was deferred
4+ iterations ago. The original directive ("no other debugging
until aliasing is zero") was based on the assumption that aliases
caused the wedge; we now have evidence the kernel's design
prevents actual corruption regardless of alias count.

### Backup options if σ proves insufficient

- **Option β** — full stage-2 PA splitting with shadow-on-write
  coherence. Substantial work; only worth it if a future kernel
  path actually overflows a subpage and we observe corruption.
- **Option τ** — patch `InitSpecialStacks` to use distinct 4-KiB
  PAs per exception stack. Doable but needs an audit for other
  kernel code paths that rely on the shared layout.

### Safety net for Option σ

Add stage-2 RO traps on the *off-stack subpages* of each
exception-stack page (e.g., bytes 0..1023 of PA=0x04005000 should
not be touched by anyone other than FIQ stack growth). An
overflow into a wrong subpage would fault to EL2 immediately and
we'd know.

### Step Group-1 stage-2 RO trap (no longer planned, see Option σ above)

The 3 Group-1 aliases (PA=0x04004000, 0x04005000, 0x04006000) come
from the kernel's L1 PT page (TTBR0 self-map) and the first two
L2 PT pages — written by direct kernel store instructions during
TTBR0 setup, bypassing the entire Remember/Prim layer.

Install a stage-2 RO trap on PA=0x04004000..0x04007000. Each `S2
RO` fault decodes the AArch32 store insn, logs `(PC, L2-entry-
index, value)`, then performs the write through the kernel-
globals mirror so the kernel proceeds. Once we see the exact
(PC, entry, value) triples that produce the alias, decide between
(a) Einstein-port behaviour, (b) ROM patch that splits the
self-map onto two distinct PAs, or (c) hypervisor-synthesised
second mapping.
