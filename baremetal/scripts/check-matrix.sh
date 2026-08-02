#!/usr/bin/env bash
# Feature-matrix build check: `cargo check` over every supported build
# combination so the axis architecture (host-io / flash-persist / input
# / audio / platform) stays green off the default path. Cargo feature
# dependencies (hardware backends imply platform-raspi3b) plus the
# platform mutual-exclusion check in build.rs reject the impossible
# cross-axis combos at configure time; this script proves the
# *supported* set still compiles.
#
# Each combination is a sequential `cargo check` in ONE shared target
# dir (CARGO_TARGET_DIR below). The combos share almost all of their
# dependency graph, so a shared dir keeps the third-party crates compiled
# once; only the newton-hypervisor crate itself re-checks per combo
# (feature change invalidates it regardless). Distinct per-combo dirs
# were measured to be ~5x slower for no isolation benefit here.
#
# Prints a PASS/FAIL line per combination and exits nonzero if any fail.
# Opt-in from guest-tests/scripts/run-all.sh via CHECK_MATRIX=1.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
cd "$root"

# Isolated, reused target dir so a check run doesn't thrash the
# incremental cache of interactive `cargo build`/`cargo run` sessions.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/check-matrix}"

# Import-discipline lint (cheap, always run — it guards structure
# rather than a combo).
if bash "$here/check-layering.sh" >/tmp/check-matrix-last.log 2>&1; then
    printf "  \e[32mPASS\e[0m  %-24s\n" "check-layering"
else
    printf "  \e[31mFAIL\e[0m  %-24s\n" "check-layering"
    sed 's/^/        /' /tmp/check-matrix-last.log
    exit 1
fi

# Each entry: "label::<cargo check args>". Args are eval'd so quoted
# feature lists survive. Env-prefixed entries (NH_GUEST_TEST=1) set the
# guest-test cfg the same way run-test.sh does.
combos=(
    "default::cargo check --release"
    "no-diag::cargo check --release --no-default-features --features \"platform-raspi3b log_traps log_irqs log_host_io\""
    "platform-fvp-base::cargo check --release --no-default-features --features \"platform-fvp-base quiet diag\""
    "fvp-no-diag::cargo check --release --no-default-features --features \"platform-fvp-base quiet\""
    "pi-bare-metal::cargo check --release --no-default-features --features pi-bare-metal"
    "pi-bare-metal-sd::cargo check --release --no-default-features --features pi-bare-metal-sd"
    "pi-bare-metal-display::cargo check --release --no-default-features --features pi-bare-metal-display"
    "pi-bare-metal-input::cargo check --release --no-default-features --features pi-bare-metal-input"
    "trace,quiet::cargo check --release --features \"trace quiet\""
    "trace_once::cargo check --release --features \"trace_once quiet\""
    "host-io-semihost::cargo check --release --features host-io-semihost"
    "sd-probe::cargo check --release --no-default-features --features \"pi-bare-metal sd-probe\""
    "fb-probe::cargo check --release --no-default-features --features \"pi-bare-metal fb-probe\""
    "ns_trace::cargo check --release --features ns_trace"
    "log-all::cargo check --release --features \"log_mmu log_tasks log_unaligned log_store\""
    "guest-test::NH_GUEST_TEST=1 cargo check --release"
)

pass=0
fail=0
failed_labels=()
start=$(date +%s)

for entry in "${combos[@]}"; do
    label="${entry%%::*}"
    cmd="${entry#*::}"
    printf "  ....  %-24s" "$label"
    if eval "$cmd" >/tmp/check-matrix-last.log 2>&1; then
        printf "\r  \e[32mPASS\e[0m  %-24s\n" "$label"
        pass=$((pass+1))
    else
        printf "\r  \e[31mFAIL\e[0m  %-24s\n" "$label"
        echo "        ---- last 20 lines of output ----"
        tail -20 /tmp/check-matrix-last.log | sed 's/^/        /'
        fail=$((fail+1))
        failed_labels+=("$label")
    fi
done

end=$(date +%s)
echo
echo "  summary: $pass passed, $fail failed  (${#combos[@]} combinations, $((end - start))s)"
if [[ $fail -ne 0 ]]; then
    echo "  failures: ${failed_labels[*]}"
    exit 1
fi
