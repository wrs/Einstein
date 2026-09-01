# Real hardware — Pi Zero 2 W

The hypervisor runs end-to-end on a real Raspberry Pi Zero 2 W
(BCM2710A1, Cortex-A53 ×4): EL2 handoff, ROM boot to the interactive
Welcome UI, HDMI display, USB touchscreen input, HDMI audio, and SD
flash persistence with non-blocking DMA autosave. This doc is the
hardware reference: firmware contracts, the as-built driver stacks,
and the porting notes that cost real-hardware round-trips to learn.

The Pi 3B is **not** a stepping stone — same SoC, same image; only
the form factor differs. QEMU `raspi3b` and the real Zero 2 W share
the `platform-raspi3b` Cargo feature.

Remaining work: Newton's serial port and PCMCIA images (see
"Remaining work" at the end).

## Hardware kit

- Pi Zero 2 W board.
- Micro-SD card (FAT32 boot partition; firmware + `kernel8.img` (the
  nhboot bootloader) + `HYPERV.IMG` (the hypervisor) + `NEWTON.BIN`).
- USB-TTL serial cable (3.3 V CMOS, NOT 5 V RS-232). GPIO 14 = TXD,
  GPIO 15 = RXD, common GND on GPIO 6/9/14/20/25/30/34/39.
- Micro-USB power supply (the data port; the Zero 2 W has no
  dedicated PWR-IN).
- Mini-HDMI cable + panel. The bench panel is a small Pi-targeted
  1280×720-capable display with speakers (HDMI audio) and an
  integrated TSTP MTouch USB touchscreen (see
  [`MTOUCH.md`](MTOUCH.md)).
- USB OTG adapter for the touchscreen.
- Host machine (this Mac) with `scripts/pi-upload.py` on the serial
  cable — it uploads images, captures the console, and power-cycles
  the board through a HomeKit switch (Shortcuts `Pi On` / `Pi Off`).
  Any terminal program works for plain watching, but only one process
  can hold the port.

## Pi firmware facts

These come from the raspberrypi.com docs and the raspberrypi/tools
armstub source, not memory. Re-verify before relying.

### EL handoff (Pi 0/2/3/4 with `arm_64bit=1`)

Verified by reading `armstub8.S`
(`github.com/raspberrypi/tools/blob/master/armstubs/armstub8.S`) and
confirmed on the actual board (the boot banner prints `CurrentEL = 2`,
`MIDR_EL1 = 0x410fd034`, matching QEMU byte-for-byte): the default
stub does

```
mov x0, #SPSR_EL3_VAL          ; SPSR_EL3_MODE_EL2H
msr spsr_el3, x0
adr x0, in_el2
msr elr_el3, x0
eret
```

so **the firmware hands off `kernel8.img` at EL2h** by default.
Secondary cores park in a WFE spin-table loop at memory offsets
0xe0/0xe8/0xf0 (core 1/2/3 entry pointers). Kernel entry address is
loaded from offset 0xfc — firmware picks `0x80000` by default for
`arm_64bit=1`.

This is conditional on:
- `arm_64bit=1` in `config.txt`.
- No `kernel_old=1` (which disables the stub entirely — the kernel
  would load at 0 and run on all 4 cores in EL3).
- No custom `armstub=<file>` overriding the default.

### UART routing on GPIO 14/15

On the Pi Zero 2 W, **PL011 (UART0) is wired to the onboard
Bluetooth chip by default**, not to GPIO 14/15; without intervention
the GPIO header carries the mini-UART (UART1/ttyS0). The hypervisor
drives PL011 at `0x3F20_1000` (`src/host/console.rs`,
`src/host/platform/raspi3b.rs`), so `dtoverlay=disable-bt` in `config.txt`
is required to route PL011 to GPIO 14/15.

- `enable_uart=1` — requests the GPIO 14/15 serial console path.
- `uart_2ndstage=1` — firmware-side debug logging to the UART. A
  useful checkpoint: if it produces no firmware output on the wire,
  the problem is upstream of our code (SD layout, GPIO ALT mode,
  baud divisor).
- PL011 clock is 48 MHz by firmware default; set
  `init_uart_clock=48000000` explicitly if baud comes out wrong.

### Boot partition

- `bootcode.bin` — GPU-side stage-1 loader; brings up DRAM. (Pi 4/5
  use an SPI EEPROM bootloader instead; the Zero 2 W still loads
  stage-1 from SD.)
- `start.elf` / `fixup.dat` — main GPU firmware + memory split.
- `config.txt` — our settings (`boot-pi/config.txt` in this repo:
  `arm_64bit=1`, `enable_uart=1`, `uart_2ndstage=1`,
  `dtoverlay=disable-bt`, `gpu_mem=16`, HDMI knobs below).
- `kernel8.img` — the nhboot bootloader (`nhboot/`, ~90 KiB), loaded at
  `0x80000` and entered at EL2. It relocates itself to `0x10000000`,
  validates `HYPERV.IMG` and copies the hypervisor to `0x80000` (the
  load address in `linker.ld.in` for raspi3b) — see "Serial image
  upload" below.
- `HYPERV.IMG` — the hypervisor, in a fixed 16 MiB container (4 KiB
  header + raw image). The firmware loads it at `0x02000000` via
  `initramfs HYPERV.IMG 0x02000000` in `config.txt`.
- `NEWTON.BIN` — the persisted 8 MiB guest flash (created on first
  save).

Firmware blobs are pinned to raspberrypi/firmware commit
`8fce67a9ec5668fb8d42d215c9ec4c199340bf41` and cached under
`target/pi-firmware-cache/` by `scripts/build-sd.sh`.

Linker note: `.eh_frame_hdr` is in the linker template's DISCARD list.
A binary with no `.rodata` (string literals folded into `.text`)
otherwise gets `.eh_frame_hdr` placed at VMA 0x80000, shifting
`_start` and crashing on the leading UDFs.

## Building and booting

```bash
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest> [sd-mount]
```

assembles the full boot partition (pinned firmware + `config.txt` +
`kernel8.img` = nhboot + `HYPERV.IMG` = the hypervisor) under `<dest>`
and optionally rsyncs it to a mounted card. That is the first-time
path; after it, a hypervisor rebuild is loaded over the serial cable
(next section) and the card is rewritten only for a firmware,
`config.txt` or nhboot change.

## Serial image upload — nhboot

Rebuilding the hypervisor does not involve the SD card: `kernel8.img`
is a small permanent bootloader, `nhboot`, and the hypervisor lives in
a second file it boots, `HYPERV.IMG`, which `scripts/pi-upload.py`
replaces over the USB-TTL cable. The bootloader never changes with a
hypervisor rebuild, so a broken build cannot take the update path
down with it: a power cycle and a re-upload recover from anything.

