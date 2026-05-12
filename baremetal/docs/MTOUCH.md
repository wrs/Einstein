# TSTP MTouch (UPerfect Y / Eviciv-class) touch panel

USB HID touchscreen used by several portable HDMI+USB monitors marketed
under UPerfect Y, Eviciv, and similar brands. Verified as the touch
controller in the panel on Walter's bench (Pi Zero 2 W + 7" portable
monitor). No init firmware download, no proprietary protocol — once
activated it speaks a standard digitizer HID report stream.

## Device identity

```
idVendor       0x0416     (Winbond Electronics Corp. — silicon vendor)
idProduct      0xC168
iManufacturer  "TSTP"     iProduct "MTouch"   iSerial "CMTP_1.0"
bcdUSB         2.00        full-speed (12 Mbps), self-powered, 100 mA
bcdDevice      0.00
```

Same VID/PID across firmware revisions we've observed (linux-hardware.org
lists 15+ installs; [FreeBSD bug 264379][bsd-bug] describes a different
revision that misadvertises interface 0 as a Boot Keyboard with a broken
descriptor — ours doesn't have that bug).

[bsd-bug]: https://bugs.freebsd.org/bugzilla/show_bug.cgi?id=264379

## USB topology

```
Configuration 1 (the only one)
  Interface 0  HID, bSubClass=1 bProto=2 (Boot/Mouse advertised,
               actually Digitizer per the descriptor)
    EP 0x81 INTR-IN   wMaxPacket=64  bInterval=5
    EP 0x02 INTR-OUT  wMaxPacket=64  bInterval=8
    Report descriptor: 735 bytes (Usage Page 0x0D Digitizer)
  Interface 1  HID, same class triple
    EP 0x83 INTR-IN   wMaxPacket=64  bInterval=8
    EP 0x04 INTR-OUT  wMaxPacket=64  bInterval=8
    Report descriptor: 142 bytes (Usage Page 0xFFFF vendor)
```

We only need interface 0 EP 0x81. Interface 1 is the vendor
config/firmware-update channel; the hypervisor doesn't bind it.

The interface-protocol byte says "Mouse" on both interfaces — boot-mouse
compatibility — but the *report descriptor* makes interface 0 a proper
multi-touch digitizer. macOS attached its HID layer only to interface 1
(the vendor channel) and ignored interface 0, which is why the panel
does nothing on a Mac.

## Activation handshake (don't skip)

The device does **not** auto-stream reports after SET_CONFIGURATION. A
passive `read(/dev/hidraw0)` on Linux returns zero bytes until something
opens the cooked input node — at which point `hid-multitouch` issues an
activation request and reports start flowing immediately.

The trigger is a Feature Report fetch:

```
GET_REPORT(type=Feature, ReportID=3, length=2)  →  bytes: 0x0a 0x00
                                                   (Contact Count Max = 10)
```

This is hid-multitouch's standard "discover max contacts" call; the
device firmware uses it as "host is ready". Once issued, Report ID 1
streams continuously at ~16 ms intervals (steady, with keep-alive
reports even when nothing changes — see "Behavior notes" below). The
hypervisor USB stack must replicate this — otherwise the panel stays
mute despite a fully-configured endpoint.

The 142-byte vendor channel (interface 1) is a separate concern and
appears to be quiescent in practice; we don't touch it.

## Report ID 1 wire format (56 bytes)

```
Offset  Size  Field
------  ----  -----
 0      1     Report ID (always 0x01)
 1      1     bit0 = tip switch, bits 1..7 = contact ID    (slot 0)
 2..3   2     X       u16 LE                                (slot 0)
 4..5   2     Y       u16 LE                                (slot 0)
 6..10  5     slot 1   (same layout: tip+id, X LE, Y LE)
11..15  5     slot 2
16..20  5     slot 3
21..25  5     slot 4
26..30  5     slot 5
31..35  5     slot 6
36..40  5     slot 7
41..45  5     slot 8
46..50  5     slot 9
51..54  4     Scan time   u32 LE   (units appear to be 100 µs)
 55     1     Contact count (0..10)
```

Each finger slot is **5 bytes**, not 6 — the tip switch (1 bit) and
contact identifier (7 bits) pack into a single byte. Inactive slots are
zero-filled; the populated count is in byte 55.

For Newton we only read bytes 0..5: tip + X + Y of slot 0. The device
puts the active single-touch contact in slot 0, so we don't need to
scan slots 1..9 unless we ever want multi-touch (which Newton doesn't
support).

