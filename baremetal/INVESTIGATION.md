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

### `0x0ca6b010` is the legitimate RelocHeap; its header is the corruption (2026-04-27, late)

NewHeap-entry / SetCurrentHeap-entry / TRefStack-NewStack-exit probes
(`src/guest_bp.rs::handle_user_bp_und`, installed from `kmain`)
disambiguate where the bogus heap pointer enters the system. Cold boot
log `/tmp/run-probe.log`:

**NewHeap is called 7 times.** Bases and sizes:

| #  | base       | size       | caller-lr  |
|----|------------|------------|------------|
| 0  | 0x0c200c00 | 1 MiB      | 0x001423f8 |
| 1  | 0x0c600c00 | 3.5 MiB    | 0x001423f8 |
| 2  | 0x0c984000 | 896 KiB    | 0x001423f8 |
| 3  | **0x0ca6b000** | **2 MiB** | 0x001423f8 |
| 4  | 0x0d601000 | 1.003 MiB  | 0x001423f8 |
| 5  | 0x0d601000 | 1.027 MiB  | 0x00142908 |
| 6  | 0x0ccac800 | 50 KiB     | 0x001423f8 |

Heap #3's header lives at `0x0ca6b010` (= base + 16). Immediately
after creation, NewHeap calls `SetCurrentHeap(0x0ca6b010)` from
`lr=0x00310e60` (the entry switch inside NewHeap). So `0x0ca6b010`
is **the legitimate RelocHeap pointer**, not a wild value.

**SetCurrentHeap is called many times with `r0=0x0ca6b010`.** Caller
LRs (and the function each LR sits in):

- 0x00141c40 — inside `NewHandle`'s heap-switch sequence
- 0x001415d4 — `NewHandle` immediately after `bl SetHeap`
- 0x0031325c, 0x003132cc — `CompactHeap` save/restore around its body
- 0x00141ef0 — inside `HUnlock`

So the kernel actively switches to RelocHeap during normal handle
allocation and compaction. The wedge isn't "bogus pointer in
globals[-16]" — it's that the heap's content has been corrupted.

**TRefStack-NewStack-exit fires 3 times before the wedge**, each time
with `r0=0` (= success per `TRefStackFv` ctor's `teq r0,#0; beq` at
0x1a4950). So NewStack is functioning correctly — the SVC-return path
is intact and stack growth is happening.

**The corrupted heap header at 0x0ca6b010 (128 bytes):**

```
+0x00  0x002dd804 0x001a48f0 0x0c645f0c 0x0cc825d8
+0x10  0x00000000 0x0cc825e0 0x0cc82510 0x0cc82038
+0x20  0x002dfa20 0x002dd7c4 0x0c6437b4 0x0cd51800
+0x30  0x0cd51800 0x0cd51c98 0x0000012c 0x00000001
+0x40  0x00000040 0x0c9842b4 0x002dfa20 0x00000000
+0x50  0x00000000 0x00000000 0x00000000 0x00000000
+0x60  0x00000000 0x0c116e7c 0x00000000 0x00000000
+0x70  0x00000000 0x00000000 0x00000000 0x00000000
```

`+0x40 = 0x40` matches NewHeap-init's `r0 = #64; str r0, [r7, #64]`.
`+0x64 = 0x0c116e7c` is the heap-store TULockingSemaphore wrapper.
`+0x44 = 0x0c9842b4` is a pointer into NewHeap #2's region. So the
heap was correctly initialised at one point.

The corruption is concentrated in `+0x00..+0x14` (looks like saved
ROM PCs / stack pointers) and `+0x48`/`+0x60` (freelist position
clobbered to a ROM PC = `0x002dfa20`). SearchFreeList walks the
freelist starting at `heap[+72] = 0x002dfa20` and dereferences
`*0x002dfa24 = 0xe52d006c` (= encoding of `str r0, [sp, #-108]!`),
which is what causes FAR=0xe52d006c.

