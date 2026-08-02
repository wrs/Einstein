#!/usr/bin/env bash
# ROM-address containment lint (refactor phase 8). Facts about a
# specific ROM build — code addresses in the 16 MiB ROM aperture and
# kernel-VA globals — belong in `src/newton/rom_ver/`; this script
# flags hex literals with those shapes anywhere else under src/.
#
# Mechanics: for every .rs file outside src/newton/rom_ver/, strip
# line comments (which exempts doc comments), collect hex literals of
# >= 5 hex digits, and flag any whose numeric value falls in:
#
#   [0x0001_0000, 0x0100_0000)  — ROM-aperture code/data addresses
#                                 (the low bound skips small masks and
#                                 immediates);
#   [0x0C00_0000, 0x0D00_0000)  — kernel VA space (globals, remapped
#                                 RAM anchors).
#
# Every hit must be covered by scripts/rom-addr-allowlist.txt
# (`<file> <normalized-literal> # note`), which was seeded with the
# post-migration residue (instruction-encoding masks whose values
# happen to land in range, hardware-layout constants that are the
# address-map authority, and diagnostic VA lists). The allowlist may
# only shrink: uncovered hits fail, and so do stale entries that no
# longer match anything.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
cd "$root"

allowfile="$here/rom-addr-allowlist.txt"
[[ -f "$allowfile" ]] || { echo "check-rom-addrs: missing $allowfile"; exit 1; }

# Load allowlist entries.
allow_files=()
allow_lits=()
allow_used=()
while read -r file lit _rest; do
    [[ -z "${file:-}" || "$file" == \#* ]] && continue
    allow_files+=("$file")
    allow_lits+=("$lit")
    allow_used+=(0)
done < "$allowfile"

violations=0
allowed=0

while IFS= read -r f; do
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        lineno="${hit%%:*}"
        lit="${hit#*:}"
        # Normalize: lowercase, strip underscores.
        norm="0x$(tr -d '_' <<<"${lit#0x}" | tr 'A-F' 'a-f')"
        val=$((norm))
        in_rom=$(( val >= 0x10000 && val < 0x1000000 ))
        in_kva=$(( val >= 0x0C000000 && val < 0x0D000000 ))
        (( in_rom || in_kva )) || continue
        # Contiguous low-bit masks (0x00FF_FFFF, …) and powers of two
        # (sizes / alignments / region strides) are never meaningful
        # code addresses — skip them rather than allowlisting each.
        (( (val & (val + 1)) == 0 )) && continue
        (( (val & (val - 1)) == 0 )) && continue
        hit_allowed=0
        for i in "${!allow_files[@]}"; do
            if [[ "$f" == "${allow_files[$i]}" && "$norm" == "${allow_lits[$i]}" ]]; then
                hit_allowed=1
                allow_used[$i]=1
                break
            fi
        done
        if [[ $hit_allowed == 1 ]]; then
            allowed=$((allowed + 1))
        else
            echo "check-rom-addrs: $f:$lineno: ROM/kernel-VA literal $lit outside rom_ver/"
            violations=$((violations + 1))
        fi
    done < <(sed 's|//.*||' "$f" | grep -noE '0x[0-9A-Fa-f_]*[0-9A-Fa-f]' \
        | grep -E '0x[0-9A-Fa-f_]{5,}$' || true)
done < <(find src -name '*.rs' -not -path 'src/newton/rom_ver/*' | sort)

stale=0
for i in "${!allow_files[@]}"; do
    if [[ ${allow_used[$i]} == 0 ]]; then
        echo "check-rom-addrs: stale allowlist entry (no longer matches — delete it):"
        echo "    ${allow_files[$i]} ${allow_lits[$i]}"
        stale=$((stale + 1))
    fi
done

if [[ $violations -gt 0 || $stale -gt 0 ]]; then
    echo "check-rom-addrs: FAIL — $violations unlisted literal(s), $stale stale allowlist entr(y/ies)"
    exit 1
fi
echo "check-rom-addrs: OK — 0 unlisted ROM-address literals ($allowed allowlisted)"
