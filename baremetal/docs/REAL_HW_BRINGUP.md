# Pi Zero 2 W bring-up plan

**Goal:** boot the current `newton-hypervisor` image on a real Raspberry
Pi Zero 2 W (BCM2710A1, Cortex-A53 ×4), ultimately reaching the same
ceiling as the QEMU/FVP boot — ideally further, since real silicon
doesn't have QEMU's banked-register quirks.

This is a parallel workstream to the phase-B debugging in `PLAN.md`. It
moves at its own pace; nothing here blocks the live stall investigation.

## Why now (and why "now" is incremental, not all-at-once)

The QEMU + FVP boot now runs ~268 ResolveFault commits, renders the boot
splash, and exercises real peripheral and CP15 code paths. Enough of the
hypervisor is exercised that a real-silicon checkpoint is informative
rather than premature. But the gap between "boots in QEMU" and "boots on
the Zero" is fragmented into independent pieces — boot handoff, linker,
flash storage, snapshot storage, display, input. Each can be closed in
isolation, and the earliest checkpoints are cheap.

Phase 0 (EL2 handoff + UART) is a half-day exercise that closes Open
Question §16.1 and de-risks every later phase. Everything after that is
optional and can be sequenced against actual need.

## Hardware kit

Minimum for Phase 0–1:

- Pi Zero 2 W board.
- Micro-SD card (any size; ROM + REx total ~10 MiB).
- USB-TTL serial cable (3.3 V CMOS, NOT 5 V RS-232). GPIO 14 = TXD,
  GPIO 15 = RXD, common GND on GPIO 6/9/14/20/25/30/34/39.
- Micro-USB power supply (the data port; the Zero 2 W has no dedicated
  PWR-IN).
- Host machine running `minicom` / `picocom` / `screen` for serial.

Phase 3+ adds:

- Mini-HDMI cable + monitor (for the framebuffer path).
- USB OTG adapter + touchscreen (or a UART-tunnelled pen-event source
  during early bring-up).

## Pi firmware reference (verified facts)

These come from the actual raspberrypi.com docs and the raspberrypi/tools
armstub source, not memory. Re-verify before relying.

### EL handoff (Pi 0/2/3/4 with `arm_64bit=1`)

Verified by reading `armstub8.S`
(`github.com/raspberrypi/tools/blob/master/armstubs/armstub8.S`): the
default stub does

```
mov x0, #SPSR_EL3_VAL          ; SPSR_EL3_MODE_EL2H
msr spsr_el3, x0
adr x0, in_el2
msr elr_el3, x0
eret
```

so **the firmware hands off `kernel8.img` at EL2h** by default. Secondary
cores park in a WFE spin-table loop at memory offsets 0xe0/0xe8/0xf0
(core 1/2/3 entry pointers). Kernel entry address loaded from offset
0xfc — firmware picks `0x80000` by default for `arm_64bit=1`.

This is conditional on:
- `arm_64bit=1` in `config.txt`.
- No `kernel_old=1` (which "disables the stub entirely, so your kernel
  can load to 0 and run on all 4 cores in EL3 on startup" — per the
  raspberrypi.com forum thread t=362613).
- No custom `armstub=<file>` overriding the default.

§16.1 in `HIGHLEVEL.md` is therefore effectively answered for the
default firmware path. Phase 0 below remains a useful confirmation
that nothing is unexpectedly different on the actual Zero 2 W in our
hands.

### UART routing on GPIO 14/15

Verified from the dt-blob/overlay README (raw file in raspberrypi/
firmware on master):

> `disable-bt`: "Disable onboard Bluetooth on Bluetooth-capable Raspberry
> Pis. On Pis prior to Pi 5 this restores UART0/ttyAMA0 over GPIOs 14 &
> 15."

So on the Pi Zero 2 W (BCM2710A1) **PL011 (UART0) is wired to the onboard
Bluetooth chip by default**, not to GPIO 14/15. Without intervention,
the GPIO header carries the **mini-UART** (UART1/ttyS0). The hypervisor
already drives PL011 at `0x3F20_1000` (`src/uart.rs`,
`src/platform/raspi3b.rs`), so for Phase 0 we put `dtoverlay=disable-bt`
in `config.txt` to route PL011 to GPIO 14/15.

`enable_uart=1` per the raspberrypi.com config.txt page:

> "enable_uart=1 (in conjunction with `console=serial0,115200` in
> cmdline.txt) requests that the kernel creates a serial console,
> accessible using GPIOs 14 and 15 (pins 8 and 10 on the 40-pin header)."

`uart_2ndstage=1`:

> "If uart_2ndstage is 1 then enable debug logging to the UART. This
> option also automatically enables UART logging in start.elf."

So `uart_2ndstage=1` is a useful early checkpoint: if it produces no
firmware-side output on the wire, the problem is upstream of our code
(SD layout, GPIO ALT mode, baud divisor, etc.).

### Boot partition contents

For a Pi Zero 2 W (BCM2710A1) bare-metal boot:

- `bootcode.bin` — GPU-side stage-1 loader; brings up DRAM.
- `start.elf` — main GPU firmware; sets up everything else.
- `fixup.dat` — memory-split parameters consumed by `start.elf`.
- `config.txt` — our settings.
- `kernel8.img` — our raw image loaded at `0x80000`.

(Pi 4 and Pi 5 use an SPI EEPROM bootloader instead of `bootcode.bin`,
but Pi Zero 2 W still uses the SD-card-loaded stage-1.)

Source the firmware blobs from
`github.com/raspberrypi/firmware/tree/master/boot`. Pin to a specific
commit and record the SHA in this doc once we know what works.

## Phase 0 — `kmain` at EL2, "Hello, EL2" over PL011

**Closes:** Open Question §16.1 (EL2 handoff on real Pi firmware) in
practice rather than in theory.
**Effort:** half a day to a day.
**Independent of:** everything else in this plan.

A standalone `pi-probe` binary (separate `[[bin]]` in `Cargo.toml`, no
linkage to the hypervisor proper) that brings up PL011 directly (no
semihosting), reads `CurrentEL`, `MIDR_EL1`, `MPIDR_EL1`, prints them,
WFE-loops.

### Tasks

1. **`config.txt`** (see `boot-pi/config.txt` in this repo):
   ```
   arm_64bit=1
   kernel=kernel8.img
   enable_uart=1
   uart_2ndstage=1
   dtoverlay=disable-bt
   ```
   `disable-bt` is the key line — without it the GPIO header carries
   the mini-UART, not the PL011 our code drives.

2. **`scripts/build-sd.sh <dest>`** — assembles the SD boot partition:
   firmware blobs (pinned commit) + `config.txt` + our `kernel8.img`.

3. **`src/pi_probe.rs`** — standalone `[[bin]]` that depends on nothing
   from the main crate. Pulls in `boot.s` via `global_asm!` and
   reaches `kmain` from there. PL011 setup duplicated inline (no
   semihosting path).

