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

**Current goal (iter-50):** **The bug is in shadow_stub's liveness
analyser, NOT in DoCommit.** iter-49 traced the wild Lookup `this`
back through the FindSuperceeder body's tail-call sequence:

```
   001488a8: mov ip, r1            ; ip = TFlashStore* (parent, from TObjRef[+16])
   001488ac: ldrb r1, [r1, #61]    ← byte access (shadow_stub UDF)
   001488b0..1488c0: teq, moveq, ldr r0, [r0], bic r1, ...
   001488c4: mov r0, ip            ← READS R12, but rom_patches replaced this PC
                                     with HVC #0x6E (FINDSUPER_MID probe) at boot
   001488c8: b Lookup_thunk        ; tail-call
```

**Root cause (chain):**

1. At hypervisor boot, `apply_717006_patches` (in `load_rom`) installs
   the FINDSUPER_MID probe HVC at PC=0x001488c4, replacing the original
   `mov r0, ip`.
2. THEN `shadow_stub::patch_rom_from_bitmap` runs and sees the
   byte-access at 0x001488ac.
3. Its liveness analyser walks ROM from PC=0x001488b0 forward. At
   PC=0x001488c4 it reads HVC #0x6E (treated as `BLink` →
   APCS-clobber-R0..R3+R12+R14) instead of the original `mov r0, ip`
   (which would mark R12 LIVE).
4. The picker concludes R12 is dead and picks it as scratch_ea.
5. Stub's `ADD R12, R1, #61` clobbers ip with `TFlashStore* + 0x3d`
   (XOR'd with 3 → +0x3e per BE32→LE32 fixup).
6. Body's `mov r0, ip` (post-HVC, run after probe handler emulates
   the original) reads the clobbered ip → Lookup gets wild this.

The same class of bug as iter-41 (R14 chosen as scratch_fl despite
being live across a tail-call). iter-42 worked around iter-41 by
removing R14 from the candidate pool. The CORRECT fix addresses
both (and any future similar case): make the analyser see the
original ROM bytes, not the post-probe-patch ROM.

iter-49 deliverables (committed):
- Production tracing logs the pick: `shadow_stub pick @0x001488ac:
  DeadReg sea=R12 sfl=None` — confirms the bug live.
- Two new regression unit tests in `src/shadow_stub.rs`:
  - `pick_scratch_at_findsuperceeder_does_not_pick_r12` — passes on
    pristine ROM (analyser correct given the right input).
  - `pick_scratch_at_findsuperceeder_when_midprobe_installed_does_not_pick_r12`
    — `#[ignore]`'d, fails on HVC-corrupted ROM (documents the bug).
- One unit test for the iter-41 R14 case
  (`pick_scratch_with_local_lr_read_does_not_pick_r14`) — passes,
  documents the correctness invariant.

iter-50 must pick a fix:
1. **Reorder install** — call `shadow_stub::patch_rom_from_bitmap`
   BEFORE `apply_717006_patches`. Simplest, but requires verifying
   no shadow_stub site PC overlaps a rom_patches probe PC (a quick
   audit of both lists).
2. **Original-ROM-aware analyser** — pass shadow_stub a "shadowed
   read function" that returns the pre-patch instruction at any
   PC in the rom_patches list. Slightly more code but localised
   to the analyser.

Option 1 is preferred unless the audit surfaces an overlap. Once
fixed, un-`#[ignore]` the second regression test and confirm it
turns green.

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

### Iteration 49: shadow_stub liveness analyser reads probe-corrupted ROM — picks R12 wrongly

iter-48 hypothesised "DoCommit doesn't initialise sp+148"; the
hypothesis is WRONG. iter-49 added a `*[r0]` wildness check in the
FindSuperceeder ENTRY probe (false positive — TObjRef[+0] is an
object ID like `0xf0000027`, not a pointer; the actual parent
TFlashStore* lives at TObjRef[+16]) and through that diagnostic
trace identified the actual mechanism.

The FindSuperceeder body uses ip (R12) as a save register across
the byte-access stub at 0x001488ac. The picker chose R12 as
scratch_ea because the liveness analyser was looking at the
**already-patched** ROM (rom_patches replaced the LOCAL
`mov r0, ip` at 0x001488c4 with HVC #0x6E for the FINDSUPER_MID
probe). HVC is treated as `BLink` → caller-saved clobber → R12
"dead" → picked. Stub clobbers ip → wild this to Lookup.

This is the same root cause as iter-41 (R14 picked as scratch_fl).
iter-42 worked around iter-41 by excluding R14 from the candidate
pool — a band-aid that doesn't address the analyser's read-the-
post-patch-ROM problem.

#### Iter-49 deliverables

- Tracing in `emit_inline_stub` for known-buggy sites — confirms
  in production cold boot:
  ```
  shadow_stub pick @0x001488ac: DeadReg sea=R12 sfl=None
  ```
- Three regression unit tests in `src/shadow_stub.rs::tests`:
  - `pick_scratch_at_findsuperceeder_does_not_pick_r12` — passes
    when the analyser sees pristine ROM bytes; demonstrates the
    analyser is correct given the right input.
  - `pick_scratch_at_findsuperceeder_when_midprobe_installed_does_not_pick_r12`
    — `#[ignore]`'d; fails on HVC-corrupted ROM. Will turn green
    when iter-50 lands.
  - `pick_scratch_with_local_lr_read_does_not_pick_r14` — passes;
    locks in the iter-41 invariant.

#### Next iteration plan (iter-50)

Pick one of:

1. **Reorder install** — move `shadow_stub::patch_rom_from_bitmap()`
   BEFORE the `apply_717006_patches` call inside `load_rom`. Audit
   that no shadow_stub byte-access PC overlaps a rom_patches probe
   PC (the lists are small; a static assertion at install time is
   easy).

2. **Original-ROM shadow** — keep a side-table of `(PC, original
   instruction)` pairs maintained by `apply_717006_patches`, and
   give shadow_stub's analyser a reader function that consults it
   first. Local change to the analyser; no ordering constraint.

After the fix lands, un-`#[ignore]` the regression test and verify
it turns green. Re-run the cold boot — iter-48's wedge (Lookup
called with wild this) should be gone.

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
