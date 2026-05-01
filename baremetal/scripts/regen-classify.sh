#!/usr/bin/env bash
# Build classify-rom and run it against the current ROM + REX, producing
# baremetal/classify/<hash>/byte-access-static.bitmap. baremetal/build.rs
# consumes that bitmap at compile time and embeds it in the hypervisor
# image; invoke this script whenever roms/newton.rom or
# ../_Data_/Einstein.rex changes.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
tool_dir="$root/tools/classify-rom"
classify_out="$root/classify"

rom="$root/roms/newton.rom"
rex="$root/../_Data_/Einstein.rex"

# Use the code-only symbol list produced by classify-symbols.py —
# feeding the raw _Data_/demangled_symbols.txt seeds classify-rom
# with data labels too (gFoo, kFoo, theFoo, string tables, ...),
# which then leaks reachability into adjacent data and pulls
# false-positive byte-access bits into the bitmap. See
# scripts/classify-symbols.py for the rule set that sorts symbols
# into code vs. data.
here_sym="$here/classify-out/code-symbols.txt"
if [[ ! -f "$here_sym" ]]; then
    echo "regen-classify.sh: regenerating code-symbols.txt" >&2
    "$here/classify-symbols.py" >/dev/null
fi
symbols="$here_sym"

for f in "$rom" "$rex" "$symbols"; do
    if [[ ! -f "$f" ]]; then
        echo "regen-classify.sh: missing input $f" >&2
        exit 1
    fi
done

# Detect the host triple from the active rustc so the build works on Intel
# Macs, Apple Silicon Macs, and Linux without anyone editing the crate's
# .cargo/config.toml. Passing --target on the command line overrides the
# build.target hardcoded in tools/classify-rom/.cargo/config.toml.
host="$(cd "$tool_dir" && rustc -vV | sed -n 's/^host: //p')"
if [[ -z "$host" ]]; then
    echo "regen-classify.sh: could not determine host triple from rustc -vV" >&2
    exit 1
fi

(cd "$tool_dir" && cargo build --release --target "$host")

bin="$tool_dir/target/$host/release/classify-rom"
if [[ ! -x "$bin" ]]; then
    echo "regen-classify.sh: classify-rom binary not found at $bin" >&2
    exit 1
fi

mkdir -p "$classify_out"

data_ranges="$here/classify-out/data-ranges.txt"
extra=()
if [[ -f "$data_ranges" ]]; then
    extra+=(--data-ranges "$data_ranges")
fi

"$bin" \
    --rom "$rom" \
    --rex "$rex" \
    --symbols "$symbols" \
    --out "$classify_out" \
    "${extra[@]}"

# Surface the hash directory so callers can see what got regenerated.
newest="$(ls -td "$classify_out"/*/ 2>/dev/null | head -n1 || true)"
if [[ -n "$newest" ]]; then
    echo "regen-classify.sh: wrote $newest"
fi
