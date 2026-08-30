# Newton Hypervisor — Architecture

**Guest:** Newton OS 2.x ROM (717006), unmodified, running natively as
AArch32 code at EL1.
**Hosts:** QEMU `raspi3b`, ARM FVP `FVP_Base_RevC-2xAEMvA`, and a real
Raspberry Pi Zero 2 W (BCM2710A1, Cortex-A53 ×4).
**Relationship to Einstein:** Einstein's peripheral state machines are
re-implemented in Rust with register-level behaviour preserved.
Einstein's software MMU, JIT, and host-OS layer have no counterpart
here — the guest's instructions execute on the A53 and its own page
tables are walked by the hardware.

The user-facing build/run guide is [`README.md`](README.md); language,
build-system and testing decisions are in
[`IMPLEMENTATION.md`](IMPLEMENTATION.md); current state and remaining
work are in [`PLAN.md`](PLAN.md).

## 1. Scope

In scope: booting an unmodified Newton 2.x ROM on Cortex-A53 under a
Type-1 hypervisor at EL2, with Newton's peripherals modelled in EL2
trap handlers and host I/O (display, pen, audio, storage) provided by
bare-metal drivers.

Out of scope: Newton 1.x ROMs, Einstein's UI layer, any software CPU
emulation or JIT, a host IP stack, running under Linux, multi-ROM
switching at runtime, and Pi 4/5 support.

## 2. Structure

```
  +--------------------------------------------------------+
  | EL0 (PL0): NewtonScript tasks, apps, most ROM code      |
  | EL1 (PL1): Newton kernel, SWI/IRQ/FIQ/ABT/UND handlers  |
  |   -- guest stage-1 MMU walks Newton's own tables --     |
  +--------------+-----------------------------------------+
                 | stage-2 faults, HVC, CP15 traps, undef
  +--------------v-----------------------------------------+
  | EL2: Newton Hypervisor                                  |
  |   - world setup, stage-2 mapping                        |
  |   - trap dispatch: MMIO, CP15, UND, alignment, HVC      |
  |   - vIRQ/vFIQ injection                                 |
  |   - Newton peripheral models (Rust)                     |
  |   - host drivers + backends (UART, FB, SD, USB, audio)  |
  +--------------------------------------------------------+
```

