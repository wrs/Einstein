# Phase B boot-stall investigation

Live notes. Update as we learn more. REMOVE old updates once resolved.

## In progress — Step 6 probes: DAH-exit + NewState + mode-aware dabt dedup (QEMU, 2026-04-26 night-2)

**Plan reference:** `docs/plans/l1-cd-lazy-investigation.md` Step 6 (a/b/c).

Three new mechanisms installed:

1. **6a — mode-aware `log_dabt_forward` dedup.** Key now includes
   `pre_abt_mode = SPSR_abt & 0x1F`, so a USR-pre-abt and SVC-pre-abt
   fault at the same FAR show as distinct lines. Goal: surface any
   silently-handled USR-pre-abt fault at FAR=0x0cd07400 preceding the
   visible SVC-pre-abt one.
2. **6b — `NewState__11TIntrpStackFv` prologue probe (HVC #0x4D)** at
   PC 0x001A46F0. Logs source-mode CPSR + caller LR. Distinguishes
   "Fill is being called from a Fresh NewState in USR" vs. "Fill is
   being run on a stale stack from inside an interrupted handler".
3. **6c — DataAbortHandler `movs pc, lr` exit probe (HVC #0x4E)** at
   *both* 0x00393B80 (success exit, post-Scheduler) and 0x00393944
   (throw exit, tail-call into `Throw` at 0x01BE319C). Probe handler
   reads ELR_EL2 to identify the call site, logs the LR/SPSR_abt
   tuple, then emulates `movs pc, lr` by setting `ELR_EL2 := lr_abt`
   and `SPSR_EL2 := SPSR_abt` so the natural ERET out of EL2 mirrors
   the kernel's intended mode-flip + branch.

`/tmp/phaseB-l1cd-probe/qemu5.log` is the cold-boot artifact. 35/35
guest tests still green.

### Major model correction (vs. the previous framing)

The old story was "FaultMonitorEntry calls FaultMonProc → Fault →
ResolveFault from inside DAH; DAH exits to USR after the kernel-side
recovery is done". That's wrong.

New empirical pattern, repeated across at least five distinct USR-
pre-abt faults in this boot:

```
1. dabt: forwarding ... DFSC=0x7 FAR=0x... mode=0x17 (pre-abt USR)
2. DAH-exit probe (success @ 0x00393b80): src_mode=0x17 (ABT)
                                          lr_abt=0x01a00024
                                          spsr_abt=...0x10 (pre-abt USR)
3. Fault(stackmgr) probe ENTER: ... src_mode=0x10 (USR)
                                caller_lr=0x00259230 (= FaultMonProc)
4. ResolveFault probe ENTER × 4: ... src_mode=0x10 (USR)
                                 caller_lr=0x00fffe40 (= our wrapper)
```

The DAH success-exit fires **before** the Fault probe — every time.
That means the kernel's DAH does **not** synchronously call the
stack monitor. It exits to USR mode at LR_abt = `0x01a00024` (a
post-ship patch-table entry), and only *then* — running in USR mode
— does the kernel walk into FaultMonProc → Fault → ResolveFault
to do the actual page-table fix-up. Our 4-iter wrapper at
`0x00FFFE00` is reached from this USR-mode user code, not from
inside DAH.

This explains why all the existing Fault/ResolveFault probes show
`src_mode=0x10 (USR)`: they really are called from USR mode.

### Fault #2 (FAR=0x0cd07400) — DAH takes the *throw* exit

```
Fill probe ENTER: this=0x0c6451c0 caller_lr=0x001a4754 src_mode=0x10 (USR)
dabt: forwarding ... DFSC=0x5 FAR=0x0cd07400 mode=0x17
  LR_abt=0x001a4ba4 (faulting PC=0x001a4b9c) SPSR_abt=0x60000113 (pre-abt SVC)
  USR sp=0x0cc82660 lr=0x0cd07418  SVC sp=0x0c000400 lr=0x001a4708
  L1[0xcd] = 0x00000090 (fault)
DAH-exit probe (throw @ 0x00393944): lr_abt=0x01be319c
                                     spsr_abt=0x80000110 (pre-abt USR)
*** Reboot canary fired ***
  R14_UND=0x000d9888 (= Reboot+4)
```

Two things were confirmed:

- Fault #2's DAH exit is the **throw path** at `0x00393944`
  (`movs pc, lr` with `lr := 0x01be319c = Throw`). This is consistent
  with no Fault/ResolveFault probe firing between fault #2 entry and
  the Reboot canary — the kernel never invokes the stack monitor for
  this abort. It tail-calls `Throw` → `UnhandledException` → `Reboot`.

- The pre-abt mode at the throw exit is reported as **USR**
  (`spsr_abt=0x80000110`), even though `dabt: forwarding` 14 lines
  earlier reported pre-abt **SVC** (`spsr_abt=0x60000113`). The two
  reads use the same `read_banked_spsr("abt")` helper. Either:
  (i) the kernel writes SPSR_abt somewhere between DAH entry and the
  throw exit (no obvious site in the disasm 0x393114..0x393944), or
  (ii) the AArch64 view of SPSR_abt is stale by the time the probe
  fires (the documented QEMU raspi3b banked-reg flakiness; see
  `docs/QEMU_BUGS.md`). FVP would be a useful cross-check.

### Hypothesis (B) ruled out

> Recovery from fault #1 never returned to USR.

This is **wrong**. The DAH success-exit for fault #1 fires with
pre-abt USR (line 2182 in qemu5.log) and `lr_abt=0x01a00024` which
is a USR-mode patch-table entry. The kernel does return to USR.
Hypothesis (B) is dead.

### Hypothesis (A) — partially refuted

> A silently-handled earlier USR-pre-abt fault at FAR=0x0cd07400
> left CPSR in SVC.

The mode-aware dedup did **not** surface any USR-pre-abt fault at
FAR=0x0cd07400. Only the SVC-pre-abt one is logged. So if (A) holds,
the silent fault must be at a *different* FAR whose recovery somehow
left CPSR in SVC for the next access at 0x0cd07400 — there is no
direct evidence of one.

### The remaining mystery — USR→SVC mode flip inside Fill body

Fill enters at `0x1a4b54` in USR (`src_mode=0x10`, `sp=0x0cc82664`).
The body 0x1a4b58..0x1a4b98 is straight-line ARM with no `cps`,
`msr CPSR_*`, `bx pc`, or other mode-changing instruction:

```
1a4b54: stmfd sp!, {lr}    ; (probe-emulated)
1a4b58: ldr r1, [r0, #20]  ; r1 = TRefStruct cursor = 0x0cd07400
1a4b5c..1a4b88: arith chain  ; no faulting access (data is in r0/r1/r2/r3)
1a4b8c: cmp r1, lr          ; cursor vs loop bound
1a4b90: bcs 0x1a4ba8        ; not taken (cursor < bound)
1a4b94: mov r3, r2
1a4b98: add r2, r2, #4
1a4b9c: str r3, [r1], #4   ; FAULT — write to L1[0xCD]=0x90 lazy region
```

So if the Fill probe's USR-source identification is correct, the
str at 0x1a4b9c should fault from USR mode. The architecture-level
banked SP/LR view supports that:

- `USR sp=0x0cc82660` matches the probe-emulated `stmfd` push
  (0x0cc82664 - 4 = 0x0cc82660).
- `USR lr=0x0cd07418` matches Fill's bound-computation result at
  0x1a4b78 (`add lr, r3, lr, lsl #2`).

But `SPSR_abt=0x60000113 (SVC)` says the abort was taken from SVC.

Two surviving sub-hypotheses:

- **(C) QEMU AArch64↔AArch32 SPSR_abt flakiness** — `mrs spsr_abt`
  at EL2 doesn't always reflect the just-saved CPSR. We've seen this
  pattern enough times that an FVP cross-check is high-value.
  `docs/QEMU_BUGS.md` already documents related x[13]/x[14]
  misdiagnoses; this could be the SPSR sibling.

- **(D) An exception fired between Fill prologue and 0x1a4b9c that
  switched to SVC and never returned.** Candidates: alignment fault,
  external abort, async-exception. None expected, but the architecture
  permits asynchronous SError. Would need a probe at the SError /
  alignment-fault entry to rule out.

### Reproduction artifacts

- `/tmp/phaseB-l1cd-probe/qemu5.log` — quiet boot with all nine probes
  (`0x46–0x4E`).

### Next steps

1. **Cross-check on FVP.** If sub-hypothesis (C) is correct, FVP's
   accurate banked-reg model should show consistent SPSR_abt (= SVC
   at every read after the abort) or USR (if the abort really was
   USR). Either resolves the ambiguity.
2. **Probe the kernel's USR-mode fault dispatch path.** We now know
   Fault → ResolveFault are invoked from USR via the patch-table
   entry at `0x01a00024`. Trace what happens between DAH-exit and
   Fault probe ENTER for fault #1 — that's where the kernel's user-
   side stack-recovery state lives, and where the cause of the
   wedge most likely sits.
3. **Investigate why fault #2's DAH path takes the throw exit.** The
   SVC-pre-abt branch at 0x393158 (`bne 0x393898`) routes to a region
   that the disassembler labels `<UNDEFINED>`. The kernel's behaviour
   there is implementation-defined — we should figure out exactly
   what executes between 0x393898 and 0x3938c4 (the lr-load that sets
   up the Throw tail-call).

---

## Earlier — Fault/ResolveFault probes: kernel never reaches stack-monitor for fault #2 (QEMU, 2026-04-26 night)

**Plan reference:** `docs/plans/l1-cd-lazy-investigation.md` Step 5 prep.

Two new HVC probes installed by `apply_l1_cd_probes`:

- HVC #0x4B at `Fault__13TStackManagerFR15TProcessorState` entry
  (`0x001F83E4`, original `mov ip, sp` = `0xE1A0C00D`). Logs source-mode
  CPSR + caller LR + (manager*, processor_state*) + FAR (read from
  `processor_state[+0x44]`). Emulates the original `mov ip, sp`.
- HVC #0x4C at `ResolveFault__13TStackManagerFP10TStackInfo` entry
  (`0x001F7978`, original `mov ip, sp`). Logs source-mode CPSR + caller
  LR + (manager*, info*) + FAR + info bounds [info+0x18, info+0x1C).
  Captures both wrapper-driven calls (caller_lr=0x00fffe40 = WRAPPER+0x40)
  and direct calls (caller_lr=0x001f6b98 from FMLockHeapRange).

Fresh boot artifact at `/tmp/phaseB-l1cd-probe/qemu4.log`. 35/35 guest
tests still green. The new probes pin three things that were guesses
before:

### Fault #1 (USR-pre-abt, FAR=0x0ccee800) — fully recovered

```
dabt: forwarding ... DFSC=0x7 FAR=0x0ccee800 mode=0x17
  LR_abt=0x001a4710 (faulting PC=0x001a4708) SPSR_abt=0x60000110 (pre-abt USR)
Fault(stackmgr) probe ENTER: this=0x0c112cb8 procst=0x0c1133a4 far=0x0ccee800
                             caller_lr=0x00259230 src_mode=0x10 (USR)
ResolveFault probe ENTER: ... far=0x0ccee000 info_bounds=[0x0ccee800,0x0cd06800)
ResolveFault probe ENTER: ... far=0x0ccee400 info_bounds=[0x0ccee800,0x0cd06800)
ResolveFault probe ENTER: ... far=0x0ccee800 info_bounds=[0x0ccee800,0x0cd06800)
ResolveFault probe ENTER: ... far=0x0cceec00 info_bounds=[0x0ccee800,0x0cd06800)
Fill probe ENTER: this=0x0c6451c0 caller_lr=0x001a4754 src_mode=0x10 (USR)
```

- Fault is reached via `FaultMonProc__15TUDomainManager` (caller LR
  `0x00259230` = inside FaultMonProc just past `mov lr, pc; ldr pc, [r4]`,
  the vtable dispatch on the manager object).
- The 4-iter wrapper at `0x00FFFE00` runs all four iterations: subpages
  0/1 are below info_lo so ResolveFault returns -10203 fast; subpages
  2/3 land in [info_lo, info_hi) and complete the kernel's per-subpage
  bookkeeping. After the wrapper returns r0=0, `Fault` returns r5=0,
  FaultMonProc cleans up and the kernel ERETs back to USR — Fill probe
  fires next, confirming the recovery returned to USR mode at NewState's
  call site.
- `info[+20]` (info base_va) is `0x0ccee000`; `info[+24]` (lower bound)
  is `0x0ccee800`; `info[+28]` (upper bound) is `0x0cd06800`. The
  4-KiB-aligned page base for the fault is therefore `0x0ccee000`, which
  is why iters 0/1 fall below the lower bound — that's the documented
  "previous-stack-slot" guard inside the 4-iter wrapper, working as
  designed.

### Fault #2 (FAR=0x0cd07400) — kernel never invokes the stack monitor

```
dabt: forwarding ... DFSC=0x5 FAR=0x0cd07400 mode=0x17
  LR_abt=0x001a4ba4 (faulting PC=0x001a4b9c) SPSR_abt=0x60000113 (pre-abt SVC)
  USR sp=0x0cc82660 lr=0x0cd07418   SVC sp=0x0c000400 lr=0x001a4708
  r0=0x00000005 r1=0x80000110 r2=0x0ccee804 r3=0x0ccee800 r12=0x0cd07400
*** Reboot canary fired ***
  ELR_EL2=0x00ffff58 SPSR=0x000001db (UND, 0x1b)
  R0=0xffffd8a5 (= -10075)  R14_UND=0x000d9888 (= Reboot+4)
```

**Neither `Fault(stackmgr)` nor `ResolveFault` fires after the dabt
forward.** No `Remember` / `AllocatePageTable` probe fires either. So
the kernel's DataAbortHandler does not route this abort to the stack
monitor (FaultMonProc → Fault → ResolveFault), nor does it issue a
`Remember` SWI to grow `L1[0xCD]`. It walks straight to
`Reboot(-10075)`, reached via UND mode at `LR_UND=0x000d9888`
(`Reboot+4`) — i.e. the kernel raised an `UnhandledException` whose
trampoline ended up calling `Reboot` from UND.

The two faults differ in DFSC (#1 = `0x7` page translation; #2 = `0x5`
section translation) AND in pre-abt mode (#1 = USR; #2 = SVC). Either
gate could route the kernel into the panic path; the data so far
doesn't tell them apart.

### The mode-transition mystery

Fill enters at `0x1a4b54` in **USR** (probe captured `src_mode=0x10`
with the right USR sp/lr). Eight instructions later at `0x1a4b9c` the
str faults in **SVC**. There is no mode-changing instruction in
`0x1a4b58..0x1a4b98` — just loads/arith/cmp/bcs. Yet `SPSR_abt=0x60000113`
unambiguously says SVC at the moment of fault.

Two possibilities, neither yet ruled out:

1. **A USR-pre-abt fault at FAR=0x0cd07400 fired first, was handled
   silently (no Fault/ResolveFault), and the recovery left the CPU in
   SVC.** The `log_dabt_forward` dedup is keyed on (FAR, hvc_src_mode);
   `hvc_src_mode` is always ABT for the trampoline path, so any
   subsequent dabt at the same FAR is silently suppressed. We could be
   missing the first fault entirely. Action: lift the dedup (or print
   the first N occurrences instead of just one).

2. **The recovery for fault #1 never returned to USR — the kernel kept
   running in SVC and re-entered the same code path that NewState +
   Fill execute.** SVC `lr=0x001a4708` is fault #1's faulting PC, which
   is consistent with "kernel still has fault #1's saved-state staged
   for return-to-USR but is running other code in SVC in the meantime".
   In this story Fill's probe-time USR sp/lr reflect a stale snapshot
   from before fault #1, and the Fill body actually runs on top of
   SVC `sp=0x0c000400` / SVC `lr=0x001a4708`.

To pick between these, the next probe round needs to:

- Disable the `(FAR, mode)` dedup in `log_dabt_forward` for at least
  the section-`0xCD` range so we see every dabt occurrence in order.
- Add an HVC at the kernel's monitor-dispatch return path to log when
  the kernel transitions back to the original mode. Candidate sites:
  the `subs pc, lr, #N` inside DataAbortHandler that ends the
  USR-pre-abt path, and the equivalent at the SVC-pre-abt path. (Need
  to scan DataAbortHandler 0x393114..~0x393950 for those.)
- A short trace probe at NewState entry/exit (or at the trampoline
  function `0x1a54a38` that NewState tail-calls) could clarify whether
  Fill is actually called twice (once USR, once SVC) or just once.

### What's confirmed vs. still hypothesised

| Claim | Status |
| --- | --- |
| Wrapper iterates 4 times, two below-bound + two in-range | ✓ confirmed |
| Fault → ResolveFault both reached for USR-pre-abt fault | ✓ confirmed |
| Fault and ResolveFault both bypassed for SVC-pre-abt fault | ✓ confirmed |
| Reboot reason = -10075, called from UND mode | ✓ confirmed |
| Pre-abt mode is SVC despite Fill probe showing USR entry | ✓ confirmed |
| Mode flip happens because fault #1 recovery skipped USR-return | ⚠ hypothesis |
| The Fill probe's stmfd emulation is innocent | ⚠ hypothesis |

### Reproduction artifacts

- `/tmp/phaseB-l1cd-probe/qemu4.log` — quiet boot with all seven probes
  (`0x46–0x4C`).

---

## Earlier — Fill+NewStack probes pin wedge to TInterpreter ctor's TRefStructStack #2 (QEMU, 2026-04-26 late night)

**Plan reference:** `docs/plans/l1-cd-lazy-investigation.md` Steps 1–3 landed.

Two new probes installed by `apply_l1_cd_probes` in `src/rom_patches.rs`:

- HVC #0x49 at `Fill__15TRefStructStackFv` entry (`0x001A4B54`, original
  `stmfd sp!, {lr}` = `0xE92D4000`) — logs `this`, source-mode caller
  LR, source mode bits. Reachable from both handle_hvc and handle_und.
  Emulates the original `stmfd sp!, {lr}` (push LR onto source-mode
  stack, advance source-mode SP) so Fill continues correctly.
- HVC #0x4A at `NewStack` post-SWI (`0x001F89A8`, original
  `ldr r1, [sp, #16]` = `0xE59D1010`) — fires only on the success branch
  (the preceding `bne 0x1f89b8` skips it on SWI failure). Reads the SWI
  param block from source-mode SP — `[sp+0]=env, [sp+8]=req_size,
  [sp+16]=out_top, [sp+20]=out_base` — and logs them along with the
  real caller PC pulled from `[fp-4]` (the bl-saved LR; `lr_for_mode`
  alone returns NewStack's own clobbered LR after `bl MonitorDispatchSWI`).
  Emulates `ldr r1, [sp, #16]` so r1 := out_top for the next `str r1, [r4]`.

Step 2 also extends `handle_reboot` with a ring buffer of recent
NewStack outputs and the most recent Fill `this` pointer, then dumps
the live `TRefStructStack` object's first six words at the wedge so we
can pin `(TRefStack cursor, base; TRefStructStack base, cursor)` exactly.

### What the new probes show

```
NewStack ring (last 8, IDs are seq):
  # 14  caller_lr=0x00252390  env=0x1355  req=0x08400  base=0x0c321800  top=0x0c329000  span=0x07800
  # 15  caller_lr=0x00252390  env=0x13a5  req=0x08400  base=0x0ccde000  top=0x0cce5800  span=0x07800
  # 16  caller_lr=0x00252390  env=0x13a5  req=0x08400  base=0x0cce6400  top=0x0ccedc00  span=0x07800
  # 17  caller_lr=0x00252390  env=0x13a5  req=0x08400  base=0x0cc7b000  top=0x0cc82800  span=0x07800
  # 18  caller_lr=0x001a4948  env=0x13a5  req=0x10c00  base=0x0ccee800  top=0x0cd06800  span=0x18000  ← TRefStack
  # 19  caller_lr=0x001a4adc  env=0x13a5  req=0x10c00  base=0x0cd07400  top=0x0cd1f400  span=0x18000  ← TRefStructStack (the wedge stack)
  # 20  caller_lr=0x001a4948  env=0x13a5  req=0x10c00  base=0x0cd20000  top=0x0cd38000  span=0x18000
  # 21  caller_lr=0x001a4adc  env=0x13a5  req=0x10c00  base=0x0cd38c00  top=0x0cd50c00  span=0x18000

Fill probe ENTER: this=0x0c6451c0 caller_lr=0x001a4754 src_mode=0x10 (USR) sp=0x0cc82664
TRefStructStack object @ 0x0c6451c0:
  this->[+ 0] = 0x0ccee818   ; TRefStack cursor (= base + 0x18 = 6 entries pushed)
  this->[+ 4] = 0x0ccee800   ; TRefStack base
  this->[+ 8] = 0x0cceec98   ; TRefStack page-bound (= base + 0x498)
  this->[+12] = 0x0000012c   ; entries-per-page constant (300)
  this->[+16] = 0x0cd07400   ; TRefStructStack base
  this->[+20] = 0x0cd07400   ; TRefStructStack cursor (initial = base)
derived: TRefStack pushed = 0x18, Fill loop bound = TRefStruct base + pushed = 0x0cd07418
```

### Mechanism, no remaining ambiguity

The **TRefStructStack ctor** at `0x1a4a78` calls `NewStack(env, 0x10000)`
**twice**: once via the inherited TRefStack ctor at `0x1a48e4` (returning
to `0x1a4948`), once for itself (returning to `0x1a4adc`). Each time
the kernel grants a 96 KiB span (`req=0x10c00 → span=0x18000`) — the
small earlier stacks were 30 KiB each, but the four right before the
wedge are all 96 KiB.

`__ct__15TRefStructStackFv` stashes the second `NewStack` BASE in both
`self->[16]` and `self->[20]`, identical to how TRefStack ctor stores
the first NewStack BASE in `self->[0]/[4]`. So one TRefStructStack object
spans **two disjoint allocations**: TRefStack at `[0x0ccee800, 0x0cd06800)`
and TRefStructStack at `[0x0cd07400, 0x0cd1f400)`. The 3 KiB gap in
between is the kernel's standard inter-stack guard.

`NewState__11TIntrpStackFv` (entry `0x1a46f0`) writes 6 words to
TRefStack starting at `self->[0]` (= `0x0ccee800`) — those 6 stores
are the first writes to the new TRefStack region, and the kernel's
ResolveFault wrapper handles the resulting fault on FAR=`0x0ccee800`,
growing `L1[0xCC]` from `0x90` lazy → coarse. The 6 stores complete
and the cursor advances by `0x18` to `0x0ccee818`.

NewState then conditionally tail-calls Fill (via `bllt 0x1a54a38` at
`0x1a4750` — that intermediate function tail-jumps into Fill so the
Fill-side `caller_lr=0x1a4754` matches NewState's return point). The
Fill loop bound, computed as `self->[16] + (self->[0] - self->[4])` =
`0x0cd07400 + 0x18` = `0x0cd07418`, is fine. The first store at
`0x1a4b9c` (`str r3, [r1], #4`, with r1 = TRefStructStack cursor =
`0x0cd07400`) writes to TRefStructStack base. Section `0xCD` is still
`L1[0xCD]=0x90` lazy — the second NewStack call set L1[0xCD..0xD0] to
lazy markers but nothing has touched any of those pages yet, so
ResolveFault never ran for them. Data abort.

The kernel's data abort handler runs in SVC mode (the second fault in
the trace shows `SPSR_abt=0x60000113` → pre-mode = SVC). The handler
itself is at PC `0x1a4b9c` — i.e. the recovery from the first fault
left the CPU executing Fill in SVC, not USR. From SVC the kernel can't
recursively re-enter the data-abort handler for FAR=`0x0cd07400`, and
it Reboots.

### Why `req=0x10c00` but `span=0x18000`?

Both ctors literally do `mov r1, #0x10000` (64 KiB request) before
`bl NewStack`, so the **kernel modifies `[sp+8]` mid-SWI**. The probe
reads it after the SWI returns. Inferred: `[sp+8]` post-SWI is the
adjusted size with kernel-side per-stack housekeeping included; the
`span = top - base` is the actual usable range. For the small stacks
the relationship is `req = span + 0xC00` (0xC00 = 3 KiB inter-stack
guard), so `req` = slot pitch. For the 96-KiB stacks the kernel grants
much more than the small-slot pattern would predict. We don't yet know
why the same code path requesting 64 KiB sometimes lands in 30-KiB
slot pitch (`req=0x8400`) and sometimes in 96-KiB span (`req=0x10c00`,
`span=0x18000`). Likely a per-task or per-domain config.

### Einstein/HW cross-check (Step 4)

`baremetal/probe/results-717006-90s-full.txt` (real Newton in Einstein
emulator, 90 s wall-clock boot) shows for the same VA range:

```
VA 0x0CCEF000 to 0x0CD07000 (96 kB): page fault     ; same coarse L2 → unallocated tail of TRefStack
VA 0x0CD07000 to 0x0CD08000 ( 4 kB): small pages    ; TRefStructStack base — first page allocated
VA 0x0CD08000 to 0x0CD20000 (96 kB): page fault     ; rest of TRefStructStack lazy
VA 0x0CD20000 to 0x0CD21000 ( 4 kB): small pages    ; next TRefStack base
VA 0x0CD21000 to 0x0CD38000 (92 kB): page fault
VA 0x0CD38000 to 0x0CD39000 ( 4 kB): small pages    ; next TRefStructStack base
...
```

The `page fault` lines are L2-fault entries inside a coarse L1 table —
that is, **L1[0xCD] is already coarse on real HW**, with `small pages`
allocated for the very first 4-KiB page of every NewStack region (the
base page) and the rest left lazy. So the kernel's intended pattern is:

1. NewStack reserves the region by setting `L1[i] = 0x90` for sections
   that are still fault-class.
2. On the first write to any page in that region, the data abort
   handler grows `L1[i]` from `0x90` lazy → coarse and allocates the
   touched 4-KiB page (`small pages`).
3. Subsequent writes to *other* pages in the same section take L2-level
   page faults; the same handler chain allocates them on demand.

So Fill writing to `0x0cd07400` is a normal lazy grow on real HW. Our
hypervisor reboots instead. The kernel-side handler reaches Reboot
without ever invoking the `Remember` SWI for section 0xCD — confirmed
by the absence of any `Remember probe ENTER` line for VA in section
`0xCD` in `/tmp/phaseB-l1cd-probe/qemu3.log`.

### Where the recovery goes wrong

The two-fault transcript:

```
dabt #1: DFSC=0x7 FAR=0x0ccee800 SPSR_abt=0x60000110 (pre-mode=USR)
   stage1 walk: L1[0xcc]=0x04023481 (coarse), L2[0xee]=0  (page fault)
   handled → returns to USR, NewState completes 6 pushes, calls Fill

dabt #2: DFSC=0x5 FAR=0x0cd07400 SPSR_abt=0x60000113 (pre-mode=SVC!)
   stage1 walk: L1[0xcd]=0x00000090 (lazy)
   PC at fault = 0x1a4b9c (Fill body)
   USR sp=0x0cc82660 USR lr=0x0cd07418 (= Fill loop bound)
   handler reaches Reboot
```

The pre-mode for fault #2 is **SVC**, not USR — even though Fill was
called from USR (per our HVC #0x49 probe) and PC `0x1a4b9c` is plain
user-API code. So between the recovery from fault #1 and fault #2 the
CPU transitioned USR→SVC while still at PC `0x1a4b9c`. Newton's data
abort handler is entered in ABT mode and, for stack-fault recovery,
trampolines into SVC to run `TStackManager::Fault` → `ResolveFault`
(the patched call site at `0x001f84e0` that we re-target to our
ResolveFault wrapper). The wedge sequence appears to be:

  fault #1 (USR) → ABT vector → DataAbortHandler dispatches to
  TStackManager::Fault → SVC mode → our ResolveFault wrapper grows
  L1[0xCC] → returns to TStackManager::Fault → which **re-tries the
  failed instruction in SVC mode** (instead of returning to USR via
  ERET) → the failed instruction is now Fill's first store at
  `0x0cd07400` → fault #2 (SVC pre-mode) → DataAbortHandler can't
  re-enter SVC recovery from inside SVC → Reboot.

The "re-tries in SVC mode" step is hypothesised, not observed. To
confirm, the next session should:

- Trace the kernel's data abort handler entry/exit (HVC at
  `0x00393114` and at the handler's return path) so we can see the
  CPSR transitions across the recovery.
- Capture which sysreg instruction transitions USR→SVC at PC
  `0x1a4b9c` — likely a `movs pc, lr` or `subs pc, lr, #N` in the
  handler that doesn't restore the original SPSR correctly.
- If the kernel does mean to retry in SVC, then the wedge isn't
  about mode at all and the real bug is that ResolveFault for
  FAR=`0x0cd07400` should handle it but doesn't. Add a probe at
  `TStackManager::ResolveFault` entry to log every call and see
  whether the second-fault path even reaches it.

### What still needs to land

1. **Plan Step 4 (cross-check)** — done, captured above.
2. **Plan Step 5 (fix)** — blocked on understanding the USR→SVC mode
   transition between fault #1 and fault #2. Don't apply a guess fix;
   add the probes above first.

### Reproduction artifacts

- `/tmp/phaseB-l1cd-probe/qemu3.log` — quiet boot with all five probes
  installed (`0x46–0x4A`). 21 NewStack outputs captured before the
  wedge; the four 96-KiB stacks (`#18`–`#21`) are the TInterpreter
  TRefStructStack-pair allocations. The Reboot-canary state dump
  includes the live TRefStructStack-object snapshot quoted above.

---

## Earlier — Remember/AllocPT probes: wedge is FILL-into-0xCD from SVC, not lazy-grow failure (QEMU, 2026-04-26 evening)

**Plan reference:** `docs/plans/l1-cd-lazy-investigation.md` step 1.

Three HVC probes installed by `apply_l1_cd_probes` in `src/rom_patches.rs`:

- HVC #0x46 at `Remember (static)` entry (`0x00258E0C`) — logs args + L1
  entry when the target VA's L1 is a lazy fault marker (low 2 bits = 00,
  non-zero) or section is exactly 0xCD. Also handled in handle_und so
  USR-mode callers (trampolined to UND) work too. Emulates the
  original `mov ip, sp` so the function prologue continues correctly.
- HVC #0x47 at `Remember` post-SWI (`0x00258E50`) — logs r0 (= SWI #12
  return) when the entry probe flagged this call interesting. Emulates
  `mov r8, #237`.
- HVC #0x48 at `AllocatePageTable (static)` entry (`0x00259104`) — logs
  caller LR. Emulates `mov r2, #0`.

Output for the cold-boot run that reaches the Reboot canary:

```
L1[0xc2]=0x00000070 → SWI ret -10003 → AllocatePageTable → retry succeeds
L1[0xc6]=0x00000090 → SWI ret 0 (kernel monitor grew lazy implicitly)
L1[0xc9]=0x00000090 → SWI ret 0
L1[0xca]=0x00000090 → SWI ret 0
L1[0xcc]=0x00000090 → SWI ret 0
L1[0xc3]=0x00000070 → -10003 → AllocatePageTable → retry
L1[0xd6]=0x000000b0 → -10003 → AllocatePageTable → retry
... (no Remember call ever targets section 0xCD)
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0ccee800 mode=0x17 (USR)
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x0cd07400 mode=0x17 SPSR=0x60000113 (SVC!)
*** Reboot canary fired ***
```

### Key takeaway: the wedge is NOT a lazy-L1 grow failure

The kernel CAN grow `L1[i]=0x90` lazy markers — it did so successfully
for sections 0xC6, 0xC9, 0xCA, 0xCC during this same boot, via the
static `Remember` SWI #12 path. Notably for the `0x90` (domain=4)
marker the kernel's monitor handler grows the entry IMPLICITLY on a
`Remember(va, perm=0, ...)` call: SWI returns 0 immediately and
AllocatePageTable is NOT invoked. (For the `0x70` and `0xb0` markers
on other domains the kernel does take the -10003 → AllocatePageTable
→ retry path.) Both work.

So the framing in the plan ("L1[0xCD] = 0x90, never grown") is correct
but its cause is upstream: **no Remember call ever targets section
0xCD** — not because the kernel can't grow it, but because the kernel
never gets a request to grow it.

### What actually fails

The wedge fires when the kernel's exception handler is itself running
in **SVC mode** (`SPSR=0x60000113`) at `Fill__15TRefStructStackFv`
PC `0x001a4b9c` and writes to FAR=`0x0cd07400`. The L1 walk finds
`L1[0xCD]=0x90` (lazy), the handler can't recursively re-enter
fault-recovery from inside its own SVC handler, and it Reboots.

The TStackInfo for this stack region (per the wrapper-entry probe at
FAR=`0x0ccee800` recorded above) has bounds `[0x0ccee800, 0x0cd06800)`.
The faulting VA `0x0cd07400` is **3 KiB past the kernel-tracked top**:

```
TStackInfo info[+24] = 0x0ccee800   ; LOWER bound (kernel-granted)
TStackInfo info[+28] = 0x0cd06800   ; UPPER bound (kernel-granted)
fill_cursor = 0x0cd07400            ; user-side write attempt
                                    ;       = top + 0x0c00 (3 KiB over)
USR lr (in fault dump) = 0x0cd07418 ; user-side fill loop end target
```

So the user-side `Fill_TRefStructStackFv` cursor advanced past the
kernel-side `TStackInfo` upper bound. The kernel only saw, and grew,
sections up to 0xCC; the next fault was already past its bound, so
TStackManager couldn't pick a TStackInfo for it.

### Why does Fill walk past the kernel bound?

The `TRefStructStack::Fill` loop (disasm at `0x1a4b54`–`0x1a4ba8`)
mirrors entries from a sibling `TRefStack` into the TRefStructStack
region. The loop bound is `self->[16] + 4 * (TRefStack pushed bytes /
4)` — i.e. proportional to pushes on the *other* stack. If the user
code pushes more on TRefStack than the TRefStructStack region can
accommodate, Fill walks past the end.

`NewStack(0x10000)` requested 64 KiB but the kernel allocated 96 KiB
(`info[+28] - info[+24] = 0x18000`). User-side walked to ~99 KiB
beyond base. So the user's view of the stack and the kernel's view
disagree by 3 KiB. The actual divergence is somewhere in the
NewStack-result handshake or in TRefStack/TRefStructStack's accounting.

This is also notable: the fault is in **SVC mode**, and `Fill` is a
USER-API function with no `bl 0x1a4b54` references in the entire ROM.
So the kernel must be reaching Fill via either (a) a post-ship patch
table redirect that we haven't found, or (b) the kernel exception
handler emulating a user instruction that branches into Fill. Either
way, the kernel-side context calls a user function that walks past
its bound.

### Open next steps

1. **Find the SVC-mode caller of Fill.** Examine `SP_svc=0x0c000400`
   and unwind the SVC stack at the moment of the wedge. Patch the
   first word of `Fill_TRefStructStackFv` with `HVC` to log who calls
   it (and from what SVC-LR).
2. **Trace TRefStack pushes leading to the overflow.** TRefStack ctor
   stores `self->[0] = self->[4] = HIGH` (top of allocated region).
   Each push presumably advances `self->[0]`. After enough pushes,
   `self->[0] - self->[4] > granted_size` → Fill writes past granted
   region. Find what advances `self->[0]` past granted_size.
3. **Compare NewStack-output handshake vs Einstein.** Real Newton and
   Einstein both run this code without wedging, so the granted_size
   must equal the user-expected size on those platforms. Determine
   whether our hypervisor's TStackInfo bound differs (96 KiB vs the
   expected ~256 KiB or whatever the user expects) and trace the
   divergence to its source.
