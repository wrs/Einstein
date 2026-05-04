# Plan — Migrate guest data endianness to BE-8

**Status:** ready for execution in a fresh context
**Driver:** iter-89 alarm-soup string round-trip (`evt.ex.fr.store -48022` throws)
**Scope:** architectural migration; subsumes the iter-89 chase

This plan replaces the current "BE-32 word-invariant via load-time word
swap + UDF-trap byte-lane emulator" approach (`IMPLEMENTATION.md` §8.4)
with "BE-8 (CPSR.E=1) data accesses + selective code-only byteswap at
load." It is feasible now because the classifier
(`scripts/classify-symbols.py` + `tools/classify-rom`) reliably
distinguishes ROM code words from data words. At project bootstrap that
information wasn't available, which is why the original design
byteswapped every word and emulated byte/halfword accesses via UDF.

The fresh context starts from `master` (parent of `mpt c21813dd`,
`baremetal: iter-89 — Store/Load + TextDecomp probes`). Before
beginning, run `jj abandon` on the iter-89 diagnostic-probe commits
unless they're already abandoned — see "What gets dropped" below.

## Why this is a net win

The current architecture has a class of latent bugs where any byte- or
halfword-level data access whose PC is missing from the static
byte-access bitmap reads/writes raw LE bytes instead of going through
the XOR-2/XOR-3 byte-lane transform. The iter-89 alarm-soup throw is one
such bug; the symptom (UTF-16 string body in a heap-allocated `'string`
binary lands in raw LE byte order rather than BE-32 byte-lane order) is
characteristic. Each occurrence requires per-site classifier or
emulator additions. The class is open-ended.

Under BE-8 the CPU itself does the byte-lane transform on every
load/store of every width. There is no bitmap to keep in sync, no UDF
trap on the hot path, and no per-instruction patching aside from the
classifier-driven code-word swap at load. The whole class of "missed
byte-access PC" bugs disappears.

Secondary benefits:
- ~27 600 UDF traps per boot eliminated from the hot path
  (`shadow_stub` SBA paths reduced to a small MMIO-dispatch subset).
- `unaligned.rs` / `unaligned_inline.rs` simplify (BE-8's
  unaligned-LDR semantics differ from SA-1100 BE-32's ROR semantics
  but the rotate emulator is already self-contained — review and
  simplify rather than rewrite from scratch).
- Snapshot ring becomes simpler (no byte-lane fixup needed).
- Hypervisor diagnostics (`heap_check::pretty_print_ref` etc.) read
  guest data through a single `__rev`-applying helper instead of
  relying on the LDR-of-byteswapped-ROM-returns-BE-numerical-value
  coincidence.

## Constraints

- **The 36 guest tests must pass on every commit.** They live under
  `guest-tests/tests/` and are run via
  `baremetal/guest-tests/scripts/run-all.sh`. Several of them
  exercise byte/halfword access patterns and unaligned LDRs;
  they're the regression baseline.
- **Cold-boot must reach at least the same kernel-idle state as
  iter-89.** `pckm`, `idle`, `scrn`, `cdsv` etc. parked on
  `PortReceive` / sema-op group. Output from `cargo run --release`
  in the `baremetal/` directory.
- **The alarm-soup `evt.ex.fr.store(-48022)` throws should disappear.**
  This is the success oracle for the migration: in iter-89 cold boot,
  AlterIndexes Delete #40 / #44 / #46 on soup id `0x12b` throw because
  `_BTRemoveKey` can't find the entry's per-index key in the B-tree.
  Under BE-8 the underlying UTF-16 string body is byte-faithful, so
  `KeyToSKey` produces the correct SKey, and the lookup succeeds.
- **No regressions in `unaligned.rs` rotate emulation.** Newton's
  compiler emits `LDR Rn, [Rm, #imm]; ASR/LSR Rn, #16` with `imm % 4
  != 0` at 1 872 sites. SA-1100 BE-32 rotated the loaded word so the
  halfword at the requested address landed in the high half. BE-8 has
  no architectural ROR-on-unaligned; in BE-8 the result is
  implementation-defined. The emulator must continue to provide
  SA-1100 ROR semantics for these sites.

## What ARMv7-A says (quick reference)

`ARM DDI 0406C.d` §A3.3.1, p. 8168:

> In ARMv7-A, the mapping of instruction memory is always little-endian.
> […] ARMv7 does not support BE-32 operation, and bit SCTLR[7] is
> RAZ/SBZP. […] Each ARM instruction must have the byte order of each
> word of instruction reversed.

So:
- Instruction fetch: forced LE on Cortex-A53.
- Data accesses: configurable per-mode via `SCTLR.EE`/`CPSR.E`.
- Legacy BE-32 ROM image bytes need to be word-reversed at link or
  load time for instructions to decode; data words don't need
  reversal under BE-8 (the CPU byteswaps on each access).

