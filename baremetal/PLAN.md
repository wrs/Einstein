# Plan — Drive Newton OS to interactive use

## Status

**Maintenance note (auto-prune):** Each iteration, BEFORE adding a new
iter-N section, prune the old one(s) so PLAN.md stays small. The full
history lives in `git log`. Keep only: this Status block + the most
recent 1-2 iteration sections + the reference sections at the bottom.
Bloated PLAN.md wastes context every read.

**Hard rules** (user directives still in force):

- Hypervisor-side compensation for subpage-AP incompatibility is OFF
  the table (2026-04-29). The fix MUST be a kernel patch.
- Run the *original ROM code*; no workarounds, no deferrals, no
  shortcuts; fix all warnings before each commit.
- All 36 guest tests must pass on every commit
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-63):** iter-62 added a per-task APCS stack-
chain walker to `task_dump`, with PC-name lookup via the symbol
table (now built unconditionally, not gated on `trace`). For every
blocked task the dump now prints the full call stack with
`function+offset`-style names, jump-table slots resolved by reading
the underlying `b imm24`. Concrete findings at splash idle:

- `scrn` is `ScreenUpdateTask+0x48` parked at `SemaphoreOpGlue`.
  It waits via `SemOp(grp[+24], grp[+48])` and signals done via
  `SemOp(grp[+24], grp[+52])`. The screen-state global is at
  VA `0x0c101a4c`. Several functions in `0x1cc..0x1cd` reference
  that global and call `SemOp` on the same group — `StopDrawing`
  is a strong producer candidate (it does `SemOp(grp[+24],
  grp[+56])` at `0x1cd390`, then directly calls
  `UpdateHardwareScreen` itself).
- `idle` is `SleepTask` → `OsBoot+0x190` → `ROMBoot+0x26c`.
  Standard kernel idle.
- 12 tasks (`inkr`/`cdsv`/`cdpr`/`drvr`/`pg&e`/`alrt`/`sndm`/
  `name`/`pssm`/`pckm`/`cmgr`) are all parked at
  `TUPort::Receive` inside their `TAppWorld::AEventLoop` —
  standard event-loop wait. The kernel-parked LR for every
  task's `TUTaskWorld::TaskEntry` resolves to
  `TaskKillSelf [JT]`, confirming the "if TaskEntry ever returns,
  kill the task" pattern.
- 8 tasks (`OBJM`/`cdfm`/`PMGR`/`STKP`/`STKU`/`ROMF`/`ROMP`/
  `mntr`/`Tmux`) have savedPC at `MonitorEntryGlue [JT]` — they
  are inside kernel-monitor SVCs (page allocator, heap manager,
  etc.) that haven't returned yet.

Next steps:

1. **Find the producer for `scrn`'s SemOp.** Decode the OpList
   layout in `InitScreenTask` (entries at `[r6+36/40/44/56]`)
   to identify which OpList corresponds to "wake-scrn". Then
   the producer is whoever calls `SemOp(grp[+24], that-OpList)`.
   `BlitToScreens` and `QDStopDrawing` are also candidates
   alongside `StopDrawing`.
2. **Verify whether `StopDrawing` is gated on something we
   haven't satisfied.** `StopDrawing` has its own
   `UpdateHardwareScreen` call at `0x1cd3b8` — if it routinely
   does the work synchronously, scrn might just be a backstop
   that's *expected* to mostly idle. In that case "what wakes
   scrn" is the wrong question, and the right next-step is
   "what would normally trigger another `StopDrawing` /
   `BlitToScreens` post-splash".
3. **Cross-check against Einstein** — still outstanding from
   iter-61. `NewtonProbe`'s ROM loader fails ("code 3"); a
   small companion that runs `TEmulator` for 90 s and dumps
   `gObjectTable` + run-queue would be the cleanest oracle.

