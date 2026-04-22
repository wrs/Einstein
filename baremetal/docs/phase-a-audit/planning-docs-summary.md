# Planning-docs summary — pre-Phase-A-closeout

Output of the "what did our own planning documents claim Phase A
covered?" Explore subagent, 2026-04-21. Verbatim quotes pulled
from `PLAN.md`, `HIGHLEVEL.md`, `INVESTIGATION.md`, `README.md`,
`IMPLEMENTATION.md`, `CLAUDE.md`, `probe/FINDINGS.md`.

---

## Phase A Scope vs. Current Status: Summary

**PLAN.md explicitly states Phase A is done** (line 3-6):

> "**Phase A is done. Phase B is mid-flight.**" with "Every Phase A
> item (fine-table rewrite, UND handler, StrongARM-clock no-op,
> TSerialChip, CP10/11 native primitives, screen blit) landed with
> its own guest test. All 13 guest tests pass."

### Phase A Explicit Scope (PLAN.md §"Approach")

Line 69-70 declares: **"Phase A — build every known-required piece
as a real handler ✅ DONE"**

Seven items were required:

1. **Fine-table rewrite** — line 75: "Extend
   `guest_mem::fix_stage1_xn_bits` to rewrite type `0b11` → `0b00`
   (fault)" ✅
2. **UND handler** — line 77: "SWP/SWPB, SystemBootUND,
   DebuggerUND, TapFileCntlUND, MCR c15,1,2 clock, MCR c7,c7,0
   cache" ✅
3. **CP15 StrongARM clock** — line 88: "MCR p15, 0, Rn, c15, c1, 2
   handled in `handle_und`" ✅
4. **TSerialChip (4x)** — line 90-91: "`peripherals/serial.rs` —
   Status returns TX/RX empty" ✅
5. **CP10/11 native primitives** — line 92: "CPTR_EL2.TFP enabled;
   MCR dispatch via `native_primitives::execute`" ✅
6. **Screen blit** — line 94: "`peripherals/screen.rs` — Real blit
   copies into GUEST_FB" ✅
7. **Einstein ROM patches** — line 96: "apply 717006 patches from
   TJITGenericROMPatch.cpp" ✅

### End-of-Phase-A milestone

**PLAN.md line 169-174:**

> "All 13 tests pass at the current commit. Boot reaches deep
> initialisation code past `0x0E6B94`. No tight loops from tick
> polling."

---

## BUT: Phase B reveals the rot (INVESTIGATION.md)

### Current stall (INVESTIGATION.md line 36-49)

> "the 717006 kernel reaches 72 trace-able functions deep and
> fails flash-chip identify inside
> `TNewInternalFlash::CheckFor1LaneFlash`... our stage-2 maps
> `0x0200_0000../0x1000_0000..` as RW-RAM but doesn't model the
> Intel 28F016 command-set ('Read Identifier Codes' 0x90 →
> manufacturer 0x89, device 0xA0, etc.). **Give the flash window
> a real driver model or short-circuit identify.**"

### Tensions between documents

**HIGHLEVEL.md §3.1 (line 50-58)** lists "What works end-to-end":

> "**VIC state** (`int_present`, `int_ctrl`, `fiq_mask`, edge
> registers, four match registers)"

But PLAN.md Phase B line 134 says "Pass `TDMAManager::Init`" and
line 135 "likely trips TInterruptManager-backed delays" — implying
VIC init is *not* actually complete, it was scaffolding.

**PLAN.md line 97-109** defers Einstein patch items as "out of
early-boot critical path":
- `TJITGenericPatchNativeCall` entries (DebugStr, Debugger,
  RealClockSeconds, etc.)
- `TVirtualizedCallsPatches` entries (`__rt_sdiv`, `__rt_udiv`,
  `symcmp`)

**Yet line 96 claims:** "Missing from Phase A by oversight — the
omission forced us to debug Einstein-specific symptoms in Phase B
that had nothing to do with our hypervisor."

### Git log WIP/TODO flags in last 30 commits

Only **one commit subject** carries explicit work-in-progress
language:
- `f99b0f24` — "function-trace feature + **UND trampoline R0/R1
  preservation**" (not WIP, but the R0/R1 clobber bug showed
  Phase A missed a critical register-preservation case)

No "WIP" / "TODO" / "FIXME" in commit subjects — all Phase A
commits claim they "land" real handlers.

### The core leak: INVESTIGATION.md line 56-89

**Open block in Phase B (still unsolved at audit time):**

> "the 717006 kernel reaches 72 trace-able functions deep...
> Empirical tests with a UDF canary... show the same behaviour:
> the kernel never fetches code from our Einstein.rex, and
> `SearchForFlashDrivers` falls back to the built-in
> `T28F016_SA_SVDriver`... our stage-2 `RAM_MIRROR` at IPA
> `0x0C00_0000` and the kernel's stage-1 mapping of VA
> `0x0C10_0000+` **disagree on where in host RAM a given guest-VA
> lands**."

**This is a Phase A foundational bug** — the guest RAM layout
(line 5.2 of HIGHLEVEL.md) was supposed to be settled before
Phase B started, yet Phase B discovered stage-2 vs. stage-1
disagreement on a core memory window.

---

## Summary at audit time

- **Phase A claimed:** 14/14 guest tests, all handlers real, no stubs.
- **Phase A actually delivered:** Guest tests pass in isolation,
  but Phase B immediately hits:
  - flash-identify (no Intel 28F016 model)
  - REx not visible to guest (stage-2 / stage-1 mismatch
    hypothesised at the time — later falsified)
  - two DebuggerUND panics masked as reboot tail
  - UND-trampoline register clobber (tracer transparency bug)
- **Implication:** Phase A's "real handler" bar was too low —
  guest-test isolation doesn't catch hypervisor-level bugs that
  only emerge under full ROM boot.

The audit that followed produced
[`plan.md`](plan.md), which enumerates every Einstein-parity
piece missing at this point plus the Tier-0 RAM-mirror
investigation.
