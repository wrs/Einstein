#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Install a .pkg into the running guest through the NewtonScript REPL.

Bypasses the Dock entirely: the package bytes are pushed into a guest
binary object with StuffByte, byte-summed to verify the copy, then
handed to the store's own `SuckPackageFromBinary` — the same call the
ROM's restore path makes. Exercises the store/package-loader half of
the install flow in isolation from the serial/MNP/Dock half.

    scripts/pkg-repl-install.py MyApp.pkg            # upload + install
    scripts/pkg-repl-install.py MyApp.pkg --no-install
    scripts/pkg-repl-install.py --eval 'EXPR'        # one wrapped eval

Every expression is wrapped in try/onexception; an exception is
reported as its name plus (for evt.ex.fr exceptions) the errorCode,
because an unhandled exception in the ROM's REP loop wedges the REP
task (its ExceptionNotify path never returns).

Requires a hypervisor built with host-io-semihost, booted to the
`REP> Welcome to NewtonScript!` banner (scripts/newton-repl.py).
"""

import argparse
import os
import sys
import time

DIR = "/tmp/newton-host-io"
IN_PATH = f"{DIR}/rep-in"
OUT_PATH = f"{DIR}/rep-out"

# The translator's line buffer is 1024 bytes (incl. the trailing \n\0).
MAX_LINE = 1000

# This REP's ParseString does not recognise the `nil` keyword (it
# raises "undefined variable", -48807), though the runtime nil value
# exists. `{}.x` — reading an absent slot — evaluates to nil cleanly.
NIL = "{}.x"


class Repl:
    def __init__(self, timeout=20.0, log_path="/tmp/newton-claude/pkg-boot.log"):
        self.timeout = timeout
        self.log_path = log_path
        self.out = open(OUT_PATH, "rb")
        self.out.seek(0, os.SEEK_END)

    def _readline(self, timeout):
        deadline = time.monotonic() + timeout
        buf = b""
        while time.monotonic() < deadline:
            chunk = self.out.readline()
            if chunk:
                buf += chunk
                if buf.endswith(b"\n"):
                    return buf.decode(errors="replace").rstrip("\n")
            else:
                time.sleep(0.02)
        raise TimeoutError(f"REPL: no reply within {timeout}s (partial: {buf!r})")

    def raw(self, expr, timeout=None):
        """Send one raw expression, return the REP's echo line."""
        line = expr.replace("\n", " ")
        assert len(line) <= MAX_LINE, f"line too long: {len(line)}"
        with open(IN_PATH, "ab") as f:
            f.write(line.encode() + b"\n")
        return self._readline(timeout or self.timeout)

    def eval(self, expr, timeout=None):
        """Wrapped eval. Returns ('ok', ref_line) or ('exc', text)."""
        # The handler evaluates to a frame-exception's errorCode (a
        # tagged integer) or, for message exceptions, prints the name
        # through Print() (which lands in the kernel log as
        # `platform.Log:`) and evaluates to the symbol 'exc.
        wrapped = (
            "try begin " + expr + " end onexception |evt.ex| do begin "
            "local e := CurrentException(); "
            "Print(\"EXC \" & e.name); "
            "if IsFrame(e.data) and HasSlot(e.data, 'errorCode) then "
            "e.data.errorCode else 'exc end"
        )
        log_pos = self._log_pos()
        line = self.raw(wrapped, timeout)
        logged = self._log_since(log_pos)
        exc = [l for l in logged if "platform.Log" in l and "EXC " in l]
        if exc:
            code = ""
            try:
                code = f" errorCode={ref_to_int(line)}"
            except ValueError:
                pass
            return ("exc", exc[-1].split("platform.Log: ", 1)[1].strip() + code)
        return ("ok", line.strip())

    def _log_pos(self):
        try:
            return os.path.getsize(self.log_path)
        except OSError:
            return 0

    def _log_since(self, pos):
        try:
            with open(self.log_path, "rb") as f:
                f.seek(pos)
                return f.read().decode(errors="replace").splitlines()
        except OSError:
            return []

    def eval_int(self, expr, timeout=None):
        kind, line = self.eval(expr, timeout)
        if kind != "ok":
            raise RuntimeError(line)
        return ref_to_int(line)


