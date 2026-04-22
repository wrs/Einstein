# Phase A Closeout Audit — Einstein vs. Hypervisor

## Context

Phase A was declared done in `PLAN.md` line 3: *"Phase A is done. Phase B is mid-flight."* with the claim that every known-required piece had landed as a real handler plus a passing guest test. Phase B has since repeatedly discovered Phase A omissions under full ROM boot — the tracer R0/R1 clobber, the DebuggerUND advance-by-8 bug, `fix_stage1_xn_bits` missing late L2 populations, and now the suspected stage-2/stage-1 RAM mirror mismatch at IPA `0x0C00_0000`. Pattern: guest tests pass in isolation, full ROM boot keeps finding gaps.

This audit walks Einstein's `Emulator/` tree end-to-end, catalogs every category of non-ROM-execution work, and diffs it against `src/*.rs`. The diff *is* the Phase A todo list. Tiering below reflects "blocks current stall" vs. "latent but will trip us next" vs. "baseline-parity needed before Phase B can close" vs. "deferrable".

---

## Einstein's non-ROM-execution catalog (authoritative)

Every non-execution behavior Einstein performs, cited with the Emulator/ reference:

### A. ROM load-time patches (`Emulator/JIT/Generic/TJITGenericROMPatch.cpp`)
Word-write patches **we already have**:
- `0x0014_12F8` — avoid screen calibration
- `0x000D_B0D8 / 0x000D_B0DC` — BeaconDetect no-op
- `0x0000_13F4` — `gDebugger` on
- `0x0000_13FC` — `gNewtConfig` (kEnableListener | kDefaultStdioOn | kEnableStdout)
- `0x0008_A20C` — ignore setting time
- `0x0030_F088 / 0x0042_0750 / 0x0042_0798 / 0x004D_CA14` — Y2010 time-base constants

SWI-injection patches Einstein applies that **we do not** (deferred per `src/rom_patches.rs:13-17`):
- `0x0038_CE6C` — DebugStr logging
- `0x0038_CE70` — Debugger breakpoint
- `0x0025_5578` — RealClockSeconds (host time injection)
- `0x0008_9B80` — FTimeInSeconds (Y2010)
- `0x0008_A8A8` — FDateFromSeconds (Y2010)

Virtualized-call patches (bit-31 marker, `TVirtualizedCallsPatches`) we do not have:
- `__rt_sdiv`, `__rt_udiv`, `memmove`, `symcmp__FPcT1`

### B. Instruction-level traps
- **CP10/11 MCR** → `TNativePrimitives::ExecuteNative` (`Emulator/TNativePrimitives.cpp:177`). Driver ID in bits[23:8], subfunction in bits[7:0]. 12 driver classes (see §C).
- **CP10/11 MCR with bit-31 set** → `TVirtualizedCalls::Execute` (`Emulator/NativeCalls/TVirtualizedCalls.cpp`).
- **Undefined opcodes** at `0x0F00_0000..0x0F00_0002` → `SystemBootUND`, `DebuggerUND`, `TapFileCntlUND`.
- **SWI** with bits[23:22] set → rerouted to JIT patches.
- **Breakpoint (BKPT)** → `TEmulator::Breakpoint(ID)`.

### C. Native-primitives driver dispatch (`TNativePrimitives.cpp`)
| Driver ID | Class | Subfn range | Purpose |
|---|---|---|---|
| `0x000000` | Flash | 0x01–0x0C | Identify, Init, Write, Erase, Query |
| `0x000001` | Platform | 0x01–0x1E+ | PowerOn/Off, event queue, gestalts, user info, PCMCIA power |
| `0x000002` | Sound | 0x01–0x0B | ScheduleOutputBuffer, Start/Stop, volume, DMA setup |
| `0x000003` | Battery | 0x01–0x08+ | Status, voltage, charge |
| `0x000004` | Screen | 0x01–0x0A+ | Updates, orientation, backlight, contrast |
| `0x000005` | Tablet | 0x01–0x0E | Pen events, calibration |
| `0x000006` | Serial | 0x01–0x05 | UART config, DMA setup |
| `0x000007` | In-Translator | 0x01–0x06 | UTF-8 decode |
| `0x000008` | Out-Translator | 0x01–0x06 | UTF-8 encode |
| `0x000009` | Host Calls | 0x01–0x7F | FFI / libffi bridge |
| `0x00000A` | Network | 0x01–0x0A+ | GetMAC, Open/Close, Send/Recv packet |
| `0x00000C` | Printer | 0x01+ | Setup, SendPage, Close, status |

