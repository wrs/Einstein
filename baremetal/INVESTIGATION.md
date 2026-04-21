# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at

The ROM boot runs past MMU-on, past the early CP15 setup, past
FlushTheCache, past the REx platform-probe path, and into deep
initialisation code (beacon PCs span `0x186B8` through `0x0E6B94`
with stops in the `0x3134xx` / `0x3AD5xx` / `0x3945xx` regions).
No hard stall — the guest is making forward progress. The two
most-diagnostic panic messages that keep firing are

```
und: DebuggerUND @PC=0x3ae188 msg="Zot!  GenericSWI called from non-user mode." (resume at PC=0x3ae1b8)
und: DebuggerUND @PC=0x3ad660 msg="SWI from non-user mode (rebooting)" (resume at PC=0x3ad688)
```

Both are ROM-side assertions that a SWI instruction executed while
CPSR.mode != USR. Likely resolved by the in-flight byte-level
endianness work on the parallel track — a byte-swapped CPSR-mode
field (or any register/memory value going through a byte path)
would look exactly like this to the kernel.

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

### 5. ROM debug-logging surfaced properly

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
