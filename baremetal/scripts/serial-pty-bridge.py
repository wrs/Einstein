#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Bridge the guest's external serial port to a pty or TCP endpoint.

On the semihost host-io backend (`host-io-semihost`, QEMU and FVP) the
hypervisor moves the Newton's `extr` serial bytes through a file pair:

    /tmp/newton-host-io/serial-out   guest -> host (append-only)
    /tmp/newton-host-io/serial-in    host -> guest (append-only,
                                     tailed by the hypervisor)

This script tails `serial-out` and appends to `serial-in`, exposing the
byte stream as either

    a pty (default) — for tools that want a serial device:
        scripts/serial-pty-bridge.py            # /tmp/newton-host-io/extr.pty
        unixnpi -d /tmp/newton-host-io/extr.pty pkg.pkg

    an outbound TCP connection — e.g. NTK inside BasiliskII listening
    with `seriala tcp:3679`:
        scripts/serial-pty-bridge.py --connect 127.0.0.1:3679

The hypervisor truncates `serial-out` at boot; the bridge starts
reading at its current end so a mid-session (re)start doesn't replay
old traffic. Kill the bridge with Ctrl-C / SIGTERM; the pty symlink is
removed on exit.
"""

import argparse
import os
import select
import socket
import sys
import time
import tty

DIR = "/tmp/newton-host-io"
OUT_PATH = f"{DIR}/serial-out"   # guest -> host
IN_PATH = f"{DIR}/serial-in"     # host -> guest
POLL_S = 0.01


def open_endpoint(args):
    """Return (read_fd, write_fd, cleanup) for the tool-facing side."""
    if args.connect:
        host, _, port = args.connect.rpartition(":")
        sock = socket.create_connection((host or "127.0.0.1", int(port)))
        sock.setblocking(False)
        print(f"bridge: connected to tcp {host}:{port}", flush=True)
        fd = sock.fileno()
        return fd, fd, lambda: sock.close()

    master, slave = os.openpty()
    # Raw mode, ECHO off. The default line discipline echoes every byte
    # written into the master (the "keyboard" side) back to the master —
    # which here would bounce every Newton-bound byte straight back
    # into the guest and garble the MNP stream.
    tty.setraw(slave)
    slave_name = os.ttyname(slave)
    if os.path.islink(args.link) or os.path.exists(args.link):
        os.unlink(args.link)
    os.symlink(slave_name, args.link)
    print(f"bridge: {args.link} -> {slave_name}", flush=True)
    return master, master, lambda: os.unlink(args.link)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--link", default=f"{DIR}/extr.pty",
                    help="pty symlink path (pty mode)")
    ap.add_argument("--connect", metavar="HOST:PORT",
                    help="connect out via TCP instead of creating a pty")
    ap.add_argument(
        "--capture", metavar="PREFIX",
        help="also write raw traffic to PREFIX-fromnewton.bin / PREFIX-tonewton.bin")
    args = ap.parse_args()

    os.makedirs(DIR, exist_ok=True)
    for p in (OUT_PATH, IN_PATH):
        open(p, "ab").close()

    cap_from = cap_to = None
    if args.capture:
        cap_from = open(f"{args.capture}-fromnewton.bin", "wb")
        cap_to = open(f"{args.capture}-tonewton.bin", "wb")

    rfd, wfd, cleanup = open_endpoint(args)
    out_f = open(OUT_PATH, "rb")
    out_f.seek(0, os.SEEK_END)
    in_f = open(IN_PATH, "ab")

    try:
        while True:
            # guest -> tool: tail serial-out (detect boot-time truncate)
            pos = out_f.tell()
            end = os.fstat(out_f.fileno()).st_size
            if end < pos:
                out_f.seek(0)
            data = out_f.read(4096)
            if data:
                n = 0
                while n < len(data):
                    try:
                        n += os.write(wfd, data[n:])
                    except BlockingIOError:
                        time.sleep(POLL_S)
                if cap_from:
                    cap_from.write(data)
                    cap_from.flush()

            # tool -> guest: append to serial-in
            r, _, _ = select.select([rfd], [], [], POLL_S)
            if rfd in r:
                try:
                    chunk = os.read(rfd, 4096)
                except (BlockingIOError, OSError):
                    # pty master with no slave attached raises EIO /
                    # reads empty; that's "tool not (re)connected yet",
                    # not an error.
                    chunk = None
                    time.sleep(POLL_S)
                if chunk == b"":
                    if args.connect:
                        print("bridge: endpoint closed", flush=True)
                        break
                    chunk = None
                    time.sleep(POLL_S)
                if chunk:
                    in_f.write(chunk)
                    in_f.flush()
                    if cap_to:
                        cap_to.write(chunk)
                        cap_to.flush()
    except KeyboardInterrupt:
        pass
    finally:
        cleanup()


if __name__ == "__main__":
    sys.exit(main())
