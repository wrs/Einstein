# Newton 2.x on Bare-Metal Pi Zero 2 W — High-Level Design

**Target host:** Raspberry Pi Zero 2 W (BCM2710A1, Cortex-A53 ×4)
**Guest:** Newton OS 2.x ROMs (unmodified)
**Relationship to Einstein:** ports Einstein's peripheral emulation state machines (re-implemented in Rust, register-level behaviour preserved); replaces Einstein's software MMU, JIT, and host-OS layer. The C++ link route was tried and abandoned — see IMPLEMENTATION.md §1.2.

## 1. Goal

Boot an unmodified Newton 2.x ROM on a bare-metal Pi Zero 2 W such that the guest CPU instructions execute natively on the A53 under a small Type-1 hypervisor running at EL2. Port Einstein's peripheral emulation (`TDMAManager`, `TInterruptManager`, `TSerialChip*`, `TScreenManager`, `TSoundManager`, `TFlash`, `TPCMCIAController`) to Rust — register-level behaviour preserved — invoked from EL2 trap handlers. Replace Einstein's software MMU and JIT entirely.

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
  |   - ported Einstein peripheral managers (Rust)         |
  |   - bare-metal Pi drivers                              |
  +--------------------------------------------------------+
```

\* Kernel-only-in-PL1 **confirmed empirically** against 717006: 19 310 USR entries vs 649 SVC entries over 90 s of boot; `SVC → USR` is the dominant transition (see [`probe/FINDINGS.md`](probe/FINDINGS.md) §16.3).

### 3.1 Components

| Layer | Status | Source |
|---|---|---|
| EL2 init, stage-2 MMU, trap vectors | new | — |
| Page-table seeding (guest physical layout) | new | `TMemoryConsts`, `TFlash` image format as reference |
| Trap decoder (`ESR_EL2` / `HPFAR_EL2` → handler) | new | — |
| Peripheral emulation | ported to Rust | `TDMAManager`, `TInterruptManager`, `TSerialPortManager` + `TSerialChip*`, `TSoundManager`, `TScreenManager`, `TFlash`, `TPCMCIAController`, `TNetworkManager` (register-level behaviour preserved; C++ link route abandoned — IMPLEMENTATION.md §1.2) |
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

The Newton's own page tables. Hardware walks them in place — there is no
parallel shadow page-table tree and no per-walk rewrite. Domains and
cacheability attributes pass through unchanged. Two narrow EL2-side
normalisations are required because the ROM's tables use ARMv4
short-descriptor bit assignments that ARMv7/v8 re-interpret, and a small
per-PC stub facility is needed for instructions ARMv8 made UNDEFINED.

**Stage-1 normalisation pass** (`fix_stage1_xn_bits` in `guest_mem.rs`,
run on every guest TTBR0 install — `MCR p15,0,Rn,c2,c0,0`, trapped via
`HCR_EL2.TVM`). It walks the guest's live L1 table and every coarse L2
table it reaches, editing the descriptors *in place* in the ROM/RAM
backing the hypervisor owns:

- **Subpage-AP flattening.** ARMv4 small/large-page descriptors carry
  four 2-bit AP subfields; ARMv7 short descriptors reinterpret those bits
  as AP[2]/TEX/S/nG/XN. Each page entry is rewritten to a single uniform
  `AP[1:0] = 0b11` (RW from any mode), `C = B = 1`, `XN = 0`. This is the
  AP flattening — it exists and is load-bearing. The kernel's actual
  USR-vs-PL1 protection is still enforced, via the kernel's own DACR +
  L1-domain assignment, not via the discarded subpage bits.
- **XN clearing.** ARMv4 treats L2 bit 15 as SBZ; ARMv7/v8 read it as XN.
  Many of the ROM's prebuilt L2 entries have bit 15 set, which would make
  the corresponding code pages non-executable and abort every fetch, so
  the pass clears XN on page entries.
- **Fine-table rewrite.** 717006 installs three L1 fine-table descriptors
  (type `0b11`) covering VAs `0x78000000` / `0x90000000` / `0xAC000000`
  as PCMCIA-window placeholders; all their L2 entries are fault, and A53
  short-descriptor doesn't walk `0b11` L1 descriptors. They are rewritten
  to L1 fault (`0b00`), so any access raises a translation fault the abort
  handler dispatches — semantics-preserving because nothing is mapped
  through them.

**Per-PC inline stubs** (`shadow_stub.rs`). ARMv8 made `SWP`/`SWPB` and the
StrongARM FPA-class coprocessor ops UNDEFINED. Rather than trap-and-emulate
every occurrence, the hypervisor installs short AArch32 stubs in a reserved
window of the ROM aperture (`0x00E0_0000..0x00FF_FF00`) and rewrites the
originating PC to `B stub`. The same module provides an APCS-conformant
liveness walker (`live_regs_at`) so a stub can borrow dead caller-saved
registers as scratch. "Real code" for this walker (and for the BE-8
code/data discrimination, §6.2 / `guest_endian.rs`) is defined by the
classifier reach-bitmap baked in at build time from `tools/classify-rom`.

**Domains.** DACR is always `0x00055555` (domains 0–7 = client, 8–15 = no-access), written 38 953 times with the same value — the kernel reinstalls DACR at every context-switch. A53 short-descriptor DACR semantics match; just pass the writes through.

## 6. CPU and mode handling

Guest runs natively at EL1 AArch32. No JIT, no interpreter. Newton's SVC/IRQ/FIQ/ABT/UND vectors are entered by the hardware exactly as on StrongARM. Banked registers, SPSR, CPSR handled by the CPU.

### 6.1 ARMv4-vs-ARMv8-AArch32 deltas needing trap-and-emulate at EL2

Probe runs against 717006 narrowed the actual scope considerably; see [`probe/FINDINGS.md`](probe/FINDINGS.md) for raw counts.

- **CP15.** Total surface is **15 `(opc1, CRn, CRm, opc2, dir)` tuples** across a 90 s boot. Trap via `HCR_EL2.TVM` / `TRVM` / `TID*`. The shim's handler table has 15 entries:
  - ID read (`c0,c0,0`), SCTLR (`c1,c1,0`), TTBR (`c2,c2,0`), DACR (`c3,c3,0`), FSR (`c5,c5,0`), FAR (`c6,c6,0`) — direct AArch32 equivalents.
  - Cache maintenance (`c7` family, five encodings) — map to `DCCMVAC` / `DCCIMVAC` / `DCISW` / `DSB SY`, or no-op if we disable guest cache.
  - TLB flush (`c8` family, three encodings) — map to `TLBIALL`, `TLBIMVA` variants.
  - **StrongARM-specific `c15 op1=0 CRm=1 op2=2`** (clock control) — fires **exactly once** at boot. Trap-and-no-op.
- **SWP / SWPB.** UNDEFINED in ARMv8. 717006 has **one** call site (`0x003AE200`) firing ~400 k times in 90 s — almost certainly the kernel's atomic-exchange primitive. **Patch that single ROM site** with an `LDREX`/`STREX` sequence at boot; no traps needed.
- **`MRS Rd, SPSR` while in User mode.** `Emulator/TARMProcessor.cpp:760` notes StrongARM returned CPSR here; A53 treats it as UNPREDICTABLE. Trap in undef vector, return CPSR.
- **Imprecise data aborts and other ARMv4 edge cases.** Enumerate empirically; maintain a fixup table.

### 6.2 Thumb

Newton doesn't use it. `HSCTLR.TE = 0`.

## 7. Interrupts

- Pi peripherals raise IRQs at the BCM2710 interrupt controller. Route to EL2 via `HCR_EL2.IMO` / `FMO`.
- EL2 ISRs drive bare-metal drivers (timer tick, UART RX, SD, USB).
- Peripheral managers decide when the guest should see a virtual Newton interrupt. EL2 updates the Newton VIC shadow state (`TInterruptManager`) and raises `VI` / `VF` to the guest via `HCR_EL2`. The A53 vectors to the guest's IRQ/FIQ handlers.
- Timer: use the A53 generic timer at EL2 for host ticks; synthesize Newton's 3.6864 MHz tick and match registers (`TMemoryConsts.h:85–92`) from it.

## 8. Peripherals — guest side (ported from Einstein)

Port Einstein's classes to Rust, preserving register-level behaviour; the "register read/write" entry points are called from EL2 trap handlers instead of from a software MMU. The MMIO-window peripherals (vic, dma, pcmcia, serial, flash, screen) share an `MmioPeripheral { owns, read, write, peek_word }` contract; the native-primitive peripherals share a `NativeDriver { DRIVER_ID, handle(ctx, subfn, pc) }` contract (`src/peripherals/`). The C++-link alternative was tried and abandoned (IMPLEMENTATION.md §1.2).

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

**Deployment target: Raspberry Pi Zero 2 W. No intermediate Pi model.** The
Zero 2 W's BCM2710A1 is a repackaged BCM2837 (same Cortex-A53 quad-core,
same memory map, same peripherals as a Pi 3B), so the day-to-day dev loop
is QEMU `raspi3b` + ARM FVP for emulation, and the Zero 2 W directly for
real-silicon validation. The Pi 3B is not a stepping stone — it has the
same SoC, runs the same `kernel8.img`, and its only practical advantage
(full-size HDMI/USB connectors, easier bench wiring) doesn't move the
needle on bring-up effort. The Zero 2 W has mini-HDMI, micro-USB OTG, and
its own GPIO header; that's everything bring-up needs. Skip Pi 3B unless
the form factor is genuinely getting in the way of a specific debugging
session.

See `docs/REAL_HW_BRINGUP.md` for the concrete plan to go from QEMU/FVP-
green to Zero-2-W-green.

### 11.1 Primary dev target: QEMU `-M raspi3b`

QEMU's `raspi3b` machine is the closest off-the-shelf emulator for the
BCM2837/BCM2710A1 SoC.

- Launch: `qemu-system-aarch64 -M raspi3b -cpu cortex-a53 -smp 4 -m 1G -kernel kernel8.img -serial stdio -s -S`.
- Exposes EL3/EL2/EL1. Legacy BCM2835 VIC, ARM generic timer, mini-UART/PL011, SD controller all modelled.
- `-s -S` provides a gdb stub on `:1234`; connect with `aarch64-elf-gdb` for single-step and stage-2 fault inspection.
- `-d int,mmu,cpu_reset,guest_errors` for early trap plumbing.
- Instant iteration — no SD swaps, no serial wiring.

Gaps in QEMU's raspi3b: AArch64↔AArch32 banked-register plumbing is flaky
(see `docs/QEMU_BUGS.md`), VideoCore mailbox/framebuffer is partial, USB
(`dwc_otg`) is quirky, I2S/PWM audio is effectively absent. Sufficient
for M1–M3; insufficient for M4+ peripheral bring-up.

### 11.2 Co-primary dev target: ARM FVP `FVP_Base_RevC-2xAEMvA`

The accurate reference model. GICv3 + generic timer + cache model are all
modelled correctly; the AArch64↔AArch32 boundary works without QEMU's
quirks. Used to cross-check anything that smells like a QEMU bug. Slower
than QEMU TCG because the timer/cache model is accurate.

- Build: `cargo build --release --no-default-features --features "platform-fvp-base quiet"`.
- Launch: `scripts/fvp --timeout=90 <elf>` (wraps a dockerised FVP).
- `--gdb` for Iris debug server on port 7100; `--features trace` for the function-level tracer.

Both QEMU and FVP must stay green: `guest-tests/scripts/run-all.sh` runs
the guest tests on both. Any new divergence is tracked down rather than
papered over with a feature gate.

### 11.3 Real-silicon target: Pi Zero 2 W

The deployment target itself. Touched only after a milestone has passed
on both QEMU and FVP. Bring-up sequencing and the implementation gaps
between "works on emulators" and "works on the Zero" live in
`docs/REAL_HW_BRINGUP.md`.

### 11.4 Things to know up front

- **EL2 entry state on real Pi.** The Pi firmware's handoff to `kernel8.img` in 64-bit mode lands at EL2 by default; 32-bit lands at HYP under the right `config.txt`. QEMU `raspi3b` behaves similarly but not identically. Budget a small boot shim to converge the two. (§16.1.)
- **Park cores 1–3 in WFE** at first. SMP is out of scope; the Newton doesn't know about it anyway.
- **`-cpu cortex-a53` specifically** — not `max`, not `cortex-a72`. Otherwise system-register availability and errata diverge from the real Pi.
- **Use recent QEMU (8.x+).** Older versions had raspi3b EL2 quirks and incomplete `HCR_EL2` trap coverage.

### 11.5 Dev loop

1. Build image: `cargo build --release` (raspi3b) or `--no-default-features --features platform-fvp-base` (FVP).
2. Run on QEMU (`cargo run --release`) or FVP (`scripts/fvp …`) for fast iteration.
3. For a debug session: `DEBUG=1 cargo run --release`, then `aarch64-elf-gdb -x scripts/gdb-init …`.
4. `guest-tests/scripts/run-all.sh [--platform fvp]` before any commit that touches hypervisor functionality.
5. Real-silicon validation on the Zero 2 W is its own phased workstream — see `docs/REAL_HW_BRINGUP.md`.

### 11.6 Non-options

- **Hypervisor.framework on Apple Silicon.** Runs AArch32 guests fast, but it isn't a Pi — no BCM peripherals, no mailbox, no Pi boot protocol. Not worth the detour.
- **Unicorn / Renode.** Unicorn is CPU-only. Renode supports Pi models but is less battle-tested than QEMU/FVP for this specific workload.
- **QEMU `-M virt`.** Was considered as an M1-only fallback for "is my EL2 init correct?" isolation. FVP fills that role better and is now in the loop full-time.

## 12. Phasing

| Milestone | Exit criterion | Status |
|---|---|---|
| **M1 — "Hello, EL2."** | Bare-metal Pi image, UART console, EL2 entry, stage-2 identity map, return to a trivial EL1 AArch32 payload that prints via HVC. | **done** |
| **M2 — Guest ROM fetch.** | Load ROM/flash to guest physical; jump guest to `0x00000000`; observe first MMIO fault and log `ESR_EL2` / `HPFAR_EL2`. | **done** |
| **M3 — Interrupt controller + timer.** | `TInterruptManager` wired through EL2 traps; first vIRQ delivered; scheduler ticks fire. | **done** |
| **M4 — DMA, flash, screen.** | Boot progresses to the Notes screen. | **done** |
| **M5 — Pen input.** | USB or UART-tunneled touch events into `TScreenManager`; user interaction works. | **done (2026-05-12, real hw)** |
| **M6 — Audio, serial, PCMCIA images.** | Feature-complete stock Newton. | **audio done; serial + PCMCIA open** |
| **M7 — Performance and polish.** | Measurement vs real 162 MHz StrongARM. | not started |

M1–M5 are validated end-to-end on real hardware (Pi Zero 2 W) as well
as QEMU/FVP — see `docs/REAL_HW_BRINGUP.md`. Beyond M7, the known
functional gap not captured by this table is **add-on app packages**
(the `.pkg` installation flow); the stock ROM and builtin apps run
without it.

## 13. Risks, ranked

All retired by the working v1; kept as design rationale.

1. **Unknown ARMv4 quirks the Newton ROM depends on.** Mitigation: trap-and-emulate; Einstein's implementation as behavioral ground truth.
2. **USB stack effort.** Real work. Mitigation: PS/2 or serial input for v1. (In the end the USB host stack was built — minimal, single-device, no hub.)
3. **CP15 shim completeness.** Can only be enumerated empirically. Mitigation: instrument Einstein to collect the full set before starting (§16.4).
4. **Physical aliases and mirrors.** `TMMU.cpp` dump shows flash/ROM mirrors at `0x30000000`, `0x34000000`, `0x90000000`, `0xAC000000`. Need stage-2 entries for each, or trap-and-remap (§16.8).
5. **Thermal / power on Pi Zero 2 W.** Minor; A53 at 1 GHz under an emulator-sized workload is well within thermal envelope.

## 14. Success criteria

Newton OS 2.1 (717006 or equivalent) boots to the Notes app on a Pi Zero 2 W with no Linux underneath, accepts pen input, persists to flash across reboot, and sustains at least real-StrongARM performance.

## 15. Explicitly not in scope

JIT, recompilation, any software CPU emulation, Einstein's UI layer, Linux dependencies, multi-ROM switcher at runtime, cross-platform portability, Pi 4/5 support.

## 16. Open questions

All of these wanted verification against the actual ROM or hardware rather than memory or inference. **Every design-level question is now closed** — §16.2–§16.7 by the first probe pass (see [`probe/FINDINGS.md`](probe/FINDINGS.md)), §16.1 on 2026-05-11 when `pi-probe` booted on Walter's Zero 2 W and printed `CurrentEL = 2`, and the rest empirically by the full boot on real hardware (2026-05-12). Per-item status is noted inline; §16.13 (licensing) is the only one that remains a decision rather than a finding.

1. **EL2 availability at boot on Pi Zero 2 W.** *Answered (2026-05-11).* `pi-probe` (a standalone first-light probe binary, used only for this bring-up step) ran on real hardware and reported `CurrentEL = 2`, `MIDR_EL1 = 0x410fd034` (Cortex-A53 r0p4). Matches the QEMU `raspi3b` run byte-for-byte. The default Pi firmware path (`arm_64bit=1`, no `kernel_old`, no custom `armstub=`) loads `armstub8.S` from `raspberrypi/tools`, which eret's to EL2h before branching to `kernel8.img` at `0x80000`. PL011 routing to GPIO 14/15 requires `dtoverlay=disable-bt` in `config.txt`; otherwise the header carries the mini-UART. See `docs/REAL_HW_BRINGUP.md`.
2. **Descriptor formats used by 2.x ROMs.** *Partially answered for 717006 — see [`probe/FINDINGS.md`](probe/FINDINGS.md).* Only sections, 64 KiB large pages, and 4 KiB small pages are actively mapped. No tiny pages. Three L1 slots (at VA 0x78000000, 0x90000000, 0xAC000000) hold fine-table descriptors but their L2 entries are all fault — placeholder reservations for PCMCIA card windows. Fine tables don't walk on A53 short descriptor, but since nothing is actually mapped through them, a straightforward hypervisor-side rewrite (L1 0b11 → 0b00) at guest TTBR-install time preserves semantics. Still needs verification against 737041, localised variants, and eMate ROMs.
3. **Privilege levels of ROM regions.** *Answered — see [`probe/FINDINGS.md`](probe/FINDINGS.md).* 19 310 USR entries vs 649 SVC entries over 90 s of boot; kernel-only-PL1 confirmed. `SVC → USR` is the dominant edge. AP enforcement is the operative protection model; preserve it.
4. **Complete CP15 op set emitted by the kernel.** *Answered.* 15 unique `(opc1, CRn, CRm, opc2, dir)` tuples. All standard ARMv4 except one StrongARM-specific clock-control op that fires exactly once at boot. Hot path is cache maintenance; each op has a direct AArch32-on-A53 equivalent.
5. **SWP / SWPB frequency and call sites.** *Answered.* 405 810 SWPs from **one** PC (`0x003AE200`), zero SWPB. Single ROM patch at that site replaces the entire SWP surface with `LDREX`/`STREX`. Trap-and-emulate also viable at ~4.5 k/s peak.
6. **Domain usage.** *Answered.* DACR is written 38 953 times with the same value `0x00055555` — eight client domains, eight no-access domains, no manager domains, no StrongARM-specific side effects. A53 short-descriptor DACR semantics match exactly.
7. **Cache-line op encodings.** *Answered.* Six distinct c7 ops, all standard ARMv4, all trivially mappable to AArch32-on-A53 (`DCCMVAC`, `DCCIMVAC`, `DSB SY`, etc.) or safely no-oppable if we pass through A53 coherency.
8. **Physical aliases and mirrors.** *Answered empirically.* The stage-2 map covers every region the 717006 ROM touches through a full interactive boot; unknown IPAs halt loudly and none fire.
9. **RAM-size assumptions.** *Answered for 717006.* The `kHdWr_04RAMSize` path is honored with the configuration we present; full boot + builtin apps run. Other ROM variants unverified.
10. **PCMCIA and modem runtime assumptions.** *Answered.* 2.x boots and runs with no card present and no modem; the PCMCIA peripheral surface reports empty slots.
11. **Display geometry and depth.** *Answered.* Newton's 320×480 2 bpp framebuffer is hypervisor-side scaled (1.5× → 480×720, centred on a 1280×720 HDMI mode) on real hw; 1:1 in the host viewer on QEMU/FVP.
12. **Self-modifying ROM code.** *Answered.* The ROM itself is not self-modifying, but the kernel demand-pages code into RAM and rewrites it; handled by stage-2 RO+X ↔ RW+XN flipping with rescan-on-fetch (`src/stage2.rs`, `src/shadow_stub.rs`).
13. **Licensing.** *Still open (decision, not finding).* The peripheral layer ports Einstein (GPLv2) state machines; confirm intended license for the hypervisor before any public release.
14. **Input device for v1.** *Answered.* USB touchscreen (TSTP MTouch, `docs/MTOUCH.md`) on real hw; mouse-as-pen via the host viewer on QEMU/FVP.
15. **Minimum viable v1.** *Achieved.* Pi Zero 2 W + 717006 ROM + HDMI panel with speakers + USB touch + SD card proves the architecture end-to-end.