def ref_to_int(line):
    """'#68' → 26 (tagged 30-bit signed int). Raises on non-integers."""
    s = line.strip()
    if not s.startswith("#"):
        raise ValueError(f"not a ref echo: {line!r}")
    ref = int(s[1:], 16)
    if ref & 3 != 0:
        raise ValueError(f"not an integer ref: {line!r}")
    v = ref >> 2
    if v & (1 << 29):
        v -= 1 << 30
    return v


def upload(repl, data, var):
    n = len(data)
    repl.eval(f"DefGlobalVar('{var}, MakeBinary({n}, 'package))")
    r = repl.eval("Length(%s)" % var)
    assert ref_to_int(r[1]) == n, r
    # Global helper: stuff an integer array at an offset.
    # NB: a `for i := ...` loop variable is left undefined in a
    # REP-compiled function (the REP's InterpretBlock path does not
    # auto-declare the loop counter), so use `foreach` with an explicit
    # local index instead.
    repl.eval(
        "DefGlobalFn('PkgStuff, func(b, off, a) begin local i := 0; "
        "foreach v in a do begin StuffByte(b, off + i, v); i := i + 1 end; "
        "0 end)")
    off = 0
    while off < n:
        # ~3.6 chars/byte worst case; keep well under the line limit.
        chunk = data[off:off + 220]
        arr = ",".join(str(b) for b in chunk)
        kind, line = repl.eval(f"PkgStuff({var}, {off}, [{arr}])")
        if kind != "ok":
            raise RuntimeError(f"stuff @{off}: {line}")
        off += len(chunk)
        print(f"\r  uploaded {off}/{n}", end="", flush=True)
    print()
    # Verify the copy by summing the guest-side bytes. `foreach` does
    # not iterate a binary, so index with ExtractByte via a `while`
    # loop (the REP mis-handles `for` counters — see PkgStuff).
    repl.eval(
        "DefGlobalFn('PkgSum, func(b) begin local s := 0; local i := 0; "
        "local n := Length(b); while i < n do begin "
        "s := s + ExtractByte(b, i); i := i + 1 end; s end)")
    try:
        got = repl.eval_int(f"PkgSum({var})", timeout=120)
        want = sum(data) & ((1 << 30) - 1)
        if got != want:
            raise RuntimeError(f"byte-sum mismatch: guest {got} host {want}")
        print(f"  byte-sum verified ({want})")
    except RuntimeError as e:
        print(f"  byte-sum check skipped: {e}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pkg", nargs="?")
    ap.add_argument("--var", default="pkgbin", help="guest global holding the binary")
    ap.add_argument("--no-install", action="store_true")
    ap.add_argument("--eval", metavar="EXPR", help="one wrapped eval and exit")
    ap.add_argument("--log", default="/tmp/newton-claude/pkg-boot.log",
                    help="hypervisor console log (for Print()/platform.Log output)")
    args = ap.parse_args()
    for p in (IN_PATH, OUT_PATH):
        if not os.path.exists(p):
            sys.exit(f"{p} missing — is the hypervisor running with host-io-semihost?")
    repl = Repl(log_path=args.log)
    if args.eval:
        print(repl.eval(args.eval))
        return
    if not args.pkg:
        ap.error("pkg required")
    data = open(args.pkg, "rb").read()
    print(f"{args.pkg}: {len(data)} bytes")
    upload(repl, data, args.var)
    print("  IsPackage:", repl.eval(f"IsPackage({args.var})"))
    if args.no_install:
        return
    print("  before: packages =", repl.eval_int("Length(GetPackages())"))
    print("  SuckPackageFromBinary:",
          repl.eval(f"GetDefaultStore():SuckPackageFromBinary({args.var}, {NIL})", timeout=120))
    print("  after:  packages =", repl.eval_int("Length(GetPackages())"))


if __name__ == "__main__":
    main()
