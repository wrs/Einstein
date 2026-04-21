# Plan — Reach the TInterpreter constructor

## Status (as of 2026-04-21)

**Phase A is done. Phase B is mid-flight.** Detailed progress notes
live in `INVESTIGATION.md`; the high-level state is:

- Every Phase A item (fine-table rewrite, UND handler,
  StrongARM-clock no-op, TSerialChip, CP10/11 native primitives,
  screen blit) landed with its own guest test. All 13 guest tests
  pass.
- The first hard Phase B stall — a DABT at FAR `0x0100018B` —
  was traced to `MCR p15, 0, r0, c7, c7, 0` at PC `0x18924`
  inside FlushTheCache (ARMv4 "invalidate unified cache", UNDEFINED
  on A53). Fixed: `handle_und` now recognises the encoding and
  emulates as `IC IALLUIS`. Cascade fixes: the UND trampoline no
  longer depends on SP_und (4-instruction stack-free variant), its
  save slot moved off the L1-table-overlap at IPA `0x04000400`,
  `DebuggerUND` properly advances past its null-terminated string,
  and `fix_stage1_xn_bits` now re-runs on every M=0→M=1 SCTLR
  transition (the kernel populates L2 tables incrementally).
- Tick-polling throughput was the next cliff: ~75% of all stage-2
  traps were reads of K_HDWR_TICKS in BootOS delay loops. Fixed
  by splitting the 2 MiB stage-2 block at IPA `0x0F000000` into an
  L3 table and planting a 4 KiB RAM-backed RO page at
  `0x0F181000`, updated from the CNTHP IRQ. Throughput on a 90 s
  boot: **1.23 M traps (13.6× fewer)**, no hot PC.
- ROM debug logging (22 DebuggerUND sites + SystemBootUND +
  TapFileCntlUND) is now surfaced correctly — strings are read in
  BE byte order so the original ROM panic messages come through
  verbatim ("Zot! GenericSWI called from non-user mode.",
  "SWI from non-user mode (rebooting)", etc.) and each unique site
  logs once via a per-PC seen-set.

**Current stall**: the two SWI-from-non-user-mode DebuggerUND
panics (`PC=0x3ae188` and `PC=0x3ad660`). These are likely resolved
by the byte-level endianness work on a parallel track — a scrambled
CPSR-mode byte in a register/memory save would look exactly like
this to the kernel's SWI entry check. Parked pending that work.

## Context

The target boot chain per `_Data_/symbols.txt` is:

