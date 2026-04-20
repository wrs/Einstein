#!/usr/bin/env bash
# Cargo runner: convert the linked ELF to a flat kernel8.img and launch QEMU.
# Usage (via cargo): `cargo run`. Extra QEMU args come from env var QEMU_EXTRA.
# Set DEBUG=1 to pause at entry with a gdb stub on :1234.

set -euo pipefail

elf="${1:?usage: run-qemu.sh <elf> [qemu args...]}"
shift || true

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
if [[ "${DEBUG:-0}" == "1" ]]; then
    debug_args=(-s -S)
    echo "[run-qemu] gdb stub on :1234; machine paused at entry." >&2
fi

# QEMU's raspi3b routes the first `-serial` to the PL011 and the second to
# the mini-UART. We use PL011 for the console.
exec qemu-system-aarch64 \
    -M raspi3b \
    -kernel "$img" \
    -serial stdio \
    -display none \
    -no-reboot \
    "${debug_args[@]}" \
    ${QEMU_EXTRA:-} \
    "$@"
