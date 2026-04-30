# Plan — Drive Newton OS to interactive use

## Status

**Maintenance note (auto-prune):** Each iteration, BEFORE adding a new
iter-N section, prune the old one(s) so PLAN.md stays small. The full
history lives in `git log`. Keep only: this Status block + the most
recent 1-2 iteration sections + the reference sections at the bottom.
Bloated PLAN.md wastes context every read.

**Hard rules** (user directives still in force):

- Hypervisor-side compensation for subpage-AP incompatibility is OFF
  the table (2026-04-29). The fix MUST be a kernel patch.
- Run the *original ROM code*; no workarounds, no deferrals, no
  shortcuts; fix all warnings before each commit.
- All 36 guest tests must pass on every commit
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-48):** Lookup's TFlashBlock-pointer table at
`TFlashStore->[+44]` contains wild entries. iter-47 added a probe at
c74c8 (`ldr r0, [r4, #44]`) and observed the base = `0x0c605848` is
ALWAYS sane in 8+ logged calls (r4 = `0x0c604c04` = the sole boot-
time TFlashStore). So iter-46 hypothesis (a) "wild base" REJECTED;
hypothesis (b) "sane base, wild/NULL entry" CONFIRMED. The boot now
progresses past the iter-46 wedge and hits an unaligned-load fault
at `0xc0c9c: ldr r1, [r0, #8]` (LogEntryOffset entry) with r0 =
`0x0a000005` — yet another wild TFlashBlock\* pulled from the table.

iter-48 must probe the natural table-indexed load at c74cc (`ldr
r0, [r0, r1, lsl #2]`) — capture (base, index, returned). Halt
when `table[index]` is outside RAM (0x0c000000..0x10000000) and
ROM (0..0x800000). Dump table neighbourhood at the bad index +
walk the caller chain. Then decide: are we iterating PAST the
valid extent (r1 too large), or are specific table entries being
overwritten between init and use?

### Iteration 46: bus-abort is PhysBlock(NULL) via Lookup→IsVirgin→LogEntryOffset

Added c2418 probe (the iter-45 plan) — confirmed the c2418 site
is NOT where the fault lives (8 sane firings, none on wedge).
Re-decoded the kernel DABT trace: faulting PC = 0x000c0cd8
(`ldr r2, [r0, #36]` inside PhysBlock). iter-44's PhysBlock probe
only halted on bit-31-set r0 — extended it to also halt when
`*[r0+0]` is wild (gated on r1 != -1 so the early-return path is
unaffected). Result on cold boot: r0 = NULL — PhysBlock called
with NULL `this`. `*[NULL]` reads VA 0 = the reset vector instr
0xea0061a0; `[0xea0061a0+36] = 0xea0061c4` = unmapped → bus abort
(matches the FAR we've been chasing since iter-43).

APCS FP chain (decoded from c0808 = IsVirgin, c747c = Lookup):
PhysBlock(NULL) ← `LogEntryOffset__11TFlashBlockFv`(NULL,
wrapper@c0cac) ← `IsVirgin__11TFlashBlockFv`(NULL, c0808) ←
`TFlashStore::Lookup` (c747c, bl IsVirgin at c74d4).

Lookup body around c74c0..c74d4:

```
   c74c0: ldr r1, [r4, #96]        ; r4 = TFlashStore* this
   c74c4: lsr r1, r7, r1
   c74c8: ldr r0, [r4, #44]        ; r0 = this->[+44] (block-table base)
   c74cc: ldr r0, [r0, r1, lsl #2] ; r0 = table[index]
   c74d0: mov r9, r0
   c74d4: bl IsVirgin               ; r0 = NULL on wedge
```

The pre-wedge `dabt-trip` line in the cold-boot output captured
r0=0x20000000 r1=0x00000027 at PC=c74cc — i.e., on the wedge
call, table base = `[r4+44]` = 0x20000000 (wild, outside
RAM/ROM ranges). The IPA 0x2000009c is unmapped, so the load
returned 0 (NULL), which then propagated into IsVirgin →
LogEntryOffset → PhysBlock(NULL).

#### Next iteration plan (iter-47)

1. **Probe at c74c8 (`ldr r0, [r4, #44]`)** — capture r4 (the
   TFlashStore*) and the loaded table base. Halt if base has
   bit-31 set or is outside RAM (0x0c000000..0x10000000) /
   ROM (0..0x800000). This bisects "wild base" vs "sane base
   but indexed entry is NULL".

2. **If the table base is wild** (= 0x20000000): trace who
   wrote it. Likely candidates: a stage-2 alias to a different
   PA (cf. iter-21 PA=0x04084000 alias finding), or a
   write-during-init bug in TFlashStore setup. Add a stage-2
   RO trap on TFlashStore+44 once the TFlashStore instance is
   identified.

3. **If the base is sane but `table[r1]` is NULL**: the bug is
   that Lookup iterates past the valid block-table extent.
   Check r1 against TFlashStore's table-size field (probably
   adjacent to [+44]).

