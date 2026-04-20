#!/usr/bin/env bash
# Cross-compile every test listed in tests/MANIFEST to a flat AArch32
# binary at tests/build/<name>.bin. Usage: `scripts/build-tests.sh`.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

CC=arm-none-eabi-gcc
OBJCOPY=arm-none-eabi-objcopy

mkdir -p "$root/tests/build"

while read -r name; do
    [[ -z "$name" ]] && continue
    [[ "$name" =~ ^# ]] && continue
    src="$root/tests/$name.S"
    elf="$root/tests/build/$name.elf"
    bin="$root/tests/build/$name.bin"

    if [[ ! -f "$src" ]]; then
        echo "error: $src not found" >&2
        exit 2
    fi

    # armv7ve has the virtualization extensions (HVC opcode).
    # Linker script keeps the .text.vectors section at offset 0 so the
    # guest's reset and IRQ vectors live where the CPU expects them.
    $CC -x assembler-with-cpp -nostdlib \
        -Wl,-T,"$root/common/linker.ld" \
        -march=armv7ve -marm \
        -I"$root" \
        -o "$elf" "$src"
    $OBJCOPY -O binary "$elf" "$bin"
    size=$(wc -c < "$bin" | tr -d ' ')
    printf "  built %-20s  %5d bytes\n" "$name" "$size"
done < "$root/tests/MANIFEST"
