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


# A real-hardware hypervisor build embeds the ~8.3 MiB Newton ROM +
# REx blob at a fixed 4 KiB image offset (linker.ld.in `.rom_blob`).
# A default/QEMU-features build loads the ROM via semihosting instead
# and objcopy's to ~1 MiB — on the Pi it hangs silently before the
# first UART byte. That exact mistake (some other cargo invocation —
# guest tests, boot-check, plain `cargo run` — replacing
# target/.../release/newton-hypervisor between the pi build and the
# upload) has shipped the wrong binary repeatedly, so refuse payloads
# that can't possibly contain the blob.
MIN_REAL_HW_PAYLOAD = 4 << 20


def check_payload_shape(payload: bytes, args: argparse.Namespace) -> None:
    if args.allow_small_payload or len(payload) >= MIN_REAL_HW_PAYLOAD:
        return
    sys.exit(
        f"error: payload is {len(payload)} bytes — too small to contain the "
        f"pinned ROM/REx blob, so this is not a real-hardware build.\n"
        f"hint: the artifact was probably replaced by a QEMU-features build "
        f"(guest tests, boot-check.sh, or plain `cargo run`). Rebuild with:\n"
        f"  cargo build --release --no-default-features --features pi-bare-metal-input\n"
        f"and re-run. (--allow-small-payload overrides, e.g. for nhboot tests.)")


def load_payload(args: argparse.Namespace, workdir: Path) -> bytes:
    if args.kernel:
        payload = elf_to_binary(Path(args.kernel), workdir / "kernel8.img")
    elif args.payload:
        payload = Path(args.payload).read_bytes()
    else:
        payload = FORMAT.unwrap(Path(args.image).read_bytes())
    check_payload_shape(payload, args)
    return payload


def cmd_make_image(args: argparse.Namespace) -> int:
    out = Path(args.make_image)
    out.parent.mkdir(parents=True, exist_ok=True)
    # The objcopy intermediate goes to the scratch dir, not next to OUT:
    # build-sd.sh's OUT dir already holds nhboot as kernel8.img.
    payload = load_payload(args, WORKDIR)
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

    POLL = 0.02  # pyserial read timeout, set once at open (see __init__)

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
            # The read timeout is fixed here and never reassigned:
            # every pyserial `timeout` assignment re-runs tcsetattr,
            # and on macOS's FTDI driver that re-programs a
            # non-standard speed and discards buffered RX — at 1.5 M
            # it lost most of nhboot's TABLE. `read` polls in
            # POLL-second slices instead.
            self.ser = serial.Serial(port, baud, timeout=self.POLL, write_timeout=5)

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
            deadline = time.monotonic() + timeout
            while True:
                data = self.ser.read(1)
                if data:
                    if n > 1:
                        pending = min(self.ser.in_waiting, n - 1)
                        if pending:
                            data += self.ser.read(pending)
                    return data
                if time.monotonic() >= deadline:
                    return b""
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
# Also covers a COPY of the whole ~10 MiB payload: an uncached memcpy
# on the A53 takes ~0.3 s.
ACK_TIMEOUT = 10.0
# Block size of nhboot's TABLE (`TABLE_BLOCK` in xfer.rs): the old
# payload is fingerprinted per full block, and the delta search slides
# a window of the same size over the new payload.
TABLE_BLOCK = 4096
ADLER_MOD = 65_521
# After the COMMIT ACK nhboot writes the card: a first-time create of the
# 16 MiB file through the FAT layer runs at PIO speed (~700 KB/s → ~25 s);
# an in-place rewrite of the changed sectors is a few seconds. Leave a
# wide margin — a slow card must not be mistaken for a hang.
DONE_TIMEOUT = 300.0


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
        body, head = data, header
        if corrupt and attempt == 1:
            # A DATA message: flip a payload byte. A COPY: flip a
            # header byte (it has no payload; its header CRC must
            # catch this).
            if data:
                body = bytes([data[0] ^ 0x55]) + data[1:]
            else:
                head = bytes([header[0] ^ 0x55]) + header[1:]
            print(f"xfer: corrupting {what} on purpose", flush=True)
        link.write(tag + head + body)
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