### Decode for single-touch

```rust
let pressed = (data[1] & 0x01) != 0;
let x       = u16::from_le_bytes([data[2], data[3]]);   // 0..1024
let y       = u16::from_le_bytes([data[4], data[5]]);   // 0..600
```

## Coordinate space

```
X logical   0..1024     (Logical Max 0x0400)
Y logical   0..600      (Logical Max 0x0258)
X physical  0..608      (Phys Max 0x0260, in 0.01-cm units = 60.8 mm)
Y physical  0..340      (Phys Max 0x0154, in 0.01-cm units = 34.0 mm)
Resolution  7 units/mm  (matches a 7" 1024×600 panel)
Property    INPUT_PROP_DIRECT   (direct-contact, not touchpad-style)
```

Newton renders a 320×480 portrait FB; our HDMI output paints it scaled
1.5× to 480×720 centred on a 1280×720 panel. The panel's touch surface
spans the full 1024×600 area, which on a landscape 7" monitor covers
the whole displayed image (including the black letterbox bands around
Newton's 480×720 region). The coordinate mapping has to invert the
display transform: touches in the letterbox should be discarded, and
touches in the Newton region scaled back to 0..319 × 0..479. Exact
constants get pinned during calibration (see Phase 5e in
`REAL_HW_BRINGUP.md`).

## Behavior notes

- **Idle keep-alives.** Even with no input change the device emits
  Report ID 1 at ~16 ms intervals. The input loop must compare
  (tip, X, Y) against the previous report and only generate a pen
  event on a real change — otherwise every steady-press becomes a
  pen-stream firehose.
- **Tip-up.** Releasing produces one report with bit 0 of byte 1
  cleared, then the device goes quiet until the next touch. Detect
  the up-edge on the first report whose `pressed` bit drops to 0.
- **Contact identifier.** Slot 0's id field (`data[1] >> 1`)
  increments on each fresh touch; the kernel uses it as the
  multitouch tracking ID. For single-touch Newton we can ignore it,
  but it's a free way to distinguish "same finger still down" from
  "lifted and re-touched at the same coordinates".
- **No auto-stream after a stall.** If a transfer stalls (e.g. cable
  hiccup) the device tends to stop streaming until a fresh GET_REPORT
  Feature is issued. Drivers should re-issue the activation handshake
  after any error recovery, not just at attach time.

## How this was characterized

Captured on Walter's Pi Zero 2 W under Raspbian (kernel 6.x) on
2026-05-12. Artifacts saved at `~/mtouch_cap/` on `pi@vt100.local`:

- `descriptors.txt`     — both HID report descriptors, hex dump from `usbhid-dump`
- `lsusb_v_root.txt`    — full USB descriptor tree
- `touch.bin`           — 168 bytes of live Report ID 1 captures
- `evtest.log`          — parallel cooked events for byte-by-byte cross-validation

The decode above was verified by capturing a steady touch at one
position and confirming X=399 / Y=132 in the raw `data[2..6]` bytes
matched `ABS_X=399 / ABS_Y=132` in the parallel evtest log.

## Recipe for characterizing a different panel

When another touch panel needs adding (Phase 5d is class-pluggable —
see `REAL_HW_BRINGUP.md`):

1. Plug into a Pi running Raspbian.
2. `dmesg | tail -20` after attach — note hidraw node + VID/PID. If
   `hid-multitouch` binds, you've found a sane panel.
3. `sudo usbhid-dump -d $VID:$PID -e descriptor` — prints both
   HID report descriptors. (Works whether or not the kernel driver
   is bound. `lsusb -v` shows `** UNAVAILABLE **` for descriptors of
   a bound device, which is unhelpful.)
4. Decode the report descriptor with
   <https://eleccelerator.com/usbdescreqparser/> or by reading
   USB HID 1.11 §6.2.2. The crucial fields are Report ID, slot
   layout, X/Y bit widths, and logical/physical ranges.
5. Capture live reports: hidraw0 alone is usually mute because the
   device needs activation. Run `evtest /dev/input/eventN` on the
   cooked input node **in parallel** with `dd if=/dev/hidraw0` to
   force the kernel to issue whatever Feature/SetIdle handshake the
   driver associates with this device. Cross-check that the raw
   bytes match the cooked events.
6. If hid-multitouch's handshake is non-obvious, grep for the
   device's VID in `drivers/hid/hid-multitouch.c` for any
   quirk entries.
