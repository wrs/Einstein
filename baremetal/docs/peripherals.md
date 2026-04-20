# Newton peripheral spec (from Einstein)

This file captures the observable behaviour of the Newton peripherals we
care about in the hypervisor. Each section cross-references the Einstein
source file and line numbers where the behaviour is implemented — when
something here doesn't match reality, Einstein's C++ is the ground truth.

All addresses are *guest physical* (IPA) in hex unless noted. Word
accesses are little-endian from the guest's perspective; Einstein stores
some regions byte-swapped internally (the captured ROM in particular).

## Flash — internal store

Einstein class: `TFlash` (`Emulator/TFlash.{h,cpp}`).

Shape:
- Two 4 MiB banks at disjoint guest addresses: bank 0 at
  `0x02000000..0x02400000` (Einstein `kInternalFlash`), bank 1 at
  `0x10000000..0x10400000` (Einstein `kFlashBank2`). Einstein keeps
  both banks back-to-back in a single 8 MiB mmap-backed file, but the
  kernel sees them at their hardware addresses (not contiguous in
  guest-physical space).

Semantics that matter:

- **Raw access only.** The kernel manages the AMD-style programming state
  machine entirely in software; `TFlash` doesn't interpret command
  sequences. `Read` / `Write` / `ReadB` touch the backing bytes
  directly; `Erase` fills a region with `0xFF`.
- **Word order is big-endian inside the backing store**; `TFlash::Read`
  does `UByteSex_FromBigEndian` on the raw word. Byte reads select the
  matching byte within the big-endian word (`TFlash.cpp:269-279`).
- **Write takes a bit mask** (`TFlash::Write`): `*word = (existing & ~mask)
  | new_word`. Bits where `mask` is set come from the new word; bits
  outside the mask preserve their prior value. `mask = 0xFFFFFFFF` is
  a full-word write.
- **Erase is straight `0xFFFFFFFF` fill** of `block_size` bytes starting
  at `offset` within the selected bank.
- **First-boot seeding.** When the backing file is new (detected via
  `mFlashFile.GetCreated()`), `TFlash`'s constructor writes a Newton
  filesystem header at the start of block 0 (bank-0 offset 0) and
  duplicates it at the start of block 1 (bank-0 offset 0x10000). Bank 1
  is left zeroed. See `TFlash.cpp:137-172`:
  - offset `0x00`: `0x444C4453` ("DLDS")
  - offset `0x04`: `0x4F534344` ("OSCD")
  - offset `0x08`: `0x0000010C` (block-size or block-1 offset)
  - offset `0x50`: `0x444C4453` (second "DLDS")
  - offset `0x54`: `0xD7ECCC66` (checksum)
  - offset `0x58`: `0xFFFFFFFC` (block 0) / `0xFFFFFFF0` (block 1)
  - offset `0x8C`: `0xFFFFFFFF` (calibration-valid flag)
  - a few calibration words at `0x24` / `0x34` / `0x3C` and a zero
    "manufacture date" at `0x40`.
  Every other byte of the fresh backing is `0x00`, inherited from the
  zero-initialised mmap. **Not `0xFF`.** Software running on this
  pretends to see "erased = 0xFF" but the bytes on disk are zeros until
  explicitly written.

The hypervisor backs flash as stage-2 RW over raw memory; there's no
trap path required for plain R/W. We run the Einstein header seed on
every boot (our flash isn't yet persisted across hypervisor runs; once
it is, only a fresh backing should seed — see `peripherals/flash.rs`).

## Interrupt controller (VIC)

Einstein class: `TInterruptManager` (`Emulator/TInterruptManager.{h,cpp}`).

Shape:

- 4 × 32-bit timer-match registers at `0x0F18_2000` / `2400` / `2800` / `2C00`.
- `IntPresent`       R  `0x0F18_3000` — currently-raised interrupt bits.
- `IntCtrl`          R/W `0x0F18_3400` — per-bit enable (gate).
- `IntClear`         W  `0x0F18_3800` — writing clears the matching bits
                                         in `IntPresent`.
- `FIQMask`          R/W `0x0F18_3C00` — per-bit steering: set bit → FIQ;
                                         clear bit → IRQ.
- `IntEDReg{1,2,3}`  R/W `0x0F18_4000/4400/4800` — "edge/direction" flags
                                                   for external / GPIO
                                                   inputs.

Bit layout (`TInterruptManager.h:64-83`):

| bit     | source                       | notes                     |
|---------|------------------------------|---------------------------|
| 0x04    | RTC alarm                    |                           |
| 0x08    | timer 0 match                |                           |
| 0x10    | timer 1 match                |                           |
| 0x20    | timer 2 match                |                           |
| 0x40    | timer 3 match                |                           |
| 0x80    | DMA ch 0 (serial 0 rx)       |                           |
| 0x100   | DMA ch 1 (serial 0 tx)       |                           |
| 0x200   | DMA ch 2 (IR rx/tx)          |                           |
| 0x400   | DMA ch 3 (sound in)          |                           |
| 0x800   | DMA ch 4 (audio rx)          |                           |
| 0x1000  | DMA ch 5 (sound out)         |                           |
| 0x2000  | DMA ch 6 (modem rx)          |                           |
| 0x4000  | DMA ch 7 (modem tx)          |                           |
| 0x8000  | Keynes (BIO Interface, FIQ)  |                           |
| 0x10000 | PCMCIA 0                     |                           |
| 0x1000000 | GPIO                       |                           |
| 0x2000000 | PCMCIA 1                   |                           |
| 0x8000000 | Platform events            |                           |
| 0x10000000 | Tablet                    |                           |

Delivery gate (`TInterruptManager.cpp:561,573`):