### D. MMIO map (`Emulator/TMemoryConsts.h:43-159`, `TMemory.cpp`)
- `0x0000_0000..0x0100_0000` — ROM (identity) — *we have*
- `0x0200_0000..0x0240_0000`, `0x1000_0000..0x1040_0000` — Flash via `TFlash` (28F016 command-set model) — *we have RW storage only, no command-set model*
- `0x0400_0000..+RAMSize` — RAM — *we have, but layout may diverge at `0x0C00_0000` mirror*
- `0x0F00_0008` — Platform Version (returns `0x00010002` for UP2) — **we do not explicitly model**
- `0x0F00_1000` — Memory Access Speed — *we have as write-accept*
- `0x0F00_1800 / 0x0F00_1C00` — RAM Size registers — **we do not explicitly model**
- `0x0F08_xxxx / 0x0F09_xxxx` — DMA bank 1/2 + assignment/enable/status — *we have stub: assign latches, rest 0*
- `0x0F11_0000` — External Interrupt Mask — *we have via VIC*
- `0x0F11_0400` — High-Speed Clock constant `0x90` — **we do not explicitly model**
- `0x0F18_1000` — RTC calendar (seconds since 1904) — *we have tick page at +0 returning 0*
- `0x0F18_1400` — Alarm register — *we have tick page at +0x400 returning 0*
- `0x0F18_1800` — Ticks (3.6864 MHz) — *we have, functional, non-trapping*
- `0x0F18_2000..0x0F18_2C00` — Match reg 0..3 — *we have, functional*
- `0x0F18_3000/3400/3800/3C00` — Int present / ctrl / clear / FIQ mask — *we have, functional*
- `0x0F18_4000/4400/4800` — Int enable/disable 1/2/3 — *we have*
- `0x0F18_C000/C400` — GPIO raised / enable — *we have as stub*
- `0x0F18_D400` — GPIO PCMCIA card detect — *we have as stub*
- `0x0F1C_0000..0x0F20_0000` — 4× serial ports (Voyager UART) — *we have status=empty stub*
- `0x0F24_0000/0800` — External data abort regs — *stubbed*
- `0x0F24_1000` — Bank control — *stubbed*
- `0x3000_0000..0x7000_0000` — PCMCIA sockets 0..3 — *we stub 0+1 as "no card"*

### E. Interrupt sources (`TInterruptManager.h:63-88`)
21 wired masks (RTC alarm, 4 timers, 8 DMA ch, Keynes, 2 PCMCIA, GPIO, Platform, Tablet). We have the **state-machine plumbing** (`int_present`/`int_ctrl`/match regs/edge latches) but **only the 4 timer sources are wired to actually fire**. DMA, PCMCIA, Keynes, Platform events, GPIO, Tablet are all silent.

### F. Timers & RTC (`TInterruptManager.cpp`)
- Ticks at 3.6864 MHz — *we have*
- Calendar (seconds since 1904) patched at `0x0025_5578` to inject host time — *we return 0; patch missing*
- 4 match registers vs. Ticks — *we have*
- Alarm reg vs. Calendar — *we do not arm alarm→RTC IRQ*

### G. DMA (`TDMAManager.cpp`)
8 channels (serial 0 RX/TX, IR, audio TX/RX, tablet, serial 3 RX/TX). Einstein emulates register I/O and **fires DMA-complete IRQs immediately** without actual transfers. We have: assignment register latches, all other reads return 0, no IRQ firing at all.

