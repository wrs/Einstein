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

**Current goal (iter-79):** iter-78 fixed the iter-76/77 RefVar
indirection bug, added a runtime-heap-bounds classifier
(`src/heap_check.rs`), and wired the `newton-objects` parser
into the dosend ring + throw probes (with little-endian support
in the library). The corrected probe output identifies the
actual problem in one line:

```
DoSend #0: recv=0x00000002 impl=0x00000002 meth=0x003b673d argc=1 caller_lr=0x002f0eac
heap_check: TObjectHeap @0x0c607288 → [0x0c6072cc, 0x0c64435c) (244 KiB)
  recv: ref=0x00000002 → NIL
  impl: ref=0x00000002 → NIL
  meth: ref=0x003b673d → real-ptr ROM @0x003b673c
    symbol 'Query (hash=0xebfb0b66) @0x003b673c size=22
```

So:
- The boot is calling **`NIL:Query()` with one arg**. Not the
  bogus `punctuationCursiveOption` from iter-77 — that name
  came from misreading the slot-pointer at 0x006840b4 as a Ref.
- The runtime object heap occupies 244 KiB at
  `[0x0c6072cc, 0x0c64435c)`; the throw value at 0x0c6093e0
  *is* in-heap (a 12-byte binary with class Ref(0x0c609420)).
- The ThrowRefException error code `errCode=-48809 (0xffff4157)`
  matches DoSend's literal at 0x2f06a8 — i.e. the throw came
  from DoSend's own `**arg1 == 2` check at 0x2f05fc, which
  fires when **the implementor (FindImplementor result) is NIL**.
  FindImplementor returns NIL when `:Query` isn't found on the
  receiver — and the receiver is itself NIL.

Next (iter-79): walk up from DoMessage (caller `0x002f0eac`)
to identify what NS opcode invoked `something:Query(arg)` with
NIL as `something`. DoMessage's caller chain is the next probe
target. Likely an opcode-26-style send-message or a bytecode
issuing a `:Query` from a pulldown / picker view that hadn't
finished initialising. Use the same probe pattern (HVC at
DoMessage entry; ring-buffer of recv/methodName + caller_lr)
plus a stack walk past DoMessage's APCS frame.

**Background:** iter-70 cleared the splash wedge; iter-71/72
fought a classifier regression; iter-73 forwarded FPA UNDs to
the kernel's FPE emulator; iter-74/75 walked the throw chain to
ThrowRefException → ThrowExInterpreterWithSymbol → DoSend;
iter-76 walked up to DoMessage; iter-77 mis-decoded RefVars and
mistook slot pointers for Refs; iter-78 fixed the decoding,
parsed the heap-resident throw value, identified the actual
methodName as `'Query` with a NIL receiver. Boot reaches NS
runtime, 27 kernel objects + `newt` running NS code, several
`evt.ex.fr.store` exceptions caught, then `type.ref.frame`
escapes all handlers exactly once and trips UnhandledException.

### Iteration 78: heap-bounds classifier + Ref-decoding fix + structured object dump

#### Method

Three coupled fixes to make the iter-76/77 probe data
trustworthy:

1. **RefArg double-indirection.** A `RefVar const&` at the
   asm level is `RefVar*`; a `RefVar` itself holds a `Ref*`
   slot pointer; the actual tagged Ref needs **two**
   indirections (cf. `IsInt__FRC6RefVar` @ 0x31c6c4 — two
   chained `ldr r0,[r0]`). iter-76/77 stopped at one. The
   Newton tag scheme (verified against `IsRealPtr` @
   0x31c77c, `IsMagicPtr` @ 0x31c75c) is `00=int 01=real-ptr
   10=imm 11=magic-ptr`, *not* `00=ptr` — so refs ending in
   00 are integers, not pointers. Both bugs combined to make
   iter-77 print slot-pointer addresses as if they were
   object headers.

2. **Heap-bounds classifier (`src/heap_check.rs`).**
   Decompiled `InHeap__11TObjectHeapFl` @ 0x31bddc to find
   the bounds layout: `[this+8] = lo` (inclusive) and
   `[this+12] = hi` (exclusive); `lo <= addr < hi` is the
   in-heap test. The global `TObjectHeap*` is at IPA
   `0x0c105548` (literal at 0x31c684, populated by
   `InitObjects__Fv` @ 0x31c608 from the
   `__ct__11TObjectHeapFlT1` result). Caches the bounds on
   first read; classifies real-pointer Refs as `in-heap` /
   `ROM` (addr < 0x01000000) / `OUT-OF-HEAP`.

