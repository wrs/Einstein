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

## Phase 4 — Display

**Closes:** Newton renders to a mini-HDMI monitor.
**Effort:** 2–4 days (framebuffer alloc + blit path; mailbox is done).
**Depends on:** Phase 1. Mailbox already built in Phase 2.

VideoCore framebuffer init goes through the same property-tag
mailbox client `src/mailbox.rs` we built for SDHOST clock setup.
After init, the framebuffer is just a chunk of physical memory at
a mailbox-returned bus address.

### Tasks

1. **Framebuffer allocation tags.** Extend `src/mailbox.rs` with
   the `FB_ALLOCATE` / `FB_SET_PHYSICAL_W_H` / `FB_SET_VIRTUAL_W_H`
   / `FB_SET_DEPTH` / `FB_SET_PIXEL_ORDER` etc. tags. Single
   property request bundling them is the standard idiom (saves a
   round-trip per tag).
2. **Bus → CPU address translation.** VC returns a bus address
   (typically `0x4000_0000 | pa`). Identity-map the framebuffer
   region as Normal-WB at EL2 stage-1; mark guest-visible at
   stage-2 so the existing blit path can land bytes there.
3. **Blit backend.** `host_io` already abstracts blits; add a
   `host-io-pi-fb` variant that writes into the VC-returned
   framebuffer instead of forwarding via semihosting. Pair with
   `pi-bare-metal-sd` for the full real-hw build.
4. **Geometry.** Newton expects 320×240 mono. Two options:
   - Ask VC for that exact mode and let HDMI scaling stretch
     (probably ugly).
   - Ask for native panel resolution (1080p typical) and scale
     in software during the blit. Open question §16.11.

QEMU `raspi3b`'s mailbox is partial — this is one of the places
real hardware is more reliable than QEMU, not less.

## Phase 5 — Input

**Closes:** pen input via USB touch device or UART tunnel.
**Effort:** large for USB; small for UART tunnel.
**Depends on:** Phase 4 (need something on screen to point at).

Tracks Open Question §16.14 (input device for v1). Two paths:

- **USB OTG + HID touchscreen.** Full `dwc_otg` host stack work. Real
  effort, separate sub-project. Not the right first step.
- **UART-tunnelled pen events.** Host machine sends `(x, y, down/up)`
  packets over the mini-UART; EL2 receives them and feeds
  `TScreenManager`. Trivial. Lets the rest of the system be exercised
  end-to-end while USB waits.

Recommendation: UART tunnel first. USB is its own milestone.

## Phase 6 — Audio + serial + PCMCIA

Maps to existing milestones M6 in HIGHLEVEL.md §12. Real-hw specifics
(I2S vs PWM for audio, BCM SD vs PCMCIA image source) are not closer
than the M6 work itself. Sequence after Phase 5 has a usable system.

## Cross-cutting: what changes in the build

A `platform-pi-zero2w` Cargo feature, alongside `platform-raspi3b` and
`platform-fvp-base`. Build profile selects:

- Linker script (real load address, real DRAM size).
- Default `host-io-null`, `flash-persist-null` initially; later
  `host-io-pi-fb`, `flash-persist-sd` (FAT32 on the boot partition).
- Snapshot autosave: off by default for the real-hw profile until
  Phase 3 finishes the storage path.

`build.rs` already has the platform-feature plumbing pattern; extending
it to a third platform is mechanical.

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
| 4 — Display | **next** | VC mailbox already runs (Phase 2 dep); framebuffer alloc + blit path is the new code. |
| 5 — Input | not started | UART pen first, USB later |
| 6 — Audio / serial / PCMCIA | not started | aligns with M6 |

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
