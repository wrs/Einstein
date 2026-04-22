# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at (2026-04-22, post-trace-rewrite)

**First guest abort is a PABT with fault PC = 0x0100017C, from SVC
mode, right after trace 244 (`FlushTheMMU`).** The guest's stage-1
has `L1[0x10] = 0 (fault)` (that's the 1 MiB range 0x01000000..
0x01100000), so any fetch there raises a guest PABT. The kernel has
not yet installed its own exception vectors, so the PABT goes to the
ROM's default vector at VA 0x0C, which branches to `0x01A00010` —
also unmapped (that's the HAL-REx PrefetchAbortHandler on real
MP2100 hardware, backed by a patch REx our image doesn't carry).

Captured via patching VA 0x0C to `HVC #DIAG_TAG` (same pattern as the
DABT-vector intercept at 0x10) and reading LR_abt from the banked-reg
dump stub:

```
LR_abt    = 0x01000180   (PABT sets LR_abt = fault_PC + 4, ARM)
SPSR_abt  = 0x800001D3   (pre-PABT mode = SVC, N flag, I=F=A=1)
SP_abt    = 0x001191BD   (uninitialised — SP_abt never set by kernel)
LR_svc    = 0x00000000   (pre-PABT SVC LR was zero — so the jump to
                          0x0100017C was NOT via BL; candidates are
                          `MOV PC, Rn`, `BX Rn`, `LDR PC, [Rn, #imm]`)
SP_svc    = 0x0C004C00
guest r0..r12 at PABT:
   r0 =0x00000000 r1 =0x0c100800 r2 =0x0c106528 r3 =0x0c100800
   r4 =0x00000040 r5 =0x0c1061c4 r6 =0x00000000 r7 =0x0400d1c4
   r8 =0x04000000 r9 =0x00400000 r10=0x4401a100 r11=0x0c0003fc
   r12=0x0c004f00
```

`r10 = 0x4401A100` is suspiciously close to StrongARM SA-1100's CPU
ID (`0x4401A10x`) — probably the result of a `MRC p15,0,Rt,c0,c0,0`
masked to the top 28 bits. Our Cortex-A53 MIDR_EL1 is `0x410FD034`,
not a StrongARM ID, so the kernel's CPU-dispatch logic is taking a
branch keyed on an unexpected MIDR value. No hits for the literals
`0x0100017C` / `0x01000180` as B/BL targets in the ROM, so the call
is computed, not compiled-in.

Trace tail before the PABT (244 function entries deep):

```
...
239 AddPgPAndPermWithPageTable(r0=0x04000000, r1=0x0c107000,
                               r2=0xff, r3=0x0400e000)
240 CleanPageInDcache        (r0=0x0c107000)
241 LoadFromPhysAddress      (r0=0x04000304)
242 StoreToPhysAddress       (r0=0x0400681c, r1=0x0400effe)
243 PurgePageFromTLB         (r0=0x0c107000)
244 FlushTheMMU              (r0=0x00000000)
        <PABT here>
```

The cycle 221..244 is `MapTable(3, 0)` walking RAM-area page-table
entries. After `FlushTheMMU` returns, whatever the caller of
`MapTable(3, 0)` does next branches PC to 0x0100017C. That branch is
the first thing to root-cause.

### Einstein vs. us — concrete page-table state

Einstein's `probe/results-717006-30s.txt` shows, *after* 30 s of boot:

```
VA 0x00000000 to 0x00100000 (1024 kB): large pages   ← identity ROM
VA 0x00100000 to 0x01000000 (15360 kB): section      ← rest of ROM
VA 0x01000000 to 0x01800000 (8192 kB): fault         ← our fault range!
VA 0x01800000 to 0x01810000 (64 kB): small pages
VA 0x01900000 to 0x01A00000 (1024 kB): fault
VA 0x01A00000 to 0x01C20000 (2176 kB): small pages   ← ROM Jump Tables
```

Key facts:
1. **The ROM bytes at VAs 0x04/0x08/0x0C/0x18/0x1C really are `B 0x01A00xxx`.** Einstein runs the same ROM with the same bytes. The REx targets resolve via the "ROM Jump Tables" stage-1 mapping that `UseROMJumpTables()` (0x001832E8) installs early in boot. That mapping is what makes the stock ROM vectors work.
2. **`UseROMJumpTables` has not yet fired in our 244-trace boot.** In the older 72-trace boot (documented below) it fired at trace ~25; in the current boot the same kernel path is reaching trace 244 deep in `MapTable(3, 0)` without ever having called it. That's an ordering difference between our boot and Einstein's.
3. **Einstein ALSO leaves 0x01000000..0x01800000 as fault** — so a guest fetch at 0x0100017C would PABT in Einstein too. The fact that Einstein logs zero real kernel-mode aborts means Einstein's kernel simply doesn't compute PC = 0x0100017C on this code path. Something in our execution state is making the kernel branch there.

### What's verified vs. unverified

Verified:
- The PABT fires at fault_PC = 0x0100017C, SVC source mode.
- LR_svc = 0 at PABT entry (so the branch was not via BL).
- `UseROMJumpTables` has not been called yet.
- `r10 = 0x4401A100` at PABT — close but not identical to SA-1100
  MIDR; the kernel derives this from CP15 `c0 c0 0` via BIC+EOR+EOR.

Unverified (next-session hypotheses, don't act on without data):
- That the branch target 0x0100017C came from MIDR-based dispatch.
- That porting the remaining `TJITGenericPatchNativeCall` /
  `TVirtualizedCallsPatches` entries would prevent the branch.

### Next investigation step

Instrument the single guest instruction that computes the branch.
Options, in order of simplicity:

1. `bp 0x00018948` (the `MOV PC, LR` at FlushTheMMU's return). Single
   step ISN'T available through QEMU's AArch64 gdbstub for a 32-bit
   guest, so you'd need a cascade of `bp`s at plausible PCs following
   the return. But R14_svc at `FlushTheMMU` entry tells you the
   immediate return target — dump it via a modified `handle_trace_hvc`.
2. Add a `bp 0x0011F0F0` (MapTable entry) to catch each iteration and
   dump R14_svc — the caller's PC trail will narrow down who's about
   to branch astray.
3. Re-enable the `handle_diag_from_bp` path (kept in `guest_bp.rs`)
   and `bp 0x0100017C` itself; when vectors PABT there the guest-BP
   logic doesn't help since 0x0100017C is unmapped — you need `bp`
   at the *branching* instruction, which we don't yet know.

## Historical — 72-function stall (pre-trace-rewrite)

With the function-tracing feature and the UND-trampoline R0/R1 fix
below, the boot now runs deterministically **72 kernel-internal
functions deep** before the first fatal stall. Trace order:

```
 1..10  FlushTheCache → HandleDebugCard → InitSpecialStacks → ...
        → InitCGlobals
11..24  InitKernelHeapArea → PrimSetDomainRangeWithPageTable
        → AddPgPAndPermWithPageTable → CleanPageInDcache
        → FlushTheMMU
25..40  SetGlobalsInitialized → UseROMJumpTables
        → BuildPatchTablePageTable → FPE_Install
        → QueryMemoryReservation → ReserveContiguousMemory
        → FindHighROMProtocol → EarlyBootGetTempPage
        → TNoReuseAllocator::Allocate → TClassInfo::MakeAt
41..54  TNewInternalFlash::InitForReservedBlock
        → InitializeState → SearchForFlashDrivers
        → ConfigureFlashBank → SetBankControlRegister
55..61  CheckFor4LaneFlash → FindDriverAble
        → T28F016_SA_SVDriver::Identify
        → ConfigureNot32BitFlashBank
        → CheckFor2LaneFlash → CheckFor1LaneFlash
62      PowerOffAndReboot     ← kernel bails
63..70  IOPowerOffAll → GetPlatformDriver → DisableAllInterrupts
        → GetGPIOInterfaceObject → TGPIOInterface::Init
        → RegisterPowerSwitchInterrupt → PowerOffSystem
71      SWIBoot               ← "SWI from non-user mode (rebooting)"
72      ROMBoot               ← cold reboot cycle begins
```

Terminal condition: `TNewInternalFlash` tries all four (4-lane,
2-lane, 1-lane variants) and can't identify a flash chip. The
kernel gives up and calls `PowerOffAndReboot(long)`, which walks
IOPowerOffAll / DisableAllInterrupts / PowerOffSystem, then
triggers the soft-reset via an SWI. The SWI handler's first check
is `SPSR_svc.mode == USR` — it's not (we're kernel-internal) so it
hits `DebuggerUND "SWI from non-user mode (rebooting)"`. That
panic is the SAME one we'd been parking earlier; it turns out to
be the tail of the PowerOffAndReboot path, not an independent
issue.

## Root cause (2026-04-22, via early-patched tracer)

**The bug: `MemoryTest` leaves `gGlobalsThatLiveAcrossReboot + 0x20` as
the 0xb6db6db6 RAM-test poison pattern, and `RExScanner` reads the high
16 bits of that field to decide whether to scan at base 0x0071FC4C or
0x00B1FC4C (= base + 0x400000). On our platform post-MemoryTest
hi16 = 0xb6db ≠ 0, so the scanner looks at 0x00B1FC4C — which has no
RExBlock magic — and `REx[0..3]` never get populated.**

Evidence: installing three pre-MMU UDF trace patches at `RExScanner`
(0x313888), `ScanForREx` (0x313818), and `TestForREx` (0x3137dc)
immediately at ROM-load time (before stage-2 enables), then dumping
r0-r4 on the first fire of each:

```
trace 1 RExScanner   r0=0x0400d1c4 r1=0x00400004 r2=0x4 r3=0xe038 r4=0x0400d1c4
trace 2 ScanForREx   r0=0x0400d1c4 r1=0x00b1fc4c r2=0x0400d4dc r3=0xe038 r4=0x0400d1c4
trace 3 TestForREx   r0=0x00b1fc4c r1=0x00b1fc4c r2=0x0400d4dc r3=0xe038 r4=0x0400d1c4
```

`r1` is the cursor that `ScanForREx` was asked to probe — 0x00b1fc4c
instead of the 0x0071fc4c literal in RExScanner's pool. The scan
therefore reads the zero-filled trailing ROM at 0x00b1fc4c, finds no
magic, returns without populating REx[].

RExScanner's code (host PA 0x023138c8..0x023138d8, read over gdb
`monitor xp`):

```
0x3138c8: ldr r1, [pc, #0x4c]   r1 = literal 0x0071FC4C
0x3138cc: ldr r0, [r4, #0x20]   r0 = gGlobals[0x20]
0x3138d0: lsrs r0, r0, #0x10    r0 >>= 16; set flags
0x3138d4: addne r1, r1, #0x400000  if hi16(gGlobals[0x20]) != 0: r1 += 4 MiB
0x3138d8: mov r0, r4
0x3138dc: bl ScanForREx         call(globals, r1)
```

Fix: zero `*(r0 + 0x20)` on every `RExScanner` entry. With that one
word cleared, `ScanForREx` is called with base 0x71FC4C and populates
`REx[0]=0x71FC4C` (embedded). `RExScanner`'s conditional second call
then scans at 0x800000 and populates `REx[1]=0x800000` (the external
Einstein REx). Confirmed at `SearchForFlashDrivers` entry:

```
REx[0] at VA 0xc1064ac → PA 0x0400d4ac → 0x0071FC4C
REx[1] at VA 0xc1064b0 → PA 0x0400d4b0 → 0x00800000
```

So `PrimNextRExConfigEntry` now returns the Einstein FDRV entries and
the kernel no longer falls through to `T28F016_SA_SVDriver::Sizeof` /
`::Init` / `::Identify`. The T28F016 traces are gone from the 72-deep
boot.

### Still stalling past the REx fix

With REx registered correctly, boot still ends at `PowerOffAndReboot`
(trace 67) via `CheckFor{4,2,1}LaneFlash` → `FindDriverAble` failing.
The Einstein FDRV driver is findable but its `Identify` native-primitive
isn't getting invoked — probably a separate issue in how
`TNewInternalFlash::FindDriverAble` walks the driver classes we now
registered. That's the next thing to pin down (likely involves setting
up `peripherals::flash_driver::identify`'s native-primitive dispatch
path and/or more of the fdrv class-info struct). The REx-visibility
blocker is clear.

### Why Einstein doesn't hit this

Einstein emulates the Newton the same way but presents RAM differently:
its `TMemory` zeros the backing on allocate or the RAM-test fill
doesn't reach `gGlobals[0x20]` before `RExScanner` reads it.
Alternatively Einstein's emulated MemoryTest pass follows a different
code path that leaves the first page of the globals struct cleared.
Either way, Einstein's `gGlobals[0x20]` is 0 when `RExScanner` fires,
so the scanner takes the 0x71FC4C branch.

## Earlier root-cause attempts (2026-04-22, superseded)

Using `aarch64-elf-gdb` against QEMU's `-s` stub plus a one-shot
diagnostic in `tracer::handle_trace_und` that dumps guest RAM at
`SearchForFlashDrivers` entry:

- Our external `Einstein.rex` IS loaded correctly at guest PA
  `0x00800000`: magic `RExBlock`, manufacturer `Eins`, id=1 (patched
  from 2 by our loader), startAddr=`0x00800000`, numEntries=3
  (entries `fdrv` / `FDRV` / `pkgl`). The ROM-backing bytes match
  what `xxd` on the file shows.
- The kernel's REx base table (three parallel arrays inside
  `gGlobalsThatLiveAcrossReboot` at guest VA `0x0c1061c4`) is
  **all zero** when `SearchForFlashDrivers` runs. Specifically:
  - Table A at `gGlobals+0x2e8+id*4` (REx startAddr per id) = 0
  - Table B at `gGlobals+0x2fc+id*4` (REx ROM pointer per id)  = 0
  - Table C at `gGlobals+0x30c+id*4` (REx size per id)         = 0
- `PrimNextRExConfigEntry` (0x11ee60) first loads Table A[id] and
  returns immediately if the slot is zero. So every
  `PrimNextRExConfigEntry` call returns no entry, the flash-driver
  search never sees any `fdrv` record, and the kernel falls through
  to the built-in `T28F016_SA_SVDriver` which then fails `Identify`.
- The function that SHOULD have populated the REx tables,
  `RExScanner` (0x313888), is NOT called on first boot in our setup
  — it doesn't appear in the 72-deep trace. On *re-boot* (after
  PowerOffAndReboot → SWIBoot → ROMBoot), the trace DOES show it
  (traces 81–83 in a `trace,quiet` run): `RExScanner → ScanForREx →
  TestForREx`. So the reboot code path runs the scan but the first-
  boot code path (which we're stuck in) does not.

RExScanner's logic, from the ROM bytes read over gdb `monitor xp`:
  1. Clear all three tables for id=0..3.
  2. Load base cursor from a literal pool entry = `0x0071FC4C`
     (i.e., the embedded-REx base for 717006).
  3. Adjust base by +0x40000 if `gGlobals[0x20] >> 16 != 0`.
  4. Call `ScanForREx(globals, 0x0071FC4C)` → walks contiguous
     `RExBlock` magic from the base, registering each REx it finds.
  5. If the returned cursor is still `< 0x00800000`, call
     `ScanForREx(globals, 0x00800000)` — scans our external REx
     aperture.
  6. Call `ScanForREx(globals, 0x10000000)` — scans flash bank 2.

So the kernel DOES know about our `0x00800000` aperture; step 5 is
the explicit fallback Einstein also uses in its host-side
`LookForREXes`. We just never get there because step 1 isn't being
triggered on the first-boot code path.

A test patch that writes into all three REx tables at
`SearchForFlashDrivers` entry (id=0 → embedded REx, id=1 → external)
changes the subsequent trace: `T28F016_SA_SVDriver::Sizeof/Init`
vanishes, meaning the kernel did find a driver class via
`PrimNextRExConfigEntry`. But `TEinsteinFlashDriver::Identify` also
isn't called, and boot still falls through to CheckFor{4,2,1}Lane
and ends up in `PowerOffAndReboot`. So something else downstream
(class-info entry format, driver registration chain) also needs
setup beyond the three-table write. The REx-table population is
necessary but not sufficient. **Next:** figure out what callers of
`RExScanner` the first-boot path takes instead, by pre-patching a
UDF at `0x313888` *before* the tracer's usual MMU-rising-edge trigger
so we can see whether it's called at all pre-MMU-on.

Alternative Phase-B unblock ideas if the first-boot REx registration
is too hard to fix directly:

- Patch ROM to make the first-boot path also call `RExScanner`
  before `SearchForFlashDrivers` runs.
- Supply the REx tables from the hypervisor side right after
  `SetGlobalsInitialized` (trace 25) and before the TNewInternalFlash
  init chain (trace 41+), which is the natural-feeling injection
  point since the kernel isn't mid-flow at that moment.

## What's been resolved since this file was last written

## What's been resolved since this file was last written

### 1. First hard stall: post-MMU-on DABT at FAR=0x0100018B

**Root cause**: `MCR p15, 0, r0, c7, c7, 0` at PC `0x18924` inside
FlushTheCache. This ARMv4 "invalidate unified cache" encoding is
UNDEFINED on ARMv7+/A53. Einstein silently treats the deprecated
encoding as a no-op; we UND'd on it. The UND then cascaded through
three layers:

1. **Primary trigger**: the `MCR c7 c7 0` UND at `0x18924`. Fires
   unconditionally on A53 — the MIDR-based conditional above in
   FlushTheCache branches us straight to the deprecated-encoding
   epilogue.
2. **UND trampoline depended on SP_und**: original trampoline
   started with `push {r0, r1}` on SP_und. SP_und is not initialised
   until `SetUpStacks` runs at `0x11EFD4`, which is *after* the MCR
   at `0x18924`. The push therefore wrote through an uninitialised
   banked SP, producing the alignment fault with `FAR = 0x0100018B`
   that masked the real cause.
3. **UND save slot overlapped the guest L1 table**: the old slot at
   IPA `0x04000400` is inside TTBR0 (the kernel puts L1 at PA
   `0x04000000`). Any trampoline write there would have corrupted
   the page table.

**Fixes applied** (see commit `5fddb693`):

- `handle_und` recognises `MCR c7 c7 0` and emulates as `IC IALLUIS
  + DSB ISH` (the A53 equivalent of "invalidate unified cache inner-
  shareable"), then advances ELR by 4.
- UND trampoline rewritten to four instructions that write LR / SPSR
  via a PC-relative literal pointer — no stack usage, independent
  of SP_und state.
- UND save slots moved from IPA `0x04000400` to `0x04005F00` (the
  RAM-mirror window the DIAG stub also uses). `test_und_handler.S`
  and `test_cp15_strongarm_clock.S` updated to match.

### 2. DebuggerUND over-advanced PC

`DebuggerUND` (0xE6000510) is followed by a **null-terminated ASCII
message** padded to the next 4-byte boundary, not a single
4-byte payload. Our handler used to advance PC by 8; after the
first DebuggerUND the guest ended up re-faulting mid-string on a
random "instruction" (we saw `insn=0x2d757365` — the bytes "-use"
from the middle of "non-user mode.").

Fix (commit `5fddb693`): `scan_to_null_word_aligned` walks forward
word-by-word looking for a null byte; `log_debugger_und` logs the
string and `return_to_guest` resumes past the aligned end of the
message. Einstein's `TEmulator::DebuggerUND` does the same thing
byte-by-byte.

### 3. `fix_stage1_xn_bits` missed late L2 populations

`fix_stage1_xn_bits` only ran on the first TTBR write. The kernel
populates additional L1 coarse entries *between* TTBR write and
the M=1 SCTLR write, and again during task-switch sequences that
toggle SCTLR.M. ARMv4 small-page descriptors use bits[11:4] as four
subpage AP fields; ARMv7+ reinterprets bit 9 as AP[2] and bits[5:4]
as AP[1:0]. Unrewritten entries like `0x04007F0E` read as AP[2:0] =
0b100 (reserved → no-access) on A53, causing permission faults on
kernel writes into its own globals at `0x0C100800`.

Fix (commit `5fddb693`): run the rewrite on every M=0→M=1 rising
edge of SCTLR, not only on TTBR write. Gated on the rising edge
specifically because the kernel toggles MMU ~20k times/minute
during task switching, and the walk is ~3k L2 entries per pass;
any single pass is idempotent.

### 4. Tick-register polling dominated trap time (~75% of all traps)

K_HDWR_TICKS at IPA `0x0F181800` is the Newton 3.6864 MHz tick
register. The ROM's busy-wait delay loops at `0x19FCC` and
`0x18F38` LDR it every iteration — each load was a full stage-2
trap, ~5 µs host-time round-trip. 30 s of wall-clock boot
produced 5 M traps, ~85% at those two PCs.

Fix (commit `09da2f3c` / `ebd7352b`): split the 2 MiB stage-2 L2
block at `0x0F000000..0x0F200000` into a new L3 table
(`S2_L3_HW_TICKS`); install one valid L3 entry mapping a 4 KiB
RAM-backed `TickPage` at IPA `0x0F181000` (RO, Normal WB); have
the CNTHP IRQ handler write `vic::ticks()` into `TickPage[0x800]`
on each fire with a `DSB ISH` before return. Tightened the no-VIC-
pending CNTHP heartbeat from 100 ms to 1 ms so the guest sees a
fresh tick value at least every millisecond of wall time.

Throughput impact (90s cold boot of 717006):

| | before | after |
|---|---|---|
| total traps  | 16.77 M | **1.23 M** (13.6× fewer) |
| delay-loop   | 85% of all traps | zero hotspot |
| max PC       | 0xE6B94 | 0xE6B94 (same — next cliff is the SWI-mode panic above) |

Writes to any register in the tick page still stage-2-fault (RO),
so `vic::write` is still reached for the registers the guest
occasionally writes (calendar, alarm).

### 5. UND trampoline clobbered R0 / R1 (tracer transparency bug)

With function-level tracing wired up (`cargo run --features trace,quiet`
— embeds 31k UDF patches across ROM function entries, restores the
original word on first-touch), the trace stalled at trace 22 on
a DABT to IPA `0x00000078` from `StoreToPhysAddress`. The write
value (`0x04007c0e` — an ARMv7 section descriptor) plus the
register dump made the call chain obvious:

```
dabt-trip: PC=0x00018d10 mode=svc writing 0x04007c0e -> IPA=0x78
           r0=0x00000078 r1=0x04007c0e r4=0x0011e7c4 r7=0x0c004f00
           r5=0x04007000 r11=0x0c000378 sp=0x0c004f00 lr=0x00000000
```

`r7 == 0x0c004f00` (the UND-trampoline save-slot VA) and
`r4 == 0x0011e7c4` (= LR_svc inside MapInKernelGlobals, which the
SVC-bounce stashed as `R1`) were the clobber values surviving into
the traced function's prologue. `AddPgPAndPermWithPageTable`'s first
five instructions are

```
0x15a828  MOV  R7, R0    ; R7 <- pgP (arg1)
0x15a82c  MOV  R4, R1    ; R4 <- va  (arg2)
0x15a830  MOV  R6, R2
0x15a834  MOV  R5, R3
0x15a838  LDR  R0, [R11, #4]  ; arg5
```

— so the trampoline's R0/R1 clobber shuffled garbage straight into
the page-table-walk state. The L2 base from the bogus R7/R4 was
zero, which is why the subsequent `StoreToPhysAddress(addr, value)`
landed on IPA `0x78` (entry index 0x1E × 4 = 0x78 from base 0).

Fix (this change): rewrite the trampoline to clobber R12 only
(APCS-scratch; every Newton 2.x kernel function observed starts
with `MOV R12, R13` so it's effectively scratch at function-entry
UDF sites). Persist the pre-UND R0 and R1 into new RAM slots at
`UND_SAVE_R0_IPA = 0x0400_5F0C` / `UND_SAVE_R1_IPA = 0x0400_5F10`
before the SPSR/LR_svc writes. `handle_und` restores `ctx.x[0]` and
`ctx.x[1]` from those slots at entry so the ERET back to the
guest resumes with intact argument registers.

With the fix the trace walks 72 functions deep before the flash-
identify failure; no other regressions surfaced in the 14 guest
tests.

### 6. Data-abort halt enriched with caller context

`src/trap.rs::handle_data_abort` now prints the full AArch32
register context when an obviously-unreachable IPA (currently any
IPA in `0..0x0100_0000` — i.e., stage-2 read-only ROM) is
targeted by a write, before falling through to the MMIO halt.
Covers the common "MCR-then-STR inside a small helper far from
where the bad address was computed" pattern, which made the PA
0x78 cause directly visible in the log.

### 8. ROM debug-logging surfaced properly

The 717006 ROM carries **22 DebuggerUND sites** (15 main ROM + 7
REx) + 1 SystemBootUND + 5 TapFileCntlUND, each with a plain-ASCII
panic message right after the opcode. Our handler decodes them but
had two bugs hiding the signal:

1. **Byte order**: the ROM stores 4-byte words in big-endian byte
   order (Newton was a BE system). `load_rom` byteswaps each word
   so LDR in our LE guest returns the same u32 the BE CPU saw,
   which means the guest-memory bytes for a string are reversed
   per word. `scan_to_null_word_aligned` and `log_debugger_und`
   now iterate `to_be_bytes()`; pre-fix the messages came out as
   `"!toZeG  ireneIWSc..."` instead of `"Zot!  GenericSWI..."`.
2. **Budget-8 cap**: old counter stopped logging after 8 hits.
   Replaced with a per-PC seen-set; each unique site logs exactly
   once, repeat hits at the same site suppress.

Commit `534e3974`.

## Observed state at the current stall

Under the 717006 ROM after all fixes above:

```
beacon top 10 PCs:
  18  ELR=0x18d10     task-switch SCTLR MMU-toggle
  12  ELR=0x18cd4     ^
   9  ELR=0x18d18     ^
   8  ELR=0x18cdc     ^
   4  ELR=0x39451c    ?
   4  ELR=0x19d60     ?
   4  ELR=0x19cac     ?
   3  ELR=0x3ad544    ?
   3  ELR=0x19cd8     ?
   2  ELR=0xe6b94     ?
```

Top traps are task-switch MMU toggles — legitimate kernel work
(the 717006 scheduler reprograms SCTLR on every task switch). No
dominant hot loop. The two DebuggerUND panics fire once each,
after which the kernel "reboots" (the `0x3ad660` message is
literally "SWI from non-user mode (rebooting)" so its handler is
probably driving a reboot sequence). The full trap budget (500
traps logged before the beacon takes over) fills up with an even
mix of CP15 writes and MMIO touches — no regression to a single-
PC spin.

## Diagnostic scaffolding still in place

Kept so the next stall is caught with full context:

- **DIAG HVC patch at VA 0x10** (single-word `hvc #DIAG_TAG`
  in `guest_mem.rs`) — any stage-1 data abort traps to EL2.
- **`handle_diag` + `handle_diag_lr`** two-stage stub in
  `src/trap.rs` with a RAM-based banked-register dump at IPA
  `0x04005F00..0x04005FA7`. Bypasses QEMU raspi3b's flaky
  AArch32→AArch64 banked-LR / SPSR plumbing.
- **500-entry trap log budget** at the top of
  `trap_sync_lower_aarch32` in `src/trap.rs`. After the 500th,
  only the PC-moved beacon prints.
- **`guest_mem::dump_stage1_walk`** — walks L1/L2 for a given VA
  and prints each level. Invoked from `handle_diag` for the
  faulting VA plus a handful of bring-up-critical VAs (SVC stack,
  ABT stack target, RAM window, REx base, etc.).

None of the above needs to come off until we declare Phase B
stable. Specifically the DABT vector patch is the primary
diagnostic entry point and is far cheaper than reading LR_abt via
an AArch32 ERET trick each time.

## Open questions / next hypotheses

1. **SWI-from-non-user-mode panics** — currently top stall. Two
   separate panic sites (`0x3ae188` "GenericSWI..." and `0x3ad660`
   "SWI from non-user mode..."). The kernel's SWI entry reads
   SPSR_svc and checks the pre-SWI mode; the value it sees isn't
   USR. Most-likely-cause: byte-level endianness disagreement on
   a mode-field read or context-save. Parked pending parallel
   endianness work.

2. **Tick-page dirty tracking** — writes to calendar / alarm in
   the tick page still stage-2-fault, which is correct but adds
   a trap each time the guest programs the alarm register. If
   that shows up in a future trap profile, the fix is to map
   the page RW and keep the non-trapping semantic for writes too
   (trusting the backing store to hold the alarm value until we
   need it for CNTHP arming).

3. **`vic::ticks()` overflow at high scale** — we run
   `NEWTON_TICK_HZ = 3_686_400 * 128 = 471 MHz` which makes a
   u32 tick wrap every ~9 seconds wall time. Matches
   `probe/FINDINGS.md` behaviour (the kernel handles wrap), but
   worth revisiting if the wrap ever lands inside a delay-loop
   comparison unexpectedly.

## Reproduction

```bash
rm -f /tmp/newton-snapshot-*.bin
cd baremetal && cargo run --release
```

With the full diagnostic scaffolding in place, the first DABT
(if any) traps into the two-stage DIAG dump. Without a DABT, the
boot just runs to the 90 s timeout with beacons showing guest PC
movement.

All 13 `guest-tests/scripts/run-all.sh` tests pass with the current
state (`rm /tmp/newton-snapshot-*.bin` first or use the cleanup
already baked into `run-test.sh`).
