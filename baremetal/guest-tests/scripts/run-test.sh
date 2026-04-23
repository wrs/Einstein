#!/usr/bin/env bash
# Build the hypervisor with a given guest test image embedded, run under
# either QEMU raspi3b or ARM FVP_Base_RevC, and report PASS/FAIL based
# on the HVC protocol output.
set -euo pipefail

platform="qemu"
test_name=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --platform=*) platform="${1#--platform=}"; shift ;;
        --platform)   platform="$2"; shift 2 ;;
        -h|--help)
            cat <<'EOF' >&2
usage: run-test.sh [--platform {qemu,fvp}] <test-name>

Builds the hypervisor with baremetal/guest-tests/tests/<test-name>.bin
embedded, runs it on the chosen host, and asserts 'guest test PASSED'.
EOF
            exit 0
            ;;
        -*) echo "unknown flag: $1" >&2; exit 2 ;;
        *)  test_name="$1"; shift ;;
    esac
done

case "$platform" in
    qemu|fvp) ;;
    *) echo "--platform must be qemu or fvp (got '$platform')" >&2; exit 2 ;;
esac

if [[ -z "$test_name" ]]; then
    echo "usage: $0 [--platform {qemu,fvp}] <test-name>  (e.g. test_hello)" >&2
    exit 2
fi

here="$(cd "$(dirname "$0")" && pwd)"
bin="$here/../tests/build/${test_name}.bin"

if [[ ! -f "$bin" ]]; then
    echo "$bin not found — did you run scripts/build-tests.sh?" >&2
    exit 2
fi

cd "$here/../../"
export NH_GUEST_TEST="$bin"

log=/tmp/guest-${platform}-${test_name}.out

if [[ "$platform" == "qemu" ]]; then
    cargo build --release 2>&1 | tail -5
    elf=target/aarch64-unknown-none-softfloat/release/newton-hypervisor
    img=/tmp/kernel8-guest-${test_name}.img
    objcopy="$(find "$(rustc --print sysroot)" -name llvm-objcopy -print -quit)"
    "$objcopy" -O binary "$elf" "$img"

    # Remove any snapshot left over from a previous run. The hypervisor's
    # snapshot fingerprint is keyed on the first 1 KiB of ROM, which is
    # stable within a single test binary; a prior invocation of the SAME
    # test (or a cached slot whose fingerprint happens to collide) will
    # resume mid-run and break reproducibility.
    rm -f /tmp/newton-snapshot-*.bin

    timeout 10 qemu-system-aarch64 -M raspi3b -kernel "$img" \
        -serial stdio -display none -no-reboot \
        -semihosting-config enable=on,target=native > "$log" 2>&1 || true
else
    # FVP path — build with the fvp-base platform feature, run the ELF
    # directly (FVP loads by program headers), scrape the same markers.
    cargo build --release --no-default-features --features platform-fvp-base 2>&1 | tail -5
    elf=target/aarch64-unknown-none-softfloat/release/newton-hypervisor
    scripts/fvp --timeout=300 "$elf" > "$log" 2>&1 || true
fi

if grep -q 'guest test PASSED' "$log"; then
    echo "PASS ($platform): $test_name"
    exit 0
elif grep -q 'guest test FAILED' "$log"; then
    echo "FAIL ($platform): $test_name"
    tail -20 "$log"
    exit 1
else
    echo "TIMEOUT / no result ($platform) for $test_name"
    tail -40 "$log"
    exit 2
fi
