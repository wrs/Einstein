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

Phase A end state: every CPU instruction and every MMIO region touched by the early-boot path has a real handler behind it, and every known-required ROM patch Einstein applies is in place. "Unknown sub-case" responses are intentional, loud, and act as trip-wires for Phase B.

### Phase B — boot the 717006 ROM and debug failures one at a time

Run the Newton ROM under the hypervisor and drive toward TInterpreter. For each stall:

If it's clearly a loud failure for an unimplemented Einstein driver, implement that driver.

If it's another kind of failure:

1. Identify the exact PC where the guest is stuck (heartbeat sampler is already in `trap.rs::trap_irq`; DIAG HVC at VA 0x10 catches any DABT with full context; the function-level tracer dumps every named-symbol call).
2. Disassemble the ROM at that PC and consult `_Data_/symbols.txt` to name the function.
3. Run the same offset under Einstein (`build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 30`) and compare — the probe now also records every guest data abort with `{PC, FAR, FSR, mode}`, and every prefetch abort with `{PC, IFSR, mode}`, plus the existing CP15 / SWP / mode-transition counts. Diff vs. our hypervisor trap log isolates the divergence.
4. Reproduce the gap with a focused guest test if feasible, or directly cross-reference against Einstein / `_Data_/symbols.txt` to identify the cause.
5. Fix the hypervisor.
6. Re-run ROM, go to next stall.

#### Phase B progress milestones

| Date | Trace | Wedge | Resolution |
|------|-------|-------|------------|
| 2026-04-25 | ~108 k | recursive DABT in TStackInfo::Init | flash recovery path eliminated |
| 2026-04-25 | ~145 k | newt-DABT alias narrows to scheduling order | IRQ-rate + tick-page divergence fixed |
| 2026-04-26 | ~170 k | BootOS canary entry #2 (R0=0x0cc80c80) — `name`-task stack-overrun corrupts neighbour task on shared PA | **fixed** via 3-instruction ROM patch that forces per-page stack allocation in `TStackManager::ResolveFault` (mask=0xF) |
| 2026-04-26 (current) | 403 k+ | `Reboot` canary (kernel-driven self-reboot, R0=0xffffd8a5, LR=0x000d9888) — UnhandledException in user-mode flash-driver work | open |

See `INVESTIGATION.md` for the full chain of analysis on each.

## Critical files

Current layout:

