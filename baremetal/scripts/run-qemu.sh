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
# the mini-UART. The PL011 is the guest's external serial port (`extr`);
# the debug log goes out over semihosting, so redirecting the PL011
# chardev exposes the Newton's serial wire to host tools without
# touching the log. NH_SERIAL0 overrides the chardev spec:
#   NH_SERIAL0=pty                        pty for NCX / UnixNPI / minicom
#   NH_SERIAL0=tcp:127.0.0.1:3679        client, e.g. NTK in BasiliskII
#                                         (seriala tcp:3679)
#   NH_SERIAL0=telnet:127.0.0.1:5556,server,nowait   ad-hoc poking
# Default keeps the historic behaviour (serial + monitor muxed on stdio,
# which boot-check.sh and the guest-test harness grep).
serial0="${NH_SERIAL0:-mon:stdio}"

# Semihosting is enabled so the hypervisor can save/load snapshots via
# HLT #0xF000 (see src/hv/snapshot.rs). target=native makes the hypervisor
# itself own the semihosting surface; paths are resolved against the
# host's cwd.
exec qemu-system-aarch64 \
    -M raspi3b \
    -kernel "$img" \
    -serial "$serial0" \
    -display none \
    -no-reboot \
    -semihosting-config enable=on,target=native \
    ${debug_args[@]+"${debug_args[@]}"} \
    ${QEMU_EXTRA:-} \
    "$@"