## Target state

| component | current | target |
|---|---|---|
| ROM image at load | every word byteswapped | only **code** words byteswapped (per `byte-access-static.bitmap` complement, i.e. the reachable-code bitmap from `classify/<hash>/reach.bitmap`) |
| Guest CPSR.E | 0 (LE data) | 1 (BE-8 data) |
| `SCTLR_EL1.EE` | 0 | 1 |
| `LDR (word)` semantics | LE u32 = original BE numerical value (because of byteswap-at-load) | CPU byteswaps; result = BE numerical value at the addressed bytes |
| `LDRB / LDRH / STRB / STRH` | UDF-trapped to EL2; XOR-3/XOR-2 byte-lane transform | native, no trap; CPU's BE-8 byte/halfword access lands at the correct byte-lane position |
| `LDR Rd,[Rm]; LSR/ASR #16` (the iter-89 idiom) | works on byteswapped ROM only by accident; broken on heap when XOR-3 STRB writers are missing from bitmap | works natively |
| Unaligned `LDR` | `unaligned.rs` emulates SA-1100 ROR semantics on top of byteswapped memory | `unaligned.rs` emulates SA-1100 ROR semantics on top of natively BE memory (math simplifies but the entry path is the same — alignment fault → emulator) |
| `shadow_stub` byte-lane UDF emulator | every byte/halfword PC patched | **deleted entirely** — stage-2 abort handler in `trap.rs::handle_data_abort` already routes byte/halfword MMIO via `mmio::read(ipa, sas, …)` / `mmio::write(ipa, sas, value, …)`. The MMIO-routing branch of shadow_stub is redundant with that path; once byte-lane UDFs are gone, no shadow_stub state needs to live |
| EL2 read of guest u32 | plain `read_word_va` | `read_word_va` byteswaps before returning (so all callers continue to see BE numerical values) |

## Phasing

The migration is staged so each commit is verifiable. The atomic
"flip" is Phase 2 — that's the single-commit step where ROM layout,
CPSR.E, EL2 helpers, and `shadow_stub` mode all change together. Phase
1 is preparatory refactoring; Phases 3–5 are post-flip cleanup.

### Phase 0: pre-migration housekeeping — sweep all leftover diagnostic probes

In a fresh `jj new -m 'WIP: sweep diagnostic probes from iter-50..89'`
change. The probe sweep is broader than the iter-89 ones — many
prior iterations left their scaffolding in place. Inventory of
`*_PROBE_HVC_IMM` constants in `src/rom_patches.rs`:

| HVC range | category | disposition |
|---|---|---|
| `0x46`, `0x48–0x4E` | page-allocator / heap / stack-mgr / Resolve probes | drop |
| `0x53–0x5D` | page-get / Prim Remember/Forget / IdleProc / NW entry/return / DL / LockHeapRange / ExtendVMHeap / NewBlock | drop |
| `0x5E–0x68` | TUnicodeCompressor introspection (`WRITE_RUN`, `WRITE_CHUNK`, `COMP_NEW`, `COMP_RESET`, `WC_LOAD`, `WC_STORE`, `WC_RELOAD`, `WC_ADD`, `WC_POSTLOAD`, `WC_POSTLDRB`, `WC_BNE`) | drop |
| `0x6B–0x6F` | CardFault throw, Lookup entry, FindSuper entry/mid, Throw entry | drop |
| `0x70–0x74` | PhysBlock / wrapper@c0cac / LogOffset@c2418 / Lookup table-base / Lookup table-idx | drop |
| `0x75–0x7E` | ThrowRefException / ThrowExIntrp / DoSend entry / Print / REP-stack-trace / REP-ex-notify / putc / flush / stack-trace / ex-notify | drop |
| `0x7F` | ResolveMagicPtr | drop |
| `0x80` | FPE entry | **keep** — load-bearing FP-bypass plumbing, not diagnostic |
| `0x81–0x86` | soup-index Add/Delete return, B-tree Insert/Delete pre, UpdateNode pre-replace, ReadNode post-read | drop (iter-89 chase) |
| `0x87–0x91` | AlterIndexes entry/throw, EntryRemove, KeyToSKey entry/done, AlterIndexes-entry, LoadPerm/StorePerm entry/exit, TextDecomp entry/exit | drop (iter-89 chase) |

Net: every probe HVC except `0x80` (FPE) goes. The operational HVC
immediates (`UND_TAG`, `DIAG_TAG`, `ALIGN_TAG`, `SBA_RETRY_TAG`,
`GPIO_TRIGGER_TAG`, tracer immediates) stay — they're how the
hypervisor actually works, not iteration scaffolding.

