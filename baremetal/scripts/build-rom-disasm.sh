#!/usr/bin/env bash
#
# Build a symbol-annotated disassembly of the 717006 ROM (+ Einstein.rex
# loaded at 0x00800000) into `baremetal/scripts/disasm-out/rom.dis`.
#
# Output format matches `arm-none-eabi-objdump -D` with two additions:
#   - branch / call targets that resolve to a known symbol are suffixed
#     with `<symbol>`
#   - a blank line + `ADDR <symbol>:` header precedes each function's
#     first instruction, so you can locate a symbol by grep.
#
# Run once; output is gitignored. Re-run after editing symbols.txt.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
baremetal=$(cd "$here/.." && pwd)
einstein=$(cd "$baremetal/.." && pwd)

rom=$baremetal/roms/newton.rom
rex=$einstein/_Data_/Einstein.rex
syms=$einstein/_Data_/symbols.txt
outdir=$here/disasm-out
out=$outdir/rom.dis

mkdir -p "$outdir"

if ! command -v arm-none-eabi-objdump >/dev/null; then
    echo "error: arm-none-eabi-objdump not on PATH" >&2
    exit 1
fi

if [[ ! -f $rom ]]; then
    echo "error: missing $rom" >&2
    exit 1
fi

# Byteswap main ROM (stored big-endian per-word) to little-endian, and
# overlay Einstein.rex at offset 0x00800000 also byteswapped. The baremetal
# hypervisor does the same swap at load time; keeping the disasm in the
# same byte order means addresses in the disasm match what the guest
# CPU sees.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

python3 - "$rom" "$rex" "$tmp/rom.le" <<'PY'
import sys, struct
rom_path, rex_path, out_path = sys.argv[1:4]
with open(rom_path, 'rb') as f:
    rom = f.read()
# 16 MiB image: first 8 MiB swapped ROM, then pad, Einstein.rex at 0x800000.
image = bytearray(0x1000000)
# Main ROM occupies [0 .. len(rom)], swap each 4-byte word.
for i in range(0, len(rom), 4):
    image[i:i+4] = rom[i+3:i-1:-1] if i else rom[3::-1]
# Einstein.rex loads at PA 0x00800000, same word-byteswap.
try:
    with open(rex_path, 'rb') as f:
        rex = f.read()
    base = 0x00800000
    for i in range(0, len(rex), 4):
        w = rex[i:i+4]
        if len(w) < 4:
            image[base+i:base+i+len(w)] = w[::-1]
        else:
            image[base+i:base+i+4] = w[::-1]
except FileNotFoundError:
    sys.stderr.write(f"note: {rex_path} not found; disasm covers main ROM only\n")
with open(out_path, 'wb') as f:
    f.write(image)
PY

raw=$tmp/rom.raw.dis
arm-none-eabi-objdump -D -b binary -m arm -EL \
    --adjust-vma=0 \
    "$tmp/rom.le" > "$raw"

# Locate the most recent classifier output so we can annotate each
# disassembled word with its code/data classification. If no
# reach.bitmap is found the disasm still builds; the marker column
# just shows a `?` instead of `*` / space.
classify_dir="$baremetal/classify"

# Post-process: annotate branch targets, dereference literal-pool
# `@ 0xADDR` comments, inject function headers, and add per-word
# columns (code-marker + ASCII gloss) to the right of the hex data.
python3 - "$raw" "$syms" "$tmp/rom.le" "$out" "$classify_dir" <<'PY'
import re
import struct
import sys
from pathlib import Path

raw_path, syms_path, image_path, out_path, classify_dir = sys.argv[1:6]

# Load the classifier's reach.bitmap (the same file
# scripts/dump-data-regions.py reads). Pick the most-recently-written
# subdirectory so the disasm matches the current classification. If
# none exists, the marker column falls back to `?`.
def find_reach_bitmap(root: Path):
    if not root.is_dir():
        return None
    cands = [d / "reach.bitmap" for d in root.iterdir()
             if (d / "reach.bitmap").is_file()]
    if not cands:
        return None
    cands.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return cands[0]

