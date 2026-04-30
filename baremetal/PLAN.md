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

**Current goal (iter-49):** Lookup is called with **wild this**
(r4 = 0x0c604c42 = TFlashStore* + 0x3e, NOT 0x0c604c04). iter-48's
table-indexed probe at c74cc halted with `base=0x00100000 index=0x27
entry=0x0a000005` — the "base" (= [r4+44] = [0x0c604c6e]) is just
random bytes from a stack/heap region that happen to land in ROM
range (so iter-47's base check missed it). The real bug is upstream:
**Lookup's `this` argument is wild**.

The call path: UnlockStore → DoCommit → FindSuperceeder (TAIL-CALL)
→ Lookup. FindSuperceeder body at 0x001488ac..0x001488c8 ends with
`ldr r0, [r0]; bic r1, r0, #0xf0000000; ...; b Lookup_thunk`. So
Lookup's `this` = `*[input_TObjRef]` from FindSuperceeder. The
input TObjRef is `DoCommit.sp+148` (a stack-local TObjRef in
DoCommit's frame). On the wedge call, `*[sp+148]` = 0x0c604c42 —
garbage, not a real TFlashStore* (note the iter-37/38 narrative
already saw r0=0x0c604c42 entering Lookup; we just hadn't yet
identified that it's coming from a poorly-initialised TObjRef
local).

iter-49 must probe FindSuperceeder thunk entry (0x01af8c14) or
FindSuperceeder body entry (0x001488a0) to capture incoming r0
(the TObjRef ptr) AND dump 8 words at that pointer (the full
TObjRef contents). Halt when TObjRef[+0] is outside RAM/ROM (=
wild). Walk the caller chain to identify where in DoCommit the
TObjRef was supposed to be populated.

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

### Iteration 48: r4 (Lookup's `this`) is itself wild — the bug is upstream of Lookup

Patched `c74cc: ldr r0, [r0, r1, lsl #2]` with HVC. Halt when
`table[index]` is outside RAM/ROM. Cold boot: 12 logged Lookup-idx
events with sane entries, then halt:

```
TFlashStore::Lookup loaded WILD entry from table[index]
  base=0x00100000  index=0x27 (39)  entry_va=0x0010009c  entry=0x0a000005
  TFlashStore* r4=0x0c604c42  r7 (search key)=0x00000027
  caller_lr=0x000c4c4c  sp=0x0c328dec  fp=0x0c328e14
```

**r4 = 0x0c604c42**, not the sane 0x0c604c04 we saw in iter-47's 8
logged calls. r4 = TFlashStore + 0x3e — clearly wild (also non-
4-aligned). iter-47's c74c8 probe accepted base=0x00100000 as
"sane" (it's < 0x800000 = ROM range), so the upstream wildness
slipped through. The "table" at base 0x00100000 is literally ROM
code bytes (table[0x21] = `e1a08000 = mov r8, r0`, etc.) —
treating ROM instructions as 4-byte TFlashBlock pointers. The
entry at index 0x27 is `0a000005` (= `beq 0x18` instruction
encoding), which has bits set outside RAM/ROM ranges → halt.

#### Decoded call chain (from FP walk + disasm)

```
[0] fp=0x0c328e14  pc_at_fp=0x000c7488  caller_lr=0x000c96cc  ; Lookup
[1] fp=0x0c328ee8  pc_at_fp=0x000c94fc  caller_lr=0x000c87bc  ; DoCommit
[2] fp=0x0c328f70  pc_at_fp=0x000c875c  caller_lr=0x00387060  ; UnlockStore
[3]+ ...
```

UnlockStore (c8750) → DoCommit (c94f0) → at c96c8 `bl
FindSuperceeder` with r0=sp+148, r1=sp+120. FindSuperceeder body
at 0x001488a0..0x001488c8 ends with:

```
   001488bc: e5900000  ldr r0, [r0]                  ; r0 = *[input TObjRef]
   001488c0: e3c0120f  bic r1, r0, #0xf0000000       ; r1 = lower 28 bits
   001488c4: e140067e  UDF (SBA-emulated load)
   001488c8: ea66d9a8  b   0x01afef70 → Lookup       ; tail-call
```

So **Lookup's r0 = `*[DoCommit.sp+148]` = TObjRef[+0]**. On the
wedge call, that's 0x0c604c42 (garbage). The TObjRef at
sp+148 is a stack-local in DoCommit; it's not properly
initialised before being passed to FindSuperceeder.

(Iter-37/38 already observed `Lookup r0=0x0c604c42` entering
the wedge, but at the time we were chasing the r3=0x80000110
shadow_stub corruption — iter-42 fixed that — and missed
that r0 was independently wild.)

#### Next iteration plan (iter-49)

1. **Probe at FindSuperceeder thunk or body entry** (0x01af8c14
   or 0x001488a0). Capture incoming r0 (= the TObjRef pointer
   passed by DoCommit) AND dump 8 words at that pointer (the
   TObjRef contents). Halt when TObjRef[+0] is outside RAM/ROM.

2. **If TObjRef[+0] is consistently wild from the first call**:
   the bug is in DoCommit's setup — the TObjRef at sp+148 is
   NEVER properly initialised, OR it's initialised by a path
   that's broken. Check what writes sp+148 in DoCommit upstream
   of c96c0.

3. **If TObjRef[+0] is sane initially and goes wild later**:
   stack overflow from a callee or aliasing. Check whether
   sp+148's PA aliases another active VA.

4. **Cross-reference iter-37/38**: those captured `Lookup r0
   =0x0c604c42` for the SAME wedge call (caller_lr=c96cc).
   The value is recurring — strongly suggests a deterministic
   init bug, not random corruption.

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
