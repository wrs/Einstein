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

# Post-process: annotate branch targets, dereference literal-pool
# `@ 0xADDR` comments, and inject function headers.
python3 - "$raw" "$syms" "$tmp/rom.le" "$out" <<'PY'
import re
import struct
import sys

raw_path, syms_path, image_path, out_path = sys.argv[1:5]

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

with open(raw_path) as fin, open(out_path, 'w') as fout:
    for line in fin:
        m = addr_line_re.match(line)
        if m:
            addr = int(m.group(1), 16)
            if addr in syms:
                fout.write(f'\n{addr:08x} <{syms[addr]}>:\n')

        def sub_target(mm):
            a = int(mm.group(2), 16)
            name = syms.get(a)
            if name is None:
                return mm.group(0)
            return f'{mm.group("prefix")}0x{mm.group(2)} <{name}>{mm.group("suffix")}'

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
            return f'{mm.group("head")}0x{mm.group("addr")}{tail}{mm.group("tail")}'

        def sub_comment(mm):
            a = int(mm.group(2), 16)
            name = syms.get(a)
            if name is None:
                return mm.group(0)
            return f'{mm.group(1)}0x{mm.group(2)} <{name}>{mm.group(3)}'

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
