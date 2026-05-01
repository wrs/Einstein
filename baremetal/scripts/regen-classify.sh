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
    echo "regen-classify.sh: wrote $newest"
fi
