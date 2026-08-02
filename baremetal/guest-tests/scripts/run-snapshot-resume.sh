#!/usr/bin/env bash
# Two-run driver for test_snapshot_resume. Run 1 boots the test cold
# (snapshots cleared) — the guest plants a GPR+RAM pattern and issues
# HVC #0x18 (HVC_SNAPSHOT) to save. Run 2 boots the SAME binary with the snapshot
# present and must resume from it (printing "Resuming guest from
# snapshot") and pass the guest-side post-resume pattern check.
#
# Snapshot-file hygiene matches run-test.sh: we clear
# /tmp/newton-snapshot-*.bin before run 1 and again after run 2, so a
# developer's slots are clobbered no more than any normal `run-all.sh`
# already does. Between the two runs the slot MUST survive, so the
# clear happens exactly once up front, never between runs.
#
# Usage: run-snapshot-resume.sh [--platform {qemu,fvp}]
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
test_name="test_snapshot_resume"
bin="$here/../tests/build/${test_name}.bin"

if [[ ! -f "$bin" ]]; then
    echo "$bin not found — did you run scripts/build-tests.sh?" >&2
    exit 2
fi
bin_abs="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"

cd "$here/../../"
export NH_GUEST_TEST=1

run1=/tmp/guest-${platform}-${test_name}-run1.out
run2=/tmp/guest-${platform}-${test_name}-run2.out

# Build the hypervisor once (semihost guest-test mode); both runs reuse
# the same image.
if [[ "$platform" == "qemu" ]]; then
    cargo build --release 2>&1 | tail -5
    elf=target/aarch64-unknown-none-softfloat/release/newton-hypervisor
    img=/tmp/kernel8-guest-${test_name}.img
    objcopy="$(find "$(rustc --print sysroot)" -name llvm-objcopy -print -quit)"
    "$objcopy" -O binary "$elf" "$img"
    semihost_arg="enable=on,target=native,arg=${bin_abs}"
    qemu_run() {
        timeout 15 qemu-system-aarch64 -M raspi3b -kernel "$img" \
            -serial stdio -display none -no-reboot \
            -semihosting-config "$semihost_arg" > "$1" 2>&1 || true
    }
else
    # FVP: scripts/fvp has no semihosting-cmdline plumbing (the QEMU
    # `arg=` path), so the semihost test-bin loader can't run. Use EMBED
    # mode instead — the test bin is compiled into the hypervisor at
    # build time, so no cmdline is needed. scripts/fvp bind-mounts each
    # EXISTING /tmp/newton-snapshot-{0..3}.bin into the container, so the
    # guest's semihosting writes reach the host files; we pre-create all
    # four slots before run 1 so the save side has a mount to write to.
    NH_GUEST_TEST="$bin_abs" cargo build --release \
        --no-default-features --features "platform-fvp-base diag" 2>&1 | tail -5
    elf=target/aarch64-unknown-none-softfloat/release/newton-hypervisor
    qemu_run() {
        scripts/fvp --timeout=300 "$elf" > "$1" 2>&1 || true
    }
fi

# --- Run 1: cold boot, save. Clear any prior slots first. ---
rm -f /tmp/newton-snapshot-*.bin
if [[ "$platform" == "fvp" ]]; then
    # Pre-create empty slot files so scripts/fvp's per-slot bind-mount
    # exists for the save side (it only mounts files present on the
    # host). Empty files fail the magic/version check, so run 1 still
    # cold-boots — exactly what we want.
    for s in 0 1 2 3; do : > "/tmp/newton-snapshot-${s}.bin"; done
fi
qemu_run "$run1"

if ! grep -q 'guest test PASSED' "$run1"; then
    echo "FAIL ($platform): $test_name (run 1 did not PASS)"
    tail -30 "$run1"
    rm -f /tmp/newton-snapshot-*.bin
    exit 1
fi
if ! grep -q 'snapshot: seq=.* saved' "$run1"; then
    echo "FAIL ($platform): $test_name (run 1 produced no snapshot save line)"
    tail -30 "$run1"
    rm -f /tmp/newton-snapshot-*.bin
    exit 1
fi
if ! ls /tmp/newton-snapshot-*.bin >/dev/null 2>&1; then
    echo "FAIL ($platform): $test_name (no snapshot file on disk after run 1)"
    rm -f /tmp/newton-snapshot-*.bin
    exit 1
fi

# --- Run 2: snapshots present — must resume, not cold-boot. ---
qemu_run "$run2"

ok=1
if ! grep -q 'Resuming guest from snapshot' "$run2"; then
    echo "FAIL ($platform): $test_name (run 2 did not print the resume banner — it cold-booted)"
    ok=0
fi
if grep -q 'Entering Newton ROM' "$run2"; then
    echo "FAIL ($platform): $test_name (run 2 cold-booted instead of resuming)"
    ok=0
fi
if ! grep -q 'guest test PASSED' "$run2"; then
    echo "FAIL ($platform): $test_name (run 2 post-resume pattern check did not PASS)"
    ok=0
fi

# Clean up so a re-run starts fresh and developer slots aren't left
# holding this test's state.
rm -f /tmp/newton-snapshot-*.bin

if [[ $ok -eq 1 ]]; then
    echo "PASS ($platform): $test_name (run 1 saved, run 2 resumed + verified)"
    exit 0
else
    echo "  ---- run 1 tail ----"; tail -15 "$run1"
    echo "  ---- run 2 tail ----"; tail -30 "$run2"
    exit 1
fi