### What is on the card

| File | What | Size |
|---|---|---|
| `kernel8.img` | nhboot (`nhboot/`, its own package) | ~90 KiB |
| `HYPERV.IMG` | container: 4 KiB header + raw hypervisor image + zero pad | 16 MiB, fixed |
| `NEWTON.BIN` | guest flash (unchanged by any of this) | 8 MiB |
| `config.txt` | + `initramfs HYPERV.IMG 0x02000000` | |

The file is a fixed size so that an upload can rewrite individual
sectors in place without ever touching the FAT allocation.

Header (`nhboot/src/image.rs`, mirrored by `ImageFormat` in
`scripts/pi-upload.py`; little-endian):

```
0x000  "NHIMG001"
0x008  u32 payload_len      raw image bytes after the header
0x00C  u32 payload_crc      CRC-32 (zlib) of the payload
0x010  u32 hdr_crc          CRC-32 of bytes [0x000, 0x010)
       zero to 0x1000, then the payload
```

### Boot sequence and RAM map

1. Firmware loads `kernel8.img` at `0x80000` and `HYPERV.IMG` at
   `0x02000000` (`initramfs <file> <addr>` — the option Linux uses
   for its initrd; it "performs the actions of both ramfsfile and
   ramfsaddr" per raspberrypi.com's config.txt reference, and is
   written without an `=`), then enters nhboot at EL2 with x0 = the
   DTB pointer.
2. `nhboot/src/boot.s` parks cores 1–3, copies the bootloader to its
   link address `0x10000000` (the prologue is position-independent),
   `IC IALLU`, branches there, sets up stack/guard/bss.
3. `main` brings up the PL011 at 115200, prints its banner (`nhboot v1
   el=2 dtb=… entered_at=0x80000 linked_at=0x10000000`) and the
   container's state (`image::inspect` — magic, header CRC, length,
   payload CRC).
4. Handshake window: with a valid image nhboot listens 1 s for a host
   hello and otherwise boots; without one it waits forever, printing
   `nhboot: no bootable image; waiting for upload` every 5 s.
5. Boot: the payload is copied to `0x80000`, `IC IALLU`, and entered
   with x0 = the DTB pointer, exactly as the firmware would have
   entered it (the hypervisor's `_start` ignores x0).

```
0x0008_0000  nhboot as loaded; then the hypervisor (copied here in step 5)
0x0200_0000  HYPERV.IMG as loaded by the firmware ("old" image)
0x0300_0000  staging for an upload ("new" image), same container layout
0x1000_0000  nhboot after self-relocation (+ 16 KiB stack, .bss)
```

The MMU stays off in nhboot (all RAM Non-cacheable, MMIO Device):
polled drivers, a one-shot 10 MiB memcpy and CRC-32 via the ARMv8
`crc32x` instructions (~0.3 s) need nothing more.

### Protocol (`nhboot/src/xfer.rs` ↔ `scripts/pi-upload.py`)

```
host   →  \x01NHUP <baud>\n          text, 115200, repeated every 100 ms
nhboot →  NHUP-OK <baud>\n           after fingerprinting the old image;
                                     both sides switch to <baud>
nhboot →  T  u32 n, n×{u32 adler32, u32 crc32}, u32 crc32(entries)
                                     one entry per full 4 KiB block of the
                                     old payload (n = 0 without one)
host   →  D  u32 offset, u32 len(≤64 KiB), u32 crc32, bytes    DATA
host   →  C  u32 new_off, u32 old_off, u32 len, u32 crc32      COPY (old→new)
host   →  K  u32 payload_len, u32 payload_crc                  COMMIT
nhboot →  A  u32 echo            ACK   (offset for D/C, len for K)
nhboot →  N  u32 echo, u8 reason NAK   1 bad crc, 2 bad offset/len,
                                       3 rx timeout, 4 no old image,
                                       5 unknown tag
nhboot →  text: persist: … lines, then DONE\n; baud back to 115200
```

Stop-and-wait; the host retries a NAK'd or unanswered message three
times. nhboot prints nothing between `T` and the COMMIT ACK because
the console *is* the link. A byte gap of 2 s inside a message
abandons it (NAK 3); after an unknown tag nhboot drains input until
100 ms of silence so garbage costs one NAK, not one per byte. The
transfer baud is the host's choice (default 1.5 M; the PL011's 48 MHz
reference and the FTDI cable both allow 3 M = clk/16).

### Persistence (`nhboot/src/persist.rs`)

After the COMMIT ACK, nhboot brings up the SD card with a PIO-only
copy of the SDHOST driver (CMD17/CMD24 — deliberately no DMA, so the
bootloader is independent of the hypervisor's DMA save path; the
copy is kept in step by hand), mounts the FAT32 volume with the
vendored `embedded-sdmmc`, and:

- opens `HYPERV.IMG`; if it is missing or not 16 MiB it is recreated
  through the FAT API (the slow fallback — `build-sd.sh` puts a
  correctly sized file on a fresh card);
- resolves every sector's LBA through the cluster chain
  (`file_cluster_lbas`, the vendored addition), so fragmentation
  doesn't matter;
- writes only the sectors that differ from the firmware-loaded copy
  (those are, by construction, the bytes on the card), **header
  sector last** — a power cut mid-write leaves a container whose CRC
  fails, and nhboot then waits for a re-upload instead of booting a
  half-written image.

An SD failure is reported (`persist: FAILED (…) — image boots from RAM
only this time`) and the uploaded image still boots for that run.

### Delta

The 8 MiB ROM + REx blob inside the image is identical build to
build but moves whenever the code before it grows, so the delta is
offset-independent (the rsync algorithm): nhboot fingerprints each
4 KiB block of the old payload (adler32 + crc32, ~2500 entries), the
host computes the adler32 of every 4 KiB window of the new image at
every byte offset with numpy prefix sums (~1 s), verifies the
candidates by crc32 and walks greedily into merged COPY runs and DATA
runs. A one-line rebuild sends a few hundred KiB (3 % of the image);
an identical image sends one COPY and writes nothing. `--full`
forces DATA-only.

### Host tool cheatsheet

```bash
# The loop: build, power-cycle, upload the delta, boot, capture until the marker.
cargo build --release --no-default-features --features pi-bare-metal-input
scripts/pi-upload.py --kernel target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
    --until 'Welcome to NewtonScript' --timeout 120

scripts/pi-upload.py --no-upload --until 'DMA save complete' --timeout 60   # power-cycle + capture
scripts/pi-upload.py --no-power-cycle --kernel <elf>        # board already off/on by hand
scripts/pi-upload.py --baud 3000000 --kernel <elf>          # faster link (see status below)
scripts/pi-upload.py --make-image out/HYPERV.IMG --kernel <elf>   # container only (build-sd.sh)
```