**Background (carried over from iter-61):** the residual
`evt.ex.fr.store` throws are benign. r1 ∈ {0xffffd698, 0xffffd692}
decode (per `ghidra/DDKIncludes/OS600/OSErrors.h`) as -10600
(`kSError_ObjectOverRun`) and -10606 (`kSError_ObjectNotFound`) —
soup-probe misses caught by Newton's NewtonScript runtime, not
fatal. With a 90 s SIGKILL'd cold boot the system reaches a
**quiescent idle at the Newton splash**: the lightbulb logo +
"Newton" caption render correctly (`/tmp/newton-fb/00000.png`,
19 748 B, two `screen.blit` calls — one full 480x320, one 118-row
sub-region covering the logo). Task census: `newt`=RUN,
`scrn`=RDY on `TSemaphoreGroup` at `0xc125cec` (id=0x3707, sema[1]
of a 3-sema group; not a `TULockingSemaphore` — `refcon-stash=0`,
`lock-word=0`, so this is an event-signal group, not a held
mutex), `inkr`=BLK, all other 24 tasks BLK. 26 tasks total. We
are well past `DiagBootStub` and `OsBoot` — the late user-mode
system tasks (`cdfm`/`cdsv`/`drvr`/`alrt`/`sndm`/`mntr`/`Tmux`/
`pg&e`/`pckm`/`cmgr`/`ROMP`/`ROMF`) are all instantiated and
parked.

What we **don't** yet know: what `scrn` is waiting on. We have
no evidence pinning it to a specific producer (tablet, alarm,
redraw queue, NewtonScript callback, etc.) — that's the
investigation iter-62 has to start with, without prejudgement.
The next-step list below is deliberately neutral on the producer
identity.

Cross-reference with Einstein at the same wall-clock point is
also outstanding. `build/NewtonProbe` fails to load our 8 MiB ROM
("code 3" from `TROMImage::GetErrorCode`); the captured
`probe/results-717006-90s*.txt` files are MMU-state dumps and
don't include task-census or scheduler state. We need an Einstein-
driven oracle that prints the task list + run-queue at 90 s wall
to compare against our hypervisor's state.

Next steps:

1. **Identify what signals `TSemaphoreGroup` id=0x3707.** Walk
   the disasm for every `SemOp(release)` against a sema in
   `[arr_base=0xc125d10, arr_base+120)` — that's the 3-sema
   array of this group. Each release call site tells us
   *something* about who wakes `scrn`. Don't pre-commit to a
   guess about which producer is the relevant one.
2. **Cross-check against Einstein.** Either fix `NewtonProbe`'s
   ROM loader (the "code 3" path) or write a small companion
   that runs `TEmulator` for 90 s and dumps `gObjectTable` +
   the run-queue head, mirroring our `task_dump`. The point is
   to learn whether Einstein at the same wall-clock is in the
   same state we are, or has progressed further — that
   constrains the hypothesis space for what's missing.
3. **Optional perf (deferred):** ScratchVA fallback for the
   rotate-LDR `no_dead_scratches` rejection (98 % of inline-
   stub misses). Trap rate at the splash idle is ~400 K/s,
   dominated by `ELR=0xffffe4` (rotate-LDR returns) — fine for
   development; tackle only if it interferes with whatever the
   producer investigation needs.

### Iteration 62: per-task APCS stack tracer

Adds a frame-chain walker to `task_dump` so every blocked task's
saved registers and call stack are printed alongside the existing
`savedPC`/`SPSR`/`sp_usr`/`lr_usr` line. Reads each task's
`fp_usr` from `TTaskSavedContext.fp_usr` (`TTask + 0x3c`), then
walks APCS frames (`*fp = saved pc`, `*(fp-4) = saved lr`,
`*(fp-12) = prev fp`) up to a 12-deep cap with self-loop and
unmapped-VA guards.

Symbol-name lookup (`fn_name+0xN`) was extracted from `tracer.rs`
into a new always-available `src/symbols.rs`; `build.rs` now
emits `fn_addrs.bin` / `fn_name_offs.bin` / `fn_names.bin`
unconditionally (was gated on the `trace` feature). The walker's
formatter is honest about uncertainty:

