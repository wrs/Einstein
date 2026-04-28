# Working-style notes for Phase B

## Always round-trip ARM encodings through the assembler

When designing a ROM patch that replaces existing AArch32
instructions (especially data-processing immediates with non-trivial
constants), **never trust hand-computed encodings** — round-trip
through `arm-none-eabi-as` then `arm-none-eabi-objdump -d`:

```sh
cat > /tmp/check.s << 'EOF'
.arm
.syntax unified

mov r0, #0x9000
sub r1, r0, #4096
add r0, r0, r0, lsl #3
@ ...etc
EOF
arm-none-eabi-as -mcpu=cortex-a8 -o /tmp/check.o /tmp/check.s
arm-none-eabi-objdump -d /tmp/check.o
```

This catches imm12 rotation mistakes — for example, `mov r?, #0x9000`
is `0xE3A?_?A09` (imm8=0x09, rot_imm=10 → ROR(0x09, 20)). Hand
computation easily produces `0xC09` (rot_imm=12 → ROR(0x09, 24) =
0x0900, not 0x9000) which assembles to a different value, silently.
Also catches shift-amount typos and Rd/Rn/Rm field swaps.

The assembler must be available — `/Applications/ArmGNUToolchain/`
on macOS, or `apt install gcc-arm-none-eabi` on Linux.

**Why:** the 2026-04-28 FMNewStack patch attempt installed
`mov r?, #0x9000` as `0xE3A?_?C09` (hand-computed, wrong) on first
draft. The hypervisor would have applied wrong values silently — the
pre-patch sanity check (`patch_probe`) only verifies the *original*
word matches expected, not that the *new* word decodes correctly.
Assembler round-trip caught it before commit.

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