4. **Outcomes.**
   - `EL=2` over the serial line: §16.1 confirmed positively on the
     actual hardware in our hands. Proceed to Phase 1.
   - Garbage characters: baud / clock mismatch. PL011 clock should be
     48 MHz (firmware default) — if it's actually different, set
     `init_uart_clock=48000000` explicitly.
   - Firmware banner from `uart_2ndstage=1` but nothing from us: our
     binary isn't running. Check the load address in `linker.ld`
     (`0x80000`) matches the firmware's default for `arm_64bit=1`.
   - No output at all: serial wiring, GPIO ALT mode, or
     `dtoverlay=disable-bt` missing. Swap TX/RX first.

5. **Capture.** Update `HIGHLEVEL.md` §16.1 to "answered on real Zero
   2 W on \<date\>; default armstub → EL2h confirmed".

## Phase 1 — Full hypervisor `kmain` running, ROM patched, ERET

**Closes:** "the hypervisor image runs on real silicon".
**Effort:** 1–3 days.
**Depends on:** Phase 0.

Drop the actual `newton-hypervisor` binary (built with
`platform-pi-zero2w`) onto the SD card. Goal: get all the way through
ROM load + patches + ERET to guest. Doesn't need to boot Newton — just
needs to not crash before the first guest-side trap.

### Tasks

1. **Replace SD-card stub with the real binary.** Same boot pipeline as
   Phase 0; just a different `kernel8.img`.

2. **ROM source on real hardware.** Currently the ROM is embedded in
   the binary or loaded via semihosting. For real hw:
   - **Option A (simplest):** embed the ROM in the binary via
     `include_bytes!`. ~8 MiB extra in `kernel8.img`. Acceptable.
   - **Option B:** load from FAT32 on the boot partition. Requires an SD
     driver. Defer to Phase 2 unless the binary is uncomfortably large.

3. **Disable semihosting-dependent paths at compile time.**
   - `host-io`: default to `host-io-null` for real-hw builds. No display
     until Phase 3.
   - `flash-persist`: default to `flash-persist-null` until Phase 2.
     Flash writes go nowhere; the guest sees a fresh flash every boot.
   - `snapshot.rs`: gate the autosave hook behind a `cfg(feature =
     "snapshot-semihost")` or similar. Off by default on real hw.
   - All three are already structured for backend selection; just need
     the real-hw build profile to pick the null backends.

4. **Timer cadence sanity check.** QEMU TCG and FVP run at very
   different wall-clock speeds. Real silicon will be different again.
   `AUTOSAVE_INTERVAL_MS = 2000` and any IRQ-storm thresholds may need
   adjustment based on real-silicon timing — note discrepancies in this
   doc as they show up.

5. **Outcomes.**
   - Serial log shows the same boot trace as QEMU/FVP up to whatever
     ceiling exists. Any divergence is the interesting data point.
   - If a divergence is "QEMU bug" territory, FVP likely already agrees
     with real hw — cross-check there first before fixing.

## Phase 2 — SD-card storage (FAT32 on the boot partition)

**Closes:** flash writes survive reboot on the Zero; serial-log /
snapshot bytes have somewhere to land.
**Effort:** ~1 week, dominated by the block-device driver.
**Depends on:** Phase 1.

The current `flash-persist-semihost` backend writes to
`$HOME/.newton/flash.bin` via Arm semihosting — no equivalent on real
silicon. The plan: read and write **files on the same FAT32 partition
the firmware booted from**, so existing SD-card contents (firmware
blobs + `config.txt` + `kernel8.img`) are preserved. No re-partitioning
required.

### Stack we're building

```
  ┌───────────────────────────────────────────────────┐
  │ flash_persist::sd    snapshot::sd    serial::tee  │   ← consumers
  ├───────────────────────────────────────────────────┤
  │ embedded-sdmmc::VolumeManager (FAT32 read/write)  │   ← filesystem
  ├───────────────────────────────────────────────────┤
  │ MbrBlockDevice   (selects partition 1 = boot)     │   ← partition
  ├───────────────────────────────────────────────────┤
  │ Bcm2835SdHost    (raw 512-byte sector R/W)        │   ← driver
  ├───────────────────────────────────────────────────┤
  │ BCM2710 SDHOST controller @ 0x3F20_2000           │   ← hardware
  └───────────────────────────────────────────────────┘
```

### Controller choice (read this before reaching for a Pi 4 SD driver)

The Pi Zero 2 W routes the **micro-SD slot to the BCM2835 SDHOST
controller**, not to the SDHCI-compatible "Arasan EMMC" block —
on this SoC the EMMC block is wired to the **on-package
BCM43436B0 Wi-Fi/BT chip via SDIO** instead. (Pi 4 / Pi 5 swap this
around with a separate EMMC2 controller for the card slot, so Pi 4 SD
code does **not** port.)

GPIO routing on Pi Zero 2 W:

- GPIO 48–53 ALT0 → SDHOST → micro-SD slot.
- GPIO 34–39 ALT3 → Arasan EMMC → on-package WLAN/BT (SDIO).

The Pi firmware (`bootcode.bin` + `start_cd.elf`) uses SDHOST to load
`config.txt` / `kernel8.img` and leaves the controller in an
undefined state on handoff. We must reinitialise: GPIO pinmux,
clock setup, CMD0 / CMD8 / ACMD41 enumeration, CSD parse, then 512-
byte sector R/W in polled mode.

### Crate choice — `embedded-sdmmc` over `fatfs`