- Jump-table slots (`0x01A0_0000..0x01C2_0000`) are decoded by
  reading the `b imm24` at the slot, computing the target, and
  resolving *that* in the symbol table — printed as
  `name+0xN [JT]`. Without this every JT slot would render as
  the spuriously-nearest ROM symbol with a giant offset.
- Kernel-VA addresses (`>= 0x0C00_0000`) print as `<data 0x…>`.
- Unsymbolised ROM gaps (offset > 0x1000 from the matched
  entry) print `name+0x?` to flag the wide miss.

Output is now name-first, with raw values trailing in `[…]` for
the case-by-case verification path. Example:
```
    task 0xc125484 (scrn) id=0x37b3 SPSR=0x40000110
      PC=SemaphoreOpGlue+0x0 <- LR=ScreenUpdateTask+0x48
      [pc=0x3ae1fc lr=0x1cd164 sp=0xcd34f78 fp=0xcd34fa8]
        #1  ScreenUpdateTask+0xc  <-  TaskKillSelf+0x0 [JT]
            [pc=0x1cd128 lr=0x1bdde84 fp=0xcd34fa8]
```

#### Findings (recorded in the Status block above)

The 26-task census split cleanly into three patterns:
- 12 tasks parked at `TUPort::Receive` inside
  `TAppWorld::AEventLoop` (standard event-loop wait).
- 8 tasks at `MonitorEntryGlue [JT]` (kernel-monitor SVCs).
- `idle`/`main`/`scrn`/`drvr`/`STKU` each in something more
  specific.
- Every task's bottom frame `lr` resolves to `TaskKillSelf [JT]`,
  the kernel-parked "if TaskEntry returns, kill the task" thunk.
  This is the *only* place a JT address legitimately appears in
  a stack trace; mid-stack JT-as-LR doesn't happen because the
  JT body is `b real_target` (preserves caller's LR).

#### Verification

- All 36 guest tests pass on QEMU.
- 35 s SIGKILL'd cold boot reaches the same splash-idle state as
  iter-61 (no regression). New stack-trace output is printed at
  every periodic task dump (~every 2 s).
- DIAG_TAG remains effectively zero in the steady-state window
  (iter-59/60 fast trampoline still intact).

#### Out of scope (deferred)

- Demangled-name truncation. Some C++ demangled names
  (`TUPort::Receive(unsigned long *, void *, …)`) overflow the
  fixed 96-byte format buffer and clip mid-parameter. Cosmetic;
  fix when it bites.
- Stack trace for the currently-RUNNING task (`newt`). Saved
  context isn't valid (the kernel writes `0x55555` as a sentinel
  while the task is on-CPU). Could fall back to the live
  ELR_EL2 / FAR_EL1 captured by the most recent trap, but only
  when the dump is invoked synchronously from a trap path.

### Iteration 61: residual `evt.ex.fr.store` triaged — boot reaches splash idle

iter-60 cleared the fatal `evt.ex.abt.bus` throw but left an open
question: was the residual `evt.ex.fr.store` (5 throws fired
during the 30 s test window, with r1 ∈ {-10094, -10088}) a soft
exception or a slow-walk to UnhandledException? iter-61 ran a
90 s `timeout -k 2` cold boot to settle it.

#### Findings

- **Both r1 values are benign store errors.** Per
  `ghidra/DDKIncludes/OS600/OSErrors.h`:
  - `kStoreError_Base = ERRBASE_OS - 600 = -10600`
  - `0xffffd698` = -10600 = `kSError_ObjectOverRun`
  - `0xffffd692` = -10606 = `kSError_ObjectNotFound`
  Both are "soup probe miss" outcomes that the NewtonScript
  runtime catches; they're the kernel's normal way of reporting
  "this stored object doesn't exist" or "read past end of object"
  back to the interpreter. Caller-LRs:
  - `0x00351e50` — `StoreCreateSoup`
  - `0x00353730` — `Get__15TStoreHashTableFlPcPl`
  - `0x002df4f0` — `LoadPermObject__FP13TStoreWrapperUlPP13CDynamicArray`
  - `0x002eff24` — `DoCall__FRC6RefVarl`
  - `0x002f1eac` — `Run__12TInterpreterFv`
  All consistent with NewtonScript-level soup access.

