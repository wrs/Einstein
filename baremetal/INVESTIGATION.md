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

## Next iteration — probe `PrimRememberMapping`

Args: `(env=r0, va=r1, &TPhys=r2, perm=r3)`. PA: `*(r2+16) >> 12 << 12`.

1. Pick a fresh HVC tag (e.g. `PRIM_REMEMBER_PROBE_HVC_IMM = 0x54`).
2. Patch first word at `0x00163480` (`mov ip, sp` = `0xE1A0_C00D`)
   with `HVC #0x54`.
3. Add `handle_prim_remember_probe` in `trap.rs`. Dereference
   `&TPhys` to get PA. Run the same per-PA → first-VA tracker.
   Emulate `mov ip, sp` (= `ctx.x[12] = sp_for_mode(...)`).
4. Cold-boot. Compare alias log against verify-mmu enumeration.

If `PrimRememberMapping` doesn't catch them, try
`PrimRememberPhysMapping` (variant with pre-resolved PA), then
`PrimRememberPermMapping`. If none catch them, escalate to a stage-2
RO trap on the L2 backing pages.
