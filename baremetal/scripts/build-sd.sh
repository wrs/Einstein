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
#   PI_CARGO_FEATURES   base Cargo features (default: pi-bare-metal).
#                       Set to `pi-bare-metal-sd` for SD-backed flash
#                       persistence — that feature aggregate is
#                       mutually exclusive with the default's null
#                       backend so it must replace, not append.
#   PI_EXTRA_FEATURES   space-separated Cargo features appended to
#                       PI_CARGO_FEATURES (e.g. `sd-probe`)
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
# cut-down GPU firmware (~700 KB vs ~3 MB for start.elf); it omits
# HDMI codec licensing and some camera support — none of which we
# use. We tested the full firmware (start.elf, gpu_mem=64) to see
# if it would clear the persistent firmware-side white bar at the
# top of HDMI scan-out — it didn't, and cost 48 MB of GPU RAM, so
# we stayed on the cut-down variant. The bar is cleared only by
# the KMS/DispmanX path, which would be a much bigger bring-up.
FW_FILES=(
    bootcode.bin
    start_cd.elf
    fixup_cd.dat
    bcm2710-rpi-zero-2-w.dtb
    overlays/disable-bt.dtbo
)

usage() {
    cat >&2 <<EOF
usage: scripts/build-sd.sh <dest-dir> [<sd-mount>]

  <dest-dir>  will contain a complete boot-partition layout.
  <sd-mount>  if given, also rsync the layout to that path — typically
              the mounted SD card (e.g. /Volumes/bootfs on macOS).
              Only files whose mtime+size differ are written, so
              re-running after a small kernel rebuild touches just
              kernel8.img.

  Set PI_KERNEL_BIN to pick a different [[bin]] (default: pi-probe).
EOF
    exit 1
}

[[ $# -eq 1 || $# -eq 2 ]] || usage
dest="$1"
sd_mount="${2:-}"

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
#
# Use the `pi-bare-metal` feature aggregate for both bins: it pulls in
# platform-raspi3b (PL011 base, MMIO map) and no-semihost / flash-
# persist-null for the real-silicon build of the main hypervisor.
# pi-probe is unaffected by no-semihost / flash-persist-null but
# satisfies its `required-features = ["platform-raspi3b"]` via the
# aggregate. See Cargo.toml for the feature definition.
features="${PI_CARGO_FEATURES:-pi-bare-metal}${PI_EXTRA_FEATURES:+ $PI_EXTRA_FEATURES}"
echo "build: cargo --release --no-default-features --features '$features' --bin $kernel_bin"
(
    cd "$repo_root"
    cargo build --release --no-default-features --features "$features" \
        --bin "$kernel_bin"
)

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

# --- 4. Sync firmware + config into the staging dir ----------------
# rsync (default mtime+size compare) skips files that haven't
# changed, so re-running after a kernel rebuild only re-writes
# kernel8.img. Important when the dest is a slow SD card.
for f in "${FW_FILES[@]}"; do
    rsync -a "$cache/$f" "$dest/$f"
done
rsync -a "${repo_root}/boot-pi/config.txt" "$dest/config.txt"

# --- 4b. Optionally also push to a mounted SD card -----------------
if [[ -n "$sd_mount" ]]; then
    if [[ ! -d "$sd_mount" ]]; then
        echo "error: <sd-mount> '$sd_mount' is not a directory" >&2
        exit 1
    fi
    echo "sync: $dest/ → $sd_mount/"
    rsync -a "$dest/" "$sd_mount/"
    sync
fi

# --- 5. Summary --------------------------------------------------------
cat <<EOF

SD boot partition assembled at: $dest

contents:
$(ls -la "$dest" "$dest/overlays" | sed 's/^/  /')

next steps:
  1. Format a microSD card as FAT32 (single partition, MBR).
  2. Sync the contents of $dest to the SD card. Either:
       - re-run this script with the mount path as the second arg:
           scripts/build-sd.sh "$dest" /Volumes/<SD-card-name>
         (only changed files are written; safe to do every rebuild)
       - or rsync manually:
           rsync -a "$dest"/ /Volumes/<SD-card-name>/
  3. Eject, insert into the Pi Zero 2 W (note: PWR IN port for power).
     Wire the 3.3V USB-TTL cable to the Pi GPIO header:
       Pi pin  6  (GND)        <->  cable GND
       Pi pin  8  (GPIO 14 TX) -->  cable RX
       Pi pin 10  (GPIO 15 RX) <--  cable TX
     Leave the cable's 5V/VCC pin disconnected.
     Open serial at 115200 8N1, then power on the Pi.
  4. Expect serial output from kernel8.img ($kernel_bin) on the wire.
     For pi-probe: a few-line banner with CurrentEL / MIDR_EL1 / MPIDR_EL1.
     For newton-hypervisor: the M0 banner, capability dump, MMU init,
       stage-2 init, and progress into the Newton ROM boot. Use --features
       trace,quiet for a function-level trace of the ROM execution.

  See docs/REAL_HW_BRINGUP.md if it doesn't print.
EOF
