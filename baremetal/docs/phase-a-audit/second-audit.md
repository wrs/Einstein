# Phase A re-audit — 2026-04-21 (post-closeout)

Second pass on the Einstein↔hypervisor diff after the original audit's
closeout commits (`8ca2ff10` Phase A closeout handlers, `729173b4`
SWI-injection ROM patches). Same structure as the first audit — walks
Einstein's catalog in `einstein-non-rom-catalog.md` and confirms each
item against current `src/` HEAD.

## Status of the original tiered todo list

| # | Item | Status | Evidence |
|---|---|---|---|
| 1 | RAM mirror at IPA `0x0C00_0000` removed | ✅ done | `src/stage2.rs:114-120` — explicit "no mirror" comment; `set_l2_blocks` only touches 0x00, 0x02, 0x04, 0x0E, 0x10, pool windows. |
| 2 | Native prim 0x00 (flash driver) | ✅ done | `src/peripherals/flash_driver.rs` — Identify/Init/Write (16+32 bit)/StartErase/IsEraseComplete/BeginWrite, guarded by VTABLES_32BIT. |
| 3 | Native prim 0x01 (platform driver) | ✅ done | `src/peripherals/platform.rs` — 34 subfns covering New/Delete/Init, power, gestalt, GetNextEvent, user info, Log. |
| 4 | MIDR → SA-1100 | ✅ done | `src/guest.rs:66-68` — `msr vpidr_el2, #0x4401_A100`. VPIDR_EL2 overrides MIDR without trapping. |
| 5 | RTC calendar = host time | ✅ done | `src/peripherals/vic.rs:126-180` — host SYS_TIME + CNTPCT elapsed → seconds since 1904. `K_HDWR_CALENDAR_REG` read returns `calendar_seconds()`. |
| 6 | Platform Vers `0x0F00_0008` | ✅ done | `src/peripherals/vic.rs:357` returns `5`. **Note:** original audit's "`0x00010002`" was wrong — Einstein's `TPlatformManager::GetVersion()` is literally `return 5;` at `TPlatformManager.cpp:110`. Current code matches actual Einstein. |
| 7 | RAM size `0x0F00_1800 / 0x0F00_1C00` | ✅ done | `src/mmio.rs:71-72` returns `0x4040_0040` and `0`. Matches Einstein's `(pageCount<<24)\|(pageCount<<16)\|pageCount` pattern for 4 MiB (`TMemory.cpp:868-874`). |
| 8 | High-Speed Clock `0x0F11_0400 = 0x90` | ✅ done | `src/peripherals/vic.rs:359`. |
| 9 | DMA-complete IRQ firing | ✅ done (intentional no-fire) | `src/peripherals/dma.rs:96-103` — matches Einstein's `TDMAManager.cpp:112,147` where the IRQ post is commented out. |
| 10 | SWI-injection ROM patches | ✅ done (HVC rewrite) | `src/rom_patches.rs:161-311` — DebugStr/Debugger/RealClockSeconds/FTimeInSeconds/FDateFromSeconds replaced with inline ARM + HVC traps at `0x40/0x41`. |

All Tier 0 / 1 / 2 / 3 items (1-10) from the original plan are closed.
20 guest tests in the MANIFEST, matching the new handlers.

## Still absent (Tier 3-4, deferred from the original plan)

