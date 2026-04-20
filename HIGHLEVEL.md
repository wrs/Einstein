# Newton 2.x on Bare-Metal Pi Zero 2 W — High-Level Design

**Status:** draft
**Target host:** Raspberry Pi Zero 2 W (BCM2710A1, Cortex-A53 ×4)
**Guest:** Newton OS 2.x ROMs (unmodified)
**Relationship to Einstein:** reuses Einstein's peripheral emulation classes; replaces Einstein's software MMU, JIT, and host-OS layer.

## 1. Goal

Boot an unmodified Newton 2.x ROM on a bare-metal Pi Zero 2 W such that the guest CPU instructions execute natively on the A53 under a small Type-1 hypervisor running at EL2. Reuse Einstein's peripheral emulation (`TDMAManager`, `TInterruptManager`, `TSerialChip*`, `TScreenManager`, `TSoundManager`, `TFlash`, `TPCMCIAController`) invoked from EL2 trap handlers. Replace Einstein's software MMU and JIT entirely.

### Non-goals (v1)

- Newton 1.x ROMs.
- Einstein's FLTK / SDL / Cocoa UI.
- Networking via a host IP stack.
- Running under Linux.
- Other Pi models (Pi 4/5 port is a follow-on).

## 2. Why this is plausible

- Cortex-A53 implements ARMv8-A with AArch32 execution at EL0/EL1, including the VMSAv7 short-descriptor MMU format (1 MiB sections, 64 KiB large pages, 4 KiB small pages, domains, AP bits). See Open Question §16.1.
- Einstein's `Emulator/TMMU.cpp` contains an annotated dump (lines 1141–1248) of MMU state from a running 2.x ROM. Every mapped region uses sections, 64 KiB large pages, or 4 KiB small pages. **No fine tables, no tiny pages.** To be re-verified across all 2.x ROM variants we care about (§16.2).
- All Newton MMIO lives in one contiguous region at `0x0F000000+` (`Emulator/TMemoryConsts.h:54`). Stage-2 trap-and-emulate is the natural handling.
- Einstein already has working implementations of every Newton peripheral as C++ classes with clean register-level entry points (see the dispatch table at `Emulator/TMemory.cpp:865+`). They lift cleanly out of the host-OS-dependent harness.

## 3. Architecture

```
  +--------------------------------------------------------+
  | EL0 (PL0): NewtonScript tasks, apps, most ROM code*    |
  | EL1 (PL1): Newton kernel, SWI/IRQ/FIQ/ABT/UND handlers |
  |   -- guest stage-1 MMU walks Newton page tables --     |
  +--------------+-----------------------------------------+
                 | stage-2 faults, HVC, CP15 traps, undef
  +--------------v-----------------------------------------+
  | EL2: Newton Hypervisor                                 |
  |   - world setup, stage-2 mapping                       |
  |   - trap dispatch: MMIO, CP15, SWP, undef              |
  |   - vIRQ/vFIQ injection                                |
  |   - reused Einstein peripheral managers                |
  |   - bare-metal Pi drivers                              |
  +--------------------------------------------------------+
```

\* The kernel-only-in-PL1 split is an assumption pending verification (§16.3).

### 3.1 Components

| Layer | Status | Source |
|---|---|---|
| EL2 init, stage-2 MMU, trap vectors | new | — |
| Page-table seeding (guest physical layout) | new | `TMemoryConsts`, `TFlash` image format as reference |
| Trap decoder (`ESR_EL2` / `HPFAR_EL2` → handler) | new | — |
| Peripheral emulation | reused, reglued | `TDMAManager`, `TInterruptManager`, `TSerialPortManager` + `TSerialChip*`, `TSoundManager`, `TScreenManager`, `TFlash`, `TPCMCIAController`, `TNetworkManager` |
| CP15 shim | new | `TARMProcessor` CP15 dispatch as reference |
| Pi bare-metal drivers (UART, mailbox/framebuffer, SD, USB HID, I2S) | new | — |
| Boot/config loader | new | Einstein prefs format as reference |

## 4. Boot flow

1. Pi boots with stock firmware and a `config.txt` selecting our image as `kernel=`.
2. Our image starts at EL2 (verify firmware handoff state on Pi Zero 2 W — §16.1).
3. EL2 init:
   - enable MMU and caches at EL2;
   - allocate guest-physical region in Pi DRAM;
   - load ROM image and flash image from SD into guest physical;
   - program `VTCR_EL2` for stage-2 translation;
   - build stage-2 tables (identity map over guest RAM; no-access over `0x0F000000`–`0x0F3FFFFF` and other MMIO windows).