4. **Sanity**: 64 prior PhysBlock calls had sane r0 values
   (0x0c605950, 0x0c605970). The transition to r0=NULL on
   call #65+ suggests iter-past-end rather than gradual
   corruption — favors hypothesis #3.

### Iteration 47: Lookup's `[r4+44]` table base is consistently sane — wild values are TABLE ENTRIES, not the base

Patched `c74c8: ldr r0, [r4, #44]` with HVC. Handler reads
`[r4+44]`, halts if outside RAM (0x0c000000..0x10000000) or ROM
(0..0x800000), else emulates and continues.

Cold-boot result: 8+ logged Lookup-base events all show
`r4=0x0c604c04` (sole boot-time TFlashStore, lives in RAM), and
`[r4+44] = 0x0c605848` (in RAM, sane) for every call. **No halt
fired.** Hypothesis (a) "wild base = 0x20000000" REJECTED. The
prior iter-46 `dabt-trip: PC=0xc74cc r0=0x20000000` line was a
recoverable kernel DABT on an unrelated path — not the wedge call.

The boot now progresses past iter-46's PhysBlock(NULL) wedge:
the iter-44 PhysBlock probe halts on `*[r0]` wild ONLY when r1
!= -1 (the early-return gate). Some path through Lookup now
returns `table[index] = 0x0a000005` (not NULL, not in PhysBlock-
probe halt class), and that wild pointer reaches
`LogEntryOffset__11TFlashBlockFv` at c0c9c → `ldr r1, [r0, #8]`
faults UNALIGNED (0x0a00000d & 3 != 0). End-of-boot output:

```
unaligned: cannot read aligned 0x0a00000c (EA=0x0a00000d) at PC=0xc0c9c
  r0..r7: 0x0a000005 0x00000027 0x0000000d 0xe59d0000
          0x0c604c42 0x0000000d 0x0c328e90 0x00000027
```

So `Lookup.table[index] = 0x0a000005` for some index. Either
the index is past the valid extent (table holds garbage past N),
or specific entries got overwritten after init.

#### Next iteration plan (iter-48)

1. **Probe at c74cc** (`ldr r0, [r0, r1, lsl #2]`). Capture
   `(base, index, table[index])`. Emulate the load. Halt when
   the loaded value is outside RAM/ROM. Dump 16 words around
   `base + index*4` to see whether neighbouring entries are
   sane (→ specific corruption) or wild (→ iterating past end).

2. **If iter-past-end**: Lookup's calling convention probably
   has a max-index check upstream. Check what `r7` (the search
   key) and `[r4+96]` (the shift) are doing — `r1 = r7 >> shift`
   should bound the index. Trace whether a too-large r7 is
   reaching Lookup.

3. **If specific-entry corruption**: install a stage-2 RO trap
   on the affected table page (PA backing 0x0c605848) once the
   bad index is known. Capture every writer.

4. **Cross-reference Einstein**: `build/NewtonProbe` should
   show what a fully-booted Newton's TFlashStore->[+44] table
   looks like. If our boot's table differs only in a few
   entries, that's the corruption fingerprint.

## Workflow per stop

1. Capture verify-mmu output (`fix_stage1_xn_bits` ratchets per
   alias-onset). Each alias is a `(PA, VA1, VA2)` tuple.
