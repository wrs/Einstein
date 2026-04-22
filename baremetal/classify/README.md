# Endianness-patch classifier artifacts

Per-ROM-hash bitmaps of every ARM instruction in `newton.rom` + `Einstein.rex`
that needs the shadow-stub endianness fix-up (LDRB / STRB / LDRH / STRH /
LDRSB / LDRSH / SWPB). Eventual intent: drive a pre-launch patching pass
that pre-computes every stub, eliminating `shadow_stub::patch_code_range`'s
runtime linear scan from the hypervisor entirely.

## Layout

```
<hash>/                       # FNV-1a-32 of raw on-disk rom || rex bytes
├── byte-access.bitmap        # oracle — JIT execute-time record, from NewtonProbe
├── byte-access-static.bitmap # static — classify-rom walker output (authoritative)
└── summary.txt               # counts, walker stats, invariant status
```

Both bitmaps: 524 288 bytes = 1 bit per 32-bit word across 16 MiB of guest
ROM space (PA 0x00000000..0x01000000). Bit index `= addr / 4`; byte index
`= bit / 8`; within-byte position `= bit % 8`, LSB-first.

## Producing the data

```
# Oracle — instrument the JIT, boot for 90 s, dump byte-access.bitmap
cmake --build build --target NewtonProbe -j 8
build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 90

# Static — walk every reachable word, decode byte-access, dump byte-access-static.bitmap
(cd baremetal/tools/classify-rom && cargo build --release)
baremetal/tools/classify-rom/target/aarch64-apple-darwin/release/classify-rom \
  --rom baremetal/roms/newton.rom --rex _Data_/Einstein.rex \
  --symbols _Data_/demangled_symbols.txt --out baremetal/classify
```

classify-rom checks `oracle ⊆ static` as a hard post-condition. If an oracle
bit is missing from static, it exits non-zero and lists the offending PCs —
that means either the walker lost reachability to a real call site or the
JIT hook records something `is_byte_access` doesn't (drift from
`shadow_stub::decode`).

## Oracle source

`baremetal/probe/probe.cpp` + `baremetal/probe/probe_sink.h::probe_record_ba_site`,
called from three JIT unit templates at execute time (not translate time —
translation happens per-page regardless of reach and would fire on
literal-pool words that look like byte accesses):

- `Emulator/JIT/Generic/TJITGeneric_SingleDataTransfer_template.h` — `#if FLAG_B`
- `Emulator/JIT/Generic/TJITGeneric_HalfwordAndSignedDataTransfer_template.h` — everything except LDRD/STRD
- `Emulator/JIT/Generic/TJITGeneric_SingleDataSwap_template.h` — `#if FLAG_B && (Rd != Rm)`

The carve-outs (LDRD/STRD, SWPB Rt==Rm) mirror the refusal set of
`baremetal/src/shadow_stub.rs::decode`, which is what the patcher will
actually consult. Identical acceptance sets = invariant preservable.

## Static source

`baremetal/tools/classify-rom/src/main.rs` — standalone host Rust crate.
Walks every reachable word, runs a line-for-line port of
`shadow_stub::decode` (as `is_byte_access`), sets bits for accepted words.

Reachability is built in three passes, re-running to a fixed point:

1. Direct recursive-descent from every non-linker-marker symbol in
   `demangled_symbols.txt` + the 8 exception vectors. Follows B/BL/Bcc and
   fall-through. Terminates at unconditional B/BX, PC-writing DP/LDR, LDM
   with PC, SWI, UDF.
2. Indirect-target recovery: every word-aligned value that points at a
   prologue-shaped target (`tracer.rs`'s allowlist) is seeded as a root.
   Catches vtables, dispatch tables, callback arrays.
3. Prologue sweep: any unreached word whose content is itself a
   prologue-shaped instruction is seeded. Covers REX code (no symbol file
   covers our `Einstein.rex`) and cold code reachable only via indirect
   calls the walker can't follow.

Newton-specific control-flow idioms the walker understands:

- `MOV LR, PC` (or `ADD LR, PC, #0`) immediately before a PC-write is a
  hand-rolled `BL` — the fall-through is live.
- `<cond> <dp> pc, pc, <reg>, LSL #2` is a jump-table dispatch. The walker
  then treats the run of unconditional B entries that follows as table
  slots (push target, keep walking), and treats LDM-with-pc inside that
  run as a default-case return (not a terminator).

## Output directory

Regeneratable, `.gitignore`d. The checked-in files are this README, the
`.gitignore`, and nothing else.
