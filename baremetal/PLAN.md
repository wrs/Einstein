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

**Current goal (iter-47):** The Phase-B bus-abort (`evt.ex.abt.bus`,
FAR=0xea0061c4) is `c0cd8: ldr r2, [r0, #36]` inside `PhysBlock`,
called with r0=NULL via the chain
TFlashStore::Lookup → IsVirgin → LogEntryOffset (wrapper@c0cac) →
PhysBlock(NULL). The NULL originates in Lookup at `c74cc: ldr r0,
[TFlashStore->[+44], index, lsl #2]`. iter-47 must probe Lookup`s
table-base load (c74c8) to bisect:
  (a) wild base (= 0x20000000) → trace who wrote the wrong base, OR
  (b) sane base, NULL entry → Lookup iterating past valid extent.
Cold-boot dabt-trip captured r4[+44]=0x20000000 r1=0x27 at PC=c74cc,
which favours (a). 64 prior PhysBlock calls had sane r0 (0x0c605950
or 0x0c605970).

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

### Iteration 45 (next-loop iter 5): wrapper@c0cac fp always sane — fault is in the tail-called TFlashPhysBlock::LogEntryOffset, not in the wrapper

Iter-44 hypothesized fp corruption at the wrapper's ldmdb.
Iter-45 patches the wrapper's first instruction (`mov ip, sp`
at 0x000c_0cac, which is INSIDE `LogEntryOffset__11TFlashBlockFv`)
to capture incoming fp and walk the caller chain when wild.

#### Cold-boot output

All 8+ logged wrapper@c0cac calls have SANE incoming fp:

```
wrapper@c0cac #0: fp=0x0c328f14 caller_lr=0x000c0818 sp=0x0c328f08
wrapper@c0cac #1: fp=0x0c328f14 caller_lr=0x000c0818 sp=0x0c328f08
wrapper@c0cac #2: fp=0x0c328e88 caller_lr=0x000c0818 sp=0x0c328e7c
wrapper@c0cac #3: fp=0x0c328e88 caller_lr=0x000c0818 sp=0x0c328e7c
wrapper@c0cac #4: fp=0x0c328f14 caller_lr=0x000c0818 sp=0x0c328f08
wrapper@c0cac #5: fp=0x0c328f14 caller_lr=0x000c0818 sp=0x0c328f08
wrapper@c0cac #6: fp=0x0c328ea0 caller_lr=0x000c0818 sp=0x0c328e94
wrapper@c0cac #7: fp=0x0cc77d28 caller_lr=0x000c0818 sp=0x0cc77d1c
```

All fp values are in valid Tmux stack range. Hypothesis #2
(fp inherited wild) is REJECTED.

#### The actual fault site

`c0cac` is INSIDE `LogEntryOffset__11TFlashBlockFv` (entry at
0xc0c9c). The function:

```
000c0c9c <LogEntryOffset__11TFlashBlockFv>:
   c0c9c: ldr r1, [r0, #8]       ; early return check
   c0ca0: cmn r1, #1
   c0ca4: moveq r0, #0
   c0ca8: moveq pc, lr            ; return r0=0 if [r0+8]==-1
   c0cac: mov ip, sp              ← our probe
   c0cb0: push {fp, ip, lr, pc}
   c0cb4: sub fp, ip, #4
   c0cb8: bl PhysBlock             ; lr = 0xc0cbc
   c0cbc: ldmdb fp, {fp, sp, lr}
   c0cc0: b 0x1afef68 <LogEntryOffset__15TFlashPhysBlockFv>
                                   ← tail-call to a DIFFERENT class's
                                     getter — same name, different class
```

The tail-call target is `LogEntryOffset__15TFlashPhysBlockFv`
at 0x000c2418. Looking at it:

```
000c2418 <LogEntryOffset__15TFlashPhysBlockFv>:
   c2418: ldr r0, [r0, #12]      ← THE FAULTING INSTRUCTION
   c241c: mov pc, lr               ; return
```

A single-instruction getter that reads field at offset 12
from the TFlashPhysBlock pointer (= the value PhysBlock
returned). If PhysBlock returned a wild pointer, this ldr
faults.

Since `b 0x1afef68` is a plain branch (not bl), lr is
preserved as 0xc0cbc through the tail-call. Hence Throw's
caller_lr = 0xc0cbc matches even though the actual faulting
PC is at c2418 (inside a function entered via tail-call).

#### What PhysBlock returns

Reviewing PhysBlock's body:

```
000c0cc4 <PhysBlock>:
   c0cc4: ldr r1, [r0, #8]      ; r1 = this->[8] (offset)
   c0cc8: cmn r1, #1
   c0ccc: moveq r0, #0
   c0cd0: moveq pc, lr           ; if [r0+8]==-1, return 0
   c0cd4: ldr r0, [r0]           ; r0 = *this (a TFlashStore*)
   c0cd8: ldr r2, [r0, #36]      ; r2 = store->[36] (table base)
   c0cdc: ldr r0, [r0, #88]      ; r0 = store->[88] (shift)
   c0ce0: lsr r0, r1, r0         ; r0 = r1 >> shift (index)
   c0ce4: add r0, r0, r0, lsl #1 ; r0 = index * 3
   c0ce8: add r0, r2, r0, lsl #3 ; r0 = table + index * 24 (TFlashPhysBlock pointer)
   c0cec: mov pc, lr             ; return
```

PhysBlock returns `table_base + (offset >> shift) * 24`,
indexing into a TFlashPhysBlock array. If `index` overshoots
the array bounds OR `table_base` is corrupted, the returned
pointer is wild.

In the wedge call, PhysBlock might have returned a value
like 0xea0061b8 (TFlashPhysBlock pointer + 12 = 0xea0061c4
= the FAR we observed).

#### Next iteration plan (iter-46)

1. **Probe at LogEntryOffset__15TFlashPhysBlockFv entry**
   (PC=0x000c2418) to capture r0. Patch the `ldr r0, [r0, #12]`
   with HVC; if r0 is wild, halt with the caller chain
   walked. Otherwise emulate the load.

2. **If r0 entering this getter is wild**: the bug is in
   how PhysBlock computes the table index. Look at PhysBlock's
   inputs (its own r0 = TFlashBlock* and the [r0+8] offset
   field). If offset is too large or the table base
   (TFlashStore->[36]) points to wild memory, the returned
   pointer is wild.

3. **Cross-reference with the prior PhysBlock probe**: extend
   it to also log r0 at PhysBlock EXIT (the returned value)
   so we can pair-match `(input r0, returned r0)`.

#### Status

- Build clean.
- 65+ wrapper@c0cac calls observed with sane fp.
- Bus-abort precisely localized: `ldr r0, [r0, #12]` at PC=
  0x000c2418 inside `LogEntryOffset__15TFlashPhysBlockFv`,
  with r0 = wild TFlashPhysBlock* returned by PhysBlock.
- iter-45 deliverable: rejected the iter-44 fp-corruption
  hypothesis; pinned the actual faulting instruction; identified
  the data-flow source (PhysBlock's return value).

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