### H. Coprocessor handling (`TARMProcessor::SystemCoprocRegisterTransfer`, `TARMProcessor.cpp:67`)
- **MIDR** — Einstein returns `0x4401_A100` (Intel SA-1100 StrongARM) or `0x4104_7102` (DEC). *We return Cortex-A53 MIDR.* Risk: ROM code may conditionalize on MIDR — our `FlushTheCache` experience at `0x18924` already confirmed this pattern (`MCR c7 c7 0` branch was taken because A53 MIDR selected the ARMv4 deprecated path).
- SCTLR / TTBR / DACR / VBAR / FSR / FAR / cache-ops / TLB-ops — *we have*
- CP14 debug — Einstein reads 0 / writes ignored. *We halt on unexpected CP14 accesses.*

### I. Serial (`Emulator/Serial/TSerialPortDriver.h`)
4 UART emulations with TX/RX bridged to **host stdin/pipes/TCP** via `TSerialHostPort` subclasses. We stub TX (logged, consumed) and RX (returns 0 forever).

### J. Screen / Tablet (`Emulator/Screen/TScreenManager.cpp`)
- Framebuffer blit — *we have as native primitive 0x4*
- Contrast, backlight, orientation — **missing**
- Tablet pen events → DMA ch 5 + IRQ `0x1000_0000` — **missing**
- Tablet calibration 5×u32 struct — **missing**

### K. Sound (`Emulator/Sound/TSoundManager.h`)
8/16-bit PCM, DMA ch 3/4, volume control, platform backends (null / SDL / Android). **Entirely missing** in hypervisor.

### L. PCMCIA (`Emulator/PCMCIA/`)
`TPCMCIAController` + `TATACard` / `TNE2000Card` / `TLinearCard`. Attribute/common memory, power control (Vcc/Vpp via native 0x0A), insertion GPIO signaling. We return `0xFFFFFFFF` ("no card") for the whole window.

### M. Flash storage (`Emulator/TFlash.cpp`)
Host-file-backed, models the Intel 28F016 command state machine via native primitive 0x00 (Identify / Write / Erase / IsEraseComplete). Platform 16/32-bit detected via vtable address (`TNativePrimitives.cpp:378-386`). **We have raw stage-2 RW with DLDS/OSCD header seeding but no command-set model** — current Phase B stall.

### N. Platform Manager (`Emulator/Platform/TPlatformManager.cpp`)
- PowerOn/Off (native 0x0F / 0x0E)
- PowerOnSubsystem / PowerOffSubsystem (native 0x0A/0x0B, e.g. 0x1D = flash)
- Event queue lock/unlock (native 0x18/0x19), SendAEvent, GetNextEvent (native 0x15)
- Gestalt (native 0x17) — returns `0x00010002` UP2 at `0x0F00_0008`
- User info (native 0x1B) — name, company, owner, serial
- Power-off mask `0x0C40_0000` (Reset + FIQ + IRQ enable)

**Entirely missing** in hypervisor — and the kernel hits PowerOff paths after flash-identify fails (trace entries 62–72 in `INVESTIGATION.md:30-36`).

### O. Host Calls / Native Calls (`Emulator/NativeCalls/TNativeCalls.cpp`)
libffi bridge — out of Phase B critical path; defer.

### P. Virtualized Calls (`Emulator/NativeCalls/TVirtualizedCalls.cpp`)
`__rt_sdiv`, `__rt_udiv`, `memmove`, `symcmp`. Performance optimization; the ROM runs these natively on A53 fine. Not Phase A blocking.

### Q. Network / Printer
Network: `TUsermodeNetwork` / `TTapNetwork` + NE2000 card emulation. Printer: `TPrinterManager`. Both out of Phase B critical path; defer.

### R. Initial CPU state (`TARMProcessor::Reset`, `TARMProcessor.cpp:382`)
R0–R12=0, SP_svc=0, LR_svc=0, PC=0x4 (reset vector + prefetch), CPSR=`0x0000_00D3` (SVC | I | F).
**Our state** (`src/guest.rs:65`): x0–x14=0, CPSR=`0x0000_01D3` (SVC | I | F | **A**).
Divergence: we also mask SError (A bit). Likely benign but noted.

---

## Hypervisor's actual coverage (condensed)