| Item | Einstein ref | Hypervisor behavior |
|---|---|---|
| Virtualized calls (bit-31) `__rt_sdiv`, `__rt_udiv`, `memmove`, `symcmp` | `TVirtualizedCalls.cpp` | `native_primitives::execute` halts with "virtualized-call path not wired up" |
| Native prim 0x02 Sound | `TSoundManager` | unknown-driver halt |
| Native prim 0x03 Battery | `TPlatformManager` BatteryDriver | unknown-driver halt |
| Native prim 0x05 Tablet | `TScreenManager` pen pipeline | unknown-driver halt |
| Native prim 0x06 Serial | `TSerialPortDriver` | unknown-driver halt |
| Native prim 0x07/0x08 In/Out Translators | UTF-8 convert | unknown-driver halt |
| Native prim 0x09 Host Calls (libffi) | `TNativeCalls` | unknown-driver halt |
| Native prim 0x0A Network | `TUsermodeNetwork` / NE2000 | unknown-driver halt |
| Native prim 0x0C Printer | `TPrinterManager` | unknown-driver halt |
| Screen subfns beyond blit (contrast, backlight, orientation) | `TScreenManager::ExecuteScreenDriverNative` | only subfn 0x4 is wired; others loud-halt |
| Serial RX plumbing | `TSerialHostPort` → host stdin | RX register reads 0 forever |
| RTC alarm → IRQ | `TInterruptManager` alarm cmp | alarm reg accepts writes, never fires |
| PCMCIA card emulation | `TATACard` / `TNE2000Card` / `TLinearCard` | window reads `0xFFFF_FFFF`, writes drop |
| Non-timer interrupt sources (DMA, Keynes, Platform events, PCMCIA GPIO, Tablet, GPIO 0-31) | `TInterruptManager.h:63-88` | state machine present, only 4 timers wire through |
| Flash 28F016 command-set state machine | `TFlash.cpp` | flash mapped RW raw; 717006 uses `TEinsteinFlashDriver`, so not on critical path |

These are the items the original Tier 3-4 list said to add on
first-touch halt (trip-wire discipline) rather than speculatively. No
evidence any of them have been touched yet.

## New observations from the fresh read

1. **Initial CPSR `0x01D3` (A bit set) vs. Einstein's `0x00D3`** —
   unchanged from original audit's §R note. Still diverges; still
   undetected as problematic. Low risk.

2. **CP14 (debug coprocessor)** — no EL2 trap installed; AArch32
   `MCR/MRC p14, ...` passes through to A53 hardware. Einstein reads 0
   / ignores writes. Current behavior returns A53-specific
   debug-register values. Phase B has not triggered this; if it does,
   add `EC=0x05` handler.

3. **HCR_EL2.TGE not set** — guest SWI goes to guest's own SVC vector
   at ROM `0x00000008` (the kernel's syscall handler). This is correct
   — none of our ROM patches use SWI anymore (they use HVC). No action
   needed; noted for completeness.

4. **TICK_PAGE non-trapping read** (`src/stage2.rs:82-97`) — we serve
   the hot `K_HDWR_TICKS` read from a stage-2 RO page. Einstein traps
   every read. This is an optimization, not a divergence; flagged so
   the next auditor knows to expect it.

5. **"Probe-for-absent" regions** (`src/mmio.rs:52-62`) —
   `0x0800_0000..0x0900_0000` (absent RAM bank) and
   `0x1040_0000..0x2000_0000` (no extra REx/flash) are modeled as
   deterministic zero reads + dropped writes so the Newton kernel's
   presence probes conclude "absent" cleanly. Einstein just doesn't
   map them, producing the same observable behavior through a
   different mechanism.

6. **Pattern: every deliberate stub halts on the sub-case outside its
   whitelist.** The Phase A loud-halt discipline (`mmio.rs`, `vic.rs`,
   `dma.rs`, `serial.rs`, `native_primitives.rs`, `platform.rs`,
   `flash_driver.rs`) is consistently applied. Unknown addresses /
   subfns / driver classes all trip wires instead of silent-stubbing.
   Good for Phase B.

## Verdict

Every item on the original Phase A plan is landed. The current set of
"absent" items are the ones explicitly scoped as
deferred-until-first-touch. No new gaps surfaced that weren't already
known.

One minor correction: the original audit's §F claim that Einstein
returns `0x00010002` for `0x0F00_0008` was off-by-reference
(`kUP2Version` is the gestalt payload, not the MMIO read). The current
code returning `5` is correct.