1. `0x00018688 BootOS` → cache/MMU flushes, stack setup
2. `0x00021B70 TADC::Init` (touchscreen ADC)
3. `0x000307D4 InitAlertManager`
4. `0x000E6C44 InitCirrusHW` (main-ROM entry — not the REx stub we've been looking at)
5. `0x0007CC4C TDMAManager::Init`
6. `0x00030F54 TAppWorld::Init`
7. `0x00034500 TApplication::InitToolbox`
8. `0x0038C89C __main`
9. `0x002F40E0 TInterpreter::TInterpreter` ← midterm goal

All of these are in the main ROM (< `0x00800000`). That means the REx references we've been chasing may be side paths — the primary path only needs the main ROM mapped, the peripheral devices actually modelled (not stubbed), and the CPU-level behaviours (SWP, CP15 quirks, fine-table descriptors, UND opcodes) handled correctly at EL2.

## Approach (two phases)

### Phase A — build every known-required piece as a real handler ✅ DONE

By the end of Phase A, every piece of hardware / CPU behaviour required by the 717006 ROM's early-boot path must have a *real* implementation — no per-opcode patches, no stubs. "Real" here means: when the guest executes a SWP, takes an UND exception, does an MCR to CP10, or touches a serial-chip register, our hypervisor routes that access to a proper EL2 handler (or a properly-modelled MMIO device) that does the correct thing. Unknown sub-cases return a loud error, not a silent stub value.

Each item lands as its own commit + its own `guest-tests/tests/test_<name>.S`. If a test fails we fix it against the test, not the ROM. The ROM is not touched in Phase A.

1. **Fine-table (0b11) L1 descriptor rewrite.** ✅ Done. `HIGHLEVEL.md` §5.4 + `probe/FINDINGS.md`. The 717006 kernel installs three L1 fine-table descriptors (VA `0x78000000` / `0x90000000` / `0xAC000000`) that ARMv7 doesn't walk. Extend `guest_mem::fix_stage1_xn_bits` to rewrite type `0b11` → `0b00` (fault) in the guest's L1, so touching those VAs raises a proper stage-1 translation fault that our abort handler can see rather than looping in undefined walker behaviour. Guest test: synthesise an L1 with a fine descriptor, run the fix, verify the entry was rewritten and a subsequent stage-1 walk for that VA takes a translation fault. Test: `test_finetable_rewrite`.

2. **Undefined-instruction handler at EL2 (SWP + Einstein UND opcodes).** ✅ Done. Test: `test_und_handler`. ARMv7 AArch32 has no HCR_EL2 bit that traps UND directly to EL2, so we install a trampoline at the guest's UND vector (VA `0x00000004`; ROM offset 0x04 patched to a branch to a helper at ROM offset 0x00FFFF00) that HVCs to EL2. In `trap.rs`, `handle_und` reads the faulting instruction from guest memory, decodes it, and dispatches:
   - `SWP/SWPB` (any encoding) → emulate atomically via EL2 load-store on the translated PA.
   - `0xE6000010` (`SystemBootUND`) → NOP semantic; ELR += 8 (opcode + payload word).
   - `0xE6000510` (`DebuggerUND`) → read the null-terminated ASCII message that follows the opcode, log it (deduped by PC), advance ELR past the aligned message tail.
   - `0xE6000810` (`TapFileCntlUND`) → ELR += 8; read payload word, log (deduped by PC).
   - `MCR p15, 0, Rt, c15, c1, 2` (StrongARM clock-control, ARMv4-only, UND on A53) → no-op.
   - `MCR p15, 0, Rt, c7, c7, 0` (ARMv4 "invalidate unified cache", UND on A53) → emulate as `IC IALLUIS; DSB ISH`.
   - Anything else → log opcode + PC + banked context dump and **halt loudly**.

   **Trampoline redesign (Phase B discovery)**: the original push-based trampoline depended on SP_und being initialised, which BootOS doesn't do until `SetUpStacks` at 0x11EFD4 — after the first UND fires at 0x18924. Rewritten to a 4-instruction stack-free form that writes LR / SPSR to a RAM slot via a PC-relative literal pointer. Save slot also moved from `0x04000400` (overlaps the guest L1 table at TTBR0 base!) to `0x04005F00` in the RAM-mirror window.

3. **CP15 StrongARM clock-control quirk.** ✅ Done. Test: `test_cp15_strongarm_clock`. `MCR p15, 0, Rn, c15, c1, 2` handled in `handle_und` (not `handle_cp15_trap` — it UNDs at EL1 before TIDCP can trap it). Same "no-op, ELR += 4, budgeted log" pattern.

4. **`peripherals/serial.rs` — four TSerialChip implementations.** ✅ Done. Test: `test_serial`. `peripherals::serial` owns `0x0F1C_0000..0x0F20_0000`. Status register returns "TX FIFO empty + RX empty" so polling loops terminate; TX writes are consumed and logged; RX returns no-data.

5. **`peripherals/native_primitives.rs` — CP10/11 EL2 handler.** ✅ Done. Test: `test_native_primitives`. `CPTR_EL2.TFP` enabled in `guest.rs::configure_el2_traps`. `handle_fp_simd` decodes the MCR, calls `native_primitives::execute`, which matches against an encoding table. Unknown encodings halt with full context.

6. **`peripherals/screen.rs` — Blit/screen primitive handler.** ✅ Done. Test: `test_screen_blit`. Registered as the screen-class dispatch target in `native_primitives`. Real blit copies into `GUEST_FB` via stage-1 + stage-2 translation.

Phase A end state: 13/13 guest tests passing. Every CPU instruction and every MMIO region touched by the early-boot path has a real handler behind it. "Unknown sub-case" responses are intentional, loud, and act as trip-wires for Phase B.

### Phase B — boot the 717006 ROM and debug failures one at a time

Run the Newton ROM under the hypervisor and drive toward TInterpreter. For each stall:

1. Identify the exact PC where the guest is stuck (heartbeat sampler is already in `trap.rs::trap_irq`; DIAG HVC at VA 0x10 catches any DABT with full context).
2. Disassemble the ROM at that PC and consult `_Data_/symbols.txt` to name the function.
3. Run the same offset under Einstein (`build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 30`) and compare — the probe now also records every guest data abort with `{PC, FAR, FSR, mode}`, and every prefetch abort with `{PC, IFSR, mode}`, plus the existing CP15 / SWP / mode-transition counts. Diff vs. our hypervisor trap log isolates the divergence.
4. Reproduce the gap with a focused guest test if feasible, or directly cross-reference against Einstein / `_Data_/symbols.txt` to identify the cause.
5. Fix the hypervisor.
6. Re-run ROM, go to next stall.

Concrete checkpoints — current status:

- ✅ **Reach `BootOS+8`** — `cp15.sctlr.mmu_on` at PC `0x18898`.
- ✅ **Reach `FlushTheCache` / `FlushTheMMU`** (`0x000188F8`, `0x0001892C`).
- ✅ **Survive post-MMU-on** — the `MCR c7 c7 0` UND at 0x18924 is handled; the DebuggerUND advance-past-string is fixed; `fix_stage1_xn_bits` re-runs on M=0→M=1 edges so late-populated coarse L2 entries are normalised.
- ✅ **Tick polling no longer dominates runtime** — non-trapping K_HDWR_TICKS via stage-2 RAM-backed page, 13.6× trap reduction.
- 🟡 **Pass the SWI-from-non-user-mode panics** (`0x3ae188`, `0x3ad660`) — current stall. Parked pending byte-level endianness work on the parallel track. The ROM's own debug strings are now surfaced in the log (via BE byte-order reading + per-PC dedup) so the next panic, whatever it is, will be diagnostic by default.
- ⬜ **Pass `InitCirrusHW` (main-ROM, `0x000E6C44`)**.
- ⬜ **Pass `TDMAManager::Init` (`0x0007CC4C`)** — will exercise our DMA port.
- ⬜ **Pass `TAppWorld::Init` (`0x00030F54`)** — first application-world init; likely trips TInterruptManager-backed delays (now cheap thanks to the non-trapping tick page).
- ⬜ **Reach `__main` (`0x0038C89C`)** — C++ runtime static initialisers.
- ⬜ **Break on `0x002F40E0` (`TInterpreter::TInterpreter`)** — declare midterm victory.

## Critical files

Current layout:

- `src/guest_mem.rs` — ROM load, byteswap, fix_stage1_xn_bits (L1 + coarse-L2 normalise; re-run on M=0→M=1 SCTLR edges), UND-vector trampoline at ROM offset 0x00FFFF00, DABT-vector DIAG HVC patch at ROM offset 0x10, `dump_stage1_walk` helper.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / DIAG_LR_TAG), `handle_und` (SWP, SystemBoot/Debugger/TapFileCntl UND, `MCR c15,1,2` StrongARM clock, `MCR c7,c7,0` deprecated cache-invalidate), `handle_fp_simd` → CP10/11 dispatch, `handle_diag` / `handle_diag_lr` two-stage DABT-intercept stub.
- `src/guest.rs` — HCR_EL2 setup (TVM, TIDCP, TSW, IMO, FMO, AMO), CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3 tables. 2 MiB block layout for ROM/RAM/flash/FB, refined to 4 KiB L3 pages for the `0x0F000000..0x0F200000` MMIO window so a RAM-backed `TickPage` lives non-trapping at IPA `0x0F181000`. `tick_page::update()` pumped from `timer::on_irq`.
- `src/timer.rs` — CNTHP driver; 1 ms heartbeat when no VIC match pending so tick-page updates progress even during guest busy-waits.
- `src/peripherals/serial.rs` — four TSerialChip models.
- `src/peripherals/native_primitives.rs` — CP10/11 handler with encoding table.
- `src/peripherals/screen.rs` — blit into `GUEST_FB`.
- `src/peripherals/vic.rs` — interrupt controller + tick clock; `K_HDWR_TICKS` now advances the non-trapping RAM page instead of returning from a trap.
- `src/mmio.rs` — routes `0x0F1C_0000..0x0F20_0000` to `serial`, plus existing VIC / DMA / PCMCIA / stub dispatch.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-{0..3}.bin`.
- `guest-tests/tests/` — 13 tests (test_hello, test_vic, test_flash, test_dma, test_pcmcia, test_cp15_fault_regs, test_finetable_rewrite, test_und_handler, test_cp15_strongarm_clock, test_serial, test_native_primitives, test_screen_blit, test_snapshot).
- `guest-tests/scripts/run-test.sh` — clears `/tmp/newton-snapshot-*.bin` before each run so a stale snapshot can't cause a test to resume mid-run.

## Verification

Each Phase A / Phase B commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 13 tests pass at the current commit.

End-of-Phase-A milestone (met):

```
cd baremetal && timeout 30 cargo run --release
```

Boot reaches deep initialisation code past `0x0E6B94`. No tight loops
from tick polling; traps are evenly distributed across task-switch
SCTLR toggles and scattered MMIO touches. Current terminal condition
is the SWI-from-non-user-mode DebuggerUND panics visible in the log.

End of Phase B / midterm goal (pending):

```
cd baremetal && timeout 60 cargo run --release
```

Trap log shows guest PC sampled at or near `0x002F40E0` — the TInterpreter constructor.

Einstein as reference:

```
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 30
```

Captures CP15 / SWP / mode-transition counts plus the new data-abort
and prefetch-abort logs. Key cross-reference point: Einstein's first
`SVC→ABT` transition is the *voluntary* `MSR CPSR_c, #0xd7` at PC
`0x18C10` (the stack-init helper), not a real abort — and Einstein
records **zero kernel-mode aborts** in 30 s of boot. Our hypervisor
matching that count is the cleanest "progress is real" signal.

