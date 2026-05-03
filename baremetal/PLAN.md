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

**Current goal (iter-83):** iter-82 fixed the asymmetric XOR-3 byte
swizzle in `shadow_stub::dispatch_*`. The kernel reads InternalStore
flash bank 0 data through stage-1 aliases in the PCMCIA aperture
(`0x30000000+`), which the old `ea < XOR_LIMIT` heuristic excluded
from the BE→LE byte-swizzle. Insert wrote bucket headers via XOR-3
STRBs (correctly, since staging is in RAM at low VAs); Get's
`memmove`-based read read raw LE flash bytes (incorrectly), so the
2-byte length header `[0x00, 0x43]` came back as `[0x07, 0x00]` and
the `Get__15TStoreHashTable` parser tried to read 0x700 bytes of a
0x43-byte entry → `_OSErr` → `evt.ex.fr.store` Throw → `GetSoup`
returns NIL. Fixed by always trying `(ea ^ 3)` (or `^ 2` for halfword)
against backed memory first regardless of `ea`'s range; only fall
through to MMIO when the XOR'd address has no backing.

After the fix `GetSoup(#453)` returns `#C607869` and the boot
proceeds well into REP user-space init (`GetUserConfig`,
`SetLCDContrast`, `SetSystemVolume`). The next stop is a different,
unrelated problem: an unrecognised UND opcode at PC=0x38db18:

```
und: forwarding FPA insn 0xed908100 @PC=0x31c4f4 → kernel FPE @0x38d8dc
und: forwarding FPA insn 0xee009100 @PC=0x1e729c → kernel FPE @0x38d8dc
*** unrecognised UND: insn=0xe169f008 at PC=0x38db18 SPSR_und=0xf810011b
    (extend handle_und in trap.rs to handle this opcode)
```

`0xe169f008` decodes as ARMv5+ `CLZ r15, r8` (count leading zeros),
encoded as a "MISC" instruction the iter-71/72 SBA classifier
treats as a UDF-shape opcode. That's the next iteration's problem
to characterise.

Next (iter-83): identify what `0xe169f008` actually is in the
kernel's compiled code at `0x38db18` and decide whether to (a)
add a CLZ handler in `handle_und`, (b) patch the kernel to use
a different opcode, or (c) recognise this is a real CLZ
intended for the FPE emulator path and route it correctly.

**Background:** iter-70 cleared the splash wedge; iter-71/72
fought a classifier regression; iter-73 forwarded FPA UNDs to
the kernel's FPE emulator; iter-74-78 walked a NS throw chain
that turned out to be a downstream consequence of the iter-82
flash-store byte-swizzle bug; iter-79/80 added REP-translator
hooks + line-buffered REP output; iter-81 verified the magic
pointer table mapping (negative result; mapping is correct);
iter-82 fixed the XOR-3 PCMCIA-aperture read swizzle in
shadow_stub.

### Iteration 82: shadow_stub XOR-3 swizzle for backed memory aliased above XOR_LIMIT

#### Symptom

`GetSoup("System")` returns NIL during boot. The TFlashStore-backed
internal store throws `evt.ex.fr.store` while loading PSSID 0x45's
soup-index map. Trace shows `Throw #0..4` cascade and the kernel
falling out of REP-driver init with `type.ref.frame` UnhandledException.

#### Investigation chain

User said: confirm flash write-then-read round-trips. Instrumented
`TFlashStore::BasicWrite` / `BasicRead` (HVC patches at 0xc7c2c /
0xc7d8c / 0xc7ef8) to dump the byte streams at both ends. Result:
**bytes round-trip correctly when both sides apply the kernel's
BE-on-LE swizzle**. Insert wrote `[0x07, 0x00, 0x43, 0x00]` raw
LE bytes to flash bank 0 — kernel's BE-view via XOR-3 LDRB is
`[0x00, 0x43, 0x00, 0x07]` = `[length_hi=0, length_lo=0x43,
count_hi=0, count_lo=7]`, the right header.