4. **Cross-check the per-page wrapper's interaction with NewStack.**
   The ResolveFault wrapper claims 4 subpages per fault — does that
   cause TStackInfo's granted-size accounting to be inconsistent with
   the user-passed size?

### Reproduction artifacts

- `/tmp/phaseB-l1cd-probe/qemu2.log` — quiet boot with the three
  probes installed. 9 interesting probe events captured before the
  Reboot canary fires.

---

## Earlier — ResolveFault wrapper: 4-iter call-the-allocator-per-subpage (QEMU, 2026-04-26 evening)

**Status:** Restructured the per-page stack-allocation fix. Removed the three
`mov r3, #0xF` patches at the `bl FindOrAllocPage` sites and replaced them
with a thin **wrapper** at `0x00FF_FE00` that re-runs the whole
`TStackManager::ResolveFault` four times per kernel-side fault — once per
1-KiB subpage of the faulting 4-KiB page. The wrapper:

1. Reads the original FAR from `this->[+64]->[+68]` and saves it (r8).
2. Computes the 4-KiB page boundary *relative to* `info->[+20]` — adjacent
   stack slots in `FMNewStack` sit 33 KiB apart, so `info->base_va` isn't
   4-KiB-aligned in general.
3. Loops `r10 = 0..3`, sets FAR to `page_base + r10*1024`, calls the real
   `ResolveFault`. Treats `r0 == -10203 / -10204` (out of bounds) as
   "subpage belongs to another stack — skip"; only propagates `r0 == 4`
   (FindOrAllocPage failure) to the wrapper's caller.
4. Restores the original FAR and returns 0 on success.

Patches the single `bl ResolveFault` site in `TStackManager::Fault` at
`0x001F_84E0` to call the wrapper instead. The other call site in
`FMLockHeapRange` (`0x001F_6B94`) is intentionally left untouched —
patching both broke early BootOS bring-up.

The wrapper makes the kernel's per-subpage bookkeeping (refcount0[sub_idx],
RememberMappings perm bits, SetRestrictedPage state) match the physical
reality that all four subpages of the page are accessible after first
allocation, since ARMv7's loss of subpage-AP otherwise leaves the kernel's
view diverged.

Boot now reaches the **same wedge as the original 3-PATCH baseline**:
- 6 forwarded kernel DABTs handled successfully via the wrapper.
- 7th forwarded DABT at FAR=`0x0cd07400` (DFSC=5, L1[0xCD]=`0x00000090`
  lazy) wedges with `Reboot(-10075)`.

So the wrapper structurally replaces the 3-PATCH set with no regression
and no advancement — it gets us to the same point cleanly.

**Next:** address the L1[0xCD]=0x90 lazy-section wedge (the remaining Phase
B goal). Options remain those documented earlier in this file:
hypervisor-side L2 coarse-table pre-allocation for lazy L1 entries, or
tracing what the kernel's domain-monitor does for DFSC=5 vs DFSC=7.

### TStackInfo layout (correction)

Direct dump from a wrapper-entry probe at FAR=`0x0ccee800` corrected the
field interpretation. For a `NewStack(0x10000)` allocation in this run:

```
info[+ 0] = 0x0cd06800   ; (some "top" — unclear)
info[+ 4] = 0x0ccee800   ; (some "base" — unclear)
info[+ 8] = 0x0000001a   ; num_pages = 26 (NOT NewStack-size / 4 KiB)
info[+12] = 0x00003063   ; flags?
info[+16] = 0x0c122030   ; page_table[]
info[+20] = 0x0cced000   ; offset basis for (FAR - this) >> 10
info[+24] = 0x0ccee800   ; LOWER bound (FAR must be >= this)
info[+28] = 0x0cd06800   ; UPPER bound (FAR must be < this)
info[+32] = 0x00000000   ; flags
info[+36] = 0x000013a5   ; domain (env id)
```

So the bound check is `info[+24] <= FAR < info[+28]`. The "+20 offset
basis" is *separate* from the "+24 bound base" — adjacent stacks pack
into 33-KiB slots within a domain page, so `info[+20]` may sit 1-3 KiB
*below* `info[+24]`. The wrapper has to be aware of this when computing
which subpages of the page belong to *this* stack.

---

## Earlier — wedged inside `TInterpreter::TInterpreter` constructor (Phase B goal reached!) (QEMU, 2026-04-26 afternoon)

**Status:** Boot reached **`TInterpreter::TInterpreter` at 0x002F40E0** — the
literal goal of Phase B per `PLAN.md`. Wedge was *inside* the constructor's
first call to `TIntrpStack::NewState` after both `TRefStructStack` sub-objects
have been constructed.

### Reboot-canary fire signature (no-trace, repro at trace ~270k)

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0ccee800 mode=0x17
  LR_abt=0x001a4710 (faulting PC=0x001a4708) SP_abt=0x0c004c00 SPSR_abt=0x60000110 (pre-abt mode=0x10)
  USR sp=0x0cc82664 lr=0x002f41ac   SVC sp=0x0c000400 lr=0x003ae324
  r0=0x00000007 r1=0x00000110 r2=0x0c606ea8 r3=0x0c105560 r12=0x0cc82678
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x0cd07400 mode=0x17
  LR_abt=0x001a4ba4 (faulting PC=0x001a4b9c) SP_abt=0x0c004c00 SPSR_abt=0x60000110 (pre-abt mode=0x10)
  USR sp=0x0cc82660 lr=0x0cd07418   SVC sp=0x0c000400 lr=0x001a4708
  r0=0x00000005 r1=0x80000110 r2=0x0ccee804 r3=0x0ccee800 r12=0x0cd07400

*** Reboot canary fired ***
  ELR_EL2=0x00ffff58 (= UND-trampoline HVC slot — fired via UND→trampoline→HVC #0x43,
                        the canary patched at REBOOT_PC = 0x000d9884 from USR mode)
  SPSR_EL2=0x000001db mode=UND (0x1b)
  R0=0xffffd8a5  R3=0x7fffffce  R12=0x0cc82544
  R14_UND=0x000d9888 (= UND-entry-set return address; the architectural caller LR
                        is in R14_USR which the canary printer needs to be taught
                        to read for this UND-from-USR path)