## Explicit non-goals for this plan

- Exhaustively implementing every TNativePrimitives encoding upfront — only the encodings the early-boot path can realistically hit get transcribed in Phase A; others are discovered (with a loud halt) during Phase B and transcribed then. The handler itself is real; the table grows as evidence demands.
- Real screen emulation beyond a framebuffer dump — no compositor, no pen input.
- Any work past TInterpreter — scheduler, app world, package loading — waits for the next milestone.
- **Byte-level endianness equivalence** — tracked on a parallel work stream. This plan relies on it: the current Phase B stall (SWI-from-non-user-mode panics) is most-likely a symptom of it. Don't fix those panics here until the endianness track has landed; it's likely to obviate them.

## Still-in-place diagnostic scaffolding

These items should come off once Phase B is stable — leave them in
for now because they're load-bearing for the current debugging loop:

- DABT-vector HVC patch at ROM offset 0x10 (`guest_mem.rs`) → two-stage `handle_diag` / `handle_diag_lr` in `trap.rs`. Catches every stage-1 DABT with full banked-register context.
- 500-entry trap log budget at the top of `trap_sync_lower_aarch32`.
- Bring-up-critical VA walks in `handle_diag`.

Once we're past TInterpreter and confident no silent abort is
hiding, these can be pulled; the behavioural invariants they
enforce are already codified in guest tests.