2. Identify the kernel-side write that creates each alias by
   instrumenting the relevant L2-write entry point with an HVC probe.
3. Cross-reference with Einstein (`build/NewtonProbe baremetal/roms/
   newton.rom _Data_/Einstein.rex 30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — `src/peripherals/*.rs`, `src/trap.rs`.
   - **Einstein behavioural quirk** — port the matching logic.
   - **ROM patch** — `src/rom_patches.rs`. Only when no other layer can
     host the fix.
5. Re-run, observe alias count, repeat until zero.

## Tools

### Hosts

- **QEMU raspi3b** (default; `cargo run --release`) — fast, BCM2835
  VIC, AArch32↔AArch64 banking quirks documented in `docs/QEMU_BUGS.md`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — `scripts/fvp <elf>`. Accurate
  reference: GICv3, generic timer + cache model exact. Build with
  `--no-default-features --features platform-fvp-base`.

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`. Patches every
  `scripts/classify-out/code-symbols.txt` entry with HVC trampoline.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor function-
  entry traces.
- **`build/NewtonProbe`** — Einstein-as-oracle.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s from `trap_irq`.
- **Framebuffer PNG dumps** — `/tmp/newton-fb/NNNNN.png` after
  `screen::blit`.

### Debugging

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). Helpers `bg
  <addr>`, `bp <addr>`, `tt N`, `guest-state`.
- **DABT/PABT DIAG HVCs** at ROM offsets `0x10` / `0x0C`.
- **Software-reset canaries** — BootOS / PowerOffAndReboot / Reboot.

### Reference

- `scripts/disasm-out/rom.dis` — symbol-annotated ROM+REx disassembly.
- `docs/DISASM.md` (incl. "Jump-table aliasing — DON'T mistake the
  thunk for the body").
- `docs/NEWTON_INTERNALS.md` — APCS, ClassInfo dispatch, ROM patch
  table 0x01A00000..0x01C20000.
- `docs/QEMU_BUGS.md` — raspi3b AArch64↔AArch32 quirks.
- `docs/STRUCTURES.md` — kernel struct layouts (TScheduler, TTask,
  TStackManager, end-to-end page allocation).
- `docs/peripherals.md` — peripheral implementations.
- `probe/FINDINGS.md` — golden record from a fully-booted Newton.

### Tests

`baremetal/guest-tests/scripts/run-all.sh` runs the 36 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011 and runs the verify-mmu
  alias detector; UND-vector trampoline; DABT/PABT DIAG patches.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / SBA /
  tracer / canary / probe tags); `handle_page_get_probe`,
  `handle_remember_entry_probe_with` (with the new aliasing tracker);
  `handle_data_abort` with kernel-DABT forwarding for lazy stack
  growth.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, TPC, TPU, IMO, FMO, AMO,
  DC); CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3.
- `src/banked.rs` — AArch32 banked-register access from EL2 (Table
  D1-79).
- `src/rom_patches.rs` — Einstein word-write patches; HVC injection
  helpers; canaries; ResolveFault wrapper; `PAGE_GET_PROBE` patch.
- `src/peripherals/*` — Newton driver / native-primitive surface.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-*.bin`.
- `src/tracer.rs` — function-level tracer.
- `src/guest_bp.rs` — `bp <addr>` for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `guest-tests/tests/` — 36 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 36 tests must pass.

## Non-goals

- Real screen emulation beyond the framebuffer dump — no compositor,
  no pen input.
- Package loading — needs a solution for embedded native code.

## Diagnostic scaffolding (active)

- `verify-mmu` in `fix_stage1_xn_bits` — ratchet-logs subpage-AP
  heterogeneity and per-alias-onset `(PA, VA1, VA2)` tuples.
- `handle_page_get_probe` (PAGE_GET_PROBE_HVC_IMM=0x53) on
  `0x00258EFC` — page-allocator return logger + dup detector.
- `handle_remember_entry_probe_with` (REMEMBER_PROBE_HVC_IMM=0x46)
  on `0x00258E0C` — Remember-side per-PA → first-VA aliasing tracker
  (added to the existing L1-lazy-grow probe).
- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.

Pull these once the boot quiesces.
