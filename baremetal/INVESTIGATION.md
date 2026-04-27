# Current-stop handoff

Live notes for the next iteration. Replace this file's body when the
current stop is fixed and a new one takes over — git history is the
archive of past investigations.

## No active stop. Steady-state idle reached (2026-04-27).

A 90 s cold boot with `cargo run --release` (default features, no
`trace*`) reaches the idle pause loop and stays there cleanly. The
last stop — `Swap(NULL, 1)` at ROM `0x3ae204` — was resolved by
mirroring Einstein's `TMemory::WriteP` silent-drop for the ROM
aperture; see the resolved-stops table in `PLAN.md`.

## What "steady-state idle" actually means here

It's the **kernel's** idle pause loop, not the user-facing idle:

- `idle` task RUN at prio 0
- `newt` task RDY at prio 10, queued on `q=0x00000000/0x0c116ed8`
  (some functions/wait queue, not the run queue)
- everything else BLK
- timer IRQ + beacon trap cycle through PCs `0x800a0c` /
  `0x3adb0c` / `0x3ad6f4`

`peripherals/screen.rs::blit` never fires, so `/tmp/newton-fb/`
stays empty. Cross-checked against the existing pre-fix
`trace_once` log at `/tmp/run-trace-once.log` (1477 unique first-
calls, 4147578 total trace events, ending at the SWP-NULL stop):

```sh
awk '/^trace / && !seen[$4]++' /tmp/run-trace-once.log \
  | grep -iE "Screen|Blit|Display|TPlatform|TBits"
```

returns `TPlatformDriver::Init`, `PowerOffSubsystem`,
`PowerOnSubsystem`, `RegisterPowerSwitchInterrupt`,
`EnableSysPowerInterrupt`, `ResetZAPStoreCheck` — but no
`TScreenDriver::*`, `TMainDisplay*`, or `TBlit*`. The display driver
was never instantiated before the wedge, and post-fix the kernel
quiesces on the same path without ever getting there.

## Pending follow-ups

### Wake `newt` (next milestone)

`newt` is RDY (prio 10) but never scheduled. `idle` (prio 0) keeps
the CPU. The `q=0x00000000/0x0c116ed8` link suggests `newt` is
waiting on a kernel-side queue / port / semaphore that no one is
posting to. Trace tail before quiesce shows the kernel cycling
through `0x3ad6f4` / `0x3adb0c` (idle pause helpers) and
`0x800a0c` (a REx loop) — find the queue / event the kernel is
spinning on, and figure out who is supposed to post to it.

Once `newt` runs, `TScreenDriver::*` should follow on the
display-init path, `peripherals/screen.rs::blit` will start firing,
and `/tmp/newton-fb/` will populate.

### Feed an input (after `newt` wakes)

PLAN's stated goal is "drive forward until the boot quiesces in a
steady-state idle that **responds to** whatever tablet / serial /
network inputs we choose to feed it." Tablet is the lightest-touch
entry point — `peripherals/tablet.rs` already produces stylus-down
/ up events, and the kernel's `pckm` task is BLK on the tablet
port. Wiring a synthetic tap should exercise the dispatch path
once the scheduler is letting `newt` run.

## Resolved stop log (this session)

### `Swap(NULL, 1)` ⇒ stage-2 perm fault on ROM-aperture write (2026-04-27)

Symptom — cold boot halted with:

```
*** data abort ISV=0 at ELR=0x95c444 SPSR=0x20000110
    IPA=0 FAR=0 iss=0x4e
```

`iss=0x4e` ⇒ `WnR=1`, `DFSC=0xe` (stage-2 permission fault, level 2),
guest VA = 0.

The misleading initial read of the trace tail was that PC `0x95c444`
looked like an Einstein.rex offset (REx base `0x00800000`,
offset `0x15c444`). REx is only 0x46c50 bytes, so that offset is well
past the loaded image. The actual answer:

