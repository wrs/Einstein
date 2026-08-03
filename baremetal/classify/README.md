# Code/data classifier artifacts

Per-ROM-hash bitmap partitioning every 32-bit word of `newton.rom` +
`Einstein.rex` into code and data. The hypervisor stages it into its image and
uses it to pick each ROM word's storage layout at load: code words
byte-reversed for the always-LE instruction fetcher, data words verbatim for
the BE-8 guest. See [`../docs/ENDIAN_FIXES.md`](../docs/ENDIAN_FIXES.md) for why
that is the whole endianness strategy, and `src/hv/guest_mem.rs` for the
`include_bytes!` site.

Misclassification is silent and ugly: a data word marked as code gets stored
reversed, so every guest read of it returns mojibake; a code word marked as
data won't decode. The bitmap-first triage recipe in
[`../docs/DEBUGGING.md`](../docs/DEBUGGING.md) is the standing response to a
wedge whose PC lands in ROM.

## Layout

```
<hash>/                # FNV-1a-32 of raw on-disk rom || rex bytes
├── reach.bitmap       # the partition — 1 bit per word, set = code
└── summary.txt        # walker stats: roots, seeders, popcount
```

`reach.bitmap` is 524 288 bytes = 1 bit per 32-bit word across 16 MiB of guest
ROM space (PA 0x00000000..0x01000000). Bit index `= addr / 4`; byte index
`= bit / 8`; within-byte position `= bit % 8`, LSB-first.

The `code-regions.txt` / `data-regions.txt` dumps that may also appear here are
human-readable renderings produced by `scripts/dump-data-regions.py`.

## Regenerating

```
scripts/regen-classify.sh 717006
```

Two chained passes: `scripts/classify-symbols.py` partitions the demangled
symbol table into the curated code-only root list, then `tools/classify-rom`
walks from those roots and writes the bitmap. Run it whenever the ROM, the
REx, or the symbol tables change — `build.rs` panics with the exact command
if the bitmap for the current ROM+REx hash is missing.

## How reachability is built

`tools/classify-rom` walks every reachable word in three passes, re-running to
a fixed point:

1. Direct recursive-descent from every non-linker-marker symbol in
   `demangled_symbols.txt` + the 8 exception vectors. Follows B/BL/Bcc and
   fall-through. Terminates at unconditional B/BX, PC-writing DP/LDR, LDM
   with PC, SWI, UDF.
2. Indirect-target recovery: every word-aligned value pointing at a
   prologue-shaped target (`tracer.rs`'s allowlist) is seeded as a root.
   Catches vtables, dispatch tables, callback arrays.
3. Prologue sweep: any unreached word whose content is itself a
   prologue-shaped instruction is seeded. Covers REx code (no symbol file
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

That `.gitignore` is `*` with only those two re-included, so a clean wipes
every `<hash>/` directory. `reach.bitmap` going missing is a hard `build.rs`
panic naming the fix, but the sibling `code-symbols.txt` loss only downgrades
to a `cargo:warning` and silently empties the diag symbol tables (hex-only
backtraces, inert tracer). `scripts/regen-classify.sh 717006` restores both.