[`embedded-sdmmc`](https://github.com/rust-embedded-community/embedded-sdmmc-rs)
0.9 (Jun 2025) wins on the dimensions that matter here:

- `#![no_std]`, **no allocator required**. Static sizing via
  `VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>` — defaults
  4/4/1 are already more than we need.
- FAT32 read + write, including `ReadWriteCreateOrAppend` for the
  flash-persist append-only path.
- `BlockDevice` trait surface is tiny: `read(&mut [Block], BlockIdx)`,
  `write(&[Block], BlockIdx)`, `num_blocks() -> BlockCount`. Trivial
  to implement against a polled-mode SDHOST driver.
- MIT OR Apache-2.0.

[`fatfs`](https://github.com/rafalh/rust-fatfs) is more feature-
complete (LFN, FSInfo, etc.) but the on-crates.io release is from
2020, license is MIT-only, and it expects a `Read+Write+Seek`
adapter rather than a block device. Skip.

### Tasks

1. **`src/sd/sdhost.rs` — BCM2835 SDHOST driver.** Polled, no IRQs,
   no DMA. State machine ported from
   [Circle `addon/SDCard/sdhost.cpp`](https://github.com/rsta2/circle/blob/master/addon/SDCard/sdhost.cpp)
   (P. Elwell @ RPi Trading, ported by R. Stange). Roughly 1.3 kLOC
   C++ → expect ~600–800 Rust LOC plus a registers module. Expose
   `read_sector(lba, &mut [u8; 512])` and `write_sector(lba, &[u8;
   512])`. Validate against known-good sectors (firmware blobs are
   readable from a host script first so we can diff).
2. **`src/sd/mbr.rs` — MBR partition table parse.** ~80 lines. Find
   partition 1 (FAT32, type 0x0B/0x0C), expose start LBA + length to
   wrap the raw block device into a partition-relative one. We never
   touch the partition table on disk.
3. **`embedded-sdmmc` integration.** Wire the partition-relative
   block device into `VolumeManager`. Mount as RW. Open files by
   short name (we control the filenames so no LFN concerns).
4. **`flash-persist-sd` backend.** New `flash_persist::sd` module
   implementing `FlashStore` against `embedded-sdmmc`. File layout
   TBD — likely `NEWTON/FLASH.BIN` (single 8 MiB file, dirty-block-
   tracked, the same shape as the semihost backend). Add the
   `flash-persist-sd` Cargo feature and `pi-bare-metal-storage`
   aggregate. Replace `flash-persist-null` in the `pi-bare-metal`
   feature.
5. **Snapshot backend (defer; see Phase 3).** Same crate, different
   files (`NEWTON/SNAP0.BIN`..`SNAP3.BIN`). 14 MiB × 4 slots = 56
   MiB; FAT32 cluster math is fine.
6. **Serial-log tee.** Independent, cheap, very useful: a
   `serial-log-sd` build option that mirrors `kprintln!` to
   `NEWTON/SERIAL.LOG` so post-mortem analysis of real-hardware
   runs doesn't require a host attached to PL011. Probably won't
   tee every byte (FAT writes are not free); buffer 4 KiB and flush
   on overflow or on `halt()`.

### Risks / unknowns

- **SDHOST driver is the long pole.** Circle's code mixes state
  machine with Circle-specific OS glue (timer/IRQ/GPIO classes); the
  port needs careful re-implementation against the BCM2835 ARM
  Peripherals manual, including CRC quirks and clock-tuning. No
  drop-in Rust prior art exists (rust-embedded-community discussion
  #134 acknowledges this). Plan to validate it standalone with
  polled-mode reads against known sectors before adding FAT on top.
- **Card-write atomicity.** Power loss mid-write can shred FAT. We're
  not putting irreplaceable data on the card (flash + snapshots can
  be regenerated by a cold boot), so the consequence is "lose state",
  not "brick the SD card", but we should at least journal the
  flash-persist writes the same way the semihost backend does.
- **Interaction with `flash_persist::maybe_save`'s wall-clock gate.**
  An SD write is much slower than a `SYS_WRITE`; the 2-s autosave
  cadence may be too tight. Measure once the driver lands.
- **Card variability.** Real-silicon testing should include both a
  small / slow card and a large / fast one to make sure we're not
  fitting only one timing profile.

### Fallback if SDHOST proves hostile

Slot the original "UART tunnel" idea in as a `flash-persist-uart`
backend — flash blocks shipped over the mini-UART to a host script.
Slow but trivial; gives us a working flash-persist on real silicon
while the SDHOST driver matures.

## Phase 3 — Snapshot ring on real silicon

**Closes:** the snapshot/resume workflow works on the Zero.
**Effort:** 1 day, once Phase 2 has a real storage path.
**Depends on:** Phase 2.

The autosave ring (`snapshot.rs`) currently calls semihosting `SYS_WRITE`
for ~14 MiB every 2 s. On real hw:

- Reuse the Phase 2 SD-card / FAT backend. The four snapshot slots
  (56 MiB total) fit easily on the existing FAT32 boot partition.
- Or accept that real-hw runs are cold-boot only and leave the ring
  disabled. For pure regression testing this is fine; for iterative
  hypervisor-code debugging the QEMU/FVP loop is faster anyway.

Snapshot compatibility across host platforms is already gated on a ROM
fingerprint; the storage layer change doesn't break that.

## Phase 4 — Display (closed 2026-05-12)

Newton renders end-to-end to a mini-HDMI monitor. Build the full
real-hw image with:

```bash
PI_KERNEL_BIN=newton-hypervisor PI_CARGO_FEATURES=pi-bare-metal-display \
  scripts/build-sd.sh /tmp/sd /Volumes/bootfs
```

The `pi-bare-metal-display` aggregate combines `platform-raspi3b`,
`no-semihost`, `flash-persist-sd`, and `host-io-pi-fb`. After
power-on the panel shows Newton's boot sequence in a centred
480×720 region, byte-identical (up to scaling) to the QEMU/FVP
host-viewer output.

### Stack as built

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
centred horizontally on a 1280×720 panel. Vertical fills the
panel exactly; horizontal leaves 400 px black on each side.

### Bring-up lessons

In rough order of how much they cost to find:

- **Batch all FB setup tags in a single mailbox message.** The Pi
  property mailbox treats each request as an atomic transaction;
  state set in one (e.g. `set_physical_size`) does NOT persist
  into the next (`fb_allocate`). Allocating after separate-message
  setup leaves you with firmware defaults (size=512, pitch=32),
  the VC then scans random DRAM as pixels and you see crazy flicker
  on top of garbage. One message, seven tags, fixed.
- **Force a CEA mode if the panel's native is non-standard.** The
  panel on the bench is a small Pi-targeted display reporting DMT
  39 (1360x768). HDMI lock on DMT modes is borderline on cheap
  cables / panels — produced intermittent flicker that
  `hdmi_drive=2 / hdmi_force_hotplug=1 / config_hdmi_boost=7`
  couldn't fix. Forcing CEA mode 4 (1280x720 @60, `hdmi_group=1
  hdmi_mode=4`) made it rock-solid. The panel scales internally;
  a small loss of pixels is worth the stability.
- **Row-major blit, always.** Column-major iteration in the
  gradient fill produced a visible left-to-right paint sweep at
  ~0.5 s per frame because pitch (5120 B) ≫ cache line (64 B) and
  every store was a fresh miss. Row-major gets 16 pixels per
  cache fill — same code drops to <50 ms. The host_io blit path
  inherits this discipline.
- **Don't undercount stack consumption.** Naïve 'precompute the
  per-column row into a stack buffer' would have taken half the
  16 KiB boot stack. Per-pixel recomputation is cheap enough.
- **`avoid_warnings=1` + `disable_overscan=1`** suppress the Pi
  firmware's icon overlay and overscan padding respectively.
  Neither suppresses panel-side OSD strips — the small Pi-targeted
  panel in question has a permanent ~10 px white bar at the very
  top that survives every Pi-side config knob. Accepted as panel
  hardware quirk; it lives above row 0 of our framebuffer.

### Known TODOs (none blocking)

- **Better scaler.** 1.5× nearest-neighbor produces visible
  jaggies, especially on diagonal edges. Bilinear or 2x-integer-
  with-letterbox would look much cleaner. Reasonable next visual
  polish step.
- **Cache mapping.** The FB region is Normal-WB; we `dc_civac`
  the touched rows after every blit. For Newton's typical small
  dirty rects this is bounded; if a profile shows it dominating,
  remap as Normal Non-Cacheable and drop the maintenance.
- **Multi-mode geometry.** The 1.5× factor assumes a 720-line
  output. A 1080p panel would benefit from 2.25× or a different
  scaler. Revisit when we ship to anything other than the panel
  on Walter's bench.

## Phase 5 — USB input (touchscreen)

**Closes:** pen input from the TSTP MTouch USB panel on real silicon.
**Effort:** 3–5 weeks total, split into five small phases that each
land usable plumbing.
**Depends on:** Phase 4 (need something on screen to point at).
**Reference:** [`MTOUCH.md`](MTOUCH.md) — the specific panel decoded.

### Why USB directly (no UART tunnel)

The earlier plan kept a UART-tunnel placeholder for input. We've
dropped it: pen input on QEMU/FVP is already handled via the host
viewer's pointer (no tunnel needed), and on real hw the touchscreen
sits on the bench plugged into the Pi. Building a UART tunnel for
test-only fake input would be writing throwaway code that delays
the actual goal. We go straight to the USB stack.

### Pluggability requirements

The stack has to leave room for one future expansion: **more touch
panels.** The TSTP MTouch is one specific device; adding "panel B"
should be a new ~100-line driver, not a stack rewrite. Different
panels differ in report ID, layout, and activation handshake — but
the rest is shared.

Audio is *not* a future expansion of this stack — Phase 6 takes the
HDMI-audio path (the bench panel has speakers, the VC firmware
emits IEC 60958 over HDMI, we feed samples via PCM/I2S). No USB hub,
no USB DAC, no UAC class driver. See Phase 6 below.

The two pluggability seams that pay for themselves:

```
   ┌───────────────────────────────────────────────────┐
   │ TScreenManager (Newton consumer)                  │
   ├───────────────────────────────────────────────────┤
   │ trait PenSource                                   │   ← input seam
   ├───────────────────────────────────────────────────┤
   │ device drivers — match on (VID,PID)               │
   │   impl for TSTP MTouch (0416:c168)                │
   │   impl for <future panel>                         │   ← device seam
   ├───────────────────────────────────────────────────┤
   │ usb::class::hid  (helpers, not a trait yet)       │
   ├───────────────────────────────────────────────────┤
   │ trait UsbHostController — control + intr only     │
   │   impl Dwc2     BCM2710 OTG, full-speed           │
   ├───────────────────────────────────────────────────┤
   │ DWC2 controller @ 0x3F98_0000                     │
   └───────────────────────────────────────────────────┘
```

Permanent scope cap: **single full-speed device, no hub.** The Pi
Zero 2 W's micro-USB OTG goes straight to the touchscreen. Audio
exits via HDMI. We never need two USB devices on the bus, so the
USB stack stays small forever: no hub class, no Transaction
Translator, no split transactions, no isochronous transfers. The
`UsbHostController` trait carries control + interrupt only.

Don't pre-design what we can't see. Each trait is added when its
second implementation makes the case for it — `PenSource` lands in
5a with one impl (`NullPen`), `UsbHostController` is born with one
impl (Dwc2). The HID class stays a module of helpers (not a trait)
until a second class shows up, and on this hardware it won't.

### Sub-phases

#### 5a — `PenSource` seam + null backend
**Effort:** half a day. **Depends on:** Phase 4.

- `src/input/mod.rs` — `trait PenSource { fn poll(&mut self) -> Option<PenEvent>; }`
  and a simple `PenEvent { Down{x,y}, Move{x,y}, Up }`.
- `src/input/null.rs` — always returns `None`. Default backend for QEMU
  and FVP (their input comes through the existing host viewer, not
  this seam).
- Hook one `pen.poll()` call per timer IRQ in `src/trap.rs`, feeding
  the result to whatever feeds `TScreenManager`. Find the existing
  guest-viewer pen path first and route through it rather than duplicating.
- Build feature `input-null` (default), `input-mtouch` reserved.

Standalone-deliverable check: cold-boot with `input-null` on real hw
behaves identically to before — no behavioural change, just an idle
trait call per IRQ.

#### 5b — DWC2 host controller, polled
**Effort:** 2–3 weeks. **Depends on:** 5a.

Largest chunk by far; this is the actual USB stack work.

- `src/usb/mod.rs` + `src/usb/host/mod.rs` with `trait UsbHostController`.
  Minimum surface: port reset, control transfer, interrupt-IN submit,
  interrupt-OUT submit, address assignment. No bulk, no iso — we won't
  use them on this hardware.
- `src/usb/host/dwc2/` — port from Circle (`addon/usb/dwchcd*` and
  `lib/usb/dwhcixferstagedata.cpp`). Polled mode, no IRQs, no DMA. Same
  pattern we used for the SDHOST port: re-implement against the
  BCM2835 ARM Peripherals manual + Synopsys DWC2 Programming Guide,
  cross-check semantics against Circle.
- Scope cap: **full-speed only, single device, no hub, no splits, no
  iso, no device-mode.** Pi Zero 2 W's micro-USB OTG goes straight to
  DWC2 with no internal hub — our touchscreen is device 1. Audio is
  HDMI in Phase 6, not USB, so iso and hub support never become
  needed.
- Verification: a `usb-probe` standalone bin (parallel to `pi-probe`
  / `sd-probe` / `fb-probe`) that enumerates whatever's plugged in and
  prints device + configuration + interface descriptors over PL011.
  Test against the touchscreen, a known USB stick (mass storage —
  enumerates but won't be driven), and a USB keyboard.

Risks: DWC2 host-mode initialisation has known sequence sensitivities
(power-on order, HPRT speed-detect timing). Circle's code is the oracle.

#### 5c — USB enumeration + HID class
**Effort:** ~1 week. **Depends on:** 5b.

- `src/usb/enumerate.rs` — descriptor walker that fires after a port
  reset: GET_DESCRIPTOR(Device) → SET_ADDRESS → GET_DESCRIPTOR(Config,
  full) → SET_CONFIGURATION. Output: a small `UsbDevice` struct with
  the parsed configuration tree.
- `src/usb/class/hid.rs` — HID class operations: SET_IDLE,
  GET_REPORT(type, id, len), SET_REPORT, GET_DESCRIPTOR(HID/Report).
  All as helper functions over the host-controller trait — no class
  trait yet (no second impl yet, see §pluggability above).
- `src/usb/dispatch.rs` — given an enumerated `UsbDevice`, walk a
  static `&[&dyn UsbDeviceDriver]` table and ask each driver whether
  it claims the device. Trait surface: `fn matches(dev: &UsbDevice) -> bool`
  and `fn attach(...) -> Result<Box<dyn DeviceHandle>, Err>`. The `Box`
  is fine — we'll have at most a handful of attached devices ever, and
  the alternative (static slots) buys nothing.

#### 5d — TSTP MTouch device driver
**Effort:** 1–2 days. **Depends on:** 5c.

The actual touch panel. Everything we need is in `MTOUCH.md`.

- `src/usb/device/mtouch.rs` — `impl UsbDeviceDriver` matching
  VID=0x0416 / PID=0xC168. On `attach()`:
  1. Issue the activation handshake: `GET_REPORT(Feature, ReportID=3,
     length=2)` on interface 0. Confirm reply is `0x0a 0x00`.
  2. Submit a periodic interrupt-IN read on EP 0x81, 64-byte buffer.
- On each interrupt completion, parse the 56-byte Report ID 1 frame
  per `MTOUCH.md` §"Report ID 1 wire format". Slot 0 only.
- Emit `PenEvent::Down / Move / Up` against the panel's 1024×600
  logical coordinate space (transform deferred to 5e).
- `impl PenSource` over a small ring buffer of pending events. The
  IRQ-time `pen.poll()` from 5a drains the ring.
- Build feature `input-mtouch` enables this driver in the dispatch
  table and selects it as the `PenSource` impl.

#### 5e — Calibration / coordinate mapping
**Effort:** 2–3 days. **Depends on:** 5d + the panel mounted on its
final position.

The panel's 1024×600 touch surface covers the full screen, but Newton
is painted in a 480×720 region centred on a 1280×720 HDMI output (see
Phase 4). Touches in the letterbox bands should be discarded; touches
in the Newton region need scaling back to 0..319 × 0..479.

- Implement the inverse-of-Phase-4-transform as a const function in
  `src/input/calibrate.rs`. Output: `Option<(x, y)>` in Newton coords,
  or `None` for letterbox / out-of-range.
- Allow a per-panel offset/scale override loaded from a `CALIB.BIN`
  file via the existing SD-card backend (Phase 2 plumbing). Default
  to the constants we derived from the math.
- Validate by tapping the four corners + centre of the screen and
  confirming the Newton-side coordinates land where expected. Trip
  one ROM symbol (probably `IO_TBOpenScreen` or the screen-manager
  pen handler) under gdb to see what Newton actually sees.

### Out of scope for Phase 5 (and the USB stack permanently)

- **USB hubs.** Pi Zero 2 W's single OTG port goes direct to the
  touchscreen; audio exits via HDMI in Phase 6. No second USB device
  is ever planned, so no hub class, no port-status pipe.
- **Split transactions / TT.** Without a hub, the bus stays at
  full-speed throughout — no high-speed-to-full-speed translation
  needed.
- **Isochronous + bulk transfer types.** Audio is HDMI; mass storage
  isn't a goal. Control + interrupt-IN cover the touchscreen.
- **Suspend / resume.** The hypervisor doesn't suspend; the panel is
  always-on.
- **IRQ-driven USB.** Polling on the existing CNTHP timer IRQ is fine
  at ~16 ms cadence. IRQ-driven USB is real engineering work and we
  don't need it.
- **Other panels.** 5d delivers one driver. Adding panel B is a
  separate small change once the seams are proven.

## Phase 6 — HDMI audio + serial + PCMCIA

Maps to existing milestones M6 in HIGHLEVEL.md §12.

### Audio: HDMI out via the VC4 HDMI MAI block

**Effort:** in progress. **Depends on:** Phase 4 (HDMI link already up).
**Reference:** Circle `lib/sound/hdmisoundbasedevice.cpp`,
`drivers/gpu/drm/vc4/vc4_hdmi.c`.

**Correction over an earlier version of this doc:** the original
plan said "feed BCM2835 PCM/I2S peripheral at `0x3F20_3000` and the
VC firmware embeds it into HDMI". That's wrong on Pi 0–3: the
PCM/I2S peripheral only reaches GPIO 18–21 (external I²S DAC). HDMI
audio on Pi 0–3 goes through the VC4 HDMI block's MAI ("Multi-
channel Audio Interconnect") FIFO at `0x3F90_2000`, fed by SPDIF /
IEC 60958 subframes that the HDMI encoder embeds into the video
blanking interval. The bench panel has speakers and accepts HDMI
audio; that path is what `src/audio/pi_hdmi.rs` drives.

Landed (initial cut, needs real-hw validation):

1. **`audio` module seam** (`src/audio/mod.rs`) — same shape as
   `host_io` / `input` axes; backend selected by `audio-*` Cargo
   features and resolved in `build.rs` to `cfg(nh_audio_*)`. Null
   default for QEMU/FVP; `audio-pi-hdmi` rolled into
   `pi-bare-metal-input` so the existing real-hw aggregate gets
   sound for free.
2. **VC4 HDMI MAI bring-up** (`src/audio/pi_hdmi.rs`) — MAI_CTL
   reset + flush, MAI_FMT = 44.1 kHz PCM, MAI_CONFIG bit-reverse +
   format-reverse + channel-mask = stereo, MAI_CHANNEL_MAP =
   0b001000 (Pi ≤3 stereo L+R), AUDIO_PACKET_CONFIG = stereo +
   B-preamble, CRP_CFG external-CTS + N=5644 for 44.1 kHz, CTS0/1
   = 27000 (default 27 MHz pixel clock).
3. **CEA Audio InfoFrame** so the receiver knows to expect PCM
   stereo 16-bit 44.1 kHz. Written into the HDMI RAM packet slot
   and enabled via `HDMI_RAM_PACKET_CONFIG`.
4. **Newton sample feed** — Newton's 22.05 kHz mono BE-S16 is
   sample-and-hold upsampled to 44.1 kHz stereo (exact 2× ratio,
   so no interpolator), pushed into a 4096-frame ring from
   `sound::handle` subfn 0x07.
5. **SPDIF encoding + polled FIFO feed** — `audio::pump` from the
   trap-IRQ and sync-trap tails: for each ring frame, build two
   IEC 60958 subframes (24-bit sample shifted into bits 27..4,
   parity in bit 31, B-preamble on the first subframe of each
   192-frame block) and write to `HDMI_MAI_DATA`.
6. **Buffer-completion IRQ** — once `pump` drains the ring to the
   mark the producer recorded, raise the output-interrupt mask the
   kernel passed in subfn 0x1F so subfn 0x07 fires again with the
   next half of the ping-pong.

What still needs first-light testing on real hw:

- **MAI register bitfields are reconstructed**, not validated. The
  exact bit positions for `MAI_CTL_ENABLE`, `MAI_CTL_FULL`,
  `MAI_CTL_CHNUM_SHIFT`, the sample-rate code in `MAI_FMT`, etc.
  came from Circle's `hdmisoundbasedevice.cpp` + `vc4_hdmi.c`
  cross-reference. Live silicon may need tweaks — `bringup_mai`
  in `pi_hdmi.rs` is where to twiddle.
- **CTS regeneration math** — we hard-code CTS = 27000 for a
  27 MHz pixel clock, which is the standard 720p60 audio clock.
  Panels at non-standard modes (1366×768, 1024×600, …) need CTS
  recomputed against the real pixel clock; if audio comes out
  pitch-shifted or stutters with no underrun warnings, this is
  the first place to check.
- **`hdmi_drive=2`** in `boot-pi/config.txt` (landed alongside).
  Without it the encoder strips audio packets regardless of what
  we write into MAI_DATA.

Out of scope for this cut:
- DMA-driven audio. Polling from the trap tail is enough at
  44.1 kHz stereo if the trap rate doesn't drop below ~700 Hz.
  If a quiet stretch of the guest underruns the FIFO and the
  output sounds chunked, switch to BCM2835 DMA via DREQ from the
  HDMI block — Circle's reference does exactly this.
- PWM audio output. Pi Zero 2 W has no 3.5 mm jack and the GPIO
  PWM path needs an external low-pass filter — not worth it.
- BCM2835 PCM/I2S to GPIO. Same reason — needs an external DAC.

### Serial + PCMCIA

The PL011 mini-UART is already up for the kernel log; Newton's
guest serial port maps to a separate buffer the host can drain. No
new hardware work needed.

PCMCIA image source (Newton's flash storage cards) maps to files
on the SD card via the Phase 2 flash-persist backend; no new bus
work needed either.

## Cross-cutting: feature aggregates

QEMU `raspi3b` and the real Pi Zero 2 W share the same SoC, so a
single `platform-raspi3b` Cargo feature drives both (no separate
`platform-pi-zero2w`). The differences live in opt-in backends
selected by aggregate features:

| Feature | semihost | flash-persist | host-io | input | audio | Intended target |
|---|---|---|---|---|---|---|
| (default) | on | semihost | null | null | null | `cargo run` against QEMU |
| `pi-bare-metal` | off | null | null | null | null | first-light real-hw boot |
| `pi-bare-metal-sd` | off | sd | null | null | null | real-hw with persistent state |
| `pi-bare-metal-display` | off | sd | pi-fb | null | null | real-hw, full display |
| `pi-bare-metal-input` | off | sd | pi-fb | mtouch | pi-hdmi | real-hw, full display + USB touch + HDMI audio |
| `platform-fvp-base` | on | semihost | null | null | null | FVP cycle-accurate runs |

The `pi-bare-metal-input` aggregate compiles cleanly today; pen
events flow only when the DWC2 host stub (`src/usb/host/dwc2/`)
finishes coming up. Until then the build behaves like
`pi-bare-metal-display`.

Probe features (`sd-probe`, `fb-probe`, `sd-probe-trace`) are
additive on top of any aggregate. The build script accepts
`PI_CARGO_FEATURES` to override the base and `PI_EXTRA_FEATURES`
to append.

`build.rs` resolves the active `flash-persist-*` and `host-io-*`
backends through small per-axis selectors that panic on mutually
exclusive picks. To add a new backend (e.g. `host-io-pi-emmc`
once we have one), add the feature in `Cargo.toml`, an arm in the
relevant resolver, and a `#[cfg(nh_*)]`-gated module under the
matching directory.

## Open questions specific to real hardware

These don't block the plan but should be captured as they come up:

- **Firmware version drift.** Pin a specific `bootcode.bin` / `start.elf`
  combo so behaviour is reproducible. Note the SHA in this doc.
- **DRAM size.** The Zero 2 W has 512 MiB. Plenty for the ~32 MiB guest
  carve-out plus EL2 heap + framebuffer.
- **Thermal at sustained 1 GHz.** HIGHLEVEL.md §13.5 calls this minor;
  re-verify with a Phase 1 long run.
- **Cores 1–3 entry state.** Pi firmware parks them at a spin-table
  address in low memory. Need to either consume that contract or rely
  on the firmware's WFE state. Document which.

## Status tracker

| Phase | Status | Notes |
|---|---|---|
| 0 — EL2 handoff + UART | **done (2026-05-11)** | `CurrentEL = 2` on Walter's Zero 2 W; §16.1 closed |
| 1 — Hypervisor `kmain` on Zero | **done (2026-05-11)** | Boots through `kmain`, ROM patches, stage-2, ERET to guest. Initial run halted on the trip-wire for an unmapped-IPA write at `0x01683800` from `DiagBootStub` `PC=0x1a01c`; a subsequent real-silicon run shows the OS continuing past that point and **booting + running without observed crashes**. No detailed serial trace captured for the longer run yet — Phase 2 below will give us a place to land logs. |
| 2 — Persistent flash | **done (2026-05-12)** | `flash-persist-sd` backend running on real hw. SDHOST at 25 MHz / 4-bit; ~700 KB/s through the FAT layer (CMD17/CMD24 per sector — multi-block command is the obvious next optimisation). Pi-bare-metal-sd boot loads + saves `/NEWTON.BIN` end-to-end across cold boots. |
| 3 — Snapshot on real hw | **deferred** | Snapshots are valuable when debugging late-boot state on QEMU/FVP; on real silicon the boot tends to *complete*, so the rewind-by-2s loop they accelerate isn't load-bearing. Skip until a real-hw bug demands them. |
| 4 — Display | **done (2026-05-12)** | `host-io-pi-fb` backend running on real hw. Newton's 320x480 2 bpp FB scaled 1.5x to 480x720 centred on a 1280x720 HDMI panel (CEA mode 4 forced for link stability). Output looks like the QEMU/FVP host-viewer image but with nearest-neighbor aliasing; bilinear or integer-scale-with-letterbox is a follow-up. |
| 5 — USB input (touchscreen) | **done (2026-05-12)** | TSTP MTouch panel working end-to-end on Walter's Pi Zero 2 W. Taps on the HDMI-connected touchscreen drive Newton's UI (Continue button on Welcome screen responds to both fast and slow taps). |
| 6 — HDMI audio / serial / PCMCIA | audio: initial cut landed, real-hw validation pending | HDMI audio via the VC4 HDMI MAI block at 0x3F90_2000 (not the BCM2835 PCM/I2S — that goes to GPIO 18-21 only). `pi-bare-metal-input` aggregate now includes `audio-pi-hdmi`. Bench panel has speakers. USB stack stays single-device-no-hub permanently. |

### Phase 5 — closed (2026-05-12)

TSTP MTouch USB touchscreen working on Walter's Pi Zero 2 W. Taps
land on the Newton UI through the full chain — confirmed with the
Welcome screen's Continue button responding to both fast and slow
taps.

#### What landed

```
  src/input/         PenSource trait + null backend + mtouch driver
    mod.rs           PenEvent enum, drain_into_queue marker logic
    null.rs          no-op (default, used by every QEMU/FVP build)
    mtouch.rs        TSTP MTouch driver — activation handshake,
                     IN-endpoint poll, slot-0 decode, ring buffer
    calibrate.rs     panel 1024x600 → Newton 320x480 (inverse of
                     Phase 4 transform); compile-time spot checks
  src/usb/
    mod.rs           shared types: SetupPacket, UsbError, request
                     codes, descriptor type constants
    descriptor.rs    Device/Config/Interface/Endpoint/HID parsers +
                     walk_config iterator
    enumerate.rs     standard §9.1.2 sequence (GET_DESC, SET_ADDR
                     + 50ms tDSETADDR delay, SET_CONFIG + 50ms);
                     produces UsbDevice
    class/hid.rs     SET_IDLE / GET_REPORT / GET_DESCRIPTOR(Report)
    host/mod.rs      UsbHostController trait
    host/dwc2/       Synopsys DWC2 driver — full host-mode init,
                     channel-0 control + interrupt-IN transfers,
                     per-endpoint DATA0/DATA1 toggle tracking,
                     pre-transfer channel-disable safeguard
    dispatch.rs      UsbDeviceDriver trait (for any future device)
  src/usb_probe.rs   standalone bin — reads DWC2 GSNPSID, confirms
                     OTG core is alive on real hw
```

Wiring:

- `input::pump()` runs from the same trap-return tail as
  `host_io::pump_input` (`src/trap.rs`); both feed the same pen-
  sample queue (`host_io::queue`) so the rest of the hypervisor
  (TScreenManager / INT_TABLET / `NativeGetSample`) is oblivious to
  the source.
- Cargo axis `nh_input_*` resolved by `build.rs`. Aggregate
  `pi-bare-metal-input` = `pi-bare-metal-display` + `input-mtouch`.
- DWC2 implementation cross-checked twice against Circle's
  `lib/usb/{dwhcidevice.cpp, dwhcixferstagedata.cpp, usbendpoint.cpp,
  usbhostcontroller.cpp, dwhciframeschednoSplit.cpp}` plus the
  `include/circle/usb/dwhci.h` register map.

Calibration math (`src/input/calibrate.rs`):

- Touch 0..1024 × 0..600 maps to a 1280×720 panel; Newton paints
  the centre 480×720 region (Phase 4).
- Left letterbox band: panel touch X < 320 → drop.
- Right band: panel touch X ≥ 704 → drop.
- In-region: `newton_x = (touch_x - 320) * 320 / 384`,
  `newton_y = touch_y * 480 / 600`.
- Compile-time spot checks assert corner + centre map correctly.

#### Bring-up lessons (each cost a real-hw round-trip)

- **`HCDMA` takes the GPU-bus uncached alias, not the bare ARM PA.**
  Pi 2/3/Zero-2-W's DWC2 AHB master sees DRAM through
  `pa | 0xC0000000` (Circle's `BUS_ADDRESS` macro on
  `GPU_MEM_BASE = GPU_UNCACHED_BASE`). Passing the bare PA gives
  `XACT_ERR` on the first transaction every time. The cache-flush
  call still uses the ARM VA — only HCDMA gets the alias.
- **USB 2.0 §9.2.6.3 `tDSETADDR` is real.** Skipping the 50 ms post-
  SET_ADDRESS delay makes the *next* SETUP at addr=1 hit XACT_ERR —
  the device is still listening at addr=0. Same applies to
  SET_CONFIGURATION. Matches `usbhostcontroller.cpp:80`.
- **DMA mode does NOT auto-advance the host's data-toggle PID.**
  The DWC2 core keeps its internal state, but the host driver has
  to track DATA0↔DATA1 across separate transfers and pass the
  expected PID in HCTSIZ. Without this, the first interrupt-IN
  packet works and every subsequent one silently drops with
  `DATA_TGL_ERR`. Circle's `CUSBEndpoint::SkipPID()` is the
  reference. Ours lives on `Dwc2::int_next_pid[16]`.
- **HID 1.11 §8.6 inserts the Report ID byte.** The MTouch
  activation reply is `[0x03, 0x0a]` (`[ReportID=3,
  ContactCountMax=10]`), not the `[0x0a, 0x00]` originally
  documented in `MTOUCH.md` — Linux's `hid-multitouch` strips the
  ID byte for userland, which is what we saw with `usbhid-dump`.
- **`FRM_OVRUN` on a periodic-IN is the *normal* idle response,
  not an error.** When the device NAKs and the frame ends without
  a successful packet, the core sets FRM_OVRUN + ChHltd. Treating
  it as a transaction error spams the console at 16 ms cadence and
  hides real bus errors. Classify it as "no data this poll" (with
  no log) and the steady-state goes quiet.
- **Newton's pen-detection cares about the specific pressure value,
  not just non-zero.** Einstein's `TScreenManager::PenDown` default
  pressure is 4; passing 8 makes the samples reach `NativeGetSample`
  cleanly but the UI silently ignores them. Match Einstein
  byte-for-byte: `PRESSURE: u16 = 4` in `input::drain_into_queue`.

### Phase 2 — closed (2026-05-12)

The full SD storage stack runs on real silicon. Build with
`--features pi-bare-metal-sd` and the hypervisor will:

1. Bring up the BCM2835 SDHOST controller. GPIO 48..53 → ALT0;
   firmware `CLOCK_ID_CORE` rate queried via VC mailbox at
   `0x3F00_B880`. Identification at 400 kHz / 1-bit; post-CMD7
   bump to **25 MHz / 4-bit** (SDCDIV=8, ACMD6 then
   `SDHCFG_WIDE_EXT_BUS`). Init prints a one-line summary:

       sd: bus ready (25.0 MHz, 4-bit)

2. Enumerate the card: CMD0 → CMD8 → ACMD41 (HCS) → CMD2 → CMD3
   → CMD9 → CMD7 → CMD16 → CMD55+ACMD6. The 128 GB SDHC card
   used during bring-up reports RCA=`0xd5550000`.
3. Mount the FAT32 boot partition via `embedded_sdmmc::VolumeManager`.
   `sd-probe` builds verify the path end-to-end by reading
   `/CONFIG.TXT` back from the same card we wrote it onto and
   round-tripping a writeable file (`EL2HELLO.TXT`).
4. Persist `GUEST_FLASH` (8 MiB) to `/NEWTON.BIN` on autosave
   cadence (2 s wall-clock, driven from `trap_irq` via
   `snapshot::maybe_autosave`'s no-semihost branch). Cold boots
   load it back with a fingerprint check.

#### Throughput (real-card numbers)

At 25 MHz / 4-bit through the FAT layer:

- Full 8 MiB save / load: ~12 s (≈ 700 KB/s).
- Incremental save of N × 64 KiB blocks: roughly linear at the
  same rate.

That is **~10× below** what the bus alone can do (≈ 12.5 MB/s
theoretical / ≈ 5–8 MB/s realistic). The gap is per-sector
overhead — we issue a CMD17/CMD24 per 512-byte sector, so 8 MiB
= ~16k commands. The path forward when this matters:

- **CMD18 / CMD25 multi-block transfers** — single command, many
  sectors. Amortises command latency and the FSM-completion poll
  across the burst. Realistic target: 5+ MB/s, i.e. 1–2 s per full
  save instead of 12.
- Stretch: revisit `embedded-sdmmc`'s call shape to see whether it
  ever passes us multi-block slices, or whether we'd need to
  buffer at our `BlockDevice` impl.

Not blocking for Phase 4+ — left as a "when this starts hurting"
task.

#### Bring-up lessons for the BCM2835 SDHOST

In case anyone else ports it from scratch:

- The `SDHCFG_*_IRPT_EN` bits are misnamed. They don't just gate
  IRQ generation — `SDHCFG_DATA_IRPT_EN` gates the FSM's data-
  movement path itself, even in polling mode. Without it the FSM
  walks READWAIT → DATAMODE but the FIFO stays empty. The trace
  shape of "CMD17 OK, FSM transitions, no errors, FIFO_FILL stuck
  at 0" is the signature.
- Don't poll `SDHSTS_DATA_FLAG` for PIO drain. It's threshold-driven
  (set when FIFO_FILL ≥ READ_THRESHOLD, clears below) and will lie
  to you in a word-at-a-time loop. Poll `SDEDM[8:4]` (FIFO_FILL)
  directly; both Linux and Circle do this.
- Data-phase completion is `SDEDM.FSM == DATAMODE | IDENTMODE`,
  not `SDHSTS.BLOCK_IRPT`. If the FSM is stuck in READWAIT (read)
  or WRITESTART1 (write) at end of transfer, write
  `SDEDM | FORCE_DATA_MODE` to kick it out.
- The Pi Zero 2 W routes the micro-SD slot to **SDHOST**, not the
  Arasan EMMC block (which serves the on-package WLAN/BT SDIO on
  this SoC). Pi 4 / Pi 5 invert this — their SD code won't port.
- ACMD6 to switch the card to 4-bit must happen **before** writing
  `SDHCFG_WIDE_EXT_BUS` to the controller — the reverse drives 4
  data lines into a card still in 1-bit mode and trashes every
  transfer.
- 25 MHz / 4-bit is reliable with default GPIO pulls (CMD + DAT0..3
  pull-up, CLK pull-off via the BCM2835 GPPUD/GPPUDCLK1 sequence).
  Higher (50 MHz HS, UHS) would require CMD6 SWITCH_FUNCTION
  negotiation and likely tuning we haven't needed yet.
- `flash_persist::maybe_save` is normally called from
  `snapshot::maybe_autosave`. On real silicon that path is gated
  behind `cfg(not(no-semihost))` because the snapshot ring itself
  is inert. To get flash saves firing under `no-semihost` we
  added a sibling branch that runs the same wall-clock gate but
  only calls `flash_persist::maybe_save` (no snapshot work). Easy
  to miss; the symptom is "init runs, file never written".

#### Pieces reusable by later phases

- Polled VC mailbox property-tag client (`src/mailbox.rs`).
  Phase 4 framebuffer alloc is the immediate consumer. The per-
  tag response-indicator check sits on `buf.words[4]`, not
  `buf.words[3]` (response status is in the third header word).
- BCM2835 SDHOST driver + `embedded_sdmmc` integration
  (`src/sd/`). Phase 3 will lift the same dirty-tracking pattern
  if/when we decide we want it.

#### Known small TODOs

- CSD decode for the actual card-size value reported through
  `BlockDevice::num_blocks`. Currently returns `u32::MAX`
  (sufficient for partition reads — per-partition bounds come
  from MBR).
- Multi-block I/O (see "Throughput" above).

### Phase 1 — closed (2026-05-11)

**update (2026-05-11, later in the day):** a subsequent real-hardware
boot — same `newton-hypervisor` image — runs past the
`IPA=0x01683800` trip-wire and continues with the OS apparently
running without further crashes. We don't have a full serial capture
of that longer run yet (the previous halt produced one because the
hypervisor itself halted; the post-halt run just keeps going). So:

- The "Phase 1 ceiling" recorded immediately below is the **initial**
  Phase 1 result and is no longer the boot ceiling on real silicon.
- We need a way to persist serial logs (and ideally snapshots) from
  real-hardware runs to study what the OS actually does after the
  `DiagBootStub` window. That's the motivation for promoting Phase 2
  (SD-card storage) ahead of any further investigation here.

Original Phase 1 closure (kept for reference):

Real-hardware result on Walter's Pi Zero 2 W, `newton-hypervisor` built
with `--no-default-features --features pi-bare-metal` (= platform-
raspi3b + no-semihost + flash-persist-null), same SD pipeline as
Phase 0:

- Firmware reads `config.txt`, `start_cd.elf`, `fixup_cd.dat`.
- Hypervisor banner + capability dump appear over PL011.
  `CNTFRQ_EL0 = 19_200_000 Hz` (vs QEMU's 62.5 MHz) — real silicon's
  generic-timer reference clock.
- MMU EL2 stage-1, ROM load (with REx patch + 256 NATIVE_PRIM rewrites),
  39 simple ROM patches + 5 native-call injections, 86 CP15 encoding
  rewrites, stage-2 build (ROM/RAM/flash/framebuffer/tick-page),
  shadow-pool smoke test, g1-capture, alrt-capture all run identically
  to QEMU.
- `Entering Newton ROM...` ERET fires. First few guest traps work
  (HVC, CP15 `MCR SCTLR`, UND for StrongARM `MCR c15,c1,2`, DABT,
  timer IRQs).
- PCMCIA MMIO writes match the QEMU sequence.
- Reaches Newton kernel `DiagBootStub` and continues ~0x60 bytes
  past where QEMU's spin ceiling sits, then halts on the trip-wire
  for an unmapped-IPA write:

  ```
  *** unknown MMIO write halted ***
    IPA    = 0x01683800  W  value=0x000866b0  @ELR=0x1a01c
    region: outside known windows
  ```

  This is real-silicon-specific: QEMU stays in a tight spin at
  `DiagBootStub+0xa6c` and never reaches the write, while real
  silicon's 3.25× slower CNTPCT lets the loop progress and triggers
  the write. The behaviour is a Phase-B debugging target (decide
  whether to model the IPA, widen stage-2, or patch the call site),
  not a Phase-1 blocker.

The Phase-1 exit criterion ("`kmain` runs, doesn't crash before the
first guest-side trap") is cleared by a wide margin.

### Phase 0 — closed (2026-05-11)

Real-hardware result on Walter's Pi Zero 2 W, default firmware path
(commit `8fce67a9`, `arm_64bit=1`, `gpu_mem=16`, `dtoverlay=disable-bt`):

```
Read File: config.txt, 1786
Read File: start_cd.elf, 851132 (bytes)
Read File: fixup_cd.dat, 3273 (bytes)

=== newton pi-probe ===
CurrentEL = 2
MIDR_EL1  = 0x00000000410fd034
MPIDR_EL1 = 0x0000000080000000
ok, parking core 0 in WFE
```

Matches the QEMU `raspi3b` run byte-for-byte (same MIDR, same EL,
same MPIDR with bit 31 RES1). §16.1 in `HIGHLEVEL.md` is closed.

#### Completed sub-pieces

- `src/pi_probe.rs` — standalone `[[bin]]` (`pi-probe`). Prints
  CurrentEL / MIDR_EL1 / MPIDR_EL1 over PL011, WFE-loops. Verified in
  QEMU `raspi3b`: `CurrentEL = 2`, MIDR matches Cortex-A53.
- `boot-pi/config.txt` — `arm_64bit=1`, `enable_uart=1`,
  `uart_2ndstage=1`, `dtoverlay=disable-bt`.
- `scripts/build-sd.sh <dest>` — fetches pinned Pi firmware blobs
  (cached under `target/pi-firmware-cache/`), builds `pi-probe`,
  objcopies to `kernel8.img`, assembles the full boot-partition
  layout under `<dest>`. Pinned firmware commit:
  `8fce67a9ec5668fb8d42d215c9ec4c199340bf41`.
- `linker.ld` / `linker-fvp.ld` — added `.eh_frame_hdr` to the
  DISCARD list. Without `.rodata` to anchor orphan-section
  placement (the probe binary has no `.rodata` because string
  literals fold into `.text`), the linker was placing
  `.eh_frame_hdr` at VMA 0x80000, shifting `_start` 12 bytes
  later and crashing on the leading UDFs. Main hypervisor binary
  unaffected (its `.rodata` section already anchored
  `.eh_frame_hdr` elsewhere).

Update this table as phases close. Each row should eventually link to
the commit(s) that closed it.
