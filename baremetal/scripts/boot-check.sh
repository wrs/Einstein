#!/usr/bin/env bash
# Deterministic QEMU boot verifier: launch `cargo run --release`, poll
# the redirected log for success markers, and kill QEMU the moment they
# all appear — so a boot check costs ~the boot time (~17 s to the
# Welcome UI) instead of a fixed worst-case timeout. Prints the elapsed
# time and each marker's first matching line on success; on timeout (or
# an early cargo/QEMU exit) prints the log tail and exits 1.
#
# Usage:
#   scripts/boot-check.sh [--cold] [--timeout N] [--log PATH]
#                         [--marker REGEX]...
#
#   --cold          rm -f /tmp/newton-snapshot-*.bin first (force a
#                   cold boot instead of a snapshot resume).
#   --timeout N     hard fallback in seconds (default 180).
#   --log PATH      boot log destination
#                   (default /tmp/newton-claude/boot-check.log).
#   --marker REGEX  success marker (grep ERE), repeatable; ALL given
#                   markers must appear. Overrides the default pair.
#                   BOOT_MARKERS (newline-separated) does the same.
#
# Default markers are the cold-boot steady-state milestone from the
# phase-0 baseline: the NewtonScript REP banner plus a full-screen
# 480x320 blit. Resume-based runs have a different shape — override
# with e.g. --marker 'Resuming guest from snapshot'.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
cd "$root"

markers=()
timeout=180
log=/tmp/newton-claude/boot-check.log
cold=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cold)    cold=1; shift ;;
        --timeout) timeout="${2:?--timeout needs a value}"; shift 2 ;;
        --log)     log="${2:?--log needs a value}"; shift 2 ;;
        --marker)  markers+=("${2:?--marker needs a value}"); shift 2 ;;
        *) echo "boot-check: unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ ${#markers[@]} -eq 0 ]]; then
    if [[ -n "${BOOT_MARKERS:-}" ]]; then
        while IFS= read -r m; do
            [[ -n "$m" ]] && markers+=("$m")
        done <<<"$BOOT_MARKERS"
    else
        markers=(
            'REP> Welcome to NewtonScript!'
            'copied=(38400|153600)'
        )
    fi
fi

# QEMU outlives a killed `cargo run`, so every exit path — success,
# timeout, Ctrl-C — must pkill it. SIGKILL, not the default SIGTERM:
# QEMU in our semihosting configuration defers SIGTERM while the guest
# is busy (see docs/QEMU_BUGS.md "timeout doesn't kill QEMU"), and a
# boot-check kill fires mid-boot, exactly when the guest is busy.
# Exact process name, not -f: matches qemu-system-aarch64 without
# catching unrelated argv.
run_pid=""
cleanup() {
    pkill -KILL qemu-system 2>/dev/null
    for _ in 1 2 3 4 5; do
        pgrep -q qemu-system 2>/dev/null || break
        sleep 1
    done
    if pgrep -q qemu-system 2>/dev/null; then
        echo "boot-check: warning: qemu-system still running after SIGKILL" >&2
    fi
    if [[ -n "$run_pid" ]]; then
        wait "$run_pid" 2>/dev/null
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

mkdir -p "$(dirname "$log")"
if [[ $cold -eq 1 ]]; then
    rm -f /tmp/newton-snapshot-*.bin
fi

cargo run --release >"$log" 2>&1 &
run_pid=$!
start=$(date +%s)

fail() {
    echo "boot-check: $1 (log: $log)"
    echo "        ---- last 20 lines of output ----"
    tail -20 "$log" | sed 's/^/        /'
    exit 1
}

while :; do
    all=1
    for m in "${markers[@]}"; do
        grep -q -E -e "$m" "$log" || { all=0; break; }
    done
    if [[ $all -eq 1 ]]; then
        elapsed=$(( $(date +%s) - start ))
        echo "boot-check: all ${#markers[@]} marker(s) matched after ${elapsed}s"
        for m in "${markers[@]}"; do
            grep -m1 -E -e "$m" "$log" | sed 's/^/  /'
        done
        exit 0
    fi
    if ! kill -0 "$run_pid" 2>/dev/null; then
        fail "cargo run exited before all markers appeared"
    fi
    if (( $(date +%s) - start >= timeout )); then
        fail "timeout after ${timeout}s without all markers"
    fi
    sleep 1
done