- `src/guest_mem.rs` — ROM load, byteswap, `fix_stage1_xn_bits` (L1 + coarse-L2 normalise; re-runs on M=0→M=1 SCTLR edges; **note: flattens ARMv4 subpage-AP to AP=011 — see INVESTIGATION.md "TCardMessage-alias wedge resolved"**), UND-vector trampoline at ROM offset 0x00FFFF00, DABT-vector DIAG HVC patch at ROM offset 0x10, `dump_stage1_walk` helper, scratch-VA L1 section installer at L1[0x60].
- `src/trap.rs` — CP15 shim (TVM trap on writes to virtual-memory regs), HVC dispatch (UND_TAG / DIAG_TAG / DIAG_LR_TAG / SBA / tracer / canary tags), `handle_und` (SWP, SystemBoot/Debugger/TapFileCntl UND, `MCR c15,1,2` StrongARM clock, `MCR c7,c7,0` deprecated cache-invalidate), `handle_fp_simd` → CP10/11 dispatch, `handle_diag` / `handle_diag_lr` two-stage DABT-intercept stub, `handle_data_abort` with kernel-DABT forwarding for lazy stack growth.
- `src/guest.rs` — HCR_EL2 setup (TVM, TIDCP, TSW, IMO, FMO, AMO), CPTR_EL2.TFP for CP10/11, DC bit toggling for stage-1-off vs stage-1-on regimes.
- `src/stage2.rs` — stage-2 L1/L2/L3 tables. 2 MiB block layout for ROM/RAM/flash/FB, refined to 4 KiB L3 pages for the `0x0F000000..0x0F200000` MMIO window (non-trapping `TickPage` at IPA `0x0F181000`) and for the 64 KiB shadow-stub scratch carve-out at IPA `0x0600_0000`. `tick_page::update()` pumped from sync-trap exits and from heartbeat IRQs.
- `src/timer.rs` — CNTHP driver; instruction-anchored synthetic ticks (no wall-clock skew between trace/no-trace runs).
- `src/banked.rs` — AArch32 banked-register access from EL2 (SP_<mode>, LR_<mode>) per ARM ARM Table D1-79.
- `src/peripherals/serial.rs` — four TSerialChip models.
- `src/peripherals/serial_driver.rs` — TInternalSerialDriver native-primitive wrapper.
- `src/peripherals/native_primitives.rs` — CP10/11 handler with encoding table.
- `src/peripherals/screen.rs` — blit into `GUEST_FB`.
- `src/peripherals/platform.rs` — TPlatformDriver (battery / power / wake-up).
- `src/peripherals/battery.rs` — battery state.
- `src/peripherals/tablet.rs` — TTabletDriver / pen input stubs.
- `src/peripherals/sound.rs` — TSoundDriver.
- `src/peripherals/network.rs` — TNetworkDriver.
- `src/peripherals/printer.rs` — TPrinterDriver.
- `src/peripherals/host_call.rs` — Newton's HostCall ABI (file/network requests routed through Einstein-style hooks).
- `src/peripherals/in_translator.rs`, `out_translator.rs` — translator native-primitive plumbing.
- `src/peripherals/flash.rs`, `flash_driver.rs` — Newton flash filesystem + TFlash driver.
- `src/peripherals/vic.rs` — interrupt controller + tick clock; `K_HDWR_TICKS` advances a non-trapping RAM page driven by sync-trap progress + heartbeat.
- `src/peripherals/dma.rs`, `pcmcia.rs` — DMA + PCMCIA.
- `src/mmio.rs` — routes `0x0F1C_0000..0x0F20_0000` to `serial`, plus VIC / DMA / PCMCIA / stub dispatch and `0x3000_0000..0x5000_0000` to `pcmcia`.
- `src/rom_patches.rs` — Einstein word-write patches; debugger HVC injections (DebugStr / Debugger / RealClockSeconds / FTimeInSeconds / FDateFromSeconds); GetClock / SetAlarm wrap-detect ls→cc fixes; `PowerOffAndReboot`, `Reboot`, `BootOS` software-reset canaries; **`TStackManager::ResolveFault` per-page-stack-allocation patches (mask=0xF, three sites in 0x001f7a10..0x001f7c24) that fix the BootOS-canary wedge**.
- `src/shadow_stub.rs` — BE-32 byte/halfword-access patcher with three stub variants: `DeadReg` (preferred — no save/restore), `Stack` (regression-test-only), `ScratchVA` (live fallback — saves caller scratch into a 64 KiB hypervisor carve-out at VA/IPA `0x0600_0000`, plus TPIDRURW for the third register). 16-word stub layout. Pool capacity 32 764 stubs; scratch-slot capacity 8 192 (1 695 used in live ROM).
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-{0..3}.bin`.
- `src/tracer.rs` — function-level tracer (HVC trampolines on every `code-symbols.txt` entry); per-function one-shot probes (e.g. `dump_movefreeblock_entry` for the `name`-task SP / stack-page diagnostic).
- `src/guest_bp.rs` — guest breakpoint / `bp <addr>` infrastructure for the gdb workflow.
- `src/task_dump.rs` — TScheduler / TTask data-structure dumps from EL2.
- `src/tarmac.rs` — Tarmac-like instruction-trace markers (`<<TRM_START>>` / `<<TRM_STOP>>`).
- `src/unaligned.rs` — `handle_align_fault` emulator for SCTLR.A=1-induced unaligned LDR/STR aborts.
- `guest-tests/tests/` — 35 tests covering CPU / MMIO / driver paths (test_hello, test_vic, test_flash, test_dma, test_pcmcia, test_cp15_fault_regs, test_finetable_rewrite, test_und_handler, test_cp15_strongarm_clock, test_midr, test_mmio_regs, test_rtc_calendar, test_rom_patches, test_serial, test_native_primitives, test_flash_driver, test_platform_driver, test_screen_blit, test_snapshot, test_shadow_stub, test_spsr_eret, test_spsr_eret_und, test_rotate_ldr, test_battery, test_tablet, test_serial_driver, test_translators, test_host_call, test_network, test_printer, test_sound, test_bio_bank, test_alarm, test_dma_irq, test_gpio).
- `guest-tests/scripts/run-test.sh` — clears `/tmp/newton-snapshot-*.bin` before each run so a stale snapshot can't cause a test to resume mid-run.

## Verification

Each Phase A / Phase B commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 35 tests pass at the current commit.

## Explicit non-goals for this plan

- Real screen emulation beyond a framebuffer dump — no compositor, no pen input.
- Package loading — needs a solution for embedded native code

## Diagnostic scaffolding

These items should come off once the system is stable — leave them in
for now because they're load-bearing for the current debugging loop:

- DABT-vector HVC patch at ROM offset 0x10 (`guest_mem.rs::patch_dabt_vector`) → two-stage `handle_diag` / `handle_diag_lr` in `trap.rs`. Catches every stage-1 DABT with full banked-register context.
- PABT-vector HVC patch at ROM offset 0x0C — same DIAG path; tripwire for prefetch aborts.
- `handle_diag_from_bp` hook in `guest_bp.rs::handle_user_bp_und` — lets a `bp <addr>` hit hand off to the banked-reg dump stub.
- 500-entry trap log budget at the top of `trap_sync_lower_aarch32`; HVC #0x50 (tracer TAG) suppressed to avoid doubling trace output.
- Bring-up-critical VA walks in `handle_diag`.
- `tracer.rs::dump_movefreeblock_entry` one-shot probe at the `name`-task `MoveFreeBlock(0x0c2041e0, 0x20)` call — the regression diagnostic for the per-page-stack-allocation fix; stays in tree until the Phase-B chain is fully resolved.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs` — fire `HVC #0x42`/`0x43`/`0x44` on the first call rather than letting the kernel quietly re-enter and obscure the cause.

Once we're confident no silent abort is hiding, these can be pulled; the
behavioural invariants they enforce are already codified in guest tests.