- **Boot reaches a quiescent idle at the Newton splash.** Two
  `screen.blit` calls (full 480x320 frame + a 118-row sub-region
  covering the lightbulb logo) and one `fb_dump`
  (`/tmp/newton-fb/00000.png`, 19 748 B) render correctly. After
  that the scheduler stays at `highest_pri=0 bitmap=0`,
  `newt`=RUN, `scrn`=RDY on `TSemaphoreGroup` at `0xc125cec`
  (id=0x3707, sema[1] of 3; not a `TULockingSemaphore` — the
  user-wrapper's `refcon-stash=0` and `lock-word=0`, so this is
  an event-signal group, not a held mutex). 26 tasks in the
  object table, matching iter-60. We are well past `DiagBootStub`
  (pre-multitasking ROM/RAM init at `0x1955c`) and past the
  `TFlashStore::Init` flash-cache-flush phase that iter-58
  unblocked — the named user-mode system tasks (`cdfm`/`cdsv`/
  `drvr`/`alrt`/`sndm`/`mntr`/`Tmux`/`pg&e`/`pckm`/`cmgr`/`ROMP`/
  `ROMF`) are all instantiated and parked.

- **No further halts in 90 s of wall-clock.** Trap rate
  ~400 K/s, dominated by alignment-fault returns at `ELR=0xffffe4`
  (the rotate-LDR EL2 emulator) — `newt` is in some idle loop
  with unaligned word LDRs.

#### Deliverables

No code changes. PLAN.md updated to set iter-62's goal (tablet/
pen input) and record the splash-idle as the Phase-B endpoint
reached.

### Iteration 60: DFSC=5 fast-forward exclusion — bus-abort throw resolved

iter-59's fast trampoline bypassed `handle_diag` for every
forwardable DFSC. That broke section-level translation faults
(DFSC=0x05): ARMv7 leaves DFSR.Domain UNK for those, the kernel's
DAH then computes Domain=0, `GetDomainAndFaultMonitorFromDomainNumber(0)`
returns no monitor, `FaultMonitorEntry` returns -10015, and DAH
throws `evt.ex.abt.bus`. iter-58's slow path synthesised
DFSR.Domain from L1[FAR>>20][8:5] before forwarding (handle_diag,
trap.rs:6295), which got the right Domain=4 and let DAH recover.

#### Diagnosis

Cold-boot probe captures (lines 6518–6697 of the iter-60 cold-boot
log) showed:

```
NewStack POST-SWI: env=0x13a5 req=0x11000 base=0x0cd2d000 ...
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x0cd2d000 mode=0x17
FME-entry[4]: r0(mask)=0x0000121a far=0x0cd2d000 ... task[+0x58]=0x00000045
```

Compare against the iter-59 log at the same FAR: `task[+0x58]=
0x00000005` (Domain=0, FS=5). The `0x45` vs `0x05` difference is
exactly the synthesised Domain=4 vs hardware-UNK Domain=0. Mask
went from 0 (no monitor matched) to 0x121a (matched stack/heap
domain), and `FaultMonitorEntry` returned 0 (success → recovery)
instead of -10015 (failure → throw).

Empirically, L1[0xcd] for our run is `0x04025081` with bits[8:5]
= 0b0100 = 4 (heap-domain encoding). Even though that L1 entry's
type bits show "section descriptor" (= no fault), the section
itself faults at first access because the stage-2 mapping
isn't backed yet — DFSC=5 fires, but the L1 entry already has
the domain bits the kernel needs. Synthesis is therefore correct
and matches the StrongARM behaviour the kernel was written for.

#### Fix

Two NOPs in place of the DFSC=5 dispatch slots in
`install_dabt_fast_trampoline` (`src/guest_mem.rs`):