The guest spends nearly all its time in USR mode: 19 310 USR entries
vs 649 SVC entries over a 90 s boot measured against 717006
([`probe/FINDINGS.md`](probe/FINDINGS.md)). Page-table protection, not
mode-based trapping, is what separates user code from the kernel, and
it stays in force (through the kernel's own domains — see §4.3).

Source is one crate in six layer directories with an enforced
dependency direction (`arch ← hv ← newton`, plus `peripherals`, `host`
and `diag`); see [`IMPLEMENTATION.md`](IMPLEMENTATION.md) §3 and
`scripts/check-layering.sh`.

## 3. Boot flow

1. The platform's firmware (or QEMU/FVP) enters the image at EL2:
   `0x80000` on raspi3b, `0x80000000` on FVP.
2. `boot.s` sets up stacks and BSS on core 0 and parks cores 1–3;
   `kmain` (`src/main.rs`) runs the boot narrative below.
3. EL2 stage-1 MMU and caches on (`arch::mmu`), console up
   (PL011 or semihosting), EL2 vector table installed.
4. Region backings registered with the layout manifest
   (`hv::layout::register_backing`), the host-backend seams wired into
   the guest models, ROM + REx loaded and patched (`newton::loader`),
   flash seeded and its persistent image loaded.
5. Stage-2 tables built from the manifest (`hv::stage2::init`), then
   the peripheral models and remaining host backends
   (`peripherals::vic`, `hv::timer`, `host::host_io`, `host::input`).
6. `HCR_EL2` programmed (§6), `SPSR_EL2` set for AArch32 SVC,
   `ELR_EL2 = 0`, `ERET`.
7. The ROM boots as if on Newton hardware. Peripheral accesses fault to
   EL2; EL2 dispatches to the Rust peripheral models.

## 4. Memory model

### 4.1 Guest-physical regions

One manifest — `hv::layout::REGIONS` — is the single source of truth
for stage-2 mapping, EL2-side IPA→host-pointer resolution, and
snapshot serialization. A boot-time `cross_check` makes "mapped at
stage-2 but missing from the other two" a loud halt.

| IPA | Size | Contents | Stage-2 |
|---|---|---|---|
| `0x00000000` | 16 MiB | ROM + REx aperture (incl. hypervisor-written stub/trampoline windows in the tail) | RO |
| `0x02000000` | 4 MiB | Flash bank 0 (internal store) | RO, writes absorbed |
| `0x04000000` | 4 MiB | RAM | RW, 4 KiB pages |
| `0x06000000` | 384 KiB | Inline-stub scratch pool (identity-mapped IPA == VA) | RW, 4 KiB pages |
| `0x0E000000` | 2 MiB | Framebuffer | RW |
| `0x10000000` | 4 MiB | Flash bank 1 | RO, writes absorbed |

Flash content is mutated only through the flash native primitives, so
the banks are stage-2 read-only and stray writes are absorbed in the
DABT path. Flash is persisted through `host::flash_persist`, not
through the snapshot.

There is deliberately no RAM mirror at IPA `0x0C000000`: on real
Newton hardware that range is a stage-1 remap onto discrete 4 KiB
pages in `0x04xxxxxx`, so a blanket alias would make pre-MMU writes
and post-MMU reads land in different host cells.

### 4.2 MMIO windows

`hv::layout::MMIO_WINDOWS` lists the trap-handled IPA windows,
walked first-match-wins by the `hv::mmio` router. Each window carries
a policy: route to a peripheral model, read-zero/drop-write, or halt
loudly on anything unmodelled.

- `0x0F000000..0x0F400000` — the Newton hardware window: VIC/RTC/GPIO,
  DMA, serial, BIO register banks, and the ASIC/memory-controller
  clusters. Unknown accesses inside it halt with a context dump naming
  the register to add.
- `0x30000000..0x70000000` — PCMCIA (modelled as "no card").
- `0x08000000..0x09000000` — the absent second RAM bank, so BootOS's
  signature probe cleanly concludes 4 MiB.
- `0x10400000..0x20000000` and `0x20000000..0x30000000` —
  absent-REx/flash probe space and Einstein's silent-zero "unknown
  bank #5", both read-zero/drop-write.

One exception to trap-on-touch: the 4 KiB tick page at `0x0F181000`
(calendar / alarm / `K_HDWR_TICKS`) is backed read-only at stage-2 so
the kernel's hot tick reads don't trap; writes still fault into the
VIC model.

### 4.3 Guest stage-1

The Newton's own page tables are walked by the hardware in place.
There is no shadow page-table tree and no per-walk rewrite. Domains
and cacheability pass through unchanged (DACR is always `0x00055555` —
eight client domains, eight no-access, rewritten at every context
switch; A53 short-descriptor DACR semantics match exactly).

Two EL2-side normalisations are needed because the ROM's tables use
ARMv4 short-descriptor bit assignments that ARMv7/v8 reinterpret.
`fix_stage1_xn_bits` (`src/newton/os.rs`) runs on every guest TTBR0
install (`MCR p15,0,Rn,c2,c0,0`, trapped via `HCR_EL2.TVM`) and edits
the guest's live L1 and reachable coarse L2 descriptors in place:

- **Subpage-AP flattening.** ARMv4 small/large-page descriptors carry
  four 2-bit AP subfields; ARMv7 reinterprets those bits as
  AP[2]/TEX/S/nG/XN. Each page entry is rewritten to a uniform
  `AP[1:0] = 0b11`, `C = B = 1`, `XN = 0`. USR-vs-PL1 protection is
  still enforced — by the kernel's own DACR + L1 domain assignment,
  not by the discarded subpage bits.
- **XN clearing.** ARMv4 treats L2 bit 15 as SBZ; ARMv7/v8 read it as
  XN. Many prebuilt ROM L2 entries have it set, which would abort
  every fetch from those pages.
- **Fine-table rewrite.** 717006 installs three L1 fine-table
  descriptors (type `0b11`) at VAs `0x78000000` / `0x90000000` /
  `0xAC000000` as PCMCIA-window placeholders; all their L2 entries are
  fault, and the A53 short-descriptor walker does not walk `0b11` L1
  descriptors. They are rewritten to L1 fault (`0b00`), which is
  semantics-preserving because nothing is mapped through them.

RAM pages additionally carry a stage-2 RW+XN ↔ RO+X state machine
(`src/hv/stage2.rs`): the kernel demand-pages code into RAM and
rewrites it, so a page flips to executable on first fetch and back to
writable on first write, with a code rescan in between.

### 4.4 MMU-off windows and `HCR_EL2.DC`

The kernel performs every "physical" memory access — above all its
page-table reads and writes — by turning its stage-1 MMU off and back
on around the access (ROM routines `LoadFromPhysAddress` `0x18CA4`,
`StoreToPhysAddress` `0x18CE0`, and the byte variants `0x18D1C` /
`0x18D58`); once the store is busy there are thousands of these
windows per second. Both `SCTLR.M` writes trap via `HCR_EL2.TVM` and
reach the `on_stage1_mmu_disable` / `on_stage1_mmu_enable` hooks
(`src/newton/os.rs`), which call `hv::guest::set_dc_for_stage1_off`:

- **Falling edge (M=1→0):** set `HCR_EL2.DC`, so the MMU-off data
  accesses are Normal-WB cacheable — coherent with the hypervisor's
  own view of DRAM and with the stage-1 walker's WB attributes —
  instead of Non-cacheable/Device.
- **Rising edge (M=0→1):** clear DC — with DC=1, `SCTLR_EL1.M` behaves
  as 0 from EL2's side (DDI 0487 D13.2.50), which would break every
  non-identity guest mapping — then re-run the descriptor
  normalisation of §4.3 (`fix_stage1_xn_bits` + the scratch-pool L1
  section), since the kernel may have rewritten descriptors during
  the window.

**Every DC change is followed by `TLBI VMALLE1; DSB ISH; ISB`
(`hv::guest::set_dc_for_stage1_off`). Do not "optimise away" that
TLBI.** Per DDI 0487, `HCR_EL2.DC` "is permitted to be cached in a
TLB"; a TLB may cache such control fields "even when any or all of
the stages of translation are disabled"; and "software must perform
TLB maintenance after updating the System registers" when the update
invalidates what a TLB may hold for the current translation context.
Without the invalidation, a stage-1 entry cached under DC=0 can serve
a later MMU-off access: the kernel's "physical" `ldr`/`str` is then
translated through its own VA mapping instead of flat-mapped — a
page-table read returns a data page's contents as a descriptor, and a
page-table write lands in whatever data page the kernel maps at that
address. On the Pi Zero 2 W this was the root cause of months of
intermittent, timing-dependent heap/page-table/store corruption
(`docs/project-history.md` §9). QEMU does not cache DC in a TLB and
never reproduces the failure mode, so a hardware-only symptom in this
area should be checked against this rule first.

## 5. CPU and mode handling

The guest executes natively at EL1 AArch32. No JIT, no interpreter.
Newton's SVC/IRQ/FIQ/ABT/UND vectors are entered by the hardware
exactly as on StrongARM; banked registers, SPSR and CPSR are the CPU's.
Thumb is unused (`HSCTLR.TE = 0`).

The ARMv4/StrongARM-to-ARMv8 deltas the ROM depends on:

- **CP15.** The whole surface 717006 issues is 15
  `(opc1, CRn, CRm, opc2, dir)` tuples, trapped via `HCR_EL2.TVM` /
  `TRVM` / `TIDCP` and handled by the shim in `src/hv/trap/cp15.rs`:
  ID read, SCTLR, TTBR, DACR, FSR, FAR, five `c7` cache-maintenance
  encodings, three `c8` TLB encodings, and one StrongARM `c15`
  clock-control write that fires exactly once at boot (no-op). The
  StrongARM lax encoding (`MCR p15,0,Rn,cN,cN,0`) is rewritten to the
  ARMv7 `CRm=0` form at ROM load. The shim forces `SCTLR_EL1.A = 1`
  (see unaligned access, below) and `SCTLR_EL1.EE = 1`.
- **Unaligned LDR.** The SA-1100 (BE-32, `SCTLR.U = 0`) result for an
  unaligned word load is `word_at(addr & ~3) ROR ((addr & 3) * 8)`;
  ARMv7+ instead loads four contiguous bytes. The 717006 kernel has
  ~1300 sites that depend on rotate semantics, so `SCTLR_EL1.A` is
  forced on and every alignment fault is handled: the patched DABT
  vector routes it to `newton::unaligned`, which decodes and emulates
  the access. Because that round-trip dominates steady-state UI
  rendering, `newton::unaligned_inline` additionally installs a
  per-PC in-ROM stub the first time a given LDR faults, so subsequent
  executions rotate natively without trapping.
- **SWP / SWPB.** UNDEFINED on ARMv8. 717006 issues them from a single
  site (`Swap` at `0x3AE204`, ~400 k executions in a 90 s boot); they
  are emulated in the UND path (`src/hv/trap/und.rs`).
- **FPA-class coprocessor ops.** The StrongARM FPA control/status
  register accesses are emulated at EL2; FPA load/store/arithmetic
  UNDs are routed to the kernel's own FPE handler through a bypass
  stub in the ROM tail.
- **`MRS Rd, SPSR` in User mode.** StrongARM returned CPSR; A53 makes
  it UNPREDICTABLE. Emulated in the UND path.

### 5.1 Endianness

The ROM was assembled for the SA-1100 in ARMv4 **BE-32**
(word-invariant big-endian). Cortex-A53 AArch32 has only LE and BE-8.
The guest therefore runs BE-8 (`CPSR.E = 1`, `SCTLR_EL1.EE = 1`), and
the load-time ROM image is split by role:

- **Code** words are byteswapped at load, because ARMv7-A always
  fetches instructions little-endian (DDI 0406C.d §A3.3.1).
- **Data** words are stored BE-natural, so a `CPSR.E=1` `LDR` returns
  the kernel's intended numerical value.

Which words are which comes from a build-time classifier bitmap
(`classify/<hash>/reach.bitmap`, one bit per ROM word), produced by
`scripts/classify-symbols.py` + `tools/classify-rom` and consumed by
`newton::loader`. EL2-side reads and writes of guest memory funnel
through `hv::guest_endian`, which swaps for data addresses and passes
through for ROM-code addresses. The full derivation and the audit of
every B-bit-visible behaviour are in
[`docs/ENDIAN_FIXES.md`](docs/ENDIAN_FIXES.md).

The same bitmap defines "real code" for the inline-stub liveness
walker, so instruction rewriting can never wander into a literal pool
or string table.

### 5.2 In-ROM stubs and trampolines

The ROM aperture's tail holds hypervisor-written AArch32 code, all
registered as hypervisor code ranges so the endian layer and the
tracer treat it correctly:

- `0x008FFF00` — the DABT fast trampoline: it dispatches alignment
  faults straight to the emulator and falls through to the slow path
  otherwise.
- `0x00900000..0x00E00000` — function-tracer trampoline pool
  (`--features trace`).
- `0x00E00000..0x00FFFF00` — the inline-stub pool: 16-word slots,
  reachable from any ROM call site by a ±32 MiB `B`, used by
  `unaligned_inline`. Scratch registers are chosen by an
  APCS-conformant liveness walk (`inline_patch::live_regs_at`); the
  scratch pool at IPA `0x06000000` holds per-stub literals and the
  trampolines' banked-register save area.
- `0x00FFFF00..` — the UND and DABT vector trampolines, which capture
  banked state on the guest side before HVC-ing into EL2.

## 6. Interrupts and timer

`HCR_EL2.IMO`/`FMO`/`AMO` route physical IRQ/FIQ/SError to EL2. Host
IRQs arrive at the BCM2835 VIC (raspi3b) or GICv3 (FVP, brought up
through an EL3 stub) and are dispatched behind `host::platform`, so
the generic IRQ path carries no platform conditionals.

Peripheral models decide when the guest should see a Newton interrupt:
EL2 updates the VIC shadow state (`peripherals::vic`) and raises
`HCR_EL2.VI`/`VF`, and the CPU vectors to the guest's own handlers.
The EL2 physical timer (CNTHP) rearms on every guest match-register
write, giving the kernel's 3.6864 MHz tick and match registers real
wall-clock pacing; WFI wakes on real time.

## 7. Peripherals — guest side

Rust ports of Einstein's models under `src/peripherals/`, register-level
behaviour preserved. Two contracts: MMIO-window peripherals implement
`MmioPeripheral { owns, read, write, peek_word }`; native-primitive
peripherals implement `NativeDriver { DRIVER_ID, handle }` behind the
CP10/CP11 gateway. Per-peripheral detail and Einstein cross-references
are in [`docs/peripherals.md`](docs/peripherals.md).

VIC, DMA, flash (+ driver), PCMCIA, serial (+ driver), screen, tablet,
sound, battery, printer, network, platform, ASIC, the in/out
translators, and the host-call bridge are all modelled. Every handler
halts loudly on an input it doesn't model, with a dump naming the
table entry to extend — the loud halt is the trip-wire, not a nuisance.

## 8. Peripherals — host side

Bare-metal drivers under `src/host/`, selected per I/O axis by Cargo
feature (see the feature table in [`README.md`](README.md)):

- **Console** — PL011, with a DMA-fed TX path on real hardware; Arm
  semihosting when a host is listening.
- **Display** — VideoCore mailbox + framebuffer (`host-io-pi-fb`), or
  a blit stream forwarded over semihosting to `tools/host-viewer`
  (`host-io-semihost`).
- **Input** — TSTP MTouch USB digitizer over the DWC2 OTG controller
  (`input-mtouch`); mouse-as-pen through the host viewer on QEMU/FVP.
- **Audio** — VC4 HDMI MAI injector (`audio-pi-hdmi`); the null
  backend still arms timer-paced DMA-completion IRQs so the guest's
  sound path completes.
- **Storage** — BCM2835 SDHOST + FAT32 for flash persistence
  (`flash-persist-sd`), with non-blocking multi-block DMA autosave
  ([`docs/SD_DMA_AUTOSAVE.md`](docs/SD_DMA_AUTOSAVE.md)); a host file
  over semihosting otherwise.

## 9. Development and debugging

Both emulated hosts must stay green on every commit
(`guest-tests/scripts/run-all.sh`, `--platform fvp` for FVP); any
divergence between them is tracked down rather than gated away.

- **QEMU `raspi3b`** — fast iteration. Its AArch64↔AArch32 banked
  register plumbing is quirky; the catalogue is
  [`docs/QEMU_BUGS.md`](docs/QEMU_BUGS.md), and the banked-register
  entries in particular should be read before blaming our code.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — accurate reference: GICv3,
  exact generic-timer and cache model. Slower wall-clock.
- **Pi Zero 2 W** — the deployment target, validated end-to-end. The
  Pi 3B is not a stepping stone: same SoC, same `kernel8.img`, only
  the connectors differ. See
  [`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md).

EL2 breakpoints work directly under gdb; AArch32 guest breakpoints go
through the `bg` / `bp` helpers in `scripts/gdb-init` because
qemu-system-aarch64's gdbstub is aarch64-only. Beyond gdb the
hypervisor carries a function-level tracer, trap histograms, kernel
task/heap dumps, and the guest-visible diagnostic vectors — see
[`README.md`](README.md) and `src/diag/`.

## 10. State

The 717006 ROM boots through kernel, scheduler and NewtonScript
interpreter to the Welcome UI, and the builtin apps run interactively
on all three hosts. Remaining work — add-on package installation,
snapshot resume, the unported guest serial port and PCMCIA images,
guest-TLB maintenance, performance measurement, and other ROM
versions — is tracked in [`PLAN.md`](PLAN.md).
