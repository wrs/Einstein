#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyserial", "numpy"]
# ///
"""Host side of the Pi Zero 2 W serial image loader (nhboot).

Builds the HYPERV.IMG container the bootloader expects, and (later
phases) power-cycles the board, uploads a new hypervisor image over
the USB-TTL cable and captures the console. See
docs/REAL_HW_BRINGUP.md, "Serial image upload".

    scripts/pi-upload.py --make-image OUT (--kernel ELF | --payload BIN)
        Wrap a hypervisor image in the container (for build-sd.sh /
        first-time card setup).
"""

from __future__ import annotations

import argparse
import shutil
import struct
import subprocess
import sys
import zlib
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class ImageFormat:
    """HYPERV.IMG container layout. Mirrors nhboot/src/image.rs —
    change both together."""

    image_addr: int = 0x0200_0000  # config.txt: initramfs HYPERV.IMG 0x02000000
    hdr_size: int = 4096
    file_size: int = 16 * 1024 * 1024
    load_addr: int = 0x8_0000  # hypervisor link address
    magic: bytes = b"NHIMG001"
    # Header: magic @0, u32 payload_len @8, u32 payload_crc @12,
    # u32 hdr_crc (over bytes [0, 16)) @16, zero to hdr_size.
    hdr_struct: str = "<8sII"

    @property
    def max_payload(self) -> int:
        return self.file_size - self.hdr_size

    def wrap(self, payload: bytes) -> bytes:
        if len(payload) == 0:
            raise ValueError("empty payload")
        if len(payload) > self.max_payload:
            raise ValueError(
                f"payload is {len(payload)} bytes; the container holds at most "
                f"{self.max_payload} (FILE_SIZE - HDR_SIZE in nhboot/src/image.rs)"
            )
        head = struct.pack(self.hdr_struct, self.magic, len(payload), zlib.crc32(payload))
        head += struct.pack("<I", zlib.crc32(head))
        head += b"\0" * (self.hdr_size - len(head))
        body = payload + b"\0" * (self.max_payload - len(payload))
        img = head + body
        assert len(img) == self.file_size
        return img


FORMAT = ImageFormat()


def find_llvm_objcopy() -> Path:
    """Same lookup as scripts/run-qemu.sh: the llvm-tools-preview
    component inside the active toolchain's sysroot."""
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"], check=True, capture_output=True, text=True
    ).stdout.strip()
    hits = sorted(Path(sysroot).rglob("llvm-objcopy"))
    if not hits:
        sys.exit(
            f"error: llvm-objcopy not found under {sysroot}\n"
            "hint: run 'rustup component add llvm-tools-preview'"
        )
    return hits[0]


def elf_to_binary(elf: Path, out: Path) -> bytes:
    objcopy = find_llvm_objcopy()
    subprocess.run([str(objcopy), "-O", "binary", str(elf), str(out)], check=True)
    return out.read_bytes()


def load_payload(args: argparse.Namespace, workdir: Path) -> bytes:
    if args.kernel:
        return elf_to_binary(Path(args.kernel), workdir / "kernel8.img")
    return Path(args.payload).read_bytes()


def cmd_make_image(args: argparse.Namespace) -> int:
    out = Path(args.make_image)
    out.parent.mkdir(parents=True, exist_ok=True)
    payload = load_payload(args, out.parent)
    img = FORMAT.wrap(payload)
    tmp = out.with_suffix(out.suffix + ".tmp")
    tmp.write_bytes(img)
    tmp.replace(out)
    print(
        f"make-image: {out} ({len(img)} bytes): payload {len(payload)} bytes, "
        f"crc32 {zlib.crc32(payload):08x}"
    )
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    src = p.add_argument_group("image source (one of)")
    src.add_argument("--kernel", metavar="ELF", help="hypervisor ELF; objcopy'd to a raw image")
    src.add_argument("--payload", metavar="BIN", help="already-raw hypervisor image")
    p.add_argument("--make-image", metavar="OUT", help="write the HYPERV.IMG container to OUT and exit")
    # Later phases add: --port, --baud, --no-power-cycle, --no-upload,
    # --until, --timeout, --log (upload + console capture).
    args = p.parse_args()

    if bool(args.kernel) == bool(args.payload):
        p.error("exactly one of --kernel / --payload is required")
    if args.make_image:
        return cmd_make_image(args)
    p.error("nothing to do: --make-image is the only mode implemented so far")
    return 2


if __name__ == "__main__":
    sys.exit(main())