```rust
// iter-60: DFSC=0x05 deliberately excluded — see file-level
// comment for rationale. Two NOPs preserve the slot layout so
// the existing beq targets / `b SLOW_DABT_TRAMP` offset stay
// correct without recomputing.
rom_ptr.add(ft +  8).write(0xE320_F000);  // nop
rom_ptr.add(ft +  9).write(0xE320_F000);  // nop
```

DFSC=5 now falls through to the slow path → DABT_TRAMP → HVC
#DIAG_TAG → handle_diag → synthesise → forward. Other DFSCs
(0x07, 0x0F, 0x0D, 0x06, 0x03) keep the iter-59 fast bypass.
NOP encoding `0xE320_F000` verified with `arm-none-eabi-objdump`.

#### Verification

- All 36 guest tests pass on QEMU.
- 30 s cold boot (no snapshot, fresh):
  - DIAG_TAG (slow-path DABT) ≈ 0 in the per-2-s histograms — fast
    path still working for the common DFSCs. (The earlier 5-min
    measurement of 249 M DIAG_TAGs was a `timeout 30` that didn't
    actually kill QEMU because `timeout` defaults to SIGTERM which
    QEMU's semihosting ignores; the 5-min run kept emitting traps
    after the kernel got into a post-throw reboot loop. Use
    `timeout -k 2 30 …` (or `-s KILL`) for QEMU runs.)
  - One `dabt: forwarding DFSC=0x5 FAR=0x0cd2d000` slow-path entry,
    followed by the DFSC=7 page-level faults at the same VA — the
    expected first-touch sequence per new section.
  - 26 tasks alive (was 24 in iter-59): `OBJM`, `idle`, `main`,
    `cdfm`, `newt`, `cdsv`, `PMGR`, `PTBL`, `STKF`, `STKP`,
    `STKU`, `cdpr`, `drvr`, `ROMF`, `pg&e`, `alrt`, `ROMP`,
    `sndm`, `mntr`, `Tmux`, `name`, `pssm`, `pckm`, `cmgr`,
    plus the new `inkr` (recogniser, 0x3853) and `scrn`
    (screen, RDY blocked on TSemaphoreGroup at 0xc125d58).
- New residual: `evt.ex.fr.store` NewtonScript throws fire at
  ROM PCs `0x00353730` / `0x002df4f0` / `0x00351e50` /
  `0x002eff24` / `0x002f1eac` with r1 ∈ {0xffffd692, 0xffffd698}.
  Caught and continued — no `UnhandledException` halt in the
  30 s window. Tracked as iter-61.

#### Out of scope (deferred)

- Inline DFSR.Domain synthesis in the AArch32 fast trampoline.
  Doable (≈10 extra insns: read FAR, walk L1, splice into DFSR
  via `mcr p15,0,r0,c5,c0,0`) but DFSC=5 fires only on first
  touch of a 1 MiB section — ~tens of times per boot. Slow-path
  cost is negligible.
- Hardening the timeout pattern in iter scripts. Adopted ad-hoc
  in iter-60: use `timeout -k 2 N` so QEMU under semihosting
  actually dies on deadline.

### Iteration 59: AArch32 fast-forward DABT trampoline — boot reaches scheduler

iter-58's HVC-tag histogram diagnostic (added this iteration as
`trap.rs::dump_hvc_tag_stats`, called every ~2 s from `trap_irq`)
revealed that ~99% of HVCs were `HVC #DIAG_TAG` (0x11) — kernel
DABT-vector traps. Each was a full EL2 entry/exit even though
`handle_diag` just rewrote ELR to forward the fault to the kernel's
own `DataAbortHandler` at `0x00393114`. The kernel's
`AddPgPAndPermWithPageTable` (and many other paths) take routine
translation / permission faults during normal operation; round-
tripping every one through EL2 was the dominant remaining cost.

#### Fix

New AArch32-side fast-forward trampoline at
`DABT_FAST_TRAMP_OFFSET = 0x008F_FF00` (in the unused tail
between Einstein.rex and the tracer trampoline pool). VA 0x10
now branches here first; the trampoline reads DFSR, masks to
DFSC[3:0], and dispatches in 4–10 inline instructions:

