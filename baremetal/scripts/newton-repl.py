#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Interactive NewtonScript REPL against the running hypervisor.

The ROM's own read-eval-print loop does the work: input lines appended
to /tmp/newton-host-io/rep-in reach the patched
`PHammerInTranslator::ProduceFrame`, the ROM parses and evaluates
them, and the rendered output (mirrored to /tmp/newton-host-io/rep-out
by the PHammerOutTranslator patches) is printed here. Requires a
hypervisor built with `host-io-semihost` (QEMU or FVP), already booted
to the `REP> Welcome to NewtonScript!` banner.

    scripts/newton-repl.py                 # interactive
    scripts/newton-repl.py --eval '3+4'    # one-shot, prints the result

NewtonScript quickies: `GetRoot()`, `Gestalt(...)`, `foo := 42`,
`Print("hi")`. Statements need no trailing semicolon; each input line
is parsed as one expression.
"""

import argparse
import os
import sys
import threading
import time

DIR = "/tmp/newton-host-io"
IN_PATH = f"{DIR}/rep-in"
OUT_PATH = f"{DIR}/rep-out"


def send(line: str):
    with open(IN_PATH, "ab") as f:
        f.write(line.encode() + b"\n")
        f.flush()


def tail_out(stop, print_line):
    with open(OUT_PATH, "rb") as f:
        f.seek(0, os.SEEK_END)
        buf = b""
        while not stop.is_set():
            pos = f.tell()
            end = os.fstat(f.fileno()).st_size
            if end < pos:
                f.seek(0)
            chunk = f.read(4096)
            if not chunk:
                time.sleep(0.05)
                continue
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                print_line(line.decode(errors="replace"))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--eval", metavar="EXPR", help="send one expression, print output, exit")
    ap.add_argument("--quiet-ms", type=int, default=800,
                    help="--eval: exit after this long with no new output")
    args = ap.parse_args()

    for p in (IN_PATH, OUT_PATH):
        if not os.path.exists(p):
            sys.exit(f"{p} missing — is the hypervisor running with host-io-semihost?")

    stop = threading.Event()
    last_out = [time.monotonic()]

    def show(line):
        last_out[0] = time.monotonic()
        print(line, flush=True)

    t = threading.Thread(target=tail_out, args=(stop, show), daemon=True)
    t.start()

    if args.eval is not None:
        send(args.eval)
        deadline_quiet = args.quiet_ms / 1000.0
        while time.monotonic() - last_out[0] < deadline_quiet:
            time.sleep(0.05)
        stop.set()
        return

    print("newton-repl: connected (Ctrl-D to exit)", flush=True)
    try:
        while True:
            try:
                line = input("newton> ")
            except EOFError:
                break
            if line.strip():
                send(line)
                time.sleep(0.3)  # let the reply land before re-prompting
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()


if __name__ == "__main__":
    main()
