#!/usr/bin/env bash
# Build every ARM-guest test and run it under the chosen host (QEMU
# raspi3b by default, ARM FVP_Base_RevC with --platform fvp). Exits 0
# only if all tests report PASSED via HVC_PASS (HVC #0x12).
set -euo pipefail

platform="qemu"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --platform=*) platform="${1#--platform=}"; shift ;;
        --platform)   platform="$2"; shift 2 ;;
        -h|--help)    echo "usage: $0 [--platform {qemu,fvp}]" >&2; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

# Opt-in feature-matrix build check (CHECK_MATRIX=1). Off by default so
# the normal test loop doesn't pay ~10 extra cargo checks.
if [[ "${CHECK_MATRIX:-0}" == "1" ]]; then
    echo "feature-matrix check:"
    bash "$root/../scripts/check-matrix.sh"
    echo
fi

"$here/build-tests.sh" >/dev/null

pass=0
fail=0
while read -r name; do
    [[ -z "$name" ]] && continue
    [[ "$name" =~ ^# ]] && continue
    # test_snapshot_resume is a two-run save/resume test, not a one-shot.
    # Dispatch it to its dedicated driver; everything else goes through
    # the normal single-run run-test.sh.
    if [[ "$name" == "test_snapshot_resume" ]]; then
        runner=("$here/run-snapshot-resume.sh" --platform "$platform")
    else
        runner=("$here/run-test.sh" --platform "$platform" "$name")
    fi
    # Keep each runner's output. A bare PASS/FAIL column with the output
    # discarded makes a failure undiagnosable after the fact, and the
    # per-test log run-test.sh writes is overwritten by the next run of
    # the same test.
    out="/tmp/guest-run-${platform}-${name}.log"
    if "${runner[@]}" </dev/null > "$out" 2>&1; then
        printf "  \e[32m%-10s\e[0m %s\n" "PASS" "$name"
        pass=$((pass+1))
    else
        # run-test.sh exits 1 on an explicit HVC_FAIL and 2 when the run
        # produced no verdict at all (hang, halt, host-side error). Those
        # are different problems, so don't collapse them into one label.
        rc=$?
        case "$rc" in
            1) label="FAIL" ;;
            2) label="NO-VERDICT" ;;
            *) label="ERROR($rc)" ;;
        esac
        printf "  \e[31m%-10s\e[0m %s\n" "$label" "$name"
        tail -25 "$out" | sed 's/^/        | /'
        printf "        (full output: %s)\n" "$out"
        fail=$((fail+1))
    fi
done < "$root/tests/MANIFEST"

echo
echo "  summary ($platform): $pass passed, $fail failed"
[[ $fail -eq 0 ]]
