# Probe findings — 717006

Results of running `NewtonProbe` against the `717006` Newton ROM, with the
Einstein REx, on a Linux x86-64 host (`TEmulator::Run` + generic JIT, 90
wall-clock seconds).

Raw captures:
- [`results-717006-30s.txt`](results-717006-30s.txt) — 30 s cook, MMU-only
  dump from the first pass (before full instrumentation was wired).
- [`results-717006-90s.txt`](results-717006-90s.txt) — 90 s cook, MMU-only,
  used to confirm no late-boot tiny-page emergence.
- [`results-717006-90s-full.txt`](results-717006-90s-full.txt) — 90 s cook
  with the full instrumentation set (CP15 / SWP / mode transitions). The
  authoritative capture.

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

## Answer to §16.4: complete CP15 op set

Only **15 unique (opc1, CRn, CRm, opc2, dir) tuples** emitted in 90 seconds
of boot. This is the entire surface a bare-metal CP15 shim has to cover:

| dir | op1 | CRn | CRm | op2 | count | purpose |
|---|---|---|---|---|---|---|
| MRC | 0 | 0  | 0  | 0 |    37 026 | read CPU ID |
| MCR | 0 | 1  | 1  | 0 |    56 165 | write control register (MMU / S / R bits) |
| MCR | 0 | 2  | 2  | 0 |         1 | install TTBR — **once**, at boot |
| MCR | 0 | 3  | 3  | 0 |    38 953 | write DACR (always `0x00055555`) |
| MRC | 0 | 5  | 5  | 0 |       328 | read fault status register |
| MRC | 0 | 6  | 6  | 0 |       115 | read fault address register |
| MCR | 0 | 7  | 6  | 0 |     1 419 | cache op (invalidate data cache) |
| MCR | 0 | 7  | 6  | 1 |   427 067 | cache op (clean/invalidate DC entry) |
| MCR | 0 | 7  | 7  | 0 |         1 | cache op (invalidate unified cache) |
| MCR | 0 | 7  | 10 | 1 |   427 067 | cache op (clean data cache entry) |
| MCR | 0 | 7  | 10 | 4 |   427 067 | cache op (drain write buffer) |
| MCR | 0 | 8  | 5  | 0 |     1 259 | TLB flush — ITLB |
| MCR | 0 | 8  | 6  | 1 |     1 259 | TLB flush — DTLB entry |
| MCR | 0 | 8  | 7  | 0 |        13 | TLB flush — all |
| MCR | 0 | 15 | 1  | 2 |         1 | StrongARM-specific: clock control |

Every entry except the last is a standard ARMv4 CP15 op with a direct
AArch32-on-A53 equivalent (SCTLR, TTBR0, DACR, IFSR/DFSR, IFAR/DFAR, DC/IC/BPI
maintenance, TLBI). The StrongARM `c15 op1=0 CRm=1 op2=2` clock-control write
fires **once** at boot; trap-and-no-op is fine. The hot path is cache
maintenance (~1.28 M ops in 90 s); each op has a one-line AArch32 equivalent
(`mcr p15, 0, Rn, c7, c10, 1` → `DCCMVAC`, etc.), or we can trust A53
coherency and emit no-ops if we disable the guest cache entirely.

## Answer to §16.5: SWP frequency and sites

**405 810 word SWPs, 0 byte SWPs, from exactly one PC: `0x003AE200`.**

The kernel has a single atomic-exchange primitive (almost certainly a
compare-and-swap or lock-acquire wrapper). Because every SWP comes from the
same instruction, the bare-metal port has two clean options:

1. **Patch the ROM** at `0x003AE200` to a `LDREX`/`STREX` sequence. One
   patch covers 100 % of observed SWP traffic.
2. **Trap-and-emulate** via the ARMv8 UNDEFINED-instruction vector. At
   ~4.5 k SWPs/second under heavy boot load, hypervisor trap overhead is
   tolerable; in steady-state use it will be much lower.

## Answer to §16.3: privilege levels

Mode-transition counts over 90 s confirm Walter's recollection: **kernel-only
PL1, everything else PL0**. The guest's entries-into-mode tally:

| mode | entries |
|---|---|
| USR | **19 310** |
| SVC |    649 |
| IRQ |     44 |
| ABT |    232 |
| FIQ |      5 |
| UND |      1 |

The dominant transition is `SVC → USR` (19 143 events) — kernel returning
to user code. Every exception entry either comes from USR (IRQ, ABT, UND) or
is re-entered from kernel itself. No code path enters USR-to-USR directly, as
expected. AP enforcement is the operative protection model; hypervisor must
preserve it (which A53 short descriptor does trivially).

## Answer to §16.6: domain usage

DACR is written 38 953 times with the value `0x00055555` — the *same* value
every write. Decoded:

| domain | bits | meaning |
|---|---|---|
| 0–7 | `01` | client (AP bits honoured per descriptor) |
| 8–15 | `00` | no access |

No manager-domain usage, no dynamic domain reconfiguration, no weird
StrongARM-specific side effects. Eight client domains for the kernel to
assign to task isolation groups; the rest permanently faulting. Cortex-A53
short-descriptor DACR semantics match exactly. The high write count is the
kernel reinstalling DACR at context-switch boundaries — no behavioral
concern.

## Answer to §16.7: cache-line op encodings

Subset of the CP15 table above: six distinct c7 op encodings, all standard
ARMv4. AArch32-on-A53 equivalents:

| CP15 op | ARMv4 meaning | AArch32 A53 equivalent |
|---|---|---|
| `c7 c6 op2=0` | Invalidate entire data cache | `MCR p15,0,Rn,c7,c6,0` (still defined) or `DCISW` loop |
| `c7 c6 op2=1` | Clean+invalidate DC line (MVA) | `DCCIMVAC` |
| `c7 c7 op2=0` | Invalidate unified cache | deprecated; loop `DCISW` + `ICIALLU` |
| `c7 c10 op2=1` | Clean DC line (MVA) | `DCCMVAC` |
| `c7 c10 op2=4` | Drain write buffer / DSB | `DSB SY` |

All mappable to A53 with a one-line trap handler each, or no-op when we
treat the guest caches as pass-through.

## What remains open

- **Other 2.x ROMs** (737041, localised, eMate). Per your judgement, unlikely
  to differ meaningfully; rerun the probe against each when captured just
  to confirm the CP15 surface matches.
- **§16.8 physical aliases** — `FDump` enumerates mapped guest VAs but not
  the set of distinct guest PAs. Trivial extension: dump the PA range for
  each region. Do when needed for stage-2 sizing.
- **§16.9 RAM-size assumptions, §16.10 PCMCIA, §16.11 display geometry,
  §16.12 SMC, §16.14 input device.** Mostly not probe-answerable; they
  need either hypervisor-level experiments or design decisions.

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