Newt's actual user stack is at 0x0cc82xxx (sp_usr=0x0cc81f04 at the
fault). The RelocHeap is at 0x0ca6b000..0x0cc6b000 (2 MiB). They
do **not** overlap — the corruption isn't direct stack/heap aliasing.

Diagnostic scaffolding installed (all stay armed via re-occupy-slot
in `handle_user_bp_und`; capped at 32 lines per probe but
unconditionally log on the wedge-relevant matches `r0=0x0ca6b010` for
SetCurrentHeap and `r0=0x0ca6b000` for NewHeap):

- `0x00313308` SearchFreeList wild-r0 dump (halt)
- `0x001a4948` TRefStack post-NewStack r0/r4/sp/lr
- `0x00142df0` SetCurrentHeap entry r0/lr
- `0x00310e24` NewHeap entry r0(base)/r1(size)/lr

Concrete next steps:

1. Identify the WRITE that lands `0x002dd804` at heap[+0]. Add a stage-2
   write-watch on `0x0ca6b010..0x0ca6b020` (or a guest-side gdb
   data-breakpoint via QEMU's WP) that fires on the first store.
   Cross-check the writing PC against a function that thinks it's
   pushing a stack frame.
2. Walk the heap once at NewHeap-exit (extra probe) and again at the
   first SearchFreeList hit on RelocHeap to bracket when the
   corruption appears in time.
3. Run Einstein at the same boot offset and dump the equivalent heap
   region for comparison. If Einstein's RelocHeap survives, the bug
   is hypervisor-side (stage-2 mapping / aliasing); if Einstein
   corrupts it the same way, it's a ROM-level data-flow bug we need
   to mirror or work around.

### Earlier: Bus-error origin pinned to PC=0x00313308, FAR=0xe52d006c (2026-04-27, evening)

Cold-boot-with-quiet log (`/tmp/run-quiet.log`) plus a hypervisor-side bp
at SearchFreeList's `ldr r3, [r0]` (ROM 0x00313308; install in `kmain`,
emulation+halt-on-wild-r0 in `handle_user_bp_und`) gives a single dump
when the bus-error chain fires:

- DAH-OR[8] FAR=0xe52d006c, current task = newt.
- Faulting PC 0x00313308 in `SearchFreeList` (ROM 0x003132d8).
- ctx at the fault: r0=0xe52d006c, r1=0x0ca6b010, r4=0x7c (request
  size), r9=0x0ca6b12c (= r1+0x11c), lr_usr=0x003132ec (back in the
  same fn), fp=0x0cc81f14, sp_usr=0x0cc81f04.

The "heap" at r1=0x0ca6b010 is NOT a real heap. Its 128-byte header has
ROM PCs at +0x00, +0x04, +0x20, +0x24 instead of vtable / size / start /
freelist position:

```
+0x00 0x002dd804  ; instruction inside __ct__18TStoreObjectWriter,
                  ; right after `bl __ct__9TRefStackFv` at 0x2dd800
+0x04 0x001a48f0  ; first body instruction of __ct__9TRefStackFv
+0x08 0x0c645f0c  ; RAM+0x14 0x0cc825e0  ; stack pointer
+0x18 0x0cc82510  ; stack pointer
+0x1c 0x0cc82038  ; stack pointer (read as heap[+28]="size limit")+0x20 0x002dfa20  ; instruction `mov r0, #0` at MakeStoreObject; read
                  ; as heap[+32]="freelist start sentinel"+0x24 0x002dd7c4  ; instruction inside __ct__18TStoreObjectWriter
+0x28 0x0c6437b4  ; RAM
+0x2c 0x0cd51800  ; the new TRefStack stack base from the latest
                  ; NewStack POST-SWI in this trace+0x30 0x0cd51800  ; same+0x38 0x0000012c  ; 300 = TRefStack::ctor's `mov r0, #0x12c`
+0x48 0x002dfa20  ; heap[+72]="saved freelist position" — same ROM PC
                  ; as +0x20
