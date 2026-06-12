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
- Micro-SD card (FAT32 boot partition; firmware + image + `NEWTON.BIN`).
- USB-TTL serial cable (3.3 V CMOS, NOT 5 V RS-232). GPIO 14 = TXD,
  GPIO 15 = RXD, common GND on GPIO 6/9/14/20/25/30/34/39.
- Micro-USB power supply (the data port; the Zero 2 W has no
  dedicated PWR-IN).
- Mini-HDMI cable + panel. The bench panel is a small Pi-targeted
  1280×720-capable display with speakers (HDMI audio) and an
  integrated TSTP MTouch USB touchscreen (see
  [`MTOUCH.md`](MTOUCH.md)).
- USB OTG adapter for the touchscreen.
- Host machine running `minicom` / `picocom` / `screen` for serial.

## Pi firmware facts

These come from the raspberrypi.com docs and the raspberrypi/tools
armstub source, not memory. Re-verify before relying.

### EL handoff (Pi 0/2/3/4 with `arm_64bit=1`)

Verified by reading `armstub8.S`
(`github.com/raspberrypi/tools/blob/master/armstubs/armstub8.S`) and
confirmed on the actual board (`pi-probe` prints `CurrentEL = 2`,
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
drives PL011 at `0x3F20_1000` (`src/uart.rs`,
`src/platform/raspi3b.rs`), so `dtoverlay=disable-bt` in `config.txt`
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
- `kernel8.img` — our raw image loaded at `0x80000` (must match
  `linker.ld`).
- `NEWTON.BIN` — the persisted 8 MiB guest flash (created on first
  save).

Firmware blobs are pinned to raspberrypi/firmware commit
`8fce67a9ec5668fb8d42d215c9ec4c199340bf41` and cached under
`target/pi-firmware-cache/` by `scripts/build-sd.sh`.

Linker note: `.eh_frame_hdr` is in the linker scripts' DISCARD list.
A binary with no `.rodata` (string literals folded into `.text`)
otherwise gets `.eh_frame_hdr` placed at VMA 0x80000, shifting
`_start` and crashing on the leading UDFs.

## Building and booting

```bash
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest> [sd-mount]
```

assembles the full boot partition (pinned firmware + `config.txt` +
`kernel8.img`) under `<dest>` and optionally rsyncs it to a mounted
card.

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

Probe features (`sd-probe`, `fb-probe`, `sd-probe-trace`,
`usb-probe`, plus the standalone `pi-probe` bin) are additive on top
of any aggregate; each boots, tests one peripheral, and parks. The
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
  │   2 bpp Newton FB rect → 32 bpp panel rect          │     screen.rs
  │   nearest-neighbor 1.5x, centre-x offset            │     blits
  ├─────────────────────────────────────────────────────┤
  │ display::fb::alloc_native + FbInfo                  │   ← per-boot
  │   panel native size, 32 bpp RGB, 4 KiB align        │     allocation
  ├─────────────────────────────────────────────────────┤
  │ mailbox::fb_setup_and_allocate (single batched msg) │   ← VC property
  ├─────────────────────────────────────────────────────┤
  │ mailbox_call (cache flush + doorbell + response)    │   ← shared with
  │                                                     │     SDHOST clock
  └─────────────────────────────────────────────────────┘
```

Newton's 320×480 2 bpp framebuffer scales 1.5× → 480×720, painted
centred horizontally on a 1280×720 panel (vertical fills exactly;
400 px black bands left and right).

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

- 1.5× nearest-neighbor produces visible jaggies; bilinear or
  2×-integer-with-letterbox would look cleaner.
- The FB region is Normal-WB with `dc_civac` per blit; if a profile
  shows the maintenance dominating, remap Normal Non-Cacheable.
- The 1.5× factor assumes a 720-line output; a 1080p panel wants
  2.25× or a different scaler.

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
  src/input/         PenSource seam + backends
    mod.rs           PenEvent enum, drain_into_queue
    null.rs          no-op (default for every QEMU/FVP build)
    mtouch.rs        TSTP MTouch driver — activation handshake,
                     IRQ-driven interrupt-IN, slot-0 decode, ring
    calibrate.rs     panel 1024x600 → Newton 320x480 (inverse of
                     the display transform); compile-time checks
  src/usb/
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
    dispatch.rs      UsbDeviceDriver trait (device seam)
  src/usb_probe.rs   standalone bin — reads DWC2 GSNPSID, confirms
                     the OTG core is alive
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

Calibration (`src/input/calibrate.rs`): touch 0..1024 × 0..600 maps
to the 1280×720 panel; Newton paints the centre 480×720. Letterbox
bands (touch X < 320 or ≥ 704) are dropped; in-region,
`newton_x = (touch_x - 320) * 320 / 384`,
`newton_y = touch_y * 480 / 600`. Compile-time spot checks assert
corners + centre.

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
GPIO 18–21 (external I²S DAC). `src/audio/pi_hdmi.rs` drives the MAI
path. References: Circle `lib/sound/hdmisoundbasedevice.cpp`, Linux
`drivers/gpu/drm/vc4/vc4_hdmi.c`.

The stack:

1. **`audio` module seam** (`src/audio/mod.rs`) — same shape as the
   `host_io` / `input` axes; backend selected by `audio-*` features,
   resolved in `build.rs` to `cfg(nh_audio_*)`. Null default for
   QEMU/FVP.
2. **VC4 HDMI MAI bring-up** (`src/audio/pi_hdmi.rs`) — MAI_CTL
   reset + flush, MAI_FMT = 44.1 kHz PCM, MAI_CONFIG bit-reverse +
   format-reverse + channel-mask = stereo, MAI_CHANNEL_MAP =
   0b001000 (Pi ≤3 stereo L+R), AUDIO_PACKET_CONFIG = stereo +
   B-preamble, CRP_CFG external-CTS + N=5644 for 44.1 kHz.
3. **CEA Audio InfoFrame** (PCM stereo 16-bit 44.1 kHz) written into
   the HDMI RAM packet slot, enabled via `HDMI_RAM_PACKET_CONFIG`.
4. **Newton sample feed** — Newton's 22.05 kHz mono BE-S16 is
   sample-and-hold upsampled to 44.1 kHz stereo (exact 2× ratio, no
   interpolator), pushed into a ring from `sound::handle` subfn 0x07.
5. **SPDIF encoding** — `audio::pump` (trap-IRQ and sync-trap tails)
   builds two IEC 60958 subframes per frame (24-bit sample in bits
   27..4, parity in bit 31, B-preamble each 192-frame block) into
   the DMA TX ring.
6. **Cyclic DMA feed** — the MAI FIFO is drained by BCM2835 DMA
   channel 4 paced by the HDMI DREQ (17), a looped CB chain that
   never stops (silence subframes between clips keep the receiver
   from renegotiating); per-period completion IRQs advance the
   consumer counter. See `src/host_dma.rs` and the "DMA
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

1. **`src/host_dma.rs`** — BCM2835 DMA driver (register
   map, CB layout, TI/CS bit fields, DREQ table, IRQ-controller
   offsets cited against BCM2835 ARM Peripherals (2012-02-06)
   §1.2.3–4, §4.2.1, §7.5; the DMA rows Broadcom's IRQ table leaves
   blank are cross-checked against Circle's `bcm2835int.h`).
   Channel 5 = UART TX (also: channel 4 = HDMI MAI, channel 6 = SD —
   see the respective sections).
2. **`src/platform/raspi3b.rs`** — `enable_bcm2835_irq` /
   `bcm2835_irq_pending_1` for the ARM Peripherals IC at
   `0x3F00_B000`. DMA channel N → GPU IRQ source `16 + N`. CNTHP
   still arrives via the BCM2836 local-peripheral block at
   `0x4000_0040`.
3. **`src/uart.rs::tx_dma`** — 16384-slot ring. `enqueue` masks
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

Debug facility: `uart::write_str_polled` + `raw_print!` /
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