# ---------------------------------------------------------------- delta
# rsync in miniature. nhboot's TABLE lists {adler32, crc32} for every
# full TABLE_BLOCK of the image it already holds. We evaluate the same
# adler32 for the window starting at *every byte offset* of the new
# payload (vectorised prefix sums — a pure-Python walk over 10 MiB is
# far too slow), verify the candidates the weak hash proposes with
# crc32, and turn the matches into COPY runs; whatever is left is sent
# as DATA. Offset independence is the point: the 8 MiB ROM+REx blob
# inside the hypervisor image shifts whenever the code before it
# grows, and would otherwise be re-sent every build.

@dataclass(frozen=True)
class Copy:
    new_off: int
    old_off: int
    len: int


@dataclass(frozen=True)
class Data:
    new_off: int
    len: int


def window_adler32(new: bytes, block: int):
    """adler32 of new[p:p+block] for every p in [0, len-block], as a
    uint32 numpy array. With x the bytes as int64:
        a(p) = 1 + Σ x[p..p+B)
        b(p) = B + Σ_{i<B} (B-i)·x[p+i]
             = B + (p+B)·Σ x[p..p+B) - Σ k·x[k] for k in [p, p+B)
    int64 is safe: 255·4096·(10·2^20) < 2^63."""
    import numpy as np

    x = np.frombuffer(new, dtype=np.uint8).astype(np.int64)
    n = len(x)
    if n < block:
        return np.zeros(0, dtype=np.uint32)
    s1 = np.concatenate(([0], np.cumsum(x)))
    s2 = np.concatenate(([0], np.cumsum(np.arange(n, dtype=np.int64) * x)))
    p = np.arange(n - block + 1, dtype=np.int64)
    win_sum = s1[p + block] - s1[p]
    win_ksum = s2[p + block] - s2[p]
    a = (1 + win_sum) % ADLER_MOD
    b = (block + (p + block) * win_sum - win_ksum) % ADLER_MOD
    return ((b << 16) | a).astype(np.uint32)


def check_window_adler32(new: bytes, count: int = 5) -> None:
    """Spot-check the vectorised adler32 against zlib on random windows
    (cheap; runs once per upload so a numpy dtype slip can't silently
    turn every block into a DATA op)."""
    import random

    if len(new) < TABLE_BLOCK:
        return
    ad = window_adler32(new, TABLE_BLOCK)
    for p in random.sample(range(len(new) - TABLE_BLOCK + 1), min(count, len(ad))):
        want = zlib.adler32(new[p : p + TABLE_BLOCK])
        if int(ad[p]) != want:
            raise AssertionError(f"window_adler32 mismatch at {p}: {ad[p]:#x} != {want:#x}")


def compute_delta(new: bytes, table: list[tuple[int, int]]) -> list[Copy | Data]:
    """COPY/DATA ops that rebuild `new` from the old payload `table`
    describes. Greedy: at a verified block match, extend the COPY while
    the next window matches the next old block; otherwise the byte is
    DATA and the walk moves on to the next candidate position."""
    import numpy as np

    b = TABLE_BLOCK
    n = len(new)
    ops: list[Copy | Data] = []
    if not table or n < b:
        return [Data(off, min(DATA_CHUNK, n - off)) for off in range(0, n, DATA_CHUNK)]

    by_adler: dict[int, list[tuple[int, int]]] = {}
    for j, (ad, crc) in enumerate(table):
        by_adler.setdefault(ad, []).append((j, crc))
    ad_all = window_adler32(new, b)
    candidate = np.isin(ad_all, np.fromiter(by_adler.keys(), dtype=np.uint32))
    cand_pos = np.flatnonzero(candidate)  # ascending window starts

    def match_at(p: int) -> int | None:
        """Old block index whose crc verifies at new offset p, or None."""
        if p + b > n or not candidate[p]:
            return None
        crc = zlib.crc32(new[p : p + b])
        for j, c in by_adler[int(ad_all[p])]:
            if c == crc:
                return j
        return None

    def matches_block(p: int, j: int) -> bool:
        """Does old block j (specifically — an image has many identical
        zero blocks, and `match_at` would name the first of them) sit
        at new offset p?"""
        if j >= len(table) or p + b > n or not candidate[p]:
            return False
        ad, crc = table[j]
        return int(ad_all[p]) == ad and zlib.crc32(new[p : p + b]) == crc

    def flush_data(start: int, end: int) -> None:
        for off in range(start, end, DATA_CHUNK):
            ops.append(Data(off, min(DATA_CHUNK, end - off)))

    p = 0
    data_start = 0
    ci = 0  # index into cand_pos
    while p + b <= n:
        # Skip to the next candidate at or after p.
        ci = int(np.searchsorted(cand_pos, p))
        if ci >= len(cand_pos):
            break
        q = int(cand_pos[ci])
        j = match_at(q)
        if j is None:
            p = q + 1
            continue
        # A verified match at q for old block j: extend.
        run_start, run_j, run_blocks = q, j, 1
        while matches_block(q + run_blocks * b, run_j + run_blocks):
            run_blocks += 1
        if run_start > data_start:
            flush_data(data_start, run_start)
        ops.append(Copy(run_start, run_j * b, run_blocks * b))
        p = run_start + run_blocks * b
        data_start = p
    if data_start < n:
        flush_data(data_start, n)
    return ops