4. Configure `HCR_EL2`: `VM=1`, `AMO=IMO=FMO=1` for interrupt virtualization, `TVM`/`TRVM`/`TID*`/`TSW`/`TWI`/`TWE` as appropriate to trap guest system-register access.
5. Set `SPSR_EL2` for return to EL1 AArch32 SVC mode; set `ELR_EL2 = 0x00000000` (ROM reset vector in guest VA); `ERET`.
6. Newton ROM boots as if on real hardware. Peripheral accesses fault to EL2; EL2 dispatches to Einstein peripheral managers.

## 5. Memory model

### 5.1 Guest physical layout (from `TMemoryConsts.h`)

| Range | Contents |
|---|---|
| `0x00000000 – 0x00FFFFFF` | ROM (low + high) |
| `0x02000000 – 0x023FFFFF` | Flash bank 1 (internal store) |
| `0x04000000 – …` | RAM (size configurable; 4/8/16 MiB) |
| `0x0F000000+` | MMIO (trap) |
| `0x3C000000+`, `0x4C000000+` | PCMCIA windows (trap or back with real memory — §16.10) |

### 5.2 Host physical

Carve ~32 MiB from Pi DRAM for guest physical; remainder is EL2 heap, framebuffer, and hypervisor code/data.

### 5.3 Stage-2

4 KiB granule. Identity map guest→host inside the carved region. Everything outside faults. ROM region backed by the loaded image with stage-2 read-only as defense-in-depth. MMIO regions stage-2 `no-access` so every guest touch faults to EL2.

### 5.4 Stage-1 (guest)

The Newton's own page tables. Hardware walks them. AP bits, domains, and cacheability attributes are preserved unchanged. No software shadow table, no AP flattening.

## 6. CPU and mode handling

Guest runs natively at EL1 AArch32. No JIT, no interpreter. Newton's SVC/IRQ/FIQ/ABT/UND vectors are entered by the hardware exactly as on StrongARM. Banked registers, SPSR, CPSR handled by the CPU.

### 6.1 ARMv4-vs-ARMv8-AArch32 deltas needing trap-and-emulate at EL2

- **CP15.** StrongARM's system coprocessor register set differs from A53's. Trap guest CP15 access via `HCR_EL2.TVM`/`TRVM`/`TID*`. Maintain a shadow of guest CP15 (TTBR, DACR, SCTLR bits) and program real CP15 to the equivalent intent (§16.4).
- **SWP / SWPB.** UNDEFINED in ARMv8. Trap via the undefined-instruction vector, emulate atomically with `LDREX` / `STREX`. If hot, patch the ROM (§16.5).
- **`MRS Rd, SPSR` while in User mode.** `Emulator/TARMProcessor.cpp:760` notes StrongARM returned CPSR here; A53 treats it as UNPREDICTABLE. Trap in undef vector, return CPSR.
- **Cache maintenance ops.** StrongARM CP15 c7 encodings differ from A53's. Trap and translate each to its A53 equivalent or a safe no-op (§16.7).
- **Imprecise data aborts and other ARMv4 edge cases.** Enumerate empirically; maintain a fixup table.

### 6.2 Thumb

Newton doesn't use it. `HSCTLR.TE = 0`.

## 7. Interrupts

- Pi peripherals raise IRQs at the BCM2710 interrupt controller. Route to EL2 via `HCR_EL2.IMO` / `FMO`.
- EL2 ISRs drive bare-metal drivers (timer tick, UART RX, SD, USB).
- Peripheral managers decide when the guest should see a virtual Newton interrupt. EL2 updates the Newton VIC shadow state (`TInterruptManager`) and raises `VI` / `VF` to the guest via `HCR_EL2`. The A53 vectors to the guest's IRQ/FIQ handlers.
- Timer: use the A53 generic timer at EL2 for host ticks; synthesize Newton's 3.6864 MHz tick and match registers (`TMemoryConsts.h:85–92`) from it.

## 8. Peripherals — guest side (reused from Einstein)

Reuse Einstein classes; adapt only the memory-interface boundary so the "register read/write" entry points are called from EL2 trap handlers instead of from the software MMU.

- `TInterruptManager`, `TDMAManager`, `TFlash` — pure state machines, no host-OS dependencies.
- `TScreenManager` — redirect output to the Pi framebuffer.
- `TSoundManager` — replace host audio backend with I2S (or PWM as a shortcut).
- `TSerialPortManager` / `TSerialChip*` — route external-serial to the Pi mini-UART or USB-CDC.
- `TPCMCIAController` — back with a file-image card from SD.
- `TNetworkManager` — out of scope for v1; stub.

## 9. Peripherals — Pi side (new bare-metal drivers)

- Mailbox + framebuffer (VideoCore) for display.
- SD/EMMC for ROM, flash, and card images.
- USB host (dwc_otg) for HID — keyboard, mouse-as-pen. The full USB stack is the biggest single new subsystem. **v1 shortcut:** PS/2-over-GPIO or a UART-driven input tunnel to defer USB.
- I2S for audio (or PWM for v1).
- Mini-UART for console and gdb stub.