```

freelist walk reads node[0]@0x002dfa20 = `{size=0xe3a00000 (= mov r0,#0
insn), next=0xe52d006c (= str r0,[sp,#-108]! insn)}` — i.e. the walker
is interpreting MakeStoreObject's body as a freelist node.

So `GetCurrentHeap` (which reads `*(*0x0c10105c - 16)` = current task's
`globals[-16]`) returns a pointer to something that is **not a heap** —
likely a TStoreObjectWriter / TRefStack / CatchHeader struct that has
saved return addresses where heap header fields belong. Walking it as
a heap reads ROM bytes that look like instruction encodings (0xe52d006c
= `str r0, [sp, #-108]!` — the function-prologue stack push), and the
next-pointer dereference faults with FAR=0xe52d006c.

Einstein doesn't hit this fault (probe log: 38 unique data-abort
tuples, all FAR ≤ 0x0CDDDC00, none with FAR=0xe5xxxxxx). So between
DAH-OR[7] (FAR=0x0cc81ff8 — the last common stack-grow we both take)
and the divergence, our hypervisor either:
- writes the wrong value into newt's `globals[-16]` (our `SetCurrentHeap`
  source), or
- reads `globals[-16]` from the wrong memory backing (stage-2 bug, or
  task-globals lookup pointing at the wrong page), or
- skips a normal heap-init step that Einstein performs (page-grow
  DABTs at PC=0x002DDF2C / FAR=0x0CDDDC00 that Einstein takes 22 times
  but we don't take at all — those are exactly inside the 0x002dxxxx
  `MakeStoreObject` / `TStoreObjectWriter` neighbourhood whose code
  bytes leak into our walker).

Diagnostic scaffolding stays loaded for now (kmain installs the bp,
handle_user_bp_und emulates the LDR + halts on wild r0). The next
concrete step is to identify why we miss the 22 `0x002DDF2C / FAR=
0x0CDDDC00` page-grow faults Einstein takes — those faults come from
`str r1, [r2], #4` inside what looks like TStoreObjectWriter::Prescan
(ROM 0x002dde20 region), populating a stack-allocated buffer. If our
stage-1 page table already has those pages mapped (so no DABT fires),
the writes land but read-back later returns ROM bytes from a stale
mapping — exactly the symptom we see. Inspect newt's L1/L2 entries
covering 0x0c... in the stack-grow window before the divergence.

### Earlier: newt self-deadlocks on its own heap semaphore (2026-04-27, am)

`newt` is queued on `q=0x00000000/0x0c116ed8`. That queue address is
**TSemaphore + 0x20** (the BlockOnInc queue) of a TSemaphore at
`0x0c116eb8`. Layout citations:

- task[+0x6c] flags = `0x2100000` — bit 0x100000 ("on a TSemaphore wait
  queue", set by TSemaphore::BlockOnInc / TTaskQueue::Add at ROM
  0x1d4dc8) | bit 0x02000000 (paged stack).
- TSemaphore is 40 bytes (ROM 0x1d5114 `mov r0, #40`); BlockOnZero
  queue at +0x18, BlockOnInc queue at +0x20 (TSemaphore::TSemaphore
  ROM 0x1d512c / 0x1d5134).
- The candidate `sema+0x20 = 0x0c116eb8` has `[+0x10] = 0x1ae40` which
  matches the TSemaphore vtable initialised at ROM 0x1d513c.

The TSemaphore is sema[0] of a TSemaphoreGroup at `0x0c116e94`
(kernel id `0x13d7`, count=1). Its TUSemaphoreGroup user wrapper is
at `0x0c116e7c`. The wrapper's `+0x08` (refcon) holds
`0x0c116e8c` which is `uwrapper + 8` — the malloc'd 4-byte
lock-state word for a TULockingSemaphore (TULockingSemaphore::Init
at ROM 0x25a514: `str r0, [r4, #8]; ... bl SetRefCon`). That word
contains `0x3063` — which is **newt's own task id**.

Newt's saved PC = `0x3ae1fc` (the `svc 0xb` of `SemaphoreOpGlue`),
`SPSR=0x110` (SVC mode), `lr_usr=0x25a2e0` (= the instruction after
`bl SemOp` in `TULockingSemaphore::Acquire` at ROM 0x25a298). The user
stack just below sp_usr has saved LRs:

- `+0x20 = 0x143334` — return into `DisposPtr` after its `bl Acquire`
  at ROM 0x143330.
- `+0x60 = 0x354724` — return into `MakeStoreObject`'s exception
  handler at ROM 0x354718, the `b 0x3544f4` catch loop that calls
  `TStoreWrapper::Abort` and `NextHandler`.
- `+0x60 = 0x353af0` — return inside `TStoreWrapper::~TStoreWrapper`.

So the call chain at the wedge is:

1. Newt entered `MakeStoreObject` (ROM 0x354178) and called
   `LockStore` (which Acquires the heap-store TULockingSemaphore =
   our id 0x13d7). `Swap` returned 0 (lock free) → newt acquired it.
   `lock-word` now = `0x3063` (newt's id).
2. Newt did store work (`StorePermObject`, `TStoreObjectWriter` ctor,
   etc.).
3. Something **threw `exBusError`** (Throw at trace 4149074, r0 =
   `0x000afda0` which is the literal pool pointer to `exBusError`
   class at ROM 0x3712b8). The bus-error origin is unidentified —
   most likely an MMIO read or stage-2 fault we should turn into a
   silent-default rather than a guest-visible bus error.
4. `setjmp`/`longjmp` cleanup triggered the catch handler at ROM
   0x3544f4. **It calls `TStoreWrapper::Abort` (0x354b50) but NOT
   `UnlockStore` — Abort does not release the lock** (verified by
   reading 0x354b50: it only resets TNodeCache, calls Abort on
   TStore + the two TStoreHashTables, no UnlockStore).
5. The catch handler invokes `NextHandler` and chains to the
   destructor. `~TStoreWrapper` (ROM 0x353ae4) calls
   `DisposeRefHandle` which eventually reaches `DisposPtr` (ROM
   0x14320c). DisposPtr calls `Acquire` on the **heap semaphore at
   ROM 0x143330**.
6. That `Acquire`'s `Swap` finds `lock-word == 0x3063` (newt's own
   id, still set by step 1). Swap puts newt's id back and returns
   `0x3063 ≠ 0`, so Acquire calls `SemOp` → `BlockOnInc`. Newt is
   queued on its own held lock — self-deadlock.

The `newt`-on-`sema+0x20` linkage is therefore not a "kernel waiting
for an event" mystery; it's a **lock leak in the C++ exception
unwind path**: TStoreWrapper's catch arm doesn't unlock the store
before destroying the wrapper, and the destructor's heap free path
re-enters the same lock.

Einstein cross-check (NewtonProbe 60 s, `/tmp/probe-60s.log`): at
t=2 s Einstein already has `Tmux RUN`, `newt(3cf3) RDY`, `scrn RDY`
(prio 11), `newt(2f13) BLK`; at t=4–60 s `fser RUN` (prio 13),
plus tasks `cdsv`, `scpl`, `codc`, `scrn`, `newt(2f13)` cycling
RDY/BLK. Einstein never lands on this deadlock — most likely
because step 3 (the Bus Error) doesn't fire there. So the right
fix is to identify the Bus Error origin and make it not throw.

Investigation tools (`src/task_dump.rs`):
- `dump_semaphore_waits` — for each task with flag 0x100000 set, dump
  the queue head and probe both `sema+0x18`/`sema+0x20` candidates
  (whichever has `[+0x10]=0x1ae40` is the real TSemaphore).
- `find_semaphore_owner` — walks `gObjectTable` for KernelType=7
  (TSemaphoreGroup), matches by array-base + size.
- `dump_blocked_pcs` — prints saved PC / sp_usr / lr_usr from each
  blocked task's SWIBoot save area at task+0x10..+0x54, plus
  newt's user-stack window.

Next concrete step: re-run with `trace,quiet` (every-call trace) to
catch the exact memory access that triggers the Bus Error throw.
Compare against Einstein's run at the same offset; the divergence
will name the MMIO/DABT we need to silently default. After that,
the deadlock disappears even without changing the lock semantics.

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