Exit status: 0 when `--until` matched, 1 on `--timeout` or a protocol
error, 130 on Ctrl-C. The raw console is appended to `--log` (default
`/tmp/newton-claude/serial.log`); only one process can hold the port,
so stop any `miniterm`/`screen` on it first.

**Watching the console.** The script is the serial terminal: while it
runs, everything the board prints goes to its stdout and to the log
file; when it exits (match, timeout, Ctrl-C) it releases the port and
nothing is listening any more, although the board keeps running. So:

- A bounded check (`--until REGEX --timeout N`) captures the boot up
  to the marker and stops — read the rest from the log afterwards.
  Each run starts with a `===== pi-upload.py run <timestamp> =====`
  line, so the last run is everything after the last such line.
- To keep watching, run without `--until` (and `--timeout 0`, the
  default): it streams until Ctrl-C, holding the port. Stop it before
  the next upload.
- A second terminal can follow the same output live with
  `tail -f /tmp/newton-claude/serial.log`, whichever mode the script
  is in — that is the intended split between an agent driving the
  script and a person watching.
- A soak (N cold boots, each judged by markers) is a shell loop around
  `--no-upload`. Unattended soaking is the tool for intermittent
  hardware-only bugs: it turns "fails sometimes" into a measured rate,
  and a fix into a statistical verdict (a ~1-in-5 failure needs ~20
  clean boots for P ≈ 1 % that it is still there; the SD-save
  corruption hunt closed on 39 of 39 clean, `docs/project-history.md`
  §9). Unlike a bare power-toggle loop, `--no-upload` also verifies
  that the switch really cycled (it waits for the bootcode banner):

  ```bash
  L=/tmp/newton-claude/serial.log
  for i in $(seq 1 20); do
      scripts/pi-upload.py --no-upload --timeout 45 \
          --until '\*\*\* HALTED|!!!panic|REP> Welcome to NewtonScript' >/dev/null
      slice=$(awk '/^===== pi-upload.py run/{s=""} {s=s $0 "\n"} END{printf "%s", s}' "$L")
      if grep -q '\*\*\* HALTED\|!!!panic' <<<"$slice"; then echo "boot $i: HALT"; break; fi
      grep -q 'Welcome to NewtonScript' <<<"$slice" || { echo "boot $i: stall"; break; }
      echo "boot $i: clean"
  done
  ```

  (`--until` is an alternation so the run ends at the first decisive
  line either way; the verdict comes from grepping the run's slice of
  the log, which the `awk` extracts as everything after the last run
  header.) Add the dwell the investigation needs via `--timeout` and a
  later marker.
- A plain serial terminal still works for watching without uploading
  (`uv run --with pyserial python -m serial.tools.miniterm --eol LF
  /dev/cu.usbserial-BG03U2PN 115200`), but it holds the port: close it
  before the next `pi-upload.py`.

Power cycling goes through the Shortcuts app: `Pi Off` / `Pi On` each
run a Home action on the switch the Pi is plugged into. The script
runs them with stdin closed (`shortcuts run` otherwise blocks on
stdin forever and the Off never happens) and does not trust their
exit status: it waits for the firmware's `uart_2ndstage` banner
(`Raspberry Pi Bootcode`) on the wire, retries the cycle once, and
fails loudly if the board never rebooted.

Under QEMU the same script talks to a socket serial, which is how the
protocol and the SD path are tested without hardware:

```bash
# FAT32 disk image (MBR partition, as the firmware expects), 64 MB is plenty
hdiutil create -size 64m -fs "MS-DOS FAT32" -volname NHTEST -layout MBRSPUD sd.dmg
hdiutil attach sd.dmg && cp HYPERV.IMG /Volumes/NHTEST/ && hdiutil detach /Volumes/NHTEST

qemu-system-aarch64 -M raspi3b -kernel nhboot.bin \
    -device loader,file=HYPERV.IMG,addr=0x02000000 \        # what the firmware's initramfs does
    -drive if=sd,file=sd.dmg,format=raw \
    -chardev socket,id=s0,path=serial.sock,server=on,wait=on -serial chardev:s0 \
    -display none -no-reboot -monitor none
scripts/pi-upload.py --port unix:serial.sock --no-power-cycle --kernel <elf> \
    --until 'Welcome to NewtonScript' --timeout 120
```

(`nhboot.bin` is `llvm-objcopy -O binary` of
`nhboot/target/aarch64-unknown-none-softfloat/release/nhboot`; leave
out `-device loader` to exercise the no-image path, `-drive` to
exercise the persist-failure path.)

### Store-erase alert on a new build

A hypervisor build whose in-ROM patch population differs from the
previous boot's (new inline stubs, new trampolines) can trip
NewtonOS's ROM-compatibility check: the guest boots to "The internal
store was erased because a different ROM has been installed" and the
first-boot setup wizard. This is benign and known — dismiss the
alert, tap through setup, reinstall any add-on packages (Dock). It
is NOT store corruption; don't start a corruption hunt off this
alert alone.

### Serial pen injection + capture-side measurement

The see-and-measure loop for display work runs over the same wire and
a USB HDMI digitizer on the Pi's HDMI output:

- **`serial-pen-inject`** (Cargo feature, deliberately not in any
  `pi-bare-metal*` aggregate): an escape shim on the guest
  external-serial RX seam turns `~p<x>,<y>[,<hold_ms>]\n` lines from
  the host console into pen taps on the same queue mtouch feeds
  (`src/host/serial_pen.rs`). Newton coordinates (320x480). With the
  feature off the RX wiring is byte-identical to a build without it,
  so production builds cannot be driven from the wire.
- **`scripts/capture-timing.py`**: records the digitizer via ffmpeg
  avfoundation (device resolved by name, never index) and prints a
  per-frame change timeline; `grab` computes a luma-threshold bounding
  box of the painted region (the geometry regression check); `record
  --tap x,y` sends a pen tap over the serial port mid-capture, so
  tap-to-quiescent for a UI animation becomes a number.
- **`blit_timing`** (`src/diag/blit_timing.rs`, `diag` builds): per-16
  window count/total/avg/max lines for `screen.blit` (emulation) and
  `push_blit` (paint), separately attributable.

The standard benchmark: `record --seconds 15 --tap 18,453` (opens the
Extras drawer), `--tap 306,421` (closes it), `grab` for the bbox.

### Recovery and limits