reach_bitmap = None
reach_path = find_reach_bitmap(Path(classify_dir))
if reach_path is not None:
    reach_bitmap = reach_path.read_bytes()
    sys.stderr.write(
        f'rom.dis: reach.bitmap from {reach_path} '
        f'({len(reach_bitmap)} bytes)\n'
    )
else:
    sys.stderr.write(
        f'rom.dis: no reach.bitmap under {classify_dir}; '
        'code-marker column will show "?"\n'
    )

def reach_bit(addr: int) -> int:
    """Return 1 if word at `addr` is classified as code, 0 if data,
    -1 if no bitmap was loaded or the address is out of range."""
    if reach_bitmap is None:
        return -1
    if addr & 3 != 0:
        return -1
    word_idx = addr >> 2
    byte_idx = word_idx >> 3
    if byte_idx >= len(reach_bitmap):
        return -1
    return (reach_bitmap[byte_idx] >> (word_idx & 7)) & 1

def ascii_gloss(hex_word: str) -> str:
    """Render the four bytes of an 8-hex-digit word in BE byte order
    as printable ASCII; non-printables become '.'. Mirrors the gloss
    used by data-regions.txt / code-regions.txt."""
    out = []
    for i in range(0, 8, 2):
        b = int(hex_word[i:i+2], 16)
        out.append(chr(b) if 0x20 <= b < 0x7f else '.')
    return ''.join(out)

syms = {}
with open(syms_path) as f:
    for line in f:
        parts = line.split(None, 2)
        if len(parts) < 2:
            continue
        addr = parts[0]
        name = parts[1]
        if not addr.startswith('0x'):
            continue
        try:
            a = int(addr, 16)
        except ValueError:
            continue
        # Keep the first name we see for each address (symbols.txt
        # sometimes has duplicate entries — both _Foo and Foo aliases).
        syms.setdefault(a, name)

# 16 MiB ROM+REx image (LE word view). `read_word` reads a u32 at a
# byte address and returns None if out of range or misaligned.
with open(image_path, 'rb') as f:
    image = f.read()

def read_word(addr):
    if addr & 3 != 0:
        return None
    if addr < 0 or addr + 4 > len(image):
        return None
    return struct.unpack_from('<I', image, addr)[0]

# Operand patterns we care about:
#   * `bl 0xXXXX`, `blx 0xXXXX`, `b 0xXXXX`, `bCC 0xXXXX`
#   * `@ 0xXXXX` objdump side comment (PC-relative literal address)
# Everything else (e.g. `#0x..`, `,#0x..`) is an immediate, not an address.
target_re = re.compile(
    r'(?P<prefix>\b(?:bl|blx|b|bal|beq|bne|bcs|bhs|bcc|blo|bmi|bpl|bvs|bvc|bhi|bls|bge|blt|bgt|ble)\s+)0x([0-9a-f]+)(?P<suffix>\b)'
)
# Literal-pool reference: `<insn> [pc, #N] ... @ 0xADDR`. Match only
# PC-relative loads (`ldr*`) so we don't dereference objdump's
# immediate-value glosses (`@ 0xff` etc.) which use the same `@ 0xN`
# syntax but aren't pool addresses.
litpool_re = re.compile(
    r'(?P<head>\b(?:ldr|ldrh|ldrsh|ldrsb|ldrb|ldrd|ldfs|ldfd)\b[^\n]*?\[pc,\s*#-?\d+\][^\n]*?@\s+)0x(?P<addr>[0-9a-f]+)(?P<tail>\b)'
)
# Other `@ 0xADDR` comments — branch-target annotations on
# conditional branches etc.; we only add a `<symbol>` lookup, not a
# dereference, since the address itself is the target.
comment_re = re.compile(r'(@\s+)0x([0-9a-f]+)(\b)')

addr_line_re = re.compile(r'^\s+([0-9a-f]+):\s')