`0x95c444` lives inside the **tracer trampoline pool**
(`0x00900000..0x00E00000`, `src/tracer.rs`). The pool is a flat array
of 5-word slots; `0x95c444 - 0x900000 = 0x5c444 = 18 896 × 20 + 4`,
so the PC is at `slot[1]` (offset +4) of slot index 18 896. Slot
index 18 896 of `scripts/classify-out/code-symbols.txt` resolves to
function **`Swap`** at ROM `0x003ae204`, whose body is one instruction:

```
003ae204 <Swap>:
  3ae204:  e1000091   swp r0, r1, [r0]
```

`Swap` is the kernel's atomic-exchange primitive. It's reached via
`Acquire(TULockingSemaphore*, SemFlags)` (ROM `0x1bce754` →
`0x55b1c`'s `TCardSocket::VccOff` etc.). The trace tail before the
abort:

```
trace 4147559 0x00050d18 VccOff(int)              (usr) ...
trace 4147560 0x00050d28 VccOff(int, unsigned long) (usr) ...
*** data abort ISV=0 at ELR=0x95c444 ...
```

— bare-function `VccOff__Fi`/`VccOff__FiUl` (NOT
`TCardSocket::VccOff`). Inside `VccOff__FiUl` (ROM `0x50d28` —
disassembled in `scripts/disasm-out/rom.dis`) is a chain that
indexes `gPowerSemaphore[arg0]` (`g 0x0c105f54`) and passes it to
`Acquire`. On the failing path that table entry is NULL, so
`Acquire(NULL)` reaches `Swap` with `r0 = 0`. The SWP then tries to
write to VA = 0; stage-1 identity-maps to IPA = 0; stage-2 has the
ROM aperture mapped RO, so we take a stage-2 perm fault.

Einstein oracle — `Emulator/TMemory.cpp:1755-1766`:

```cpp
TMemory::WriteP(PAddr inAddress, KUInt32 inWord) {
    if (inAddress < TMemoryConsts::kRAMStart) {
        if (inAddress < TMemoryConsts::kHighROMEnd) {
            if (mLog) mLog->FLogLine(
                "Ignored write word access to ROM at P0x%.8X (%.8X)",
                ...);
            // FALL THROUGH — no fault, no write.
        }
        ...
    }
}
```

Writes to anywhere `< kHighROMEnd` (0x01000000) are silently dropped.
For SWP the read-side still runs (`TJITGeneric_SingleDataSwap_template.h`
calls `Read` then `Write`), so `r0` ends up with `ROM[0]` (the reset
vector word `0xea0061a0`) and the kernel's spin-loop sees a non-zero
value.

Fix — `src/trap.rs::try_absorb_rom_write` (called from the ISV=0 arm
of `handle_data_abort`):

- Bail unless the IPA is in the ROM aperture (`< 0x01000000`).
- Read the faulting instruction at ELR (via stage-1 if up, else PA-
  direct so the path works for the early-boot / guest-test case).
- For SWP/SWPB (`(insn & 0x0FB0_0FF0) == 0x0100_0090`): set Rd to
  `ROM[ipa]` (word or byte), drop the store, advance ELR.
- Anything else falls through to the loud halt — the absorber is
  intentionally narrow so the next novel write to ROM stays loud.

Verification:

- 90 s cold boot reaches steady-state idle (no halts; `idle` task
  RUN, `newt` task RDY, beacons cycle through ELR=`0x800a0c` /
  `0x3adb0c` / `0x3ad6f4`).
- All 36 guest tests pass (`baremetal/guest-tests/scripts/run-all.sh`).
- New regression test `guest-tests/tests/test_swp_rom_aperture.S`
  exercises word SWP, the kernel's exact `swp r0, r1, [r0]` alias
  pattern (encoded as `.word 0xe1000091` because gas rejects
  `Rn==Rd`), byte SWPB, and a non-zero ROM-aperture address.