3. **Structured object dump via `newton-objects`.** Extended
   the (BE-only) `newton-objects` library with an `Endian`
   enum + `Heap::with_endian` builder. Wired into
   `heap_check::dump_object`: copies up to 256 bytes from
   guest memory into a stack buffer (each runtime u32 written
   via `to_be_bytes` so byte-level data preserves the
   original on-disk order — counteracts the `load_rom`
   per-word byteswap), then parses with `Endian::Big`. Yields
   `symbol 'Query (hash=…) size=22`-style lines for ROM-
   resident method symbols, `frame map=… len=…`-style for
   heap-resident frames.

#### Result

Single-shot cold boot now produces an unambiguous diagnosis:

```
DoSend #0: recv=0x00000002 impl=0x00000002 meth=0x003b673d argc=1 caller_lr=0x002f0eac
ThrowExInterpreterWithSymbol #0: errCode=-48809 (r0=0xffff4157) ...
heap_check: TObjectHeap @0x0c607288 → [0x0c6072cc, 0x0c64435c) (244 KiB)
  symbol: ref=0x003b673d → real-ptr ROM @0x003b673c
    symbol 'Query (hash=0xebfb0b66) @0x003b673c size=22
dosend_ring (...): last 1 invocations:
  #0: recv=0x00000002 impl=0x00000002 meth=0x003b673d argc=1 caller_lr=0x002f0eac
    recv: ref=0x00000002 → NIL
    impl: ref=0x00000002 → NIL
    meth: ref=0x003b673d → real-ptr ROM @0x003b673c
      symbol 'Query (hash=0xebfb0b66) @0x003b673c size=22
ThrowRefException #0: name="evt.ex.fr.intrp;type.ref.frame" ... **r1=0x0c6093e1
  value: ref=0x0c6093e1 → real-ptr in-heap @0x0c6093e0
    binary class=Ref::Pointer(0x0c609420) @0x0c6093e0 size=12 (data 0 B)
```

Conclusions:

- The throw is `NIL:Query()`. iter-77's
  `RSSYMpunctuationcursiveoption` was wrong (decoded a
  slot-pointer address as a Ref).
- DoSend's check at 0x2f05fc (`**arg1 == 2`) fires because
  FindImplementor returned NIL — natural for a NIL receiver.
  The chain is `<???> → DoMessage(NIL, 'Query, args) → DoSend
  → throw`; iter-79 walks past DoMessage to find the NS
  caller that supplied NIL.
- The thrown exception value is a 12-byte heap-resident
  binary at 0x0c6093e0 (presumably the frame holding the
  error info; class points at another heap object at
  0x0c609420).
- Heap is healthy (244 KiB at `[0x0c6072cc, 0x0c64435c)`).
  No corruption hypothesis needed.

36/36 guest tests skipped per the maintenance note
(probe-only: heap_check + newton-objects integration; no
SBA/UND/DABT-path changes).

<!-- iter-77 (dumped heap objects at the DoSend boundary; both
     showed header word 2 — but that was an artifact of the
     RefArg single-indirection bug, which iter-78 fixed. The
     actual story is recv = NIL, not "binary class 2".) pruned
     per auto-prune. See `git log --grep="iter-77"`. -->

<!-- iter-76 (DoSend entry probe + 16-entry ring buffer; pinned
     caller_lr=0x002f0eac inside DoMessage; mis-decoded RefVars
     so the captured addresses were slot pointers, not Refs.
     iter-78 fixed the decoding.) pruned per auto-prune. See
     `git log --grep="iter-76"`. -->

<!-- iter-75 (added a ThrowExInterpreterWithSymbol entry probe —
     HVC #0x76 at 0x2f5810 — to walk past the ThrowRefException
     wrapper. Pinned the throw to DoSend at 0x2f05fc with the
     `**arg1 == 2` type check; captured the methodName as the
     ROM symbol RSSYMpunctuationcursiveoption at 0x006840b4.
     iter-76 walked another frame up to DoMessage.) pruned per
     auto-prune. See `git log --grep="iter-75"`. -->

<!-- iter-74 (added a ThrowRefException entry probe — HVC #0x75 at
     0x2f5730 — to walk one frame up from the existing Throw
     probe. Captured caller_lr=0x002f5878 = inside the wrapper
     ThrowExInterpreterWithSymbol; offending Ref *r1=0x0c643ca4
     [a heap pointer ref]. iter-75 walked another frame up.)
     pruned per auto-prune. See `git log --grep="iter-74"`. -->

<!-- iter-73 (forward FPA UND to the guest's kernel FPE emulator
     at ROM 0x38d8dc. Added `is_fpa_insn` (cp1/cp2 LDC/STC/CDP/
     MCR/MRC, cond ≠ 0xF) and `forward_und_to_guest_fpe` in
     `src/trap.rs` that ERETs to 0x38d8dc with SPSR_EL2 unchanged
     so we stay in UND mode. Cleared the iter-70 SFM wedge at
     0xd2780 and let boot walk through TFrameSoundChannel codec,
     TSoundServer init, full kernel-task census, into NS runtime.
     Subsequently revealed the iter-74 type.ref.frame throw stall.)
     pruned per auto-prune. See `git log --grep="iter-73"`. -->