# Insert the code-marker + ASCII gloss columns after the 8-digit hex
# word that objdump emits. The raw line shape is
#   `  ADDR:\tHEX \tMNEMONIC...`
# (verified with `od -c`); we want
#   `  0xADDR:\tHEX <C> <ASCII>\tMNEMONIC...`
# where <C> is `*` for code, space for data, `?` if no bitmap.
# The `0x` prefix on the line-start address means a `b 0x392924`
# branch target is grep-findable to the matching `  0x392924:` line.
hex_col_re = re.compile(
    r'^(?P<lead>\s+)(?P<addr>[0-9a-f]+):(?P<sep>\t)'
    r'(?P<hex>[0-9a-f]{8}) (?P<rest>\t.*)$'
)
# Section/symbol headers objdump emits look like `00000000 <.data>:`.
# Rewrite the leading address to `0x...` so they're searchable too.
objdump_hdr_re = re.compile(r'^(?P<addr>[0-9a-f]+)(?P<rest>\s+<[^>]+>:)$')

with open(raw_path) as fin, open(out_path, 'w') as fout:
    for line in fin:
        m = addr_line_re.match(line)
        if m:
            addr = int(m.group(1), 16)
            if addr in syms:
                fout.write(f'\n0x{addr:08x} <{syms[addr]}>:\n')

        # Inject the code-marker + ASCII gloss columns when the line
        # is a disassembled word (objdump skips this format for the
        # "..." gap-filler lines, which we leave untouched).
        col_m = hex_col_re.match(line)
        if col_m:
            addr_v = int(col_m.group('addr'), 16)
            hex_v = col_m.group('hex')
            bit = reach_bit(addr_v)
            marker = '*' if bit == 1 else ' ' if bit == 0 else '?'
            gloss = ascii_gloss(hex_v)
            # Use a fixed 2-space lead so every instruction line
            # column-aligns: addresses are uniformly 8 hex digits
            # wide, so objdump's variable-width pad isn't needed.
            line = (
                f"  0x{addr_v:08x}:"
                f"{col_m.group('sep')}{hex_v} {marker} {gloss}"
                f"{col_m.group('rest')}\n"
            )
        else:
            # Rewrite objdump's own section/symbol headers
            # (`00000000 <.data>:`) to `0x00000000 <.data>:` so they
            # match the same `0x...` search pattern.
            hdr_m = objdump_hdr_re.match(line.rstrip('\n'))
            if hdr_m:
                hdr_addr = int(hdr_m.group('addr'), 16)
                line = f"0x{hdr_addr:08x}{hdr_m.group('rest')}\n"

        # Always zero-pad the rewritten address to 8 hex digits so a
        # single search pattern (`0x00018688`) finds both branch refs
        # and the matching `0x00018688:` line label / `<sym>` header.
        def sub_target(mm):
            a = int(mm.group(2), 16)
            name = syms.get(a)
            tail = f' <{name}>' if name is not None else ''
            return f'{mm.group("prefix")}0x{a:08x}{tail}{mm.group("suffix")}'

        # Literal-pool dereference: replace `@ 0xADDR` (under a PC-rel
        # `ldr*`) with `@ 0xADDR = 0xVALUE <symbol>` so each LDR shows
        # both the pool slot and what's in it. No `<symbol>` if the
        # value isn't a known symbol.
        def sub_litpool(mm):
            a = int(mm.group('addr'), 16)
            v = read_word(a)
            if v is None:
                return mm.group(0)
            value_sym = syms.get(v)
            tail = f' = {v:#010x}'
            if value_sym is not None:
                tail += f' <{value_sym}>'
            return f'{mm.group("head")}0x{a:08x}{tail}{mm.group("tail")}'

        def sub_comment(mm):
            a = int(mm.group(2), 16)
            name = syms.get(a)
            tail = f' <{name}>' if name is not None else ''
            return f'{mm.group(1)}0x{a:08x}{tail}{mm.group(3)}'

        line = target_re.sub(sub_target, line)
        # Dereference literal-pool refs first; the replaced fragment no
        # longer matches `comment_re` because the `@ 0xADDR` is now
        # followed by ` = 0x...`.
        line = litpool_re.sub(sub_litpool, line)
        line = comment_re.sub(sub_comment, line)
        fout.write(line)

sys.stderr.write(f'rom.dis: wrote {out_path} with {len(syms)} symbols known\n')
PY

echo "wrote $out"
