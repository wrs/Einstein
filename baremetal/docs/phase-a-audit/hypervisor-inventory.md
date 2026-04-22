# Hypervisor inventory — pre-Phase-A-closeout

Output of the "what does our hypervisor currently do?" Explore
subagent, 2026-04-21. Based on exhaustive source review:
`src/*.rs`, `src/peripherals/*.rs`, `PLAN.md`, `HIGHLEVEL.md`,
`README.md`. This snapshot pre-dates the Phase A closeout commits;
every "⚠ Stub / ❌ Absent" item tagged here was the audit target.

---

## 1. ROM LOAD-TIME PATCHES

**File: src/rom_patches.rs, src/guest_mem.rs**

Applied **after byteswap**, **before first ERET**:

| PC/Offset | Patch Type | Value Written | Purpose | Lines |
|-----------|-----------|----------------|---------|-------|
| 0x0000_13F4 | gDebugger | 0x0000_0001 | Enable driver-dependent boot path | rom_patches.rs:67 |
| 0x0000_13FC | gNewtConfig | 0x0000_8202 | kEnableListener\|kDefaultStdioOn\|kEnableStdout | rom_patches.rs:68 |
| 0x0008_A20C | Ignore RTC write | MOV PC,LR (0xE1A0_F00E) | Skip stall on RTC hw access | rom_patches.rs:69 |
| 0x000D_B0D8..0xDC | BeaconDetect no-op | MOV R0,#0; MOV PC,LR | Skip hanging geoport detect loop | rom_patches.rs:70-71 |
| 0x0014_12F8 | Screen calibration skip | B +0x24 | Skip calibration sequence | rom_patches.rs:72 |
| 0x0030_F088, 0x0042_0750, 0x0042_0798, 0x004D_CA14 | Year 2010 time-base | Minutes/seconds constants | Keep Newton time arithmetic valid | rom_patches.rs:73-76 |
| **Offset 0x04** | UND vector | B UND_TRAMPOLINE (0xEA...) | Trampoline to stack-free stub at 0x00FFFF00 | guest_mem.rs:760 |
| **Offset 0x10** | DABT vector (DIAG patch, temporary) | HVC #0x11 (0xE140_0171) | Intercept first DABT for context dump (Phase B diagnostic) | guest_mem.rs:661 |
| **CP15 encodings** | StrongARM → ARMv7 | CRm ← 0 for c1,c2,c3,c5,c6 | Rewrite StrongARM lax MCR/MRC encoding to ARMv7 standard | guest_mem.rs:815 |
| **UND trampoline** | 13-word stub at PA 0x00FFFF00 | Stack-free save → HVC #0x10 | Saves R0/R1, LR_und, SPSR_und, LR_svc to RAM slots | guest_mem.rs:763-776 |

**Affected Memory Regions:**
- ROM offset 0x00..0x20: vector table (+ UND trampoline branch at 0x04)
- ROM offset 0x00FFFF00..0x00FFFF34: UND trampoline body
- ROM offset 0x80..0xFF: pre-allocated zero space (CP15 patches allowed)
- RAM IPA 0x0400_5F00..0x0400_5F10: UND save slots

**L1 Table Normalization (`fix_stage1_xn_bits`, guest_mem.rs:215-319):**
- Re-run on **every SCTLR M=0→M=1 transition** (TTBR0 write = CP15 trap)
- **Fine-table rewrite**: L1 descriptors with type 0b11 → 0b00 (fault); fixes PCMCIA VA placeholders at 0x78M/0x90M/0xACM
- **Section normalization**: Clear XN/AP[2]/TEX/S/nG bits; force AP=0b11, C=B=1
- **Coarse L2 normalization**: Rewrite L1 entries type 1 to minimal valid form; walk all L2 tables (coarse), strip ARMv4 subpage bits, force AP=0b11, XN=0

---

## 2. TRAP HANDLERS

**File: src/trap.rs**

All trap handlers invoked from the EL2 synchronous exception vector
(offset 0x600 in vectors.s).