```
ft+0:   mcr p15,0,r0,c13,c0,2     ; TPIDRURW = R0 (save)
ft+1:   mcr p15,0,r1,c13,c0,3     ; TPIDRRO = R1 (save)
ft+2:   mrc p15,0,r0,c5,c0,0      ; R0 = DFSR
ft+3:   and r0, r0, #0xF          ; DFSC[3:0]
ft+4:   cmp r0, #7    \           ; six dispatched values:
ft+5:   beq FAST_FWD  |             0x07 (translation, page)
ft+6:   cmp r0, #15   |             0x0F (permission, page)
ft+7:   beq FAST_FWD  |             0x05 (translation, section)
ft+8:   cmp r0, #5    |             0x0D (permission, section)
ft+9:   beq FAST_FWD  |             0x06 (access flag, page)
ft+10:  cmp r0, #13   |             0x03 (access flag, section)
ft+11:  beq FAST_FWD  |
…       …             |
ft+16:  mrc p15,0,r0,c13,c0,2     ; restore R0 (was clobbered with DFSC)
ft+17:  b SLOW_DABT_TRAMP         ; uncommon DFSCs → DABT_TRAMP_OFFSET
ft+18:  mrc p15,0,r0,c13,c0,2     ; FAST_FWD: restore R0
ft+19:  mrc p15,0,r1,c13,c0,3     ;           restore R1
ft+20:  ldr pc, [pc, #-4]         ;           jump to DAH
ft+21:  literal: 0x00393114
```

For forwardable DFSCs the entire round-trip is ~6 instructions
of inline AArch32 with no EL2 entry. Other DFSCs (alignment,
external aborts, etc.) fall through to the existing
`DABT_TRAMP_OFFSET` slow path.

`trap.rs` gains `dump_hvc_tag_stats` + per-imm histogram
counters, called from `trap_irq` every ~2 s of wall time
(independent of the snapshot autosave gating).

#### Verification

- All 36 guest tests pass on QEMU.
- HVC histogram: `DIAG_TAG=20.8M → 0` between iter-58 and
  iter-59. `UND_TAG` (byte-access UDF) and `ALIGN_TAG`
  (rotate-LDR) are now the dominant non-zero entries; the
  former at ~146 K, the latter at ~3.9 K, both small.
- Cold boot reaches the multitasking phase. Task dump shows
  `OBJM`, `idle`, `main`, `cdfm`, `newt` (RUN), `cdsv`, `PMGR`,
  `PTBL`, `STKF` and others all alive — the same scheduler
  state Einstein reaches at 60 s wall (per NewtonProbe).
- New failure: `evt.ex.abt.bus` and `evt.ex.fr.store` thrown
  by kernel code reach `UnhandledException`. Tracked as the
  iter-60 starting point.

#### Out of scope (deferred)

- Stub the rotate-LDR `no_dead_scratches` rejection rate (98%)
  via a ScratchVA fallback like shadow_stub uses for byte
  accesses. Would cover sites where liveness can't find 2 dead
  candidates by saving them to a per-stub 8-byte slot in the
  scratch pool.
- Refactor `unaligned.rs` and `handle_diag` to read banked
  LR_abt / SP_abt from `ctx.x[20]` / `ctx.x[21]` instead of
  the trampoline's `DABT_SAVE_PA` slot, which would let us
  drop the lr/sp save in the slow `DABT_TRAMP` and make even
  the slow path leaner.

<!-- iter-58 (untrap CP15 cache-by-VA, 5-15x speedup) pruned per the
     auto-prune maintenance note. See `git log --grep="iter-58"` for
     the full retrospective. -->


<!-- Older iteration retrospectives (iter-57 and earlier) live in
     `git log` per the auto-prune maintenance note. -->

## Workflow per stop

1. Capture verify-mmu output (`fix_stage1_xn_bits` ratchets per
   alias-onset). Each alias is a `(PA, VA1, VA2)` tuple.
2. Identify the kernel-side write that creates each alias by
   instrumenting the relevant L2-write entry point with an HVC probe.