Touch:

- `src/rom_patches.rs`:
  - Delete every `*_PROBE_HVC_IMM` constant in the table above and
    its companion `*_PROBE_PC` / `*_FIRST_INSN` constants.
  - Delete the `patch_probe(...)` calls inside
    `apply_717006_patches` for each. The remaining `patch_probe`
    calls should be only those for the operational
    instrumentation Walter still wants live.
- `src/trap.rs`:
  - Delete the matching arms in the main HVC dispatcher (`v if v
    == crate::rom_patches::*_PROBE_HVC_IMM => …`).
  - Delete the matching arms in the UND-trampoline secondary
    dispatcher (`_ if insn ==
    rom_patches_hvc_insn(crate::rom_patches::*_PROBE_HVC_IMM) =>
    …`).
  - Delete the `handle_*_probe[_with]` handler functions.
  - Delete supporting state: ring buffers (`TextDecompCall` and
    similar), `text_decomp_save` / `text_decomp_take`,
    `nib`, any helpers used only by deleted handlers.
- Delete now-orphan helper modules / constants if any (e.g.
  `src/dosend_ring.rs`, `src/g1_capture.rs`, `src/alrt_capture.rs`,
  `src/rep_print.rs`, `src/heap_watch.rs` — audit each for
  whether anything still references them; most are probe-only).
- Keep `src/heap_check.rs` (general-purpose Ref pretty-printer),
  `src/tracer.rs`, `src/snapshot.rs`, `src/guest_bp.rs`,
  `src/task_dump.rs`. (`heap_check::read_object_bytes` will get a
  one-line update in Phase 4.)
- Verify: `cargo build --release` clean; 36/36 guest tests pass.
  Cold boot still reaches the iter-89 idle state. Commit.

Estimated diff: 3 000–5 000 lines deleted (~80 % of `trap.rs`'s
probe-handler code goes). No semantic change.

### Phase 1: introduce byte-order-aware accessor helpers

Goal: route every place that reads or writes guest data through a
single bottleneck, so the Phase 2 flip is a one-line change in a few
helpers rather than a survey of the whole hypervisor.

In a fresh `WIP:` change, add a new module `src/guest_endian.rs` (or
extend `guest_mem.rs`) with the API:

```rust
/// Read a 32-bit word from guest memory and return it as a Newton-side
/// numerical value (i.e. BE-interpreted regardless of how the bytes are
/// laid out in host memory).
pub fn guest_read_u32_va(va: u32) -> Option<u32>;
pub fn guest_read_u32_pa(pa: u32) -> Option<u32>;

/// Write a 32-bit Newton-side numerical value to guest memory.
pub fn guest_write_u32_va(va: u32, value: u32) -> bool;
pub fn guest_write_u32_pa(pa: u32, value: u32) -> bool;

/// Read a single byte at the given guest VA, treating the address as
/// a Newton-side logical byte address (i.e. byte 0 of a u32 is the
/// most-significant byte). Used by diagnostic byte-walkers.
pub fn guest_read_u8_va(va: u32) -> Option<u8>;
pub fn guest_read_u16_va(va: u32) -> Option<u16>;

/// Read a contiguous range of guest bytes in Newton-side logical-byte
/// order (so the buffer ends up matching what the BE-32 source code
/// would see). Replaces the ad-hoc `read_object_bytes` in heap_check.
pub fn guest_read_bytes_va(va: u32, out: &mut [u8]) -> Option<usize>;
```

Initial implementation (Phase 1, no semantic change):
- `guest_read_u32_va` calls the existing
  `guest_mem::read_word_va` and returns the result unchanged. (In
  the current design, that already returns the BE numerical value
  because the ROM is word-swapped at load and heap RAM is XOR-3
  patched.)
- `guest_read_u8_va(va)` reads the byte at `va ^ 3` in host LE (the
  current XOR-3 byte-lane transform).
- `guest_read_u16_va(va)` reads the halfword at `va ^ 2` in host LE
  (the current XOR-2 transform).
- `guest_read_bytes_va` walks `va, va+1, va+2, …` calling
  `guest_read_u8_va` for each.

These are intentionally identical in behavior to the current ad-hoc
helpers. Phase 1 is purely a routing refactor.

Then migrate every guest-data read/write to the new API. Use
`grep -n` for the current accessors and rewrite mechanically:

| current call | new call |
|---|---|
| `guest_mem::read_word_va(va)` | `guest_endian::guest_read_u32_va(va)` |
| `guest_mem::read_word_pa(pa)` | `guest_endian::guest_read_u32_pa(pa)` |
| `guest_mem::write_word_va(va, val)` | `guest_endian::guest_write_u32_va(va, val)` |
| ad-hoc XOR-3 byte read | `guest_endian::guest_read_u8_va(va)` |
| `read_object_bytes` (heap_check) | `guest_endian::guest_read_bytes_va` |

