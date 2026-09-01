#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow"]
# ///
"""Headless companion for the host-io-semihost IPC files.

Does the two things the interactive viewer (tools/host-viewer) does,
but scriptably, so a guest UI flow can be driven and observed from a
shell (e.g. walking the Dock connect flow while testing the external
serial port):

  host-io-tool.py screen OUT.png [--scale N]
      Replay every BlitEvent in /tmp/newton-host-io/out into the
      320x480 2 bpp backing store and write it as a grayscale PNG.

  host-io-tool.py tap X Y [--hold MS]
      Append pen down + up PenEvents at panel coords (X, Y) to
      /tmp/newton-host-io/in. The hypervisor polls `in` every 16 ms;
      --hold (default 150 ms) keeps the pen down long enough for the
      guest to sample it.

  host-io-tool.py drag X0 Y0 X1 Y1 [--steps N] [--hold MS]
      Pen down at (X0, Y0), interpolated moves, pen up at (X1, Y1).

  host-io-tool.py power
      Send a power-switch press (wakes the guest from PowerOff).

Wire formats mirror tools/host-viewer/src/main.rs and
src/host/host_io/mod.rs: 24-byte BlitEvent headers + 2 bpp payloads
(MSB-first, 0 = white, 3 = black) in `out`; 8-byte PenEvents
(kind u8, pad, le16 x, y, pressure; 1 down / 2 move / 3 up /
4 power) in `in`.
"""

import argparse
import struct
import sys
import time
from pathlib import Path

OUT_PATH = Path("/tmp/newton-host-io/out")
IN_PATH = Path("/tmp/newton-host-io/in")

SCREEN_W, SCREEN_H = 320, 480
FB_ROW_BYTES = SCREEN_W * 2 // 8  # 80
FB_LEN = FB_ROW_BYTES * SCREEN_H

BLIT_HEADER_LEN = 24
BLIT_KIND_BLIT = 1
BLIT_KIND_FULL_REPAINT = 2

PEN_DOWN, PEN_MOVE, PEN_UP, POWER_SWITCH = 1, 2, 3, 4

# 2 bpp index -> 8-bit gray, matching the viewer's 4-gray palette.
GRAY = [0xFF, 0xAA, 0x55, 0x00]


def parse_header(buf: bytes):
    kind, _mode, _bpp, _pad = buf[0], buf[1], buf[2], buf[3]
    (sl, st, sr, sb, dl, dt, _dr, _db, row_bytes, payload_len) = struct.unpack_from(
        "<10H", buf, 4
    )
    return kind, sl, st, sr, sb, dl, dt, row_bytes, payload_len


