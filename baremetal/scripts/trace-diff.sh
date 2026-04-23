#!/usr/bin/env bash
# trace-diff.sh — run Einstein (NewtonTrace) and the bare-metal hypervisor
# with function-entry tracing on, then diff the two logs.
#
# Usage:
#   scripts/trace-diff.sh [--einstein-seconds N] [--baremetal-seconds M]
#                         [--lines L] [--out-dir DIR]
#
# Both sides log to DIR (default /tmp/trace-diff/). Diff is limited to the
# first L matching `^trace ` lines (default 5000) so the output stays
# readable on long boots.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BAREMETAL_DIR="$REPO_ROOT/baremetal"
BUILD_DIR="$REPO_ROOT/build"

einstein_seconds=5
baremetal_seconds=30
diff_lines=5000
out_dir=/tmp/trace-diff

while [[ $# -gt 0 ]]; do
    case "$1" in
        --einstein-seconds) einstein_seconds="$2"; shift 2 ;;
        --baremetal-seconds) baremetal_seconds="$2"; shift 2 ;;
        --lines) diff_lines="$2"; shift 2 ;;
        --out-dir) out_dir="$2"; shift 2 ;;
        -h|--help)
            sed -n 's/^# \{0,1\}//p' "$0" | head -12
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$out_dir"

rom_path="$BAREMETAL_DIR/roms/newton.rom"
rex_path="$REPO_ROOT/_Data_/Einstein.rex"
symbols_path="$BAREMETAL_DIR/scripts/classify-out/code-symbols.txt"

for f in "$rom_path" "$rex_path" "$symbols_path"; do
    if [[ ! -f "$f" ]]; then
        echo "missing: $f" >&2
        echo "(regenerate symbols via: $BAREMETAL_DIR/scripts/regen-classify.sh)" >&2
        exit 1
    fi
done

# --- Einstein side ---------------------------------------------------------
if [[ ! -x "$BUILD_DIR/NewtonTrace" ]]; then
    echo "building NewtonTrace..." >&2
    (cd "$BUILD_DIR" && cmake --build . --target NewtonTrace -j 4 >/dev/null)
fi

einstein_log="$out_dir/einstein-trace.log"
echo "==> Einstein: $einstein_seconds wall-seconds -> $einstein_log" >&2
"$BUILD_DIR/NewtonTrace" \
    "$rom_path" "$rex_path" "$symbols_path" "$einstein_log" "$einstein_seconds"

# --- Baremetal side --------------------------------------------------------
# trace feature shifts ROM bytes, so old snapshots are invalid — remove
# before cold-booting (see baremetal/CLAUDE.md "Gotchas").
rm -f /tmp/newton-snapshot-*.bin

baremetal_raw="$out_dir/baremetal-raw.log"
baremetal_log="$out_dir/baremetal-trace.log"
echo "==> Baremetal: up to $baremetal_seconds wall-seconds -> $baremetal_log" >&2
(
    cd "$BAREMETAL_DIR"
    # Cap the hypervisor run with a timeout. The hypervisor never exits on
    # its own; a guest stall will leave QEMU spinning otherwise.
    # gtimeout is coreutils on macOS (brew install coreutils); fall back to
    # /usr/bin/timeout on Linux.
    if command -v gtimeout >/dev/null 2>&1; then
        TIMEOUT=gtimeout
    else
        TIMEOUT=timeout
    fi
    $TIMEOUT --preserve-status --signal=KILL "${baremetal_seconds}s" \
        cargo run --release --features trace,quiet 2>&1 \
        | tee "$baremetal_raw" >/dev/null || true
)

# Pull just the `trace ` lines from the baremetal raw log. kprintln! adds
# no prefix, so the format is identical; strip CRs from QEMU UART output
# to keep the diff clean. `|| true` because grep exits 1 when the raw log
# has no trace lines (e.g. the hypervisor halted before ROMBoot).
grep '^trace ' "$baremetal_raw" 2>/dev/null | tr -d '\r' > "$baremetal_log" || true

# --- Diff ------------------------------------------------------------------
einstein_head="$out_dir/einstein-head.log"
baremetal_head="$out_dir/baremetal-head.log"
head -n "$diff_lines" "$einstein_log" > "$einstein_head"
head -n "$diff_lines" "$baremetal_log" > "$baremetal_head"

einstein_count=$(wc -l < "$einstein_log" | tr -d ' ')
baremetal_count=$(wc -l < "$baremetal_log" | tr -d ' ')
echo "==> trace lines: einstein=$einstein_count  baremetal=$baremetal_count" >&2

if [[ "$baremetal_count" -eq 0 ]]; then
    echo "==> baremetal produced no trace lines — hypervisor likely halted before any traced function ran" >&2
    echo "==> baremetal raw log tail:" >&2
    tail -20 "$baremetal_raw" >&2
    exit 2
fi

echo "==> diffing first $diff_lines lines..." >&2
diff_out="$out_dir/trace.diff"
if diff -u "$einstein_head" "$baremetal_head" > "$diff_out"; then
    echo "==> MATCH (first $diff_lines lines)" >&2
    exit 0
else
    divergence=$(grep -n '^[-+]trace ' "$diff_out" | head -1 | cut -d: -f1 || true)
    echo "==> DIVERGE; full diff at $diff_out (first divergence near line $divergence)" >&2
    head -40 "$diff_out"
    exit 1
fi
