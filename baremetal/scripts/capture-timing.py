#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy", "pyserial", "pillow"]
# ///
"""Capture-side measurement for the Pi's HDMI output (Phase 1 of the
video-path plan): record the USB HDMI digitizer with ffmpeg
avfoundation and turn "the animation feels slow" into numbers.

    scripts/capture-timing.py devices
        List avfoundation video devices (index + name).

    scripts/capture-timing.py grab [--out PNG]
        Grab one frame, print the luma-threshold bounding box of the
        painted (non-black) region, save a PNG. The geometry
        regression check: today's baseline is roughly x 667-1373,
        y 4-1059 (~706x1055) on a 1920x1080 capture.

    scripts/capture-timing.py record --seconds N [--tap X,Y[,MS]]
        Record N seconds, print a per-frame change timeline and the
        change-span duration (frames-with-change x 33 ms). With
        --tap, send a `~p` pen-tap escape line over the serial
        console ~TAP_DELAY s into the recording (needs a hypervisor
        built with the `serial-pen-inject` feature) and log serial
        output alongside; the timeline is then tap-relative. This is
        the tap-to-window-open benchmark every optimisation phase is
        judged against.

The digitizer is resolved by NAME substring (default: "video capture
device"), never by index — indexes shift as other cameras come and
go. Known-good capture parameters (verified by hand):
`-f avfoundation -pixel_format uyvy422 -framerate 30` at 1920x1080.
Scratch and outputs live under /tmp/newton-claude/capture/.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

import numpy as np

OUTDIR = Path("/tmp/newton-claude/capture")
DEVICE_NAME_DEFAULT = "video capture device"
CAP_W, CAP_H = 1920, 1080
FPS = 30
# Downscaled analysis raster for `record` — full-res raw at 30 fps is
# ~124 MB/s; 480x270 gray keeps a long capture small while a Newton
# UI change (hundreds of panel pixels) is still thousands of raster
# pixels.
ANA_W, ANA_H = 480, 270
# Per-pixel luma delta below this is digitizer noise, not a repaint.
DIFF_THRESH = 24
# Frames whose changed-pixel count is below this are considered idle.
CHANGED_PX_MIN = 12
# Luma above this counts as "painted" for the grab-mode bounding box
# (the letterbox surround is black; Newton's white page is ~235).
BBOX_LUMA_THRESH = 40
# Serial console (same USB-TTL cable pi-upload.py uses).
SERIAL_PORT_DEFAULT = "/dev/cu.usbserial-BG03U2PN"
SERIAL_BAUD = 115_200
SERIAL_LOG = Path("/tmp/newton-claude/capture/serial-tap.log")
# Seconds of recording before the tap is sent, so the capture holds
# some pre-tap idle frames to calibrate "no change" against.
TAP_DELAY = 1.0


# ---------------------------------------------------------------- devices

def list_devices() -> list[tuple[int, str]]:
    """avfoundation video devices as (index, name). ffmpeg prints the
    listing to stderr and exits nonzero; both are expected."""
    r = subprocess.run(
        ["ffmpeg", "-hide_banner", "-f", "avfoundation",
         "-list_devices", "true", "-i", ""],
        capture_output=True, text=True)
    devices: list[tuple[int, str]] = []
    in_video = False
    for line in r.stderr.splitlines():
        if "AVFoundation video devices" in line:
            in_video = True
            continue
        if "AVFoundation audio devices" in line:
            in_video = False
            continue
        m = re.search(r"\[(\d+)\]\s+(.+?)\s*$", line)
        if in_video and m:
            devices.append((int(m.group(1)), m.group(2)))
    return devices


def resolve_device(name_substr: str) -> int:
    devices = list_devices()
    hits = [(i, n) for i, n in devices if name_substr.lower() in n.lower()]
    if len(hits) == 1:
        return hits[0][0]
    listing = "\n".join(f"  [{i}] {n}" for i, n in devices) or "  (none)"
    if not hits:
        sys.exit(f"error: no avfoundation video device matches "
                 f"{name_substr!r}. Devices:\n{listing}")
    sys.exit(f"error: {len(hits)} devices match {name_substr!r} — narrow "
             f"--device-name. Devices:\n{listing}")


def cmd_devices(_args: argparse.Namespace) -> int:
    for i, n in list_devices():
        print(f"[{i}] {n}")
    return 0


# ------------------------------------------------------------------- grab

def ffmpeg_input_args(index: int) -> list[str]:
    return ["-f", "avfoundation", "-pixel_format", "uyvy422",
            "-framerate", str(FPS),
            "-video_size", f"{CAP_W}x{CAP_H}", "-i", str(index)]


def grab_frame(index: int) -> np.ndarray:
    """One full-res grayscale frame as a (CAP_H, CAP_W) uint8 array."""
    r = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error",
         *ffmpeg_input_args(index),
         "-frames:v", "1", "-vf", "format=gray",
         "-f", "rawvideo", "-"],
        capture_output=True, timeout=30)
    want = CAP_W * CAP_H
    if r.returncode != 0 or len(r.stdout) < want:
        sys.exit(f"error: ffmpeg grab failed (rc={r.returncode}, "
                 f"{len(r.stdout)} bytes):\n{r.stderr.decode(errors='replace')[-2000:]}")
    return np.frombuffer(r.stdout[:want], dtype=np.uint8).reshape(CAP_H, CAP_W)


def luma_bbox(frame: np.ndarray, thresh: int) -> tuple[int, int, int, int] | None:
    """(x0, y0, x1, y1) inclusive bounds of pixels brighter than
    `thresh`, or None if nothing qualifies."""
    mask = frame > thresh
    ys, xs = np.nonzero(mask)
    if len(xs) == 0:
        return None
    return int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())


def print_bbox(frame: np.ndarray) -> None:
    bbox = luma_bbox(frame, BBOX_LUMA_THRESH)
    if bbox is None:
        print(f"bbox: no pixels above luma {BBOX_LUMA_THRESH} — panel black?")
        return
    x0, y0, x1, y1 = bbox
    w, h = x1 - x0 + 1, y1 - y0 + 1
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    print(f"bbox: x {x0}-{x1}, y {y0}-{y1}  ({w}x{h}, aspect {w / h:.3f}, "
          f"centre {cx:.0f},{cy:.0f}; frame centre {CAP_W // 2},{CAP_H // 2})")
    print(f"      baseline reference: ~x 667-1373, y 4-1059 (~706x1055)")


def cmd_grab(args: argparse.Namespace) -> int:
    index = resolve_device(args.device_name)
    frame = grab_frame(index)
    print_bbox(frame)
    out = Path(args.out) if args.out else (
        OUTDIR / f"grab-{_dt.datetime.now():%Y%m%d-%H%M%S}.png")
    out.parent.mkdir(parents=True, exist_ok=True)
    from PIL import Image

    Image.fromarray(frame, mode="L").save(out)
    print(f"frame: {out}")
    return 0


# ------------------------------------------------------------------ record

def send_tap(port: str, tap: str, log: Path, listen_s: float = 4.0) -> None:
    """Open the serial console, send the `~p` escape line, and log
    whatever the board prints for a couple of seconds (the injector's
    ack plus any blit_timing lines)."""
    import serial

    x_y_ms = tap.replace(" ", "")
    line = f"~p{x_y_ms}\n".encode()
    log.parent.mkdir(parents=True, exist_ok=True)
    with serial.Serial(port, SERIAL_BAUD, timeout=0.2) as ser, \
            open(log, "ab", buffering=0) as lf:
        stamp = _dt.datetime.now().strftime("%H:%M:%S")
        lf.write(f"\n===== tap {x_y_ms} at {stamp} =====\n".encode())
        ser.write(line)
        ser.flush()
        print(f"tap: sent {line!r} on {port}", flush=True)
        deadline = time.monotonic() + listen_s
        while time.monotonic() < deadline:
            data = ser.read(4096)
            if data:
                lf.write(data)


def cmd_record(args: argparse.Namespace) -> int:
    index = resolve_device(args.device_name)
    OUTDIR.mkdir(parents=True, exist_ok=True)
    n_frames = int(args.seconds * FPS)

    proc = subprocess.Popen(
        ["ffmpeg", "-hide_banner", "-loglevel", "error",
         *ffmpeg_input_args(index),
         # Output-side CFR at the capture rate. The digitizer delivers
         # no frame-rate metadata (tbr shows as 1000k), so without this
         # ffmpeg CFR-fills at 1 MHz from the first frame — the whole
         # "recording" is frame 1 duplicated, finished in milliseconds.
         "-r", str(FPS),
         "-frames:v", str(n_frames),
         "-vf", f"scale={ANA_W}:{ANA_H},format=gray",
         "-f", "rawvideo", "-"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    frame_bytes = ANA_W * ANA_H
    # Read the first frame BEFORE arming the tap: avfoundation takes
    # 1-2 s to open the device, and a tap sent on wall-clock alone can
    # land (and the UI can finish its brief repaint) before frame 0
    # exists.
    first = proc.stdout.read(frame_bytes)

    tap_thread = None
    tap_frame_est = None
    if args.tap and first:
        listen_s = max(4.0, args.seconds - TAP_DELAY)

        def tap_later() -> None:
            time.sleep(TAP_DELAY)
            send_tap(args.port, args.tap, SERIAL_LOG, listen_s)

        tap_thread = threading.Thread(target=tap_later, daemon=True)
        tap_thread.start()
        tap_frame_est = 1 + int(TAP_DELAY * FPS)

    raw = first + proc.stdout.read(frame_bytes * (n_frames - 1))
    proc.stdout.close()
    stderr = proc.stderr.read().decode(errors="replace")
    proc.wait()
    if tap_thread:
        tap_thread.join(timeout=args.seconds + 8)
    got = len(raw) // frame_bytes
    if got < 2:
        sys.exit(f"error: only {got} frames captured (rc={proc.returncode}):\n"
                 f"{stderr[-2000:]}")
    if got < n_frames:
        print(f"warning: short capture — {got}/{n_frames} frames "
              f"(rc={proc.returncode})", flush=True)

    frames = np.frombuffer(raw[: got * frame_bytes], dtype=np.uint8)
    frames = frames.reshape(got, ANA_H, ANA_W).astype(np.int16)
    diffs = np.abs(np.diff(frames, axis=0))  # (got-1, H, W)
    changed_px = (diffs > DIFF_THRESH).sum(axis=(1, 2))

    print(f"\ncapture: {got} frames at {FPS} fps ({got / FPS:.2f} s), "
          f"analysis raster {ANA_W}x{ANA_H}, diff>{DIFF_THRESH}, "
          f"idle<{CHANGED_PX_MIN}px")
    if tap_frame_est is not None:
        print(f"tap: sent ~{TAP_DELAY:.1f} s in (frame ~{tap_frame_est})")

    changed_frames = []
    print("\nper-frame change timeline (frames with changes only):")
    for i, n in enumerate(changed_px, start=1):
        if n >= CHANGED_PX_MIN:
            changed_frames.append(i)
            rel = (f"  tap{(i - tap_frame_est) / FPS:+.2f}s"
                   if tap_frame_est is not None else "")
            print(f"  frame {i:4d}  t={i / FPS:7.3f}s{rel}  changed={int(n)}px")
    if args.save_raw:
        stamp = _dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        rawfile = OUTDIR / f"record-{stamp}-{ANA_W}x{ANA_H}x{got}.gray"
        rawfile.write_bytes(raw[: got * frame_bytes])
        print(f"raw frames: {rawfile}")
    if not changed_frames:
        print(f"  (none — the screen never changed; max per-frame diff count "
              f"{int(changed_px.max())}px, max luma delta {int(diffs.max())})")
        return 0

    first, last = changed_frames[0], changed_frames[-1]
    span_frames = last - first + 1
    print(f"\nsummary: {len(changed_frames)} changed frames; "
          f"first #{first} (t={first / FPS:.3f}s), "
          f"last #{last} (t={last / FPS:.3f}s)")
    print(f"animation span: {span_frames} frames x {1000 / FPS:.0f} ms = "
          f"{span_frames / FPS * 1000:.0f} ms "
          f"({len(changed_frames)} frames actually changed = "
          f"{len(changed_frames) / FPS * 1000:.0f} ms of visible painting)")
    if tap_frame_est is not None:
        print(f"tap-to-first-change: ~{(first - tap_frame_est) / FPS * 1000:.0f} ms, "
              f"tap-to-quiescent: ~{(last - tap_frame_est) / FPS * 1000:.0f} ms")
        print(f"serial log: {SERIAL_LOG}")

    return 0


# -------------------------------------------------------------------- main

def main() -> int:
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--device-name", default=DEVICE_NAME_DEFAULT,
                   help=f"avfoundation device-name substring "
                        f"(default {DEVICE_NAME_DEFAULT!r})")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("devices", help="list avfoundation video devices")

    g = sub.add_parser("grab", help="single frame + painted-region bbox")
    g.add_argument("--out", help="PNG path (default under /tmp/newton-claude/capture/)")

    r = sub.add_parser("record", help="record + per-frame change timeline")
    r.add_argument("--seconds", type=float, default=6.0,
                   help="capture length (default 6)")
    r.add_argument("--tap", metavar="X,Y[,MS]",
                   help="send a serial-pen-inject tap during the capture")
    r.add_argument("--port", default=SERIAL_PORT_DEFAULT,
                   help=f"serial console for --tap (default {SERIAL_PORT_DEFAULT})")
    r.add_argument("--save-raw", action="store_true",
                   help="keep the downscaled raw frames for re-analysis")

    args = p.parse_args()
    return {"devices": cmd_devices, "grab": cmd_grab, "record": cmd_record}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