def replay_screen() -> bytearray:
    fb = bytearray(FB_LEN)  # index 0 = white
    data = OUT_PATH.read_bytes()
    off = 0
    while off + BLIT_HEADER_LEN <= len(data):
        kind, sl, st, sr, sb, dl, dt, row_bytes, payload_len = parse_header(
            data[off : off + BLIT_HEADER_LEN]
        )
        payload = data[off + BLIT_HEADER_LEN : off + BLIT_HEADER_LEN + payload_len]
        if len(payload) < payload_len:
            break  # truncated tail (hypervisor mid-write)
        off += BLIT_HEADER_LEN + payload_len

        if kind == BLIT_KIND_FULL_REPAINT:
            n = min(len(payload), FB_LEN)
            fb[:n] = payload[:n]
            continue
        if kind != BLIT_KIND_BLIT:
            continue
        height, width = sb - st, sr - sl
        if row_bytes == 0 or height <= 0 or width <= 0:
            continue
        for row in range(height):
            src_row_off = row * row_bytes
            if src_row_off + row_bytes > len(payload):
                break
            dst_row = dt + row
            if dst_row >= SCREEN_H:
                break
            dst_row_off = dst_row * FB_ROW_BYTES
            for col in range(width):
                dst_col = dl + col
                if dst_col >= SCREEN_W:
                    break
                src_byte = payload[src_row_off + col // 4]
                val = (src_byte >> (6 - 2 * (col % 4))) & 0x3
                dst_off = dst_row_off + dst_col // 4
                dst_shift = 6 - 2 * (dst_col % 4)
                mask = 0x3 << dst_shift
                fb[dst_off] = (fb[dst_off] & ~mask) | (val << dst_shift)
    return fb


def cmd_screen(args):
    from PIL import Image

    fb = replay_screen()
    img = Image.new("L", (SCREEN_W, SCREEN_H))
    px = img.load()
    for y in range(SCREEN_H):
        row_off = y * FB_ROW_BYTES
        for x in range(SCREEN_W):
            b = fb[row_off + x // 4]
            px[x, y] = GRAY[(b >> (6 - 2 * (x % 4))) & 0x3]
    if args.scale > 1:
        img = img.resize((SCREEN_W * args.scale, SCREEN_H * args.scale), Image.NEAREST)
    img.save(args.png)
    print(f"wrote {args.png} ({img.width}x{img.height})")


def send_pen(kind: int, x: int = 0, y: int = 0, pressure: int = 0):
    rec = struct.pack("<BBHHH", kind, 0, x, y, pressure)
    with IN_PATH.open("ab") as f:
        f.write(rec)
        f.flush()


# Pen-down pressure 4 matches the viewer (and Einstein's PenDown
# default) in case the kernel thresholds it; pack_pen_sample also
# truncates pressure to 4 bits, so larger values would alias.
PEN_PRESSURE = 4


def cmd_tap(args):
    send_pen(PEN_DOWN, args.x, args.y, PEN_PRESSURE)
    for _ in range(3):
        time.sleep(args.hold / 4000.0)
        send_pen(PEN_MOVE, args.x, args.y, PEN_PRESSURE)
    time.sleep(args.hold / 4000.0)
    send_pen(PEN_UP, args.x, args.y, 0)
    print(f"tapped ({args.x}, {args.y})")


def cmd_drag(args):
    send_pen(PEN_DOWN, args.x0, args.y0, PEN_PRESSURE)
    time.sleep(args.hold / 1000.0)
    for i in range(1, args.steps + 1):
        x = args.x0 + (args.x1 - args.x0) * i // args.steps
        y = args.y0 + (args.y1 - args.y0) * i // args.steps
        send_pen(PEN_MOVE, x, y, PEN_PRESSURE)
        time.sleep(args.hold / 1000.0)
    send_pen(PEN_UP, args.x1, args.y1, 0)
    print(f"dragged ({args.x0}, {args.y0}) -> ({args.x1}, {args.y1})")


def cmd_power(_args):
    send_pen(POWER_SWITCH)
    print("power switch pressed")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("screen", help="render the blit stream to a PNG")
    s.add_argument("png")
    s.add_argument("--scale", type=int, default=1)
    s.set_defaults(func=cmd_screen)

    s = sub.add_parser("tap", help="tap panel coords")
    s.add_argument("x", type=int)
    s.add_argument("y", type=int)
    s.add_argument("--hold", type=int, default=150, help="ms between down and up")
    s.set_defaults(func=cmd_tap)

    s = sub.add_parser("drag", help="drag between panel coords")
    s.add_argument("x0", type=int)
    s.add_argument("y0", type=int)
    s.add_argument("x1", type=int)
    s.add_argument("y1", type=int)
    s.add_argument("--steps", type=int, default=8)
    s.add_argument("--hold", type=int, default=40, help="ms between events")
    s.set_defaults(func=cmd_drag)

    s = sub.add_parser("power", help="send power-switch press")
    s.set_defaults(func=cmd_power)

    args = p.parse_args()
    if args.cmd in ("tap", "drag", "power") and not IN_PATH.exists():
        sys.exit(f"{IN_PATH} missing — is the hypervisor running with host-io-semihost?")
    if args.cmd == "screen" and not OUT_PATH.exists():
        sys.exit(f"{OUT_PATH} missing — is the hypervisor running with host-io-semihost?")
    args.func(args)


if __name__ == "__main__":
    main()
