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

## Phase 2 — Persistent flash on real silicon

**Closes:** flash writes survive reboot on the Zero.
**Effort:** 1–2 days for a minimal driver.
**Depends on:** Phase 1. Optional if you only care about cold boots.

The current `flash-persist-semihost` backend writes to
`$HOME/.newton/flash.bin` via Arm semihosting — no equivalent on real
silicon. Options:

- **SD-card backend.** Add a minimal BCM2835 EMMC driver, mount a raw
  partition (no FAT), use as a block device. This is real driver work
  (~few hundred lines) but well-trodden territory; reference
  implementations exist (`raspberrypi/linux` EMMC driver, plus dozens of
  bare-metal Pi tutorials).
- **UART tunnel.** Slot a backend that sends flash blocks over the
  mini-UART to a host script. Slow but trivial. Useful for early
  debugging before the EMMC driver exists.
- **Onboard SPI flash via the `flash-persist-pico` placeholder slot.**
  Not relevant for the Zero 2 W itself (no onboard user-writable SPI
  flash); reserved for a future Pico-as-storage-bridge configuration.

Recommendation: UART tunnel first (it's cheap), EMMC after.

## Phase 3 — Snapshot ring on real silicon

**Closes:** the snapshot/resume workflow works on the Zero.
**Effort:** 1 day, once Phase 2 has a real storage path.
**Depends on:** Phase 2.

The autosave ring (`snapshot.rs`) currently calls semihosting `SYS_WRITE`
for ~14 MiB every 2 s. On real hw:

- Reuse the Phase 2 SD/EMMC backend if it exists. The four snapshot
  slots fit easily on a partition.
- Or accept that real-hw runs are cold-boot only and leave the ring
  disabled. For pure regression testing this is fine; for iterative
  hypervisor-code debugging the QEMU/FVP loop is faster anyway.

Snapshot compatibility across host platforms is already gated on a ROM
fingerprint; the storage layer change doesn't break that.

## Phase 4 — Display

**Closes:** Newton renders to a mini-HDMI monitor.
**Effort:** 2–4 days (mailbox + framebuffer + blit path).
**Depends on:** Phase 1. Independent of Phase 2/3.

VideoCore mailbox-channel-1 (framebuffer init) is the standard path.
After init, the framebuffer is just a chunk of physical memory at a
mailbox-returned address.

### Tasks

1. **Mailbox driver.** ARM↔VC mailbox at `0x3F00B880` (BCM2710 base).
   Property-channel call: get framebuffer at requested W×H×bpp.
2. **Blit path.** `host_io` already abstracts blits; add a real-hw
   backend that writes into the VC-returned framebuffer.
3. **Geometry decision.** Newton expects 320×240 mono in its native
   format. Either ask VC for that and let scaling happen in hardware
   (probably ugly), or ask for native panel resolution and scale the
   Newton FB ourselves. Open question §16.11.

QEMU `raspi3b`'s mailbox is partial — this is one of the places real
hardware is more reliable, not less.

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
  `host-io-pi-fb`, `flash-persist-pi-emmc`.
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
| 0 — EL2 handoff + UART | **ready to flash** | probe binary + SD pipeline done; awaiting boot on real Zero 2 W |
| 1 — Hypervisor `kmain` on Zero | not started | depends on Phase 0 |
| 2 — Persistent flash | not started | start with UART tunnel |
| 3 — Snapshot on real hw | not started | optional |
| 4 — Display | not started | mailbox + FB |
| 5 — Input | not started | UART pen first, USB later |
| 6 — Audio / serial / PCMCIA | not started | aligns with M6 |

### Phase 0 — completed sub-pieces (2026-05-11)

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