| EC Code | Trap Reason | Handler | Key Actions |
|---------|-------------|---------|-------------|
| **0x03** | Trapped CP15 (TVM/TRVM/TIDCP) | `handle_cp15_trap` (line 1570) | Decode MCR/MRC; dispatch to 15-entry table: SCTLR/TTBR/DACR/VBAR/CPACR read/write, FSR/FAR read, cache ops (c7), TLB ops (c8), StrongARM clock (c15 c1 2 UND-caught instead). Trigger tracer on M=0→M=1 SCTLR edge. |
| **0x07** | FP/SIMD trap (MCR p10/11) | `handle_fp_simd` (line 1753) | Route to `peripherals::native_primitives::execute`; dispatches driver-ID + subfn. Unknown codes halt loudly. |
| **0x12** | HVC AArch32 | `handle_hvc` (line 620) | Dispatch immediate: 0x01=putchar, 0x02=log-hex, 0x03=PASS, 0x04=FAIL, 0x05=progress-mark, 0x10=UND_TAG, 0x11=DIAG, 0x12=DIAG_LR, 0x20=snapshot-save, 0x30=shadow-stub-patch. Unknown immediates halt. |
| **0x20** | Instruction abort (lower EL) | `handle_instruction_abort` (line 537) | If permission fault + in RAM: lazy-patch shadow stubs, flip XN off, retry. Else: halt loudly. |
| **0x24** | Data abort (lower EL) | `handle_data_abort` (line 227) | Decode ISV/WNR/SAS/SRT; shadow-stub transparency path for patched code (inject into guest DABT vector with un-XOR'd FAR); else route IPA to `mmio::read/write`. Advance ELR by 4. |
| **Unknown EC** | Unexpected trap | `handle_unknown` (line 1968) | Halt immediately with ESR/ELR/SPSR dump. |

**Async Handlers (IRQs):**

| Source | Handler | Behavior |
|--------|---------|----------|
| CNTHP (EL2 physical timer) | `trap_irq` (line 149) | Call `vic::poll_timer_matches()`; call `timer::on_irq()`; call `update_virq()`; call `snapshot::maybe_autosave()`. |

**Internal helpers:**

| Function | Purpose | Line |
|----------|---------|------|
| `handle_und` | UND opcode decode/emulate. SWP/SWPB, SystemBootUND, DebuggerUND, TapFileCntlUND, CP15 c15 c1 2 clock-control no-op, CP15 c7 c7 0 deprecated cache-invalidate (IC IALLUIS), tracer UDFs. | 747 |
| `handle_diag` / `handle_diag_lr` | Phase B diagnostic: capture DABT context + stage-1 walk; read R14_abt via AArch32 trampoline. | 915, 1118 |
| `emulate_swp` | Atomic swap: read PA, write PA, update Rt context. Single-threaded at EL2. | 1320 |

---

## 3. HVC/SVC DISPATCH TABLE

**File: src/trap.rs, handle_hvc (line 620)**

| Immediate | Name | Behavior |
|-----------|------|----------|
| 0x01 | putchar | Read r0[7:0], write to UART (append CR if '\n') |
| 0x02 | log-hex | Read r0, print as hex |
| 0x03 | PASS | Halt hypervisor with "guest test PASSED" |
| 0x04 | FAIL | Halt hypervisor with "guest test FAILED (code=r0)" |
| 0x05 | progress-mark | Read r0, log as milestone marker |
| 0x10 | UND_TAG | Enter `handle_und` |
| 0x11 | DIAG_TAG | Enter `handle_diag` |
| 0x12 | DIAG_LR_TAG | Enter `handle_diag_lr` |
| 0x20 | snapshot-save | Save guest GPRs + PC/CPSR to snapshot ring |
| 0x30 | shadow-stub-patch | Patch ROM/RAM range; return count in r0 |
| unknown | — | Halt |

---

## 4. GUEST PHYSICAL MEMORY MAP & STAGE-2 MAPPING

**File: src/stage2.rs, src/guest_mem.rs**

| Guest IPA Range | Size | Backing | Perms | Notes |
|-----------------|------|---------|-------|-------|
| 0x0000_0000–0x00FF_FFFF | 16 MiB | GUEST_ROM | RO | Newton ROM + Einstein REx at 0x00800000 |
| 0x0200_0000–0x023F_FFFF | 4 MiB | GUEST_FLASH[0..0x400000] | RW | Flash bank 0 (seeded with DLDS/OSCD headers) |
| 0x0400_0000–0x043F_FFFF | 4 MiB | GUEST_RAM | RW | Newton kernel L1 table at start (TTBR0=0x0400_0000) |
| 0x0C00_0000–0x0C3F_FFFF | 4 MiB | GUEST_RAM (mirror) | RW | ⚠ Phase A bring-up crutch — targeted for removal |
| 0x0E00_0000–0x0E1F_FFFF | 2 MiB | GUEST_FB | RW | Framebuffer |
| 0x0F00_0000–0x0F18_0FFF | varies | — | stage-2 fault | MMIO region |
| 0x0F18_1000–0x0F18_1FFF | 4 KiB | TICK_PAGE (L3 entry) | RO | Non-trapping K_HDWR_TICKS; updated by CNTHP IRQ |
| 0x0F18_2000–0x0F18_4FFF | varies | — | stage-2 fault | VIC match registers + interrupt state |
| 0x0F08_0000–0x0F09_9000 | varies | — | stage-2 fault | DMA bank 1/2 |
| 0x0F1C_0000–0x0F20_0000 | 64 KiB | — | stage-2 fault | 4 × TSerialChip UART windows |
| 0x1000_0000–0x1040_0000 | 4 MiB | GUEST_FLASH[0x400000..] | RW | Flash bank 1 |
| 0x3000_0000–0x4000_0000, 0x4000_0000–0x5000_0000 | 256 MiB × 2 | — | stage-2 fault | PCMCIA slot 0 + 1 (stub "no card") |
| 0x0800_0000–0x0900_0000 | 16 MiB | — | stage-2 fault | RAM probe "absent bank" |
| 0x1040_0000–0x2000_0000 | varies | — | stage-2 fault | REx / extra-flash absent probe |

**Stage-2 table structure (4 KiB granule, T0SZ=32, SL0=1, start at L1):**
- L1: 512 × 1 GiB entries. L1[0] → L2 table descriptor
- L2: 512 × 2 MiB block descriptors
- L3 (TICK_PAGE only): 512 × 4 KiB page descriptors for 0x0F000000..0x0F200000

---

## 5. PERIPHERALS IN src/peripherals/

| Module | File | IPA Range | Behavior |
|--------|------|-----------|----------|
| VIC | vic.rs | 0x0F18_xxxx subset | Functional — int_present/int_ctrl/fiq_mask/match_reg/GPIO/edge latching; match-reg writes rearm CNTHP |
| Timer (CNTHP) | timer.rs | controls EL2 physical timer | Functional — programs CNTHP_CVAL_EL2; 1 ms fallback heartbeat |
| DMA | dma.rs | 0x0F08_0000–0x0F09_8800 | ⚠ Stub: assignment latches, rest 0 |
| Flash | flash.rs | 0x0200_0000, 0x1000_0000 | ⚠ Raw RW + seeded DLDS/OSCD headers; **no Intel 28F016 command-set model** |
| PCMCIA | pcmcia.rs | 0x3000_0000–0x5000_0000 | ⚠ Stub: reads 0xFFFFFFFF ("no card"); writes drop |
| Serial | serial.rs | 0x0F1C_0000–0x0F20_0000 | ⚠ Stub: TX logged, RX empty |
| Screen | screen.rs | via native_primitives driver 0x4 | Real blit implementation |
| Native Primitives | native_primitives.rs | via CP10/11 trap | ⚠ Only null test + screen blit; all other driver classes halt |

**Registers stubbed write-accept / read-constant in mmio.rs:**
- Memory controller: 0x0F00_1000, 0x0F04_3000, 0x0F04_3800, 0x0F04_8000, 0x0F05_2C00, 0x0F05_3000, ...
- External data abort / bank control: 0x0F24_0000, 0x0F24_1000, 0x0F24_1800, 0x0F24_2400, 0x0F24_3000
- Bus / pin-strap: 0x0F28_0000, 0x0F28_0400, 0x0F28_0800, 0x0F28_3000, 0x0F28_3400, 0x0F28_4000
- Power / GPIO: 0x0F18_CC00, 0x0F18_D000, 0x0F18_D800, ...

All are **write-accept, no-op reads**. Unknown addresses halt immediately.

---

## 6. NATIVE PRIMITIVES DISPATCH TABLE

**File: src/peripherals/native_primitives.rs**

Invoked on MCR p10/p11 traps (EC=0x07, CPTR_EL2.TFP enabled).

| Driver ID | Subfn | Behavior |
|-----------|-------|----------|
| 0x000000 | 0x00 | Null primitive (test gateway): set r0 = 0 |
| 0x000004 | (screen) | Blit: copy guest region to GUEST_FB |
| **unknown** | — | ⚠ Halt with "unknown native primitive" |

**Extensibility:** simple switch on (driver_id, subfn). Bit 31 set
triggers "virtualized call" error (deferred).

---

## 7. COPROCESSOR HANDLING

**File: src/trap.rs (handle_cp15_trap, line 1570)**

| Opcode | Encoding | Trap Via | Handler |
|--------|----------|----------|---------|
| MIDR | MRC p15, 0, Rt, c0, c0, 0 | TVM/TRVM | ⚠ Returns Cortex-A53 MIDR (NOT patched to StrongARM) |
| SCTLR | MCR/MRC p15, 0, Rt, c1, c0, 0 | TVM/TRVM | Pass-through to SCTLR_EL1; triggers tracer + fix_stage1_xn_bits |
| TTBR0 | MCR p15, 0, Rt, c2, c0, 0 | TVM | Pass-through; triggers fix_stage1_xn_bits on first write |
| TTBR1 | MCR p15, 0, Rt, c2, c0, 1 | TVM | Pass-through to TTBR1_EL1 |
| DACR | MCR/MRC p15, 0, Rt, c3, c0, 0 | TVM | Pass-through to DACR32_EL2 |
| FSR | MRC p15, 0, Rt, c5, c0, 0 | TRVM | Return DFSR |
| FAR | MRC p15, 0, Rt, c6, c0, 0 | TRVM | Return DFAR (FAR_EL1 low 32) |
| c7 cache ops | — | TSW | AArch64 equivalent |
| c8 TLB ops | — | TIDCP | AArch64 equivalent |
| VBAR | MCR/MRC p15, 0, Rt, c12, c0, 0 | TRVM | Pass-through to VBAR_EL1 |
| CPACR | MCR/MRC p15, 0, Rt, c1, c0, 2 | CPTR | Open on cold boot |
| CLIDR, CCSIDR | MRC p15, 1, Rt, c0, c0, 0/1 | TRVM | Hardcoded A53 layout |

**UND-caught CP15 quirks:**
- MCR p15, 0, Rt, c15, c1, 2 (StrongARM clock): no-op in handle_und
- MCR p15, 0, Rt, c7, c7, 0 (deprecated cache invalidate): emulate as IC IALLUIS

**HCR_EL2 configuration:**
- RW=0 (AArch32), TVM/TRVM/TIDCP/TSW (CP15 trapping), FMO/IMO/AMO (interrupt routing)
- CPTR_EL2.TFP=1 (CP10/11 to native_primitives)

---

## 8. INTERRUPT CONTROLLER EMULATION

**File: src/peripherals/vic.rs, src/timer.rs**

Newton VIC emulation (TInterruptManager-equivalent):

| Register IPA | Name | Behavior |
|--------------|------|----------|
| 0x0F183000 | int_present | OR of all latched sources; read-only |
| 0x0F183400 | int_ctrl | Gate mask; write rearms CNTHP |
| 0x0F183C00 | fiq_mask | FIQ-only mask |
| 0x0F182000 + n×0x400 | match_reg[0..3] | Deadline in Newton ticks; write reprograms CNTHP |
| 0x0F184000 + n×0x400 | int_ed_1/2/3 | Edge-detection registers |

**Timer match delivery:**
1. Guest writes match_reg[i]
2. timer::rearm() programs CNTHP_CVAL_EL2 to nearest match
3. CNTHP IRQ → `trap_irq` → `vic::poll_timer_matches()` → latch match bits
4. `update_virq()` sets HCR_EL2.VI if int_present & int_ctrl != 0
5. ERET: guest takes virtual IRQ at VA 0x18

**Tick clock:**
- Real: A53 CNTPCT_EL0 @ CNTFRQ_EL0 (typically 62.5 MHz)
- Newton domain: 3.6864 MHz × 128 scaling = 471 MHz for real-time sync
- Exposed via non-trapping TICK_PAGE

---

## 9. TIMERS

**File: src/timer.rs, src/stage2.rs (tick_page)**

- CNTHP initialized to far-future; reprogrammed on match-reg writes
- CNTHPIRQ routed to core-0 IRQ via BCM2836 per-core local peripheral
- 1 ms fallback heartbeat ensures tick page advances

**Tick page:**
- Backing: stage-2 L3 page at IPA 0x0F181000
- +0x000 calendar ⚠ returns 0 (not wired to host time)
- +0x400 alarm ⚠ returns 0
- +0x800 K_HDWR_TICKS — functional

---

## 10. SERIAL

**File: src/peripherals/serial.rs**

Four UART emulations at 0x0F1C_0000..0x0F20_0000. All ⚠ stub:
- Status returns "TX FIFO empty, RX empty"
- TX writes logged
- RX reads return 0 (not plumbed to host stdin)

---

## 11. TRACER / FUNCTION TRACE

**File: src/tracer.rs**

Enabled via `cargo run --release --features trace,quiet`.

- Build-time: parses `_Data_/demangled_symbols.txt`, emits 3 blobs
- Load-time: defers UDF patching
- Enable-time (first SCTLR M=0→M=1): walks FN_ADDRS, replaces known
  function prologues with `UDF #i`, stashes originals
- Trap-time: handle_trace_und logs name + LR, restores instruction,
  invalidates icache, rewinds ELR

Each function logs once per boot. Limitations: requires cold boot
(ROM fingerprint differs with patches), pre-MMU functions not traced.

---

## 12. SNAPSHOT/RESUME

**File: src/snapshot.rs**

Guest state save/restore via QEMU semihosting (HLT #0xF000).

- Format: magic, version, ROM fingerprint, x0..x14, PC, CPSR, EL1
  sysregs, RAM (4 MiB), flash (8 MiB), FB (2 MiB) — ~14 MiB per slot
- 4-slot ring at `/tmp/newton-snapshot-{0..3}.bin`
- Autosave: periodic (CNTPCT wall-time, default 2 s) + on-demand via
  HVC #0x20
- Resume: load highest-seqnum valid slot, restore EL1 sysregs,
  ERET into guest at saved PC

---

## 13. INITIAL GUEST REGISTER STATE (cold boot)

**File: src/guest.rs (zero_el1_guest_state, line 65)**

| Register | Value | Rationale |
|----------|-------|-----------|
| CPSR/SPSR_EL2 | 0x000001D3 | SVC, I=1, F=1, A=1 — all interrupts masked |
| ELR_EL2 | 0x00000000 | ROM reset vector |
| SCTLR_EL1 | 0x00000000 | Stage-1 MMU off |
| TCR_EL1 | 0x00000000 | Short-descriptor VMSAv7 mode |
| TTBR0_EL1, TTBR1_EL1 | 0 | Kernel will program |
| CPACR_EL1 | 0x0C30_0000 | CP10/11 full access so MCR p10 traps |
| x0..x14 | 0 | Zeroed on cold boot |

**⚠ Divergence from Einstein:** A bit set (SError disabled);
Einstein has 0x000000D3 without A.

---

## 14. BOOT FLOW

**File: src/main.rs (kmain)**

```
1. boot.s → park cores 1-3, zero BSS, set SP, call kmain [core 0]
2. uart::init()
3. print_banner() / print_caps()
4. mmu::init() — identity-map EL2, 1 GiB Normal-WB + BCM device
5. install_vectors() — write VBAR_EL2
6. guest_mem::load_rom() — load ROM, byteswap BE→LE, apply patches
7. peripherals::flash::init() — seed DLDS/OSCD headers
8. stage2::init() — build stage-2 L1/L2/L3 tables
9. stage2::enable()
10. peripherals::vic::init() — capture CNTPCT epoch
11. timer::init() — route CNTHPIRQ, program CNTHP
12. snapshot::init() — scan existing slots
13. snapshot::load_latest() — resume if slot valid, else cold boot
14. guest::run_newton_rom() — configure HCR_EL2 + CPTR_EL2,
    zero_el1_guest_state, ERET → AArch32 SVC at IPA 0
```

---

## SUMMARY: WHAT WORKS vs. WHAT'S STUBBED (pre-closeout)

| Category | Status | Notes |
|----------|--------|-------|
| EL2 MMU + stage-1 identity map | ✅ Functional | 1 GiB Normal-WB + BCM device |
| Stage-2 page tables (L1/L2/L3) | ✅ Functional | ROM RO, RAM RW, FB RW, MMIO faults, tick page RO |
| CP15 shim (15 tuples) | ✅ Functional | |
| UND handler (SWP/SWPB, Einstein UNDs, CP15 quirks) | ✅ Functional | |
| HVC dispatch (test protocol + UND_TAG + DIAG) | ✅ Functional | 8 known immediates |
| VIC state machine | ✅ Functional | 4 timer sources wired |
| CNTHP timer + BCM2836 routing | ✅ Functional | |
| Tick page (non-trapping reads) | ✅ Functional | Ticks only — calendar/alarm return 0 |
| Fine-table / section / L2 XN normalization | ✅ Functional | |
| ROM patches (word-write only) | ✅ Functional | 10 patches applied |
| Shadow-stub lazy patching | ✅ Functional | |
| Function tracer | ✅ Functional | |
| Snapshot save/resume | ✅ Functional | |
| Serial (4 UART) | ⚠ Stub | Status=empty, TX logged, RX=0 |
| DMA registers | ⚠ Stub | Assign latches, rest 0; no transfer |
| Flash banks (raw RW) | ⚠ Stub | Seeded headers; no erase/program |
| PCMCIA windows | ⚠ Stub | "No card" |
| Native primitives | ⚠ Minimal | Null test (0x000000, 0x00) + screen blit (0x4) |
| Screen blit | ✅ Functional | VA translation to GUEST_FB |
| ~40 peripheral regs | ⚠ Stub | Write-accept no-ops |
| Flash identify (Intel 28F016) | ❌ Not modeled | **Current Phase B stall** |
| RTC calendar | ❌ Not modeled | Returns 0 |
| PCMCIA card emulation | ❌ Not modeled | |
| **MIDR virtualization** | ❌ Not modeled | Returns Cortex-A53 |
| **Platform driver (native prim 0x01)** | ❌ Not modeled | |
| **SWI-injection ROM patches** | ❌ Not modeled | DebugStr, Debugger, RealClockSeconds, FTimeInSeconds, FDateFromSeconds |

**Observed boot progress:** ROM reaches 72 trace-able functions deep
then falls into `PowerOffAndReboot` after flash-identify fails
(`T28F016_SA_SVDriver`).
