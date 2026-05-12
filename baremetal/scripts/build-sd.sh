#!/usr/bin/env bash
#
# Build a Pi Zero 2 W boot-partition layout from the current source tree.
#
# Usage:
#   scripts/build-sd.sh <dest-dir>
#       Build the pi-probe binary, fetch the Pi firmware blobs (cached
#       under target/pi-firmware-cache/), and assemble a complete boot-
#       partition layout under <dest-dir>. The user then copies the
#       contents to the root of a FAT32-formatted SD card.
#
# Env vars:
#   PI_FIRMWARE_CACHE   override the default cache location
#   PI_KERNEL_BIN       which [[bin]] to use as kernel8.img
#                       (default: pi-probe; Phase 1+ will swap to
#                       newton-hypervisor)
#   PI_FIRMWARE_COMMIT  raspberrypi/firmware commit to pin to
#
# See docs/REAL_HW_BRINGUP.md for the full Phase-0 workflow.

set -euo pipefail

# Pinned firmware commit. Bump deliberately, and only after re-testing
# end-to-end on the Zero 2 W — firmware updates have broken bare-metal
# boots in the past.
PI_FIRMWARE_COMMIT="${PI_FIRMWARE_COMMIT:-8fce67a9ec5668fb8d42d215c9ec4c199340bf41}"

FW_BASE="https://raw.githubusercontent.com/raspberrypi/firmware/${PI_FIRMWARE_COMMIT}/boot"

# Minimum set for the Pi Zero 2 W (BCM2710A1). start_cd.elf is the
# cut-down GPU firmware (~700 KB vs ~3 MB for start.elf); it omits HDMI
# overscan, codec licensing, and some camera support — none of which
# we use in Phase 0. fixup_cd.dat is its matching memory-split file.
FW_FILES=(
    bootcode.bin
    start_cd.elf
    fixup_cd.dat
    bcm2710-rpi-zero-2-w.dtb
    overlays/disable-bt.dtbo
)

usage() {
    cat >&2 <<EOF
usage: scripts/build-sd.sh <dest-dir>

  <dest-dir> will contain a complete boot-partition layout: copy its
  contents to the root of a FAT32-formatted SD card.

  Set PI_KERNEL_BIN to pick a different [[bin]] (default: pi-probe).
EOF
    exit 1
}

[[ $# -eq 1 ]] || usage
dest="$1"

# Repo root = directory containing this script's parent.
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cache="${PI_FIRMWARE_CACHE:-${repo_root}/target/pi-firmware-cache}"
kernel_bin="${PI_KERNEL_BIN:-pi-probe}"

mkdir -p "$dest" "$dest/overlays" "$cache" "$cache/overlays"

# --- 1. Fetch firmware blobs into the cache (skipped if present) -------
for f in "${FW_FILES[@]}"; do
    if [[ ! -f "$cache/$f" ]]; then
        echo "fetch: $f"
        curl -fsSLo "$cache/$f" "$FW_BASE/$f"
    fi
done

# --- 2. Build the chosen [[bin]] ---------------------------------------
echo "build: cargo --release --bin $kernel_bin"
( cd "$repo_root" && cargo build --release --bin "$kernel_bin" )

elf="${repo_root}/target/aarch64-unknown-none-softfloat/release/${kernel_bin}"

# --- 3. Convert ELF → raw kernel8.img ----------------------------------
sysroot="$(rustc --print sysroot)"
objcopy="$(find "$sysroot" -name 'llvm-objcopy' -print -quit)"
if [[ -z "$objcopy" ]]; then
    echo "error: llvm-objcopy not found under $sysroot" >&2
    echo "hint: run 'rustup component add llvm-tools-preview'" >&2
    exit 1
fi
echo "objcopy: $kernel_bin → kernel8.img"
"$objcopy" -O binary "$elf" "$dest/kernel8.img"

# --- 4. Copy firmware + config -----------------------------------------
for f in "${FW_FILES[@]}"; do
    cp "$cache/$f" "$dest/$f"
done
cp "${repo_root}/boot-pi/config.txt" "$dest/config.txt"

# --- 5. Summary --------------------------------------------------------
cat <<EOF

SD boot partition assembled at: $dest

contents:
$(ls -la "$dest" "$dest/overlays" | sed 's/^/  /')

next steps:
  1. Format a microSD card as FAT32 (single partition, MBR).
  2. Copy the contents of $dest to the root of the SD card:
       cp -R "$dest"/* /Volumes/<SD-card-name>/
  3. Eject, insert into the Pi Zero 2 W (note: PWR IN port for power).
     Wire the 3.3V USB-TTL cable to the Pi GPIO header:
       Pi pin  6  (GND)        <->  cable GND
       Pi pin  8  (GPIO 14 TX) -->  cable RX
       Pi pin 10  (GPIO 15 RX) <--  cable TX
     Leave the cable's 5V/VCC pin disconnected.
     Open serial at 115200 8N1, then power on the Pi.
  4. Expect:
       === newton pi-probe ===
       CurrentEL = 2
       MIDR_EL1  = 0x...
       MPIDR_EL1 = 0x...
       ok, parking core 0 in WFE

  See docs/REAL_HW_BRINGUP.md if it doesn't print.
EOF