```

R0 = `0xffffd8a5` = `(0xa5 - 0x2800)` is the literal computed at
`UnhandledException` 0xb02b4-0xb02bc — the "evt.ex.abt.bus, warm
reboot!" exception code path.

### Root path

USR `lr=0x002f41ac` from the first DABT lands inside
`__ct__12TInterpreterFv` immediately after `bl NewState__11TIntrpStackFv`
at PC `0x002f41a8`. Disassembly excerpt:

```
002f40e0 <__ct__12TInterpreterFv>:
  ...
  2f410c: bl __ct__15TRefStructStackFv   ; construct stack #1 at self+8
  2f4114: bl __ct__15TRefStructStackFv   ; construct stack #2 at self+0x20
  ...                                     ; AllocateRefHandle x6 + struct init
  2f41a0: mov r0, r5                     ; r0 = self+8 (TRefStructStack #1)
  2f41a8: bl NewState__11TIntrpStackFv   ; ← USR lr at 1st DABT lands here (+4)
  2f41ac: str r0, [r4, #76]
  2f41b0: ldr r0, [r0]
  2f41b4: str r6, [r0]
  2f41b8: mov r0, r4
  2f41bc: bl SetFastLoopFlag__12TInterpreterFv

001a46f0 <NewState__11TIntrpStackFv>:
  1a46f0: mov ip, sp
  1a46f4: push {r4, fp, ip, lr, pc}
  ...
  1a4700: ldr r0, [r0]                   ; r0 = self->[0] = stack1_base
  1a4704: mov r1, #2
  1a4708: str r1, [r0]                   ; ← FAULT #1: write to 0x0ccee800
  1a470c: str r1, [r0, #4]
  ...

001a4b54 <Fill__15TRefStructStackFv>:
  1a4b54: stmfd sp!, {lr}
  1a4b58: ldr r1, [r0, #20]              ; r1 = self->[0x14] = stack2_top
  ...
  1a4b94: mov r3, r2
  1a4b98: add r2, r2, #4
  1a4b9c: str r3, [r1], #4               ; ← FAULT #2: write to 0x0cd07400
  1a4ba0: cmp r1, lr
  1a4ba4: bcc 0x1a4b94
```

`TRefStructStack::TRefStructStack` (and its `TRefStack` base) each call
`NewStack(0x10000)` (= 64 KB lazy-grow region via `MonitorDispatchSWI`
sub-fn 1, dispatched to `TStackManager`). For each
`TRefStructStack` we allocate **two** 64-KB stacks (one in TRefStack
base ctor, one in the TRefStructStack ctor itself), and the
TInterpreter has **two** TRefStructStack sub-objects → **4× 64 KB =
256 KB** of lazy-grow stack memory allocated during construction.

### Hypothesis

The first DABT (DFSC=0x7 FAR=0x0ccee800) is a **page-translation
fault inside an existing L1 coarse table** — the kernel's lazy-grow
handler can serve this. The second DABT (DFSC=0x5 FAR=0x0cd07400) is
a **section-translation fault** — there is no L1 entry for section
0xCD at all, so the kernel's per-page growth path can't help.

If `NewStack` returns a base inside section 0xCC and the lazy region
spans into section 0xCD, the kernel needs section 0xCD's L1 entry to
be pre-allocated (with an empty L2) before the per-page grow path can
fill in pages. Either:

1. The kernel normally pre-allocates the L1 entry for the entire
   `NewStack` region but doesn't because of state corrupted upstream.
2. Our `mask=0xF` per-page subpage-flatten patch
   (`PATCHES_717006::TStackManager::ResolveFault`) collides with
   `NewStack`'s allocator — `NewStack` pages might be supposed to
   share 4 KB pages with other stack subpages, but our mask now grabs
   the whole page each fault, exhausting the L1 coarse-table reserve
   sooner than expected.
3. `NewStack` is supposed to allocate the full 64 KB up-front (and
   does, on Einstein) but the kernel's `MonitorDispatchSWI` path is
   miscomputing or short-allocating in our run.

### Stage-1 walk evidence (notrace v3 / v4 with enhanced log)

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0ccee800 mode=0x17
  stage1 walk VA=0x0ccee800:  L1[0xcc] = 0x04025481  (coarse)
    coarse L2 @ PA 0x4025400, L2[0xee] = 0x00000000  (fault)

dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x0cd07400 mode=0x17
  stage1 walk VA=0x0cd07400:  L1[0xcd] = 0x00000090  (fault)
    L1 neighbourhood around section 0xcd:
      L1[0xc9] = 0x0401c481  (coarse)
      L1[0xca] = 0x0401c081  (coarse)
      L1[0xcb] = 0x00000090  (fault)
      L1[0xcc] = 0x04025481  (coarse)
      L1[0xcd] = 0x00000090  (fault)  ← here
      L1[0xce] = 0x00000090  (fault)
      L1[0xcf] = 0x00000090  (fault)
      L1[0xd0] = 0x00000090  (fault)
      L1[0xd1] = 0x00000090  (fault)
```

`0x90` is the kernel's **lazy/unallocated L1 marker** — type=00 (fault) with
`bits[8:5]=0x4` (domain field set, picking out a fault-monitor domain via
`GetDomainAndFaultMonitorFromDomainNumber` at ROM 0x1bd39b4) and bit 4 set.
The kernel fills "reserved-for-future-allocation" sections with this
pattern at MMU init and grows them coarse-by-coarse on first fault.

`probe/results-717006-90s.txt` (Einstein eventual-state MMU dump)
shows section 0xCD fully populated as a coarse table with a sparse
small-page pattern (e.g. `VA 0x0CD07000..0x0CD08000: small pages`).
Einstein routinely lazy-grows section 0xCD in this run; our run wedges
on the first attempt.

### Why the kernel can't grow section 0xCD in our run

`DataAbortHandler` (`0x00393114`) dispatches by `DFSR.FS[3:0]` via a
jump table at PC `0x39329c`:

```
DFSC=0/2:  0x3932dc  (alignment-ish / fall-through)
DFSC=1/3:  0x3932fc  (alignment fault)
DFSC=4:    0x39339c  (page-fault throw)
DFSC=5/7:  0x393314  ← page/section translation: domain-monitor dispatch
DFSC=6:    0x39339c
```

Both DFSC=5 (section translation) and DFSC=7 (page translation) land at
`0x393314`, so the kernel *intends* to handle both via the same
`GetDomainAndFaultMonitorFromDomainNumber` lookup. The kernel pulls the
domain index from `DFSR.bits[7:4]`. For our `L1[0xCD]=0x90`, the domain
field is 4 (`bits[8:5]=0b0100`), so the kernel will dispatch to
"domain 4's fault monitor".

The wedge fires at `Fill__15TRefStructStackFv` PC `0x1a4b9c`
(`str r3, [r1], #4`) where `r1 = self->[0x14]` = the `Fill` write head
that has just rolled into section 0xCD. **Either** the fault monitor
for domain 4 is not handling DFSC=5 the same as DFSC=7 in our run,
**or** something upstream in the kernel's domain-monitor / TStackManager
path has been perturbed by our `PATCHES_717006::TStackManager::ResolveFault`
mask=0xF patch (the same patch that resolved the BootOS-canary wedge).

### Suspect: mask=0xF interacts with NewStack-via-MonitorDispatchSWI

`NewStack` (`0x001f8968`) is a thin SWI wrapper over
`MonitorDispatchSWI` sub-fn 1, which dispatches into the
TStackManager monitor. With `mask=0xF` we force `ResolveFault` to take
all four subpages of every faulting page, which fixed BootOS-canary but
may now starve the lazy-section-grow path of free pages, leaving lazy
L1 entries (the 0x90 marker) unconverted to coarse.

### Open next steps

1. **Trace what the kernel's domain-monitor does for DFSC=5 vs DFSC=7.**
   Disassemble the `GetDomainAndFaultMonitorFromDomainNumber` (`0x1bd39b4`)
   handler chain and verify that domain 4's monitor for "section grow"
   path is reachable / non-null in our run. Probably involves dumping
   the relevant `gKernelGlobals` struct at the moment of the second
   DABT.
2. **Try reverting `mask=0xF` and add a different fix for the BootOS
   canary** — maybe a more surgical patch that doesn't perturb the
   domain-4 fault-monitor invariants. Side branch / experiment.
3. **Pre-allocate L1 entries in the hypervisor.** When we see the
   guest's L1[i]=0x90 marker and a DFSC=5 forward, install a coarse
   L2 table on the kernel's behalf and rewrite the L1 entry to a
   real coarse type. Brittle — cuts across the kernel's expected
   domain-monitor flow — but would unblock investigation.
4. **Cross-check on FVP.** Validate that the same wedge happens
   identically on FVP — confirms it's kernel-logic-deterministic, not
   a QEMU-AArch64 banked-reg artefact.

### Experiments performed (2026-04-26 PM continuation)

**Experiment A — disable patch (1/3) at `0x001f7a10` (normal-fault path).**
Hypothesis: patch (1/3) was the one starving the lazy-L1 grow; (2/3)+(3/3)
on the collision-grow paths were sufficient for the BootOS-canary fix.
Result: **REJECTED.** Without (1/3), boot wedges earlier with a different
BootOS-canary signature (R0=0, R2=0x0cc82604, R12=0x0cc8260f, snapshot
seq=46 vs original ~46 — but earlier in the trace timeline). Patch (1/3)
is load-bearing for past-stage progress. Reverted.

**Experiment B — normalise the 0x90 lazy-L1 marker to 0 in
`fix_stage1_xn_bits`.** Hypothesis: maybe the kernel's grow path expects
canonical fault (0) rather than annotated fault (0x90) and our
`fix_stage1_xn_bits` is preserving 0x90 unnecessarily. Result:
**REJECTED.** Boot wedges very early with `task_dump: gScheduler unset`
and `Reboot canary: R0=0xffffd8a4` (different code than the prior
0xffffd8a5; -10204 ≠ -10075). The 0x90 markers carry information the
kernel relies on — bit 4 may be the "lazy-domain-4-region" sentinel and
zeroing it breaks early scheduler init. Reverted.

**Conclusion of A+B:** the wedge isn't on either of the two simplest
local levers. The next investigation needs to step inside the kernel's
DABT-handler dispatch to determine *why* the kernel's grow path doesn't
fire on the DFSC=5 forward at FAR=0x0cd07400, while the equivalent
DFSC=7 at FAR=0x0ccee800 (same domain 4) IS handled successfully. A
hypervisor probe at `0x393894` (throw fast-path) and `0x39339c`
(success path) would directly reveal which arm the kernel takes.

**Experiment C — track L1[0xCD] across SCTLR M=0→M=1 transitions.**
Result: only TWO transitions seen across the entire boot up to wedge:

```
L1[0xcd] probe: transition #1    0xdeadbeef -> 0x00000000
L1[0xcd] probe: transition #4121 0x00000000 -> 0x00000090
```

Reading: the kernel writes L1[0xCD] = 0 (canonical fault) at boot
init (#1), then later (#4121, presumably after `BuildDomainsAndHeaps`
or similar domain-init code) writes the lazy marker `0x90`. **The
kernel never grows L1[0xCD] past `0x90`** in our run, despite the
DFSC=5 fault at FAR=0x0cd07400. So the kernel's
`FaultMonitor → ResolveFault → RememberMappings → Remember →
GenericSWI #12 → AllocatePageTable` chain (which we know exists at
`0x258e0c`) never fires for our DFSC=5 case. Either the chain is
short-circuited earlier, or the FaultMonitor never even hands off
to ResolveFault for stack2's TStackInfo.

### User hypothesis (2026-04-26 PM, walter)

> Perhaps the interpreter is the first thing that actually needs
> more than 4k of stack? I suspect our previous hack to allocate
> stacks 4k at a time didn't completely do that — the kernel may
> still think that when a stack fault happens it only has to add
> another 1k subpage rather than a whole new 4k page.

This points at a subtle subpage-counter divergence: our `mask=0xF`
patch makes `FindOrAllocPage` claim all 4 subpages of one 4 KB page
per fault, but the *kernel's bookkeeping* (counters in TStackInfo,
TStackPage's per-subpage refcounts at +0x10..+0x14, the
`SetRestrictedPage` write at `1f7a98`) may still treat it as a
1 KB grant per fault. After 4 faults the kernel may think "4 KB
grown" while actually 16 KB of subpage bits are claimed; over many
faults the kernel's internal address tracker (`r4->[64]->[68]` in
`ResolveFault`'s bounds check at `1f79a8..1f79c0`) drifts past the
end of the TStackInfo and ResolveFault returns error `-10204`
(`0xffffd824`) which propagates up to UnhandledException → Reboot.

**TInterpreter is plausibly the first task needing >4 KB of stack.**
The kernel boot sequence has 24 task structs (cdsv, pckm, drvr, etc.)
all using small stacks. TInterpreter's `__ct__15TRefStructStackFv` x 2
each call `NewStack(0x10000)` (= 64 KB) twice = 256 KB total of
lazy-grow stack region. Earlier tasks never exceeded 4 KB and so
never hit the subpage-counter drift. TInterpreter is the canary.

### Kernel-state dump at canary fire (2026-04-26 PM, walter follow-up)

Added a one-shot dump in `handle_reboot` (src/trap.rs). Captures L1 walk
+ the monitor list referenced by DataAbortHandler at 0x393318. Result:

```
L1 walk (sections 0xC0..0xD7):
  L1[0xc0] = 0x00001401  (coarse, kernel low-VA scratch)
  L1[0xc1] = 0x04006841  (coarse)
  L1[0xc2] = 0x0401cc61  (coarse)
  L1[0xc3] = 0x04025861  (coarse, kernel-side stack page table)
  L1[0xc4] = 0x00000070  (fault — different marker, domain=3)
  L1[0xc5] = 0x00000070  (fault — domain=3)
  L1[0xc6] = 0x0401c881  (coarse)
  L1[0xc7] = 0x00000090  (fault — domain=4 lazy)
  L1[0xc8] = 0x00000090  (fault — domain=4 lazy)
  L1[0xc9] = 0x0401c481  (coarse, in-use stack region)
  L1[0xca] = 0x0401c081  (coarse, in-use stack region)
  L1[0xcb] = 0x00000090  (fault — domain=4 lazy)
  L1[0xcc] = 0x04025481  (coarse, in-use stack region — stack1!)
  L1[0xcd..0xd5] = 0x00000090  (all fault — domain=4 lazy)
  L1[0xd6] = 0x04025ca1  (coarse)
  L1[0xd7] = 0x000000b0  (fault — domain=5 lazy variant)

gKernelGlobals @VA=0xc100ff8 PA=0x4007ff8 = 0x0c1215f8  (= newt task!)
task @VA=0xc1215f8 PA=0x40535f8
  ->[0x74] = 0x0c118ae0  (monitor)
  ->[0x78] = 0x00000000  (none)
  ->[0x7c] = 0x00000000  (no list)
monitor[+0x74] @VA=0xc118ae0 PA=0x4023ae0
  ->[0x10] = 0x00055555  (fault-handler bitmask)
```

`gCurrentTask = newt` confirms TInterpreter ctor runs on the `newt`
task (which makes sense — `newt` is the NewtonScript runner).

The bitmask `0x00055555` analyzed against the kernel's
`add pc, pc, r0, lsl #2` dispatch at `0x393384`:
- Shift index for DFSC=5/7: `(DFSR_lo8 >> 3) & 0x1e = 8`
- `(0x55555 >> 8) & 3 = 1` → dispatch arm 1 → `0x39339c`
  (= **success path**, calls FaultMonitorEntry).

So **the kernel DOES enter the FaultMonitorEntry chain for our DFSC=5
fault at FAR=0x0cd07400** — the bitmask routes both DFSC=5 and DFSC=7
to the same success arm (since `(DFSR_lo8 >> 3) & 0x1e` collapses
both to 8). The wedge is therefore downstream of FaultMonitorEntry.
The two most likely failure points are:

1. **ResolveFault's bounds check at `0x1f79b8`/`0x1f79cc`.** It reads
   `TStackManager->[64]->[68]` (some "tracker" the kernel maintains)
   and compares against `TStackInfo->[24]` and `TStackInfo->[28]`.
   If the tracker is outside the bounds of the TStackInfo we picked
   (e.g., because the kernel found stack1's TStackInfo but the FAR
   is in stack2's range), ResolveFault errors `-10204`.
2. **`Remember` → `SWI #12` returning a different error than -10003.**
   Without -10003 the kernel skips the AllocatePageTable retry loop
   and propagates whatever error came back. If our hypervisor's SWI
   path mishandles GenericSWI #12 (or the kernel has internal state
   that causes the SWI to return an unexpected error), we'd see this.

### Next experiments motivated by this

1. **Audit ResolveFault's r4->[64]->[68] tracker.** Identify what
   that field tracks, dump it before/after each fault. If it advances
   by 1 KB per fault but should advance by 4 KB, the kernel logic is
   1-KB-stride and we need to either patch the stride to 4 KB or use
   a different fix-up.
2. **Patch `SetSubPageInfo` to bump the page-allocated counter by 4
   instead of 1.** That's the "kernel thinks 4 KB was added" fix —
   matches the actual physical allocation under mask=0xF.
3. **Try `mask=0x1` (only the faulting subpage, the kernel default)
   plus a separate fix for the BootOS-canary alias.** E.g., a
   different patch that doesn't share pages between tasks but uses
   the kernel's natural subpage logic.

### Reproduction artifacts

`/tmp/phaseB-2026-04-26-reboot/`:
- `qemu_trace.log` — 240 s trace+quiet boot (633 k traces; doesn't reach
  canary because tracer overhead pushes the kernel through the flash log
  scan loop too slowly — only 1409 unique functions hit, ending with
  `TCardEventHandler::IdleProc` / `VppIdleOff` / `InternalVppIdleOff`).
- `qemu_trace.firsts` — `awk '/^trace / && !seen[$4]++'` over above.
  Useful for "what new code did we reach" diff against prior runs.
- `qemu_notrace.log` — 120 s quiet-only boot, hits the canary at trace
  ~270k with the original short canary report.
- `qemu_notrace_v2.log` — same boot with the enhanced `log_dabt_forward`
  capture; this is the source of the LR_abt/USR_lr breakdown above.

---

## Earlier — TCardMessage-alias wedge resolved by per-page stack allocation (QEMU, 2026-04-26 evening)

**Status:** the BootOS-canary wedge that's been blocking Phase B for
weeks is resolved. Boot now progresses 2.4× past the previous stall —
from trace ~170 k (BootOS canary entry #2 with R0=0x0cc80c80) to trace
403 k+ (deep into user-mode task setup: TUTaskWorld, TUPort,
TUSharedMem, TUSemaphoreGroup) and later to a **separate, downstream**
`Reboot` canary at no-trace runtime.

### Root cause

The 717006 kernel uses **ARMv4 subpage-AP** to pack up to four 1 KiB
stacks onto a single 4 KiB physical page. Each task's stack lives in
one of the page's four 256 B-aligned 1-KiB subpages; the other three
subpages are guard regions (AP=00) so SP-relative writes that drift
into them fault and the kernel can grow the stack with a fresh page.

ARMv7 (our Cortex-A53 host) **has no subpage-AP support** — bits[11:4]
of the L2 small-page descriptor are reinterpreted as
`(nG, S, AP[2], TEX[2:0], AP[1:0])` instead of four 2-bit subpage AP
fields. The 717006 kernel's encoding ends up as
`AP[2:0] in {000, 100, 111}` (= no-access / Reserved / deprecated)
which would fault every access. To work around this,
`fix_stage1_xn_bits` flattens every L2 entry to AP=011 (full RW) so
accesses don't fault — necessary for boot to proceed at all.

The side effect is that **stack A's overrun corrupts stack B** when
the two stacks share a physical page via different VAs to subpages
of the same PA. In our boot, `name`-task's stack-frame push (in
`MoveFreeBlock`) walks past its 1-KiB subpage into another task's
subpage on the same PA, and the corruption propagates downstream
until the BootOS canary fires.

### Fix: per-page stack allocation

Per @walter's insight: subpage AP is **only used for stacks**, and a
4-KiB-page-per-stack regime never relies on subpage protection. The
kernel's stack-page allocator (`TStackManager::ResolveFault`) decides
per-fault which subpages of a candidate page to claim. By forcing
`ResolveFault` to claim **all four** subpages on every fault-driven
allocation (mask = `0xF` instead of `1 << subpage_idx`), each task
ends up with a fresh 4-KiB page that nobody else can grab subpages
on. ARMv7's loss of subpage-AP semantics no longer matters: a stack
overrun stays on the task's own slack space rather than corrupting
another task's data.

Implemented via three ROM patches in `rom_patches.rs::PATCHES_717006`,
each replacing the mask-setup instruction immediately before a
`bl FindOrAllocPage_ReturnUnLockedOnNoPage` call in `ResolveFault`
(PC 0x001f7978..0x001f7c7c) with `mov r3, #0xF` (= `0xE3A0_300F`):

| Site | Original | Context |
|------|----------|---------|
| `0x001f_7a10` | `lsl r3, r0, r8` | normal-fault single-subpage mask |
| `0x001f_7bd4` | `ldr r3, [sp, #60]` | collision-grow mask reload |
| `0x001f_7c24` | `orr r3, r1, r0` | collision-grow combined mask |

### Boot-trajectory evidence

| Variant | Wedge | Trace count |
|---------|-------|-------------|
| Pre-patch (baseline) | BootOS canary entry #2 (R0=0x0cc80c80, ELR=0x00ffff58) | ~170 k |
| With ScratchVA only | Same as above (ScratchVA exonerated as a separate hypothesis) | ~170 k |
| **With mask=0xF patch** | **`Reboot` canary** (kernel-driven self-reboot, separate failure) | **403 k+** |

The new wedge fires `Reboot` (not BootOS canary) with R0=0xffffd8a5,
LR=0x000d9888 inside user-mode flash-driver work. Different chain;
not part of the same alias problem.

### Diagnostic / follow-up

`tracer.rs::dump_movefreeblock_entry` — the one-shot probe at the
specific `name`-task `MoveFreeBlock(0x0c2041e0, 0x20)` call — stays
in tree as the regression diagnostic. With the patch in place it
fires identically (SP=0x0c320824, all stage-1 walks unchanged) but
the subsequent stack push no longer corrupts a neighbor.

Reproduction artifacts:
- `/tmp/phaseB-2026-04-26-scratchva/qemu_mask_f.log` — trace+quiet
  boot, 403 k+ traces, no canary.
- `/tmp/phaseB-2026-04-26-scratchva/qemu_mask_f_notrace.log` —
  quiet boot, hits `Reboot` canary at the new failure point.

### Next stall

Investigate the `Reboot` canary trigger. R0=0xffffd8a5 is the
exception code; trace context just before the canary should show the
`Throw`/`UnhandledException` chain. R12=0x0cc82544 is in the
TCardServer-allocated region — possibly a related card-driver issue
that the original wedge was masking, or possibly an entirely separate
kernel state divergence.

---

## Earlier — Stack-variant alias hypothesis EXONERATED via ScratchVA swap (QEMU, 2026-04-26 PM)

**Status:** the focused experiment to swap `StubVariant::Stack` for a
non-stack-touching `StubVariant::ScratchVA` (per
`docs/plans/shadow-stub-scratch-va.md`) is complete. The swap landed
clean: 1,695 ScratchVA-fallback sites install successfully, 35/35
guest tests pass, and the boot reaches the **same wedge point as the
baseline**:

- Boot trajectory: BootOS canary entry #2 at trace ~170,369 (baseline:
  ~169,986; the small delta tracks the per-stub instruction count
  increase for ScratchVA-eligible sites).
- Canary signature identical to baseline: `R0=0x0cc80c80`,
  `R1=0xffffffff`, `R14_UND=0x0001868c`, ELR_EL2=`0x00ffff58`.
- Preceding tracer entries are byte-identical to the
  Stack-variant baseline (`TCardMessage::Clear` at PC `0x0004ed84` →
  `ZeroBytes`/`FillLongs` writing to VA 0x0cc80c00, lr=0x0004ed68).

**Conclusion.** The Stack-variant inline stub's PUSH/POP-onto-mode-banked-SP
side effect was NOT the silent perturbation masking the kernel-mode
stack-fault chain Einstein takes. The `name`-task wedge persists
unchanged with stack-touching removed. Suspect list narrows to:

1. **Heap-allocator divergence past Einstein's recording cap** (trace
   1,063). Heap state past that point is unmodelled by Einstein but
   our recording continues; the divergence may live there.
2. **`gPhysAllocator` ordering / TPhys descriptor selection.** The PA
   recycle into TCardMessage write region (PA `0x0402b000`) is
   downstream of physical-page allocator state we haven't yet diffed
   against Einstein.
3. **A QEMU-vs-FVP-vs-Einstein behavioural difference** outside the
   shadow-stub plumbing (e.g., timer cadence, IRQ delivery ordering,
   device-state side effects).

### ScratchVA implementation summary

- **Stub layout** (`shadow_stub.rs`): bumped from 12 to 16 words. New
  variant `StubVariant::ScratchVA { sfl, sad, scratch_slot_idx }`
  saves caller `scratch_addr` to TPIDRURW (slot 0 MCR / slot 13 MRC),
  loads per-stub scratch slot VA via `LDR scratch_addr, [PC, #+48]`
  from a literal at slot 15, and STR/LDRs caller `scratch_ea` /
  `scratch_fl` through that VA at slots 2/3 and 11/12. Slots 4/9 do
  the standard MRS/MSR NZCV save/restore.
- **Operand-exclusion picker** extended from 2 regs to 3
  (`pick_operand_excluded_triple`). Always succeeds: 6 candidates
  (`R12, R0..R3, R14`) − up to 3 operand registers ≥ 3 spare.
- **Address layout**: `SCRATCH_POOL_VA = SCRATCH_POOL_IPA = 0x0600_0000`
  (identity-mapped). Stage-2 refines L2[0x30] to a 64 KiB RW carve-out
  at IPA `0x0600_0000..0x0601_0000` backed by host
  `shadow_stub::SCRATCH_POOL`. Kernel L1[0x60] is observed-free in the
  717006 boot at FATAL-time (gap L1[0x52..0xBF], 110 unallocated slots);
  `install_scratch_pool_l1_section` writes a section descriptor
  `0x0600_0C1E` (RW, XN=1, identity PA, normal cacheable) to L1[0x60]
  on every M=0→M=1 transition.
- **VA pick rationale.** First attempt (L1[0x18], the `kwmklzru`
  precedent) failed: the 717006 kernel runtime-allocates a coarse
  table at L1[0x18] = `0x00018001` on the third M=0→M=1 transition.
  Same outcome at L1[0x1A] (`0x00016001`). The FATAL halt (re-installer
  detects the kernel's coarse) confirmed both slots are unsafe. Full
  L1 census (`/tmp/phaseB-2026-04-26-scratchva/qemu_l1_dump.log`)
  showed kernel populates L1[0x000..0x2FF] across boot, with
  observed-free gaps at L1[0x52..0xBF] and L1[0xC2..0xEF].
- **TPIDRURW IRQ-race risk** (mitigation 1 from the plan): documented
  and tolerated. Same risk exists in the existing `DeadReg` variant's
  CPS sysreg use; hasn't surfaced in 35-test guest suite or boot.
- **Guest test** `subtest_24_scratch_va_preserves_caller` exercises a
  ScratchVA-fallback site by reading R0..R3, R12, R14 after the
  access (forcing liveness picker to mark all candidates live).
  Verifies caller GPRs and NZCV survive the stub round-trip.

### Reproduction artifacts

`/tmp/phaseB-2026-04-26-scratchva/`:

- `qemu_notrace.log` — quiet-only boot (90 s wall). Wedges at the
  baseline canary point. Install stats: `inline pool 26614/32764,
  scratch slots 1695/8192`.
- `qemu_trace.log` — `--features trace,quiet` boot (180 s wall).
  Final tracer entries identical to baseline through the wedge.
- `qemu_l1_dump.log` — diagnostic dump from the L1[0x18] / L1[0x1A]
  failed install attempts; shows kernel L1 census.

### Next steps

The remaining suspects (heap-allocator divergence past Einstein's
recording cap; TPhys allocator ordering) need independent investigation.
The ScratchVA variant stays in tree as the live fallback (Stack
variant is retained behind regression-test-only wiring); future
experiments touching the byte-access fallback no longer have to
worry about stack-page-aliasing side effects.

### Follow-up — physical-allocator + stack-extension trace audit

Pinned the divergence point by tracing every guest physical-allocator
and stack-extension call. Watchlist (~80 symbols) lives at
`/tmp/key_events.txt`; filtered events
(`/tmp/qemu_key_events.log` — 5 216 hits in our 170 K-event boot)
cover `TPageTracker::{Take,Put}`, `TPhys::*`, `Prim*Mapping`,
`AddPgP*` / `AddSec*`, `TStackManager::*`, `TStackPage::*`,
`TUDomainManager::*Map*`, and `TPageManager::*`.

**Take history is LIFO and 1:1 stride 0x1C** — descriptor base
decreases by one TLittlePhys-record-size each Take. We Take 28
TLittlePhys descriptors over the 170 K trace; the earliest 26 map
into VAs that match `einstein.va_pa` byte-for-byte through line 231:

| Take | Trace # | Descriptor base | Mapped VA |
|------|---------|-----------------|-----------|
| 1 | 16 715 | `0x0c10fb58` | (kernel internal — no PrimRememberMapping; lr inside `0x001c575c`) |
| ... | ... | ... | ... |
| 21 | 106 898 | `0x0c10f928` (PA `0x0402b000`) | `0x0c204000` (C heap) |
| ... | ... | ... | ... |
| 26 | 146 300 | `0x0c10f8b0` | `0x0cc87000` |
| **27** | **147 706** | **`0x0c10f880`** (PA `0x04031000`) | **`0x0ca6b000`** ← **first divergence** |
| 28 | 165 396 | `0x0c10f864` | `0x0c204000` (alias #2) |

`einstein.va_pa` line 232 records `REMEM 0x0c318000 0x0c10f928` —
i.e. Einstein's stack-fault path **re-uses an already-Taken
descriptor** (the PA-`0x0402b000` one, allocated earlier as our
Take 21 / Einstein's equivalent) and **adds an aliasing kernel-VA
mapping** for it. Our run instead allocates a fresh descriptor
(Take 27, descriptor `0x0c10f880`) and maps it to a user-heap VA
`0x0ca6b000`.

**Key invariant: every PrimRememberMapping through line 231 is
identical between Einstein and us.** That's 231 events of byte-for-
byte agreement on TPhys-→-VA bookkeeping. The ONLY divergence is
the missing fault: between our trace 146 517 (line 231, last match)
and trace 147 765 (line 232, first divergence) — a window of 1 248
trace events — Einstein issues 6 extra PrimRememberMappings (the
`name`-task fault chain `Fault → ResolveFault → AllocNewPage /
PageMatchFound → RememberMappings`); our run issues zero.

**Stack-event census (170 K trace):**

| Function | Count |
|----------|-------|
| `TStackManager::Fault` | 3 (traces 57 696, 169 805, 170 061) |
| `TStackManager::ResolveFault` | 94 |
| `TStackManager::FindOrAllocPage` | 32 |
| `TStackManager::AllocNewPage` | 16 |
| `TStackPage::Init` | 16 |
| `TStackManager::SetRestrictedPage` | 284 |
| `TStackManager::RememberMappings` | 95 |
| `TStackManager::ForgetMappings` | 2 |

Einstein's run hits `TStackManager::Fault` at trace ~147 500 (the
`name`-task `MoveFreeBlock` fault); our run never reaches a 4th
Fault in the entire pre-wedge boot. The two fault-cluster traces
near the end (169 805 / 170 061) are the recursive DABT chain that
the canary catches; not the missed `name`-task fault.

**Conclusion of the audit:** the divergence is purely
control-flow — same byte-identical pre-fault state, same args to
`MoveFreeBlock`, but our SP-relative access lands on a mapped page
where Einstein's lands on an unmapped one. The state difference
isn't in `va_pa` (= TPhys/PrimRememberMapping bookkeeping) and
isn't in any of the stack-manager calls preceding the divergent
trace. Candidates for the hidden state:

1. **TStackPage subpage-restriction state** — `SetRestrictedPage`
   is called 284 times; we don't currently log r0/r1/r3 args
   ("which subpage of which TStackPage"). A subpage-state diff vs
   Einstein could expose whether our restricted-page mask differs.
2. **Kernel-internal SP value of `name` task at the fault moment**
   — needs a non-trace probe. A guest-BP at `MoveFreeBlock`'s entry
   inside `name` task could capture the actual SP and compare.
3. **TUDomainManager FaultMonProc path** — the call site of all 3
   of our `TStackManager::Fault` is `lr=0x00259230` inside
   `TUDomainManager::FaultMonProc`. A possible upstream is the
   page-monitor delivery order; if a faulting message gets routed
   to a different domain's FaultMonProc on our run, the fault is
   "swallowed" silently.

Reproduction artifacts: `/tmp/key_events.txt` (watchlist),
`/tmp/qemu_key_events.log` (filtered trace), `/tmp/take_history.py`
(LIFO Take→VA mapper), `/tmp/qemu_scratchva.va_pa` (canonical
va_pa diff against `einstein.va_pa`).

### Follow-up — root cause: ARMv4 subpage-AP flattened by `fix_stage1_xn_bits`

A one-shot probe at the entry of the specific `name`-task call
`MoveFreeBlock(0x0c2041e0, 0x20)` (trace 146 848 in our run) dumped
SP=0x0c320824 and the stage-1 mappings around SP. The decisive
observation:

- `L1[0xc3]` = `0x04023861` (coarse, domain 3, L2 at PA `0x04023800`)
- `L2[0x20]` (VA `0x0c320000`) = `0x0401b03e` → PA `0x0401b000`,
  AP[2:0]=011 (full RW)
- `L2[0x18]` (VA `0x0c318000`) = `0x0401b03e` → same PA, also AP=011
  (alias)
- `DACR` = `0x00000155`, so D3 = `01` (client; AP-bits enforced)

Reading the disassembly of `AddPgPAndPerm` (`0x0015a8f0`) confirmed
that the kernel actually writes ARMv4-style descriptors with
**subpage-AP** bits[11:4]:

- VA `0x0c320000` (with `perm_high=0x30`, `perm_low=0x01`): kernel
  writes `0x0401b30e` — ARMv4 subpages
  `{ AP0=00, AP1=00, AP2=11, AP3=00 }` (only subpage 2 RW; rest are
  no-access guards).
- VA `0x0c318000` (with `perm_high=0x0c`): kernel writes
  `0x0401b0ce` — subpages `{ 00, 11, 00, 00 }` (only subpage 1 RW).

These ARMv4 entries are the kernel's lazy-allocation guard pattern:
the LIVE 1 KiB subpage is RW; surrounding subpages are no-access
guards. On real ARMv4 hardware (Einstein), an SP-relative store
that drifts into a guard subpage faults, the `DataAbortHandler`
runs `TStackManager::Fault`, and a fresh stack page is allocated.

ARMv7 (our hardware) has **no subpage-AP support** — bits[11:4] are
reinterpreted as `(nG, S, AP[2], TEX[2:0], AP[1:0])`. Newton's
mixed-subpage encoding always reinterprets to AP[2:0] in
`{000, 100, 111}` (= no-access / Reserved / deprecated), which would
fault every access. To work around this, `fix_stage1_xn_bits`
rewrites every small-page L2 entry to `0x...03E` —
**AP[2:0]=011 (full RW)** — which makes accesses succeed
unconditionally. This is what ships and works for kernel-mode
boot, but **silently destroys the kernel's stack-fault chain**:

1. The `name`-task `MoveFreeBlock` prologue pushes registers, SP
   drifts from 0x0c320824 (subpage 2, live) into 0x0c3207f8 (subpage
   1, guard).
2. On Einstein: subpage 1 has AP=00 → fault → kernel allocates
   fresh PA `0x0402b000` for VA `0x0c318000` → no PA aliasing.
3. On us: `fix_stage1_xn_bits` flattened the page to AP=011
   (everyone-RW) → no fault → no fresh allocation → PA `0x0402b000`
   stays in `gPhysAllocator`'s free pool → later TCardServer
   allocation aliases it into the user-heap range → BootOS-canary
   wedge.

### Failed fix attempts (2026-04-26)

Tried two replacement strategies for the ARMv4-→-ARMv7 conversion in
`fix_stage1_xn_bits`'s small-page handler. Both reverted.

1. **Mixed subpages → AP[2:0]=000 (fault).** Boot wedges very early
   (~60 traces in) inside `MakePrimaryMMUTable` with PABT chains at
   ELR=0x186xx. Some early ARMv4 entries that are deliberately
   AP=00-uniform get marked fault, which breaks BootOS's MMU
   bring-up.
2. **Mixed subpages → AP[2:0]=010 (PL1 RW, PL0 RO).** Same early
   wedge — DABT forwards to kernel at PCs 0x186b4..0x18710 with
   ESR=0x9381_0047, suggesting a USR-mode write to a kernel-RO page
   the kernel still expects to be writable.

The conversion needs to distinguish at least three classes of
ARMv4 subpage-AP entries:

- **Boot-time bring-up entries** (BootOS / MMU primary table): the
  current flatten-to-AP=011 behaviour is correct.
- **Stack-guard entries**: should fault on USR write to trigger the
  lazy-allocation chain.
- **Code/RO-data**: USR fetches and reads must succeed.

The hint distinguishing them is probably either:
(a) the calling site (kernel boot vs runtime AddPgPAndPerm); or
(b) the stage-1 walk context (which task's TTBR0 is live).
Neither is currently visible inside `fix_stage1_xn_bits`'s passive
walk.

### Suggested next directions

1. **Trap kernel L2 writes via stage-2 RO-protect**, then convert
   per-page on the trap (knowing the calling context). Heavy
   plumbing but precise.
2. **Selective conversion based on perm bits seen at AddPgPAndPerm
   call time**: hook the SVC-mode call entry, capture
   `(VA, PA, perm_high, perm_low)`, and rewrite the resulting L2
   entry to the ARMv7 equivalent that preserves the kernel's
   intent. Requires post-AddPgPAndPerm L2-entry rewriting.
3. **Selective conversion based on subpage pattern**: keep the
   uniform-subpage cases mapped 1-to-1; for mixed-subpage entries,
   determine whether the access pattern needs guard semantics by
   looking at the VA range (kernel-stack VAs `0x0c3xx000` →
   convert to fault-on-USR-write; everything else → flatten). The
   717006 ROM's stack VA layout is documented enough that this
   could be a targeted patch.

The reproduction probe (`tracer.rs::dump_movefreeblock_entry`)
stays in tree as the diagnostic for any future fix experiment.

---

## Earlier — root cause narrowed: missing stack-fault on `name` task drives PA-recycle into user heap (QEMU, 2026-04-26 PM)

**Status:** the `AddPgPAndPerm` audit pinned the precise divergence point
and the upstream cause. The wedge is NOT tracer-induced — a fresh
no-trace cold boot reaches the same recursive DABT and the same BootOS
canary at the same TCardServer allocation chain.

### The divergence point (trace 147612 Einstein vs 147932 ours)

Both runs make byte-identical AddPgPAndPerm / PrimRememberMapping calls
through trace 147186 (Einstein) / 146678 (ours) — five `0xcc8x000` →
`0x0402[ef0]000` pairs that match across both implementations. Then:

| | Einstein | Ours |
|---|---|---|
| trace # | 147612 | 147932 |
| `Remember(env, va, perm, phys_id)` | `(0x1355, 0x0c318000, 0, 0x1b3b)` | `(0x13a5, 0x0ca6b000, 0, 0x1edb)` |
| Resulting `AddPgPAndPerm(va, _, pa, _)` | `(0x0c318000, _, 0x0402b000, _)` | `(0x0ca6b000, _, 0x04031000, _)` |
| Caller env | new env 0x1355 (faulting task's domain) | same env 0x13a5 (default) |

Einstein's call is driven by **TStackManager::Fault** — `name` task
(0x0c119c74) faulted while running `MoveFreeBlock` at trace 147523-147524
(`DataAbortHandler` mode=ABT, faulting access in domain 3). The fault
handler unschedules `name`, schedules STKF (0x0c112e00), and STKF
allocates a new TStackPage at VA `0x0c318000` (a kernel-side address)
backed by PA `0x0402b000`.

Ours never faults: at the equivalent trace 147028 our `name` task runs
the same `MoveFreeBlock(r0=0x0c2041e0, r1=0x00000020)` with byte-identical
args and returns normally. Without a stack fault, STKF never allocates
the kernel-side page, so PA `0x0402b000` stays in `gPhysAllocator`'s
free pool.

### The downstream alias chain (PA 0x0402b000)

Tracking every `AddPgPAndPerm` / `RemovePgPAndPerm` / `PrimRememberMapping`
hit on PA 0x0402b000 (TPhys descriptor 0x0c10f928):

```
trace ours    Einstein   op       VA           PA
107143/107638 ADD     0x0c204000   0x0402b000  (initial: in C heap)
148115        ADD     0x0cc82000   0x0402b000  (alias #1)
148144        RM      0x0cc82000               (bookkeeping only)
148201        ADD     0x0cc82000   0x0402b000  (re-add)
165326        ADD     0x0c204000   0x0402b000  (re-add)
165641        ADD     0x0c204000   0x04032000  (REMAPPED to different PA)
165752        ADD     0x0cc82000   0x0402b000  (re-add)
169509        ADD     0x0cc7f000   0x0402b000  (alias #2 — never RM'd)
169740        ADD     0x0cc80000   0x0402b000  (alias #3 — TCardMessage region!)
                ↑ wedge: TCardServer fills VA 0x0cc80xxx with 'newt'/'cdsv'
                  literals. Writes corrupt the same PA used by
                  pre-existing alias VAs → recursive DABT → BootOS canary.

# Einstein side (no aliasing in the user heap range):
                147631 ADD     0x0c318000   0x0402b000  (kernel page, no writes)
                147660 RM      0x0c318000
                147717 ADD     0x0c318000   0x0402b000  (re-add)
```

The kernel deliberately allows multi-VA-to-one-PA aliasing
(this is by design for stack-page sharing). The bug is which VA the
kernel chooses: Einstein picks VA `0x0c318000` (kernel internal,
nothing writes there); ours picks user heap VAs `0x0cc7f/0cc80/0cc82`
(target of `TCardServer::TCardServer`'s 62-element TCardAsyncMsg
array constructor at ROM 0x34502c).

### The wedge mechanism

At trace 169981 the array constructor reaches r4=0x0cc80bc8.
TCardMessage::TCardMessage explicitly stores `'newt'` (0x6e657774) at
`*(self+0)` and `'cdsv'` (0x63647376) at `*(self+4)`, then `Clear`
zero-fills `+8..+0xb8`. Those writes hit PA `0x0402b000` + offsets
`0xbc8..0xc80`, which is also live as VA 0x0c204xxx (the C heap that's
been there since trace 107143).

Concretely: pckm or another task with a stack frame on the aliased
range reads what should be a saved LR slot, finds zero (one of the
zero-fills landed there), and a subsequent `mov pc, lr` jumps to PC=0.
PC=0 hits the reset vector at VA 0x0 = `b 0x18688 BootOS`. From USR
mode, HVC at 0x18688 (the canary patch) is undefined, fires UND, the
trampoline routes to EL2, and the canary detects the second BootOS
entry as a software reset.

### No-trace confirmation (it's not tracer overhead)

A second cold boot with `--features quiet` (no `trace`) reaches the
same wedge: `dabt: forwarding to kernel DataAbortHandler — DFSC=0x7
FAR=0x0cc7fcc8 mode=0x17` then a recursive abort with FAR=0x0cc80001,
then the same BootOS canary at entry #2 with R0=0x0cc80c80,
R14_UND=0x0001868c. So the alias is a real bug, not a tracer-induced
timing artefact. The earlier "wall-clock-skew" theory (closed in the
previous section by switching to instruction-anchored ticks) was a
related but distinct issue.

### Why our `name` task doesn't fault (open question)

This is the load-bearing mystery. Both runs:

1. Reach `TNameServer::RegisterForSystemEvent(0x70776f66, 0x1e12)` at
   identical entry args (ours trace 146910, Einstein 147405).
2. Walk identical paths through `SysEventTester` ctor → `CList::Search`
   → `CDynamicArray` ops → `operator new(0x14)` → `malloc` → `NewPtr`
   → `IsSafeHeap` → `NewBlock` → `MoveFreeBlock(0x0c2041e0, 0x20)`.
3. Args at every traced step are byte-identical.

Einstein's `MoveFreeBlock` faults; ours returns normally. The
deterministic kernel logic with identical args **must** depend on
heap state that has diverged. The alloc-arg sequence file
(`SafeHeapPage::Alloc(r0, r1, r2)`) was byte-identical for the first
1063 calls per the prior section, but Einstein's recorded alloc trace
ends there (200 k trace cap). Beyond 1063, allocations may have
diverged silently before reaching this code.

Plausible upstream divergences:

1. **Allocation return values** — args matching doesn't mean returned
   pointers match. If our heap's free chain is in a slightly different
   order, `NewPtr` returns a different chunk address, downstream code
   touches different slots.
2. **Stack-page state** — the *fault* in MoveFreeBlock is about which
   page the SP touches. If `name` task's stack has a page boundary at
   slightly different offsets between runs, our SP may stay inside an
   already-mapped page while Einstein's crosses into an unmapped one.
3. **Kernel object IDs** — our env 0x13a5 vs Einstein's 0x1355 implies
   we allocated environments in a different order earlier, leading to
   different IDs being live for the same logical request.

### Possible next steps

1. **Audit `__nw__FUi` returns past trace 1063.** The current alloc
   diff stops at Einstein's recording cap. Re-run NewtonProbe with a
   larger trace cap to extend Einstein's alloc reference past 1063
   calls, then diff our 1064..1488 to find the first divergent return
   address. Once that's found, walk back to the upstream perturbation.
2. **Hypervisor-side state cross-check.** Look at our `gPhysAllocator`
   layout (39 RAM-page TPhys descriptors). If the order or content of
   the descriptors differs from Einstein's, `TPhys::Get(0x1edb)` vs
   `TPhys::Get(0x1b3b)` could resolve to different physical pages —
   that's what `phys_id` divergence would mean.
3. **Force the alias to be benign.** Even if we can't make the kernel
   choose `0x0c318000`, we might detect the multi-VA-aliasing pattern
   in `AddPgPAndPerm` (stage-2 trap) and either (a) fault if the
   newly-mapped VA already has live content at the same PA via a
   different VA, or (b) reject the aliasing add. Risky — kernel-side
   stack sharing depends on aliasing.
4. **Cross-check on FVP.** FVP has a different trap-cost profile;
   if the same wedge happens identically, that confirms the kernel
   logic alone (no QEMU artefact). If FVP advances past, the
   divergence is QEMU-specific.
5. **Replace `StubVariant::Stack` with a non-stack-touching scratch
   variant.** See "shadow-stub Stack-variant experiment" below — the
   current Stack-variant PUSH/POPs onto the user task's mode-banked
   SP, which is a strong candidate for the source of the page-mapping
   divergence. A scratch-page variant (write/read from a fixed
   stage-2-mapped scratch VA instead of the user stack) would
   preserve the kernel's view of stack-page liveness.

### Shadow-stub Stack-variant experiment (2026-04-26 PM)

Hypothesis prompted by question: could `StubVariant::Stack` be the
silent perturbation that prevents our `name` task from triggering the
MoveFreeBlock fault Einstein triggers?

**Mechanism.** The Stack-variant inline stub (chosen when liveness
analysis finds < 2 dead candidate registers — see
`shadow_stub::emit_inline_stub`) emits:

```
slot 0:  PUSH scratch_ea     ; SP -= 4, write [SP] = scratch_ea
slot 1:  PUSH scratch_fl     ; SP -= 4, write [SP] = scratch_fl
slot 2:  MRS scratch_fl, cpsr
slots 3-7: address compute + access
slot 9:  POP scratch_fl      ; SP += 4
slot 10: POP scratch_ea      ; SP += 4
slot 11: B orig_pc + 4
```

While SP is restored at exit and the popped values are discarded,
each Stack-variant PUSH writes to memory below the current SP. If
that memory lands on a guest page the kernel hasn't yet lazily mapped,
the PUSH triggers a stage-1 translation fault that our DABT-vector
trampoline forwards to the kernel's `DataAbortHandler`. The kernel
maps the page and returns, the PUSH retries successfully, and the
next time MoveFreeBlock (or any other code) pushes onto that same SP
range, the page is already mapped — so the kernel-mode stack-fault
chain Einstein triggers (`TStackManager::Fault → STKF → kernel-side
TStackPage allocation at VA 0x0c318000 backed by PA 0x0402b000`)
never fires for us.

**Experiment.** Modified `emit_inline_stub` to return `Err` on the
liveness-fail branch, with `patch_one_site` falling back to UDF
emulation for those sites. Result on cold boot:

- Install stats: 1647 sites that previously emitted Stack-variant
  inline stubs now emit UDF (out of 27799 total byte-access sites).
  So the Stack-variant *was* 6 % of byte-access sites.
- Boot wedged **much earlier** — DIAG vector intercept at PC
  `0x00ffff64` (= the SBA pre-fault stub's `HVC #SBA_RETRY_TAG`),
  with `LR_svc=0` and `LR_und=0x00045474`, in BootOS's stack-
  initialisation region. The recursion: a Stack-variant fallback
  fired in early BootOS (before the kernel's `DataAbortHandler` was
  fully wired), the SBA-emulator's pre-fault probe LDRB took a DABT,
  and the unprepared kernel state caused a recursive abort chain.

**What this tells us.**

1. The Stack-variant is heavily used (1647 sites) and is depended on
   for early BootOS execution. Naive replacement with UDF breaks
   pre-`SetUpStacks` code paths — the comment in `emit_inline_stub`
   explicitly noting "first hit: 0x225d8 inside TADSPEndpoint::nSnd"
   is now stale; the actual install includes earlier sites.
2. The hypothesis is *plausible but not directly tested* by this
   experiment. We can't isolate post-`name`-task timing from pre-
   `SetUpStacks` correctness with a one-knob replacement.
3. A focused fix would replace `StubVariant::Stack` with a variant
   that writes scratch_ea / scratch_fl to a fixed hypervisor-allocated
   stage-2-mapped scratch VA (or to TPIDR_EL0-style scratch in the
   ROM aperture) instead of the mode-banked SP. That preserves the
   inline-stub fast path (no UDF round-trip) while removing the
   stack-touching side effect.

**Source state.** Experiment reverted; `src/shadow_stub.rs` is
unchanged from parent. Only INVESTIGATION.md updated this commit.

### Reproduction artifacts

`/tmp/phaseB-2026-04-26/`:

- `qemu_fresh.log` — fresh trace+quiet cold boot, 180 s wall, wedges at
  trace 169986 with BootOS canary entry #2.
- `qemu_fresh.firsts` — `awk '/^trace / && !seen[$4]++'` over above.
- `qemu_fresh.va_pa` — VA/PA tuples from AddPgPAndPerm /
  PrimRememberMapping calls.
- `qemu_fresh.alloc2` — `SSafeHeapPage::Alloc` arg sequence (1488
  entries; matches Einstein's 1063 byte-for-byte).
- `qemu_notrace.log` — fresh quiet-only cold boot, 90 s wall, same
  wedge without tracer.
- `einstein.va_pa` — extracted from `/tmp/phaseB-2026-04-25/einstein.head200k`.

`einstein.va_pa` ⊊ our `qemu_fresh.va_pa` for the matched prefix; the
single line `ADD/REMEM 0x0c318000 …` is in Einstein only and seven
unique-pair lines are in ours only (all in the user-VA alias range).

---

## Earlier — instruction-anchored ticks land; heap allocator now matches Einstein bit-for-bit (QEMU, 2026-04-26)

**Status:** the timing-induced divergence identified in the prior section
is fully closed. Newton-tick advancement is now decoupled from host
wall clock and tied to guest sync-trap progress. Heap allocator state
(`SSafeHeapPage::Alloc` arg sequences) is now byte-identical to
Einstein's first 1063 calls. Boot trajectory tracks Einstein within
~600 trace events through the residual `newt-DABT` wedge.

The remaining divergence is **stage-1 page-table state**: even with
identical heap allocations and identical `TCardMessage` VAs
(0x0cc7fd70 → 0x0cc800a0 → … in lockstep with Einstein), the kernel's
`TStackManager`/`TPageManager` decisions about which PA backs which VA
differ — our run aliases pckm's stack PA into the `0x0cc80xxx` range
where Einstein doesn't, and the alias-write still corrupts pckm's
saved frame and triggers the recursive newt DABT.

### What changed

`src/peripherals/vic.rs`, `src/stage2.rs`, `src/timer.rs`:

1. **`vic::SYNTH_TICKS`** — synthetic 32-bit Newton-tick counter,
   replaces the old wall-clock-anchored `vic::ticks()`. Advanced only
   by `tick_advance_sync_trap` (Δ_sync = 6 per guest sync trap) and
   `tick_advance_heartbeat` (Δ_heartbeat = 1024 per CNTHP heartbeat).
   `LAST_TICKS` ratchet is gone — fetch_add is monotonic by
   construction.
2. **`tick_page::update_from_sync_trap`** — sync-trap path. Bumps
   ticks by Δ_sync, polls VIC matches, republishes the tick / calendar
   pages.
3. **`tick_page::update_from_heartbeat`** — heartbeat path. Just
   republishes; does *not* bump ticks itself, so the no-progress
   detector below sees a clean signal.
4. **`vic::heartbeat_tick_update`** — runs from `timer::on_irq`.
   Bumps ticks by Δ_heartbeat (so non-trapping busy-waits make
   progress), and *additionally* fast-forwards SYNTH_TICKS past any
   pending VIC match deadline if no sync trap has fired since the
   prior heartbeat — i.e., the guest is parked in WFI or a long
   non-trapping loop. Detection is "SYNTH_TICKS unchanged from the
   value after last heartbeat update".
5. **`timer::rearm`** — drops the wall-anchored
   `newton_ticks_to_cntpct` translation. CNTHP arms a fixed 16 ms
   heartbeat only; VIC matches are polled at sync-trap granularity
   instead, which is plenty fine for the kernel's preemption / alarm
   cadence.

Calendar / RTC stays wall-clock-anchored (read of CNTPCT in
`vic::calendar_seconds`); the kernel's tick-domain math no longer
agrees with calendar seconds, but RTC semantics aren't load-bearing
for boot.

### Calibration (NEWTON_TICK_HZ irrelevant under synthetic clock)

| | Δ_sync | Δ_heartbeat | TaskKillSelf #2 trace | Δ vs Einstein |
|--|--|--|--|--|
| wall-anchored (prev) | n/a | n/a | 54 368 | -77 035 |
| Δ_sync 8, Δ_h 4096 | 8 | 4 096 | 113 043 | -18 360 |
| Δ_sync 6, Δ_h 4096 | 6 | 4 096 | 113 501 | -17 902 |
| **Δ_sync 6, Δ_h 1024** | **6** | **1 024** | **130 813** | **-590** |

Δ_sync = 6 derives from Einstein's BIO chip-detect loop (65 polls
across a 400-tick threshold ≈ 6.15 ticks/poll). Δ_heartbeat = 1024 is
the smallest value that keeps `BootOS::SafeShortTimerDelay` (an
11 058-tick non-trapping busy-wait) finishing in ≤ 11 heartbeats
≈ 176 ms wall.

### Boot trajectory side-by-side (post-fix QEMU vs Einstein 200 k)

| event | ours | Einstein | Δ |
|---|---|---|---|
| TaskKillSelf #1 (r2=0x0c111c98) | 38 472 | 39 065 | -593 |
| TUTask::Start #4 (r0=0x0c310338) | 53 502 | 54 095 | -593 |
| TUTask::Start #5 (r0=0x0c601320) | 130 416 | 131 006 | -590 |
| TaskKillSelf #2 (r2=0x0c310274) | 130 813 | 131 403 | -590 |
| TUTask::Start #6 (r0=0x0c601348) | 134 029 | 134 619 | -590 |
| TUTask::Start #7 (r0=0x0cc825e0) | 137 594 | 138 184 | -590 |
| TCardMessage at 0x0cc7fd70 | 170 066 | 170 433 | -367 |
| TCardMessage at 0x0cc800a0 | 170 321 | 170 663 | -342 |

Identical args, ~600-trace head start that grows to ~340 by trace
170 k. **`SSafeHeapPage::Alloc`'s first 1063 calls are byte-identical
in r0/r1/r2** between our run and Einstein's same window. Same heap
allocation order = same `__nw__FUi` returns = same VAs.

PreEmptiveTimerInterruptHandler now fires **0 times** in our 187 k
window (Einstein: 1× at trace 228 k). That's a 13× reduction from the
wall-anchored baseline.

### What's left — stage-1 PA-recycle decision still differs

`TCardMessage` allocations land at the same VAs in both runs, but the
PA backing those VAs differs:

- Einstein: VAs 0x0cc7fd70 / 0x0cc80xxx / 0x0cc82250 each get fresh
  PAs from `TPageManager::Get`. Pckm's stack at PA 0x0402a000 stays
  uniquely mapped at VA 0x0cc7a000.
- Ours: at least one of the `0x0cc80xxx` VAs gets PA 0x0402a000
  recycled into it (via `TStackManager::CopyPagesAfterStackCollided`
  or similar). The TCardMessage write at that VA paints
  `'newt'`/`'cdsv'` over pckm's saved frame at PA 0x0402a000+0x250 →
  pckm resumes → recursive DABT → BootOS canary.

Boot now wedges at trace 170 479 (ours) while writing the
0x0cc80bc8 TCardMessage. Same allocation chain Einstein survives at
trace 170 466.

### Possible next steps

The remaining bug is **PA-recycle order in `TStackManager`**.
Heap-allocator drift was the timing-induced piece; this is the
underlying TPhys / TStackPage selection logic.

1. **`AddPgPAndPerm` audit.** Hook a counter on every
   `AddPgPAndPerm(VA, PA)` call and dump the (VA, PA) pair to a log.
   Compare the first call where (VA, PA) differs between our run and
   Einstein's NewtonTrace. That's the upstream divergence in
   stage-1 page-table state.
2. **`TPageManager::Get` / `TPhys` selection.** The kernel's TPhys
   pool returns recycled PAs in some order. If our `gPhysAllocator`
   (TPhys table at 0xc1082a0) state differs from Einstein's at the
   moment of the divergent `AddPgPAndPerm`, that's the cause.
3. **Hypervisor-side state we touch differently.** The two
   implementations have different stage-2 page-table layouts; though
   the kernel doesn't see stage 2 directly, *which* PAs the kernel's
   TPhys allocator sees as "free" depends on what RAM regions the
   hypervisor has carved out. Cross-check
   `gPhysAllocator`'s 39 RAM-page TPhys descriptors against our
   `stage2::init` PA layout.

### Files changed

- `src/peripherals/vic.rs` — `SYNTH_TICKS`, `tick_advance_sync_trap`,
  `tick_advance_heartbeat`, `heartbeat_tick_update`,
  `next_pending_match` retained for the heartbeat fast-forward path.
- `src/stage2.rs` — `tick_page::update_from_sync_trap` /
  `update_from_heartbeat` split.
- `src/timer.rs` — `rearm` simplified to a 16 ms heartbeat;
  `newton_ticks_to_cntpct` removed; `on_irq` calls
  `vic::heartbeat_tick_update` then `update_from_heartbeat`.

All 35 guest tests still pass.

### Reproduction artifacts

`/tmp/phaseB-2026-04-25/`:

- `qemu_synth7.log` — current state (Δ_sync = 6, Δ_h = 1024).
- `qemu_synth7.alloc` — `SSafeHeapPage::Alloc` arg sequence;
  `diff einstein.alloc qemu_synth7.alloc` is empty for the first
  1063 calls.
- Older runs in same dir for regression comparison.

---

## Earlier — root cause: trace+UDF wall-clock skew, not heap state (QEMU, 2026-04-25 late evening)

**New diagnostic data from a fresh QEMU+Einstein NewtonTrace pair plus
trace-PC and heap-allocation diffs.** The residual newt-DABT divergence
is downstream of a fundamental wall-clock-vs-instruction-throughput
skew — not a heap-allocator quirk we can fix locally.

### Setup

- QEMU run: `cargo run --release --features trace,quiet`, 120 s wall,
  reaches trace 197 120 / 1224 unique functions before timeout.
- Einstein NewtonTrace: 180 s wall on
  `_Data_/Einstein.rex + roms/newton.rom`, reaches **89.6 M traces**.
- Both runs preserve all current source fixes (NEWTON_TICK_HZ natural,
  tick_page refresh on sync trap, flash RO, PCMCIA chip-detect, etc.).
- Both produce byte-identical first 16 779 PCs (line-aligned diff).

### First trace-PC divergence: trace 16 780 (BIO polling loop)

`diff /tmp/phaseB-2026-04-25/qemu.pcs /tmp/phaseB-2026-04-25/einstein.pcs`
shows the runs are PC-identical for the first 16 779 trace events,
then **Einstein executes 65 calls to `0x0008ea34
TDelayTimer::TimedOut(void)` that our run does not**. The caller is
`TBIOInterface::WaitBIOStatus` at `0x0026ba20`, polling for BIO chip
status with a 400-tick (≈108 µs wall) timeout.

Einstein iterates 65 times before timing out. Our run iterates **once**
(the first inline BIO read matches expected because our BIO model
returns 0 — but Einstein returns 0 too per `TMemory.cpp:952` "unknown
bank #3" fallback). The difference is:

| | wall budget | guest instructions executed in budget | poll iterations |
|--|---|---|---|
| Einstein JIT | 108 µs | ~3M (≈25M instr/s) | 65 |
| Our QEMU+trace+UDF | 108 µs | ~150 (≈1.5M instr/s) | 1–3 |

Each polling iteration in Einstein reads BIO state, calls TimedOut,
maybe stays in atomic blocks. Across 200 k traces:

| function | ours (197 k) | einstein (200 k) | Δ |
|---|---|---|---|
| `TDelayTimer::TimedOut` | 5 | 207 | **−202** |
| `TDelayTimer::GetHardwareTime` | 256 | 2 410 | −2154 |
| `SetAndClearBitsAtomic` | 598 | 948 | −350 |
| `StartScheduler` | 106 | 371 | −265 |
| `MakeConforming` | 640 | 0 | +640 (we reach this code earlier) |
| `LoadFromPhysAddress` | 2 363 | 629 | +1 734 |
| `Swap` | 6 602 | 4 674 | +1 928 |

**Reading: per-trace, our run skips most polling iterations and
"races" through to later boot phases.** Einstein spends thousands of
trace events idling in delay loops; we don't. Net trace counts in the
window are roughly equal because we offset by doing much more
page-table work (`MakeConforming`, `Load/StoreToPhysAddress`,
`FlushDCache`) that Einstein hasn't yet reached.

### First heap-state divergence: SafeHeapPage::Alloc call #534

Tracking `SSafeHeapPage::Alloc` arg `r0` (the page pointer) through
both runs:

| call # | ours r0 | einstein r0 | Δ trace |
|---|---|---|---|
| 530 | `0x0c119000` | `0x0c119000` | identical |
| 531 | `0x0c118000` | `0x0c118000` | identical |
| 532 | `0x0c11a000` | `0x0c11a000` | identical |
| 533 | `0x0c11a000` | `0x0c11a000` | identical |
| **534** | `0x0c11a000` | **`0x0c119000`** | **first divergence** |
| 535 | `0x0c11a000` | `0x0c118000` | |
| 538 | `0x0c11a000` | `0x0c119000` | |

After call 534, Einstein cycles through pages
(`0x11a, 0x11a, 0x119, 0x118` repeating) while our run keeps allocating
from `0x0c11a000`. The decision is in `SafeHeapAlloc` at `0x001c5f8c`:
`r0 = [r4+16]` (heap.first_page). Einstein's first_page rotates;
ours doesn't.

That happens because the **prior allocs filled `0x0c11a000`'s free
space differently between runs** — our slow-execution path took fewer
intermediate allocations from `0x118` / `0x119`, leaving `0x11a` with
more free room when call 534 runs. From that point on the two heaps
walk different sequences of free chunks, and `__nw__FUi(184)` for
`TCardMessage` ends up returning a different VA range
(`0x0cc82xxx` vs `0x0cca3xxx`), driving the alias bug we still see.

### IRQ-rate confirmation

Even with `NEWTON_TICK_HZ` already at natural rate, our preemptive
timer fires far more often than Einstein's per trace event:

| | first IRQ trace | total IRQs in 200k traces |
|---|---|---|
| Ours | 40 548 | 13 |
| Einstein | 228 108 | 1 (full 89.6 M run: 30) |

Same root cause: our slow per-wall execution means the kernel-armed
match register reaches its deadline (20 ms wall = `0x12000` ticks for
the preemption slice in `PreEmptiveTimerInterruptHandler` at
`0x001cc480`) after fewer guest instructions executed.

### Negative result — slowing NEWTON_TICK_HZ doesn't fix the alias

Tried `NEWTON_TICK_HZ = 245_760` (15× slower than natural) on the
hypothesis that polling-loop iteration counts would match Einstein's,
keeping the heap allocator's interleave aligned. Result: BootOS
canary fired at trace 164 k — boot crashed earlier than at natural
rate (which reaches 197 k+). The kernel's `TCardServer::TCardServer`
allocation chain still hit the alias and corrupted pckm's stack at
PA 0x0402a000 + 0x250, just at a slightly different VA range. The
alias is not driven by tick rate alone.

(Reverted; current source has `NEWTON_TICK_HZ = 3_686_400` again.)

### What this means

The "scheduling-order divergence" in the prior section is a *symptom*
of polling-loop iteration counts not matching Einstein. As long as
we're wall-clock-anchored and the tracer adds ~30 µs per call (from
`docs/QEMU_BUGS.md`), our QEMU+TCG+trace+UDF run executes ≈ 1/200th
the guest-instruction throughput of Einstein-JIT in the same wall
second, and any tick-deadline-bounded polling loop completes 100× too
quickly in trace-event terms.

### Possible directions

1. **Anchor tick advancement to instruction-count proxy** rather than
   wall-clock. E.g., increment `TICK_PAGE` by a fixed Δ per sync-trap
   plus a residual wall-clock catch-up at heartbeat. Would require
   careful tuning so calendar/RTC stays sane and so polling loops
   still terminate; the right Δ has to track Einstein's effective
   "instructions per Newton tick" ratio (≈ 6.78). The risk is that
   any Δ chosen is brittle vs. tracer-overhead changes.

2. **Implement BIO-bank canonical reads** matching Einstein's
   documented values from `TMemoryConsts.h`
   (`P0F052C00 → 0x0000004E`, `P0F053000 → 0x00007000`, etc.). Right
   now our model returns 0 for every bank. Some of these defaults
   carry status bits that the kernel polls; matching them may close
   the BIO loop without changing tick semantics. **This is the most
   targeted, lowest-risk experiment to try next.**

3. **Investigate whether the kernel's BIO chip detect path is what
   ultimately determines `TStackInfo`/`TStackPage` allocation order.**
   If yes, fixing the BIO model fixes the alias by side effect even
   though the alias is in StackManager.

4. **Bound delay loops via trace-count instead of tick-deadline.**
   Highest-effort fix and only viable in `--features trace`; probably
   not worth pursuing.

### Files changed this session

None. Slow-NEWTON_TICK_HZ experiment was reverted (jj abandon). Source
state is unchanged from the parent commit `wq 3588f7d5`.

### Reproduction artifacts

`/tmp/phaseB-2026-04-25/`:

- `qemu.log` — 120 s QEMU trace (197 k events).
- `einstein.trace` — 180 s NewtonTrace (89.6 M events).
- `einstein.head200k` — first 200 k trace events for fast diff.
- `qemu.pcs` / `einstein.pcs` — PC-only sequences for line-aligned
  diff (first divergence at line 16 780).
- `qemu.alloc` / `einstein.alloc` — `SSafeHeapPage::Alloc` arg
  sequences (first divergence at call 534).
- `freq.deltas` — per-function call-count delta, sorted, with names.
- `joined.named` — per-function first-trace-number side-by-side.

---

## Earlier — IRQ-rate + tick-page divergence fixed; newt-DABT alias narrows to scheduling order (QEMU, 2026-04-25 night)

**Status:** Two upstream divergences vs Einstein eliminated. Both
emerged from comparing per-trace function-set diffs against a fresh
NewtonTrace baseline.

### Fix 1 — `NEWTON_TICK_HZ` reduced from `× 16` to natural rate

`src/platform/raspi3b.rs::NEWTON_TICK_HZ` was `3_686_400 * 16`. The
multiplier was originally `× 128` (df05e998) to keep BootOS calibration
loops fast; reduced to `× 16` (579a2c72) when the alarm engine couldn't
keep up. Cross-check against Einstein in the first 130k trace events
showed our boot taking **107 timer IRQs** while Einstein took **0**:
the ×16 wall-clock-anchored tick rate made every kernel-armed
`match_reg` cross its deadline in ~1/16 of the wall time the kernel
intended, firing a flood of `PreEmptiveTimerInterruptHandler` /
`SetAlarm{,1,Atomic}` / `RestartTimerOverflowDetect` calls that each
allocated from the safe heap and perturbed subsequent allocations.
Setting the multiplier to `× 1` (= natural 3.6864 MHz, matching FVP
and matching what `kFreqGenFreq` constants throughout the kernel
assume) drops IRQ count to 12 in 130k events. All 35 guest tests
still pass — the comment's worry about early-boot calibration loops
turned out to be unfounded once the `tick_page` heartbeat was in
place.

### Fix 2 — refresh `TICK_PAGE` on every sync-trap exit

`K_HDWR_TICKS` (0x0F181800) is mapped non-trapping via the RAM-backed
`TICK_PAGE`, and `tick_page::update()` was only called from
`timer::on_irq` (≈ every 16 ms heartbeat). Tight delay loops like
`TSerialNumberROM::Init` at 0x1dd8d0 (1-Wire bit-bang protocol with
a `cmp r0, #20` deadline = 5.4 µs natural) read the cached page —
which stays constant between heartbeats — so each loop runs ~heartbeat
wall time regardless of the requested delay. On QEMU TCG with the
tracer feature each `GetHardwareTime` HVC adds 30+ µs of overhead,
amplifying the iteration count: we ran **11335 polls** through this
loop versus Einstein's **2698** (4.2× longer), accumulating ~9k
extra trace events that propagated downstream as scheduling drift.

Adding `crate::stage2::tick_page::update()` at the bottom of
`trap_sync_lower_aarch32` (after `update_virq`) makes every guest
sync-trap refresh the cached tick — exactly when the kernel is
between bursts of work and likely to re-read ticks. Drops the same
delay loop to **256 polls** (44× reduction), and brings TStackInfo::Init
#11 from trace 62439 → 50938 (Einstein 53611) and #12 from 133360 →
121574 (Einstein 125856). Per-trap cache-maintenance cost is one
`dc cvac` to a single hot line — negligible on a single-core boot.

### What's left — second TaskKillSelf still ~77k traces too early

Even with both fixes, the per-window function-set diff in
TStackInfo::Init #11..#12 still shows our run taking the
recycled-TStackInfo path (TForkWorld::~TForkWorld → TaskKillSelf →
TStackInfo::~TStackInfo → ScavengeAll → recycle slot 0x0c11aad8)
while Einstein takes the allocate-new path (TStackPage::TStackPage →
TPageTracker::Take → TPageManager::Get → fresh slot 0x0c117e18).
Trace counts:

| event | our run | Einstein | Δ |
|---|---|---|---|
| 1st TaskKillSelf (r2=0x0c111c98) | 36669 | 39342 | -2673 |
| 2nd TaskKillSelf (r2=0x0c310274) | 54822 | 131680 | **-76858** |

Both runs schedule the dying task `0x0c11aa88` exactly **58 times**
before it kills itself — same task, same amount of work. The drift
is in **how much time elapses between those 58 schedules**: Einstein
spreads them across 92k traces, ours across 18k. That's because in
Einstein's window a 5th `TUTask::Start` call has fired (drvl spawning
another driver task at trace 131283), and Einstein has scheduled two
extra task structs (`0x0c112e00` = STKF, `0x0c115c00`) that our run
hasn't reached yet by the time the dying task gets its 58th schedule.

So the remaining gap is: **why don't STKF / 0x0c115c00 get scheduled
in our run before the dying task finishes?** Open hypotheses:

1. **Some MMIO source we model differently** is suppressing an event
   that would unblock STKF. Compare what wakes STKF in Einstein and
   confirm the same wake-event path runs in ours.
2. **Drvl is making a request earlier in our run** because of timing,
   getting a faster response from drvr/drvl/PMGR/etc.
3. **A periodic IRQ Einstein receives that we don't** — even with
   IRQ count down to 1 in 130k, perhaps a specific source (sound DMA?
   GPIO?) fires in Einstein in this window.

### Files changed in this session

- `src/platform/raspi3b.rs` — `NEWTON_TICK_HZ = 3_686_400` (was
  `* 16`); comment updated.
- `src/trap.rs::trap_sync_lower_aarch32` — added
  `tick_page::update()` after `update_virq()` so every sync trap
  refreshes the cached tick value.

### Files preserved from prior session

`src/stage2.rs` flash bank 0/1 RO mapping, `src/trap.rs::drop_flash_write`,
`src/peripherals/flash.rs::is_flash_pa`, `src/mmio.rs` TEST_SCRATCH.
Both flash drop-write fix and IRQ-rate fix are needed for the current
trajectory.

---

## Earlier — flash recovery path eliminated; newt-DABT alias still present (QEMU, 2026-04-25 late night)

**Status:** Failure B (flash[0..4] DLDS corruption) resolved by mapping
flash stage-2 RO + dropping direct guest writes. The kernel now takes
Einstein's success path (`PersistentRecovery` at trace 1713, no
`UpdateBlock0FromBlock1` / `EraseRange` / `T{16,32}BitFlashRange::DoWrite` /
`MarkStoreAsValid` / `CompareFlashAndMemRebootIfDifferent` etc. ever
fires). Trace count drops from ~305 K to ~234 K and the recovery-cycle
function set is gone from `awk '/^trace / && !seen[$4]++'`.

The newt-DABT alias still triggers, now earlier (trace 174 655 vs.
234 292 before): `TCardServer::AddCardHandler → __nw__FUi(184) →
TCardMessage::TCardMessage at VA 0x0cc82250 → strs 'newt'/'cdsv' over
pckm's `sp_usr+8` save slot at PA 0x0402a250`. The flash recovery cycle
was a *concurrent* heap-state perturbation but not the root cause of
which TStackPage the kernel picks for the alias — that decision is
upstream.

### What changed in this session

#### Failure point A (still FIXED): ROM/REx checksum drift after fix_stage1_xn_bits

`trap.rs::reseed_flash_checksums_if_needed()` runs after every
`fix_stage1_xn_bits` invocation. The kernel's runtime
`CalculateROMREXCheckSums` matches what's written into
`flash[0x64..0x8C]`, the `operator==(TROMREXCheckSums)` returns true.

#### Failure point B (NOW FIXED): direct guest writes to flash bank IPAs

Einstein's `TMemory::WriteP` at `Emulator/TMemory.cpp:1777` silently
ignores all direct CPU writes to flash bank addresses
(`kFlashBank1..End`, `kFlashBank2..End`); flash is mutated only via the
`TEinsteinFlashDriver` native primitives (`WriteToFlash16/32Bits`,
`EraseFlash`). Our hypervisor was mapping the banks stage-2 RW, so
AMD-style command-sequence writes the kernel's flash chip code emits
landed in the backing and corrupted the seeded DLDS header to
`0x00FF00FF`.

**Fix:** Map flash bank 0 (`0x02000000..0x02400000`) and bank 1
(`0x10000000..0x10400000`) RO at stage-2; intercept stage-2 RO write
faults to those IPAs in `trap::handle_data_abort` and silently drop
them (matching Einstein). Flash mutations the kernel actually wants
to commit go through `peripherals::flash_driver` which writes the
host backing directly via `flash::program_word` / `flash::erase_block`,
bypassing stage-2 entirely.

`drop_flash_write` handles ISV=1 (just advance ELR) and the common
ISV=0 forms (STR/STRH-immediate with writeback, STR-register) so any
AMD-style sequence, post-indexed write, etc. is absorbed without
mutating the backing or losing the writeback to Rn. Unsupported forms
fall through to the existing loud-halt path so future surprises
surface.

### Boot trajectory after fix

| metric | original | partial (A) | full (A + B) |
|---|---|---|---|
| total trace events (~120s wall) | 261 949 | 304 954 | 234 069 |
| unique functions | 1290 | 1294 | 1222 |
| flash recovery cycle | active | active | none |
| newt-tripwire fires at trace | 194 617 | 234 292 | 174 655 |
| ultimate wedge | newt UnhandledException | (unchanged) | (unchanged) |

The newt-tripwire firing earlier is consistent with the heap state no
longer being delayed by the recovery cycle's allocations. The
underlying alias bug (`AddPgPAndPerm(VA=0x0cc82000, PA=0x0402a000)`
with pckm's stack already at PA 0x0402a000 via VA 0x0cc7a000) is
unchanged.

### Where the residual divergence lives (TStackInfo::Init #12)

Diffing `TStackInfo::Init` invocations between our run and Einstein
locates the first heap-allocator divergence precisely:

```
call # | our run                            | Einstein
-------|------------------------------------|------------------------------------
1..11  | r0 (StackInfo*) and args identical | (identical)
12     | r0=0x0c11aad8 r1=cc93000 r2=cc83400 r3=12 | r0=0x0c117e18 r1=cc93000 r2=cc83400 r3=12
```

i.e. the kernel calls `TStackInfo::Init` with byte-identical
`(vaddr_base, max_addr, mode_flags)` arguments but the TStackInfo
allocation address differs starting at the 12th call. The args going
in match, so the kernel is doing the same logical work — but the
allocator (going through `operator new(0x48) → malloc → NewPtr →
SSafeHeapPage::Alloc`) has reached a different state.

Trace-count drift between TStackInfo::Init #11 and #12: ours runs
~2873 more trace events than Einstein in that window. Function-set
diff in the same window (us vs. Einstein):

- **In our run, missing in Einstein:** `DispatchIRQInterrupt`,
  `IRQHandler`, `IRQCleanUp`, `PreEmptiveTimerInterruptHandler`,
  `RestartTimerOverflowDetect`, `SetAlarm{,1,Atomic}`,
  `SMemCopyToSharedSWI`, `LowLevelCopyDoneFromKernelGlue`,
  `DeleteTask/Port/SharedMemMsg`, `RemovePgPAndPerm`,
  `PrimForgetMapping`, `ForgetMapping`. These are mostly **timer-IRQ
  / alarm-loop bookkeeping** plus shared-mem-message teardown — work
  that Einstein doesn't do here at all.
- **In Einstein, missing in our run:** `TStackPage::TStackPage`,
  `TStackPage::Init`, `TStackManager::AllocNewPage`,
  `TPageTracker::Take`, `TPageManager::Get`,
  `TPageManager::MonitorProc`, `TUDomainManager::Get`. Einstein takes
  the **page-not-found** branch out of
  `FindOrAllocPage_ReturnUnLockedOnNoPage` and allocates a new
  TStackPage; our run takes the **page-found** branch (because some
  earlier work left a matching page already cached in our heap).

So the residual divergence has two components:
1. **Our run takes more timer/alarm IRQs than Einstein** in this
   window. Even though hypervisor-side ratcheting + ROM-patched
   `addls→addcc` keeps the alarm engine from wedging, the alarm
   *frequency* differs — every alarm IRQ runs `RestartTimerOverflow-
   Detect`, `SetAlarm`, `SetAlarm1`, `SetAlarmAtomic` and may queue
   work that allocates from the safe heap.
2. **The heap that backs `TStackInfo` allocation has been touched by
   different things,** so the next `operator new(0x48)` returns a
   different chunk. Once the StackInfo addresses diverge, the
   downstream `RememberMapping(VA=0x0cc82000)` vs Einstein's
   `RememberMapping(VA=0x0cca3000)` follows mechanically.

### Open next steps

1. **Compare alarm-IRQ frequency vs Einstein's.** Count
   `RestartTimerOverflowDetect` invocations in the first 130 k trace
   events on both sides. If ours fires materially more than
   Einstein's, the alarm/timer-tick model is off — likely
   hypervisor-side `vic::ticks` cadence vs Einstein's
   `TInterruptManager::Tick`. Cross-reference `peripherals/vic.rs`
   tick-page update cadence and the kernel's `gAlarm` queue depth.
2. **Audit safe-heap allocations between TStackInfo::Init #11 and
   #12.** Trace `SSafeHeapPage::Alloc` calls in this window on both
   sides; the first call whose return address (= allocated chunk
   start) differs is the upstream perturbation.
3. **Compare ROM patch sets.** Einstein's `TJITGenericROMPatch` table
   includes patches that the hypervisor doesn't replicate (the
   `TJITGenericPatchNativeCall` and `TVirtualizedCallsPatches` entries
   listed as "not yet ported" in PLAN.md §A.7). Some of those gate
   timer behaviour (`RealClockSeconds`, `FTimeInSeconds`,
   `FDateFromSeconds`); even one un-ported patch can change how the
   kernel queues alarms.
4. **Re-run on FVP for cross-check.** QEMU and FVP have agreed on
   the alias wedge since 2026-04-26; another FVP run with the new
   trace counts should confirm the residual divergence isn't a
   QEMU-only artifact.

### Files changed in this session

- `src/stage2.rs` — flash bank 0/1 mapped `BLOCK_NORMAL_RO` instead
  of RW; comment + log lines updated.
- `src/trap.rs` — `handle_data_abort` recognises flash-bank writes
  and silently drops them via new `drop_flash_write` helper
  (handles ISV=1 + common ISV=0 STR/STRH/STR-register forms,
  applying any base-register writeback so the guest's CPU state is
  consistent with a successful store).
- `src/peripherals/flash.rs` — added `is_flash_pa` helper.
- `src/mmio.rs` — added `TEST_SCRATCH` (R/W byte-array storage at
  IPA `0x12000000..0x12000010`) so `test_shadow_stub` can verify
  the inline-stub no-XOR branch above XOR_LIMIT now that flash
  bank 1 is RO. Ordered ahead of `NO_REX_PROBE_BASE..END` so the
  scratch sub-window wins the dispatch arm.
- `guest-tests/tests/test_flash.S` — rewritten to verify the new
  drop semantics (writes silently dropped, seeded header preserved,
  bank independence).
- `guest-tests/tests/test_shadow_stub.S` — `SCRATCH_HI` now points
  at `0x12000000` instead of flash bank 1.

---

## Earlier (preserved) — newt-DABT root cause: kernel stack-collision picks the wrong reuse VA (QEMU, 2026-04-25 night, post-NewtonTrace cross-check)

**Direct comparison with Einstein NewtonTrace + NewtonProbe nailed
the divergence.** Both implementations end up with PA 0x0402a000
mapped at multiple VAs — but in Einstein the *other* VAs are
addresses no code ever writes to, so the alias is benign. In our
hypervisor's run the second VA is 0x0cc82000, which the
`TCardServer::TCardServer` allocation array writes a chain of
TCardMessages into via `__vc__FPvT1iPFPv_v` (the C++ array
constructor at ROM 0x34502c). Those writes corrupt pckm's saved
stack at PA 0x0402a248, which is where pckm resumes on its next
schedule.

### Cross-implementation evidence (NewtonProbe / NewtonTrace, 2026-04-25)

Built `NewtonTrace` (`baremetal/probe/trace.cpp`) and ran the same
ROM 100 s wall under Einstein. Boot reaches **7.6 M trace events /
3551 unique functions**, deep into `TSCPLoader / TEndpointPipe /
TFramedAsyncSerTool / TCircleBuf` — no DABT, boot is healthy.

Aggregate page-table activity over the same run:

| metric | Einstein 100 s | hypervisor 100 s |
|---|---|---|
| `CopyPagesAfterStackCollided` calls | 100 | 4 |
| `AddPgPAndPerm` calls (total) | 1083 | (smaller) |
| `AddPgPAndPerm` with PA = 0x0402a000 | 23 | 7 |
| `CopyPhysicalPage` with dst PA = 0x0402a000 | 1 | 2 |
| TCardMessage allocations | 62 | 61 |
| TCardAsyncMsg allocations | 38 | 48 |
| `SwapInGlobals` for pckm (0xc118dd8) | 275 | (many) |

Einstein resuses PA 0x0402a000 for many different VAs over the run
(0x0cc7a000 for pckm initially, then 0x0cca3000 and 0x0ccac000 in
later remappings). Our hypervisor reuses it for VA 0x0cc7a000,
0x0cc79000, and **0x0cc82000** — and 0x0cc82xxx is exactly where
the kernel's TCardServer constructor places its 184-byte
TCardAsyncMsg array (62 elements spanning 0x0cc7fxxx..0x0cc82xxx,
allocated via `__vc__FPvT1iPFPv_v` at ROM 0x34502c, called from
`TCardServer::TCardServer` at lr=0x000529a0 — see trace 180956).

`TCardMessage::TCardMessage` at ROM 0x4ed3c..0x4ed48 stores the
ASCII fourcc literals `'newt'` (0x6e657774) at `*(self+0)` and
`'cdsv'` (0x63647376) at `*(self+4)`. For the message instance
allocated at VA 0x0cc82250, those stores land at PA 0x0402a250 /
0x0402a254 — exactly pckm's `sp_usr+8 / sp_usr+12` save slots.

### Why the same alias in Einstein doesn't crash

In both runs the running task at the destructive `CopyPhysicalPage`
is **STKU** (`0x0c113dd8`), and pckm's saved sp_usr is `0x0cc7a248`
with stage-1 walk → PA 0x0402a248. Pckm task struct, save area,
and L1/L2 for that VA range are *bit-identical* across
implementations.

The divergence is **which other VA the kernel maps to PA
0x0402a000**:

```
                         our run             Einstein
init pckm stack          AddPgPAndPerm       AddPgPAndPerm
  trace ours / Einstein  55088               49142
  VA / PA                0x0cc7a000 → PA     0x0cc7a000 → PA
                         0x0402a000           0x0402a000

reuse #1 of PA 0x0402a000:
  trace                  156706              145256
  VA                     0x0cc82000          0x0cca3000   <-- different VA
  caller (RememberMapping r1) same path, same TPhys id 0x176b/0x176b

reuse #2 of PA 0x0402a000:
  trace                  163684              150438
  VA                     0x0cc79000          0x0ccac000   <-- different VA
```

Einstein's reuse VAs (0x0cca3000, 0x0ccac000) are heap addresses no
code writes to. Our reuse VA (0x0cc82000) is in the user-RAM range
the TCardAsyncMsg array constructor *will* fill — the allocator's
`new(184)` chain wrote sequentially up to 0x0cc82520, painting
'newt'/'cdsv' over PA 0x0402a000+0x250 along the way.

### Why the kernel chooses a different VA

The reuse VA is determined by the `TStackInfo` / `TStackPage` pair
the kernel passes to `CopyPagesAfterStackCollided`. The `r1` arg
(`r1 = ldr [params+16]`) is the destination StackPage; its
`vaddr_base + sub_page << 12` becomes the new VA. So the
divergence is **which TStackPage the kernel picks as the source
of the migration**, which depends on which user-task hit a stack
collision at that moment.

`SwapInGlobals` data shows STKU is running at the moment in both
runs. But the stack-collision parameters differ — one of the
upstream `TStackManager::ResolveFault` paths is selecting a
different `TStackInfo` because heap state diverged earlier in
boot. The `r2` ID passed to the second `GetPhys` (per
`CopyPagesAfter` at 0x1f7610) lookup is **0x000013cb** in our run
vs **0x0000160b** in Einstein — different TPhys descriptors, even
though both eventually resolve to PA 0x0402a000.

So the chain is:

  our heap state at trace ~156k diverges from Einstein's
  → kernel picks TStackPage X (with TPhys id 0x13cb) as collision
    source instead of TStackPage Y (id 0x160b)
  → CopyPagesAfter remaps VA 0x0cc82000 → PA 0x0402a000
    instead of VA 0x0cca3000 → PA 0x0402a000
  → TCardServer's later allocator fills VA 0x0cc82000
  → corrupts pckm's saved stack at PA 0x0402a250
  → pckm resumes, reads `newt`/`cdsv` from sp_usr+8/+12,
    DataAbortHandler fires, recursive abt → "warm reboot!"

### Refined model (post `dump_all_phys` walk, 2026-04-25 evening)

Adding `task_dump::dump_all_phys` / `dump_phys_for_pa` (commit
`963b3389`) walking all three known kernel object tables shows:

- The kernel uses **three** TObjectTable instances, two of which
  carry TPhys:
  - `*(0x0c101164)` — `gPhysAllocator` at `0xc1082a0`. Holds **39
    RAM-page TPhys** descriptors, one per 4 KiB page covering
    PAs `0x04018000..0x04039000`. `GetPhys` (ROM `0x11c168`) hits
    this table first when the requested type is 0xb.
  - `*(0x0c100fc8)` — points at `gObjectTable` (`0xc10fc34`).
    Holds **8 MMIO-region TPhys** at PAs `0x30000000..0x68000000`
    (PCMCIA controller bases).
  - `gObjectTable` itself — same 8 entries (the `*(0x0c100fc8)`
    pointer dereferences to it).
- **Exactly one TPhys claims PA 0x0402a000.** It's id `0x176b` at
  TPhys-VA `0xc10f944`, in `gPhysAllocator`. So the "alias" is
  *not* "two TPhys, one PA" — it's "one TPhys, two L2 entries".
- The L2 page-table corruption sequence is therefore:

  ```
  trace 156706  AddPgPAndPerm(VA=0x0cc82000, PA=0x0402a000)   # write L2[0x82]=0x0402a00e
  trace 156751  CopyPhysicalPage(0x0402a000, 0x0401f000, 2)    # cnt=0x02 = bitmap → bytes 0x400-0x7FF
                                                              # of pckm's stack page get
                                                              # overwritten with whatever is at
                                                              # PA 0x0401f000 (which had been
                                                              # VA 0x0cc82xxx's prior backing).
  ...
  trace 180958  TCardAsyncMsg::TCardAsyncMsg @ VA 0x0cc7fd70
                ...62 ctor calls cycling through VAs up to 0x0cc82520
  trace 180652  TCardMessage::TCardMessage at VA 0x0cc82250
                writes 'newt' to *(self+0) and 'cdsv' to *(self+4)
                → those land at PA 0x0402a250 / 0x0402a254 = pckm's sp_usr+8/+12
  ```

  i.e. the corruption isn't `CopyPhysicalPage`'s 0x400-byte chunk
  (which doesn't intersect `sp_usr=0x248`); it's the kernel
  allocator's *direct* writes through the alias VA after the
  remap — `TCardServer::TCardServer`'s array constructor fills
  TCardMessages, and the one at VA 0x0cc82250 paints "newt"/"cdsv"
  on top of pckm's saved stack frame.

### Why Einstein doesn't crash with the same logic

Einstein's `RememberMapping` for the recycled PA 0x0402a000 chooses
**VA `0x0cca3000`** instead of our `0x0cc82000`. Same kernel TPhys
(id `0x176b`), same physical destination, but the *new* L2 entry
sits in a region where no later allocator writes — so PA
0x0402a000 keeps the (mostly-zero) contents from the post-copy
state. Pckm wakes up, reads `sp_usr+8` = 0, the
`cmp r5, #0; strne r1, [r5]` predicate at ROM 0x3ae234..0x3ae238
sees Z=1, the `strne` is skipped, and pckm proceeds normally.

### What determines the new VA

`TStackManager::CopyPagesAfterStackCollided` (ROM `0x1f7540`) takes
its destination StackPage from `params[+16]`. In our run the
destination StackPage's VA range covers 0x0cc82xxx; in Einstein's,
it covers 0x0cca3xxx. The choice is upstream of `CopyPagesAfter`
in `TStackManager::ResolveFault` / `FindOrAllocPage`. The
divergence has to be in *which TStackInfo* gets selected (i.e.
which task is the collision target), and that depends on the heap
state established by earlier boot.

### Open next steps

1. **Audit `gPhysAllocator` at the boot phase corresponding to
   trace ~156k.** With `dump_all_phys` we can now print the PA
   map of every RAM page on both sides. If our hypervisor's
   layout of {id → PA} differs from Einstein's at the same phase,
   that's the upstream divergence. Add the same walker to
   `baremetal/probe/probe.cpp` so the dumps are byte-identical
   when the kernel state is byte-identical.

2. **Audit StackInfo / StackPage state at the wedge.** The new
   `dump_full` doesn't yet enumerate `TStackInfo`; add one. The
   key fact is `StackInfo[+16]` (= "VA base") for the destination
   page in the `CopyPagesAfter` call — if our run's StackInfo
   has VA-base 0x0cc82000 while Einstein's has 0x0cca3000, the
   StackInfo pool is the divergent input.

3. **`RememberMapping` audit.** Hook a count + last-args probe
   on `RememberMapping` (ROM `0x11c7d8`) and emit a periodic
   summary. The first call where `(va_arg, phys_id_arg)` differs
   from Einstein's same-numbered call is the upstream divergence.

4. **`__nw__FUi(184)` audit.** The TCardAsyncMsg ctor allocates
   from a heap whose growth determines which VAs get filled. In
   our run the array spans 0x0cc7fd70..0x0cc82520 (62 entries, 0xCC
   stride). In Einstein's run the same array spans different VAs.
   Identify the heap that's serving these allocs and compare
   its size + growth pattern.

(Note: prior speculation about TPhys descriptors with duplicate
PAs is wrong — confirmed via the new walker. Likewise the
"CopyPhysicalPage corrupted bytes 0x248" story was wrong: the
copy's 0x02 bitmap covers only bytes 0x400..0x7FF.)

---

## Resolved — PCMCIA controller chip-detect (QEMU, 2026-04-25)

Fresh cold-boot trace identified that the boot reaches `TCardServer::MainConstructor`
→ `TCardSocket::GetChipInfo` (ROM 0x55714) which writes a magic
pattern to controller reg_3000 / reg_3800 and reads it back to detect
the chip. Our `peripherals/pcmcia.rs` returned `0xFFFF_FFFF` for every
read regardless of writes, failing the pattern check, and steering
boot into the heavy "no chip" teardown path
(`TUPhys::Invalidate` → `DeletePhys` → `~TCardSocket` → flood of
`TCardAlertEvent` / `TCardAlertDialog` / `TCardSystemEventHandler`
allocations).

### Original symptom analysis (still valid)

1. `TStackManager::CopyPagesAfterStackCollided` at trace 156102
   remaps VA 0x0cc82000 from PA 0x0401f000 to PA 0x0402a000
   (`AddPgPAndPerm(0x0cc82000, 0, 0x0402a000, 1)` at trace 156093,
   then `StoreToPhysAddress(0x04023608, 0x0402a00e, ...)` at trace
   156097 — the actual L2[0x82] write). PA 0x0402a000 is still
   mapped at VA 0x0cc7a000 as pckm's user stack (mapped at trace
   54992, never unmapped) — this is the alias.
2. The collision arises because the kernel allocates a long chain
   of `TCardMessage` / `TNewCardAsyncMsg` objects spanning 0x0cc7fxxx
   .. 0x0cc82xxx (≥160 of them, see traces 179690..180815). The
   chain runs out of fresh PA/VA space and the kernel resorts to
   `CopyPagesAfterStackCollided` to recycle a page.
3. The TCardMessage chain is built by `TCardServer::MainConstructor`
   → `TCardEventHandler::Init` → ... → `TCardSocket::Init`. After
   socket init the kernel calls **`TCardSocket::GetChipInfo`** at
   trace 205540, which fails to detect a chip.
4. `GetChipInfo` (rom.dis @ 0x55714) does the standard chip-detect
   ritual: write `0xa5a5` to `base+0x3000`, write `0x5a5a` to
   `base+0x3800`, read both back, verify the 16-bit values stuck.
   On chip-detect failure (`r2=0`) the function bails to
   `Subexception` and the path triggers `TUPhys::Invalidate`
   (trace 205560) → `DeletePhys` (215486) → `TCardSocket::~TCardSocket`
   (215550) → a flood of `TCardAlertEvent` / `TCardAlertDialog` /
   `TCardSystemEventHandler` / `TNewCardAsyncMsg` allocations.

### Why chip-detect fails

`src/peripherals/pcmcia.rs::read` returns `0xFFFF_FFFF` for every
PCMCIA register read, regardless of prior writes. The kernel writes
`0xa5a5` to reg_3000, reads back `0xFFFF`, masks to 16 bits, fails
the `teq r8, ip` check, sets `r2=0`, takes the no-chip path.

Einstein's `Emulator/PCMCIA/TPCMCIAController.cpp` implements proper
register storage: writes to reg_3000 / reg_3800 are persisted, reads
return the stored value. So GetChipInfo on Einstein detects the
chip, and the kernel takes a different (less-allocation-heavy) path
even with no card actually inserted (no-card is reported via
reg_1C00's `k1C00_CardIsPresent` bit, not via failed chip-detect).

### Fix (landed)

`src/peripherals/pcmcia.rs` rewritten to model 4 sockets
(SLOT0..SLOT3 at base 0x3000_0000 / 0x4000_0000 / 0x5000_0000 /
0x6000_0000), each with a 17-register storage cell mirroring
Einstein's `TPCMCIAController.cpp`:

- Card-side spaces (attribute/IO/memory at offsets 0..0x0BFF_FFFF
  inside a slot): reads return 0, writes are dropped (no card).
- Controller registers at offsets 0x0C00_0000..0x0C00_4400: simple
  R/W storage (so chip-detect's stuck-write check passes).
- reg_1C00 reads OR'd with `k1C00_CardIsPresent (0x000C)` to flag
  "no card inserted".
- reg_4400 reads as 0xFC (Einstein hardcoded).

In `src/trap.rs::handle_data_abort`, added `try_emulate_isv0_dabt`
fallback for ISV=0 stage-2 aborts on LDR/STR-immediate (A1) forms.
Newton uses pre-indexed-with-writeback LDR for PCMCIA controller
register access (e.g. `DisableSocketInterrupt @ 0x55208`), and the
syndrome can't carry the destination register for that form. The
fallback fetches the instruction at ELR, decodes the LDR/STR-imm
fields, performs the MMIO access, applies writeback to Rn — then
falls through to `advance_elr(4)`.

Added MMIO read entries at `0x0F18_CC00 / D000 / D800 / DC00 /
E000` returning 0 — `TGPIOInterface::DisableInterrupt` does
read-modify-write on these GPIO interrupt-control registers; the
matching write entries already no-op.

Test: `guest-tests/tests/test_pcmcia.S` rewritten to verify chip-
detect storage works (write 0xa5a5 to reg_3000, read back), no-card
flag is reported in reg_1C00, reg_4400 returns 0xFC, slot isolation
works. All 23 guest tests pass.

Boot trajectory after fix: 219k → 264k trace events / 1266 → 1290
unique functions. Next stalls listed above under "Currently at".

(Earlier-section content preserved below for reference.)

---

## Earlier — pckm task at sp_usr=0x0cc7a248 reads TAEventHandler bytes instead of stack frame (QEMU + FVP, 2026-04-27)

**Root divergence narrowed**: the recursive "newt" DABT
(FAR=0x6e657774) is caused by the pckm task (id=0x1753, struct at
0x0c118dd8) resuming with sp_usr=0x0cc7a248 and reading user RAM at
sp+8 / sp+12 that contains the literal ASCII fourccs `'newt'` and
`'cdsv'` instead of the stack pointers TUPort::Receive's prologue
(0x259d2c) should have pushed there.

### Evidence (one-shot diagnostic dump in DABT-fast-path)

`src/task_dump.rs::dump_save_area_for_named` fires once at the FAR=
0x6e657774 forward and prints the SWIBoot context-save area
(task+0x10..0x54) plus a ±0x80 user-stack window plus a stage-1 walk.

For task `0x0c118dd8` (id=0x1753, named `cdsv` in our run, named
`pckm` in Einstein's run — same struct slot, same task throughout
the boot, just different `find_task_name` heuristic hits as the
globals area gets repopulated by the AppWorld over time):

```
Our hypervisor                          Einstein (NewtonProbe)
sp_usr  = 0x0cc7a248                    sp_usr  = 0x0cc7a248        (SAME)
saved-PC = 0x003ae230                   saved-PC = 0x003ae230        (SAME)
lr_usr  = 0x00259d48                    lr_usr  = 0x00259d48         (SAME)
fp/ip   = 0x0cc7a29c / 0x0cc7a2b0       fp/ip   = 0x0cc7a29c / 0x0cc7a2b0  (SAME)

stage-1 walk: VA 0x0cc7a248 → PA 0x0402a248
                                stage-1 walk: VA 0x0cc7a248 → PA 0x0402a248  (SAME PA)

user-stack window @ sp_usr:
  [+0]=0  [+4]=0                        [+0]=0x0c600d2c  [+4]=0x0c600d1c  (pushed r4,r5 from PortReceiveSWI)
  [+8]=0x6e657774  ("newt")             [+8]=0x0cc7a270   (push of r0=sp+16 from 259d2c)
  [+12]=0x63647376 ("cdsv")             [+12]=0x0cc7a26c  (push of r1=sp+12)
  [+16..+20]=0,0                        [+16]=0x0cc7a264  [+20]=0x0cc7a268  (push of r2,r3)
```

Both implementations pick the same VA→PA, save the same context.
Only the contents at PA 0x0402a248..0x0402a25f differ. Einstein has
the four valid stack pointers from TUPort::Receive's `push {r0..r3}`;
ours has the literal pattern of a `TAEventHandler{ signal='newt',
class='cdsv', ...}` (signal at +0x08, class at +0x0c — see
`docs/STRUCTURES.md` "TAEventHandler"). Trace 183155 in our run is
the only `TAEventHandler::Init(handler, 'cdsv', 'newt')` call, but
its handler address was `0x0c602e2c`, not `0x0cc7a248` — so the
pattern at PA 0x0402a248 came from somewhere else.

### Faulting site

When pckm resumes at PC=0x3ae230 (= post-`svc #2` in `PortReceiveSWI`
at 0x3ae228):

```
003ae228 <PortReceiveSWI>:
  3ae228: push {r4, r5}
  3ae22c: svc  #2
  3ae230: ldr  r5, [sp, #8]    ; r5 ← 0x6e657774 ("newt")
  3ae234: cmp  r5, #0
  3ae238: strne r1, [r5]       ; ← DABT here, FAR=0x6e657774, DFSC=0x05
                                 ;   (translation, section — no L1 entry
                                 ;    for the 0x6e000000..0x70000000 range)
```

The L1 fault recurses through DataAbortHandler → ConvertIdToObj →
Throw → UnhandledException → "Unhandled exception evt.ex.abt.bus,
warm reboot!".

### Root cause confirmed: stage-1 page-table aliasing

Per-trace-event tripwire (`src/tracer.rs::log_trace_at`) bisected the
write to **trace 180652** (= `TCardMessage::Clear` entry — but the
write actually happened in the prior trace event):

```
trace 180650 0x0004ed10 TCardMessage::TCardMessage(void) (usr) r0=0x0cc82250 ...
trace 180651 0x00025d1c TAEvent::TAEvent(void)         (usr) r0=0x0cc82250 ...
trace 180652 0x0004ed84 TCardMessage::Clear(void)      (usr) r0=0x0cc82250 r1=0x6e657774 ...
*** newt-tripwire fired AT trace 180652 (PA 0x0402a250=0x6e657774 0x0402a254=0x63647376)
```

`TCardMessage::TCardMessage` at 0x0004ed10 explicitly stores
"newt"+"cdsv" into its `self`:

```
4ed3c: ldr r0, [pc, #44]    @ 0x4ed70 = 0x6e657774 ('newt')
4ed40: str r0, [r4]          ; *(self+0) = 'newt'
4ed44: ldr r0, [pc, #40]    @ 0x4ed74 = 0x63647376 ('cdsv')
4ed48: str r0, [r4, #4]      ; *(self+4) = 'cdsv'
```

with `self = r4 = 0x0cc82250` for this allocation. The two literals
are the magic class IDs used to identify TCardMessage in untyped
buffers (the constructor calls them after the TAEvent base ctor and
before its own `Clear`).

**The kicker — page-table alias:**

```
*** stage-1 walk for VA 0x0cc82250 (TCardMessage write target):
  L1[0xcc] = 0x04023481  (coarse, L2 @ PA 0x04023400)
  L2[0x82] = 0x0402a03e  (small)
  → PA 0x0402a250

*** stage-1 walk for VA 0x0cc7a250 (pckm sp_usr+8 read site):
  L1[0xcc] = 0x04023481  (same coarse table)
  L2[0x7a] = 0x0402a03e  (small) ← same PA
  → PA 0x0402a250
```

`L2[0x7a]` and `L2[0x82]` of the same kernel L2 table both map to PA
0x0402a000. So a write through VA 0x0cc82250 lands at the same
physical page that backs pckm's user-stack VA 0x0cc7a000. When pckm
next resumes and `PortReceiveSWI` reads `[sp_usr+8]`, it reads the
"newt"/"cdsv" magic from the TCardMessage instead of the stack
pointer that `TUPort::Receive` 0x259d2c pushed there.

This is a *kernel-side* divergence — the kernel's heap/page allocator
picked PA 0x0402a000 for the new TCardMessage even though that page
was already mapped at VA 0x0cc7a000 as pckm's stack. Einstein doesn't
do this, so its L2 entries don't alias.

### Open next steps

1. **Find the diverging allocation.** Walk back from the TCardMessage
   alloc (`__nw__FUi(184)` at trace 180650) and identify why the
   kernel's TPageManager / heap chose PA 0x0402a000. Compare against
   Einstein's allocation order.
2. **Bisect the earlier divergence.** The two implementations agree
   on L1/L2 layout for many earlier pages. The first L2 entry that
   diverges between Einstein and our hypervisor is the clue. Add a
   diagnostic that dumps both L1 + L2 contents at periodic
   intervals and diff against Einstein's NewtonProbe.
3. **Investigate likely peripheral-state-driven divergence.** The
   `TNewCardAsyncMsg` chain is in the PCMCIA card-insertion path
   (`TCardSocket::~TCardSocket`, `TCardAlertEvent`, `TCardPart-
   Handler` were already traced as new-territory functions before
   the fault). Our PCMCIA driver returns different state than
   Einstein's, plausibly steering the heap allocator down a path
   that reuses pckm's stack page.

---

## Earlier — kernel-mode "newt" UnhandledException (QEMU + FVP, 2026-04-26)

After resolving the STKU wedge (see "Resolved — STKU wedge: QEMU
Bug #1 leak from unaligned `msr spsr_el2`" below), QEMU now reaches
the same `0x6e657774` ("newt" ASCII) recursive kernel-mode DABT that
FVP has hit since Apr 24 — both platforms now agree.

A fresh cold-boot QEMU trace runs to ~213k entries with **1262
unique functions** (vs. ~1087 / 156k pre-fix), advances `gCurrentTask`
past STKU → cdsv → and into `Throw`/`UnhandledException` /
`Subexception` / `__vfprintf` user-mode reporting code. The trace
ends with the Reboot canary firing at IPA 0x00FFFF58, mode UND:

```
trace 213650 0x00393114 DataAbortHandler (abt) ... lr=0x003ae240
trace 213652 0x0011fc60 FaultMonitorEntry(unsigned long) (abt) ...
trace 213657 0x00250864 RebootIfFaultWasInStack (abt) r0=0x6e657774 ...
trace 213658 0x000b00c8 Throw (usr) r0=0x000afda0 r1=0x6e657774 ...
trace 213663 0x000b0220 UnhandledException(char *, ...) (usr) ...
putc 213671..213722: "Unhandled exception evt.ex.abt.bus, warm reboot!"
```

Decode (from the prior FVP-side finding still applies verbatim):

- Faulting PC = `LR_abt - 8 = 0x259d40` = `ldr r0, [r0]` in
  `TUPort::Receive` (just before `bl PortReceiveSWI` at `0x259d44`).
- Faulting VA = `0x6e657774` = "newt" ASCII — `r0` was loaded from
  `[fp, #4]` (caller's saved arg0 = `self`) and dereferenced. The
  TUPort `self` pointer is occupying "newt" ASCII bytes.
- mode=0x17 (ABT) → recursive abort: kernel was inside its DABT
  handler when the next access faulted.

Pre-failure path now passes through `cdsv` task initialisation
(`SwapInGlobals 0xc118dd8 → 0x00393114 DataAbortHandler` at trace
213649/213650), which means the cdsv task struct itself contains
"newt" ASCII at the offset that `TUPort::Receive` dereferences. This
is the same Apr 24 finding from the QEMU run that briefly
"transient-cleared" before the regression we just resolved hid it.

### Open next steps

1. Read the saved `cdsv` task struct at `0xc118dd8` — specifically
   the per-task-globals area at `task+0xa0` and the TUPort field that
   `TUPort::Receive` reads — to identify which slot holds the "newt"
   ASCII bytes.
2. Find which symbol-table entry holds the literal `0x6e657774`
   pattern (per Apr 24 hypothesis: a runtime symbol-name lookup
   returned a name string instead of a code/data pointer). The
   symbol prefixes `newtConnects`, `SYMnewtaboutview`, `SYMnewtinfobox`
   are candidates.
3. Walk back from the SwapInGlobals at trace 213649 in the function
   trace to find what *created* the cdsv task and what it intended to
   pass as the TUPort `self`. The corruption could be the kernel
   storing the symbol name into a pointer slot (off-by-one in a
   shared-memory layout), or our hypervisor mishandling a prior
   write to that slot (symbol-table region or RAM page alias).

## Resolved — STKU wedge: QEMU Bug #1 leak from unaligned `msr spsr_el2` (2026-04-26)

The STKU page-copy SWI wedge (PC=0x3ae1bc / SVC mode, persistent for
several minutes) was caused by **QEMU Bug #1** triggered from
`unaligned::set_return`. The fix is in `src/unaligned.rs::set_return`:
delegate to `trap::return_to_guest_from_und`, which ERETs into the
existing `UND_RETURN_STUB` at IPA `0x00FFFFE4` while leaving SPSR_EL2
untouched. The mode switch happens AArch32-side via `movs pc, lr` and
never goes through QEMU's buggy MSR helper.

### Root cause

The Newton ROM has ~1300 sites that depend on SA-1100 rotate-LDR
semantics for unaligned word loads (`UstrlenPrivate` at 0x1944b8
alone fires a fault on every other call — UTF-16 strings are 2-byte
aligned). With `SCTLR_EL1.A` forced on, each unaligned LDR raises an
alignment fault that the DABT-vector trampoline forwards to EL2 via
`HVC #ALIGN_TAG`. `handle_align_fault` decodes and emulates the
load, then called `set_return` to `msr elr_el2 / msr spsr_el2` and
ERET back to the pre-fault mode.

Per `docs/QEMU_BUGS.md` Bug #1, **`msr spsr_el2, x` from EL2 leaks
`x` into AArch32 SPSR_svc (banked_spsr[1])**. A direct probe
(`mrs spsr_el1` before/after the buggy write) confirmed the leak:

```
qemu-clobber-probe[4]: SPSR_EL1 pre=0x000001d3 post=0x000001d3 (wrote spsr_el2=0x200001d3)
qemu-clobber-probe[5]: SPSR_EL1 pre=0x200001d3 post=0x200001d3 (wrote spsr_el2=0x600001d3)
qemu-clobber-probe[6]: SPSR_EL1 pre=0x600001d3 post=0x600001d3 (wrote spsr_el2=0x800001d3)
```

(Each pre value matches the previous probe's wrote-value — the leak
is exact.) Pre-fault mode at every observed alignment fault was
0x1d3 = SVC, so SPSR_svc was being clobbered to a SVC-mode value
during the kernel's SVC handler. When the SVC handler eventually ran
its `movs pc, lr` epilogue (at 0x3ada6c / 0x3adb10 in `SWIBoot`),
CPSR was restored from the corrupted SPSR_svc → CPSR=SVC instead of
USR. The post-`svc #5` `mov pc, lr` at GenericSWI 0x3ae1bc then
self-looped because LR_svc = 0x3ae1bc and the instruction is the
non-mode-restoring form (no `s` suffix).

### Why FVP got past it before

FVP doesn't have Bug #1 — the AArch64 banked-SPSR helper handles
SPSR_EL2 writes correctly. So FVP's STKU iteration completed
normally and it advanced to cdsv → newt-exception. QEMU stuck at
STKU because every unaligned access during the SVC handler corrupted
SPSR_svc.

### Verification

- All 23 guest tests pass (no regression).
- Cold-boot QEMU trace: 213k entries / 1262 unique functions,
  task_dump shows `curr=0xc11b2c0` (cdsv) past the STKU state.
- Trajectory now matches FVP (both reach the "newt"
  UnhandledException as the next stall).

## Resolved (was) — wedge isolated to STKU monitor task body (QEMU, 2026-04-25 night)

### Pre-flight: restored DABT→kernel forward fast-path

The merge resolution dropped the DABT-forward fast-path from
`handle_diag` ("keep mz banked-register fixes, drop mn DABT-forward
fast-path"). On a fresh boot that drop wedges on the **first** non-
alignment DABT — the SetFreeChain APCS prologue's `STMFD sp!,
{...,fp,ip,lr,pc}` crossing into an unmapped page below `SP_usr=0x0cc7a010`
(FAR=0x0cc79ff4, DFSC=0x07, page-translation fault). Newton's own
`DataAbortHandler` at `0x0039_3114` is the legitimate handler for that
class of fault; the hypervisor's DIAG halt was a Phase-B trip-wire,
not the right behaviour for routine on-demand paging.

Restored the fast-path in `trap.rs::handle_diag`:

- Source-mode gate: only forward when HVC source mode is `MODE_ABT`.
  guest_bp UND-source hits and PABT-vector hits still take the loud
  halt.
- DFSC gate: `0x03 | 0x05 | 0x06 | 0x07 | 0x0D | 0x0F` (translation /
  permission / access-flag for both section + page).
- R0/R1 restored from TPIDR_EL0 / TPIDRRO_EL0 (the DABT trampoline
  stashed them there before clobbering with DFSR / SPSR_abt).
- `ELR_EL2 = 0x0039_3114`, then ERET; SPSR_EL2 stays as captured (mode
  ABT). LR_abt / SP_abt / SPSR_abt remain hardware-populated.
- Budgeted `dabt:` log dedups by (FAR, mode), 16 unique-pair cap.

After restore: a 90-s cold boot logs **one** DABT forward
(`DFSC=0x7 FAR=0x0cc79ff4 mode=0x17` — the SetFreeChain stack-extension)
and otherwise progresses through the same trajectory as the prior
investigation: trace ~156k entries, last unique user-mode call
`PSoundDriver::SoundOutputIH` (sound IRQ injection probe), wedged at
`PC=0x3ae1bc CPSR=SVC SP_svc=0x0c000400 LR_svc=0x3ae1bc`.

### New observation: LR_svc readback now reliable, and reads PC

The previous heartbeat used `MRS x, sp_el1` / `MRS x, elr_el1` and
returned `0` from EL2 IRQ context — flagged in `docs/QEMU_BUGS.md`.
The banked-reg overhaul replaced those with `ctx.x[19]` / `ctx.x[18]`
per ARM ARM Table D1-79, which gives architecturally-defined values
on both QEMU and FVP.

The reliable readback shows:

```
timer_irq[late]: ELR=0x3ae1bc SPSR=0x60000113 SP_svc=0x0c000400
                 LR_svc=0x3ae1bc FAR_EL1=0x0c116e66
                 intid=0 VI=0 ipres=0x40 ictrl=0xc401420 pend=false
```

`LR_svc == ELR == 0x3ae1bc`. That's the address of `mov pc, lr` (note:
no `s`, not `movs`) at the end of `GenericSWI`:

```
003ae174 <GenericSWI>:
  ...
  3ae1b8: ef000005   svc #5
  3ae1bc: e1a0f00e   mov pc, lr
```

Architecturally, when `svc #5` at `0x3ae1b8` fires, hardware sets
`LR_svc = 0x3ae1bc` and switches to SVC mode. Normal exit through
`GenericSWIHandler` does an `LDM SP!, {..., PC}^` that restores CPSR
from `SPSR_svc` (= the saved USR CPSR) and PC from the saved LR. After
that, `mov pc, lr` at `0x3ae1bc` runs **in USR mode** and falls back
to the user-mode caller via `LR_usr`.

The wedge state — `PC=0x3ae1bc, mode=SVC, LR_svc=0x3ae1bc,
SP_svc=0x0c000400` — is the smoking gun for one of:

1. The SWI epilogue used `LDM SP!, {..., PC}` (no `^`) or `MOV PC, LR`
   (no `s`), so CPSR is not restored and we stay in SVC. `mov pc, lr`
   at `0x3ae1bc` then jumps to `LR_svc=0x3ae1bc` — **infinite loop in
   SVC mode**.
2. A re-entrant `svc` somewhere in the SVC handler clobbered `LR_svc`
   to `0x3ae1bc`, and the outer return drops us at `0x3ae1bc` in SVC
   mode where `mov pc, lr` self-loops.

Either way the kernel is sitting in a tight `mov pc, lr` self-jump
in SVC mode, with sound DMA IRQs preempting the loop on each
heartbeat (no progress is made).

`SP_svc = 0x0c000400` is the BootOS-set initial SVC stack base —
matches "SVC stack frame fully unwound", so the handler did get to its
final pop before the issue.

### Why FVP got past it before, why QEMU doesn't

Per the prior FVP cross-check (180 s wall, run `mn` bad09ce3): on FVP
the STKU dump appears once (during the page-copy SWI) and then
`gCurrentTask` advances to `cdsv` (CardServer). On QEMU the wedge is
permanent. The recent banked-reg work did not change that — confirming
the wedge is a QEMU TCG behaviour at the AArch32 SVC return path,
specifically around how `LDM ... {pc}^` restores SPSR_svc to CPSR
when control re-enters AArch32 from EL2 IRQ-trap context.

`docs/QEMU_BUGS.md` Bug #1 (SPSR_svc clobber via `msr spsr_el2, x`)
is *not* the cause here: HVC and DABT round-trips are documented to
use the auto-saved SPSR_EL2 unchanged, and the SVC handler's
`LDM ... {pc}^` reads `banked_spsr[1]` directly. But the same QEMU
sub-system (banked SPSR plumbing across the AArch32↔AArch64 boundary)
is what's faulty.

### Open next steps

1. **Tarmac trace on FVP across one STKU iteration** (the prior plan
   from `mn` bad09ce3 — still pending). Capture the exact instruction
   sequence STKU executes after PhysSubPageCopy returns, so we know
   what the "correct" path looks like and can compare against QEMU.
   Specifically: does FVP also see `LR_svc = 0x3ae1bc` momentarily
   and recover, or does the kernel's SVC return path go somewhere
   different on FVP?
2. **Inspect `GenericSWIHandler` (0x000d8a64) tail** in ghidra MCP to
   find the SWI return idiom. If it uses `LDM SP!, {..., PC}^` and
   the wedge is QEMU's `^` plumbing dropping SPSR_svc on the floor,
   we have the bug isolated.
3. **Test on QEMU**: replace the SVC-handler return idiom in ROM
   patches with a hypervisor-mediated path (HVC → EL2 → re-construct
   correct CPSR + ELR → ERET). If that fixes the QEMU wedge, the
   bug is QEMU's `LDM {pc}^` semantics in TCG.

## Resolved (was) — wedge isolated to STKU monitor task body (QEMU, 2026-04-25 night)

Added `src/task_dump.rs`: walks the scheduler at `*0x0c100fd0`,
gCurrentTask at `*0x0c101000`, the per-priority TTaskQueues at
`gScheduler+0x1c+prio*8`, and decodes the task fourcc name from
STaskSwitchedGlobals.fTaskName (heuristic search a few words below
each task's `globals` pointer at `task+0xa0`).

At the wedge state, the dump consistently reports:

```
task_dump: gSched=0xc1084b4 curr=0xc113dd8 highest_pri=10 bitmap=0x400
           last_rem=0x0 want=0 hold=0 curr_glob=0xc11446c
  current:
  task 0x0c113dd8 prio=20 name=STKU globals=0x0c11446c q=0/0 stk_bot=0x0c114030
  prio 10 queue@0xc108520:
  task 0x0c119c74 prio=10 name=name globals=0x0c320a58 ...   (NameServer task)
  task 0x0c1180a8 prio=10 name=drvl globals=0x0cc82790 ...   (driver loader)
```

Key facts the dump establishes:

1. **Scheduler state is healthy**: `gWantSchedule=0`, `gHoldSchedule=0`,
   highest occupied priority = 10, bitmap=0x400 (only bucket 10 set).
   `gCurrentTask` = STKU at priority 20 with `q.next=0 q.prev=0` —
   correctly removed from the run queue while running.
2. **Newton priority convention** (verified from `TScheduler::Add`'s
   `cmp r0, r4 / movcc r0, r4` against `highest_pri`): higher number =
   higher priority. So STKU (prio 20) > drvl/name (prio 10) — the
   scheduler is right not to preempt with the lower-priority ready
   tasks.
3. **Only TWO ready tasks in the system** (drvl, name). Sound server,
   pkg, the TStackManager user etc. are all blocked on
   semaphores/ports — they don't appear in any per-priority run
   queue. (Walking blocked-task lists from semaphore wait queues
   would need the gObjectTable scan; not yet wired.)
4. **STKU's wq1/wq2 links are 0** at task+0xbc/0xc8 — STKU isn't
   waiting on a semaphore-queue or port-queue we know to look at.

So the wedge is **inside the STKU monitor task's execution body**,
not a scheduler/dispatcher bug. From the snapshot at PC=0x3ae1bc
LR_svc=0x1f7cc4 the call frame is `TStackManager::ResolveFault →
CopyPageAfterCollisionSWI → GenericSWI tail` — i.e., the SWI
returned (heartbeat fires *post*-svc-ret). The next instructions to
execute would be `add sp, sp, #40` then `b 0x1f7ab0`
(`Release(semaphore); ldrb [r4,#192]; …`) which loops back into
ResolveFault. None of that shows in the function tracer — the
loop body is either doing all of it inside the same already-traced
functions OR genuinely not executing.

QEMU SP_EL1 / ELR_EL1 readback from EL2 IRQ context returns 0
(documented QEMU bug — see `docs/QEMU_BUGS.md`). So the dump's
"SP_EL1=0 ELR_EL1=0" line is not informative on QEMU; FVP is the
only way to verify SP_svc/LR_svc directly.

Open next steps:

1. **Run on FVP** to (a) confirm the wedge reproduces, (b) read
   SP_EL1/ELR_EL1 reliably, (c) get a bounded tarmac trace across a
   single iteration of the supposed STKU loop body.
2. **Identify what makes STKU return to its idle/Receive loop** in
   Einstein. The smoking gun is below: in Einstein STKU is BLK
   (blocked), our hypervisor it's RUN forever. Find the SVC return
   path or unscheduling that we're missing.

### Einstein-vs-hypervisor task census (Phase B oracle, 2026-04-25)

`baremetal/probe/probe.cpp::task_dump` dumps the same scheduler
state on the Einstein side (every 2s). Diffing at matching boot
phases:

| field            | hypervisor (wedge) | Einstein (t=12s) |
|------------------|--------------------|------------------|
| total tasks      | 16                 | 29               |
| total kernel obj | 119                | 404              |
| gCurrentTask     | STKU id=0x12e3     | fser id=0x4793   |
| highest_pri      | 10                 | 12               |
| ready tasks      | 1 (drvl)           | 4 (Tmux, cdsv, scpl, codc) |
| STKU state       | **RUN** (stuck)    | **BLK** (idle waiting for next msg) |
| OBJM/PMGR/PTBL/STKF/STKP/STKU/ROMF/ROMP | all BLK (q=0/0 wq=0/0) | all BLK (same pattern) |

So **the wedge is STKU failing to return to its idle blocked state
after the CopyPageAfterCollisionSWI completes**. Einstein's STKU
finishes the same SVC, returns to its TUMonitor main loop, calls
some Receive() that blocks, and `fser` / `Tmux` / etc. take over.
Our hypervisor's STKU never reaches that block — it's stuck at
PC=0x3ae1bc in SVC mode, the post-svc-#5 `mov pc, lr` of GenericSWI.

**The empty-link `q=0/0 wq1=0/0 wq2=0/0` pattern IS the normal
blocked state in Newton**: blocked tasks have empty task-side
links and live only on the blocking object's (port/sem/etc.)
waiter queue. So our 14 BLK tasks are correctly blocked — STKU is
the one anomaly.

Tasks Einstein has that we don't (at this boot phase): Tmux, cdsv,
codc, cdfm, cdpr, pg&e, newt, pssm, scrn, inkr, cmgr, scpl, fser.
These are post-monitor-init tasks (GUI / ink / file server / power
mgmt) — boot can't reach them while STKU holds whatever resource
they're transitively waiting on.

### Investigation plan from here

The SVC handler ran ~110 traced functions inside CopyPagesAfter-
StackCollided and then stopped emitting traces after `_ExitFIQAtomic`
at trace 154686. The handler's return-to-user path normally:
1. Restores user-mode CPSR (USER) from SPSR_svc.
2. ERETs back to PC after the `svc 0x05` (= 0x3ae1bc).
3. Executes `mov pc, lr` → resumes user-mode caller at LR_usr.
4. Caller (TStackManager::ResolveFault @0x1f7cc4) cleans stack +
   loops back to Release semaphore + check for more work.
5. Eventually returns to TUMonitor::Main which calls Receive() to
   block until next request.

We're observing CPSR=SVC at 0x3ae1bc with LR_svc apparently
(via QEMU snapshot) = 0x1f7cc4. But the task-dump comparison says
this should ultimately end in STKU being BLK. So somewhere between
trace 154686 (last svc trace) and the would-be Receive() block,
control is lost.

Likely culprits to check next on FVP:
- `ldmdb fp, {…, pc}`-style multi-register restore in SVC handler
  exits — if the saved registers on the kernel stack are corrupted
  (bad page-copy interaction?) the wrong PC is restored.
- A `subs pc, lr, #4` from IRQ context that maps SPSR back to SVC
  mode by accident (we set HCR_EL2.IMO so EL2 takes IRQs — does
  the AArch32→AArch64 SPSR plumbing on QEMU corrupt the SPSR?).
- Re-entrant `svc` from SVC mode somewhere in the SVC handler
  itself, clobbering LR_svc — would make `mov pc, lr` self-loop.

FVP tarmac trace across the suspected wedge window would tell us
which.

### FVP cross-check (180s wall, 2026-04-25)

```
scripts/fvp --timeout=180 \
    target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

Periodic task_dump output:

```
task_dump: gSched=0xc1084b4 curr=0xc108624  highest_pri=0  bitmap=0x0     # OBJM, idle setup
task_dump: gSched=0xc1084b4 curr=0xc113dd8  highest_pri=10 bitmap=0x400   # STKU, same as QEMU wedge state
task_dump: gSched=0xc1084b4 curr=0xc11b2c0  highest_pri=0  bitmap=0x0     # cdsv (CardServer)
task_dump: gSched=0xc1084b4 curr=0xc11b2c0  highest_pri=0  bitmap=0x0
task_dump: gSched=0xc1084b4 curr=0xc11b2c0  highest_pri=0  bitmap=0x0
```

**FVP gets past STKU.** The STKU dump appears once (transient,
probably during page-collision handling) and then the scheduler
moves on to `cdsv` (CardServer). On QEMU, STKU stays as
gCurrentTask forever. So **the STKU wedge is QEMU-specific.**

After ~180s on FVP, boot crashes with a different failure: the
"newt" (`0x6e657774`) exception in `UnhandledException` —
matches the Apr 24 finding (kernel-mode DABT with FAR ASCII =
"newt") that was previously seen on QEMU. So FVP doesn't deadlock
on STKU but DOES hit a separate kernel-state corruption later.

**Most likely culprit for the QEMU-specific STKU wedge** (per
`docs/QEMU_BUGS.md` "AArch64↔AArch32 boundary"): the EL2 trap entry
or ERET path mishandles the SVC-mode banked LR (`R14_svc`). When an
IRQ is taken from SVC-mode AArch32 to AArch64 EL2, ELR_EL2 holds
the trap PC (= 0x3ae1bc) and SPSR_EL2 holds the AArch32 CPSR
(= 0x60000113 SVC). On ERET back, `R14_svc` should be unchanged
from before the trap. If QEMU is corrupting it (or our trap stub
inadvertently writes through `LR_svc`), `mov pc, lr` at 0x3ae1bc
would jump to a wrong PC.

Two falsifiable next steps to confirm:

1. **Save+restore SP_EL1 / ELR_EL1 explicitly** in the EL2 IRQ
   trap stub to bypass any QEMU bug. If the QEMU wedge clears,
   the bug is in QEMU's banked-reg plumbing.
2. **Tarmac trace on FVP across one STKU iteration** — capture the
   exact instruction sequence STKU executes after SVC return so
   we know what the "correct" path looks like and can compare
   against QEMU's stuck state.

Lower priority once the wedge is QEMU-side: the FVP "newt"
exception (kernel state corruption around `gCurrentGlobals`-relative
addressing) — that was previously chased on QEMU and presumably
masked when the STKU wedge started covering it. We'll see it again
when the wedge is fixed.

## Resolved (was) — sound subfn map known; wedge in StackManager page-copy persists (QEMU, 2026-04-25 late)

Captured the actual native-primitive subfn sequence the Newton kernel
exercises during sound init by adding "first-occurrence" logging in
`peripherals/sound.rs::handle`:

```
sound: first subfn 0x1f @PC=0x8013f8 r1=0x400 r2=0x1000 r3=0xc401420
sound: first subfn 0x5  @PC=0x8011f0 r1=0xcc84140 r2=0xea0 r3=0xcc85030
sound: first subfn 0x6  @PC=0x801204 r1=0xcc86030 r2=0xea0 r3=0xcc87030
sound: first subfn 0xa  @PC=0x801254 (PowerOutputOff)
sound: first subfn 0xc  @PC=0x80127c (PowerInputOff)
sound: first subfn 0x1e @PC=0x8013e4 (InputIntHandler  — only after our injection fires INT_DMA3)
sound: first subfn 0x1d @PC=0x8013d0 (OutputIntHandler — only after our injection fires INT_DMA5)
```

So the kernel's sound init goes:
1. `NativeSetInterruptMask(input=INT_DMA3=0x400, output=INT_DMA5=0x1000)`
2. `SetOutputBuffers(0xcc84140, 0xea0, 0xcc85030, 0xea0)` — two 0xea0-byte
   output buffers in RAM.
3. `SetInputBuffers(0xcc86030, 0xea0, 0xcc87030, 0xea0)` — likewise input.
4. `PowerOutputOff` / `PowerInputOff`.
5. End of sound init — kernel proceeds, never calls subfn 0x07
   (ScheduleOutputBuffer), 0x09 (PowerOutputOn), or 0x0d (StartOutput).
   So the sound subsystem is configured but parked.

`GetSoundHardwareInfo` (subfn 0x04) is NOT called during the early-boot
path — our previous suspicion that the kernel needed the 7-word info
struct written is false. We still implement Einstein's behaviour
(write the struct + return 0) so future paths that exercise it
behave the same as Einstein, but it's not load-bearing for this stall.

The subfn 0x1d / 0x1e firings only happen after our wedge probe
injects INT_DMA3 + INT_DMA5; the kernel's IRQ path runs the IH chain
and SendForInterrupt queues a deferred message. **That alone doesn't
unblock the boot**: heartbeat continues to show PC=0x3ae1bc (= post-
SVC#5 mov pc,lr in GenericSWI) with int_present=0x40 (TIMER_3 latched
but unused) and irq_pend=false.

The actual wedge: TStackManager monitor task (id 0x0c113dd8) is
processing the sound task's `LockStack` collision through
`FMLockHeapRange / ResolveFault / CopyPageAfterCollisionSWI`. Two
collision iterations get traced (155559, 155720) — the loop is real
and per-iteration work is ~270 trace lines — but no further unique
functions appear past trace ~156725. SwapInGlobals shows
~10 distinct tasks rotating through the scheduler (so it's not a
classic deadlock), but the user/svc paths only re-enter
already-traced code.

Heartbeat reads of `SP_EL1=0 ELR_EL1=0` (= AArch32 R13_svc/R14_svc
when at EL1 AArch32 SVC) from EL2 are likely unreliable on QEMU
raspi3b — see `docs/QEMU_BUGS.md`. The existing `handle_diag_lr` path
uses a guest-side stub to read banked regs into RAM precisely because
LLVM's AArch64 `MRS sp_svc` / `MRS lr_svc` plumbing on QEMU is
documented-flaky. Snapshot saves at sync-trap time read non-zero
values via the same sysregs, suggesting the readback is only
unreliable from EL2 IRQ-trap context.

Pending-work hypothesis: the StackManager monitor task is **looping
correctly** but each iteration enters only previously-traced code, so
the function tracer's "first-occurrence" view shows no progress. The
real boot may eventually complete the loop. Worth running for 5+ min
or moving the trace from "first occurrence" to "every-Nth call" to
confirm forward progress vs. true wedge.

Open next steps:
1. Use ghidra MCP to read the kernel-mode REx-side `0x1b16b6c b
   0x1f7540` chain's caller frame (`FMLockHeapRange`) and identify
   what loop bound it's iterating to — see whether the boot is
   waiting for many pages to copy or just a few.
2. Verify SP_svc/LR_svc on FVP at the wedge — if they read sane
   values there, the QEMU readback was the misdiagnosis source.
3. Switch the function tracer to "log every Nth call" or wire a
   per-call counter so we can see whether already-traced functions
   are being re-entered (real progress) or genuinely stuck.

## Resolved (was) — boot wedges inside StackManager monitor's page-copy SWI; sound IRQ injection partially unblocks (QEMU, 2026-04-25 evening)

The "kernel waiting for sound DMA IRQ" hypothesis below was tested and is
**partially correct but not the primary blocker**:

1. Added a wedge probe to `trap_irq` that injects `INT_DMA_CH3 |
   INT_DMA_CH5` (0x1400) into `vic::int_present` after the heartbeat
   detects 64+ consecutive samples at the same guest PC and the kernel
   has armed the sound IRQ enables in `int_ctrl` (mask & 0x1400 ==
   0x1400). Implementation: `peripherals/vic.rs::inject_sound_dma_irq`,
   wired into `trap.rs::trap_irq`.

2. **The injection works**: with it enabled the kernel's IRQ path runs
   `IRQHandler → DispatchIRQInterrupt → PSoundDriver::InputIntHandler →
   TSoundServer::SoundInputIH → SendForInterrupt`, then the same chain
   for OutputIntHandler / SoundOutputIH. So the kernel **does** want
   sound DMA IRQs after registering them in `int_ctrl=0xc401420`.

3. **But the SoundIH runs in IRQ context only** — it doesn't unblock
   the StackManager monitor task that's wedged in SVC mode mid-page-
   copy. After the IRQ returns, control resumes at the same idle PC=
   0x3ae1bc and no new user/svc-mode functions are entered.

4. The actual wedge: `TStackManager::FMLockHeapRange` / `BuildPerms` /
   `AddPgPAndPerm` for the sound task's stack pages stops making
   progress around trace 155832 (last `_ExitFIQAtomic`). Sync trap
   counter keeps growing (cache-flush MCRs, shadow-stub UDFs) but
   `awk '/^trace / && !seen[$4]++'` shows no new function entries past
   `PhysSubPageCopy` regardless of how long we run (180+ s).

5. Open question: where in the REx-side `0x1b16b6c
   CopyPagesAfterStackCollided` (or its callees) are we stuck? The
   user-mode wrapper is just `ldr r0, [r0]; b 0x1b16b6c`, so the
   actual loop lives in REx code that the rom.dis tooling doesn't
   cover. Probably needs ghidra MCP to inspect.

6. Heartbeat reads SP_EL1=0 LR_EL1=0 (which should alias R13_svc /
   R14_svc) at the wedged state, but the snapshot save reads non-zero
   values via the same sysregs at sync-trap time (per
   `INVESTIGATION.md` history: r13(SP_svc)=0x0c1142bc r14(LR_svc)=
   0x001f7cc4 sampled from snapshot). Either QEMU's
   AArch32↔AArch64 banked-register plumbing is unreliable for IRQ-
   trap context (see `docs/QEMU_BUGS.md`), or the kernel does
   genuinely have SP_svc=LR_svc=0 in some idle path. Worth verifying
   on FVP before assuming the kernel is at fault.

The sound DMA IRQ injection is left in place as a probe; it doesn't fix
the wedge but does extend coverage by exercising the sound IH path.

Next steps:
1. Use ghidra MCP to inspect the REx-side `0x1b16b6c
   CopyPagesAfterStackCollided` to identify the loop termination
   condition and what state the kernel is checking that doesn't
   advance.
2. Cross-check by running the same boot point on FVP — if SP_svc /
   LR_svc read coherently there, the QEMU readback is the
   misdiagnosis source; if they're also 0, the kernel really is
   parked there with SP_svc=LR_svc=0 (interesting).
3. Independent path: check whether the wedged kernel-mode task is
   waiting on a specific kernel-internal semaphore or condition
   variable that `inject_sound_dma_irq` can't unstick.

## Resolved (was) — kernel idle waiting for non-timer IRQ after stack-collision SWI (QEMU 16×+ratchet+ROM-patch, 2026-04-25)

After both the ratchet fix (hypervisor-side) and the
addls→addcc ROM patch (kernel-side) below, the timer/alarm
subsystem is fully working. In a 180-s run:

- `TTimerEngine::Alarm` fires 45× (was 1× before)
- `RestartTimerOverflowDetect` fires 45× (was 0× before — never)
- `UpdateClock` fires 46× (was 1×)
- `TTimerEngine::QueueTimer` runs 46× (was 1×)

The kernel's gClock now properly tracks tick wraps; alarm.high
matches gClock.high in snapshots; alarms queued at `gClock + delay`
fire at the right moment.

Boot reaches the same stack-collision page-copy SWI as the
shorter runs (TSoundServer::TheMain → LockStack →
CopyPageAfterCollisionSWI → CopyPagesAfterStackCollided →
PhysSubPageCopy → CleanPageInDcache → PurgePageFromTLB →
_ExitFIQAtomic, last traced call ~155350). After that, no new
unique functions appear for the remaining ~25 seconds of the run.

Heartbeats show steady-state:
- PC=0x3ae1bc CPSR=0x60000113 (SVC mode, IRQs enabled, Z=1)
- int_present=0x0 (no timer match latched at sample time)
- int_ctrl=0xc401420 (TIMER_2 + DMA3/DMA5 + power-off enabled,
  TIMER_3 / GPIO disabled)
- VI=0, irq_pend=false

The alarm engine cycles through `RestartTimerOverflowDetect`
once per ~3.7s (delay = 0x0d2f0000 ticks at 59 MHz scaled), but
no other code progresses between alarm IRQs.

Hypothesis: the kernel set DMA channel 3 (Sound input,
0x400) and DMA channel 5 (Sound output / Tablet rcv, 0x1000)
IRQ enables in `int_ctrl` during sound subsystem init, then
called a `WaitOn` that depends on sound DMA completion to
deliver an IRQ. We don't model sound DMA, so that IRQ never
fires, the kernel sits idle through alarm cycles.

Next steps:
1. Confirm by inspecting the saved task struct at the heartbeat
   PC: which task is running, what semaphore it's blocked on.
2. Either implement minimal sound DMA stubs (return
   "transfer complete" immediately) or short-circuit the
   sound subsystem entirely if it's optional for early boot.
3. Cross-check Einstein's TDMAManager / sound-driver path —
   what does it return for these channels?

## Resolved — alarm-loop wedge from spurious wrap detection (QEMU, 2026-04-25)

**Two complementary fixes** ended up needed:

1. Hypervisor-side: `peripherals/vic.rs::ticks()` now ratchets
   via `LAST_TICKS` so consecutive in-hypervisor calls return
   strictly increasing values.

2. ROM patch in `rom_patches.rs`: replace `addls` with `addcc`
   (`ls`→`cc` swap on cond field) at the three wrap-detect
   sites in the kernel — `GetClock` 0x3ad430, and
   `SetAlarm` 0x3ad46c / 0x3ad49c. The kernel reads the live
   tick register via the non-trapping `stage2::TICK_PAGE`
   mapping, which only refreshes on hypervisor heartbeat
   (~16 ms) — so the hypervisor-side ratchet doesn't help when
   the kernel reads the same page twice in quick succession.
   The ROM patch makes wrap-detect strictly less-than instead
   of less-or-equal, so equal successive reads don't fire a
   false wrap.

Without the ROM patch alone, the alarm engine still wedges
because `addls` treats `current_ticks == gClock.low` as a wrap
(see "Verified by reading guest RAM" below). Without the
ratchet, hypervisor-side `ticks()` calls (e.g. for the tick
page itself) can return equal values across two close calls,
which is harmless after the ROM patch but still violates the
"strictly monotonic" contract that other code might rely on.

QEMU boot was getting stuck in a `TTimerEngine::Alarm` →
`SetAlarm` → `SetAlarm1` → `DisableAlarm1` tight loop right after
`UserBoot`, never advancing past trace 27313. Symptom: same alarm
time (low word) being re-armed forever, with current ticks already
past it.

Root cause: the Newton kernel's `GetClock` (0x003ad41c) reads
gClock from RAM, then reads the live tick register, then bumps
the output's `high` word if `current_ticks <= gClock.low` — that
is, equality counts as "wrapped". Designed for an environment where
two consecutive ticks reads are guaranteed to differ.

In QEMU TCG, `CNTPCT_EL0` advances slowly relative to instruction
count, so two `ticks()` calls in quick succession (e.g., the
`UpdateClock` call in `StartTimerOverflowDetect` followed
immediately by `QueueTimer`'s `GetClock`) can return the same
value. That trips the equal-counts-as-wrapped path and bumps the
local TTime.high to 1, even though no wrap occurred. The freshly
queued alarm gets `alarm.high = 1` while the global `gClock.high`
in RAM stays at 0, so `CompCompare(now, alarm)` permanently
returns -1 and the alarm engine wedges.

Verified by reading guest RAM out of a snapshot: gClock at IPA
0x04008_56c (VA 0x0c10156c via stage-1 walk through L1[0xc1] →
L2[1] = 0x0400803e) was `(0, 0x1A52512C)` — exactly the ticks
value at the boot's first-and-only `UpdateClock` call. The alarm
queue head at IPA 0x040085a0 had `(1, 0x2781512C)` = gClock +
0x0d2f0000, but with the +1 in the high word — the smoking gun.

Fix in `peripherals/vic.rs::ticks()`: ratchet via static
`LAST_TICKS` so consecutive calls always return strictly
increasing values. If the raw computation lands at-or-below the
previous reading, return `last + 1` instead. Real wraps still
work because the raw value drops by ~2^32 and the ratchet steps
naturally past 0xFFFFFFFF on subsequent calls.

After fix: boot advances from trace 27313 to trace 156638, past
`UserBoot` / `InitDomainsAndEnvironments` / `BuildDomainsAndHeaps`
/ `MakeSystemStackManager` / `TPageManager::Register` / sound
hardware probe, into the page-copy SWI for stack-collision
handling.

## Resolved — kernel page-mapping loop, PC=0x3ae1bc (FVP/QEMU, 2026-04-24)

The BLTG-reboot from `BuildDomainsAndHeaps` is **resolved**. Root cause
was in `shadow_stub::analyze_insn`: a *conditional* APCS return (e.g.
`LDMDBNE fp, {…, pc}`, `MOVNE pc, lr`, `BXNE lr`, `LDRNE pc, [sp], #4`)
was reported as `BranchKind::Return` regardless of the cond, so the
liveness walker stopped there and never visited the fall-through. Newton
ROM @ `MakeObject` (0x2595c8) is the canonical site:

```
2595c8: ldrb r0, [r0, #4]      ; ← byte access patched by shadow_stub
2595cc: teq  r0, #0
2595d0: movne r0, #200
2595d4: subne r0, r0, #10240   ; conditional return setup
2595d8: ldmdbne fp, {r4..r10, fp, sp, pc}   ; *conditional* return!
2595dc: str  r1, [r4, #8]      ; reads r1 — only reached on fall-through
2595e0: str  r3, [r4]          ; reads r3 — only reached on fall-through
2595e4: …
2595fc: bl   MonitorDispatchSWI
```

Walker at 0x2595cc stopped at the conditional `ldmdbne` thinking it was
an unconditional return → reads of r1 and r3 at 2595dc/2595e0 were
missed → `pick_scratch_regs` saw r1 and r12 as dead → inline stub
clobbered r1 with CPSR. Downstream `str r1, [r4, #8]` then put garbage
into the `ObjectMessage` op-code field; `ObjectAlloc`'s op-dispatch took
the default arm and returned -10006; `Init__9TUMonitorFPFPv…` propagated
that out of `Init__13TStackManagerFv`; `MakeSystemStackManager` ran the
TStackManager destructor and left `*(0x0c104c08+4) = NULL`; later
`BuildDomainsAndHeaps → NewHeapDomain` dereferenced the null pointer to
read the monitor id, dispatched on monitor 0, and the cumulative error
walks took the BLTG-reboot escape hatch.

Fix: new `BranchKind::CondReturn` variant emitted whenever a return
instruction has a non-AL condition. The walker merges
`APCS_RETURN_LIVE` (taken path) with the recursive walk of PC+4
(fall-through). `nzcv_dead_recursive` does the analogous merge.
Regression: `liveness_cond_return_walks_fallthrough` documents the
0x2595c8 motif.

After the fix, the trace counter advances from ~26900 entries (before
the BLTG-reboot) to ~156700+. Boot reaches deep page-mapping code
(`AddPgPAndPerm`, `LoadFromPhysAddress`, `CleanPageInDcache`,
`PurgePageFromTLB`) and then converges on a steady-state heartbeat at
guest PC=0x3ae1bc CPSR=0x60000113 (SVC mode) — that's `mov pc, lr` at
the tail of `GenericSWI` (just after `svc #5`). The kernel is
repeatedly issuing GenericSWI #5; whether this is a legitimate
busy-loop or another stall is the next question.

Next step: dump the trace tail to identify which caller is spinning on
GenericSWI #5 and what the SWI does. Likely candidates: scheduler
idle loop, timer wait, or a paging operation that never finishes.

### Snapshot inspection at the stuck point (2026-04-24)

`/tmp/run.firsts` (awk first-trace-per-function over `/tmp/run`) shows
the boot reaches `TSoundServer::TheMain` (lr=0x000cb2a8 — vtable +0x34
dispatch from the world runner), then `LockStack` triggers
`CopyPageAfterCollisionSWI` → `CopyPagesAfterStackCollided` →
`TStackManager::CopyPageState` → `CopyPhysicalPage` → `PhysSubPageCopy`.
The kernel completes the stack-collision page-copy work (last traced
function: `_ExitFIQAtomic` at trace #148096) and then issues no more
traced calls for the remaining 16+ s of the run.

Snapshot 3 (the last save before the run was killed) has guest GPRs:

```
PC=0x3ae1bc  CPSR=0x60000113  (mode=SVC, I=0 IRQs ENABLED, Z=1, C=1)
r0=0x00000000   r1=0x00000005
r2=0x0c114250   r3=0x0c113f88   r4=0x0c112cb8   r5=0x0c115fa4
r6=0x0c116e44   r7=0x00000008   r8=0x00000001   r9=0x0c1181b0
r10=0x00000000  r11=0x0c114334  r12=0x00000010
r13(SP_svc)=0x0c1142bc
r14(LR_svc)=0x001f7cc4
spsr_svc=0x60000113   spsr_und=0x20000110   spsr_abt=0x110
sctlr_el1=0x11b7      ttbr0_el1=0x4000048
```

`r14=0x1f7cc4` is the post-`bl CopyPageAfterCollisionSWI` PC inside
`TStackManager::FindCollidedPage`. So `mov pc, lr` at 0x3ae1bc would
branch back into the kernel-mode body of that function (which is
exactly what we'd see if the SVC handler exited correctly — the trace
shows ~150 ROM functions called inside this SWI before the heartbeat
takes over).

Critical CPSR bit: **I=0**, IRQs enabled. So a pending vIRQ would fire
immediately on ERET. The fact that snapshots 24..31 (~16 s of wall time)
all capture *exactly* this state — same PC, same regs — means the
guest is making zero forward progress. Either:

1. A pending vIRQ keeps firing each cycle, the guest's IRQ handler
   doesn't clear `vic::int_present`, and we re-trap immediately on
   ERET. (HCR_EL2.VI sampled at heartbeat would tell us.)
2. EL2 is stuck in an IRQ storm of its own — but heartbeat fires at
   the expected ~1/64 cadence, so this isn't an EL2 storm.
3. Something at PC=0x3ae1bc takes a sync trap (DABT? PABT? alignment?)
   on the `mov pc, lr` itself. But this insn is 0xe1a0f00e — no memory
   access, no shift on PC, should never fault.

Heartbeat anomaly: `intid=0` for every IRQ this run. CNTHP is wired as
PPI INTID 26 (`gicv3.rs:146`); the GIC's IAR returning 0 (= SGI 0)
means either the priority mask is rejecting the CNTHP priority, or
ICC_IGRPEN1_EL1 isn't taking effect. We then EOI intid=0, which on
GICv3 deactivates SGI 0 — leaving CNTHP-26 active forever. That
matches "physical IRQ keeps firing, EL2 never deasserts at the GIC
level." Worth a focused look at why IAR1 reads 0 on FVP.

Plan:
- Add a heartbeat-time dump of `HCR_EL2.VI`, `vic::int_present`,
  `vic::int_ctrl`, and `irq_pending()` so we can tell whether case (1)
  is happening.
- Investigate the GICv3 intid=0 puzzle — verify ICC_PMR_EL1 and
  ICC_IGRPEN1_EL1 are sticky after EL3-to-EL2 handoff on FVP, and
  whether IAR is actually returning 0 or whether ack() is reading a
  stale register.

### Update — heartbeat diagnostics added; boot now reaches scheduler activation, then a kernel-mode DABT (2026-04-24)

After adding `VI / int_present / int_ctrl / irq_pending` to the
heartbeat log (`trap.rs::trap_irq`), a fresh cold-boot run reaches
**trace 230925** (vs. the previous 148096), so the previous PC=0x3ae1bc
heartbeat-only state appears to have been a transient/stale snapshot
rather than a true hang — this run flies past the stack-collision SWI
into multitasking. New territory:

- `TCardReinsertAlertDialog::Init`, `TCardPositionAlertDialog::Init`
- `TPartHandler::Init`, `TPartHandler::Register`, `TPartEventHandler`
- `TPkRegisterEvent::TPkRegisterEvent`
- `Sleep(0x8ffc)` from `TPartHandler::Register` (caller `0x18233c`)
- `InitVppManager` (Vpp = high-voltage flash supply driver)

The heartbeat now shows correct intid=26, `int_present=0x60`,
`int_ctrl=0xc400000`, `irq_pend=false`, `VI=0` — no missed-IRQ
storm. (intid=0 in the prior /tmp/run was either a stale binary or a
resume-from-snapshot artefact; still worth noting if it recurs.)

The boot exits via a recursive kernel-mode DABT:

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x6e657774 mode=0x17
trace 230833 0x00393114 DataAbortHandler (abt) ... lr=0x00259d48
... kernel calls FaultMonitorEntry, ConvertIdToObj, RebootIfFaultWasInStack, Throw
putc 230872..230923: "Unhandled exception evt.ex.abt.bus, warm reboot!"
```

Decoded:

- **Faulting PC = LR_abt - 8 = 0x259d40** = `ldr r0, [r0]` in
  `TUPort::Receive` (just before `bl PortReceiveSWI` at 0x259d44).
- **Faulting VA = 0x6e657774 = "newt" ASCII** — `r0` was loaded from
  `[fp, #4]` (caller's saved arg0 = `self`) and dereferenced. So the
  TUPort `self` pointer is "newt" string bytes.
- **mode=0x17 (ABT)** — fault taken from ABT mode → **recursive
  abort**. The kernel was already inside its DABT handler when the
  next access faulted.
- The first DABT (preceded by trace 230832 `SwapInGlobals`) corresponds
  to the scheduler picking task `0x0c118dd8` and ERETing back into its
  saved PC. That saved PC must already have been 0x259d40 with a
  corrupt FP frame, or the scheduler is loading a wrong task struct.
- 5 prior DABTs in this run all had DFSC=0x7 (translation level-2,
  legit page-in faults at `0x0cc7xxxx` heap pages); the last one is
  DFSC=0x5 (translation level-1) on a wild VA — distinctly different.

Pre-failure trace shows `InitVppManager` (Vpp = high-voltage flash
supply driver, called from `0x00054aa4` inside the platform driver
init loop) working through a normal `operator new(12)` allocation
chain (NewPtr → NewDirectBlock → NewBlock → MoveFreeBlock →
SetFreeChain) and then `TUSemaphoreGroup::GetRefCon`. The DABT comes
~1000 trace events later, after the kernel scheduler has ticked
several times.

Next investigation steps:
- Find where the TUPort::Receive task was created and what arg0
  should be. Scan back through `/tmp/run3.log` for `TUPort::Receive`
  entries to see the legitimate caller's `r0` value.
- Inspect the saved task struct at 0x0c118dd8 (offsets 0x00 / 0xa0 /
  0xd8 are what `SwapInGlobals` loads). If we can dump RAM at that
  address from a snapshot, we can verify whether the kernel's view
  of the task is corrupt or whether our hypervisor mishandled a
  save/restore at task switch.
- The "newt" byte pattern (0x6e657774) doesn't appear as a literal in
  the disassembly but does prefix several symbol names like
  `newtConnects`, `SYMnewtaboutview`, `SYMnewtinfobox` — symbol-name
  data lives in the runtime symbol table, suggesting the corrupt
  pointer came from a symbol table lookup that returned a name string
  instead of a code/data address.
- Confirm reproducibility — re-run cold boot without snapshots and
  verify the DABT site / FAR are stable.