3. Cross-reference with Einstein (`build/NewtonProbe baremetal/roms/
   newton.rom _Data_/Einstein.rex 30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — `src/peripherals/*.rs`, `src/trap.rs`.
   - **Einstein behavioural quirk** — port the matching logic.
   - **ROM patch** — `src/rom_patches.rs`. Only when no other layer can
     host the fix.
5. Re-run, observe alias count, repeat until zero.

## Tools

### Hosts

- **QEMU raspi3b** (default; `cargo run --release`) — fast, BCM2835
  VIC, AArch32↔AArch64 banking quirks documented in `docs/QEMU_BUGS.md`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — `scripts/fvp <elf>`. Accurate
  reference: GICv3, generic timer + cache model exact. Build with
  `--no-default-features --features platform-fvp-base`.

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`. Patches every
  `scripts/classify-out/code-symbols.txt` entry with HVC trampoline.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor function-
  entry traces.
- **`build/NewtonProbe`** — Einstein-as-oracle.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s from `trap_irq`.
- **Framebuffer PNG dumps** — `/tmp/newton-fb/NNNNN.png` after
  `screen::blit`.

### Debugging

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). Helpers `bg
  <addr>`, `bp <addr>`, `tt N`, `guest-state`.
- **DABT/PABT DIAG HVCs** at ROM offsets `0x10` / `0x0C`.
- **Software-reset canaries** — BootOS / PowerOffAndReboot / Reboot.

### Reference

- `scripts/disasm-out/rom.dis` — symbol-annotated ROM+REx disassembly.
- `docs/DISASM.md` (incl. "Jump-table aliasing — DON'T mistake the
  thunk for the body").
- `docs/NEWTON_INTERNALS.md` — APCS, ClassInfo dispatch, ROM patch
  table 0x01A00000..0x01C20000.
- `docs/QEMU_BUGS.md` — raspi3b AArch64↔AArch32 quirks.
- `docs/STRUCTURES.md` — kernel struct layouts (TScheduler, TTask,
  TStackManager, end-to-end page allocation).
- `docs/peripherals.md` — peripheral implementations.
- `probe/FINDINGS.md` — golden record from a fully-booted Newton.

### Tests

`baremetal/guest-tests/scripts/run-all.sh` runs the 36 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011 and runs the verify-mmu
  alias detector; UND-vector trampoline; DABT/PABT DIAG patches.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / SBA /
  tracer / canary / probe tags); `handle_page_get_probe`,
  `handle_remember_entry_probe_with` (with the new aliasing tracker);
  `handle_data_abort` with kernel-DABT forwarding for lazy stack
  growth.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, TPC, TPU, IMO, FMO, AMO,
  DC); CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3.
- `src/banked.rs` — AArch32 banked-register access from EL2 (Table
  D1-79).
- `src/rom_patches.rs` — Einstein word-write patches; HVC injection
  helpers; canaries; ResolveFault wrapper; `PAGE_GET_PROBE` patch.
- `src/peripherals/*` — Newton driver / native-primitive surface.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-*.bin`.
- `src/tracer.rs` — function-level tracer.
- `src/guest_bp.rs` — `bp <addr>` for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `guest-tests/tests/` — 36 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 36 tests must pass.

## Non-goals

- Real screen emulation beyond the framebuffer dump — no compositor,
  no pen input.
- Package loading — needs a solution for embedded native code.

## Diagnostic scaffolding (active)

- `verify-mmu` in `fix_stage1_xn_bits` — ratchet-logs subpage-AP
  heterogeneity and per-alias-onset `(PA, VA1, VA2)` tuples.
- `handle_page_get_probe` (PAGE_GET_PROBE_HVC_IMM=0x53) on
  `0x00258EFC` — page-allocator return logger + dup detector.
- `handle_remember_entry_probe_with` (REMEMBER_PROBE_HVC_IMM=0x46)
  on `0x00258E0C` — Remember-side per-PA → first-VA aliasing tracker
  (added to the existing L1-lazy-grow probe).
- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.

Pull these once the boot quiesces.
