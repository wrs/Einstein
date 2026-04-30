#!/usr/bin/env bash
# Cargo runner: convert the linked ELF to a flat kernel8.img and launch QEMU.
# Usage (via cargo): `cargo run`. Extra QEMU args come from env var QEMU_EXTRA.
# Set DEBUG=1, or pass --gdb, to pause at entry with a gdb stub on :1234.
# (DEBUG=1 and --gdb are equivalent.)

set -euo pipefail

elf="${1:?usage: run-qemu.sh <elf> [qemu args...]}"
shift || true

# Capture --gdb before forwarding remaining args to QEMU.
gdb_flag=0
qemu_passthrough=()
for arg in "$@"; do
    case "$arg" in
        --gdb) gdb_flag=1 ;;
        *)     qemu_passthrough+=("$arg") ;;
    esac
done
set -- ${qemu_passthrough[@]+"${qemu_passthrough[@]}"}

# Locate rust-objcopy from the active toolchain.
sysroot="$(rustc --print sysroot)"
objcopy="$(find "$sysroot" -name 'llvm-objcopy' -print -quit)"
if [[ -z "$objcopy" ]]; then
    echo "error: llvm-objcopy not found under $sysroot" >&2
    echo "hint: run 'rustup component add llvm-tools-preview'" >&2
    exit 1
fi

img="${elf%.elf}.img"
# cargo passes an ELF with no extension; if so, append .img next to it.
if [[ "$img" == "$elf" ]]; then
    img="${elf}.img"
fi
"$objcopy" -O binary "$elf" "$img"

debug_args=()
if [[ "${DEBUG:-0}" == "1" || "$gdb_flag" == "1" ]]; then
    debug_args=(-s -S)
    echo "[run-qemu] gdb stub on :1234; machine paused at entry." >&2
    echo "[run-qemu]   attach from another terminal:" >&2
    echo "[run-qemu]     aarch64-elf-gdb -ex 'target remote :1234' '$elf'" >&2
fi

# QEMU's raspi3b routes the first `-serial` to the PL011 and the second to
# the mini-UART. We use PL011 for the console.
#
# Semihosting is enabled so the hypervisor can save/load snapshots via
# HLT #0xF000 (see src/snapshot.rs). target=native makes the hypervisor
# itself own the semihosting surface; paths are resolved against the
# host's cwd.
exec qemu-system-aarch64 \
    -M raspi3b \
    -kernel "$img" \
    -serial mon:stdio \
    -display none \
    -no-reboot \
    -semihosting-config enable=on,target=native \
    ${debug_args[@]+"${debug_args[@]}"} \
    ${QEMU_EXTRA:-} \
    "$@"