Files affected:
- `src/heap_check.rs` (printer, `read_object_bytes`).
- `src/trap.rs` (every probe handler that reads/writes guest state).
- `src/peripherals/*.rs` (peripheral managers reading guest descriptors).
- `src/banked.rs` (banked-register helpers).
- `src/task_dump.rs` (TScheduler/TTask walker).
- `src/snapshot.rs` (snapshot save/restore).
- `src/shadow_stub.rs` (effective-address translation).
- `src/unaligned.rs` (unaligned-LDR emulator).
- `src/guest_bp.rs` (software breakpoint patcher).

The audit is done by:

```bash
grep -rn 'read_word_va\|read_word_pa\|write_word_va\|write_word_pa' src/
grep -rn 'translate_va.*read\|translate_pa.*read' src/
```

Verify after each batch: `cargo build --release && guest-tests/scripts/run-all.sh`.

Estimated work: ~150 call sites, mechanical. One commit at the end of
the audit.

### Phase 2: atomic flip to BE-8

This is the load-bearing commit. It changes ROM layout, guest CPSR.E,
and the helper implementations all together. Until this commit lands
the system must build but is never run with mismatched halves; the
`jj` working copy stays at this commit until verified.

Scope:

#### 2a. Selective ROM byteswap

`src/guest_mem.rs::load_rom` (and the equivalent for REx loading):
- Read the classifier reach bitmap from
  `classify/<hash>/reach.bitmap` (already loaded into the binary
  via `build.rs::include_bytes!`).
- For each 4-byte aligned address in the ROM aperture
  (`0x00000000..0x01000000`), look up the bit in `reach.bitmap`:
  - bit set → word is reachable code → byteswap on load (write
    `word.to_be().to_le_bytes()` — i.e., reverse the on-disk
    byte order so an LE-mode CPU fetches the original BE
    instruction encoding correctly).
  - bit clear → word is data (or unreachable padding) → write
    on-disk bytes verbatim.
- Same logic for `_Data_/Einstein.rex` at `0x00800000..0x01000000`,
  using the REx-side bits in the same bitmap.

Edge cases:
- Code/data adjacency: aligned 4-byte words, no half-words. The
  classifier's reach bitmap is per-word, so no straddle issues.
