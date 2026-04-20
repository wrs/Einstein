# Probe findings — 717006

Results of running `NewtonProbe` against the `717006` Newton ROM, with the
Einstein REx, on a Linux x86-64 host (`TEmulator::Run` + generic JIT, 90
wall-clock seconds).

Raw captures:
- [`results-717006-30s.txt`](results-717006-30s.txt) — 30 s cook, minimal
  boot past the MMU-enable point.
- [`results-717006-90s.txt`](results-717006-90s.txt) — 90 s cook, gives the
  kernel more time to install per-task stacks and heap pages.

## Answer to HIGHLEVEL.md §16.2: descriptor formats in use

**Descriptor types the 717006 ROM actually uses in mapped regions:**

| Type | Size | Present? |
|---|---|---|
| Section | 1 MiB | **Yes**, throughout the low kernel / ROM / flash windows |
| Large page | 64 KiB | **Yes**, VA 0x00000000–0x00100000 (ROM window) |
| Small page | 4 KiB | **Yes**, jump tables, kernel stacks, domain heap, per-task fragments |
| Tiny page | 1 KiB | **No.** Not a single descriptor observed in 90 s. |

**Fine page tables (ARMv4 L1 descriptor 0b11):**

Three L1 slots contain fine-table descriptors:

```
VA 0x78000000 – 0x80000000  (128 MiB)   fine: fault
VA 0x90000000 – 0x9C000000  (192 MiB)   fine: fault
VA 0xA0000000 – 0xAC000000  (192 MiB)   fine: fault
```

Every L2 entry in all three fine tables is `fault`. They exist as sparse
reservations — likely placeholders for PCMCIA card windows (`0x90000000`
and `0xA0000000` are the PCMCIA bases per `TMemoryConsts.h:199`) — not as
paths to actively-mapped memory.

## What this means for the bare-metal hypervisor

Cortex-A53 AArch32 short descriptor supports sections, 64 KiB large pages,
and 4 KiB small pages — everything 717006 actively uses. Fine tables and
tiny pages were dropped from ARMv7+ short descriptors, which would be a
blocker **if the Newton were relying on them for any real mapping**. It
isn't.

### What still needs handling

The three fine-table L1 entries would be interpreted as UNPREDICTABLE by
the A53 short-descriptor walker. Since all their L2 entries are `fault`,
the cleanest fix is to substitute them with L1 fault descriptors
(`bits[1:0] = 0b00`) at the moment the guest installs its page table.
Options, in rough order of increasing work:

1. **Post-write rewrite.** Trap guest writes to TTBR via `HCR_EL2.TVM`;
   on each write, scan the new L1 table, rewrite any 0b11 entries to
   0b00 in a shadow copy, point real TTBR at the shadow. The guest never
   notices because those VAs would fault either way.
2. **Synthetic coarse replacement.** Replace each 0b11 entry with a 0b01
   descriptor pointing at a statically-allocated coarse L2 of all-fault
   entries. Same observable behavior, marginally more memory.
3. **Lazy shadow.** Leave the guest's tables alone; let stage-2 translation
   intercept any access to the three VA ranges (they're above 0x78000000
   so they won't collide with anything we care about). This works because
   the guest never successfully dereferences into these regions anyway.

Recommendation: option 1 during the first stage-2 setup pass. It's a few
lines of code in the EL2 trap handler for `MCR p15, 0, Rn, c2, c0, 0`
(TTBR write), and the rewrite keeps the guest tables byte-accurate where
the A53 walker actually reads them.

### What this answers

- §16.2 — **descriptor formats**: decisively resolved for 717006. Only
  section / 64 KiB / 4 KiB are in active use. No tiny pages. Fine-table
  descriptors exist but are empty placeholders, trivially handled.
- §16.8 — **physical aliases and mirrors**, partially: the dump enumerates
  every mapped L2 region. Cross-check against the intended stage-2 map.

### What it does not answer

- §16.3 (PL0 vs PL1 by region), §16.4 (CP15 ops), §16.5 (SWP frequency),
  §16.6 (domain usage patterns). These need additional instrumentation
  inside `TARMProcessor` — not added in this first probe pass.
- Other 2.x ROM variants (737041, localised builds, eMate). Rerun the
  probe against each when available.

## Reproduction

```bash
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom - 90 > out.txt 2>&1
```

The `-` tells the probe to use Einstein's built-in REx rather than an
external `Einstein.rex` file. Change the `90` to any wall-clock duration;
30 s is enough to catch the MMU coming up, 90 s gives the kernel time to
fill in more per-task fragments. 717006 output stabilises at that scale;
we have not observed any new descriptor types appearing between 30 s and
90 s.