- A bad upload (line noise past the CRCs, a power cut during the
  card write) leaves a container that fails validation; nhboot says
  so and waits for an upload. Nothing a hypervisor build can do
  affects nhboot.
- nhboot itself, `config.txt` and the firmware are updated only by a
  card write (`build-sd.sh <dest> <mount>`).
- The card write is per changed 512-byte sector at ~700 KB/s of PIO.
  `linker.ld.in` pins the ROM + REx blob (8.3 MiB, `.rom_blob`) at
  image offset 0x1000, ahead of `.text`/`.rodata`, so a code change
  moves only the code and data behind it; a full rewrite (~15 s)
  happens only when the blob itself changes (a different ROM
  version).
- No flow control on the link: nhboot's receive loop is tight (no
  printing inside a message) and the PL011 FIFO is 16 deep; a
  dropped byte costs one 64 KiB retry.

**Hardware status.** Verified on the Pi Zero 2 W (2026-08-29), all
driven by `pi-upload.py` from the Mac with no hands on the board:

- The firmware honours `initramfs HYPERV.IMG 0x02000000` — nhboot
  finds the magic at 0x02000000 and boots the hypervisor to the
  Welcome UI (~15 s after power-on, the same as a direct
  `kernel8.img` boot).
- Identical image: 2 ops, 3 KiB sent, 0 sectors written, 9.5 s from
  power-on to `DONE` at 1.5 M.
- Real rebuild (the `quiet` variant over the input build): 16 ops,
  400 KB sent (3.8 %), 5.0 s transfer, 12.7 s from power-on to the
  COMMIT. With the blob still inside `.rodata` at the time, persist
  rewrote 19676 of 20639 sectors in 15.7 s (Welcome UI at 38 s); with
  the blob pinned at offset 0x1000 the same rebuild rewrites 2886
  sectors in 6.3 s (Welcome UI at 28 s) — `quiet` touches most of
  `.text`, so that is the ceiling for a code change; a small edit
  moves far less.
- Persistence: a plain power-cycle (`--no-upload`) booted the
  uploaded build from the card.
- 1.5 M and 3 M baud both work. The one host-side pitfall found:
  reassigning pyserial's `timeout` re-runs `tcsetattr`, and on
  macOS's FTDI driver that re-programs a non-standard speed and drops
  buffered RX — `Link` sets the timeout once and polls `in_waiting`.

Verified under QEMU only: boot with `HYPERV.IMG` absent (the firmware
side of that case has not been observed on the board).

### Feature aggregates

The differences between QEMU and real hardware live in opt-in
backends selected by aggregate features:

| Feature | semihost | flash-persist | host-io | input | audio | Intended target |
|---|---|---|---|---|---|---|
| (default) | on | semihost | null | null | null | `cargo run` against QEMU |
| `pi-bare-metal` | off | null | null | null | null | minimal real-hw boot |
| `pi-bare-metal-sd` | off | sd | null | null | null | real-hw with persistent state |
| `pi-bare-metal-display` | off | sd | pi-fb | null | null | real-hw, full display |
| `pi-bare-metal-input` | off | sd | pi-fb | mtouch | pi-hdmi | real-hw, display + touch + audio |
| `platform-fvp-base` | on | semihost | null | null | null | FVP cycle-accurate runs |

Probe features (`sd-probe`, `fb-probe`) are additive on
top of any aggregate; each boots, tests one peripheral, and parks. The
build script accepts `PI_CARGO_FEATURES` to override the base and
`PI_EXTRA_FEATURES` to append.

`build.rs` resolves the active `flash-persist-*` / `host-io-*` /
`input-*` / `audio-*` backends through small per-axis selectors that
panic on mutually exclusive picks. To add a new backend, add the
feature in `Cargo.toml`, an arm in the relevant resolver, and a
`#[cfg(nh_*)]`-gated module under the matching directory.

