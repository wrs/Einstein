# Plan — Reach the TInterpreter constructor

## Context

We've sunk a lot of effort into individual symptoms (tick-polling stalls, DFSR pass-through, abort-handler recursion) without progressing the boot itself. Honest assessment:

- Commits so far land us around M3/M4 of `HIGHLEVEL.md` §11: EL2 stage-1 MMU, stage-2 for guest physical layout, CP15 shim, async CNTHP timer, VIC with real edge-triggered match delivery, pure-Rust ports of flash, DMA, PCMCIA, Einstein REx loaded at PA `0x00800000`, DFSR/DFAR reads no longer hijacked by TRVM.
- Boot with the 717006 ROM stalls shortly after MMU-on in an abort-handler recursion: vector `0x0C` branches to VA `0x01A00010` (REx `PrefetchAbortHandler`), `L1[26..27]` is unmapped, re-faults forever. Einstein has the same gap in its MMU emulation (TMMU.cpp:212 walks the guest's L1 directly; no L1[26..27] populated anywhere) — Einstein gets further only because its JIT / `SystemBootUND` stub / ROM-patching workflow happens to avoid that branch in practice.

We've been reaching for patches ("just skip the vector", "just scale the tick rate") rather than fixing the actual boot mechanics. The user's direction: stop the hacks, actually fix what's wrong, and move toward a concrete midterm goal — the TInterpreter constructor at `0x002F40E0`.

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

### Phase A — build every known-required piece as a real handler

By the end of Phase A, every piece of hardware / CPU behaviour required by the 717006 ROM's early-boot path must have a *real* implementation — no per-opcode patches, no stubs. "Real" here means: when the guest executes a SWP, takes an UND exception, does an MCR to CP10, or touches a serial-chip register, our hypervisor routes that access to a proper EL2 handler (or a properly-modelled MMIO device) that does the correct thing. Unknown sub-cases return a loud error, not a silent stub value.

Each item lands as its own commit + its own `guest-tests/tests/test_<name>.S`. If a test fails we fix it against the test, not the ROM. The ROM is not touched in Phase A.

1. **Fine-table (0b11) L1 descriptor rewrite.** `HIGHLEVEL.md` §5.4 + `probe/FINDINGS.md`. The 717006 kernel installs three L1 fine-table descriptors (VA `0x78000000` / `0x90000000` / `0xAC000000`) that ARMv7 doesn't walk. Extend `guest_mem::fix_stage1_xn_bits` to rewrite type `0b11` → `0b00` (fault) in the guest's L1, so touching those VAs raises a proper stage-1 translation fault that our abort handler can see rather than looping in undefined walker behaviour. Guest test: synthesise an L1 with a fine descriptor, run the fix, verify the entry was rewritten and a subsequent stage-1 walk for that VA takes a translation fault.

2. **Undefined-instruction handler at EL2 (covers SWP + Einstein's three UND opcodes).** ARMv7 AArch32 has no HCR_EL2 bit that traps UND directly to EL2, so we install a single one-word trampoline at the guest's UND vector (VA `0x00000004` with low vectors; handled generically by writing through the resolved ROM copy at load time) that executes `HVC #<UND_TAG>`. In `trap.rs`, a new `handle_und` is called from the HVC path, reads the faulting instruction from guest memory, decodes it, and dispatches:
   - `SWP/SWPB` (any encoding — not just the one ROM site) → emulate atomically via EL2 load-store-exclusive on the translated PA; write the original memory value back to the destination register; resume at the instruction after the SWP. Correct semantic, no patch at the call site. Performance: ~400 k SWPs × EL2-round-trip is well under a second of real-time overhead; optimise later if a probe run shows it matters.
   - `0xE6000010` (`SystemBootUND`) → Einstein-documented NOP semantic; ELR += 4.
   - `0xE6000510` (`DebuggerUND`) → consume the 4-byte payload word at `ELR`, log it (budgeted), ELR += 8.
   - `0xE6000810` (`TapFileCntlUND`) → same payload-consume shape; ELR += 8.
   - Anything else → log the opcode + guest PC and **halt loudly** ("unrecognised UND — time to implement it").

   Guest test: assembled binary containing a SWP, one of each Einstein opcode, and a known-bogus UND with a sentinel after each; verify the sentinel after SWP reads the swapped value, the Einstein opcodes advance ELR correctly and log their payloads, and the bogus UND halts the test with the expected message.

3. **CP15 StrongARM clock-control quirk.** `FINDINGS.md`: `MCR p15, 0, Rn, c15, c1, 2` fires exactly once at boot. Extend `trap.rs::handle_cp15_trap` to recognise CRn=15 / opc1=0 / CRm=1 / opc2=2 as a no-op (plus a budget-limited log). Guest test: issue the MCR, verify we advance ELR without halting and a repeat issue is silently dropped.

4. **`peripherals/serial.rs` — four TSerialChip implementations.** `docs/peripherals.md` + `TMemoryConsts.h:128–131`. `kExternalSerialBase=0x0F1C_0000`, `kInfraredSerialBase=0x0F1D_0000`, `kBuiltInSerialBase=0x0F1E_0000`, `kModemSerialBase=0x0F1F_0000`. New module with `owns()` covering `0x0F1C_0000..0x0F20_0000`, a real register model per port derived from `Emulator/Serial/TSerialPortManager*.cpp` + `TVoyagerSerialPort.cpp`: status register returns "TX FIFO empty" + "RX empty" so polling loops terminate, TX writes are consumed (and logged up to a budget) as the physical chip would consume them, RX reads return "no data". This is the complete set of behaviours the early-boot probe exercises. Unknown register offsets inside the four windows return zero with a single log per unique offset; unknown accesses outside the windows halt. Guest test: a test binary reads each base's status register, verifies ready bits; writes TX, verifies no crash; reads an unknown offset, verifies the log is emitted once; issues `0xDEAD` to an unmapped MMIO slot, verifies the halt trips.

5. **`peripherals/native_primitives.rs` — CP10/11 EL2 handler.** Enable CP10/11 trapping via `CPTR_EL2.TFP` (and the relevant `HCPTR` equivalents for AArch32) so every AArch32 MCR/MRC/CDP/LDC/STC to coproc 10 or 11 traps to EL2. In `trap.rs`, decode the trap and dispatch to `peripherals::native_primitives::handle_cp10` / `handle_cp11`. These are *real* handlers — they fully decode (coproc, opc1, CRn, CRm, opc2, Rt/Rt2, direction) and match against an encoding table populated from `Emulator/Platform/TNativePrimitives.cpp`. Any encoding we haven't transcribed yet halts with a complete context dump (tuple + guest PC + `r0..r3`). That's not a stub — it's the correct response to a call the hypervisor doesn't know how to service, and it's the signal to extend the table. Known-safe encodings for the earliest boot (log / flush, debug output) are handled at this step so the table isn't empty on day one. Guest test: issue a known-handled encoding, verify the correct side-effect + register result; issue a deliberately-unknown encoding, verify the halt trips with the right context.

6. **`peripherals/screen.rs` — Blit/screen primitive handler.** Screen is driven entirely through native primitives, so step 5 already owns the dispatch; `screen.rs` exposes a `handle_primitive(name, ctx)` that the native-primitives table calls for screen-class encodings. Implementation includes a real `GUEST_FB` copy-in path: read source coords + bitmap pointer, translate through guest stage-1 + our stage-2, copy bytes into the framebuffer region. Unknown screen encodings halt with full context. Guest test: synthesise a call whose encoding classifies as `Screen/Blit`, verify the framebuffer region receives the expected bytes.

By Phase A end, every CPU instruction and every MMIO region touched by the early-boot path has a real handler behind it. "Unknown sub-case" responses are intentional, loud, and act as trip-wires for Phase B.

### Phase B — boot the 717006 ROM and debug failures one at a time

Once Phase A is in, run the Newton ROM under the hypervisor and drive toward TInterpreter. For each stall:

1. Identify the exact PC where the guest is stuck (heartbeat sampler is already in `trap.rs::trap_irq`).
2. Disassemble the ROM at that PC and consult `_Data_/symbols.txt` to name the function.
3. Run the same offset under Einstein (`build/NewtonProbe baremetal/roms/newton.rom - 90`) and compare:
   - What register state does Einstein have there?
   - What does Einstein return for the MMIO / CP15 / native-primitive at that PC?
   - What code path does Einstein take out of this block?
4. Reproduce the gap with a focused guest test that triggers the same trap at a known PC.
5. Fix the hypervisor so the test passes.
6. Re-run ROM, go to next stall.

Concrete checkpoints we should watch for:

- **Reach `BootOS+8`** (already done; we see `cp15.sctlr.mmu_on` at PC `0x18898`).
- **Reach `FlushTheCache` / `FlushTheMMU`** (`0x000188F8`, `0x0001892C`) — already hit; these are mostly CP15 ops.
- **Pass `InitCirrusHW` (main-ROM, `0x000E6C44`)** — this is the next concrete milestone. It isn't the REx `InitCirrusHW`; our current stall on the indirect call to `0x01A6A520` is the fall-through the kernel takes when it doesn't find a better one, and the phase-A work (UND handlers, native primitives) should remove that fall-through.
- **Pass `TDMAManager::Init` (`0x0007CC4C`)** — exercises our DMA port. Likely surfaces bugs in the register model we implemented earlier.
- **Pass `TAppWorld::Init` (`0x00030F54`)** — first time the kernel asks for application-world infrastructure; likely trips TInterruptManager-backed delays.
- **Reach `__main` (`0x0038C89C`)** — C++ runtime static initialisers fire here.
- **Break on `0x002F40E0` (`TInterpreter::TInterpreter`)** — declare midterm victory.

## Critical files

- `src/guest_mem.rs` — ROM load, byteswap, existing fix_stage1_xn_bits. Extend for: fine-table rewrite; install UND-vector trampoline (`HVC #UND_TAG`).
- `src/trap.rs` — CP15 shim (add `c15,1,2` no-op), HVC path (dispatch `UND_TAG` to new `handle_und`), CP10/11 trap hook into `peripherals::native_primitives`.
- `src/guest.rs` — HCR_EL2 setup; add `CPTR_EL2` / `HCPTR` configuration for CP10/11 trapping.
- `src/peripherals/serial.rs` — new module.
- `src/peripherals/native_primitives.rs` — new module (real handler with an encoding table).
- `src/peripherals/screen.rs` — new module (real blit copy-in, registered into the native-primitives table).
- `src/peripherals/mod.rs` — expose new modules.
- `src/mmio.rs` — route `0x0F1C_0000..0x0F20_0000` to `peripherals::serial`.
- `guest-tests/tests/test_<name>.S` — one per Phase A item.
- `guest-tests/tests/MANIFEST` — append new test names.

## Verification

Each Phase A commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All tests pass; the new test asserts specific behaviour of its feature.

End of Phase A:

```
cd baremetal && timeout 30 cargo run --release
```

Hypervisor boots, trap log shows reach into main-ROM `InitCirrusHW` at PC `0x000E6C44`.

End of Phase B / midterm goal:

```
cd baremetal && timeout 60 cargo run --release
```

Trap log shows guest PC sampled at or near `0x002F40E0` — the TInterpreter constructor. If we pass that point once, we've cleared the pre-scheduler stretch.

Einstein as reference:

```
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom - 30
```

Captures CP15 / SWP / mode-transition counts that we can compare against what our hypervisor sees at each stall.

## Explicit non-goals for this plan

- Exhaustively implementing every TNativePrimitives encoding upfront — only the encodings the early-boot path can realistically hit get transcribed in Phase A; others are discovered (with a loud halt) during Phase B and transcribed then. The handler itself is real; the table grows as evidence demands.
- Real screen emulation beyond a framebuffer dump — no compositor, no pen input.
- Any work past TInterpreter — scheduler, app world, package loading — waits for the next milestone.
- Reviving or rewriting the vector / CP15 ROM patches we already have. They stay until Phase A's UND handler genuinely replaces the need for the undef-vector patch — then that patch comes off, and we test ROM boot again to see whether the prefetch-abort / data-abort vector patches can follow.
