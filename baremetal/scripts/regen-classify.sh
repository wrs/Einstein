#!/usr/bin/env bash
# regen-classify.sh [ver] — regenerate the classifier outputs for one
# ROM version (default: 717006). Two chained passes:
#
#   1. scripts/classify-symbols.py — partitions the demangled symbol
#      table into code/data/drop and writes code-symbols.txt, the
#      curated code-only list that build.rs turns into the diag-layer
#      symbol tables (PC→name lookup, tracer trampoline pool).
#   2. tools/classify-rom — walks the ROM+REx from the symbol roots and
#      writes baremetal/classify/<hash>/reach.bitmap (plus companion
#      region dumps), where <hash> is the FNV-1a of ROM||REx. build.rs
#      stages reach.bitmap into the image for the shadow-stub
#      code/data classifier.
#
# Input selection mirrors resolve_rom_version() in build.rs: 717006
# reads the historical locations (roms/newton.rom, ../_Data_/*.txt,
# ../_Data_/Einstein.rex) with per-file overrides from roms/717006/
# when present; any other version reads exclusively from roms/<ver>/.
# Run whenever the ROM, the REx, or the symbol tables change.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
tool_dir="$root/tools/classify-rom"
classify_out="$root/classify"

ver="${1:-717006}"
ver_dir="$root/roms/$ver"

# pick <per-version override> <legacy fallback...>: first existing path
# wins — same order as build.rs's candidate lists.
pick() {
    local c
    for c in "$@"; do
        if [[ -f "$c" ]]; then
            echo "$c"
            return 0
        fi
    done
    # Nothing exists: return the first candidate so the error message
    # names the preferred location.
    echo "$1"
}

if [[ "$ver" == "717006" ]]; then
    rom="$(pick "$ver_dir/newton.rom" "$root/roms/newton.rom")"
    rex="$(pick "$ver_dir/Einstein.rex" "$root/../_Data_/Einstein.rex")"
    symbols="$(pick "$ver_dir/symbols.txt" "$root/../_Data_/symbols.txt")"
    demangled="$(pick "$ver_dir/demangled_symbols.txt" "$root/../_Data_/demangled_symbols.txt")"
    # build.rs prefers roms/717006/code-symbols.txt over the historical
    # scripts/classify-out/ location; write wherever the build will read.
    if [[ -f "$ver_dir/code-symbols.txt" ]]; then
        code_symbols_out="$ver_dir/code-symbols.txt"
    else
        code_symbols_out="$here/classify-out/code-symbols.txt"
    fi
else
    rom="$ver_dir/newton.rom"
    rex="$ver_dir/Einstein.rex"
    symbols="$ver_dir/symbols.txt"
    demangled="$ver_dir/demangled_symbols.txt"
    code_symbols_out="$ver_dir/code-symbols.txt"
fi

for f in "$rom" "$rex" "$symbols" "$demangled"; do
    if [[ ! -f "$f" ]]; then
        echo "regen-classify.sh: rom-$ver: missing input $f" >&2
        exit 1
    fi
done

# Pass 1: curated code-only symbol list (consumed by build.rs; also the
# root set audit trail under scripts/classify-out/).
"$here/classify-symbols.py" \
    --symbols "$demangled" \
    --rom "$rom" \
    --rex "$rex" \
    --code-symbols-out "$code_symbols_out"

# Pass 2: reach bitmap. classify-rom takes the raw (mangled)
# symbols.txt and applies its own root filters — see load_symbol_roots
# in tools/classify-rom/src/main.rs.
(cd "$tool_dir" && cargo build --release)

# The classify-rom crate pins its host target to aarch64-apple-darwin via
# its own .cargo/config.toml. Look there first, then fall back to the
# generic release dir in case a contributor edits that config.
bin="$tool_dir/target/aarch64-apple-darwin/release/classify-rom"
if [[ ! -x "$bin" ]]; then
    bin="$tool_dir/target/release/classify-rom"
fi
if [[ ! -x "$bin" ]]; then
    echo "regen-classify.sh: classify-rom binary not found under $tool_dir/target" >&2
    exit 1
fi

mkdir -p "$classify_out"

"$bin" \
    --rom "$rom" \
    --rex "$rex" \
    --symbols "$symbols" \
    --out "$classify_out"

# Surface the hash directory so callers can see what got regenerated.
newest="$(ls -td "$classify_out"/*/ 2>/dev/null | head -n1 || true)"
if [[ -n "$newest" ]]; then
    echo "regen-classify.sh: wrote $code_symbols_out"
    echo "regen-classify.sh: wrote $newest"
fi
