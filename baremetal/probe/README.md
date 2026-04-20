# NewtonProbe — headless Einstein instrumentation harness

A small executable that boots a Newton ROM inside Einstein's emulator core
(no FLTK, no UI), lets it run for a fixed wall-clock window, then dumps MMU
state and exits. Used to answer the open questions in
[`../../HIGHLEVEL.md`](../../HIGHLEVEL.md) §16 against real ROMs.

## Build

The `NewtonProbe` target is added to Einstein's main `CMakeLists.txt`. After
the regular Einstein dependencies (FLTK + newt64) are in place, build just the
probe:

```bash
cmake --build build --target NewtonProbe
```

## Run

```bash
build/NewtonProbe baremetal/roms/newton.rom [rex|-] [seconds]
```

- `rom` — path to a raw 8 MiB Newton ROM dump (big-endian as captured).
- `rex` — path to `Einstein.rex`, or `-` to use the built-in REx image.
- `seconds` — wall-clock seconds to let the ROM run before dumping (default 30).

Output is plain text on stdout: a banner, the time at which the guest MMU came
up, the final PC / TTB / DACR, and `TMMU::FDump`'s annotated memory map.

## What the probe prints

Each region of the L1 table is summarised as one line of the form:

```
VA 0x<start> to 0x<end> (<kB> kB): <descriptor-type>
```

Descriptor types:

- `section` — ARMv4 L1 section (1 MiB).
- `large pages` / `small pages` — L2 entries in a coarse (L1 bits 0b01) table,
  64 KiB or 4 KiB respectively.
- `fine: fault` / `fine: large pages` / `fine: small pages` /
  `fine: TINY PAGES` — L2 entries in a fine (L1 bits 0b11) table. Tiny pages
  (1 KiB) only exist here, and fine tables only exist on ARMv4/v5; ARMv7+
  short descriptors do **not** walk them. Any `fine: TINY PAGES` hit flags a
  region that will need a hypervisor shadow or L1 rewrite before bare-metal
  A53 can host the guest.
- `fault` / `page fault` / `RESERVED` — unmapped.

## Current findings

See [`FINDINGS.md`](FINDINGS.md) for interpretation. Results against the
717006 ROM are captured in `results-717006-*.txt`.

## Adding more probes

The probe currently only dumps the MMU table (answers §16.2). Counters for
CP15 ops (§16.4), SWP frequency (§16.5), and mode transitions (§16.3) are
straightforward additions — each is a handful of fprintfs guarded behind
`#if PROBE_INSTRUMENT` in `TARMProcessor.cpp` or similar, plus a summary
print here. Do that when we're ready to answer those questions; the probe
scaffolding is in place.
