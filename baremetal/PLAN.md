# Plan — Reach the TInterpreter constructor

## Status

**Phase A is done. Phase B is mid-flight.**

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

7. **Einstein's ROM patches (word-write set).** ✅ Done (retroactively). `src/rom_patches.rs::apply_717006_patches` applies every `TJITGenericPatch` entry from `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp` that targets the 717006 ROM: `gDebugger = 1`, `gNewtConfig = 0x8202`, "ignore setting time", "BeaconDetect no-op", "avoid screen calibration", time-base (4 words). These are known preconditions Einstein relies on — several disable code paths that would otherwise spin on hardware we don't model, and at least one (`gDebugger`) selects the driver-enabled boot path. Missing from Phase A by oversight — the omission forced us to debug Einstein-specific symptoms in Phase B that had nothing to do with our hypervisor.

   Not yet ported (deferred, out of early-boot critical path):
   - `TJITGenericPatchNativeCall` entries (`DebugStr`, `Debugger`,
     `RealClockSeconds`, `FTimeInSeconds`, `FDateFromSeconds`). These
     write a `SWI #0x8xxxxx` marker that only Einstein's JIT intercepts;
     on real hardware the SWI would take the ROM's own SWI path. To
     port them we'd replace the `SWI` marker with our tracer-style
     UDF + EL2 Rust handler.
   - `TVirtualizedCallsPatches` entries (`__rt_sdiv`, `__rt_udiv`,
     `symcmp`). 5-word sequence with a bit-31 marker caught by
     Einstein's `TNativePrimitives::ExecuteNative`. Safe no-op on a
     native A53 — the ROM's own software-division routines run fine,
     the virtualized version is only a JIT speed-up.

Phase A end state: 14/14 guest tests passing. Every CPU instruction and every MMIO region touched by the early-boot path has a real handler behind it, and every known-required ROM patch Einstein applies is in place. "Unknown sub-case" responses are intentional, loud, and act as trip-wires for Phase B.

### Phase B — boot the 717006 ROM and debug failures one at a time

Run the Newton ROM under the hypervisor and drive toward TInterpreter. For each stall:

If it's clearly a loud failure for an unimplemented Einstein driver, implement that driver.

If it's another kind of failure:

1. Identify the exact PC where the guest is stuck (heartbeat sampler is already in `trap.rs::trap_irq`; DIAG HVC at VA 0x10 catches any DABT with full context).
2. Disassemble the ROM at that PC and consult `_Data_/symbols.txt` to name the function.
3. Run the same offset under Einstein (`build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 30`) and compare — the probe now also records every guest data abort with `{PC, FAR, FSR, mode}`, and every prefetch abort with `{PC, IFSR, mode}`, plus the existing CP15 / SWP / mode-transition counts. Diff vs. our hypervisor trap log isolates the divergence.
4. Reproduce the gap with a focused guest test if feasible, or directly cross-reference against Einstein / `_Data_/symbols.txt` to identify the cause.
5. Fix the hypervisor.
6. Re-run ROM, go to next stall.

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
- `src/shadow_stub.rs` — BE-32 byte/halfword-access patcher. Each patched site is replaced in place with `UDF #(0x8000 | idx)`; the UND trampoline at IPA 0x00FFFF00 routes into EL2 where `handle_sba_udf` decodes the original instruction from a 32 K-entry site table and emulates the access in Rust (with the BE-32 XOR for real memory, passthrough for MMIO). Writeback to banked R13 / R14 exits through a post-emulation trampoline at IPA 0x00FFFF80 that writes SP / LR natively in the faulting mode. See IMPLEMENTATION.md §8.5 for the full design; `INVESTIGATION.md` records why the earlier in-guest stub approach couldn't preserve CPSR flags across every mode and MMU state.
- `guest-tests/tests/` — 20 tests (test_hello, test_vic, test_flash, test_dma, test_pcmcia, test_cp15_fault_regs, test_finetable_rewrite, test_und_handler, test_cp15_strongarm_clock, test_midr, test_mmio_regs, test_rtc_calendar, test_rom_patches, test_serial, test_native_primitives, test_flash_driver, test_platform_driver, test_screen_blit, test_snapshot, test_shadow_stub).
- `guest-tests/scripts/run-test.sh` — clears `/tmp/newton-snapshot-*.bin` before each run so a stale snapshot can't cause a test to resume mid-run.

## Verification

Each Phase A / Phase B commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 20 tests pass at the current commit.

End-of-Phase-A milestone (met):

```
cd baremetal && timeout 30 cargo run --release
```

Boot reaches deep initialisation code past `0x0E6B94`. No tight loops
from tick polling; traps are evenly distributed across task-switch
SCTLR toggles and scattered MMIO touches.

End of Phase B / midterm goal (pending):

```
cd baremetal && timeout 60 cargo run --release
```

Trap log shows guest PC sampled at or near `0x002F40E0` — the TInterpreter constructor.

Current Phase B progress (2026-04-23):

- Shadow-byte-access flag-preservation resolved by replacing the
  in-guest stub with a UDF-trap emulator in EL2 Rust. See
  `IMPLEMENTATION.md` §8.5 and `INVESTIGATION.md` "Resolved —
  shadow-stub flag-preservation via UDF-trap".
- Boot now advances past `MemObjManager::PrimGetEnvDomainName` into
  `TUSharedMem::CopyToShared` / `SMemCopyToSharedSWI`, DABTs at guest
  PC 0x003AE3E4 (stage-1 translation fault at VA 0x0C001000 during
  `LDR R5, [R13, #0xC]`). Next stall to root-cause — see
  `INVESTIGATION.md` "Currently at".
- Post-MMU shadow-stub dispatch PABT was resolved earlier by moving
  pool A into the ROM aperture; the pool is gone entirely under
  UDF-trap.

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

## Still-in-place diagnostic scaffolding

These items should come off once Phase B is stable — leave them in
for now because they're load-bearing for the current debugging loop:

- DABT-vector HVC patch at ROM offset 0x10 (`guest_mem.rs`) → two-stage `handle_diag` / `handle_diag_lr` in `trap.rs`. Catches every stage-1 DABT with full banked-register context.
- PABT-vector HVC patch at ROM offset 0x0C (`guest_mem.rs`) — same DIAG path; added during the pool-A-in-ROM investigation and kept as tripwire for future prefetch aborts.
- `handle_diag_from_bp` hook in `guest_bp.rs::handle_user_bp_und` — lets a `bp <addr>` hit hand off to the banked-reg dump stub.
- 500-entry trap log budget at the top of `trap_sync_lower_aarch32`; HVC #0x50 (tracer TAG) suppressed to avoid doubling trace output.
- Bring-up-critical VA walks in `handle_diag`.

Once we're past TInterpreter and confident no silent abort is
hiding, these can be pulled; the behavioural invariants they
enforce are already codified in guest tests.