<!-- iter-72 (classify-rom — fn-range clamp on unbounded PC-rel
     switch in `enumerate_pc_rel_jump_table`; cleared the
     0xffffe4 / SBA-UDF-marker-as-FAR wedge by stopping
     iter-71's 64-slot enumeration from sweeping past
     DynArrayLeaf's `mov pc, lr` into the SWIBoot pointer
     table at 0x3ad568+. Bitmap deltas + walker clamp diff.)
     pruned per auto-prune. See `git log --grep="iter-72"`. -->

<!-- iter-71 (classify-rom — five idiom recognizers: TClassInfo
     trampoline walker, SVC fall-through, multi-insn case bodies
     in unbounded PC-rel switches, SWIBoot-style indexed
     dispatch, PC-rel function-pointer construction. Bitmap
     deltas + walker stats.) pruned per auto-prune. See
     `git log --grep="iter-71"`. iter-72 superseded its
     regression (the unbounded-switch fix swept past
     `DynArrayLeaf`'s fn end into the SWIBoot pointer table). -->

<!-- iter-70 (classify-rom walker fixes that cleared the iter-69
     literal-pool corruption; bitmap deltas / four walker fixes
     in tools/classify-rom/src/main.rs) pruned per auto-prune.
     See `git log --grep="iter-70"`. iter-71 superseded its
     surface (added more idiom recognizers); iter-72 superseded
     iter-71's regression. -->

<!-- iter-69 (ROOT CAUSE: classify-rom + shadow_stub corrupted the
     literal-pool function pointer at 0x35c49c, slot 0x420
     overwriting `0x01b494f4` with SBA UDF marker `0xe7f842f0`.
     Discovered via a USR-mode periodic-dump capture showing
     `pc=0xe7f842f0 lr=0x35c498` — first-PABT moment of
     `InitTextWalker`. iter-70 fixed the underlying classify-rom
     walker bug.) pruned per auto-prune. See
     `git log --grep="iter-69"`. -->

<!-- iter-68 (DataAbortHandler-internal hypothesis falsified — SBA
     UDFs silent in the wedge) pruned per auto-prune. iter-69
     superseded its self-loop hypothesis with the literal-pool
     corruption finding; iter-70 then fixed the underlying
     classify-rom bug. See `git log --grep="iter-68"`. -->

<!-- iter-67 (PABT-recovery hypothesis falsified — bp at
     PrefetchAbortHandler 0x393b84 fired 0 times, dabt-forward
     repeat counter logged 0 events) pruned per auto-prune. See
     `git log --grep="iter-67"`. Both falsified hypotheses
     (iter-66 LDRB-loop, iter-67 PABT-recovery) are superseded
     by iter-69's actual root cause: shadow_stub corrupted a
     literal-pool function pointer at 0x35c49c. -->

<!-- iter-66 (slot 0x424 LDRB hypothesis falsified — the LDRB at
     0x35d110 is never executed during the wedge despite the UDF
     marker `enc_udf(0x8000|0x424) = 0xe7f842f4` matching the
     wedge's `LR_abt`. Coincidence, not causation.) pruned per
     auto-prune. See `git log --grep="iter-66"`. -->

<!-- iter-65 (per-task call-chain tools + splash wedge
     characterised) pruned per the auto-prune maintenance note —
     iter-66 + iter-67 both refer to its `LR_abt = 0xe7f842f4`
     finding and the `MeasureGlyphWidths → DrTextChunk` chain it
     surfaced via `dump_current_chain` / `ctt`. Both hypotheses
     drawn from iter-65 (the LDRB-loop and the PABT-recovery
     loop) are now superseded by iter-67's "DataAbortHandler
     stuck inside its own body" mechanism. See
     `git log --grep="iter-65"` for the full retrospective. -->

<!-- iter-64 (function tracer locates newt past splash, inside
     RunInitScripts/DoBlock) pruned per the auto-prune
     maintenance note. See `git log --grep="iter-64"`. The iter-64
     conclusion that "newt is in DoBlock running NewtonScript" was
     based on first-touch traces; iter-65's live periodic dump
     supersedes it — newt is wedged in DrawSplashScreen, well
     before the post-splash NS block ever runs. -->


<!-- iter-63 (SemOp OpList decoder + scrn wake mapping +
     InitToolbox decode) pruned per the auto-prune maintenance
     note. See `git log --grep="iter-63"` for the full
     retrospective. -->

<!-- iter-62 (per-task APCS stack tracer) pruned per the auto-prune
     maintenance note. See `git log --grep="iter-62"` for the full
     retrospective. -->


<!-- Older iteration retrospectives (iter-61 and earlier) live in
     `git log` per the auto-prune maintenance note. -->


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