## 10. Debug and development workflow

- Serial console on GPIO 14/15. EL2 panic handler dumps guest + host state.
- gdb stub at EL2, exposes guest ARM state (banked regs, CPSR, SPSR, CP15 shadow).
- Einstein's `TJITGenericROMPatch` mechanism is reusable as a breakpoint primitive: install a trapping instruction at the guest VA, handle in EL2.
- Build on dev host; deploy via SD card swap, or via a tiny tftp bootloader over mini-UART.

## 11. Development environment

Real Pi Zero 2 W hardware is the deployment target, not the day-to-day dev target. The Zero 2 W is inconvenient to iterate on (no HDMI, no Ethernet, fiddly power, small form factor). Use progressively closer-to-real environments as each layer stabilises.

### 11.1 Primary: QEMU `-M raspi3b`

The Zero 2 W's BCM2710A1 is a repackaged BCM2837 — same Cortex-A53 quad-core, same memory map, same peripherals as the Pi 3B. QEMU's `raspi3b` machine is the closest off-the-shelf emulator.

- Launch: `qemu-system-aarch64 -M raspi3b -cpu cortex-a53 -smp 4 -m 1G -kernel kernel8.img -serial stdio -s -S`.
- Exposes EL3/EL2/EL1. GIC, ARM generic timer, mini-UART/PL011, SD controller all modelled.
- `-s -S` provides a gdb stub on `:1234`; connect with `gdb-multiarch` for single-step and stage-2 fault inspection.
- `-d int,mmu,cpu_reset,guest_errors` for early trap plumbing.
- Instant iteration — no SD swaps, no serial wiring.

Gaps in QEMU's raspi3b: VideoCore mailbox/framebuffer is partial, USB (`dwc_otg`) is quirky, I2S/PWM audio is effectively absent. Sufficient for M1–M3; insufficient for M4+.

### 11.2 Secondary: real Pi 3B or Pi 3B+

Same SoC as the Zero 2 W; binaries run unchanged. HDMI, Ethernet, and full-size USB make driver bring-up dramatically less painful than on the Zero 2 W itself. Minimum kit: Pi 3B, USB-TTL serial cable on GPIO 14/15, SD card, HDMI monitor.

Used from M4 onwards. The Zero 2 W itself is touched only at M6–M7 for final validation.

### 11.3 Tertiary (M1 only): QEMU `-M virt`

If the Pi boot handoff is fighting us while writing the first EL2 bytes, `-M virt -cpu cortex-a53 -machine virtualization=on` gives a clean, well-documented GICv3 + PL011 + virtio platform at EL2 with no firmware quirks. Useful for isolating "is my EL2 init correct?" from "is my Pi boot protocol correct?" Abandon once raspi3b works.

### 11.4 Things to know up front

- **EL2 entry state on real Pi.** The Pi firmware's handoff to `kernel8.img` in 64-bit mode lands at EL2 by default; 32-bit lands at HYP under the right `config.txt`. QEMU `raspi3b` behaves similarly but not identically. Budget a small boot shim to converge the two.
- **Park cores 1–3 in WFE** at first. SMP is out of scope; the Newton doesn't know about it anyway.
- **`-cpu cortex-a53` specifically** — not `max`, not `cortex-a72`. Otherwise system-register availability and errata diverge from the real Pi.
- **Use recent QEMU (8.x+).** Older versions had raspi3b EL2 quirks and incomplete `HCR_EL2` trap coverage.

### 11.5 Dev loop

1. Build image as `kernel8.img`.
2. `qemu-system-aarch64 -M raspi3b -kernel kernel8.img -serial stdio -s -S &`
3. `gdb-multiarch kernel8.elf` → `target remote :1234` → breakpoints in EL2 trap handlers.
4. Once a milestone passes QEMU, flash the same image to an SD and boot a real Pi 3B. Discrepancies reveal where QEMU's model is lying.
5. Pi Zero 2 W involved only for M6–M7 validation.

### 11.6 Non-options

- **Hypervisor.framework on Apple Silicon.** Runs AArch32 guests fast, but it isn't a Pi — no BCM peripherals, no mailbox, no Pi boot protocol. Not worth the detour.
- **Unicorn / Renode.** Unicorn is CPU-only. Renode supports Pi models but is less battle-tested than QEMU for this specific workload.

## 12. Phasing