def upload(link: Link, console: Console, payload: bytes, baud: int,
           corrupt_msg: int = 0, full: bool = False) -> None:
    FORMAT.check_payload(payload)
    t0 = time.monotonic()
    handshake(link, console, baud, timeout=30)
    table = read_table(link)
    print(f"xfer: handshake ok at {baud} baud; old image table has {len(table)} blocks",
          flush=True)
    if full or not table:
        ops: list[Copy | Data] = [Data(off, min(DATA_CHUNK, len(payload) - off))
                                  for off in range(0, len(payload), DATA_CHUNK)]
    else:
        td = time.monotonic()
        check_window_adler32(payload)
        ops = compute_delta(payload, table)
        td = time.monotonic() - td
        copied = sum(op.len for op in ops if isinstance(op, Copy))
        sent = sum(op.len for op in ops if isinstance(op, Data))
        print(f"delta: {len(ops)} ops, {copied} bytes copied, {sent} bytes sent "
              f"({100 * sent / len(payload):.1f}%), computed in {td:.2f} s", flush=True)
    t1 = time.monotonic()
    sent_bytes = 0
    for n, op in enumerate(ops, 1):
        if sent_bytes and sent_bytes % (1 << 20) < DATA_CHUNK and isinstance(op, Data):
            print(f"xfer: {sent_bytes >> 10} KiB sent, {time.monotonic() - t1:.1f} s",
                  flush=True)
        if isinstance(op, Copy):
            hdr = struct.pack("<III", op.new_off, op.old_off, op.len)
            send_msg(link, TAG_COPY, hdr + struct.pack("<I", zlib.crc32(hdr)), b"",
                     op.new_off, f"COPY #{n} @{op.new_off:#x}", corrupt=(n == corrupt_msg))
        else:
            chunk = payload[op.new_off : op.new_off + op.len]
            send_msg(link, TAG_DATA,
                     struct.pack("<III", op.new_off, op.len, zlib.crc32(chunk)),
                     chunk, op.new_off, f"DATA #{n} @{op.new_off:#x}",
                     corrupt=(n == corrupt_msg))
            sent_bytes += op.len
    send_msg(link, TAG_COMMIT, struct.pack("<II", len(payload), zlib.crc32(payload)), b"",
             len(payload), "COMMIT")
    t2 = time.monotonic()
    if not wait_for(link, console, b"DONE", DONE_TIMEOUT):
        raise ProtocolError("no DONE after COMMIT")
    link.set_baud(CONSOLE_BAUD)
    console.pending = b""
    dt = t2 - t1
    print(f"\nxfer: {len(payload)} bytes as {len(ops)} messages, {sent_bytes} sent, "
          f"{dt:.1f} s ({sent_bytes / dt / 1024:.0f} KiB/s of sent bytes), "
          f"{t2 - t0:.1f} s incl. handshake", flush=True)


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
    run.add_argument("--full", action="store_true",
                     help="send every byte instead of a delta against the image on the card")
    run.add_argument("--until", metavar="REGEX",
                     help="exit 0 when a console line matches")
    run.add_argument("--timeout", type=float, default=0,
                     help="seconds of capture before exiting 1 (0 = forever)")
    run.add_argument("--log", type=Path, default=DEFAULT_LOG,
                     help=f"append the raw console here (default {DEFAULT_LOG})")
    p.add_argument("--allow-small-payload", action="store_true",
                   help="skip the pinned-ROM size sanity check (see check_payload_shape)")
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
            upload(link, console, payload, args.baud, corrupt_msg=args.debug_corrupt,
                   full=args.full)
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