Confirmed by reading `src/` directly:
- **Functional:** EL2 MMU, stage-2 tables (L1/L2/L3), CP15 15-tuple shim, UND handler (SWP/SWPB + Einstein UNDs + CP15 quirks), HVC test protocol + UND_TAG + DIAG, VIC state machine (4 timer sources), CNTHP + tick page, section+L2 XN normalization, fine-table rewrite on every MMU-enable edge, 10 word-write ROM patches, shadow-stub lazy patching, function tracer, snapshot save/resume.
- **Stubs (functional for current boot):** 4× serial (status=empty / TX consumed / RX=0), DMA regs (assign latches, rest 0), flash (raw RW + seeded headers, no command set), PCMCIA ("no card"), ~40 write-accept MMIO registers.
- **Minimal:** Native primitives — only `(0x000000, 0x00)` null-test + `0x4` screen blit.
- **Absent:** Flash 28F016 command set; all of Platform Manager (power/events/gestalt); Tablet; Sound; PCMCIA card emulation; Serial RX; virtualized calls; SWI-injection ROM patches (DebugStr / Debugger / RealClockSeconds / F{Time,Date}*Seconds); Network; Printer; DMA-complete IRQ firing; Keynes/Platform/Tablet/GPIO IRQ sources; RTC-alarm→IRQ.

---

## The gap — Phase A closeout todo list

### Tier 0 — Foundational, blocking current stall
Phase A claimed "memory layout settled" (`HIGHLEVEL.md` §5.2) but `INVESTIGATION.md:71-88` demonstrates a stage-2/stage-1 disagreement at IPA `0x0C00_0000`:

