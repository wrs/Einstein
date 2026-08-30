#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyserial", "numpy"]
# ///
"""Host side of the Pi Zero 2 W serial image loader (nhboot).

Builds the HYPERV.IMG container the bootloader expects, power-cycles
the board (HomeKit switch via the "Pi Off" / "Pi On" Shortcuts),
uploads a new hypervisor image over the USB-TTL cable while nhboot is
in its handshake window, then captures the console. See
docs/REAL_HW_BRINGUP.md, "Serial image upload", and nhboot/src/xfer.rs
for the protocol this mirrors.

    scripts/pi-upload.py --kernel ELF [--until REGEX] [--timeout SEC]
        Power-cycle, upload, boot, capture the console.
    scripts/pi-upload.py --no-upload [--until REGEX]
        Power-cycle and capture only.
    scripts/pi-upload.py --make-image OUT (--kernel ELF | --payload BIN)
        Wrap a hypervisor image in the container (for build-sd.sh /
        first-time card setup).
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import re
import socket
import struct
import subprocess
import sys
import time
import zlib
from dataclasses import dataclass
from pathlib import Path

WORKDIR = Path("/tmp/newton-claude/nhboot")
DEFAULT_PORT = "/dev/cu.usbserial-BG03U2PN"
DEFAULT_LOG = Path("/tmp/newton-claude/serial.log")
CONSOLE_BAUD = 115_200
DEFAULT_XFER_BAUD = 1_500_000
WRITE_TIMEOUT = 30.0


# ---------------------------------------------------------------- image

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

    def check_payload(self, payload: bytes) -> None:
        if len(payload) == 0:
            raise ValueError("empty payload")
        if len(payload) > self.max_payload:
            raise ValueError(
                f"payload is {len(payload)} bytes; the container holds at most "
                f"{self.max_payload} (FILE_SIZE - HDR_SIZE in nhboot/src/image.rs)"
            )

    def wrap(self, payload: bytes) -> bytes:
        self.check_payload(payload)
        head = struct.pack(self.hdr_struct, self.magic, len(payload), zlib.crc32(payload))
        head += struct.pack("<I", zlib.crc32(head))
        head += b"\0" * (self.hdr_size - len(head))
        body = payload + b"\0" * (self.max_payload - len(payload))
        img = head + body
        assert len(img) == self.file_size
        return img

    def unwrap(self, img: bytes) -> bytes:
        """Payload of a container, verifying both CRCs."""
        magic, length, crc = struct.unpack_from(self.hdr_struct, img, 0)
        if magic != self.magic:
            raise ValueError("not a HYPERV.IMG container (bad magic)")
        (hdr_crc,) = struct.unpack_from("<I", img, 16)
        if zlib.crc32(img[:16]) != hdr_crc:
            raise ValueError("container header CRC mismatch")
        payload = img[self.hdr_size : self.hdr_size + length]
        if len(payload) != length or zlib.crc32(payload) != crc:
            raise ValueError("container payload CRC mismatch")
        return payload


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
    out.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([str(objcopy), "-O", "binary", str(elf), str(out)], check=True)
    return out.read_bytes()


def load_payload(args: argparse.Namespace, workdir: Path) -> bytes:
    if args.kernel:
        return elf_to_binary(Path(args.kernel), workdir / "kernel8.img")
    if args.payload:
        return Path(args.payload).read_bytes()
    return FORMAT.unwrap(Path(args.image).read_bytes())


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


# ----------------------------------------------------------------- link

class LinkError(Exception):
    pass


class Link:
    """A byte pipe to nhboot: a pyserial port, or (for QEMU) a unix
    stream socket (`unix:/path`, QEMU `-serial unix:…,server`), on
    which baud changes are no-ops."""

    def __init__(self, port: str, baud: int) -> None:
        self.port = port
        self.baud = baud
        self.unread_buf = b""
        if port.startswith("unix:"):
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.sock.connect(port[len("unix:") :])
            self.ser = None
        else:
            import serial  # pyserial

            self.sock = None
            self.ser = serial.Serial(port, baud, timeout=0.1, write_timeout=5)

    def set_baud(self, baud: int) -> None:
        self.baud = baud
        if self.ser is not None:
            self.ser.baudrate = baud

    def write(self, data: bytes) -> None:
        if self.ser is not None:
            self.ser.write(data)
            self.ser.flush()
        else:
            # A 64 KiB message only drains as fast as the emulated
            # UART is polled; the read timeouts below are far too short
            # for that, so give writes their own generous bound.
            self.sock.settimeout(WRITE_TIMEOUT)
            self.sock.sendall(data)

    def unread(self, data: bytes) -> None:
        """Push bytes back so the next `read` returns them first."""
        self.unread_buf = data + self.unread_buf

    def read(self, n: int, timeout: float) -> bytes:
        """Up to `n` bytes, returning as soon as any arrive or the
        timeout passes (empty bytes on timeout)."""
        if self.unread_buf:
            data, self.unread_buf = self.unread_buf[:n], self.unread_buf[n:]
            return data
        if self.ser is not None:
            self.ser.timeout = timeout
            data = self.ser.read(1)
            if data and n > 1:
                self.ser.timeout = 0
                data += self.ser.read(n - 1)
            return data
        self.sock.settimeout(timeout)
        try:
            data = self.sock.recv(n)
        except socket.timeout:
            return b""
        if data == b"":
            raise LinkError("peer closed the socket (QEMU exited?)")
        return data

    def read_exact(self, n: int, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        buf = bytearray()
        while len(buf) < n:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"needed {n} bytes, got {len(buf)} in {timeout:.1f}s")
            buf += self.read(n - len(buf), min(remaining, 0.5))
        return bytes(buf)


# -------------------------------------------------------------- console

class Console:
    """Everything the board prints goes here: stdout (decoded) and the
    log file (raw)."""

    def __init__(self, log_path: Path) -> None:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        self.log = open(log_path, "ab", buffering=0)
        stamp = _dt.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        self.log.write(f"\n===== pi-upload.py run {stamp} =====\n".encode())
        self.pending = b""

    def feed(self, data: bytes) -> list[str]:
        """Record `data`; return the complete lines it finished."""
        if not data:
            return []
        self.log.write(data)
        sys.stdout.write(data.decode("utf-8", "replace"))
        sys.stdout.flush()
        self.pending += data
        *lines, self.pending = self.pending.split(b"\n")
        return [ln.decode("utf-8", "replace").rstrip("\r") for ln in lines]


def wait_for(link: Link, console: Console, pattern: bytes, timeout: float,
             tick=None) -> bool:
    """Stream the console until `pattern` appears in the byte stream
    (or `timeout`). `tick()` is called ~10×/s (handshake spam). Bytes
    that arrived after the match are pushed back into the link, so a
    protocol reply that follows the pattern is not lost."""
    deadline = time.monotonic() + timeout
    window = b""
    while time.monotonic() < deadline:
        data = link.read(4096, 0.1)
        window = (window + data)[-(len(pattern) + 4096) :]
        i = window.find(pattern)
        if i >= 0:
            after = window[i + len(pattern) :]
            console.feed(data[: len(data) - len(after)])
            link.unread(after)
            return True
        console.feed(data)
        if tick is not None:
            tick()
    return False


# ---------------------------------------------------------------- power

def shortcut(name: str) -> None:
    # stdin must be closed: `shortcuts run` otherwise waits on it
    # forever and the switch never toggles.
    r = subprocess.run(["shortcuts", "run", name], stdin=subprocess.DEVNULL,
                       capture_output=True, text=True, timeout=20)
    if r.returncode != 0:
        print(f"warning: shortcuts run {name!r} exited {r.returncode}: {r.stderr.strip()}",
              file=sys.stderr)


FIRMWARE_BANNER = b"Raspberry Pi Bootcode"


def power_cycle(link: Link, console: Console) -> None:
    """Off, on, and — because the Shortcut's exit status says nothing
    about whether the Home action happened — insist on the firmware's
    `uart_2ndstage` banner before believing the board rebooted."""
    for attempt in (1, 2):
        print(f"power: off/on (attempt {attempt})", flush=True)
        shortcut("Pi Off")
        time.sleep(2)
        shortcut("Pi On")
        if wait_for(link, console, FIRMWARE_BANNER, 10):
            return
        print("power: no firmware banner within 10 s", flush=True)
    sys.exit("error: the Pi did not reboot — the HomeKit switch did not cycle "
             "(check the 'Pi Off'/'Pi On' Shortcuts and the Home app).")


# ------------------------------------------------------------- protocol
# Mirrors nhboot/src/xfer.rs — change both together.

HANDSHAKE_PERIOD = 0.1
TAG_TABLE, TAG_DATA, TAG_COPY, TAG_COMMIT, TAG_ACK, TAG_NAK = (bytes([c]) for c in b"TDCKAN")
NAK_REASONS = {1: "bad crc", 2: "bad offset/len", 3: "rx timeout", 4: "no old image",
               5: "unknown tag"}
DATA_CHUNK = 65_536
MAX_RETRIES = 3
ACK_TIMEOUT = 10.0


class ProtocolError(Exception):
    pass


def handshake(link: Link, console: Console, baud: int, timeout: float) -> None:
    hello = f"\x01NHUP {baud}\n".encode()
    last = [0.0]

    def tick() -> None:
        if time.monotonic() - last[0] >= HANDSHAKE_PERIOD:
            link.write(hello)
            last[0] = time.monotonic()

    tick()
    if not wait_for(link, console, f"NHUP-OK {baud}\r\n".encode(), timeout, tick):
        raise ProtocolError(
            "nhboot did not answer the handshake — is the card running nhboot as "
            "kernel8.img, and did the board actually reboot? Power-cycle and retry.")
    # Let nhboot's reply drain at the old rate before we switch.
    time.sleep(0.05)
    link.set_baud(baud)


def read_table(link: Link) -> list[tuple[int, int]]:
    tag = link.read_exact(1, 5.0)
    if tag != TAG_TABLE:
        raise ProtocolError(f"expected TABLE after the baud switch, got {tag!r} "
                            "(baud mismatch? power-cycle and retry, perhaps with a lower --baud)")
    (n,) = struct.unpack("<I", link.read_exact(4, 5.0))
    raw = link.read_exact(8 * n, 30.0)
    (crc,) = struct.unpack("<I", link.read_exact(4, 5.0))
    if zlib.crc32(raw) != crc:
        raise ProtocolError("TABLE crc mismatch")
    return [struct.unpack_from("<II", raw, 8 * i) for i in range(n)]


def recv_ack(link: Link) -> tuple[bool, int, int]:
    """(acked, echo, nak_reason)."""
    tag = link.read_exact(1, ACK_TIMEOUT)
    if tag == TAG_ACK:
        (echo,) = struct.unpack("<I", link.read_exact(4, ACK_TIMEOUT))
        return True, echo, 0
    if tag == TAG_NAK:
        echo, reason = struct.unpack("<IB", link.read_exact(5, ACK_TIMEOUT))
        return False, echo, reason
    raise ProtocolError(f"unexpected byte {tag!r} while waiting for ACK/NAK")


def send_msg(link: Link, tag: bytes, header: bytes, data: bytes, echo: int,
             what: str, corrupt: bool = False) -> None:
    """Stop-and-wait with retries. `corrupt` flips one data byte on the
    first attempt (fault injection for --debug-corrupt)."""
    for attempt in range(1, MAX_RETRIES + 2):
        body = data
        if corrupt and attempt == 1:
            body = bytes([data[0] ^ 0x55]) + data[1:]
            print(f"xfer: corrupting {what} on purpose", flush=True)
        link.write(tag + header + body)
        try:
            ok, got, reason = recv_ack(link)
        except TimeoutError as e:
            print(f"xfer: {what}: no reply ({e}), retrying", flush=True)
            continue
        if ok and got == echo:
            return
        if ok:
            raise ProtocolError(f"{what}: ACK for {got:#x}, expected {echo:#x} — link desynced")
        why = NAK_REASONS.get(reason, str(reason))
        if reason in (2, 4, 5):
            raise ProtocolError(f"{what}: NAK ({why}) — not retryable")
        print(f"xfer: {what}: NAK ({why}), retry {attempt}/{MAX_RETRIES}", flush=True)
    raise ProtocolError(f"{what}: gave up after {MAX_RETRIES} retries")


def upload(link: Link, console: Console, payload: bytes, baud: int,
           corrupt_msg: int = 0) -> None:
    FORMAT.check_payload(payload)
    t0 = time.monotonic()
    handshake(link, console, baud, timeout=30)
    table = read_table(link)
    print(f"xfer: handshake ok at {baud} baud; old image table has {len(table)} blocks",
          flush=True)
    t1 = time.monotonic()
    n = 0
    for off in range(0, len(payload), DATA_CHUNK):
        chunk = payload[off : off + DATA_CHUNK]
        n += 1
        if off and off % (1 << 20) == 0:
            print(f"xfer: {off >> 10} KiB sent, {time.monotonic() - t1:.1f} s", flush=True)
        send_msg(link, TAG_DATA, struct.pack("<III", off, len(chunk), zlib.crc32(chunk)),
                 chunk, off, f"DATA #{n} @{off:#x}", corrupt=(n == corrupt_msg))
    send_msg(link, TAG_COMMIT, struct.pack("<II", len(payload), zlib.crc32(payload)), b"",
             len(payload), "COMMIT")
    t2 = time.monotonic()
    if not wait_for(link, console, b"DONE", 90):
        raise ProtocolError("no DONE after COMMIT")
    link.set_baud(CONSOLE_BAUD)
    console.pending = b""
    dt = t2 - t1
    print(f"\nxfer: {len(payload)} bytes in {n} messages, {dt:.1f} s "
          f"({len(payload) / dt / 1024:.0f} KiB/s), {t2 - t0:.1f} s incl. handshake",
          flush=True)


# -------------------------------------------------------------- capture

def capture(link: Link, console: Console, until: str | None, timeout: float) -> int:
    pat = re.compile(until) if until else None
    deadline = time.monotonic() + timeout if timeout > 0 else None
    while deadline is None or time.monotonic() < deadline:
        for line in console.feed(link.read(4096, 0.2)):
            if pat is not None and pat.search(line):
                print(f"\npi-upload: matched /{until}/", flush=True)
                return 0
    print(f"\npi-upload: timeout after {timeout:.0f} s", flush=True)
    return 1


# ----------------------------------------------------------------- main

def main() -> int:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    src = p.add_argument_group("image source (one of)")
    src.add_argument("--kernel", metavar="ELF", help="hypervisor ELF; objcopy'd to a raw image")
    src.add_argument("--payload", metavar="BIN", help="already-raw hypervisor image")
    src.add_argument("--image", metavar="IMG", help="a HYPERV.IMG container")
    p.add_argument("--make-image", metavar="OUT",
                   help="write the HYPERV.IMG container to OUT and exit")
    link_g = p.add_argument_group("link")
    link_g.add_argument("--port", default=DEFAULT_PORT,
                        help=f"serial device, or unix:/path for a QEMU socket (default {DEFAULT_PORT})")
    link_g.add_argument("--baud", type=int, default=DEFAULT_XFER_BAUD,
                        help=f"transfer baud (default {DEFAULT_XFER_BAUD}; 3000000 is the ceiling)")
    run = p.add_argument_group("run")
    run.add_argument("--no-power-cycle", action="store_true",
                     help="don't toggle the HomeKit switch first")
    run.add_argument("--no-upload", action="store_true",
                     help="just (power-cycle and) capture the console")
    run.add_argument("--until", metavar="REGEX",
                     help="exit 0 when a console line matches")
    run.add_argument("--timeout", type=float, default=0,
                     help="seconds of capture before exiting 1 (0 = forever)")
    run.add_argument("--log", type=Path, default=DEFAULT_LOG,
                     help=f"append the raw console here (default {DEFAULT_LOG})")
    p.add_argument("--debug-corrupt", type=int, default=0, metavar="N", help=argparse.SUPPRESS)
    args = p.parse_args()

    sources = [s for s in (args.kernel, args.payload, args.image) if s]
    if args.make_image:
        if len(sources) != 1:
            p.error("--make-image needs exactly one of --kernel / --payload / --image")
        return cmd_make_image(args)
    if args.no_upload:
        if sources:
            p.error("--no-upload takes no image source")
        payload = None
    else:
        if len(sources) != 1:
            p.error("exactly one of --kernel / --payload / --image is required")
        payload = load_payload(args, WORKDIR)
        print(f"image: {len(payload)} bytes, crc32 {zlib.crc32(payload):08x}", flush=True)

    console = Console(args.log)
    try:
        link = Link(args.port, CONSOLE_BAUD)
    except Exception as e:  # noqa: BLE001 — report and exit, whatever pyserial raised
        sys.exit(f"error: cannot open {args.port}: {e}\n"
                 "hint: is another serial terminal (miniterm/screen) holding it?")
    try:
        if not args.no_power_cycle:
            power_cycle(link, console)
        if payload is not None:
            upload(link, console, payload, args.baud, corrupt_msg=args.debug_corrupt)
        return capture(link, console, args.until, args.timeout)
    except (ProtocolError, LinkError, TimeoutError, OSError) as e:
        print(f"\npi-upload: error: {e}", file=sys.stderr, flush=True)
        if os.environ.get("PI_UPLOAD_DEBUG"):
            import traceback

            traceback.print_exc()
        return 1
    except KeyboardInterrupt:
        print("\npi-upload: interrupted", flush=True)
        return 130


if __name__ == "__main__":
    sys.exit(main())
