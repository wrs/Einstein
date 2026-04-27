# Phase B — Wedge reframed: kernel skips stack-monitor on the SVC-mode fault

## Status (2026-04-26 night)

**Steps 1–4 done. Step 5 prep done.** Probes installed:

- `HVC #0x49` at Fill prologue (USR / direct-callable)
- `HVC #0x4A` at NewStack post-SWI
- `HVC #0x4B` at `TStackManager::Fault` prologue
- `HVC #0x4C` at `TStackManager::ResolveFault` prologue

`handle_reboot` already dumps the NewStack ring and the live
TRefStructStack object. 35/35 guest tests stay green.

Findings recorded in `INVESTIGATION.md` under "Fault/ResolveFault
probes: kernel never reaches stack-monitor for fault #2".

The earlier framing ("recovery returns to PC `0x1a4b9c` in SVC instead
of USR, second fault wedges because SVC can't recursively re-enter
fault recovery") was partially right — the second fault IS taken from
SVC mode (`SPSR_abt=0x60000113`) — but **the kernel's response is not
"recursively re-enter and fail"**. The kernel's DataAbortHandler
**bypasses the stack monitor entirely** for that fault: neither
`TStackManager::Fault` nor `TStackManager::ResolveFault` is invoked,
and no `Remember` / `AllocatePageTable` SWI fires either. The kernel
walks straight to `Reboot(-10075)` via an `UnhandledException` →
`Reboot` chain (caller LR_UND = `0x000d9888` = `Reboot+4`).

So the wedge is now: **DataAbortHandler fast-classifies SVC-pre-abt
faults (or DFSC=0x5 vs 0x7) as unrecoverable and panics**, never even
attempting lazy-grow.

The remaining mystery is **how the CPU got into SVC mode at PC
`0x1a4b9c`** in the first place. The Fill probe captured a regular
USR-mode entry at `0x1a4b54` (`src_mode=0x10`, USR sp=0x0cc82664),
and Fill's body has no mode-changing instructions in the eight
instructions between probe entry and `0x1a4b9c`. Two leading
hypotheses, see "Step 6" below for the discriminating probe round.

**Step 5 (apply a fix) is blocked** until we know whether the SVC-mode
state at fault #2 came from:

- (A) a silently-handled earlier USR-mode fault at the same FAR whose
  recovery left CPSR in SVC — `log_dabt_forward`'s `(FAR, mode)` dedup
  could be hiding it, or
- (B) the kernel's recovery from fault #1 never returned to USR at
  all — Fill in SVC ran on the kernel's SVC stack (`sp=0x0c000400`),
  and the Fill probe's snapshot reflects a stale USR state from
  before fault #1.

## Earlier framing (kept for context)

Step 1 of the previous version of this plan landed: three HVC probes in
`src/rom_patches.rs::apply_l1_cd_probes`, dispatched by handle_hvc and
handle_und (USR-callers go through the UND trampoline) in `src/trap.rs`.
Cold-boot artifact at `/tmp/phaseB-l1cd-probe/qemu2.log`. INVESTIGATION.md
captures the full findings.

The probe data invalidated the previous framing of this plan. The wedge
is **not** a lazy-L1-grow failure inside `Remember` / `AllocatePageTable`.
The kernel's lazy-grow path works correctly in this same boot:

- `L1[0xC6]=0x90`, `L1[0xC9]=0x90`, `L1[0xCA]=0x90`, `L1[0xCC]=0x90` were
  all transitioned from `0x90` lazy → coarse via `Remember(va, perm=0)`.
  For the `0x90` (domain=4) marker, SWI #12 returned 0 immediately —
  the kernel's monitor handler grows the entry implicitly, no
  `AllocatePageTable` round-trip.
- `L1[0xC2]=0x70`, `L1[0xC3]=0x70`, `L1[0xD6]=0xb0` took the
  `-10003 → AllocatePageTable → retry` path as expected.

Both paths work. **No `Remember` call ever targets section 0xCD** — not
because the kernel can't grow it, but because nothing asks the kernel
to grow it.

## What actually fails

The wedge fires when the kernel's exception handler is itself running
in **SVC mode** (`SPSR_abt=0x60000113`) at `Fill__15TRefStructStackFv`
PC `0x001a4b9c` and writes to FAR `0x0cd07400`. The L1 walk finds
`L1[0xCD]=0x90` (lazy), the kernel can't recursively re-enter
fault-recovery from inside its own SVC handler, and it Reboots.

Per the wrapper-entry probe at FAR=`0x0ccee800`, the active stack's
`TStackInfo` has bounds `[0x0ccee800, 0x0cd06800)`. The SVC-side fill
cursor advanced 3 KiB past the upper bound:

```
TStackInfo info[+24] = 0x0ccee800   ; LOWER bound (kernel-granted)
TStackInfo info[+28] = 0x0cd06800   ; UPPER bound (kernel-granted)
fill_cursor          = 0x0cd07400   ; SVC write attempt — 3 KiB past top
USR lr (loop bound)  = 0x0cd07418   ; fill loop end target — 6 words further
```

`NewStack(0x10000)` requested 64 KiB but `TStackInfo` reports 96 KiB
(`info[+28] - info[+24] = 0x18000`). The user-side / kernel-side fill
loop walked further still, to ~99 KiB above base.

## Two open questions

1. **Who invokes `Fill_TRefStructStackFv` from SVC mode?** The function
   sits in user-API space; `grep -n "bl 0x1a4b54"` over the ROM
   disassembly returns zero hits. So the kernel must be reaching it
   through the post-ship patch table, an indirect call, or an emulated
   user instruction during fault recovery. We need to know who.

2. **Why does the fill loop bound exceed the kernel's `TStackInfo`
   upper bound?** The bound is `self->[16] + 4 * (self->[0] -
   self->[4]) / 4` = `TRefStructStack base + bytes_pushed_on_TRefStack`.
   So pushes on the sibling `TRefStack` drove the cursor past the end.
   Either the user code thinks both stacks have a larger size than the
   kernel granted, or the kernel granted less than the user requested.

## Step 6 — discriminate (A) silent-recovery vs (B) recovery-never-returned

This is the next probe round. Both probes are short and read-only;
neither requires a new abstraction.

### 6a. Lift the dabt-forward dedup, at least for the wedge range

`log_dabt_forward` in `src/trap.rs` keys dedup on `(far, hvc_src_mode)`
with a 16-entry table. `hvc_src_mode` is always `MODE_ABT` for the
trampoline path, so any second/third dabt at the same FAR is silently
suppressed — including a possible USR-pre-abt fault at FAR=`0x0cd07400`
whose recovery seeded the SVC mode we now observe.

Easiest fix: include `pre_abt_mode = SPSR_abt & 0x1F` in the dedup
key. That makes USR-pre-abt and SVC-pre-abt at the same FAR distinct
log lines without flooding on tight-loop kernel-side aborts.

Run, compare new log against `qemu4.log`. If a USR-pre-abt
`FAR=0x0cd07400` line appears between Fill probe and the existing
SVC-pre-abt line, hypothesis (A) is confirmed.

### 6b. Probe NewState entry/exit and the trampoline tail-call

NewState (`0x1a46f0`) tail-calls a small trampoline at `0x1a54a38` that
itself jumps into Fill. If hypothesis (B) is correct, NewState should
appear once and Fill should appear once, with the wedge inside Fill's
first iteration. If hypothesis (A) is correct, we may see Fill called
twice (once USR, once SVC).

Add an `HVC` at `0x1a46f0` first instruction logging mode + caller LR.
Wire it through the same `apply_l1_cd_probes` / `handle_und` /
`handle_hvc` machinery as the Fill probe. Reuse the source-mode CPSR
helper (`probe_source_cpsr`).

### 6c. Identify the DataAbortHandler exit that returns to USR

DataAbortHandler runs from `0x393114`. Its USR-pre-abt path eventually
issues a `subs pc, lr, #N` (or `movs pc, lr`) to ERET back to the
faulting USR PC. Find that exit instruction in the disasm
(`scripts/disasm-out/rom.dis`, lines 972359..~973000). Patch it with
`HVC #0x4D` so we log every successful USR-return from the handler.
If we see exactly one USR-return after fault #1, hypothesis (B) is
confirmed (the kernel did return to USR; mode flip happens later); if
we see *zero*, hypothesis (B) is the wedge.

## Step 7 — apply the fix

Depends on Step 6 outcome:

- **(A) silent USR-mode fault at FAR=`0x0cd07400`**: the kernel handles
  one fault path (USR-pre-abt + DFSC=0x5) without invoking the stack
  monitor. Find the in-DataAbortHandler branch that handles this case
  and trace what it does. Most likely it sets up a tail-call to
  something that should grow `L1[0xCD]` but instead leaves CPSR=SVC
  and returns. Repair locally (probably a missing `subs pc, lr` or a
  branch to the wrong continuation).

- **(B) recovery from fault #1 didn't return to USR at all**: trace
  the kernel's path between the Fault wrapper return and the
  DataAbortHandler exit. The wrapper returning r0=0 means
  `Fault.r5=0`, FaultMonProc takes the success branch, and somewhere
  between FaultMonProc's return and DataAbortHandler's exit the mode
  is supposed to flip back to USR. If our 4-iter wrapper inadvertently
  affected that path (e.g. by growing the kernel's monitor list in a
  way the exit code can't unwind), the fix is wrapper-side.

Either way, the fix should reduce the user-vs-kernel stack-extent
diff to zero and let the boot advance past `0x0cd07400`. Probes from
Steps 1–5 stay in tree until the fix lands.

## Plan — investigate, don't add new infrastructure

### Step 1: identify the SVC-mode caller of `Fill__15TRefStructStackFv`

Patch the first word of `Fill_TRefStructStackFv` (`0x001A_4B54`,
`stmfd sp!, {lr}` = `0xE92D_4000`) with `HVC #0x49`. Handler logs the
source-mode banked R14 (= caller's return PC) and the source mode bits.
Mirror the existing probe pattern in `src/rom_patches.rs`
(`apply_l1_cd_probes` / `patch_probe`) and `src/trap.rs`
(`handle_remember_entry_probe_with` etc.). Don't forget the handle_und
arm for the USR-mode path.

To preserve `Fill`'s prologue, emulate the original `stmfd sp!, {lr}`
in the handler: read source-mode SP, decrement by 4, write source-mode
LR to the new top, write the new SP back to the source-mode banked
slot. (Or: install a 3-word wrapper at `0x00FF_FExx` that does
`HVC #0x49 ; stmfd sp!, {lr} ; B Fill+4` and patch `Fill+0` to
branch to it. Mirror the ResolveFault wrapper layout.)

Cold-boot, capture the log. Expected output: one or more "Fill called
from {svc_lr}, mode={svc}" lines pointing at the kernel function that
walked into Fill. Cross-reference against the ROM disassembly to find
the call site.

### Step 2: dump TStackInfo and TRefStack/TRefStructStack state at the wedge

Extend the existing `handle_reboot` state dump in `src/trap.rs`
(currently dumps L1 sections + monitor list) with a TStackInfo dump for
every active TStackInfo whose bounds touch sections 0xCC..0xCF, plus
the live `TRefStack` / `TRefStructStack` objects whose `self->[0..20]`
fields point into that range. The data we need:

- For each TStackInfo: `info[+24]` (lower), `info[+28]` (upper),
  `info[+8]` (num_pages?), `info[+20]` (page_table base).
- For the TRefStructStack at the wedge: `self->[0]` (TRefStack top),
  `self->[4]` (TRefStack base), `self->[16]` (TRefStructStack base),
  `self->[20]` (TRefStructStack cursor).
- Compute `self->[0] - self->[4]` (bytes pushed) and
  `self->[20] - self->[16]` (bytes already filled).
- Compare both against the granted size (96 KiB in the run we have).

The TRefStack/TRefStructStack objects sit on the heap so finding them
needs either a memory walk or correlation with the disasm
(TInterpreter at offset...; check the TInterpreter ctor).

### Step 3: reconcile NewStack-output with what the user expected

`NewStack(0x10000)` should grant a 64 KiB region; observed
`info[+28] - info[+24]` is 96 KiB. If NewStack's user-facing return
gives 64 KiB to the user but the kernel TStackInfo reflects 96 KiB,
there is no mismatch — Fill should bound at 64 KiB and never reach
0x0cd07400. If NewStack returns 96 KiB to the user, then the user's
99 KiB fill is the divergence (3 KiB over-walk).

Patch `NewStack` (`0x001F_8968`) post-SWI return (the `add sp, sp, #4`
at `0x001F_8948`, original `0xE28D_D004`) with `HVC #0x4A`. Handler
logs the two output values stored at `[sp+16]` (LOW) and `[sp+20]`
(HIGH). Capture three or four `NewStack` calls from the
`TRefStack`/`TRefStructStack` constructors during TInterpreter setup
and compare `(HIGH - LOW)` against the requested size and against the
later observed `TStackInfo` bounds.

### Step 4: cross-check against Einstein and real Newton

Once we know whether the divergence is "kernel grants too little" or
"user fills too much", regenerate the relevant probe trace in Einstein
(`Emulator/TStackManager.cpp` etc.) and `probe/FINDINGS.md`. The same
TInterpreter ctor runs on both; whichever side disagrees with us is
the side to fix.

### Step 5: fix the actual cause

Depends on Step 3+4 results:

- **If Einstein's `NewStack` returns the same range as ours and Fill
  bounds correctly there**: the divergence is in our hypervisor's
  view of stack memory. Likely candidate is the `ResolveFault`
  wrapper's per-page allocation interacting with TStackInfo's
  `num_pages` / page_table accounting. Trace which part of the
  4-iter wrapper drifts the kernel's view.
- **If Einstein returns a different range (e.g. exactly 64 KiB)**:
  our kernel's NewStack monitor handler is computing the wrong
  bound. Find why and fix it.
- **If user-side `TRefStack`/`TRefStructStack` push counters are
  wrong on our run**: trace which push site overshoots and find the
  upstream divergence.

The fix should reduce the user-vs-kernel stack-extent diff to zero,
matching real-hardware behavior. The probes from this plan stay in
tree until we land the fix; they're cheap when filtered.

## Critical files

- `src/rom_patches.rs::apply_l1_cd_probes` — pattern to mirror for
  the new `Fill` and `NewStack` probes.
- `src/trap.rs::handle_remember_entry_probe_with` — pattern for the
  source-mode-aware handler (works from both privileged HVC and
  USR-trampoline UND paths).
- `src/trap.rs::handle_reboot` — extend the one-shot dump with
  TStackInfo / TRefStack / TRefStructStack state at the wedge.
- `INVESTIGATION.md` — keep updating the "Currently at" section as
  Step 1–4 land.

## Verification

- `guest-tests/scripts/run-all.sh` must remain green throughout
  (35/35).
- Cold-boot (`rm -f /tmp/newton-snapshot-*.bin` first) and look for
  the `Reboot canary fired` line at FAR=`0x0cd07400`. With the new
  Fill-call probe installed we should also see the SVC-mode caller
  PC immediately preceding the canary.
- The fix is correct when boot advances past the wedge to whatever
  the next stall is, without inventing new hypervisor abstractions.
