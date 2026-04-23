# Working-style notes for Phase B

## Verify Einstein-driver ports with a review sub-agent

When landing any Rust port claimed to mirror Einstein (typical comment:
"Mirrors Einstein's `Foo::Bar` at `Emulator/X.cpp:N`"), spawn a
code-review sub-agent BEFORE committing.

Prompt the agent with both files (Einstein C++ + Rust port) and ask it
to list any Einstein logic that isn't faithfully reproduced. Treat
"no divergence" as the exit criterion.

**Why:** Phase B's flash work transcribed most of Einstein's
`TMemory::WriteToFlash16Bits` — including the 'high-half / low-half
based on `PA & 2`' split — but silently dropped the load-bearing
`theOffset = (theAddress − kFlashBank1) / 2` step. Writes landed at
4-byte stride, reads via the 0x30000000 alias went linearly through
the same backing, and `CompareFlashAndMemRebootIfDifferent` failed on
every halfword boundary. ~30 000 trace entries + a full flash-dump
walkthrough to find it. A 5-minute side-by-side review would have
caught it.

**Especially scrutinise:** offset math, masks, shifts, and any
conversion between `theAddress` / `theOffset` / lane selection — that's
where silent truncations hide.

## Guest tests for Phase A / Phase B hypervisor features

`guest-tests/tests/` has the end-to-end tests that exercise every
handler from inside the guest. `guest-tests/scripts/run-all.sh`
runs the full set.

When adding a new handler / MMIO stub / CP15 behavior:

1. Write the hypervisor code.
2. Write `guest-tests/tests/test_<feature>.S` that drives the
   feature and uses HVC #0x03 / #0x04 (PASS / FAIL) to signal
   outcome.
3. Add a MANIFEST entry so `run-all.sh` picks it up.
4. Run the full test suite before marking the task done.

Pattern reference: `test_flash.S`, `test_vic.S`,
`test_native_primitives.S`, `test_screen_blit.S`.

## Finish-the-phase semantics

When the user says "finish Phase X" / "do everything known to be
necessary in Phase X", land every item on the known-required
checklist. Don't:

- Propose "defer X / implement X now" as a choice.
- Classify items as "deferrable" or "only needed when feature Y runs".
- Treat plan tier labels ("Tier 3 — deferrable") as permission to skip.

Tier labels describe prioritisation order, not do/don't-do. If an
item is genuinely impossible (missing dependency), surface the
blocker and ask — don't quietly defer.