| Milestone | Exit criterion |
|---|---|
| **M1 — "Hello, EL2."** | Bare-metal Pi image, UART console, EL2 entry, stage-2 identity map, return to a trivial EL1 AArch32 payload that prints via HVC. |
| **M2 — Guest ROM fetch.** | Load ROM/flash to guest physical; jump guest to `0x00000000`; observe first MMIO fault and log `ESR_EL2` / `HPFAR_EL2`. |
| **M3 — Interrupt controller + timer.** | `TInterruptManager` wired through EL2 traps; first vIRQ delivered; scheduler ticks fire. |
| **M4 — DMA, flash, screen.** | Boot progresses to the Notes screen. |
| **M5 — Pen input.** | USB or UART-tunneled touch events into `TScreenManager`; user interaction works. |
| **M6 — Audio, serial, PCMCIA images.** | Feature-complete stock Newton. |
| **M7 — Performance and polish.** | Measurement vs real 162 MHz StrongARM. |

## 13. Risks, ranked

1. **Unknown ARMv4 quirks the Newton ROM depends on.** Mitigation: trap-and-emulate; Einstein's implementation as behavioral ground truth.
2. **USB stack effort.** Real work. Mitigation: PS/2 or serial input for v1.
3. **CP15 shim completeness.** Can only be enumerated empirically. Mitigation: instrument Einstein to collect the full set before starting (§16.4).
4. **Physical aliases and mirrors.** `TMMU.cpp` dump shows flash/ROM mirrors at `0x30000000`, `0x34000000`, `0x90000000`, `0xAC000000`. Need stage-2 entries for each, or trap-and-remap (§16.8).
5. **Thermal / power on Pi Zero 2 W.** Minor; A53 at 1 GHz under an emulator-sized workload is well within thermal envelope.

## 14. Success criteria

Newton OS 2.1 (717006 or equivalent) boots to the Notes app on a Pi Zero 2 W with no Linux underneath, accepts pen input, persists to flash across reboot, and sustains at least real-StrongARM performance.

## 15. Explicitly not in scope

JIT, recompilation, any software CPU emulation, Einstein's UI layer, Linux dependencies, multi-ROM switcher at runtime, cross-platform portability, Pi 4/5 support.

## 16. Open questions

All of these want verification against the actual ROM or hardware rather than memory or inference. The ones that gate the whole design are §16.1, §16.2, and §16.4.

1. **EL2 availability at boot on Pi Zero 2 W.** Cortex-A53 has EL2. Does the Pi Zero 2 W firmware hand control to `kernel.img` at EL2, or has it already dropped to EL1? Needs RPi Foundation docs plus boot-time experiment.
2. **Descriptor formats used by 2.x ROMs.** The `TMMU.cpp:1141+` dump suggests only sections / 64 KiB / 4 KiB descriptors are used (no fine tables, no tiny pages). Verify by instrumenting `TMMU::TranslateV` to log descriptor types on every walk, boot every 2.x ROM variant we care about (717006, 737041, localised variants, MP2100 US), and run representative workloads. Any tiny-page hit invalidates the hardware-walk plan for that region.
3. **Privilege levels of ROM regions.** Instrument `mMode` transitions in `TARMProcessor`; correlate with PC ranges. Needed to justify "kernel-only-PL1" and to decide how aggressively to lean on AP enforcement.
4. **Complete CP15 op set emitted by the kernel.** Instrument Einstein's CP15 dispatch; log unique `(opcode1, CRn, CRm, opcode2, direction)` tuples over a representative boot and workload. This set defines the CP15 shim's surface area.
5. **SWP / SWPB frequency and call sites.** Count in JIT dispatch. If more than a few dozen per second in steady state, patch the ROM.
6. **Domain usage.** Dump DACR transitions and per-descriptor domain tags; confirm manager/client/no-access usage is conventional and has no StrongARM-specific side-effect dependency.
7. **Cache-line op encodings.** Enumerate exact CP15 c7 ops the ROM issues; map each to an A53 equivalent or a documented no-op.
8. **Physical aliases and mirrors.** Enumerate every distinct guest-physical region the ROM actually touches; confirm stage-2 coverage.
9. **RAM-size assumptions.** Does 2.x handle arbitrary RAM sizes via the `kHdWr_04RAMSize` register, or are there hard-coded assumptions somewhere? `TMemory.cpp:868–876` suggests the register is honored; verify for each ROM.
10. **PCMCIA and modem runtime assumptions.** Does 2.x require a card present at boot? How is modem absence tolerated?
11. **Display geometry and depth.** Newton expects specific framebuffer dimensions; Pi framebuffer is configurable. Confirm mapping.
12. **Self-modifying ROM code.** If any exists, stage-2 write-protect-and-invalidate becomes relevant. If not, simpler.
13. **Licensing.** Einstein is GPLv2. Reusing peripheral classes imposes GPLv2 on the hypervisor. Confirm intent.
14. **Input device for v1.** USB touchscreen, UART-tunneled pen, or PS/2 keyboard + mouse-as-pen?
15. **Minimum viable v1.** Pick the smallest ROM + flash + screen + pen configuration that proves the architecture end-to-end.