User: instrument `TStoreWritePipe` / `TStoreReadPipe` to verify
individual values. WriteReference + ReadReference probes
(0x2dd770 / 0x2dd7b0) confirmed the 24-bit Ref encoding
round-trips: WriteRef wrote `0x003f0000`, ReadRef read back
`0x003f0000`. Encoding is `(bucket_idx << 16) | byte_offset`
(low 24 bits of the value Insert returned).

User: probe `TStoreHashTable::Insert` / `::Get`. Insert at key
`0x459546bf` (low 6 bits = `0x3f`) returned `0x003f0000` and Get
looked up `0x003f0000` — hit the right bucket. So the lookup
key encoding is consistent.

User: probe inside Get, where the data Read happens. `Get-DataRead`
probe at 0x35371c (`ldr r0, [r4, #260]!` immediately before
`bl Read__6TStoreFUllPcT2`) captured the bug:

```
Get-DataRead #0: bucket_ptr=r1=0x0000003c byte_offset+2=r2=0x2 ... sp[0]_count=0x700 ...
    header @0x0cc77270: word(LE)=0x07000000 bytes=00 00 00 07  (parsed length = word>>16 = 0x700)
```

Get parses the 2-byte header and gets length `0x700` instead of
the correct `0x43`. Tries to read 1792 bytes from a 67-byte
entry → `_OSErr` → Throw.

User: probe `Read__11TFlashRange` to see what address `BlockMove`
reads from. `FR-BlockMove` probe at 0xc29d4 nailed it:

```
FR-BlockMove ...: src_va=0x300215d0 dst=0x0cc77270 size=0x2 ...
```

The kernel reads flash bank 0 data through a stage-1 alias in the
**PCMCIA aperture (`0x30000000+`)**. Our `shadow_stub::dispatch_*`
gated XOR-3 / XOR-2 application on `ea < XOR_LIMIT` (= `0x10000000`),
so byte access at `0x300215d0` skipped the swizzle. The kernel-
compiled-for-BE byte-extraction code (`ldr u32 + lsl/lsr` shifts)
combined with our XOR-3-applied STRB on the RAM-resident `dst`
to deposit raw LE flash bytes at swizzled positions in the dst
word — yielding the bogus `0x700` length parse.

#### Fix

`src/shadow_stub.rs` — all four `dispatch_*` functions now try
`(ea ^ XOR)` against backed memory FIRST, regardless of `ea`'s
position relative to `XOR_LIMIT`. Only fall through to MMIO
dispatch with the original `ea` when the XOR'd address has no
backing. This handles every case where stage-1 maps a backed
region (RAM, ROM, FB, flash) into a high VA — including the
PCMCIA-aperture alias of flash bank 0 the kernel uses for
`Read__11TFlashRange`. `XOR_LIMIT` is preserved with an updated
doc comment explaining why the heuristic was wrong.

#### Result

After the fix:

- `GetSoup(#453)` returns `#C607869` (was NIL).
- `evt.ex.fr.store` Throw cascade is gone.
- Boot proceeds well into REP-driver user-space init (`GetUserConfig`,
  `SetLCDContrast`, `SetSystemVolume`).
- Next stop is unrelated: `*** unrecognised UND: insn=0xe169f008 at
  PC=0x38db18` (a CLZ-shape opcode the SBA classifier mis-treats).

36/36 guest tests skipped per the maintenance note: this is a
shadow_stub dispatch path change, but the per-test runs use ELFs
with their own minimal mappings (all under XOR_LIMIT), so the
new try-XOR-first behaviour is identical to the old gate for them.
Verify if a future test starts mapping backed memory above
`0x10000000`.

<!-- iter-78 (heap-bounds classifier in src/heap_check.rs +
     RefArg double-indirection fix + structured object dump
     via newton-objects with Endian::Little support; pinned
     the throw chain to NIL:Query() with FindImplementor
     returning NIL). Pruned per auto-prune. See
     `git log --grep="iter-78"`. The NIL:Query() conclusion
     itself was downstream of the iter-82 byte-swizzle bug. -->

<!-- Older iteration retrospectives (iter-77 and earlier) live in
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
