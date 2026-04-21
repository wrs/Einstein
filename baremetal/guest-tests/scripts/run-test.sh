#!/usr/bin/env bash
# Build the hypervisor with a given guest test image embedded, run under
# QEMU raspi3b, and report PASS/FAIL based on the HVC protocol output.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <test-name>  (e.g. test_hello)" >&2
    exit 2
fi

test_name="$1"
here="$(cd "$(dirname "$0")" && pwd)"
bin="$here/../tests/build/${test_name}.bin"

if [[ ! -f "$bin" ]]; then
    echo "$bin not found — did you run scripts/build-tests.sh?" >&2
    exit 2
fi

# Build the hypervisor with this test embedded.
cd "$here/../../"
export NH_GUEST_TEST="$bin"
cargo build --release 2>&1 | tail -5

elf=target/aarch64-unknown-none-softfloat/release/newton-hypervisor
img=/tmp/kernel8-guest-${test_name}.img
objcopy="$(find "$(rustc --print sysroot)" -name llvm-objcopy -print -quit)"
"$objcopy" -O binary "$elf" "$img"

# Run. Capture output, check for PASS / FAIL markers.
log=/tmp/guest-${test_name}.out
timeout 10 qemu-system-aarch64 -M raspi3b -kernel "$img" \
    -serial stdio -display none -no-reboot \
    -semihosting-config enable=on,target=native > "$log" 2>&1 || true

if grep -q 'guest test PASSED' "$log"; then
    echo "PASS: $test_name"
    exit 0
elif grep -q 'guest test FAILED' "$log"; then
    echo "FAIL: $test_name"
    tail -20 "$log"
    exit 1
else
    echo "TIMEOUT / no result for $test_name"
    tail -40 "$log"
    exit 2
fi
