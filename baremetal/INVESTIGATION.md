# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at

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
issue. Resolving the **flash-identify failure** is the next
Phase-B cliff: give `T28F016_SA_SVDriver::Identify` a plausible
manufacturer/device ID or emulate the Intel 28F016 command set on
the flash banks.

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
