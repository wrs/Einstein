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

## Next iteration — Group-1 stage-2 RO trap (independent track)

The 3 Group-1 aliases (PA=0x04004000, 0x04005000, 0x04006000) are
a fundamentally different class of problem from Group-2 — they
come from direct kernel L2 writes during TTBR0 setup, bypassing
the entire Remember/Prim layer. Switching to this track:

1. Install a stage-2 RO mapping on PA=0x04004000..0x04007000
   (3 pages). Make those PAs read-write through stage-1 (the
   guest sees normal RW) but stage-2-RO so writes trap to EL2.
2. EL2 fault handler: decode the AArch32 store insn (existing
   `unaligned::handle_align_fault` infrastructure has the
   relevant decoder), log `(PC, target offset, value)`, then
   commit the write through the kernel-globals mirror so the
   guest proceeds.
3. Cold-boot. Capture every store to those 3 pages during boot.
   Sort the (PC, offset, value) triples to identify the kernel's
   TTBR0 self-map setup function.
4. Decide fix layer: (a) Einstein-port behaviour (do whatever
   Einstein does for these specific writes), (b) ROM patch that
   redirects the self-map writes to a non-shared backing, (c)
   hypervisor-synthesised second mapping (write the L2 entries
   to point at a duplicate hypervisor-allocated PA so the guest
   sees the same L1/L2 byte values without underlying alias).

Group-2 stays parked at 12 aliases until Group-1 is solved.
Once Group-1 is zero, revisit Group-2 with stage-2 PA splitting
(Option C) — the hypervisor-level approach that was previously
considered "overkill" but may now be the cleanest route given
the Group-2 root cause is deliberate kernel design.

### Step Group-1 stage-2 RO trap (still needed)

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
