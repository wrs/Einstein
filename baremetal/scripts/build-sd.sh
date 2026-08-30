#!/usr/bin/env bash
#
# Build a Pi Zero 2 W boot-partition layout from the current source tree.
#
# Usage:
#   scripts/build-sd.sh <dest-dir>
#       Build nhboot (the bootloader that becomes kernel8.img) and the
#       hypervisor (wrapped into the HYPERV.IMG container nhboot
#       boots), fetch the Pi firmware blobs (cached under
#       target/pi-firmware-cache/), and assemble a complete boot-
#       partition layout under <dest-dir>. The user then copies the
#       contents to the root of a FAT32-formatted SD card.
#
#       This is the first-time (and firmware/config/nhboot-change)
#       path. Hypervisor rebuilds go over the serial cable instead:
#           scripts/pi-upload.py --kernel <elf>
#       (docs/REAL_HW_BRINGUP.md, "Serial image upload").
#
# Env vars:
#   PI_FIRMWARE_CACHE   override the default cache location
#   PI_KERNEL_BIN       which [[bin]] to wrap into HYPERV.IMG
#                       (default: newton-hypervisor — the full
#                       hypervisor, which boots to the Welcome UI on
#                       the Pi)
#   PI_CARGO_FEATURES   base Cargo features (default: pi-bare-metal-input,
#                       the display + touch + audio + SD-flash aggregate
#                       for the full hypervisor). Set to `pi-bare-metal`
#                       for the minimal null-backend build, or
#                       `pi-bare-metal-sd` for SD-backed flash
#                       persistence — the aggregates are mutually
#                       exclusive so PI_CARGO_FEATURES replaces, not
#                       appends.
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
              Only files whose mtime+size differ are written; the
              16 MiB HYPERV.IMG is rewritten only when its content
              changed.

  Set PI_KERNEL_BIN to pick a different hypervisor [[bin]] (default: newton-hypervisor).

  After the first card write, rebuilds of the hypervisor go over serial:
      scripts/pi-upload.py --kernel target/aarch64-unknown-none-softfloat/release/newton-hypervisor
EOF
    exit 1
}

[[ $# -eq 1 || $# -eq 2 ]] || usage
dest="$1"
sd_mount="${2:-}"

# Repo root = directory containing this script's parent.
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cache="${PI_FIRMWARE_CACHE:-${repo_root}/target/pi-firmware-cache}"
kernel_bin="${PI_KERNEL_BIN:-newton-hypervisor}"

mkdir -p "$dest" "$dest/overlays" "$cache" "$cache/overlays"

# --- 1. Fetch firmware blobs into the cache (skipped if present) -------
for f in "${FW_FILES[@]}"; do
    if [[ ! -f "$cache/$f" ]]; then
        echo "fetch: $f"
        curl -fsSLo "$cache/$f" "$FW_BASE/$f"
    fi
done

# --- 2. Build nhboot — the bootloader that is kernel8.img -------------
#
# nhboot is a standalone package (nhboot/Cargo.toml) with its own
# target pin. It self-relocates out of 0x80000, validates HYPERV.IMG
# (loaded by the firmware at 0x02000000 via the `initramfs` line in
# config.txt) and enters the hypervisor at 0x80000; with a host on
# the serial cable it receives a new image first.
echo "build: nhboot"
(
    cd "$repo_root/nhboot"
    cargo build --release
)
nhboot_elf="${repo_root}/nhboot/target/aarch64-unknown-none-softfloat/release/nhboot"

# --- 2b. Build the chosen hypervisor [[bin]] ---------------------------
#
# Default to the `pi-bare-metal-input` aggregate for the full
# hypervisor (display + touch + audio + SD-flash on top of
# platform-raspi3b + no-semihost). Set PI_CARGO_FEATURES for the
# minimal `pi-bare-metal` null-backend build instead. See Cargo.toml
# for the feature definitions.
features="${PI_CARGO_FEATURES:-pi-bare-metal-input}${PI_EXTRA_FEATURES:+ $PI_EXTRA_FEATURES}"
echo "build: cargo --release --no-default-features --features '$features' --bin $kernel_bin"
(
    cd "$repo_root"
    cargo build --release --no-default-features --features "$features" \
        --bin "$kernel_bin"
)

elf="${repo_root}/target/aarch64-unknown-none-softfloat/release/${kernel_bin}"

# --- 3. nhboot ELF → raw kernel8.img -----------------------------------
sysroot="$(rustc --print sysroot)"
objcopy="$(find "$sysroot" -name 'llvm-objcopy' -print -quit)"
if [[ -z "$objcopy" ]]; then
    echo "error: llvm-objcopy not found under $sysroot" >&2
    echo "hint: run 'rustup component add llvm-tools-preview'" >&2
    exit 1
fi
echo "objcopy: nhboot → kernel8.img"
"$objcopy" -O binary "$nhboot_elf" "$dest/kernel8.img"

# --- 3b. Hypervisor ELF → HYPERV.IMG container -------------------------
#
# pi-upload.py does the objcopy itself and wraps the raw image in the
# fixed 16 MiB container nhboot validates (header: magic, payload
# length, CRC-32s). The same script uploads over serial later.
echo "make-image: $kernel_bin → HYPERV.IMG"
"${repo_root}/scripts/pi-upload.py" --make-image "$dest/HYPERV.IMG" --kernel "$elf"

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
         (only changed files are written)
       - or rsync manually:
           rsync -a "$dest"/ /Volumes/<SD-card-name>/
  3. Eject, insert into the Pi Zero 2 W (note: PWR IN port for power).
     Wire the 3.3V USB-TTL cable to the Pi GPIO header:
       Pi pin  6  (GND)        <->  cable GND
       Pi pin  8  (GPIO 14 TX) -->  cable RX
       Pi pin 10  (GPIO 15 RX) <--  cable TX
     Leave the cable's 5V/VCC pin disconnected.
     Open serial at 115200 8N1, then power on the Pi.
  4. Expect on the wire: the firmware's uart_2ndstage lines, the
     nhboot banner ("nhboot v1 ... image: valid ..."), then the
     hypervisor ($kernel_bin): M0 banner, capability dump, MMU init,
     stage-2 init, and progress into the Newton ROM boot.
  5. From then on, rebuild and reload without touching the card:
       scripts/pi-upload.py --kernel $elf --until 'Welcome to NewtonScript' --timeout 120
     (power-cycles via the "Pi Off"/"Pi On" Shortcuts, uploads a delta
     of the image over the cable, boots it, captures the console).
     Re-run this script only for a firmware, config.txt or nhboot change.

  See docs/REAL_HW_BRINGUP.md if it doesn't print.
EOF
