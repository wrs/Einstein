# nhboot — serial image loader for the Pi Zero 2 W

`nhboot` is the small bootloader that runs as `kernel8.img` on the Pi
so the Newton hypervisor can be replaced over the serial console
instead of by moving the SD card. It is a standalone `no_std` package
(not a workspace member) with its own linker script and a deliberately
minimal driver set: polled PL011, mailbox core-clock query, PIO-only
SDHOST, the vendored `embedded-sdmmc` FAT stack.

The host-side tool is `../scripts/pi-upload.py`. The full design,
protocol and hardware notes live in
[`../docs/REAL_HW_BRINGUP.md`](../docs/REAL_HW_BRINGUP.md), section
"Serial image upload — nhboot"; this file is the package-level
orientation.

## What is on the card

| file | contents |
|---|---|
| `kernel8.img` | nhboot (~90 KiB). Changes rarely; updated only by a card write. |
| `HYPERV.IMG` | fixed 16 MiB container: 4 KiB header + the raw hypervisor image + zero pad |
| `config.txt` | `initramfs HYPERV.IMG 0x02000000` makes the firmware load the container to RAM |
| `NEWTON.BIN` | the guest's persisted flash (the hypervisor's, untouched by nhboot) |

`scripts/build-sd.sh <dest> [<mount>]` produces all of it.

## Boot sequence

1. The firmware loads `kernel8.img` at `0x80000` and `HYPERV.IMG` at
   `0x02000000`, then enters `_start` at EL2 (`src/boot.s`).
2. boot.s parks the secondary cores, saves the DTB pointer, and copies
   the whole image to its link address `0x10000000` — nhboot must get
   out of `0x80000`, which is where the hypervisor links.
3. `main.rs` prints a banner, validates the container
   (`image::inspect`: magic, header CRC, length, payload CRC-32) and
   listens 1 s for the host's handshake (`xfer::handshake_window`).
   With no valid container it listens forever.
4. If a host answers, `xfer::receive` runs the upload protocol into a
   second container at `0x03000000`, `persist::persist` writes the
   changed sectors of `HYPERV.IMG`, and the new container is booted.
5. `image::boot` copies the payload to `0x80000` and branches there
   with `x0` = the firmware's DTB pointer, exactly as the firmware
   would have entered a plain `kernel8.img`.

RAM map at entry: `0x80000` nhboot (later the hypervisor),
`0x02000000` the loaded container, `0x03000000` upload staging,
`0x10000000` nhboot relocated (16 KiB stack, 160 KiB bss). The MMU
stays off throughout.

## Container header (`src/image.rs`, mirrored by `ImageFormat` in `pi-upload.py`)

```
0x000  "NHIMG001"
0x008  u32 payload_len      (LE)
0x00C  u32 payload_crc32    (LE, IEEE / zlib.crc32)
0x010  u32 header_crc32     (LE, over bytes 0x000..0x010)
0x014  zero to 0x1000
0x1000 payload, zero pad to 16 MiB
```

## Protocol (`src/xfer.rs`, mirrored in `pi-upload.py`)

```
host   →  \x01NHUP <baud>\n         (text, 115200, repeated every 100 ms)
nhboot →  NHUP-OK <baud>\n           then both sides switch baud
nhboot →  T  u32 n, n×{u32 adler32, u32 crc32} of 4 KiB old blocks, u32 crc32
host   →  D  u32 offset, u32 len, u32 crc32, bytes        (≤ 64 KiB)
host   →  C  u32 new_off, u32 old_off, u32 len, u32 crc32 (copy from old)
host   →  K  u32 payload_len, u32 payload_crc32           (commit)
nhboot →  A  u32 echo   |   N  u32 echo, u8 reason
nhboot →  persist: … lines, DONE\n (text), then back to 115200
```

NAK reasons: 1 bad CRC, 2 bad offset/length, 3 RX timeout inside a
message, 4 no old image for COPY, 5 unknown tag. Nothing is printed
between the TABLE and the COMMIT ACK: the console is the link.

The host builds the COPY/DATA list rsync-style (rolling adler32 over
every byte offset of the new image, verified by crc32), so a rebuild
sends only the bytes that changed. The hypervisor's linker script pins
the ROM + REx blob at a fixed image offset so those sectors never move
on the card.

## Building and testing

```bash
cd nhboot && cargo build --release          # ELF; build-sd.sh does the objcopy
cargo clippy --release
```

Under QEMU (no card needed for the protocol; a FAT32 disk image for
persistence):

```bash
hdiutil create -size 64m -fs "MS-DOS FAT32" -volname NHTEST -layout MBRSPUD sd.dmg
qemu-system-aarch64 -M raspi3b -kernel nhboot.bin \
    -device loader,file=HYPERV.IMG,addr=0x02000000 \      # optional "old" image
    -drive if=sd,file=sd.dmg,format=raw \                 # optional card
    -chardev socket,id=s0,path=serial.sock,server=on,wait=off -serial chardev:s0 \
    -display none -no-reboot -monitor none
scripts/pi-upload.py --port unix:serial.sock --no-power-cycle \
    --kernel target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
    --until 'Welcome to NewtonScript' --timeout 120
```

On the Pi, the everyday loop is

```bash
scripts/pi-upload.py --kernel target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
    --until 'Welcome to NewtonScript' --timeout 120
```

which power-cycles the board through the `Pi Off` / `Pi On` Shortcuts,
uploads the delta at 1.5 Mbaud (`--baud 3000000` also works), waits
for the persist, and streams the console until the marker matches.
`--no-upload` is a plain power-cycle-and-capture.

## Source map

| file | role |
|---|---|
| `src/boot.s` | entry, core parking, self-relocation, stack/guard/bss |
| `src/main.rs` | banner → inspect → handshake window → receive/boot |
| `src/image.rs` | container constants, `inspect`, `write_header`, `boot` |
| `src/xfer.rs` | handshake, TABLE, DATA/COPY/COMMIT receiver, ACK/NAK |
| `src/persist.rs` | open/create `HYPERV.IMG`, sector→LBA map, write changed sectors, header last |
| `src/sd/sdhost.rs` | PIO-only copy of `../src/host/sd/sdhost.rs` (init, CMD17, CMD24) |
| `src/sd/{regs,block_device}.rs` | shared with the hypervisor via `#[path]` |
| `src/mailbox.rs` | VideoCore core-clock query for the SD divider |
| `src/uart.rs`, `src/time.rs`, `src/crc.rs`, `src/panic.rs` | PL011, CNTPCT timing, CRC-32/adler32, panic → park |
| `linker.ld`, `build.rs` | link base `0x10000000`, `__data_end` for the relocation copy |

The SDHOST driver is a copy rather than a shared module on purpose:
the bootloader must keep working while the hypervisor's DMA save path
is being debugged, and it needs none of the DMA half. When the PIO
path in the hypervisor changes, mirror it here by hand.

## Failure modes

- Bad or interrupted upload: the container's CRC fails, nhboot reports
  it and waits for another upload; the card is never left with a
  header that claims a half-written payload (header sector is written
  last).
- SD failure during persist: reported as `persist: FAILED (…)`; the
  uploaded image still boots from RAM for that run.
- Only a bad `kernel8.img`/`config.txt` needs the card moved to the
  Mac — nhboot cannot update itself.