Real-silicon timing differs from QEMU: `CNTFRQ_EL0 = 19_200_000 Hz`
(vs QEMU's 62.5 MHz).

## Storage — BCM2835 SDHOST + FAT32

### Controller choice (read this before reaching for a Pi 4 SD driver)

The Pi Zero 2 W routes the **micro-SD slot to the BCM2835 SDHOST
controller** at `0x3F20_2000`, not to the SDHCI-style "Arasan EMMC"
block — on this SoC the EMMC block is wired to the **on-package
BCM43436B0 Wi-Fi/BT chip via SDIO**. (Pi 4 / Pi 5 swap this around,
so Pi 4 SD code does **not** port.)

GPIO routing:
- GPIO 48–53 ALT0 → SDHOST → micro-SD slot.
- GPIO 34–39 ALT3 → Arasan EMMC → on-package WLAN/BT (SDIO).

The Pi firmware uses SDHOST to load `config.txt` / `kernel8.img` and
leaves the controller in an undefined state on handoff; the driver
reinitialises from scratch (GPIO pinmux, clock via VC mailbox,
CMD0 / CMD8 / ACMD41 enumeration, CSD parse, sector I/O).

### Stack as built

```
  ┌───────────────────────────────────────────────────┐
  │ flash_persist::sd  (dirty-block tracked NEWTON.BIN)│  ← consumer
  ├───────────────────────────────────────────────────┤
  │ embedded-sdmmc::VolumeManager (FAT32 read/write)  │  ← filesystem
  ├───────────────────────────────────────────────────┤
  │ MbrBlockDevice   (selects partition 1 = boot)     │  ← partition
  ├───────────────────────────────────────────────────┤
  │ SdHost           (sector R/W, PIO + DMA)          │  ← driver
  ├───────────────────────────────────────────────────┤
  │ BCM2710 SDHOST controller @ 0x3F20_2000           │  ← hardware
  └───────────────────────────────────────────────────┘
```

- Identification at 400 kHz / 1-bit; post-CMD7 bump to **25 MHz /
  4-bit** (SDCDIV=8, ACMD6 then `SDHCFG_WIDE_EXT_BUS`). Init prints
  `sd: bus ready (25.0 MHz, 4-bit)`.
- FAT32 via [`embedded-sdmmc`](https://github.com/rust-embedded-community/embedded-sdmmc-rs)
  0.9, vendored under `vendor/` (no_std, no allocator, tiny
  `BlockDevice` trait; local changes listed in
  `vendor/embedded-sdmmc/VENDOR.md`). Files are opened by short name
  — we control the filenames, so no LFN concerns. The partition
  table is never written.
- `GUEST_FLASH` (8 MiB) persists to `/NEWTON.BIN` on the 2 s
  autosave cadence; cold boots load it back with a fingerprint
  check. The hot path is the non-blocking per-cluster DMA save —
  see [`SD_DMA_AUTOSAVE.md`](SD_DMA_AUTOSAVE.md); the synchronous
  FAT path (CMD17/CMD24 per 512-byte sector, ~700 KB/s at 25 MHz /
  4-bit) remains the fallback for full saves and error recovery.
- `flash_persist::maybe_save` is reached via a `no-semihost` sibling
  branch of `snapshot::maybe_autosave` that runs the same wall-clock
  gate but skips snapshot work (the snapshot ring is inert on real
  hw). Easy to miss; the symptom of losing it is "init runs, file
  never written".

### Porting notes (BCM2835 SDHOST)

In case anyone else ports it from scratch (ours follows Circle's
`addon/SDCard/sdhost.cpp`, re-derived against the BCM2835 ARM
Peripherals manual):

- The `SDHCFG_*_IRPT_EN` bits are misnamed. They don't just gate IRQ
  generation — `SDHCFG_DATA_IRPT_EN` gates the FSM's data-movement
  path itself, even in polling mode. Without it the FSM walks
  READWAIT → DATAMODE but the FIFO stays empty. The trace shape of
  "CMD17 OK, FSM transitions, no errors, FIFO_FILL stuck at 0" is
  the signature.
- Don't poll `SDHSTS_DATA_FLAG` for PIO drain. It's threshold-driven
  (set when FIFO_FILL ≥ READ_THRESHOLD, clears below) and will lie
  to you in a word-at-a-time loop. Poll `SDEDM[8:4]` (FIFO_FILL)
  directly; both Linux and Circle do this.
- Data-phase completion is `SDEDM.FSM == DATAMODE | IDENTMODE`, not
  `SDHSTS.BLOCK_IRPT`. If the FSM is stuck in READWAIT (read) or
  WRITESTART1 (write) at end of transfer, write
  `SDEDM | FORCE_DATA_MODE` to kick it out.
- ACMD6 to switch the card to 4-bit must happen **before** writing
  `SDHCFG_WIDE_EXT_BUS` to the controller — the reverse drives 4
  data lines into a card still in 1-bit mode and trashes every
  transfer.
- 25 MHz / 4-bit is reliable with default GPIO pulls (CMD + DAT0..3
  pull-up, CLK pull-off via the GPPUD/GPPUDCLK1 sequence). Higher
  (50 MHz HS, UHS) would require CMD6 SWITCH_FUNCTION negotiation
  and likely tuning we haven't needed.

### Known limitations

- `BlockDevice::num_blocks` returns `u32::MAX` instead of decoding
  the CSD card size — sufficient because per-partition bounds come
  from the MBR.
- The `sd-probe` build wedges at the FAT mount handoff (a stale
  probe-tool quirk — the same mount path works in the full build;
  see the note in `SD_DMA_AUTOSAVE.md`). Don't validate
  FAT-dependent things via the probe.

## Display — VC4 framebuffer

`host-io-pi-fb` renders Newton to a mini-HDMI monitor.

```
  ┌─────────────────────────────────────────────────────┐
  │ host_io::pi_fb::push_blit                           │   ← consumes
  │   2 bpp Newton FB rect → 32 bpp surface rect        │     screen.rs
  │   VC-scaled: 1:1 LUT expand; fallback: bilinear     │     blits
  ├─────────────────────────────────────────────────────┤
  │ display::fb::alloc_guest_surface + FbInfo           │   ← per-boot
  │   small VC-scaled surface (probe + runtime          │     allocation
  │   fallback to panel-native), 32 bpp RGB, 4 KiB      │
  ├─────────────────────────────────────────────────────┤
  │ mailbox::fb_setup_and_allocate (single batched msg) │   ← VC property
  ├─────────────────────────────────────────────────────┤
  │ mailbox_call (cache flush + doorbell + response)    │   ← shared with
  │                                                     │     SDHOST clock
  └─────────────────────────────────────────────────────┘
```

The framebuffer's *physical* size is deliberately smaller than the
HDMI mode: `fb_h ≈ 480` (Newton's height, inflated slightly for the
`FIRMWARE_TOP_BAR_PX` allowance) and `fb_w` at the panel's aspect —
866×487 for a 1920×1080 mode. Newton's 320×480 2 bpp framebuffer is
painted into it **1:1** (centred horizontally, letterboxed black) and
the firmware/HVS scales the surface to the unchanged HDMI mode on
scan-out, so the CPU never resamples a pixel. At boot,
`alloc_guest_surface` verifies the firmware honoured the small
physical size without re-modesetting (returned geometry + HDMI
pixel-clock readback); any surprise falls back at runtime to a
panel-native surface with the old CPU software-bilinear scaler
(force that path for A/B testing with the `pi-fb-force-cpu-scale`
feature).

### Portrait rotation (default off — verified on hardware)

The reason VC-first matters: with the firmware scaling scan-out, a
90° rotation for a physically portrait-mounted monitor costs the CPU
nothing — the paint loop is byte-identical, only the surface shape
and the touch map change. Verified end-to-end on the Zero 2 W
(digitizer capture + tap test); OFF by default because it depends on
how the monitor is physically mounted.

- **Selection: the `pi-fb-rot90` Cargo feature**, paired with
  `display_hdmi_rotate=1` in `boot-pi/config.txt` (a commented-out
  block sits there ready). A build feature rather than a runtime
  probe because the mailbox property catalogue (the firmware-wiki
  page `src/host/mailbox.rs` cites) documents no rotation/transform
  readback tag — and config.txt can't be changed over the
  serial-upload path anyway, so flipping rotation always means SD
  card in hand; flip both together.
- **The physical-size readback is transposed under an active
  rotation** — `fb_get_physical_size` returns 1080×1920 for a
  1920×1080 mode. That is the one observable signal the rotation is
  on: `alloc_guest_surface` normalises it back to landscape for the
  geometry formulas, and treats a *landscape* readback with `rot90`
  asserted as the mismatched-pair case (feature on, config.txt line
  off) — loud log, panel-native fallback, with a note that the touch
  map is still rotated. The reverse mismatch (config.txt on, feature
  off) still paints wrong: the geometry checks reject the transposed
  readback and the CPU fallback paints landscape under a rotated
  scan-out.
- **Geometry** (`display::fb::alloc_guest_surface`, `rot90` arm):
  the surface is allocated with the panel's *transposed* aspect and
  Newton's *width* pinned to the reserved-top allowance, since
  surface columns scan out as panel rows:
  `fb_w = content_w × panel_h / (panel_h − reserved_top_px)`,
  `fb_h = panel_w × fb_w / panel_h`. For a 1920×1080 mode:
  **325×578**, Newton 320×480 at offset (0, 49), HVS scale ×3.32 —
  Newton spans ~1063 of 1080 panel rows and ~1595 of 1920 panel
  columns (vs 709×1064 unrotated: ~2.2× the pixels). Digitizer
  capture measured 1587×1064 at x 161–1747, y 0–1063 — the design
  numbers within a pixel of rounding.
- **Painting** is unchanged: Newton rows land 1:1 row-major
  (`pi_fb::paint_1to1`); the firmware rotates on scan-out. Newton is
  left-aligned on the surface x axis and centered on y. The boot
  splash needs no rotation of its own either — it paints row-major
  into the same surface, so it shows upright on the portrait-mounted
  panel (the design-phase "shows sideways" note was wrong); only its
  progress-bar width scales down to the narrow surface.
- **Rotation direction is 90° clockwise**, confirmed by capture:
  content top (title bar) lands on the signal's right edge,
  content-left at signal-top. The touch map's inversion — surface x
  from touch y, surface y from mirrored touch x
  (`input::calibrate`) — assumes exactly this and tap tests land
  correctly.
- **The CPU-bilinear fallback stays landscape-only** (a rotating
  software blit writes down panel columns — the cache-miss-per-store
  pattern that cost ~0.5 s per full fill pre-VC). If rot90 is
  selected and the VC probe falls back, `pi_fb::init` logs a loud
  WARNING and paints unrotated under the still-rotated scan-out.

Findings from the hardware pass (2026-08-31), retiring the two
flagged risks:

1. **GPU memory.** Confirmed: rotation needs the full `start.elf` +
   `fixup.dat` with `gpu_mem=64` (the cut-down `start_cd.elf` pair
   is only selectable via `gpu_mem=16`). `scripts/build-sd.sh` ships
   both pairs from the same pinned firmware commit; config.txt's
   `gpu_mem` selects at boot.
2. **`FIRMWARE_TOP_BAR_PX` under rotation: dropped.** The capture
   shows the allowance's spare columns landing at the panel *bottom*
   (surface column 0 scans out at the panel top under 90° CW, and
   Newton is left-aligned at column 0), while the firmware bar —
   where a sink shows one at all; the digitizer never does — lives
   at the top. So under rotation the allowance cannot dodge the bar
   on any sink; it only shrinks Newton. `pi_fb::RESERVED_TOP_PX` is
   therefore 0 under rot90 (surface 320×569, Newton spanning all
   1080 panel rows) and `FIRMWARE_TOP_BAR_PX` in landscape. If a
   bench-panel-style bar ever needs dodging under rotation, the fix
   is right-aligning Newton on the surface x axis (spare columns at
   the panel top), not the allowance.

### Hires Newton geometry (`pi-fb-hires` — experiment, DEFERRED)

The ROM learns its screen size from us: the display driver is the
Einstein-style REx driver whose `GetScreenInfo` we serve as a native
primitive (`peripherals::screen`), and `main.rs` feeds the model
whatever the host-IO backend mandates (`set_screen_size`, before the
guest's ERET). The `pi-fb-hires` feature derives the mandate from
the firmware panel readback — half the logical scan-out shape, so
the VC path keeps an exact ×2 HVS scale: 540×960 on the rotated
1080×1920 bench panel. Fully implemented and hardware-tested
(2026-08-31), then **deferred**; what we learned:

- **The OS reflows.** At 540×960 the ROM boots to the Welcome UI
  (QEMU and hardware), Notes lays out to the full screen, touch maps
  correctly (`input::calibrate` follows `panel_geometry()`), ink
  drawn at 320×480 comes back from the store at its coordinates, and
  the store survives the size change. ~2.8× the screen area at a
  crisp integer scale, zero added CPU cost.
- **Three native-size quirks**, all the same species — a specific
  position/bounds computation assumes 320×480 while the general
  drawing path honours the reported geometry:
  1. The ROM boot screen (Newton logo + copyright) fills its black
     background over the full screen but draws the logo block far
     off-center (portrait x 420–540, y 140–460). Cosmetic, ~2 s.
  2. Trash-crumple animation frames below Newton y=480 are never
     erased — debris until the next full redraw of that area. The
     erase machinery honours only the native height.
  3. Dates opens 480 rows tall with Notes visible beneath — a
     view-bounds assumption in the builtin app. The one functional
     annoyance.
- **The old "the ROM does not accept other sizes" claim** (early
  pi_fb, phase-0) traced to landscape-geometry experiments
  (animation debris "past the left half"). Portrait scaling works
  modulo the quirks above; landscape geometry remains untested.
- The screen model bounds the mandate at `MAX_SCREEN_W/H`
  (1280×960, blit-scratch sizing — `set_screen_size` halts loudly
  beyond it); GUEST_FB (2 MiB) fits any allowed geometry.

Next steps when resumed, in information-per-effort order: run
Einstein at 540×960 as the oracle on the same three quirks (if it
matches, they're inherent ROM behaviour and only a ROM patch — the
last-resort layer — could fix them); hunt Dates' 480 and the
animation save-under bounds in `rom.dis` (they may share one cached
screen-bounds global); note the OS-side Rotate button is the
related-but-separate `SetFeature`(orientation) stub in
`peripherals::screen`.

### Porting notes

- **Batch all FB setup tags in a single mailbox message.** The Pi
  property mailbox treats each request as an atomic transaction;
  state set in one (e.g. `set_physical_size`) does NOT persist into
  the next (`fb_allocate`). Allocating after separate-message setup
  leaves firmware defaults (size=512, pitch=32) — the VC then scans
  random DRAM as pixels.
- **Mailbox per-tag response indicator is `buf.words[4]`**, not
  `buf.words[3]` (the third header word is overall response status).
- **Force a CEA mode if the panel's native is non-standard.** The
  bench panel reports DMT 39 (1360×768); HDMI lock on DMT modes is
  borderline on cheap cables/panels and produced intermittent
  flicker that `hdmi_drive=2 / hdmi_force_hotplug=1 /
  config_hdmi_boost=7` couldn't fix. Forcing CEA mode 4 (1280×720
  @60: `hdmi_group=1 hdmi_mode=4`) is rock-solid; the panel scales
  internally.
- **Row-major blit, always.** Column-major iteration costs a fresh
  cache miss per store (pitch 5120 B ≫ 64 B line) — a full-screen
  fill drops from ~0.5 s to <50 ms when iterated row-major.
- **`avoid_warnings=1` + `disable_overscan=1`** suppress the
  firmware's icon overlay and overscan padding. Neither suppresses
  panel-side OSD strips — the bench panel has a permanent ~10 px
  white bar at the top; it lives above row 0 of our framebuffer.

### Polish candidates (none blocking)

- HVS scaling filter quality vs the old CPU bilinear is untuned
  (`scaling_kernel` in config.txt selects the firmware's kernel);
  speed was chosen first — tune quality afterwards.
- The FB region is Normal-WB with `dc cvac` per damaged row range on
  the VC path (`dc civac` full rows on the fallback); if a profile
  shows the maintenance dominating, remap Normal Non-Cacheable.

## USB input — DWC2 + TSTP MTouch

Pen input comes from the TSTP MTouch USB touchscreen
(VID 0x0416 / PID 0xC168 — full panel characterization in
[`MTOUCH.md`](MTOUCH.md)) through a deliberately minimal USB host
stack.

**Permanent scope cap: single full-speed device, no hub.** The
Zero 2 W's micro-USB OTG goes straight to the touchscreen; audio
exits via HDMI. So: no hub class, no Transaction Translator, no
split transactions, no isochronous or bulk transfers, no
device-mode, no suspend/resume. Control + interrupt transfers only.

```
  src/host/input/    PenSource seam + backends
    mod.rs           PenEvent enum, drain_into_queue
    null.rs          no-op (default for every QEMU/FVP build)
    mtouch.rs        TSTP MTouch driver — activation handshake,
                     IRQ-driven interrupt-IN, slot-0 decode, ring
    calibrate.rs     panel 1024x600 → Newton 320x480 (inverse of
                     the display transform); compile-time checks
  src/host/usb/
    mod.rs           shared types: SetupPacket, UsbError, request
                     codes, descriptor type constants
    descriptor.rs    Device/Config/Interface/Endpoint/HID parsers +
                     walk_config iterator
    enumerate.rs     standard §9.1.2 sequence (GET_DESC, SET_ADDR
                     + 50ms tDSETADDR delay, SET_CONFIG + 50ms)
    class/hid.rs     SET_IDLE / GET_REPORT / GET_DESCRIPTOR(Report)
    host/mod.rs      UsbHostController trait
    host/dwc2/       Synopsys DWC2 driver — host-mode init, control
                     transfers, IRQ-driven interrupt-IN, per-endpoint
                     DATA0/DATA1 toggle tracking
```

Wiring: the touchscreen's interrupt-IN endpoint is IRQ-driven —
`start_int_in` arms a DWC2 host channel with the core's host-channel
IRQ enabled (BCM2835 GPU IRQ source 9), and `mtouch::on_usb_irq`
harvests each report from `trap_irq`'s slim USB fast path and
re-arms. Decoded pen events feed the same pen-sample queue
(`host_io::queue`) as the QEMU/FVP host viewer, so
TScreenManager / INT_TABLET / `NativeGetSample` are oblivious to the
source. The DWC2 implementation is cross-checked against Circle's
`lib/usb/{dwhcidevice.cpp, dwhcixferstagedata.cpp, usbendpoint.cpp,
usbhostcontroller.cpp}` + `include/circle/usb/dwhci.h`.

Calibration (`src/host/input/calibrate.rs`): the MTouch always
reports in its 1024×600 logical space, physically coincident with
the panel. `host_io::painted_region()` describes where Newton landed
on the scan-out surface (offset + painted size, in surface pixels,
plus the firmware's scan-out rotation); calibrate maps touch →
surface (inverting the rotation when one is asserted) → painted
region → Newton pixel, dropping touches in the letterbox bands. The
same code serves the VC-scaled surface and the panel-native
fallback — the linear maps compose either way.

### Porting notes (each cost a real-hw round-trip)

- **`HCDMA` takes the GPU-bus uncached alias, not the bare ARM PA.**
  Pi 2/3/Zero-2-W's DWC2 AHB master sees DRAM through
  `pa | 0xC0000000` (Circle's `BUS_ADDRESS` on
  `GPU_MEM_BASE = GPU_UNCACHED_BASE`). A bare PA gives `XACT_ERR` on
  the first transaction every time. The cache-flush call still uses
  the ARM VA — only HCDMA gets the alias.
- **USB 2.0 §9.2.6.3 `tDSETADDR` is real.** Skipping the 50 ms
  post-SET_ADDRESS delay makes the next SETUP at addr=1 hit
  XACT_ERR — the device is still listening at addr=0. Same for
  SET_CONFIGURATION.
- **DMA mode does NOT auto-advance the host's data-toggle PID.** The
  host driver must track DATA0↔DATA1 across transfers and pass the
  expected PID in HCTSIZ; otherwise the first interrupt-IN packet
  works and every subsequent one silently drops with
  `DATA_TGL_ERR`. Circle's `CUSBEndpoint::SkipPID()` is the
  reference; ours lives on `Dwc2::int_next_pid[16]`.
- **HID 1.11 §8.6 inserts the Report ID byte.** The MTouch
  activation reply is `[0x03, 0x0a]` (`[ReportID=3,
  ContactCountMax=10]`) on the wire — Linux's `hid-multitouch`
  strips the ID byte before userland, which is what `usbhid-dump`
  shows.
- **`FRM_OVRUN` on a periodic-IN is the *normal* idle response, not
  an error.** A NAKed frame sets FRM_OVRUN + ChHltd; classify it as
  "no data this poll" (no log) or it spams the console at poll
  cadence and hides real bus errors.
- **Newton's pen detection cares about the specific pressure
  value.** Einstein's `TScreenManager::PenDown` default pressure is
  4; other values reach `NativeGetSample` cleanly but the UI
  silently ignores them. Match Einstein byte-for-byte:
  `PRESSURE: u16 = 4` in `input::drain_into_queue`.

## HDMI audio — VC4 MAI

HDMI audio on Pi 0–3 goes through the VC4 HDMI block's MAI
("Multi-channel Audio Interconnect") FIFO at `0x3F90_2000`, fed by
SPDIF / IEC 60958 subframes that the HDMI encoder embeds into the
video blanking interval. It does **not** go through the BCM2835
PCM/I2S peripheral at `0x3F20_3000` — that block only reaches
GPIO 18–21 (external I²S DAC). `src/host/audio/pi_hdmi.rs` drives the MAI
path. References: Circle `lib/sound/hdmisoundbasedevice.cpp`, Linux
`drivers/gpu/drm/vc4/vc4_hdmi.c`.

The stack:

1. **`audio` module seam** (`src/host/audio/mod.rs`) — same shape as the
   `host_io` / `input` axes; backend selected by `audio-*` features,
   resolved in `build.rs` to `cfg(nh_audio_*)`. Null default for
   QEMU/FVP.
2. **VC4 HDMI MAI bring-up** (`src/host/audio/pi_hdmi.rs`) — MAI_CTL
   reset + flush, MAI_FMT = 44.1 kHz PCM, MAI_CONFIG bit-reverse +
   format-reverse + channel-mask = stereo, MAI_CHANNEL_MAP =
   0b001000 (Pi ≤3 stereo L+R), AUDIO_PACKET_CONFIG = stereo +
   B-preamble, CRP_CFG external-CTS + N=5644 for 44.1 kHz.
3. **CEA Audio InfoFrame** (PCM stereo 16-bit 44.1 kHz) written into
   the HDMI RAM packet slot, enabled via `HDMI_RAM_PACKET_CONFIG`.
4. **Newton sample feed** — Newton's 22.05 kHz mono BE-S16 is
   sample-and-hold upsampled to 44.1 kHz stereo (exact 2× ratio, no
   interpolator), pushed into a ring from `sound::handle` subfn 0x07.
5. **SPDIF encoding** — `pi_hdmi::refill_mai_dma_ring`, called from
   `schedule_output` (subfn 0x07) and from `on_mai_dma_done` on
   period completion, builds two IEC 60958 subframes per frame
   (24-bit sample in bits 27..4, parity in bit 31, B-preamble each
   192-frame block) into the DMA TX ring via
   `pi_hdmi::encode_iec958_pair`.
6. **Cyclic DMA feed** — the MAI FIFO is drained by BCM2835 DMA
   channel 4 paced by the HDMI DREQ (17), a looped CB chain that
   never stops (silence subframes between clips keep the receiver
   from renegotiating); per-period completion IRQs advance the
   consumer counter. See `src/host/host_dma.rs` and the "DMA
   TX ring" section in `pi_hdmi.rs`.
7. **Buffer-completion IRQ to the guest** — once the ring drains to
   the producer's mark, raise the output-interrupt mask the kernel
   passed in subfn 0x1F so subfn 0x07 fires with the next half of
   the ping-pong.

Caveats for hardware other than the bench panel:

- **MAI register bitfields** come from Circle + Linux
  cross-reference rather than a datasheet. `bringup_mai` in
  `pi_hdmi.rs` is the place to twiddle if a different panel
  misbehaves.
- **CTS** is computed for the 27 MHz pixel clock of the forced CEA
  mode 4 (720p). Panels at non-standard modes (1366×768, 1024×600,
  …) need CTS recomputed against the real pixel clock; pitch-shifted
  or stuttering audio with no underrun warnings points here.
- **`hdmi_drive=2`** in `boot-pi/config.txt` is required — without
  it the encoder strips audio packets regardless of what reaches
  MAI_DATA.

Out of scope permanently: PWM audio (no 3.5 mm jack; needs an
external low-pass filter) and PCM/I2S to GPIO (needs an external
DAC).

## Kernel log — DMA-fed PL011 TX

A polled PL011 write busy-waits on `FR.TXFF` ~87 µs per byte at
115200 baud once the FIFO fills; a 100-byte `kprintln!` burns ~6 ms
of EL2 CPU — wider than the audio pump's tolerance, so logging would
glitch audio audibly. Instead, `kprintln!` enqueues and returns in
microseconds; BCM2835 DMA paced by PL011 TX DREQ drains the wire at
its baud-rate ceiling.

1. **`src/host/host_dma.rs`** — BCM2835 DMA driver (register
   map, CB layout, TI/CS bit fields, DREQ table, IRQ-controller
   offsets cited against BCM2835 ARM Peripherals (2012-02-06)
   §1.2.3–4, §4.2.1, §7.5; the DMA rows Broadcom's IRQ table leaves
   blank are cross-checked against Circle's `bcm2835int.h`).
   Channel 5 = UART TX (also: channel 4 = HDMI MAI, channel 6 = SD —
   see the respective sections).
2. **`src/host/platform/raspi3b.rs`** — `enable_bcm2835_irq` /
   `bcm2835_irq_pending_1` for the ARM Peripherals IC at
   `0x3F00_B000`. DMA channel N → GPU IRQ source `16 + N`. CNTHP
   still arrives via the BCM2836 local-peripheral block at
   `0x4000_0040`.
3. **`src/host/console.rs::tx_dma`** — 16384-slot ring. `enqueue` masks
   DAIF, copies bytes in, `maybe_kick` builds one CB per contiguous
   tail→end-of-ring segment; completion-IRQ dispatch in `trap_irq`
   acks `CS.INT`/`END`, advances tail, re-kicks. Drop-newest with a
   `<<N bytes dropped>>` marker on the next call with room.

Gotchas:

- **Storage is one u32 per character, not one byte.** BCM2835 DMA
  has no 8-bit transfer width; each 32-bit write to PL011 DR
  transmits only the low 8 bits (PL011 TRM §3.3.1), so a byte source
  delivers 1 of every 4 characters. Each ring slot holds one
  character in the low octet of a u32 (`RING_LEN × 4 = 64 KiB`).
- **PL011 `DMACR.TXDMAE` must be set** (bit 1 at offset 0x48) or the
  chip never asserts TX DREQ and the DMA waits forever (PL011 TRM
  §3.3.8). Set inside `tx_dma::init` only after `host_dma::init`
  succeeds, so a failed bring-up leaves PL011 polled-only.
- **`tx_dma::init` must run after `mmu::init`.** Pre-MMU, the A53
  treats RAM as Normal Non-cacheable; `LDXR/STXR` on that memory
  type is CONSTRAINED UNPREDICTABLE (Rust compiles `AtomicU32` RMWs
  to `LDXR/STXR` on v8.0), so an early `enqueue` aborts with no
  vector installed.

Debug facility: `console::write_str_polled` + `raw_print!` /
`raw_println!` bypass the ring via busy-wait — for when the DMA path
itself is suspect.

Scoped to `cfg(all(no-semihost, platform-raspi3b))`; QEMU
(semihosting) and FVP paths are unchanged.

## Remaining work

- **Newton serial port.** PL011 carries the kernel log; the guest's
  serial port needs a separate host-side sink/source.
- **PCMCIA images.** Newton flash-card images map naturally to files
  on the SD card via the existing flash-persist backend; not wired.
- **Snapshot ring on real hw — deferred.** Snapshots accelerate the
  rewind-by-2s debug loop on QEMU/FVP; on real silicon the boot
  completes, so the loop isn't load-bearing. Revisit if a
  real-hw-only bug demands it (the SD/FAT stack could host the
  slots).
- **Cores 1–3.** Parked by firmware in WFE spin-table state; we
  neither wake nor re-park them. Document the contract if we ever
  use them.
- **Thermal at sustained 1 GHz** — no issues observed; re-verify on
  long runs (HIGHLEVEL.md §13.5 called it minor).