```
fire_fiq = raised & int_ctrl &  fiq_mask
fire_irq = raised & int_ctrl & ~fiq_mask
```

Timer match semantics:

- Einstein doesn't implement auto-edge detection inside the class —
  that lives in the main-thread run loop around `TInterruptManager::WaitUntilInterrupt`.
- For the hypervisor we use an explicit `match_fired` bitmap: the match
  triggers on the rising edge of `ticks >= match_reg[i]`, and writing
  `match_reg[i]` clears the corresponding `match_fired` bit so a new
  match can fire.

Observed 717006 early-boot sequence (from `baremetal/probe/`):

```
FIQMask    <- 0x0C400000      (platform events + two FIQ-routed sources)
IntEDReg1  <- 0x0D400000
IntEDReg2  <- 0x0D400000
IntEDReg3  <- 0x0D400000
IntCtrl    <- 0x0D400000
```

The single IRQ-routed source enabled here is GPIO (bit 24,
`0x01000000`). No match register is programmed in the early phase.

## Ticks

Newton's tick counter ticks at 3.686400 MHz. Einstein keeps it monotonic
regardless of host wall-clock jumps (`TInterruptManager::GetTimer`,
backed by `clock_gettime(CLOCK_MONOTONIC)` and a suspend/resume
mechanism for debugger stops).

Hypervisor equivalent: scale the A53 generic timer (`CNTPCT_EL0`,
`CNTFRQ_EL0`) into the 3.6864 MHz domain. The guest reads the scaled
value through MMIO at `0x0F18_1800` (`kHdWr_Ticks` in
`Emulator/TMemoryConsts.h`).

## DMA manager

Einstein class: `TDMAManager` (`Emulator/TDMAManager.{h,cpp}`).

Shape:

- Bank 1 channel regs: `0x0F08_0000` .. `0x0F08_FBFF` (8 channels × 8
  regs, 4 B each, channel stride = 0x2000, reg stride = 0x400).
- Bank 2 channel regs: `0x0F09_0000` .. `0x0F09_7FFF` (same layout).
- Chip-wide:
  - `0x0F08_FC00` — channel-assignment register (R/W).
  - `0x0F09_8000` — enable / status (W-enable, R-status).
  - `0x0F09_8400` — disable (W).
  - `0x0F09_8800` — word-status (R).

Observed ground truth (TDMAManager.cpp):

- `mAssignmentReg` is the **only** piece of real state; writes store,
  reads return the last write (`TDMAManager.cpp:69-95`).
- `WriteEnableRegister` logs the write and does nothing observable.
- `WriteDisableRegister` same.
- `ReadStatusRegister` returns `0`.
- `ReadWordStatusRegister` returns `0`.
- Channel 0 / 1 per-register R/W delegates to the external-serial DMA
  driver via `mEmulator->SerialPorts.GetDriverFor(kExtr)`; channels
  2-7 ignore their per-register accesses (return 0 on read, log on
  write). The hypervisor can mirror this by only dispatching the
  assignment register and treating everything else as the obvious
  stub.

This is important: the Newton kernel's DMA driver manages the transfer
state machine in software; Einstein's model is almost an API stub.

## ARM processor interrupt callbacks

`TInterruptManager`'s worker thread calls
`TARMProcessor::{FIQ,IRQ}Interrupt` / `Clear{FIQ,IRQ}Interrupt` to
signal the emulated CPU. The hypervisor replaces this with
`HCR_EL2.VI` / `HCR_EL2.VF` toggled from the trap handler; no C++
method call is needed.

## Things we deliberately don't model yet

- `TSerialPorts` / `TSerialChip*` — serial UARTs. Kernel touches them
  during early init but we don't need a response for boot.
- `TPCMCIAController` — two 256 MiB socket windows at
  `0x3000_0000..0x4000_0000` (slot 0, `kPCMCIA0Base`) and
  `0x4000_0000..0x5000_0000` (slot 1, `kPCMCIA1Base`). Einstein
  also declares slots 2 and 3 but typical Newton hardware doesn't
  wire them up. Returning "card not present" (all-ones for probes,
  silent drops on writes) is correct. Ported as
  `peripherals/pcmcia.rs`.
- `TNativePrimitives` — coprocessor 10/11 gateway used by NewtonOS
  native drivers (screen, sound, tablet, battery). We'll need this
  eventually to drive the display. It's not on the pre-scheduler
  path.
- `TScreenManager::Blit` — gets a guest VA pointing at a framebuffer
  in RAM. Driving it requires intercepting the native-primitive call
  that hands over the bitmap pointer.
- `TFlash` save/restore (`TransferState`) — we don't snapshot state.

## Why we don't link Einstein directly

Originally the plan (in `IMPLEMENTATION.md`) was to link Einstein's C++
peripheral classes into a freestanding core and call them from Rust via
a C ABI. That turned out not to be worth the complexity:

- The simple peripherals (`TFlash`, `TDMAManager`) are 30-60 lines of
  logic once you strip out Einstein's save/restore and stdio-logging
  plumbing. Rust ports are comparable in size.
- The one with real mass — `TInterruptManager` — is mostly a
  `TThread` / `clock_gettime` scheduling wrapper around a small state
  machine, and none of that wrapper applies to a trap-driven
  hypervisor.
- Freestanding Einstein means stubbing pthread, stdio, exceptions,
  RTTI, mmap'd backing files, and an FFI boundary on both sides. The
  hypervisor never links the bare-metal target of any of that yet.

Instead: we port each peripheral's state machine directly into Rust
under `baremetal/src/peripherals/` and use this document plus the
Einstein source as the spec. Einstein still has value as a running
oracle for the probe (`baremetal/probe/`) and as authoritative source
code for the exact bit semantics; we just don't compile-link it into
the hypervisor.
