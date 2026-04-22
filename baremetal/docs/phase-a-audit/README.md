# Phase A closeout audit — 2026-04-21

Archive of the audit that drove the Phase A closeout. The user
noticed the original Phase A handoff kept surfacing missing
Einstein-parity pieces during Phase B, so we spawned three
Explore subagents to independently catalog Einstein's behavior,
our hypervisor's state, and the existing planning docs — then
diffed the three to produce the true Phase A todo list.

Contents:

- [einstein-non-rom-catalog.md](einstein-non-rom-catalog.md) —
  everything Einstein's emulator does *other than* interpreting
  ROM instructions (native-primitive driver dispatch, MMIO
  ranges, interrupt sources, ROM patches, coprocessor handling,
  initial register state, etc.). Cited with file:line back into
  `Emulator/`.
- [hypervisor-inventory.md](hypervisor-inventory.md) — what our
  hypervisor did *before* this audit, category by category,
  with the same structure so the diff is mechanical.
- [planning-docs-summary.md](planning-docs-summary.md) —
  condensed summary of `PLAN.md` / `HIGHLEVEL.md` /
  `INVESTIGATION.md` / `README.md` / `IMPLEMENTATION.md` /
  `CLAUDE.md` / `probe/FINDINGS.md` with verbatim quotes for
  what Phase A was supposed to cover.
- [plan.md](plan.md) — the tiered todo list that emerged from
  diffing the first two documents. Every item has been landed
  in the two commits immediately preceding this archive:
  `baremetal: Phase A closeout — Einstein-parity handlers`
  and `baremetal: Phase A — ROM patches replacing Einstein
  SWI-injection`.

These documents are frozen snapshots of the audit. Don't edit
to reflect new findings — write a fresh audit instead when the
next cliff comes.