- Vtables and pointer literals embedded in code regions: classifier
  marks them as data (they're walked into via `fnptr literal
  roots` during reach analysis but are not reached as code). Verify
  in `summary.txt`: `vtables found`, `fnptr literal roots added`.
- `apply_717006_patches` (and friends in `rom_patches.rs`) write
  ARM instructions into the ROM image after load. They write
  patched code into code regions, so they need to byteswap before
  storing. Add a helper `write_rom_code_word(rom_ptr, pc, insn)`
  that does the swap, and route all the existing `rom_ptr.add(idx)
  .write(insn)` calls in `rom_patches.rs` through it.
- The HVC trampolines, FPA bypass stub, and resolve-fault wrapper
  are all code → use the new helper.
- The site metadata table the patch helpers maintain (e.g.
  `ALTER_INDEXES_ENTRY_FIRST_INSN`'s "expected first insn" check)
  needs updating: those constants describe the on-disk encoding,
  but `load_rom` now stores them byteswapped. Either swap the
  constant before comparison, or read the byteswapped form. The
  cleanest fix is to keep the constants as on-disk encodings and
  byteswap on read inside `patch_probe`.

#### 2b. Set guest CPSR.E=1 and SCTLR_EL1.EE=1

`src/guest.rs::eret_to_guest`:
- Change `spsr_aarch32_svc` from `0x0000_01D3` to `0x0000_03D3`
  (set bit 9 = E).
- Add `msr sctlr_el1, {sctlr}` with `SCTLR_EL1.EE` (bit 25) and
  `SCTLR_EL1.E0E` (bit 24) set, before the `eret`. Currently
  `zero_el1_guest_state` writes `xzr` to `sctlr_el1` (line 160);
  replace with a value that has EE/E0E bits set.

Verify via gdb: at the first guest instruction, `info reg cpsr`
should show E flag set; `mrs Rd, sctlr` (after the first ROM
write, intercepted by our shim) should reflect EE=1.

`src/snapshot.rs::resume_from_snapshot` (and the equivalent
`eret_to_guest_resumed`): do the same for SPSR construction. The
saved SPSR at snapshot time may not have E=1 if the snapshot was
taken under the old policy — clear the snapshot ring before the
first BE-8 boot (`rm -f /tmp/newton-snapshot-*.bin`).

#### 2c. Reverse the byte-order policy in the helpers

In `src/guest_endian.rs`:
- `guest_read_u32_va(va)`: reads the LE u32 from host memory (still
  via `guest_mem::read_word_va`), then `__rev`s it before returning.
  Under BE-8 the guest stored the value with bytes in BE order; an
  LE-mode read of those bytes returns the byteswapped form, so we
  unswap.
- `guest_write_u32_va(va, value)`: `__rev` the value before storing
  via `guest_mem::write_word_va`.
- `guest_read_u8_va(va)`: reads the byte at the literal `va` (no
  XOR-3 transform — under BE-8 the byte at logical address `va`
  lives at host address `va`, since the CPU does the byte-lane
  transform on every guest store).
- `guest_read_u16_va(va)`: reads the halfword at the literal `va`,
  then `__rev16`s the low halfword.
- `guest_read_bytes_va(va, out)`: copies bytes verbatim from host
  memory at `va` (no transform). The bytes already represent the
  Newton-side logical byte order because the guest's STRB/STRH
  stored them there directly under CPSR.E=1.

The `__rev` and `__rev16` ops are emitted by Rust's `u32::swap_bytes`
and `(value as u16).swap_bytes()` respectively.

#### 2d. Delete shadow_stub entirely

The stage-2 abort handler (`trap.rs::handle_data_abort`, line 507)
already extracts `sas` (Size of Access) from `ESR_EL2.ISS` and
dispatches byte/halfword/word MMIO accesses via `mmio::read(ipa,
sas, …)` / `mmio::write(ipa, sas, value, …)`. Under BE-8, RAM
byte/halfword accesses are handled by the CPU natively, and MMIO
byte/halfword accesses fall through to stage-2 abort the same way
word accesses already do. There is no remaining role for
shadow_stub.

- Delete `src/shadow_stub.rs` and its entry from `src/main.rs` /
  `mod.rs`.
- Delete the `SBA_UDF_BASE` constant and `handle_sba_udf` arm in
  `trap.rs`.
- Delete the SBA UDF site table allocator and its `RESERVED_SCRATCH_SLOTS`
  reservation in `shadow_pool.rs`.
- Delete the `unxor_sub_word` helper in `mmio.rs` (which existed
  only to undo shadow_stub's XOR-3 transform on the way to MMIO
  dispatch). Stage-2 abort always passes the natural IPA, so no
  un-XOR is needed.
- Delete the `SCRATCH_POOL` IPA region in `stage2.rs` and the
  ScratchVA-stub install machinery — they were shadow_stub's
  per-stub literal area.
- Delete `byte-access-static.bitmap` and `byte-access.bitmap`
  generation from `tools/classify-rom`'s output (keep the walker
  + `reach.bitmap`, which is needed for Phase 2a).
- Delete the `SBA_RETRY_TAG` HVC handling.
- Drop the `SBA_POST_TRAMP_OFFSET` post-emulation trampoline at
  `0x00FFFF80` — only used for the shadow_stub R13/R14-writeback
  return dispatch.

Estimated removal: ~3 500 lines across `shadow_stub.rs` (~2 800),
`trap.rs` SBA dispatch, `shadow_pool.rs`, and `tools/classify-rom`.

#### 2e. Verify peripheral byte/halfword access handling

Today's peripheral `read` / `write` signatures take only `(ipa, value)`
and assume word-sized register access. `mmio::write(ipa, sas,
value, elr)` passes the full word `value` to the peripheral's
`write(ipa, value)` regardless of `sas`. Under the current
architecture this is fine because shadow_stub catches every
byte/halfword access before stage-2; under BE-8 these accesses fall
through to stage-2 and reach the peripheral verbatim.

For *reads*: `mmio::read` already masks the returned word via
`mask_for_size(value, sas)`, which is correct (the peripheral
returns the full register, the mask isolates the addressed lane).
No change needed.

For *writes*: a byte or halfword write should preserve the other
bytes/halfword of the register. Today `mmio::write` would clobber
them. Fix in one place rather than per-peripheral:

```rust
pub fn write(ipa: u64, sas: u8, value: u32, elr: u64) {
    let ipa = ipa & !0x3;
    let value = match sas {
        2 => value,                                          // word
        1 => splice_halfword(read(ipa, 2, elr), ipa, value), // halfword
        0 => splice_byte    (read(ipa, 2, elr), ipa, value), // byte
        _ => return halt_on_unknown(...),
    };
    // …existing word-dispatch to peripheral::write(ipa_aligned, value)…
}
```

`splice_byte` / `splice_halfword` use BE-8 byte-lane positions
(byte 0 of an aligned word is the MSB) so a byte write to address
`X` lands at bits 31:24 if `X & 3 == 0`, bits 23:16 if `X & 3 ==
1`, etc.

Audit each peripheral to confirm none of them have side effects
keyed on access width:

- `src/peripherals/vic.rs` (VIC) — register-mapped, no width side effects.
- `src/peripherals/dma.rs` (DMA) — register-mapped, no width side effects.
- `src/peripherals/serial.rs` (SerialChip / mini-UART) — register-mapped, no width side effects.
- `src/peripherals/pcmcia.rs` (PCMCIA) — register-mapped, no width side effects.
- `src/peripherals/screen.rs`, `sound.rs`, `flash*.rs`, etc. — same.

If any peripheral *does* care about access width (e.g. a FIFO that
pops a different number of bytes), it grows a `(sas)` parameter and
mmio::read/write pass it through. Newton hardware (per
`Emulator/TMemoryConsts.h`) doesn't appear to have any such
register; spot-check each peripheral file to confirm.

Add a guest test that does byte and halfword reads/writes against
a known register (e.g. the VIC's IRQ-mask) and verifies the
expected lane. Place under `guest-tests/tests/be8_mmio_lanes.rs`
or similar. This is the regression baseline for the peripheral
layer.

#### 2f. Adapt unaligned-LDR emulator

#### 2f. Adapt unaligned-LDR emulator

`src/unaligned.rs`:
- The emulator catches alignment faults on guest LDR/LDRH/LDRSH and
  emulates SA-1100 ROR-on-unaligned semantics.
- Under BE-32 word-invariant: emulator reads aligned bytes from
  byteswapped memory, computes `LE-u32 → ROR(saved_value, 8 *
  (addr & 3))`, returns the high halfword.
- Under BE-8: the underlying memory is now in natural BE byte
  order. The CPU on aligned LDR with CPSR.E=1 returns the BE
  numerical value directly. For unaligned LDR the CPU's behavior
  is implementation-defined per ARMv7, so the alignment fault
  still fires (with `SCTLR_EL1.A=1`) and we still emulate.
- The emulation math: read 4 bytes starting at `addr & ~3` from
  host memory (these are now BE bytes, no further swap needed),
  assemble as a BE u32 manually (`(b0 << 24) | (b1 << 16) | (b2
  << 8) | b3`), then ROR by `8 * (addr & 3)` and return.
- `src/unaligned_inline.rs`: the lazy in-ROM stub installer
  rewrites first-faulting unaligned LDRs with a native AArch32
  ROR-based emulation stub. The stub's instructions need to
  produce SA-1100 ROR semantics under BE-8 — which is *easier*
  than the current case because the underlying bytes are
  already BE. The stub becomes essentially `LDR + ROR Rn, Rn,
  #(8*(addr & 3))`, no extra `REV` needed. Audit each existing
  stub template; some can be deleted.

After Phase 2 build:
- 36/36 guest tests pass.
- Cold boot from clean snapshot ring reaches kernel-idle state.
- `evt.ex.fr.store(-48022)` throws on AlterIndexes Delete #40 / #44 /
  #46 are absent from the boot log. (Success oracle.)

If any of these fail, the commit stays as a WIP and we iterate within
it. The previous (Phase 1) commit is the rollback target.

### Phase 3: cleanup — documentation & residual dead code

(`shadow_stub.rs` deletion is in Phase 2d — it isn't a Phase 3
gate.)

- Delete unused `unaligned_inline.rs` stub templates that are no
  longer needed under BE-8.
- Audit `src/main.rs` and `src/peripherals/mod.rs` for any
  references to deleted modules.
- Update `IMPLEMENTATION.md` §8.4 and §8.5 — they describe the
  BE-32 word-invariant approach which no longer applies. Replace
  with a description of the BE-8 architecture; keep a
  one-paragraph "previously" note for archaeology.
- Update `HIGHLEVEL.md` §5.3 / §6 if they make any byte-order
  claims.
- `PLAN.md`: add iter-90 retrospective summarising the migration
  outcome.

### Phase 4: cleanup — diagnostics simplification

- `src/heap_check.rs::read_object_bytes`: under BE-32 word-invariant
  it read u32 LE then `to_be_bytes()`d to recover on-disk byte
  order. Under BE-8 the host bytes already are on-disk byte order,
  so just `copy_from_slice` directly.
- Drop the now-redundant `data hex` byte-reversal note in the
  heap_check comments.
- Drop the per-platform unaligned-inline stub-install summary
  lines from boot output if they're now uninteresting.

### Phase 5: validation matrix

| test | command | expected |
|---|---|---|
| guest tests on QEMU | `baremetal/guest-tests/scripts/run-all.sh` | 36/36 pass (37/37 once `be8_mmio_lanes.rs` is added) |
| guest tests on FVP | `baremetal/guest-tests/scripts/run-all.sh --platform fvp` | 36/36 (37/37) pass |
| peripheral byte/halfword writes | new `be8_mmio_lanes.rs` guest test | byte writes preserve other lanes; halfword writes preserve other halfword |
| cold boot reaches idle | `rm -f /tmp/newton-snapshot-*.bin && cargo run --release` | reaches `pckm BLK on PortReceive` etc. |
| alarm-soup throws absent | grep boot log for `evt.ex.fr.store` | zero hits during the package-install phase |
| function tracer | `--features trace,quiet` | trace lines for the soup bringup sequence look identical to iter-89's traces (modulo the throws being gone) |
| snapshot resume | take a snapshot mid-boot, restart, verify resume | resumes correctly |

## Files modified summary

| file | role | phase |
|---|---|---|
| `src/guest_endian.rs` (new) | central guest-data accessor | 1 |
| `src/guest_mem.rs` | ROM/REx loader | 2a |
| `src/guest.rs` | guest entry SPSR / SCTLR_EL1 | 2b |
| `src/snapshot.rs` | resume SPSR | 2b |
| `src/rom_patches.rs` | byteswap-on-write helpers; iter-89 probe removal | 0, 2a |
| `src/trap.rs` | iter-89 dispatch removal; SBA UDF dispatch trim | 0, 2d |
| `src/shadow_stub.rs` | **deleted entirely** | 2d |
| `src/shadow_pool.rs` | scratch-pool reservation, deleted with shadow_stub | 2d |
| `src/mmio.rs` | byte/halfword splice on write; drop `unxor_sub_word` | 2d, 2e |
| `src/peripherals/*.rs` | audit for width-side-effects (none expected) | 2e |
| `tools/classify-rom` | drop byte-access bitmap output (keep reach.bitmap) | 2d |
| `src/unaligned.rs` | unaligned-LDR emulator math | 2f |
| `src/unaligned_inline.rs` | inline stub templates | 2f, 3 |
| `guest-tests/tests/be8_mmio_lanes.rs` (new) | byte/halfword MMIO regression test | 2e |
| `src/heap_check.rs` | `read_object_bytes` simplification | 4 |
| `src/peripherals/*.rs` | guest-data reads via new helpers | 1 |
| `src/banked.rs` | banked-register reads via new helpers | 1 |
| `src/task_dump.rs` | structure walks via new helpers | 1 |
| `src/guest_bp.rs` | software-bp patcher via new helpers | 1 |
| `IMPLEMENTATION.md` | §8.4 / §8.5 description | 3 |
| `PLAN.md` | iter-89 retrospective + close out | 3 |

## What gets dropped (not migrated)

Per Walter's authorization on 2026-05-03, drop the following diagnostic
probes rather than route them through the new `guest_endian` helpers:

- iter-89 round-trip probes: Store/Load PermObject entry/exit, TextDecomp
  entry/exit, AlterIndexes entry/throw, KeyToSKey entry/done,
  EntryRemoveFromSoup entry. (HVC immediates 0x87–0x91.)
- TUnicodeCompressor introspection probes from earlier iterations:
  WriteRun, WriteChunk entry, comp/reset, WC-load/store/reload/add.
  (HVC immediates 0x5E–0x65.)

These were chasing the alarm-soup throw and the heap-junk wedge. The
migration eliminates the underlying bug class, so the probes are no
longer load-bearing. If a regression appears, regenerate the relevant
probe in the new architecture rather than restoring the old code.

The following diagnostic infrastructure is **kept** (still useful):
- `src/heap_check.rs` Ref pretty-printer + symbol-name resolver.
- `src/tracer.rs` function tracer.
- `src/snapshot.rs` rolling snapshot ring.
- `src/guest_bp.rs` software breakpoint mechanism.
- `src/task_dump.rs` TScheduler/TTask walkers.
- The SBA UDF dispatch in `shadow_stub` — but trimmed to MMIO sites
  only.

## Risks & mitigations

| risk | mitigation |
|---|---|
| Reaching Phase 2 means a single commit changes 4 things at once; if any breaks, root-cause is across all 4 | Build the new helpers in Phase 1 with identity behavior, so Phase 2 can flip them one at a time within the same commit if needed; use `git diff`/`jj diff` to bisect |
| Hidden guest-data read sites that weren't migrated to helpers in Phase 1 will silently produce byteswapped values after the flip | Phase 1 audit covers all `read_word_va` / `read_word_pa` callers via grep; instrument an EL2 panic in `guest_mem::read_word_va` after Phase 1 to ensure no direct callers remain |
| Selective ROM byteswap mis-classifies a code word as data (or vice versa) | The classifier's `oracle ⊆ static` invariant catches false negatives at boot. False positives (data marked as code) would byteswap pointer literals, breaking ROM-internal indirect jumps; spot-check via `disasm-out/rom.dis` after the flip |
| Unaligned-LDR emulator math wrong under BE-8 | Cover with a focused guest test if not already; Newton's `KeyField` extraction at `0x002E_9DE8` (UpdateNode) is a known unaligned-LDR site to instrument |
| `apply_717006_patches` writes raw insns expecting LE storage | Audit every `rom_ptr.add(idx).write(...)` in `rom_patches.rs`; route code-word writes through the new `write_rom_code_word` helper |
| Snapshots from BE-32 era won't load | Clear the snapshot ring once before first BE-8 boot; document in `PLAN.md` |
| FVP target diverges from QEMU on the EE bit setup | Test on both before declaring Phase 2 done |

## Open questions

- **Does `SCTLR_EL2.EE` need to change?** EL2 is AArch64 LE; setting
  EL2-side EE doesn't affect AArch32-EL1 data accesses, but verify
  no peripheral driver expects EL2 to be in BE. It shouldn't.
- **What about the guest's own `MCR p15, 0, Rn, c1, c0, 0` writes
  to SCTLR (which the CP15 shim intercepts)?** The shim today
  passes through SCTLR writes to `SCTLR_EL1`. After Phase 2, if
  the ROM ever clears `SCTLR_EL1.EE` we'd lose BE-8 mode mid-boot.
  Inspect 717006: the kernel writes SCTLR many times but the EE
  bit value should be a kernel-static decision. If it ever clears
  it, the shim needs a mask to keep EE forced to 1.
- **Endianness of ROM patches that the kernel writes itself?** The
  Newton kernel installs ROM patches via the patch table at
  `0x01A00000..0x01C20000` — those writes happen from inside the
  guest under CPSR.E=1, so they store BE-natural bytes, which is
  exactly what subsequent fetches need. Should "just work."

## Reference: how to validate the success oracle

In iter-89 cold boot, the alarm-soup `evt.ex.fr.store(-48022)`
throws fire from `AlterIndexes + 0x204` at PC `0x0034_7DA4`. The
relevant log lines:

```
AlterIndexes #40 ENTRY: kind=Delete r0=0x0… r3=0x0000012b …
…
TSoupIndex::Delete #0 RET: r5(retcode)=0x00000002 (signed 2) …
AlterIndexes→Throw #0: kind=Delete raw_retcode=0x00000002 …
Throw #0: name="evt.ex.fr.store" (r0=0x000afeb8) r1=0xffff446a …
```

After Phase 2, the same boot point should:
- Reach AlterIndexes Delete #40 with the same `entry_ref`,
- `_BTRemoveKey` returns 0 (key found),
- No Throw fires.

If the success oracle fails but the rest of boot is healthy, that's
an actionable second-tier debugging task — most likely a hidden
guest-data read site that wasn't routed through the new helpers in
Phase 1 — not an architectural problem with BE-8 itself.

## Recovery if Phase 2 wedges hard

If Phase 2 boot wedges before producing any output:
1. `jj abandon` the Phase 2 WIP commit; you're back to Phase 1.
2. Re-enter the Phase 2 commit one sub-phase at a time:
   - 2a alone (selective ROM byteswap, but CPSR.E still 0) → guest
     immediately faults on first fetch (instructions are now in
     "BE-on-disk-byte-order, but addressable as LE u32 = LE byte
     order on host"). Expected to fail; this isolates "is the
     selective-swap correct?"
   - 2b alone (CPSR.E=1, but ROM still all-words-byteswapped) →
     guest fetches correct LE-decoded instructions but data
     accesses byteswap once-extra → guaranteed wedge, but in a
     specific predictable way (every word read returns
     byteswapped). Useful sanity check.
   - 2a + 2b together (no helper rewrite) → EL2 reads of guest
     data return byteswapped values; first peripheral access or
     CP15 trap will likely halt with garbage.
   - 2a + 2b + 2c → matches the target state; validate.
3. The bisection above isolates which sub-phase introduces the
   wedge.

## Estimated effort

- Phase 0: 30 minutes (mechanical deletion).
- Phase 1: 4–6 hours (audit and route ~150 call sites, run guest
  tests after each batch).
- Phase 2: 4–8 hours (atomic flip, debug failures, validate).
- Phase 3: 1–2 hours (cleanup of legacy code).
- Phase 4: 1 hour (diagnostic simplification).
- Total: 1–2 working days end-to-end.