1. **Resolve the RAM mirror at IPA `0x0C00_0000`.**
   - *Symptom:* `RExScanner` writes (via pre-MMU IPA `0x0C10_64AC`) and post-MMU reads (via stage-1 VA → IPA `0x0400_D4AC`) land in different host cells. REx 'fdrv' table comes back zero → kernel falls back to built-in `T28F016_SA_SVDriver` → flash identify fails.
   - *Einstein reference:* Einstein's `TMemory` does not treat IPA `0x0C` as RAM at all; only `0x0400_0000..mRAMEnd` is RAM. Writes to `0x0C` go through a different path we haven't mapped.
   - *Action:* Audit stage-1 walks for VA `0x0C00_0000+` pre- vs. post-MMU. Either remove the RAM mirror at IPA `0x0C00_0000` (match Einstein's `TMemory` layout exactly), or map the mirror to encode the stage-1 permutation. First step is diagnostic: confirm what PA pre-MMU writes at VA `0x0C10_64AC` actually land at in Einstein.
   - *Critical file:* `src/stage2.rs`, `src/guest_mem.rs`.

### Tier 1 — Known next-trip after Tier 0 unblocks
Once the REx is visible, the flash-identify stall evaporates because the kernel will pick `TEinsteinFlashDriver`. That driver is invoked via **native primitive driver ID `0x00`**, which we do not implement:

2. **Native primitive 0x00 (Flash driver) — port TEinsteinFlashDriver.**
   - *Einstein ref:* `Emulator/TNativePrimitives.cpp:263` (`ExecuteFlashDriverNative`), subfns 0x01 Identify / 0x08 Write16-32 / 0x09 Erase / 0x0B IsEraseComplete.
   - *Action:* Implement in `src/peripherals/native_primitives.rs` as a new driver class. Back writes/erases against the existing `GUEST_FLASH` buffer.
   - *Critical file:* `src/peripherals/native_primitives.rs`, new `src/peripherals/flash_driver.rs`.

3. **Native primitive 0x01 (Platform driver).**
   - *Why:* Trace entries 62–70 in `INVESTIGATION.md` (IOPowerOffAll, GetPlatformDriver, DisableAllInterrupts, PowerOffSystem) all route through Platform driver primitives. Even if we never actually want to power off, we need to not halt loudly when the kernel probes the driver.
   - *Einstein ref:* `TPlatformManager` + `TNativePrimitives::ExecutePlatformDriverNative`. Subfns 0x0A/0x0B (PowerOnSubsystem / PowerOffSubsystem), 0x0E/0x0F (PowerOff/On), 0x15 (GetNextEvent), 0x17 (GetGestalt), 0x18/0x19 (lock/unlock event queue), 0x1B (GetUserInfo), 0x1E (GetPCMCIAPowerSpec).
   - *Action:* Stub each subfn to the equivalent "nothing happening" return (empty event queue, no events, zero user info, default gestalt). Same loud-halt discipline for unknown subfns.
   - *Critical file:* new `src/peripherals/platform.rs`.

### Tier 2 — Baseline-parity needed for Phase B close
Einstein does these; we don't; they will be probed before end-of-boot:

4. **MIDR emulation.**
   - *Einstein ref:* `TARMProcessor.cpp:67` returns `0x4401_A100` (SA-1100) for MP2100. ROM `FlushTheCache` already branched on MIDR at `0x18924` and selected the ARMv4 path (confirmed in `INVESTIGATION.md:95-98`). Other ROM code paths almost certainly do the same.
   - *Action:* Trap `MRC p15, 0, Rt, c0, c0, 0` and return `0x4401_A100`. Our CP15 shim already handles MIDR (`src/trap.rs:1570`); change the returned value rather than leaking A53 MIDR through.
   - *Critical file:* `src/trap.rs` (MIDR handler in `handle_cp15_trap`).

5. **RTC calendar / host-time injection.**
   - *Einstein ref:* `0x0025_5578` SWI-injection patch + `TJITGenericROMPatch.cpp:110` `RealClockSeconds`. Patches the ROM's RTC read to return host wall-clock.
   - *Action:* Either land the SWI-injection mechanism (see item 10) or map `0x0F18_1000` calendar-read through a handler that returns `seconds_since_1904(host_wall_clock)`.
   - *Critical file:* `src/peripherals/vic.rs` (calendar read path).

6. **Platform Version register `0x0F00_0008` = `0x00010002` (UP2).**
   - *Einstein ref:* `TMemory.cpp:947`.
   - *Action:* Add as an explicit MMIO read-only stub.
   - *Critical file:* `src/peripherals/mmio.rs` (or equivalent).

7. **RAM size registers `0x0F00_1800 / 0x0F00_1C00`.**
   - *Einstein ref:* `TMemory.cpp:868-874`. Returns an encoded pattern / zero.
   - *Action:* Return the 4-MiB pattern Einstein returns.
   - *Critical file:* `src/peripherals/mmio.rs`.

8. **High-Speed Clock `0x0F11_0400` = `0x90`.**
   - *Einstein ref:* `TMemory.cpp:898-900`.
   - *Action:* One-line stub.
   - *Critical file:* `src/peripherals/mmio.rs`.

9. **DMA-complete IRQ firing.**
   - *Einstein ref:* `TDMAManager.cpp` — on enable-bit write with complete descriptors, Einstein posts the channel's IRQ via `TInterruptManager`.
   - *Action:* When the guest writes the enable register for an armed channel, latch the per-channel IRQ mask into `int_present` and let `update_virq()` deliver it. No real transfer needed yet — the Newton serial/tablet/audio drivers just need the completion signal.
   - *Critical file:* `src/peripherals/dma.rs`, `src/peripherals/vic.rs`.

### Tier 3 — Einstein-baseline, deferrable but in the "Phase A parity" list

10. **SWI-injection ROM patch mechanism** (DebugStr `0x0038CE6C`, Debugger `0x0038CE70`, RealClockSeconds `0x0025_5578`, FTimeInSeconds `0x0008_9B80`, FDateFromSeconds `0x0008_A8A8`).
    - Einstein overwrites the call site with `SWI 0xEFC0_xxxx`; EL2 catches the SWI, reads the index, dispatches host code. We need either the same SWI-index dispatcher, or a ROM-patch alternative that installs a small ARM stub returning the right value.
    - *Critical file:* `src/rom_patches.rs`, `src/trap.rs` (SWI dispatcher).

11. **Virtualized calls dispatch** (`__rt_sdiv`, `__rt_udiv`, `memmove`, `symcmp`). Performance only; A53 runs the natural code fine. Defer unless a profile points at one of these.

12. **Remaining native-primitives classes** (0x02 Sound, 0x03 Battery, 0x05 Tablet, 0x06 Serial, 0x07/08 Translators, 0x09 Host Calls, 0x0A Network, 0x0C Printer). Add on first-touch halt — the loud-halt discipline is the trip-wire. No need to land them speculatively.

13. **Screen native-primitive subfns** beyond blit (contrast / backlight / orientation). Will trip when the kernel configures the display post-flash. Add on first touch.

14. **Serial RX plumbing.** Probably not needed for boot but listed in PLAN.

15. **Interrupt sources wiring for Keynes / Platform events / PCMCIA GPIO / Tablet / GPIO 0-31.** Hook up as we reach them; no speculative wiring.

### Tier 4 — Explicitly out of Phase A / defer without regret
PCMCIA card models, Sound backend, Network stack, Printer, libffi, iOS-integration primitive 0x0B.

---

## Critical files

Modified / new:
- `src/stage2.rs` — RAM mirror audit (Tier 0)
- `src/guest_mem.rs` — RAM mirror audit, possibly VA→IPA layout changes (Tier 0)
- `src/peripherals/native_primitives.rs` — driver-class dispatcher extension (Tier 1/2)
- new `src/peripherals/flash_driver.rs` — TEinsteinFlashDriver port (Tier 1)
- new `src/peripherals/platform.rs` — Platform driver primitives (Tier 1)
- `src/trap.rs` — MIDR value, later SWI-dispatcher (Tier 2/3)
- `src/peripherals/mmio.rs` — new registers `0x0F00_0008`, `0x0F00_1800`, `0x0F00_1C00`, `0x0F11_0400` (Tier 2)
- `src/peripherals/vic.rs` — calendar/alarm read, DMA-IRQ latching (Tier 2)
- `src/peripherals/dma.rs` — DMA-complete IRQ posting (Tier 2)
- `src/rom_patches.rs` — SWI-injection patches (Tier 3)

## Existing utilities to reuse
- `peripherals::screen::handle` (`src/peripherals/screen.rs`) — template for new native-primitive driver classes.
- `cpu::halt` + the native-primitives loud-halt discipline in `src/peripherals/native_primitives.rs:56-68` — preserve for all new dispatchers.
- `vic::poll_timer_matches` → `update_virq` pipeline — reuse for DMA-complete IRQ delivery.
- `kprintln!` vs. `dprintln!` — use `dprintln!` for recurring diagnostic log to preserve trace output (`CLAUDE.md` logging budget).

## Verification

Per item:
- **Tier 0 (RAM mirror):** Instrument with a canary — write a known pattern at VA `0x0C10_64AC` pre-MMU, enable MMU, read back via stage-1. Expect same value. If not, we have the fix target.
- **Tier 1 (flash native prim + REx visibility):** With REx reachable, the trace should advance past function 72 (`CheckFor1LaneFlash` → `SearchForFlashDrivers` returning a non-null driver). Run `cargo run --release --features trace,quiet` and confirm function trace reaches >72 entries.
- **Tier 2 (MIDR + RTC + register stubs + DMA IRQ):** Run existing `guest-tests/scripts/run-all.sh` — all 13 must still pass. Cold-boot trace should advance further than the current 72-function watermark.
- **Tier 3 (SWI-injection):** Land one patch at a time (start with RealClockSeconds); confirm via guest test that reads the clock returns host wall time.

Overall gate: trace reaches `TInterpreter::TInterpreter` at `0x002F_40E0` (Phase B goal per `CLAUDE.md`).

## Open questions

1. **Tier 0 approach — mirror vs. removal?** Einstein doesn't have a RAM mirror at IPA `0x0C`. Should we remove ours to match, or does our pre-MMU boot require the mirror? Need to audit why the mirror exists (git blame on `stage2.rs`) before deciding.
2. **MIDR change impact on existing guest tests.** Our tests may assume A53 MIDR. Check for MIDR reads in `guest-tests/` before changing the returned value.
3. **Snapshot compatibility when we change MIDR / RAM layout.** Both changes invalidate existing snapshots — plan for a `rm /tmp/newton-snapshot-*.bin` cold-boot cycle after landing Tier 0/Tier 2.
