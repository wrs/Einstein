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

## Next iteration — narrow Group-2 root cause + Group-1 stage-2 trap

### Group-2 (Prim catches these)

Two paths to investigate:

1. **`CopyPagesAfterStackCollided` (`0x001F7540`).** This function
   handles stack-stack collision recovery: copy old PA → new PA,
   `ForgetMapping(old_VA, ..., old_PA)`, `RememberMapping(?, new_VA?, mask, new_PA, perm)`.
   In principle the alias should clear after `ForgetMapping`, but
   the probe shows the same PA appearing at multiple VAs *across*
   collision events — suggesting the PA in step "new_PA" sometimes
   is a PA that's STILL mapped under another stack's VA.
   - Plan: add a probe at the `bl ForgetMapping` (`0x001f75f0`) to
     log `(old_VA, old_PA)` and verify whether ForgetMapping
     actually clears the L2 entry.
   - Cross-check `TUPageManager::Get` page-allocation: the prior
     page-get probe showed unique `PageId`s but Newton's PageId
     may map to N>1 physical pages (count=2 was observed); the
     same `PageId` returned to two consumers could mean two PAs
     each, but if only ONE is owned-by-callee and the OTHER ends
     up unclaimed, a later allocation could re-claim it.
2. **`GenericSWIHandler` (SWI #12 dispatch at `0x000D8E38`).**
   This is the user-mode-driven path. Likely callers: heap/stack
   allocation routines like `FMNewStack`, `LockHeapRange`,
   `UnlockHeapRange`. To narrow further, walk the SWIBoot save
   area at HVC time to recover the user-mode caller PC (above
   the SWI boundary).

### Group-1 (Prim does NOT catch these — direct L2 writes)

The 3 Group-1 aliases (PA=0x04004000, 0x04005000, 0x04006000)
correspond to the kernel's L1 PT page (PA 0x04004000 backs L1
sections via the TTBR0 self-map) and the first two L2 PT pages.
These are written by direct kernel store instructions during
TTBR0 setup, bypassing the entire Remember/Prim layer.

Plan: install a stage-2 RO trap on PA=0x04004000..0x04007000.
Each `S2 RO` fault decodes the AArch32 store insn, logs `(PC,
L2-entry-index, value)`, then performs the write through the
kernel-globals mirror so the kernel proceeds. Once we see the
exact (PC, entry, value) triples that produce the alias, we can
either (a) port Einstein's matching behaviour, (b) install a ROM
patch that splits the self-map onto two distinct PAs, or (c)
synthesize the second mapping at hypervisor level so the kernel
sees the same byte values it expected without aliasing in the
underlying L2 entries.
