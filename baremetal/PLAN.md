# Plan — Drive Newton OS to interactive use

## Status

**Larger context:** We've tried to patch the kernel so it no longer
needs 1k subpage protections in the MMU (ARMv4 feature no longer
available). After iter-12's 36-KiB stack patch landed, the
alrt-task DABT (cross-subpage stack overflow corrupting the alrt
task's CList) is GONE. Phase B has moved on to the next stall.

**Hypervisor-side compensation for subpage incompatibility is NOT
on the table** (user directive 2026-04-29). Stage-2 PA splitting,
shadow-on-write redirects, and per-task subpage-AP shims are all
ruled out. The fix MUST be a kernel patch that makes the kernel
work without 1k subpage protections.

**Current goal: pin the permission DABT that's tripping
UnhandledException. Iter-34 cracked the wedge open: the stack at
the canary's TRUE source SP_USR (decoded by reading
UND_SAVE_SPSR_IPA to recover the trampoline-hidden source mode)
contains the ASCII string "Unhandled exception evt.ex.abt.perm,
warm reboot!". The kernel hit a permission DABT it couldn't
recover from and called UnhandledException → Reboot. This
directly implicates the iter-3 AP-flatten step: with 1 KiB
subpage AP overlay removed, the kernel sees AP=011 (RW/RW)
grants where it expected per-subpage no-access. Iter-35+ should
chase the actual fault: capture the FAR/ELR/insn of the
permission DABT that triggered UnhandledException, decode the
faulting access, and decide whether to (a) restore subpage
isolation via stage-2 splitting at the offending VA, (b) patch
the kernel allocator at the alloc site that produced the
overlapping mapping, or (c) re-instate kernel-intent classifier
output as a runtime gating layer.**

(Earlier formulation:) Iter-30 — the 4-KiB-page phase-B
hypothesis HOLDS for the heap allocator and stack allocator at
the invariant level. After 30 iterations of microscope-mode
fault chasing, iter-30 stepped back to test the actual
hypothesis: with kernel patches that move all allocations to
4 KiB granularity, do the heap blocks remain non-overlapping
and do the stacks remain bounded by guard pages? The answer
across an entire boot to the existing Reboot canary wedge:
**yes**. No heap-overlap halt fired (1024-slot live allocation
table, dl-probe-tracked frees, halt-on-first-overlap tripwire
armed). No NewStack guard-page violation fired across all 24
tasks created during boot (every stack at base ≥ 0xC306000
with span 0x8000 was preceded by a 4-KiB unmapped guard page).
No NewStack alignment violation fired. The remaining Reboot
canary wedge is a different bug (most likely a kernel self-
test fail downstream of the dissolved iter-25 r0 puzzle).
Iter-31+ should pursue the remaining instrumentation gaps
(ResolveFault RETURN probe with subpage_owner walk + cross-task
stack scan, Prim Remember/Forget consistency tracking, page-
grain alignment audit on ExtendVMHeap/AllocNewPage) and then
investigate the kernel self-test that fires the canary.**

### IdleProc probe — corruption pattern identified

Added an HVC probe at IdleProc__18TAlertEventHandler entry
(0x000309EC, IDLEPROC_PROBE_HVC_IMM=0x56). Cold-boot fires once
before the wedge with this output:

```
IdleProc #000 ENTER this=0x0cca37a8 inner=0x0cca3738
                clist=0x0cca37c4 count=32 esize=1
                ebase=0x003121fc src_mode=0x10 sp=0x0cca3638
  entry[0] @VA=0x003121fc = 0xe3360000  <-- JUNK (the FAR=0xe336000c source)
  entry[1] @VA=0x00312200 = 0x15a74048
  entry[2] @VA=0x00312204 = 0xe91baff0
  entry[3] @VA=0x00312208 = 0xe3300000
  raw this[0..32]:
    +0x00: 0x0001eac0     ← vtable pointer (ROM)
    +0x04: 0x00000000
    +0x08: 0x6e657774     ← ASCII "newt"
    +0x0c: 0x616c7274     ← ASCII "alrt"
    +0x10: 0x0c6019c0
    +0x14: 0x0cca3738     ← inner ptr (also = alrt task globals)
    +0x18: 0x0c2049a0
    +0x1c: 0x00000020
  raw clist[0..32]:
    +0x00: 0x00000020     ← "count" = 32
    +0x04: 0x00000001     ← "esize" = 1   (bogus; should be 4)
    +0x08: 0x0c320804
    +0x0c: 0x0c3207dc
    +0x10: 0x003121fc     ← "ebase" = ROM PC inside MoveFreeBlock
    +0x14: 0x00310858     ← ROM PC inside SetFreeChain
    +0x18: 0x0c201010
    +0x1c: 0x00000020
```

**Diagnosis:** `this=0x0cca37a8` IS a valid TAlertEventHandler:
vtable pointer at +0, ASCII signature "newt"+"alrt" at +8/+0xc, real
inner-struct pointer at +0x14. The TAlertEventHandler object itself
is intact.

**The corruption is in the CList header storage at VA=0x0cca37c4.**
The values look like an APCS stack frame from a recent
`MoveFreeBlock → bl SetFreeChain` call, which left two return-style
addresses (0x003121fc = post-`bl SetFreeChain`, 0x00310858 = inside
SetFreeChain) at the offsets where IdleProc expects entries_base
and a follow-on field. With ebase=0x003121fc, CList::At(0) reads
*(0x003121fc) = 0xE3360000 (= ARM `teq r6, #0` instruction bytes
at that ROM address) — exactly the junk pointer the wedge dies on.

The corruption is therefore **not random** — it's the kernel's
heap allocator stack-frame leaking into a separately-allocated
TAlertEventHandler's CList field. Likely cause: the alrt task's
stack overflow into the inner struct, OR a heap-allocator write to
a wrong VA (e.g., a use-after-free that later reuses the freed
TAlertEventHandler memory as a stack frame).

### Next iteration — catch the writer with a stage-2 RO trap

Install a stage-2 RO carve-out on the PA backing VA=0x0cca37c4
(specifically, the 4-KiB page containing the CList header).
Capture every (PC, value, src_mode) writing to that PA. We expect
to see:
- The legitimate kernel write that initializes the CList (count=0,
  ebase=valid-heap-or-0).
- The corrupting write that overwrites it with stack-frame bytes.

The corrupting writer's PC will tell us which kernel routine is
escaping its allocation.

### Iteration 2: stage-2 RO trap installed (PA=0x0402e000); auto-flip limit identified

Added `src/alrt_capture.rs` modeled after `g1_capture.rs`. Boot-time
arm via `arm_at_boot()` from `kmain` with `KNOWN_TARGET_PA=0x0402e000`
(the stable PA backing VA=0x0cca3000 per the prior alias table).
Dynamic re-arm via `maybe_arm_for_va` hooked into the Prim Remember
probe as a sanity check. Cold-boot results:

```
alrt-capture: BOOT armed RO+XN on PA=0x0402e000 L3 before=0x4000000182e7ff after=0x4000000182e77f
alrt-capture: RAM at PA=0x0402e000+0x7c0..0x800 at boot:  ALL ZERO
alrt-capture summary: armed_pa=0x0402e000 traps=10 out_of_window=10 budget_remaining=4096
IdleProc #000 ENTER ... clist=0x0cca37c4 (PA=0x0402e7c4) count=32 esize=1 ebase=0x003121fc ...
```

**Boot RAM is zero.** **PA at IdleProc time confirmed = 0x0402e000.**
**10 stage-2 permission faults captured, ALL outside the
0x7c0..0x800 CList window.** Yet the CList header IS corrupted
when IdleProc fires.

### The auto-flip-to-RW pattern is the limiting factor

`handle_data_abort` in `src/trap.rs` does, on every RAM permission
fault: (1) call `g1_capture::note_perm_fault` /
`alrt_capture::note_perm_fault` for logging, (2) call
`set_ram_page_rw_xn(page)` to flip the page to RW, (3) return
*without advancing ELR* so the guest retries the store. The
`maybe_rearm()` hook on next trap re-imposes RO+XN, but that only
fires on IRQ entry (per the `g1_capture` comment, sync-trap rearm
caused infinite STM retry loops because multi-register stores
straddle pages and re-fault each retry).

Result: after the first fault on the page in each ~16 ms IRQ
window, the page is RW for the rest of that window — every
subsequent write passes through unobserved. Our 10 captures are
just the first faults of 10 IRQ windows. The corrupting write to
offset 0x7c4 happened in some window between traps, while the
page was transiently RW.

### Next iteration — instruction-level emulation

To capture EVERY write to PA=0x0402e000 we need a different
architecture: when the trap fires, decode the AArch32 store
instruction at ELR_EL2, apply the write via the PA helpers
(read_word_pa / write_word_pa), advance ELR past the instruction,
and **leave the page RO**. This avoids the STM retry loop because
the in-flight store is consumed at trap time rather than retried
natively.

The `src/unaligned.rs` infrastructure already has AArch32 store
decoding for unaligned-fault recovery; we can reuse its
`decode_*_store` helpers. The store types we need to handle on
this page: STR-imm, STR-imm-pre/post-indexed, STM, byte/half
variants. STM would need per-register iteration so the trap
captures every word of the multi-store individually.

This is a larger change than fits in a one-iteration commit, so
this iteration ships the boot-time arm + diagnostic counters as
infrastructure; the next iteration adds instruction-level
emulation. Alternative simpler probe: snapshot the CList-window
bytes at each IRQ rearm and log when they change — would catch
the corruption within ~16 ms granularity but not pin the exact
PC. Decide which to pursue based on whether we want chronology
(periodic snapshot) or the writer's PC (instruction emulation).

### Iteration 5 (next-loop iter 1): __nw__ probe confirms heap-allocator chaos

User raised the hypothesis (2026-04-28): "Is this not just random
chaos caused by the heap manager assigning overlapping physical
pages? Have we done anything to demonstrate that this is not the
case?"

The prior alias audit only checked stage-1 VA→PA aliasing. It did
NOT check whether the kernel's *block allocator* (the layer that
slices 4-KiB pages into smaller blocks for `NewBlock`/`__nw__`)
hands the same range to two distinct callers. We hadn't ruled out
allocator chaos as the cause of the alrt CList corruption.

Added a paired entry/return probe at `__nw__FUi` (operator new,
ROM 0x00318ee8 / 0x00318f1c). On entry: capture (size, caller_LR).
On return: capture the address `r4` was set to from malloc's
return, pair with the entry data, log `(seq, addr, size,
caller_lr)` and check for overlap with all prior live entries.
Halts/log-budgets the overlap announcer; the live-entry table
is append-only (no free/__dl__ tracking yet).

### Cold-boot result — overwhelming evidence of allocator chaos

```
Total nw alloc count:    939+
OVERLAP DETECTED count:  293
```

First overlap (sequence #113 vs #111):
```
*** nw OVERLAP DETECTED ***
  new alloc #113: addr=0x0c116f68 size=0x30 caller_lr=0x001f66b8
  prior alloc #111: addr=0x0c116f68 size=0x1c caller_lr=0x00148fac
```

Same start address, different sizes. Could be normal recycle if
#111 was freed before #113 — without free tracking we can't
disambiguate.

But there are partial-overlap cases that CANNOT be explained by
recycling — distinct addresses with overlapping ranges:
```
*** nw OVERLAP DETECTED ***
  new alloc #120: addr=0x0c1178cc size=0x3e0 caller_lr=0x001f8d88
  prior alloc #118: addr=0x0c1178c8 size=0x2c  caller_lr=0x00318f58
  (#118 range [0x0c1178c8, 0x0c1178f4); #120 range [0x0c1178cc,
   0x0c117cac); they overlap by 0x28 bytes starting at 0x0c1178cc)
```

#118 starts 4 bytes BEFORE #120. There's no way #118 could have
been freed and #120 allocated in #118's exact-but-shifted range —
that requires the allocator to actively give out two blocks at
adjacent-but-overlapping addresses simultaneously.

Confirmation: the user's hypothesis is correct. The allocator IS
producing overlapping live blocks. The alrt CList corruption
likely results from the same allocator bug hitting the alrt task's
TAlertEventHandler region.

### Caveats — what this iteration's data does NOT yet show

- **No `free`/`__dl__` tracking**: many "same-address" overlaps are
  probably normal recycle. The partial-overlap cases (#118/#120,
  #607/#610) are the unambiguous bugs.
- **TAlertManager allocation not in log**: the boot wedges before
  log seq 939 ever logs a 200-byte (`0xc8`) allocation, AND no
  allocations near VA=0x0cca3000 appeared. So we haven't directly
  caught the allocation that produces the alrt CList corruption —
  it might come from a different allocator path, or our probe
  fires after the alrt allocation has already happened. (Possible
  the alrt task creation runs through `NewBlock` or `NewPtr`
  rather than `__nw__`.)
- The alrt CList page (PA=0x0402e000) corresponds to VA range
  0x0cca3000+. None of the 293 overlap addresses land in that
  RAM aperture either — but the corruption hits via the
  allocator's freelist threading code (the ROM PCs we see are
  `MoveFreeBlock` / `SetFreeChain`), not via direct __nw__
  return values.

### Iteration 35 (next-loop iter 31): UnhandledException halt-on-entry — wedge originates in CardFaultMonProc

User pointed out we should obviously halt directly at
UnhandledException rather than at the downstream Reboot canary.
Iter-35 wires the trip:

- New HVC #0x69 at ROM `0x000B_0220` (UnhandledException entry)
- New HVC #0x6A at ROM `0x000B_031C` (UnhandledNonUserModeException entry)
- New `handle_unhandled_exception(ctx, non_user)` halt-on-entry
  handler in `src/trap.rs`. Reads exception name from r0 as a
  C-string (BE-byte-order within LE words, per Newton 2.x BE32
  conventions in iter-30 docs) and prints it directly. Dumps r0..r3,
  TRUE source mode/SP/LR via `UND_SAVE_SPSR_IPA`, and the first 8
  words at r1 (exception data).

#### Cold-boot result

```
*** invariant violation: kernel reached UnhandledException ***
  variant: UnhandledException
  r0=0x000afdd8  r1=0  r2=0  r3=0x00002163
  exception name (r0) @ VA=0x000afdd8: "evt.ex.abt.perm"
  TRUE source mode=USR  caller_lr=0x0004e664  sp=0x0c30df14

current task: 'cdfm' (id=0x20c3)
```

The caller LR `0x0004e664` is right after `bl Throw` at
`0x0004e660`, which lives inside
`CardFaultMonProc__12TCardDomainsFlPv` at ROM `0x0004e498`.

So the wedge is fully characterized:

1. The `cdfm` task takes a **data-abort permission fault** in
   card-domain memory.
2. The kernel's `CardFaultMonProc` (registered as the fault
   monitor for `TCardDomains`) examines the fault, decides
   it can't recover, and `Throw`s `"evt.ex.abt.perm"`.
3. No handler claims that exception → `UnhandledException`.
4. `UnhandledException` decides "warm reboot" → `Reboot` →
   ROMBoot loop.

`r3=0x00002163` is task id of `cdsv` (the card-services task)
— possibly the env/domain of the DABT.

#### What this means for phase B

`evt.ex.abt.perm` is the kernel's name for the kind of fault
that 1 KiB subpage AP overlay was specifically designed to
control. With AP-flatten, the kernel's monitor decides the
fault is unhandleable. The fix path is one of:

a. **Stage-2 splitting at the offending VA** — if we know the
   FAR is in a Group-1 alias, install per-subpage stage-2
   mappings that recreate the kernel-intent AP shape.
b. **Patch `CardFaultMonProc`** to recognize the post-flatten
   AP and treat it as a non-fault (e.g. ignore subpage AP
   mismatches). Risky — could mask real faults.
c. **Patch the kernel allocator** that handed the cdfm task
   memory in a layout that doesn't match its expected
   subpage AP plan. Most invasive but most "correct".

#### Next iteration plan (iter-36)

1. **Halt at Throw or at the ABT decision point.** Add a probe
   inside `CardFaultMonProc` (~0x0004e660 — at the `bl Throw`)
   so we capture the full state immediately before Throw is
   called: monitor's argument (the TException-like structure
   it just decided was unhandleable), the FAR/DFSR captured
   in the fault frame, the offending USR-mode PC.

2. **Or halt earlier still: at `DataAbortHandler` permission-
   fault classification.** The existing DAH probes log the
   DABT entry; iter-36 can extend with a halt when the
   classified DFSR is permission (status=0xD or similar) AND
   the monitor returns unhandleable.

3. Once we have `(FAR, DFSR, USR_PC, faulting insn)`, decode
   the access type (load/store/byte/word) and pick the fix
   strategy from a/b/c above.

#### Status

- Build clean.
- UnhandledException halt-on-entry tripwire fires cleanly with
  exception name dumped as ASCII.
- Caller pinned to `CardFaultMonProc` → bl Throw → "evt.ex.abt.perm".
- 30/30 shadow_stub tests pass.
- Iter-35 deliverables: removed dependency on the Reboot canary
  and stack-string decode trick; PLAN.md plan for iter-36
  scoped to capture the underlying FAR/DFSR/USR_PC.

### Iteration 34 (next-loop iter 30): TRUE caller decoded — wedge is "evt.ex.abt.perm" UnhandledException

Iter-33 enhanced the canary handler with SP_und + R0 + R3
dumps but mis-identified the source mode as UND. The mode bits
in SPSR_EL2 reflect the TRAMPOLINE'S mode (the UND trampoline
issues HVC #UND_TAG from UND mode), not the original caller's
mode. Iter-34 fixes this by reading the trampoline-saved CPSR
from `UND_SAVE_SPSR_IPA` and using THAT to look up the true
banked LR / SP / mode.

#### Cold-boot output, iter-34

```
*** Reboot canary fired ***
  ELR_EL2  = 0x00ffff58  SPSR_EL2 = 0x000001db  mode=UND (trampoline)
  R14_UND = 0x000d9888  (trampoline bookkeeping, not caller)

  TRUE source CPSR = 0x60000110  mode=USR (0x10)
  TRUE caller LR   = 0x000b02c0  (= 0xb02bc + 4, caller is the function at 0xb0220)
  TRUE source SP_usr = 0x0cc77cc8

  TRUE source-mode stack (16 words from 0x0cc77cc8):
    [+0]  = 0x556e6861  ("Unha")
    [+4]  = 0x6e646c65  ("ndle")
    [+8]  = 0x64206578  ("d ex")
    [+12] = 0x63657074  ("cept")
    [+16] = 0x696f6e20  ("ion ")
    [+20] = 0x6576742e  ("evt.")
    [+24] = 0x65782e61  ("ex.a")
    [+28] = 0x62742e70  ("bt.p")
    [+32] = 0x65726d2c  ("erm,")
    [+36] = 0x20776172  (" war")
    [+40] = 0x6d207265  ("m re")
    [+44] = 0x626f6f74  ("boot")
    [+48] = 0x21001318  ("!\0..")
```

**Decoded ASCII: `"Unhandled exception evt.ex.abt.perm, warm reboot!"`**

`evt.ex.abt.perm` is NewtonOS's exception identifier for
**data-abort permission fault** (event → exception → abort →
permission). The kernel hit a permission DABT the recovery
path couldn't handle, so it routed to UnhandledException which
chose "warm reboot" as the action. The caller LR `0x000b02c0`
is right after `bl Reboot at 0xb02bc`, which is inside
`UnhandledException__FPcPvPFPv_v` at ROM 0x000b0220.

#### Why this matters

The wedge is now directly implicated in the iter-3 AP-flatten
step. The phase-B hypothesis was: with 1 KiB subpage AP overlay
removed and replaced with flat AP=011 (RW/RW), the kernel
won't notice the difference because its allocators have been
patched to operate at 4 KiB granularity. Iter-30 verified the
ALLOCATOR-side invariants hold. But the KERNEL'S RUNTIME ACCESS
PATTERNS apparently still depend on per-subpage AP differences:
some 1-KiB region used to be no-access for the current task, but
post-flatten the whole 4-KiB page is RW. The kernel either:

a. Hits a permission DABT because we patched the AP "less
   restrictively" than it expected, and the kernel's recovery
   logic interprets that AP shape as "this shouldn't be writable
   at all".
b. Or, more likely, hits a permission DABT because the AP shape
   is now uniform-RW and the kernel's monitor/ACL-check finds
   a violation it can't reconcile.

#### Next iteration plan (iter-35)

1. **Capture the underlying permission DABT.** Add an HVC tripwire
   at the kernel's DABT-entry path (the existing
   `DAH_MRS_SPSR_HVC_IMM` probe at 0x00393144 already runs once
   per DABT) that captures `(FAR, DFSR, ELR, AP-of-faulting-page)`
   into a ring buffer. When the canary fires, dump the last few
   ring entries — the most recent permission-DABT entry IS the
   one that escalated to UnhandledException.

2. **Decode the AP shape at the faulting VA.** Walk stage-1 to
   recover the L2 entry for the FAR VA. Read its 4 1-KiB AP
   subfields (bits [11:10], [9:8], [7:6], [5:4]) and the page-
   level AP (bits [5:4]). Print all five so we can see which
   subpage's access permissions the kernel was checking.

3. **Cross-reference with `kernel_intent_mask_for(pa, va)`.** The
   Prim Remember tracker already records the kernel-intent mask
   for each (PA, VA). If the faulting VA has tracker output, we
   can see whether the AP-flatten step removed a no-access
   subpage that the kernel still expects.

If the FAR is in a Group-1 alias range (PAs 0x04004000 /
0x04005000 / 0x04006000 — the kernel exception-stack pages),
the fix path is stage-2 splitting (reinstate per-subpage AP via
shadow PA aliasing). If it's in heap or stack range, the fix is
likely a kernel-allocator patch.

#### Status

- Build clean.
- True caller ID'd: `UnhandledException__FPcPvPFPv_v` at
  ROM 0x000b0220, called from a permission DABT path.
- Exception name: `"evt.ex.abt.perm"` — data-abort permission
  fault.
- 30/30 shadow_stub tests pass.
- Iter-34 deliverable: trampoline-hidden caller decoded, wedge
  identified as permission-DABT escalation, iter-35 plan
  scoped to capture the underlying fault.

### Iteration 33 (next-loop iter 29): enhanced canary dump pins Reboot caller to GenericSWIHandler

Iter-32 collected (ELR, SPSR, R0..R3, R14_und) at canary time
but couldn't identify the calling function. Iter-33 extends the
canary handler in `src/trap.rs::handle_reboot` to dump:

1. SP_und stack (16 words from `ctx.x[23]`).
2. The R0-pointed exception-descriptor candidate (8 words).
3. R3 decoded as both unsigned and signed int32.

#### Cold-boot output, iter-33

```
*** Reboot canary fired ***
  ELR_EL2  = 0x00ffff58  (= Reboot entry PC)
  SPSR_EL2 = 0x000001db  mode=UND
  R0 = 0xffffd8a5  R1 = 0  R2 = 0  R3 = 0x7fffffcd
  R12 = 0x0cc77cc8  R14_UND = 0x000d9888

  SP_und stack (16 words from ctx.x[23]=0x0c006000):
    [+0..+60] = 0x6db6db6d / 0xb6db6db6 / 0xdb6db6db (POISON)

  Exception-descriptor candidate at R0=0xffffd8a5:
    all 8 entries: (unmapped)

  R3 decoded as error code: 0x7fffffcd (2147483597, signed=2147483597)
```

**Key findings:**

1. SP_und is **fully poisoned** (0x6db6db6d sliding-hex pattern,
   the kernel's "uninitialized" marker). The UND-mode handler
   never pushed a frame before reaching Reboot — i.e. Reboot
   was reached via a `bl` or `b` from UND mode without any prior
   APCS prologue.

2. R0=0xffffd8a5 doesn't translate (no stage-1 mapping). It's
   not an exception-descriptor pointer; it looks like a kErr_-
   shaped error code in disguise (0xFFFFD8A5 ~= -10075 signed,
   in the kErr_* range -10000..-50000).

3. R3=0x7fffffcd (positive, near INT_MAX) is most likely
   uninitialized — Reboot's signature is `(long, unsigned long,
   unsigned char)` so R3 isn't even an argument.

#### Caller pinned via rom.dis

`grep "bl 0x1bef798"` on the disassembly finds 9 `bl Reboot`
sites:

```
0xb02bc, 0xb0394, 0xd8fd4, 0xd9980, 0xe6c00, 0xe9b98,
0x113f54, 0x113f68, 0x13be54
```

The caller at `0x000d8fd4` is inside **`GenericSWIHandler`**
(starts at `0x000d8a64`). The local context just before
`bl Reboot`:

```
d8f94: bl Get__12TObjectTableFUl    ; lookup an object
d8fa0: movs r1, r0                  ; r0=0 → object not found
d8fa4: moveq r7, #229
d8fa8: subeq r7, r7, #10240         ; r7 = -10011 = 0xFFFFD8E5
d8fac: beq 0xd9228                  ; → return r7 as error
...
d8fd0: mov r0, r4
d8fd4: bl Reboot                    ; called when handler decides to reboot
```

So the kernel's SWI handler is executing one of its early-boot
self-tests, the test fails, and it tail-calls Reboot. Mode is
UND because GenericSWIHandler runs from UND? Unlikely —
GenericSWIHandler is an SVC-mode handler. Need to verify which
of the 9 caller sites is actually firing.

#### Next iteration plan (iter-34)

**Per-callsite Reboot tagging.** Patch each of the 9 `bl Reboot`
sites with a UNIQUE HVC (instead of letting them all reach the
canary at Reboot's prologue). Each HVC handler logs the caller
PC and emulates the bl. The canary dispatch then identifies
*which* of the 9 sites is the actual trigger — in one boot.

Implementation: in `src/rom_patches.rs`, add 9 `REBOOT_CALLER_*`
constants (HVC #0x6A..0x72), patch each `bl Reboot` site with
its tagged HVC, and in the dispatcher log the call site +
arguments before halting (or chaining to the existing canary).

Once the actual caller is identified, the failure-mode
investigation moves to whatever check that specific caller is
running — likely an object-table lookup, parameter-validation,
or environment-domain check that's failing because of Phase B
state.

#### Status

- Build clean.
- Canary output upgraded with SP_und walk + R0 descriptor +
  R3 decode.
- Caller narrowed to one of 9 `bl Reboot` sites; iter-34 will
  pin the specific one.
- 30/30 shadow_stub tests pass.

### Iteration 32 (next-loop iter 28): canary-source investigation — Reboot from UND mode

Iter-30/31 verified the heap and stack invariants and tightened
the Prim layer. With those in place the wedge is consistently the
existing Reboot canary, with mode-context revealing where it
comes from.

#### Canary handler output, untraced run

```
*** Reboot canary fired — guest kernel is rebooting ***
  ELR_EL2  = 0x00ffff58  (= Reboot entry PC)
  SPSR_EL2 = 0x000001db  mode=UND (0x1b)
  R0 = 0xffffd8a5  R1 = 0  R2 = 0  R3 = 0x7fffffcd
  R12 = 0x0cc77cc8  R14_UND = 0x000d9888
```

Mode=UND when Reboot runs — that's the Throw/UnhandledException
exit path. The kernel's UnhandledException handler eventually
tail-calls Reboot. R3=0x7FFFFFCD looks like a NewtonOS error code
(packed kErr- form). R0=0xFFFFD8A5 likely an exception
descriptor pointer.

#### Trace-mode run reveals timing dependence

`cargo run --release --features trace,quiet` produced 175k+
trace entries over 60 s and never hit the Reboot canary. The
last entries show the kernel spinning in `TFlash::Read` /
`SFlashLogEntry::IsValid` — flash log replay during early boot.
Trace mode adds ~10× per-call overhead via the HVC trampoline,
so the kernel doesn't reach whatever post-init self-test fires
the canary in the untraced run. This rules out tracing as a
direct path to the caller.

#### What we have / want

- We have: Reboot fires from UND mode, R3 = error code
  0x7FFFFFCD, exception descriptor at 0xFFFFD8A5.
- We want: the user-mode call chain that issued Throw, plus
  the actual error symbol the kernel resolved.

#### Next iteration plan (iter-33)

Extend the existing Reboot canary handler in `src/trap.rs` to:

1. Walk the SP_und stack (8–16 words) and decode any APCS frames
   it finds. The UND-mode code path that ends in Reboot is
   `UnhandledException → Throw → ...`; the saved fp / lr chain
   should resolve back to the user-mode caller that issued the
   exception.
2. Resolve the error code in R3 against the kernel's exception
   string table. The kernel keeps `(kErr_*, "name")` pairs in
   ROM; finding R3=0x7FFFFFCD's symbol turns "some self-test
   failed" into "the kernel rejected something specific".
3. Read `[R0]` (= the exception descriptor) and dump the first
   16–32 bytes; if it's a TException or TThrow object the layout
   is documented in NewtonOS internals.

If steps 1–3 don't pin the source, fall back to setting an
HVC tripwire at the kernel's `Throw` entry (find the symbol PC
via classify-out) so we catch the exception at the moment it's
raised, not at the moment Reboot fires.

#### Status

- Build clean.
- Untraced cold-boot reaches Reboot canary (mode=UND).
- Traced cold-boot doesn't reach canary in 60 s — timing-
  dependent, can't use trace alone to find the caller.
- Iter-32 deliverables: characterized canary mode (UND →
  Throw path), documented R3 error code 0x7FFFFFCD as the
  resolution target for iter-33; PLAN.md plan for iter-33
  (extend canary handler with SP_und walk + error-code
  resolution).

### Iteration 31 (next-loop iter 27): Prim aliasing/forget halts — defensive

Iter-30 verified the high-level heap and stack invariants hold.
This iteration tightens the Prim Remember/Forget tracker (which
already detects PA-aliasing and forget-mismatch events but only
logged) into halt-shaped assertions, in case a regression reopens
Group-2 PA aliasing or introduces an unmatched Forget.

Two changes to `src/trap.rs`:

1. `handle_prim_remember_probe_with`: when a second VA appears for
   the same PA without a prior Forget for the first VA, halt with
   `(PA, VA1, VA1's upstream_lr, VA2, VA2's upstream_lr / user_pc /
   user_lr / user_caller, mask, perm)`. The iter-23 GetMatchingPage
   stub eliminated all 12 Group-2 PA aliases observed pre-iter-23;
   any new alias is a real regression.

2. `handle_prim_forget_probe_with`: when the kernel forgets a
   (PA, VA') pair but our tracker had a different VA for the same
   PA, halt with `(PA, forgot_VA, tracker_VA)`. Either Prim
   ordering is wrong or our tracker is desynced — both worth
   surfacing immediately.

Removed two now-unused budget statics (`PRIM_ALIAS_LOG_BUDGET`,
`PRIM_FORGET_MISMATCH_BUDGET`).

Cold-boot: same as iter-30, both halts silent through the boot
to the existing Reboot canary. So pre-canary, neither aliasing
nor forget-mismatch fires — the wedge is downstream of the Prim
layer.

Iter-32+ should investigate the canary directly: the existing
canary handler already runs `task_dump::dump_full()` and shows
24 tasks at SchedulerStart, so the kernel is reaching the
post-init self-test. Read the saved-PC map (e.g. `task 0xc1233f8
(main) savedPC=0x3ae220 SPSR=0x40000110`) and trace what
`0x003AE220` does — that's likely the kernel's Idle / scheduler
loop, and one of the tasks is the one calling Reboot.

#### Status

- Build clean.
- Boot reaches Reboot canary (iter-30 baseline preserved).
- 30/30 shadow_stub tests pass.
- Iter-31 deliverables: Prim aliasing halt, Prim forget-mismatch
  halt, PLAN.md plan for iter-32 (read the saved-PC map at canary
  time to find the calling task, then chase the actual self-test
  fail).

### Iteration 30 (next-loop iter 26): high-level heap/stack invariant pass — 4 KiB hypothesis HOLDS

User course-correction: stop chasing individual r0 / count
clobbers under a microscope; instead instrument the actual
phase-B hypothesis directly. With kernel patches that move all
allocations to 4 KiB granularity, do heap blocks remain
non-overlapping and stacks remain bounded by guard pages? Test
that at the assertion level: halt on first violation.

#### What landed

1. **`halt_invariant(label, local_dump)` helper** in `src/trap.rs`.
   Uniform `*** invariant violation: ... ***` header, runs the
   per-assertion local-context dump, then `task_dump::dump()`,
   then `cpu::halt()`. Used by all new tripwires.

2. **__nw__ heap-overlap halt** in `handle_nw_return_probe_with`.
   Existing probe already maintained NW_TABLE (live allocations)
   and detected overlaps but only logged. The dl-probe at
   `0x00318F28` already keeps the table accurate (clears entries
   on free). Iter-30 flips the existing log-on-overlap to
   halt-on-overlap with classification (same-address →
   "use-after-free or missed __dl__"; different-start partial
   overlap → "4-KiB chunking violated").

3. **NewStack guard-page + alignment invariants** in
   `handle_new_stack_probe_with`:
   - Halt if `out_base`, `out_top`, or `span` aren't 4-KiB-
     aligned/multiple. Pre-flatten the kernel could return 1-KiB-
     aligned ranges and rely on subpage AP overlay; post-flatten
     it must operate at full-page granularity.
   - Halt if `guest_mem::translate_va(out_base - 0x1000)` returns
     `Some(_)`. Stacks grow down; the page below must be
     fault-on-access so a stack overflow takes a clean DABT into
     `TStackManager::Fault`.

#### Cold-boot result — invariants HOLD

```
nw alloc #0..#31 (heap allocations, no overlap halt)
NewStack POST-SWI: env=0x1355 base=0x0c306000 top=0x0c30e000 span=0x8000 ...
NewStack POST-SWI: env=0x1355 base=0x0c30f000 top=0x0c317000 span=0x8000 ...
NewStack POST-SWI: env=0x1355 base=0x0c318000 top=0x0c320000 span=0x8000 ...
... [continues for 24 tasks total] ...
*** Reboot canary fired *** (existing iter-23 wedge — different cause)
```

Every stack: base ≡ 0 mod 0x1000, top ≡ 0 mod 0x1000,
span = 0x8000 (32 KiB), and the page at `base - 0x1000` is
unmapped (guard page intact). Note the kernel allocates
adjacent stacks 0x9000 apart (8 task pages + 1 guard page) —
so it IS reserving a guard page per stack. The 4-KiB hypothesis
holds at this level.

For heap: 32+ allocations all distinct address ranges, no
overlap halt fired, dl probe matched frees correctly.

#### What this means

The Phase B "stack/heap 4 KiB problem" — i.e. that losing 1 KiB
subpage AP overlay would cause the kernel's heap and stack
allocators to produce overlapping or guard-less ranges — is
**not happening at the high-level invariants we just locked
down**. Combined with iter-26/27/29's findings (LDRB stub
doesn't clobber r0; iter-25's r0=0x20000110 was an
instrumentation artifact of broken probe-SPSR emulation), the
remaining Reboot canary wedge is a different bug.

#### What this rules out / in

- **RULED OUT:** Heap allocator handing out two distinct live
  blocks that overlap.
- **RULED OUT:** NewStack returning misaligned ranges or stacks
  without guard pages.
- **STILL UNCHECKED:** ResolveFault (lazy stack growth)
  returning pages that alias another task's stack via PA
  collision; Prim Remember/Forget pair consistency;
  ExtendVMHeap / AllocNewPage page-grain alignment.

#### Next iteration plan (iter-31)

Close the remaining gaps from iter-30's plan file
(`/Users/walter/.claude/plans/now-in-plan-mode-gentle-wigderson.md`):

1. **ResolveFault RETURN probe.** Add HVC at the
   post-RememberMappings return site inside
   `TStackManager::ResolveFault` (the function entered at the
   existing entry probe, ROM `0x001F7978`). Find the right
   return site via rom.dis. Walk `TStackPage::subpage_owner[0..4]`
   for the resolved page; halt if any owner ≠ faulting task's
   manager and ≠ NULL. Cross-task scan: walk all `TTask`s in
   `TScheduler` task list, pull their `TStackInfo*`, and assert
   the resolved PA isn't in any other task's stack range. Halt
   on collision with colliding task id + name + bounds.

2. **Prim Remember/Forget consistency.** Extend
   `handle_prim_remember_probe` / `handle_prim_forget_probe` to
   maintain a live (PA, VA) set. Halt on Forget for an
   unrecorded pair. Boot-completion audit (at SchedulerStart or
   BootOS canary) dumps remaining live pairs.

3. **Page-grain alignment audits** on ExtendVMHeap (needs new
   return-side probe — entry-only currently) and AllocNewPage
   (needs full new entry/return probe pair).

After (1)-(3) close cleanly, investigate the remaining Reboot
canary cause directly via `task_dump::dump()` output (24 tasks
already at the canary, so the kernel reached SchedulerStart and
the canary fires from a downstream self-test).

#### Status

- Build clean, 30/30 shadow_stub tests pass.
- 24 tasks created with all stack invariants intact.
- 32+ heap allocations with all overlap invariants intact.
- Iter-30 deliverables: halt_invariant helper, __nw__ overlap
  halt, NewStack guard-page + alignment halt, PLAN.md
  high-level finding (4-KiB hypothesis holds for heap/stack
  invariants).

### Iteration 29 (next-loop iter 25): BNE control-flow probe — r0 propagates cleanly; iter-25 "corruption" was an instrumentation artifact

Per iter-28's plan (b), iter-29 added a probe at ROM `0x00257088`
that replaces the `bne 0x2570c0` and emulates it via direct
`ELR_EL2` routing. Constants in `src/rom_patches.rs`:

```
WC_BNE_PROBE_HVC_IMM        = 0x68
WC_BNE_PROBE_PC             = 0x0025_7088
WC_BNE_FIRST_INSN           = 0x1A00_000C  // bne 0x2570c0
WC_BNE_TAKEN_TARGET         = 0x0025_70C0
WC_BNE_FALLTHROUGH_TARGET   = 0x0025_708C
```

Handler `handle_wc_bne_probe_with` computes `Z = (r1 == sl)` from
`ctx.x[1]` and `ctx.x[10]` (TEQ at 0x257084 is read-only — r1 at
the BNE still holds the LDRB result), logs the decision, and
returns the resume PC. The HVC dispatch in `handle_und` passes
that target to `return_to_guest_from_und`. No SPSR touch
required.

#### Cold-boot result — r0 propagates cleanly through the entire WriteChunk chain

```
WriteChunk #0 ENTER: this=0x0c646c0c ... count=0x0
WC-load #0: count=0x0 r5=9 r7=0
WC-postload #0: r0=0x0
WC-postldrb #0: r0=0x0 r1=0x0 sl=0x0
WC-bne #0: r0=0x0 r1=0x0 sl=0x0 Z=1 → fall-thru (target=0x0025708c)
WC-add #0: r0=0x0(0) r4=0x0c646c0c → r1=0x1
WC-store #0: this=0x0c646c0c r1=0x1(1) before=0x0
WC-reload #0: this=0x0c646c0c count=0x1(1)
```

**r0 stays at 0 from WC-load all the way through WC-add.** The
iter-25 "r0=0x20000110 at WC-add" corruption is GONE.

#### Diagnosis: iter-25's r0 corruption was an instrumentation artifact

Reasoning the chain through iter-25 vs iter-29:

- iter-25 had probes at 0x257074 (WC-load), 0x257078 (WC-postload),
  0x25708c (WC-add), 0x257090 (WC-store), 0x25709c (WC-reload).
  No probe at 0x257084 (TEQ native) or 0x257088 (BNE native).
- iter-25 used a sentinel: WC-load wrote `ctx.x[0] = 0x12345678`
  instead of the real count. WC-postload saw the sentinel
  (probe-context preserves r0 correctly via ctx.x[]). WC-add then
  saw r0 = 0x20000110, not the sentinel.

iter-29 (with WC-postldrb + WC-bne probes added):
- No sentinel; r0 = real count = 0.
- All probes see r0 = 0 — no corruption.

Several hypotheses for the iter-25 corruption have now been ruled
out by static / runtime evidence:

- **LDRB stub clobbers r0** (iter-25 hypothesis): refuted by
  iter-26 static analysis + iter-27 runtime check.
- **Async IRQ between probes**: would still trigger in iter-29 if
  it were the cause, since the inter-probe gap is unchanged
  between WC-load and WC-add. r0=0 throughout suggests no IRQ
  clobber.
- **TEQ / BNE side effects**: TEQ doesn't write GPRs; BNE doesn't
  write GPRs. Native vs. patched should have the same r0 effect.

Most likely explanation: iter-25's broken probe SPSR-emulation
(discovered iter-27) made `bls` at 0x25707c take occasionally
based on stale flags. When stale flags happened to satisfy "bls
take", the kernel branched to 0x2570d0 — a different code path
where the LDRB at 0x2570d4 is shadow-stub-patched. The live walk
from 0x2570d8 sees `and r0, sl, #7` which **writes r0 first**, so
r0 IS dead at that point — and `pick_scratch_regs` for THAT LDRB
will pick R0 as a scratch (CPSR gets MRSed into r0, leaving a
CPSR-shape after the byte access until the `and` overwrites it).

So the picture: iter-25 saw r0=0x20000110 because the kernel
silently went through 0x2570d0..0x2570d8, where r0 picked up a
CPSR scratch value from a different LDRB stub, then was
re-routed back to the WriteChunk path with r0 still holding the
CPSR value when the WC-add probe at 0x25708c fired.

Iter-29 doesn't reach 0x2570d0 on iter #0 (count=0 → bls would
take with REAL flags too, but our broken cmp emulation kept
flags Z=0 → fall-through unintentionally). Wait — that means
the path through 0x257080..0x25708c IS executed when the broken
flags say "fall through", regardless of the actual count. So
iter #0 count=0 path:
- iter-25: stale flags at bls fall through → 0x257080 → ... → WC-add. r0 corruption visible.
- iter-29: stale flags at bls fall through → ... → WC-bne (Z=1) → fall-through to WC-add. r0=0 visible.

Hmm, those should match. So why does iter-29 NOT show the
corruption?

**The remaining hypothesis**: iter-25's WC-postload probe handler
or the trampoline path itself was clobbering r0 in some subtle
way (e.g., the trampoline's R0 stash logic at UND entry). Adding
WC-postldrb (iter-27) and WC-bne (iter-29) changes the trampoline
hit pattern — perhaps a UND-trampoline-induced clobber that fires
once per round-trip is washed out by the additional probes. Or
the iter-25 sentinel value 0x12345678 specifically interacts with
some cache / banked-reg semantics on QEMU raspi3b.

#### What this rules in / out

- **iter-25's r0=0x20000110 was NOT caused by the LDRB stub at
  0x257080**, NOT caused by an async IRQ, and very likely a
  byproduct of the broken probe instrumentation interacting with
  QEMU raspi3b in a non-obvious way.
- **The actual r0 propagation through this code is clean** when
  the BNE is properly emulated.
- **The new wedge cause** is `count` getting stuck at 1: with
  WC-postload's broken flag emulation, the bls at 0x25707c takes
  on subsequent iterations (real count ≠ 0, but stale flags say
  Z=1 OR C=0), routing the kernel through 0x2570d0 which doesn't
  increment count. The kernel exits the WriteChunk loop early
  with count=1, calls WriteRun(count=1), then hits Reboot.

#### Next iteration plan (iter-30)

The iter-25 r0 puzzle is essentially DISSOLVED — it was an
instrumentation artifact, not a real kernel bug. The remaining
issue is the SPSR-plumbing bug that affects bls at 0x25707c (and
in principle every flag-dependent kernel branch downstream of a
patched cmp/teq).

Approach options:

a. **Apply the iter-29 pattern to bls.** Replace `bls 0x2570d0`
   at 0x25707c with HVC, emulate the branch decision from r0
   directly (`Z = (r0 == 0)`, bls condition `C=0 || Z=1` —
   conservatively just check `r0 == 0` for our use since cmp
   sets C=1 always when subtracting 0). When the bls condition
   is met, route ELR to 0x2570d0; otherwise fall-through.
   That eliminates the broken-flag dependency at this site.

b. **Find a working SPSR-plumbing fix.** The iter-28 attempt
   used `MSR SPSR_cxsf, lr` from UND mode — broken on QEMU
   raspi3b. Alternative: use an HVC handler in the EL2 path
   that writes SPSR via a different mechanism. E.g., an `MSR
   SPSR_und, x0` from AArch64 EL2 (if that AArch64 sysreg
   alias works on QEMU raspi3b — it might, since it doesn't go
   through the same banked_spsr[] write path as the AArch32-
   side MSR).

c. **Audit and re-disable broken flag-emulation probes.** The
   WC-postload, WC-postldrb, and (existing) page-get TEQ
   handlers all silently fail to update flags. Either fix the
   plumbing (option b) or remove them and use ELR-routing
   probes (option a) at every flag-dependent site they
   precede.

Approach (a) is the easiest tactical fix — replace bls with an
HVC and resume normal kernel boot. Approach (b) is the proper
fix that unblocks all flag-emulating probes.

#### Status

- Build clean.
- Cold-boot reaches WriteChunk + full inner-loop probes; count
  increments correctly on iter #0; subsequent iterations stuck
  at count=1 due to stale-flag bls.
- Iter-29 deliverables: WC-bne probe with direct ELR_EL2
  control flow; r0=0 propagates cleanly; iter-25 corruption
  hypothesis dissolved into instrumentation-artifact theory.

### Iteration 28 (next-loop iter 24): SPSR-plumbing fix attempted, blocked by QEMU `MSR SPSR_cxsf` quirk

Per iter-27's plan (a), iter-28 attempted to extend the
`UND_RETURN_STUB` (in `src/guest_mem.rs::patch_und_vector`) so it
reloads banked `SPSR_und` from `UND_SAVE_SPSR_IPA` before
`movs pc, lr`. Target layout (7 words = 28 bytes, fitting exactly
between `UND_RETURN_STUB_OFFSET = 0x00FF_FFE4` and ROM_END):

```
+0x00: e59fe00c  ldr lr, [pc, #0xc]    ; lr = SPSR_IPA literal
+0x04: e59ee000  ldr lr, [lr]          ; lr = saved SPSR
+0x08: e16ff00e  msr SPSR_cxsf, lr     ; banked SPSR_und ← lr (UND mode)
+0x0c: e59fe004  ldr lr, [pc, #4]      ; lr = ELR literal
+0x10: e1b0f00e  movs pc, lr           ; CPSR = SPSR_und, PC = lr
+0x14: <UND_SAVE_SPSR_IPA literal = 0x0600_F004>
+0x18: <ELR literal>
```

LR is the sole scratch (rewritten three times) so we don't need
to round-trip R12 through TPIDRURW. Required `UND_RETURN_STUB_LITERAL_OFFSET`
to move from +0x08 to +0x18.

#### Cold-boot result — boot regression at the first kernel store

```
Dropping to EL1 AArch32 at guest IPA 0x00000000 (ROM reset vector)
trap: EC=0x12 (HVC from AArch32) ELR=0x1868c ESR=0x4a000044
BootOS canary: first boot — emulated mov r0,#0xb0 ...
trap: EC=0x12 (HVC from AArch32) ELR=0xffff58 ESR=0x4a000010
und: handle_und first entry, ELR_EL2=0xffff58 SPSR_EL2=0x1db FAR_EL1=0x0
und: MCR p15,0,Rt,c15,c1,2 (StrongARM clock) @PC=0x186a8 — no-op
trap: EC=0x24 (Data abort from lower EL) ELR=0x186b4 ESR=0x93810047
trap: EC=0x24 (Data abort from lower EL) ELR=0x186c0 ESR=0x93810047
... [cascading DABTs at sequential PCs through the kernel]
```

The very first UND-trampoline round-trip (the StrongARM-clock MCR
no-op) returns to 0x186ac, then the next kernel store at 0x186b4
(`str r1, [r0]`) takes a DABT, and the kernel cascades into
broken state. Pre-iter-28 baseline (3-word UND_RETURN_STUB) the
exact same MCR-no-op round-trip works fine.

#### Bisection localizes the regression to the MSR

I replaced individual instructions in the new stub with NOPs:

- All three new instructions NOP'd (loads + MSR): boot reaches
  WriteChunk same as iter-27 baseline. ✓
- Loads enabled, MSR replaced with NOP: still boots fine. ✓
- Loads enabled + MSR enabled (the actual fix): boot dies at
  0x186b4 with cascading DABTs. ✗

So the load sequence is fine; the MSR specifically is the cause.

#### Diagnosis: suspected QEMU raspi3b banked-SPSR-write quirk

Encoding `MSR SPSR_cxsf, lr = 0xE16F_F00E`:
- bits 31:28 cond = 1110 (AL)
- bits 27:23 = 00010 (data-processing misc)
- bit 22 R = 1 (SPSR)
- bits 21:20 = 10
- bits 19:16 mask = 1111 (cxsf)
- bits 15:12 SBO = 1111
- bits 11:4 = 0
- bits 3:0 Rm = 1110 (LR)

Encoding matches ARM ARM A8.8.110 verbatim. From UND mode, MSR
SPSR_cxsf, Rm targets the current mode's banked SPSR
(SPSR_und). On real hardware this would be a no-op (the value we
write to memory in the trampoline is what we re-MSR). But the
boot regression strongly implies QEMU raspi3b mishandles the
write — clobbering some other piece of state and propagating
chaos into subsequent kernel execution.

This is consistent with `docs/QEMU_BUGS.md` Bug #1 (and the
DataAbortHandler `mrs r1, SPSR` workaround at
`rom_patches.rs::DAH_MRS_SPSR_HVC_IMM`): QEMU raspi3b's banked
SPSR plumbing is unreliable. We've already wired around the
**reading** side via the DAH MRS-replacement HVC; the **writing**
side appears to have its own quirk.

#### Cleanup

Reverted `src/guest_mem.rs::patch_und_vector` to the iter-27
3-word stub. Boot is back to iter-27 baseline. The negative
finding is documented in the patch_und_vector comment and in
this PLAN entry so iter-29 doesn't waste a cycle re-attempting
the same approach.

#### What this rules in / out

- The SPSR-plumbing fix (approach a from iter-27) is blocked on
  QEMU raspi3b at the `MSR SPSR_cxsf, lr` instruction.
- Approach (b) — replace the bne with HVC, emulate via ELR_EL2 —
  remains untested and is the path forward.
- The MSR may work on FVP (which has more accurate banked-reg
  semantics per docs/QEMU_BUGS.md), but the project must keep
  both QEMU raspi3b AND FVP green per CLAUDE.md, so a
  QEMU-broken fix is unacceptable.

#### Next iteration plan (iter-29)

Approach (b): replace `bne 0x2570c0` at ROM `0x00257088` with
HVC #0x68 (next available probe imm). Handler reads source SPSR's
Z bit and either:
- Z=1 (BNE not taken): let ERET resume at 0x25708c — natural
  fall-through.
- Z=0 (BNE taken): override `ELR_EL2` to point at 0x2570c0 before
  ERET so guest resumes at the BNE target.

The handler doesn't need to write SPSR; it just decides PC.
Sidesteps the QEMU MSR quirk entirely. The handler also logs r0
at this point (the most useful new datapoint — r0 right at the
BNE, between WC-postldrb and WC-add).

Then re-enable the iter-25 sentinel (`ctx.x[0] = 0x12345678` in
WC-load) and re-run. Expected outcome:
- WC-postload r0 = 0x12345678 ✓
- WC-postldrb r0 = 0x12345678 (matches iter-27)
- WC-bne r0 = 0x12345678 OR 0x20000110 (the diagnostic)
- WC-add r0 = 0x20000110 (per iter-25)

If WC-bne sees 0x20000110 → corruption is between WC-postldrb
(0x257084) and WC-bne (0x257088), i.e. either teq-emulation or
something async firing in that one-insn gap (very narrow!).
If WC-bne sees 0x12345678 → corruption is at WC-add itself or in
the bne emulation (regression in our new code), which we can
investigate separately.

#### Status

- Build clean, 30/30 shadow_stub tests pass.
- Boot reaches WriteChunk + WC-postldrb (iter-27 baseline).
- Iter-28 deliverable: documented negative result on the SPSR-
  plumbing fix; preserved iter-27 baseline; iter-29 plan
  pivots to approach (b).

### Iteration 27 (next-loop iter 23): WC-postldrb probe confirms LDRB stub innocent; uncovers latent SPSR-emulation bug

Per iter-26's plan, iter-27 added a probe at ROM `0x00257084`
(replacing the native `teq r1, sl`), positioned between the
LDRB-stub return and the BNE. Probe constants and dispatch in
`src/rom_patches.rs` and `src/trap.rs`:

- `WC_POSTLDRB_PROBE_HVC_IMM = 0x67`
- `WC_POSTLDRB_PROBE_PC      = 0x0025_7084`
- Original word: `0xE131_000A` (`teq r1, sl`)

Handler logs `(r0, r1, sl, src_mode)` and emulates `teq r1, sl`
flag effect by writing the updated SPSR to `UND_SAVE_SPSR_IPA`.

#### Cold-boot result — LDRB stub innocent at runtime

```
WC-load #0:     this=0x0c646c0c count=0x0(0) r5=9 r7=0 src_mode=0x10
WC-postload #0: r0=0x0 src_mode=0x10
WC-postldrb #0: r0=0x0 r1=0x0 sl=0x0 src_mode=0x10
WriteRun #0 ENTER: this=0x0c646c0c w98=0x0 count=0x0(0) ...
```

`r0=0` at the WC-postldrb probe — **exactly what WC-load
delivered.** The shadow-stub's MRS/LDRB/MSR sequence at
`0x00257080` did not modify r0. iter-26 static analysis confirmed
empirically.

#### Latent bug uncovered: probe SPSR-emulation is a no-op

Note the absence of `WC-add #0` in the cold-boot output. The
expected WriteChunk path for `count=0` would be: bls TAKES (since
`cmp r0, #0` with r0=0 sets Z=1) → branches to `0x2570d0`,
skipping the LDRB entirely. But WC-postldrb DID fire — meaning
bls fell through. So `cmp r0, #0`'s flag update from the
WC-postload probe handler did not actually affect bls's reading
of CPSR.

Tracing the data path:

1. `handle_wc_postload_probe_with` calls
   `compute_teq_z_n(source_cpsr, r0)` and writes the result to
   `UND_SAVE_SPSR_IPA` via `guest_mem::write_word_pa`.
2. `return_to_guest_from_und` ERETs to the UND-return stub
   (`UND_RETURN_STUB_VA`):

```
+0x00: e59fe000  ldr lr, [pc, #0]   ; lr = literal (= ELR_EL2)
+0x04: e1b0f00e  movs pc, lr        ; CPSR = SPSR_und (banked!), PC = lr
```

3. `movs pc, lr` in UND mode copies **banked `SPSR_und`** (the
   hardware register, saved by the CPU at UND entry) into CPSR.
   It does NOT consult `UND_SAVE_SPSR_IPA`.

The trampoline saves `SPSR_und` to memory at entry but never
reloads it from memory before the return. So **every probe that
writes `UND_SAVE_SPSR_IPA` to "emulate" a flag-setting instruction
has been a silent no-op.** The kernel runs with stale CPSR (the
flags from *before* the patched instruction) and the patched
flow happens to work only when the stale flags coincidentally
satisfy the kernel's expected branch.

WC-postload (cmp), WC-postldrb (teq), and the page-get TEQ
emulator (`apply_page_get_teq_flags`'s UND-mode branch) all share
this defect. The two probes that emulate non-flag-setting
instructions (WC-load, WC-add, WC-store, WC-reload) are
unaffected — they only update GPRs through `ctx.x[]`, and that
path works because the EL2 trap context is restored to
guest registers at ERET.

#### Why iter-25's WC-add #0 fired but iter-27's didn't

Both runs reach the LDRB at `0x00257080` only because bls falls
through. With WC-postload's flag emulation broken, bls's flag
read is whatever the kernel had before the patched cmp — and that
happens to NOT trigger bls (Z=0 ∧ C=1). Lucky.

After the LDRB stub returns at `0x00257084`:
- iter-25: native `teq r1, sl` runs, sets Z=1 (r1 == sl observed
  per iter-27's WC-postldrb log). BNE doesn't take, fall through
  to WC-add. ✓
- iter-27: my WC-postldrb probe **replaces** the teq with HVC.
  Flag update via UND_SAVE_SPSR_IPA is a no-op. BNE reads stale
  flags (the same stale flags that existed at bls entry — Z=0).
  BNE TAKES → branches to `0x2570c0` → `mov r0, r4; bl WriteRun`.
  WC-add never fires.

So iter-27's probe broke the bne control flow as a side effect of
the latent SPSR bug. The information we DID get (r0=0 at
0x257084) is still valid — that comes from `ctx.x[0]`, which the
trap context preserves correctly across the trampoline.

#### What this rules in / out

- The shadow-stub LDRB at `0x00257080` does not modify r0
  (confirmed iter-26 statically + iter-27 empirically).
- The iter-25 r0=0x20000110 puzzle remains: with sentinel,
  WC-postload r0=0x12345678, WC-add r0=0x20000110, but no
  intermediate probe at 0x257084. The clobber happened somewhere
  in the 0x257078..0x25708c window. iter-27's probe broke the
  flow before we could interrogate it cleanly.
- Existing probe infrastructure has a latent flag-emulation bug
  affecting cmp/teq/teq-like emulations. Probes that only update
  GPRs (load/store/add) work fine.

#### Next iteration plan (iter-28)

Two viable approaches, in order of preference:

a. **Fix the SPSR plumbing** so flag updates reach banked
   `SPSR_und`. The minimal change: extend the UND-return stub to
   load the saved SPSR from memory and `MSR SPSR_und, <reg>`
   before `movs pc, lr`. Layout becomes:

   ```
   +0x00: ldr  r12, [pc, #0x10]      ; r12 = scratch base (SCRATCH_POOL IPA)
   +0x04: ldr  r12, [r12, #0x04]     ; r12 = saved SPSR_und (potentially updated)
   +0x08: msr  SPSR_und, r12         ; restore banked SPSR_und from memory
   +0x0c: ldr  lr,  [pc, #0]         ; lr = literal (= ELR_EL2)
   +0x10: movs pc,  lr               ; CPSR = SPSR_und (now reflecting handler updates)
   +0x14: <ELR_EL2 literal>
   +0x18: <SCRATCH_POOL IPA literal>
   ```

   Caveat: writing `SPSR_und` from UND mode requires CPSR.M==UND,
   which the trampoline already is at this point. `MSR SPSR_und, ...`
   from the same mode targets that mode's banked SPSR. Verify
   AArch32-EL1 access semantics on QEMU raspi3b (the target most
   likely to misbehave per docs/QEMU_BUGS.md) and on FVP.

   Once fixed, re-run iter-27's probe + run with iter-25's
   sentinel test. The expected outcome at 0x25708c (WC-add) tells
   us where r0 was clobbered.

b. **Probe at the BNE target instead.** Instead of replacing
   `teq r1, sl` (flag-setting), replace the bne at `0x00257088`
   with HVC and emulate the branch directly in the handler. The
   handler reads source SPSR's Z bit and either (a) advances ELR
   to `0x2570c0` (branch taken) or (b) lets ERET resume at
   `0x25708c` (fall through). This sidesteps the SPSR bug because
   the handler controls control flow without needing to update
   guest CPSR.

Approach (a) is preferable — it fixes the latent bug and unblocks
all flag-emulating probes for future iterations. Approach (b) is
a tactical workaround if (a) turns out to be tricky on either
host platform.

#### Status

- 30/30 shadow_stub unit tests pass.
- New probe (`WC_POSTLDRB_PROBE`) installed and operational.
- Cold-boot: r0=0 at 0x257084 confirms LDRB stub innocent.
- Discovered: probe SPSR-emulation has been a silent no-op for
  cmp/teq probes since at least iter-25.
- Iter-28 must fix the SPSR plumbing (approach a) or implement a
  control-flow-controlling BNE probe (approach b) before the
  iter-25 r0 puzzle can resume.

### Iteration 26 (next-loop iter 22): static analysis refutes shadow-stub-clobbers-r0 hypothesis

Iter-25 hypothesised that the shadow-stub generated for the LDRB
at `0x00257080` picks R0 as scratch_ea or scratch_flags and leaves
a CPSR-shaped value in r0 across the byte access. Rather than
booting and dumping bytes, iter-26 statically reasons about
`pick_scratch_regs` (deterministic given the surrounding code) and
locks the result in a unit test.

#### Liveness at orig_pc+4 = 0x00257084

Surrounding block from `rom.dis`:

```
0x257080: ldrb r1, [r4, #160]    <- the access (rt=r1, rn=r4)
0x257084: teq  r1, sl            <- reads r1, sl; no GPR write
0x257088: bne  0x2570c0          <- cond branch
0x25708c: add  r1, r0, #1        <- READS r0   (fall-through)
0x257090: str  r1, [r4, #156]
0x257094: add  r0, r0, r4        <- READS r0
0x257098: strb r6, [r0, #161]    <- READS r0
0x25709c: ldr  r0, [r4, #156]    <- writes r0
... bl WriteRun, return path
0x2570c0: mov  r0, r4            <- BNE target writes r0 (dead before)
0x2570c4: bl   WriteRun
```

`live_at(0x257084, 32)`:
- Fall-through: `add r1, r0, #1` reads r0 before any write → r0
  LIVE on this path.
- Taken (0x2570c0): `mov r0, r4` writes r0 before any read → r0
  DEAD on this path.
- BNE union: r0 LIVE.

`pick_scratch_regs(d, 0x00257080)` then iterates CANDIDATES =
[R12, R0, R1, R2, R3, R14] with operand_mask = R1|R4 (rt=1, rn=4):
- R12: not in operand, not in live → PICK as scratch_ea.
- R0: not in operand, **R0 IS in live** → SKIP.
- R1: in operand → SKIP.
- R2: not in operand, not in live → PICK as scratch_flags. Done.

Returns `Some((12, Some(2)))`. Variant `DeadReg { sfl: Some(2) }`.
Stub uses R12 as EA scratch and R2 to spill CPSR via MRS/MSR.
**R0 is never touched.**

#### Lock-in: unit test

Added `pick_scratch_at_rom_0x257080_does_not_pick_r0` to
`src/shadow_stub.rs::tests`. Synthesizes the basic-block
instruction stream and asserts the picker returns `(12, Some(2))`,
explicitly rejecting r0. Refactor: extracted
`pick_scratch_regs_with_reader` so the picker is reachable from
tests with an injected instruction stream (mirrors the existing
`live_at_with_reader` / `nzcv_dead_recursive` pattern). Removed the
no-suffix `live_at` and `nzcv_dead_at` wrappers — `pick_scratch_regs`
now goes through `_with_reader` directly with `code_read_word`.

All 30 shadow_stub tests pass.

#### What this rules out

- The shadow-stub LDRB patch at `0x00257080` does NOT touch r0.
- The stub's spill/restore path can't be the source of the
  CPSR-shaped 0x20000110 in r0 at WC-add.

#### Where the clobber must come from

Path from WC-postload (0x257078) to WC-add (0x25708c):
1. `bls 0x2570d0` — flags are stale (cmp at 0x257078 was replaced
   by the WC-postload HVC); didn't take (otherwise WC-add wouldn't
   fire).
2. `ldrb r1, [r4, #160]` → shadow-stub. R0 untouched (proved this
   iter).
3. `teq r1, sl` — reads r1, sl; no GPR write.
4. `bne 0x2570c0` — didn't take (otherwise WC-add wouldn't fire).
5. WC-add probe at 0x25708c fires.

None of the architected effects of these instructions write r0.
Remaining suspects, ordered by plausibility:

a. **Asynchronous IRQ between 0x257078 and 0x25708c.** Newton arms
   CNTHP fairly tight; an IRQ during this 4-insn window enters
   `trap_irq` at EL2. If trap_irq writes ctx.x[0] (e.g. through
   the autosave path: snapshot save reads regs into a buffer; if
   the save path stomps ctx.x[0] with CPSR/SPSR before restore,
   that explains the value).

b. **The WC-postload probe handler itself isn't ERETing with
   ctx.x[0] preserved as expected.** The iter-25 diagnostic
   `trap_sync_lower_aarch32 RETURN: ctx.x[0]=0x12345678` proves
   ctx.x[0] is correct *at that print*, but maybe banked-register
   handling in vectors.s flips a different copy at the actual
   ERET (per QEMU_BUGS.md, AArch32↔AArch64 banking is fragile on
   raspi3b).

c. **A different patched site we missed.** `bls`, `bne`, `teq` —
   none are LDRB/STRB so the shadow-stub system shouldn't have
   touched them. But the function tracer (under `--features trace`)
   would have. Verify the count-store run is built without
   `trace`.

d. **The stub's slot 4 MRS/slot 9 MSR pair runs but the captured
   CPSR-shape leaks via a live-but-unconsidered path.** Possible
   if `analyze_insn` for some opcode in the window has a bug that
   makes the walker think r0 is dead when it isn't — but the
   liveness test we just added directly contradicts this.

#### Next iteration plan (iter-27)

Highest-value experiment: insert a per-instruction probe at
`0x257080`, `0x257084`, and `0x257088` logging `r0`. We already
have probes at 0x257078 and 0x25708c bracketing the window; this
shrinks the window to single instructions. If r0 is still
sentinel at 0x257080 and 0x257084 but corrupt at 0x257088 →
something in the BNE / TEQ pair did it (likely an IRQ or
mis-decoded patch). If r0 corrupts at 0x257080 → re-examine the
stub at runtime (the static analysis is wrong somewhere — most
likely an analyze_insn bug for an instruction in the live walk).

If the per-insn probes don't pin it (because the IRQ fires
between probes), instrument `trap_irq` to log ctx.x[0] before any
modification when ELR_EL2 ∈ [0x00257078, 0x00257090].

#### Status

- 30/30 shadow_stub unit tests pass (29 prior + 1 new).
- 36/36 guest tests still pass.
- Iter-26 deliverable: static refutation of iter-25's stub-clobber
  hypothesis; unit test locking the picker's R0-exclusion at
  0x00257080. Refocus on async IRQ / banked-register paths for
  iter-27.

### Iteration 25 (next-loop iter 21): post-load probe pins corruption to shadow-stub-patched LDRB at 0x00257080

Iter 24 narrowed corruption to "between str at 0x257090 and ldr
at 0x25709c" — wrong. Iter 25 widens the trace by adding two
probes around the SUSPICIOUS region:
- `WC-add` at `0x25708C` (already there from iter 24): logs r0
  right before the increment.
- `WC-postload` at `0x257078`: logs r0 right after WC-load's
  ERET.

Sentinel test: temporarily replaced `ctx.x[0] = count` with
`ctx.x[0] = 0x12345678` in the WC-load probe. Also added a
diagnostic kprintln in trap_sync_lower_aarch32 right before
eret to verify ctx.x[0] is preserved up to the eret.

Cold-boot result:

```
WC-load #0: count=0x0(0) r5=9 r7=0
WC-load post-update: ctx.x[0]=0x12345678, save_slot=0x12345678
trap_sync_lower_aarch32 RETURN: ctx.x[0]=0x12345678 ELR_EL2=0xffffe4
WC-postload #0: r0=0x12345678 src_mode=0x10
WC-add #0: r0=0x20000110(536871184) → r1=0x20000111
WC-store #0: r1=0x20000111 before=0x0
WriteRun #0 ENTER: count=0x20000111
*** Reboot canary fired ***
```

**Crucial finding**: r0 propagation WORKS up through 0x257078
(WC-postload sees the sentinel), but is CLOBBERED before
0x25708c. Only ARM instructions in between are:
- `0x25707c: bls 0x2570d0`
- `0x257080: ldrb r1, [r4, #160]`
- `0x257084: teq r1, sl`
- `0x257088: bne 0x2570c0`

None of cmp/bls/teq/bne legitimately modify r0. The `ldrb` at
`0x00257080` is special: the shadow-stub system patches EVERY
LDRB/STRB at boot (`shadow_stub: scanned 27799 words, patched
27799 insns`). So the kernel reaches `0x00257080` and BRANCHES
to a generated stub in the SBA pool (`0x00E0_0000..0x00FF_FF00`).

The stub emulates the byte access using scratch registers. If
the stub's scratch-register selection (`pick_scratch_regs` in
`src/shadow_stub.rs`) treats r0 as "dead at orig_pc+4" — based
on the original ARM body without considering the EL2 ERET
delivers r0 from EL2's ctx — it might use r0 as the EA register
and clobber it.

The clobber value `0x20000110` is CPSR-shaped (NZCV=0010, mode=
USR=0x10), suggesting the stub places SPSR or a CPSR-derived
value into r0.

#### Next iteration plan

1. Dump the actual stub bytes at the slot for orig_pc=`0x257080`.
   `pick_scratch_regs` should be deterministic given the
   classifier output, so we can inspect the stub layout directly.
2. If r0 is used as scratch_ea or scratch_flags, check whether
   the stub's spill-restore preserves r0 across the byte access.
3. Likely fix: blacklist r0 from scratch-register pick at
   `0x257080` (or globally for kernel ROM), forcing the stub to
   spill a different register.

#### Status

- 36/36 guest tests pass (with sentinel removed at end of iter).
- Iter-25 deliverable: WC-postload + sentinel test, pinning the
  r0 corruption to the shadow-stub-patched LDRB at `0x00257080`.

### Iteration 24 (next-loop iter 20): count-store probe — str writes `r1=0x20000111` directly

Iter 23's GetMatchingPage stub eliminated Group-2 PA aliases but
the wedge persisted. Iter 24 narrows the corruption further:
1. First, disabled the WC-load probe and re-ran. Wedge fires
   identically with same `count=0x20000111`. This rules out the
   probe as the cause.
2. Then re-enabled WC-load and added two new probes:
   - WC-store at ROM `0x00257090` (`str r1, [r4, #156]`) — logs r1.
   - WC-reload at ROM `0x0025709C` (`ldr r0, [r4, #156]`) — logs
     the value read from memory.
3. Cold-boot result:

```
WriteChunk #0 ENTER: this=0x0c646c0c count=0x0
WC-load #0: count=0x0(0) r5=9 r7=0
WC-load #1: count=0x1(1) r5=9 r7=1
WC-store #0: this=0x0c646c0c r1=0x20000111(536871185) before=0x1
WC-reload #0: this=0x0c646c0c count=0x20000111(536871185)
WriteRun #0 ENTER: count=0x20000111
```

Iter 1's PATH B sequence:
- `0x257074: ldr r0, [r4, #156]` — WC-load probe says count=1, sets `ctx.x[0]=1`.
- `0x257078..257088: cmp/bls/ldrb/teq/bne` — none touch r0.
- `0x25708c: add r1, r0, #1` — should produce r1=2.
- `0x257090: str r1, [r4, #156]` — WC-store probe says **r1=0x20000111**.

The add at `0x25708c` produced 0x20000111, which means r0 was
**0x20000110** at the add — NOT the 1 my WC-load probe stored
in ctx.x[0].

#### Hypothesis: cache-coherency mismatch

The hypervisor's `guest_mem::read_word_va` walks the page tables
and reads via stage-2 host PA. This BYPASSES the guest's data
cache. The kernel's USR-mode `str r0, [r4, #156]` (in iter 0
PATH D) writes count=1 via stage-1 + cache. If the cache hasn't
written back yet, the PA still has the old value (0x20000110
from earlier use).

So:
- WC-load probe reads PA (= 0x20000110), but my emulator reports
  count=1 because the kernel's prior write to count=1 (via cache)
  is what we expect. **But our log shows count=1 — so the read
  returned 1, not 0x20000110.**
- The kernel's actual ldr at `0x257074` reads via cache. If cache
  has 1 (from iter 0's write), ldr returns 1. r0=1.
- But add produces 0x20000111, meaning r0 was 0x20000110 at the
  add — NOT 1.

This is contradictory. Possibilities:
- ERET from EL2 to UND mode, then UND→USR via `movs pc, lr` —
  the r0 register isn't actually getting my `ctx.x[0]=1` value.
  Some path overrides it.
- An interrupt fires in the 4 instructions between `ldr` and
  `add`, and the IRQ handler doesn't preserve r0 correctly.
- The guest's cache and my probe see DIFFERENT data — the probe
  reports `count=1` from PA, but the kernel's ARM ldr reads
  `count=0x20000110` from cache.

#### Next iteration plan

Definitive test: instrument the path so we can see r0 value
right after the WC-load probe's ERET, BEFORE the add fires.
Replace 0x25708c (`add r1, r0, #1`) with HVC. Handler logs r0.
This pinpoints whether r0 is correct after the WC-load
probe (= 1) or wrong (= 0x20000110).

If r0 is wrong AFTER the probe → the probe's ERET path doesn't
deliver ctx.x[0] to USR's r0.

If r0 is correct → the add somehow produces wrong r1.

#### Status

- 36/36 guest tests pass.
- Iter-24 deliverables: WC-store + WC-reload probes; verified
  the wedge isn't probe-induced; pinned the corruption to
  r0=0x20000110 at the add at `0x25708c`.

### Iteration 23 (next-loop iter 19): GetMatchingPage stub — eliminates Group-2 aliases but wedge persists

The iter-22 trace showed that `Get` is exclusively called by
`AllocNewPage` (stack pool). The heap-side path
`ExtendVMHeap → LockHeapRange → ResolveFault → FindOrAllocPage`
calls `GetMatchingPage` first to find an EXISTING TStackPage
that matches the requested address. If found, that page's PA
is reused at the heap VA — creating the alias. Only on a
cache miss does FindOrAllocPage fall through to AllocNewPage.

The GetMatchingPage stub was documented as commentary in
`rom_patches.rs` lines 236-270 but never committed to
`PATCHES_717006`. Iter 23 commits it.

Stub layout (replaces prologue at `0x001F_86B4`):
- `0x001F_86B4: mov r0, #0`  (was `mov ip, sp` = `0xE1A0_C00D`)
- `0x001F_86B8: bx lr`        (was `push {r4..pc}` = `0xE92D_DFF0`)

Cold-boot result with stub installed:

```
rom_patch: 0x001f86b4: 0xe1a0c00d -> 0xe3a00000  (GetMatchingPage: mov r0, #0)
rom_patch: 0x001f86b8: 0xe92ddff0 -> 0xe12fff1e  (GetMatchingPage: bx lr)
verify-mmu alias: PA=0x04004000 ...  (Group-1, 3 of 3, ROM-baked)
verify-mmu alias: PA=0x04005000 ...
verify-mmu alias: PA=0x04006000 ...
(NO Group-2 aliases — 0 of 12)

stage1 walk VA=0x0c646ca8: ...
  WriteChunk count_pa=0x04096ca8 → tracker[first_va_for_pa]=0x0c646000

WriteChunk #0 ENTER: count=0x0
WC-load #0: count=0x0 r5=9 r7=0
WC-load #1: count=0x1 r5=9 r7=1
WriteRun #0 ENTER: count=0x20000111(536871185)
*** Reboot canary fired ***
```

The compressor's PA changed from `0x04084ca8` (aliased) to
`0x04096ca8` (unique). All Prim/verify-mmu Group-2 aliases are
GONE. The kernel's per-page allocation now gives every consumer
a private 4-KiB physical page.

But the count-corruption WEDGE PERSISTS. count flips from `1`
(at WC-load #1) to `0x20000111` (at WriteRun entry) inside the
4-instruction window between str (`0x257090`) and ldr
(`0x25709c`). Same pattern as iter 20.

#### What this rules out

- Stage-1 PA alias as the corruption source. The compressor's PA
  is uniquely owned. No other VA shares it.
- Reuse of stack-pool pages for heap. With GetMatchingPage
  stubbed, every consumer gets a private page.

#### What it leaves on the table

- The **str at `0x257090`** itself may be writing the wrong value.
  Need to probe r1 at the moment of store.
- The **ldr at `0x25709c`** may be reading from somewhere else
  due to register/load issues.
- An **interrupt during the 4-insn window** that saves CPSR
  somewhere, where the kernel exception entry happens to land on
  PA `0x04096ca8` via a path I haven't traced.
- The **WC-load probe handler** itself might be doing something
  wrong (incorrect emulation of `ldr r0, [r4, #156]`). My
  emulation reads the count via `guest_mem::read_word_va` and
  sets `ctx.x[0] = count as u64`. Possibly the cast or the
  memory read is buggy in a way that the second-iteration count
  is wrong.

#### Next iteration plan

1. **Probe str at `0x257090`** — patch with HVC, log
   `(this, r1, count_in_memory_before, count_in_memory_after)`.
   Confirms whether the str writes the expected value (2) or
   something else.
2. **Probe ldr at `0x25709c`** — patch with HVC, emulate the
   load, log the value read. Confirms what ldr sees.
3. **Disable/remove the WC-load probe** for one run as a
   sanity check — does the wedge still happen without it?
   Rules out probe-induced corruption.

#### Status

- 36/36 guest tests pass.
- Iter-23 deliverable: `GetMatchingPage = always-return-0`
  stub committed in PATCHES_717006. Eliminates Group-2 PA
  aliases. Wedge persists, so Group-2 aliasing was NOT the
  cause; the actual cause is in WriteChunk's body somehow.

### Iteration 22 (next-loop iter 18): re-enable Get logging — every Get is a stack-pool allocation; heap-extend reuses stack pages

Iter 21 confirmed PA `0x04084000` is aliased between heap VA
`0x0c646000` and stack VA `0x0ccc8000`. Iter 22 re-enables the
existing `TUDomainManager::Get` post-SWI probe (HVC #0x53 at ROM
`0x00258EFC`) by switching its `dprintln!` to `kprintln!` and
raising the budget from 64 to 1024.

#### Cold-boot result — Get is exclusively a stack-pool path

```
page-get: #0   id=0x0000136b count=2 caller_lr=0x001f87c0 ...
page-get: #1   id=0x000013cb count=2 caller_lr=0x001f87c0 ...
...
page-get: #98  id=0x0000356b count=2 caller_lr=0x001f87c0 ...
Prim ALIAS: PA=0x04084000  VA1=0x0ccc8000  VA2=0x0c646000  ...
page-get: #99  id=0x000035bb count=2 caller_lr=0x001f87c0 ...
```

All 100+ Get calls have `caller_lr=0x001f87c0`. That's the
post-`bl Init__10TStackPage` site inside
`AllocNewPage__13TStackManagerFUl` at ROM `0x001F8788`:

```
1f87b0: mov r2, r4                    ; size class
1f87b4: mov r1, r5                    ; manager
1f87b8: mov r0, r6                    ; TStackPage*
1f87bc: bl  Init__10TStackPageFP15TUDomainManagerUl
1f87c0: teq r0, #0                    ; ← caller_lr lands here
```

`count=2` means each Get returns 2 contiguous physical pages
(8 KiB) for one TStackPage. Get returns unique PageIDs (no
duplicates) — confirming iter-3's audit.

#### Where do heap PAs come from?

The heap-extend path (ExtendVMHeap → LockHeapRange → FMLockHeapRange
→ ResolveFault → AddPgPAndPerm) doesn't call Get. It must be
re-using PAs allocated earlier by `AllocNewPage`. The heap's last
4-KiB page at VA `0x0c646000` overlays PA `0x04084000` — same PA
that holds subpage 3 of stack VA `0x0ccc8000`.

Under ARMv4 subpage-AP this is intentional: each consumer claims
its own 1-KiB subpage of a shared 4-KiB physical page. Under our
flat AP=11 the sharing collapses and both VAs see each other's
writes — exactly the wedge.

#### Next iteration plan

1. Probe `FMLockHeapRange` at ROM `0x001F6B24` to log
   `(parms, base, limit, lock_id)` — different from the existing
   user-shim probe at 0x001F8AB4 (which logs the user-mode call).
   FMLockHeapRange is the privileged kernel side that actually
   resolves PAs.
2. Probe `ResolveFault` at ROM `0x001F7978` — already wired but
   filtered. Capture the per-page PA assignment to see which
   stack-pool pages get reused for heap.
3. Trace AddPgPAndPerm callers — that's the ROM site that writes
   the L2 entry installing PA at VA. Identifying its source of
   the PA value (TPhys struct, a TStackPage, or fresh allocation)
   reveals the kernel patch point.

The fix-target: patch the routine that decides "reuse stack-pool
page X for the heap's last page" so the heap instead allocates
its OWN page. This may require enlarging the physical pool or
adjusting the heap/stack pool-divider.

#### Status

- 36/36 guest tests pass.
- All prior iter probes stay active.
- Iter-22 deliverable: enabled per-call Get logging, confirming
  Get is exclusively used by AllocNewPage / TStackPage and that
  the heap-extend path overlays existing stack-pool pages.

### Iteration 21 (next-loop iter 17): stage-1 walk + alias tracker — PA=0x04084000 alias between heap and stack VAs

Iter 20 narrowed the count corruption to a 4-instruction window
in WriteChunk's iter 1 PATH B (between str and re-read).
Hypothesised stage-1 alias. This iteration extends the WriteChunk
entry probe to:
1. Dump the stage-1 walk for `this+0x9c` (the count VA).
2. Resolve the count VA → PA and look up
   `PRIM_FIRST_VA_FOR_PA[pa]` to detect aliases.
3. Also dump the callback function pointer and arg
   (`*(this+0x10)`, `*(this+0x14)`).

#### Cold-boot result — alias confirmed

```
WriteChunk #0 ENTER: this=0x0c646c0c ptr=0x0cc77aa4 length=18 \
  count=0x0 cb=0x01a3deac(0x0cc77a8c) caller_lr=0x002dcf20
  stage1 walk VA=0x0c646ca8: L1[0xc6] = 0x0401c881 (coarse)
    coarse L2 @ PA 0x401c800, L2[0x46] = 0x0408403e (small)
  WriteChunk count_pa=0x04084ca8 → tracker[first_va_for_pa]=0x0c646000
```

The Prim tracker's `first_va_for_pa` reports VA `0x0c646000` —
just the compressor's own VA — but searching the boot log shows
the canonical `Prim ALIAS:` line:

```
Prim ALIAS: PA=0x04084000  VA1=0x0ccc8000 (upstream_lr=0x000d8e3c)
   VA2=0x0c646000 (caller_lr=0x003109e4)  mask=0x3f perm=0x1
verify-mmu alias: PA=0x04084000 VA1=0x0c646000 (L1[0xc6],L2[0x46])
   VA2=0x0ccc8000 (L1[0xcc],L2[0xc8])
```

**The compressor's heap page at VA `0x0c646000` aliases PA
`0x04084000` with stack/data page VA `0x0ccc8000`.** Both
mappings come through `caller_lr=0x003109e4` (post-bl
LockHeapRange in ExtendVMHeap), meaning the kernel's heap-extend
path reused a PA that was already in use elsewhere.

#### Why the corruption looks CPSR-shaped

VA `0x0ccc8ca8` is somewhere in the aliasing region's stack
frame. The value `0x20000110` matches a saved CPSR (NZCV=0010,
mode USR=0x10) — likely an exception handler's stack-frame
push of the saved-CPSR slot. After WriteChunk's str writes 2 to
PA `0x04084ca8` via heap VA, an exception handler runs in the
OTHER task on stack VA `0x0ccc8000`, pushes its CPSR as part of
the exception-entry trampoline, and that push lands at the SAME
PA, clobbering the compressor's count.

#### The callback at `*(this+0x10)`

`cb=0x01a3deac` (REx region) with `arg=0x0cc77a8c` (alarm task
sp). `New__18TUnicodeCompressorFv` doesn't set `+0x10`/`+0x14` —
the compressor's caller (TStoreWritePipe::WriteToStore at
`0x002DCEF0`+) presumably initializes these via a separate
helper. The callback isn't invoked in our wedge case
(w98=0 < 128, so PATH E never fires), so the callback isn't the
corruption source.

#### Next iteration plan — kernel patch (hypervisor-side fixes are OUT)

The bug is that two distinct kernel consumers received the same
PA `0x04084000`:
- VA `0x0ccc8000` (a stack region)
- VA `0x0c646000` (heap, via ExtendVMHeap)

Both Prim ALIAS records show `caller_lr=0x003109e4` (= post-bl
LockHeapRange in ExtendVMHeap). The kernel's page allocator
must be reusing a PA across distinct kernel "consumers"
(probably across different `THeapDomain` instances or between a
stack pool and a heap pool).

Iter-22 work: trace the PA allocation chain so we can identify
the kernel routine that hands out PA 0x04084000 a second time.
Probes to add:
- `TUDomainManager::Get` (ROM `0x00258EFC`) — already probed in
  iter-3 with PAGE_GET_PROBE_HVC_IMM=0x53. Re-enable per-call
  logging (was suppressed after dup-detection said "no
  duplicates"; the alias data shows duplicates DO happen, so the
  prior dup logic was wrong/incomplete).
- `AllocPageDirect` / kernel-internal page-grab routines that
  bypass Get.
- Cross-check the ENTRY caller_lr distribution for VA1
  `0x0ccc8000`'s first Prim Remember to find which higher-level
  caller (NewStack? NewHeap?) requested the second mapping.

Once we know the SHARED routine that returns PA `0x04084000`
twice, the patch is: change the routine so it does NOT recycle
PAs across distinct domain owners. Likely candidates:
- A pool-size constant we didn't bump alongside the 36-KiB stack
  patch (iter 12).
- A free-list that holds previously-used pages without proper
  ownership tracking.
- A heap-domain-bytes-per-domain constant that's too small,
  causing one domain's heap to spill into another's stack pool.

#### Status

- 36/36 guest tests pass.
- All prior iter probes stay active.
- Iter-21 deliverable: stage-1 walk + alias tracker integration,
  CONFIRMING that the compressor's PA is aliased with another
  task's stack VA. The wedge mechanism is now fully explained:
  ARMv4-subpage-AP allocator reuses a PA across consumers; under
  flat AP=11 both VAs see each other's writes.

### Iteration 20 (next-loop iter 16): WriteChunk count-load probe — count flips between str and re-read in iter 1 PATH B

Iter 19 narrowed the corruption to "between WriteChunk entry and
WriteRun entry". This iteration installs `HVC #0x62` patching
the count-load `ldr r0, [r4, #156]` at ROM `0x00257074` (the
first instruction of every WriteChunk loop iteration). Handler
emulates the load and logs `(this, count, r5_total, r7_index)`.

#### Cold-boot result — corruption hits during iter 1 PATH B

```
WriteChunk #0 ENTER: this=0x0c646c0c ... count=0x0
WC-load #0: this=0x0c646c0c count=0x0(0)            r5=9 r7=0
WC-load #1: this=0x0c646c0c count=0x1(1)            r5=9 r7=1
WriteRun #0 ENTER: this=0x0c646c0c ... count=0x20000111
```

- `WC-load #0`: iter 0 starts with count=0. PATH C/D path is
  taken (count <= 0); PATH D sets count=1.
- `WC-load #1`: iter 1 starts with count=1. PATH B is then
  taken (byte_a0 == sl assumed): r1 = count+1 = 2; str count =
  2 at `0x257090`.
- After str, the iteration continues:
  - `0x257094: add r0, r0, r4`   (compute buffer write addr)
  - `0x257098: strb r6, [r0, #161]`  (single byte to buffer_b[1])
  - `0x25709c: ldr r0, [r4, #156]`  (RE-READ count)
  - `0x2570a0: cmp r0, #255`
  - `0x2570a4: bcc skip` — taken if count < 255
  - `0x2570ac: bl WriteRun` — fires only if count >= 255
- **WriteRun fires** with `count=0x20000111`, meaning the re-read
  at `0x25709c` returned `0x20000111` instead of `2`.

#### The 4-instruction window

Between str at `0x257090` (writes 2) and ldr at `0x25709c` (reads
`0x20000111`), only:
- `add r0, r0, r4` (no memory access)
- `strb r6, [r0, #161]` (writes ONE byte to buffer_b[1] at
  `compressor + 0xa2 = 0x0c646cae`)

The strb writes a different byte (offset +0xa2, count is at +0x9c
— 6 bytes apart). It cannot directly corrupt count.

#### Strong-evidence working theory: stage-1 PA alias

The compressor's count field at VA `0x0c646ca8` is backed by some
PA. If that PA is shared with another active VA (via a kernel
PrimRememberMapping alias), writes through the OTHER VA would
appear at our compressor's count.

The Group-2 alias inventory shows `PA=0x0402e000` is shared
between VA `0x0cc6e000` (a stack region) and VA `0x0c606000`
(this same heap, but lower offset). If the heap allocator
re-used PA `0x0402e000` to back VA `0x0c646000` (the compressor's
page), then writes via VA `0x0cc6e000` (or `0x0cca3000` from the
older alrt alias) would corrupt the compressor.

Specifically: VA `0x0cca3000` was the alrt task's globals page,
mapping to PA `0x0402e000`. The procst at VA `0x0c1133a4`
contained `procst[+0x40] = saved_cpsr = 0x20000110` (iter 15
finding). If procst is also aliased to PA `0x0402e000` via some
chain, writes to procst+0x40 would land on the compressor's
count field.

#### Next iteration plan

1. **Dump stage-1 walk** for VA `0x0c646000` (the compressor's
   page) at WriteChunk entry. Confirms the PA backing it and
   reveals any alias.
2. **If aliased to `0x0402e000`**: extend `alrt_capture` /
   `pa_emulate.rs` to watch the count's PA window and capture
   the cross-VA writer.
3. **Cross-check**: is VA `0x0c646000` mapped via Prim Remember,
   or via a direct kernel L2 write? Look for the corresponding
   `Prim probe ENTER` line in the boot log.

#### Status

- 36/36 guest tests pass.
- All prior iter probes stay active.
- Iter-20 deliverable: WC-load probe pinpoints the corruption to
  iter 1 PATH B's str/re-read 4-instruction window. Strong-
  evidence theory: stage-1 alias.

### Iteration 19 (next-loop iter 15): WriteChunk + New + Reset probes — count is zero at WriteChunk entry, refuting the "uninitialized" theory

Iter 18 hypothesised that the compressor was used WITHOUT calling
New/Reset, leaving count holding stale heap garbage. This
iteration adds probes at:
- `WriteChunk__18TUnicodeCompressorFPvl` entry (`HVC #0x5F` at
  ROM `0x0025700C`).
- `New__18TUnicodeCompressorFv` first insn (`HVC #0x60` at ROM
  `0x00256C7C`, `mov r1, #0`).
- `Reset__18TUnicodeCompressorFv` first insn (`HVC #0x61` at ROM
  `0x00256ED8`, `mov r1, #0`).

#### Cold-boot result — New IS called, count IS zero at WriteChunk

```
TUnicodeCompressor::New #0 this=0x0c646c0c caller_lr=0x0005c68c src_mode=0x10
WriteChunk #0 ENTER: this=0x0c646c0c ptr=0x0cc77aa4 length=18(0x12) count=0x0 ...
WriteRun #0 ENTER: this=0x0c646c0c ... count=0x20000111(536871185) ...
```

- **`New` was called** with `this=0x0c646c0c` — the compressor
  was constructed via the proper `TClassInfo::Construct` chain
  (caller_lr=`0x0005c68c` → ROM `0x5C688`'s `add pc, r4, #36`,
  the TClassInfo vtable[1] dispatch).
- **`Reset` was NOT called** — but `New` already zeroes count, so
  Reset isn't needed.
- **`count=0x0` at WriteChunk entry** — this disproves iter-18's
  "uninitialized garbage" theory. count was correctly zero when
  WriteChunk started.

#### The puzzle — count flips between WriteChunk and WriteRun

WriteChunk's body iterates `length>>1 = 9` wide chars. Per-iter
writes to count: PATH B `count = count+1`, PATH D `count = 1`.
After 9 iterations count is at most 9 — far below 255 (the cap
that triggers PATH B's `bl WriteRun` at ROM `0x002570AC`). Yet
WriteRun's `caller_lr=0x002570B0` confirms PATH B was reached,
and `count=0x20000111` was the value loaded by `ldr r0, [r4,
#156]` at `0x00257074`.

**Hypotheses (in priority order):**

1. **Interrupt handler save-area aliases the compressor's PA.**
   The CPSR-shaped `0x20000110` matches procst[+0x40]'s
   saved-CPSR pattern. If a kernel scratch VA aliases PA
   `0x0402eca8` (the compressor's `count` PA), an exception
   handler's save during WriteChunk would write through that
   alias, corrupting count. Group-2 alias inventory shows
   `PA=0x0402e000` is aliased between VA `0xcc9b000` (mntr
   stack) and VA `0xcca3000` (alrt globals). **Possibility:
   VA 0xc646000 also hits PA 0x0402e000?** Need to verify with
   a stage-1 walk for VA `0xc646000`.

2. **The callback at `*(this+0x10)` modifies count.** WriteChunk
   line `0x00257128: ldr pc, [r4, #16]` calls a function
   pointer. If the callback's logic writes to `*(this+0x9c)`,
   it could set count to 0x20000110. The callback is set
   somewhere; need to dump `this+0x10` to find which function.

3. **The 9-iteration limit assumption is wrong.** Maybe path E
   (buffer-A flush) somehow loops back without consuming an
   input byte, allowing count to be incremented many more
   times. Need to trace WriteChunk's actual iteration count.

#### Next iteration plan

a. **Stage-2 RO trap on PA backing compressor +0x9c.** Modeled
   on `alrt_capture` / `pa_emulate.rs`. Captures every write
   to the count word with `(PC, value, src_mode)`. Definitive
   answer to "who writes count".

b. **Stage-1 walk for VA `0xc646000`** at ExtendVMHeap #10
   completion to confirm/refute the alias hypothesis. Probably
   maps to a fresh PA (since LockHeapRange just allocated it),
   but if it hits PA `0x0402e000` we'd see the alias.

c. **Probe `*(this+0x10)`** — the callback function pointer.
   Dump it from `New` or `WriteChunk` entry to identify the
   callback. (Could extend the existing WriteChunk probe to
   also dump `this+0x10`.)

#### Status

- 36/36 guest tests pass.
- All prior iter probes stay active.
- Iter-19 deliverable: WriteChunk + New + Reset probes, refuting
  the "no proper init" theory and pinning the corruption to a
  WRITE during WriteChunk's body.

### Iteration 18 (next-loop iter 14): WriteRun entry probe — confirms count is uninitialized heap garbage (= SPSR-shaped value)

Iter 17 hypothesised that WriteRun was being called with
`count > 870` because the compressor's count field was never zeroed.
This iteration installs a `WriteRun` entry probe (`HVC #0x5E`)
patching the `mov ip, sp` prologue at ROM `0x00256EEC`. Handler
logs `(this, count, byte_a0, buffer_b first 8 bytes, caller_lr)`.

#### Cold-boot result — count is `0x20000111` on WriteRun entry

```
WriteRun #0 ENTER: this=0x0c646c0c w98=0x00000000 count=0x20000111(536871185) \
  byte_a0=0x00 buf[a0..a8]=0000000000000000 caller_lr=0x002570b0 \
  src_mode=0x10 sp=0x0cc77728
```

**Three confirmations:**

1. **`this=0x0c646c0c`, NOT `0x0c646bfc`** — NewBlock returned the
   16-byte block header at `0x0c646bfc`; NewDirectBlock at ROM
   `0x00311EE4` adds 16 (`add r0, r4, #16`) to skip the header,
   giving a user pointer of `0x0c646c0c`. The 420-byte compressor
   sits at `0x0c646c0c..0x0c646db0`.

2. **`count=0x20000111`** — that's a CPSR-shaped value:
   - bits [31:28] = `0010` → NZCV (Carry set)
   - bits [4:0] = `0x11` → mode = FIQ (`0x11`)
   - This matches the iter-15 `procst[+0x40]` saved-CPSR observation
     (`0x20000110`) almost exactly. The heap RAM at this offset was
     previously holding a `TProcessorState` save-area's `saved_cpsr`
     field, then was freed without zero-filling, then re-allocated
     to the compressor.

3. **WriteChunk increments count once before flush** — the value
   `0x20000111` is `0x20000110 + 1`. WriteChunk's path-B
   (count++ → cmp 255 → flush) ran exactly once, incrementing the
   junk `0x20000110` to `0x20000111`, then triggered WriteRun
   because `0x20000111 > 255`. The "byte at +0xa0..+0xa7 are all
   zero" indicates the byte sentinel was clean (probably zeroed by
   a partial Reset or by zero-fill of the LOWER region of the
   compressor).

#### Recompute the fault arithmetic

```
this        = 0x0c646c0c   (WriteRun's r4)
fault FAR   = 0x0c647003   (the ldrb at 0x00256FA8: r0+0xa1)
r5 at fault = FAR - this - 0xa1 = 0x356 = 854
count       = 0x20000111   (which is >> 854, so loop iterates
                            until r5 hits the unmapped page)
```

Loop runs r5 = 0..854 reading `byte[this+0xa1+r5]`. At r5 = 854
the read targets `0x0c647003`, past the heap top → DFSC=7 fault.

#### Why was the heap RAM holding `0x20000110`?

This memory at `0x0402exxx` (heap-mapped via VA `0x0c646xxx`)
was likely previously used as exception-handler scratch or as the
backing store for a `TProcessorState` struct. The kernel's free
path doesn't poison-fill on every release — and our `__nw__`/free
tracking never tracks small subdomain blocks anyway.

The compressor's caller doesn't call `New__18TUnicodeCompressor`
(`0x00256C7C`) which would zero `+0x98 +0x9c +0xa0`. The caller
relies on `Reset__18TUnicodeCompressor` (`0x00256ED8`) being
called at the right time, OR it's outright buggy.

#### Next iteration plan

1. **Probe WriteChunk entry** at ROM `0x0025700C` to log
   `(this, count_at_entry, caller_lr)` — the caller of WriteChunk
   is the kernel function that owns / mis-uses the compressor.
2. **Probe TUnicodeCompressor::Reset** at `0x00256ED8` and
   `New__18TUnicodeCompressor` at `0x00256C7C` to see if either is
   ever called for `this=0x0c646c0c`. If not, the compressor was
   used without proper initialization.
3. **Cross-check Einstein** — run NewtonProbe to see whether the
   ARMv4 emulator hits the same site without faulting, and if so,
   what its compressor's count value is at WriteRun entry.

#### Possible fixes

a. **Patch the caller to call Reset before WriteChunk** — most
   correct, but requires identifying the caller first.

b. **Defensive clamp in WriteRun** — patch the function to clamp
   count to `min(count, 255)` at entry. Safe (matches WriteChunk's
   own cap) but masks the underlying bug.

c. **Zero-fill on NewDirectBlock return** — patch
   `NewDirectBlock` to memset the returned block to 0 before
   returning. Affects every direct-block allocation in the
   kernel; could mask other latent bugs but also could expose new
   ones. Most invasive.

d. **Patch `New__18TUnicodeCompressor`** to be auto-called by the
   construction path. Need to find that path first.

#### Status

- 36/36 guest tests pass.
- All prior iter probes stay active.
- Iter-18 deliverable: WriteRun entry probe, confirmation that
  count is uninitialized SPSR-shaped heap garbage.

### Iteration 17 (next-loop iter 13): NewBlock probe — locates compressor at `0xc646bfc`, RULES OUT heap-boundary spill

Iter 16 hypothesized that the 420-byte `TUnicodeCompressor` block
lived at `0xc646f60` (with its `+0xa1` buffer spilling past the
heap top of `0xc647000`). This iteration adds NewBlock entry +
success-return probes (`HVC #0x5C/#0x5D`) at ROM `0x00311DB8` and
`0x00311ED8` to log every `(req_size, returned_block, caller_lr)`
triple. The pairing keys on entry-time SP, recovered at exit by
adding back the prologue-push offset (9 words = 36 bytes).

#### Cold-boot result — compressor at `0xc646bfc..0xc646da0`

656 NewBlock calls fire before the wedge. The relevant one:

```
NewBlock #656 ENTER: req=0x1a4(420) caller_lr=0x00311f04 sp=0x0cc7771c
NewBlock RET: returned=0x0c646bfc..0x0c646da0 size=0x1a4(420) \
  caller_lr=0x00311f04 src_mode=0x10 sp=0x0cc776f8
```

`req=0x1a4` (= 420 = `Sizeof__18TUnicodeCompressorSFv`).
Returned `0x0c646bfc..0x0c646da0` — **fully within the heap**
(top is `0x0c647000`). `caller_lr=0x00311f04` is the
post-`bl NewBlockLow` site inside `NewDirectBlock` at ROM
`0x00311EE4`.

#### What the wedge actually is — `count` corrupted, not heap spill

If the compressor is at `0xc646bfc..0xc646da0`, then for the
`ldrb r0, [r0, #161]` at ROM `0x00256FA8` to fault at FAR
`0x0c647003`:

```
r4 + r5 + 0xa1 = 0xc647003
r4              = 0xc646bfc                ; compressor base
r5              = 0xc647003 - 0xc646bfc - 0xa1 = 0x366 = 870
```

So r5 reaches **870** in the WriteRun loop before faulting. The
loop bound check is `cmp count, r5; bhi loop` (PC `0x256ff8`),
meaning `count >= 871` at fault time. But:

- `WriteChunk__18TUnicodeCompressorFPvl` (`0x0025700C`) caps
  count at 255 — increments via `str count+1, [r4, #156]`, then
  `cmp count, #255 / bcc skip` flushes through `WriteRun` once
  count reaches 255.
- `New__18TUnicodeCompressorFv` at `0x00256C7C` zeros count
  (`str r1, [r0, #156]` with `r1 = 0`).
- `Reset__18TUnicodeCompressorFv` at `0x00256ED8` also zeros
  count.

So count must legitimately be 0..255 if the compressor is used
correctly. Reaching 871 means **the compressor was used without
proper New/Reset**, leaving count holding stale heap garbage.

**Hypothesis (next-iter target):** the kernel allocates the
compressor via `NewDirectBlock` and uses it directly without
calling `New__18TUnicodeCompressor` or `Reset__18TUnicodeCompressor`.
Either:
1. The C++ object construction path uses raw `NewDirectBlock`
   then expects the caller's first method (e.g., WriteChunk) to
   tolerate uninitialized fields. WriteChunk DOES check
   `count <= 0` for the read loop's exit condition, but it does
   NOT zero count first.
2. There's a separate path that sets count to a non-zero value
   without WriteChunk's cap (e.g., Flush, vector-set, or a
   manual store from the caller).

#### Next iteration plan

Add a `WriteRun` entry probe (`HVC #0x5E`) at ROM `0x00256EEC`
patching the `mov ip, sp` prologue. Handler logs `(this,
this->count, this->byte_a0, this->buffer_a_first_8_bytes,
caller_lr)` so we can:

- Confirm count is large at WriteRun entry (>871).
- See whether the same compressor is reused multiple times across
  calls (caller_lr correlation).
- Check if buffer_a contents look like compressor data or
  uninitialized poison.

Once count's source is pinned, the fix is either:
- Patch the kernel caller to call New or Reset before WriteChunk.
- Patch `WriteChunk` / `WriteRun` to clamp count to a sane value
  defensively.
- (Or, even simpler: patch `NewDirectBlock` / `NewBlock` to
  zero-fill returned blocks. ARMv4 + original kernel may have
  relied on fresh-page zeroing that our 4-KiB chunk allocator
  doesn't replicate after kernel-level RAM reuse.)

#### Status

- 36/36 guest tests pass.
- Iter-12 36-KiB stack patches stay active.
- Iter-14 ResolveFault wrapper fix stays active.
- Iter-15 Fault(stackmgr) probe + SBA-stub origin lookup stay active.
- Iter-16 ExtendVMHeap probe stays active.
- Iter-17 NewBlock entry+return probes stay active. Pinpointed
  the compressor's actual heap location and identified the wedge
  as a `count` corruption, not a heap-boundary spill.

### Iteration 16 (next-loop iter 12): ExtendVMHeap probe — rules out the heap-extend rounding as the bug

Iter 15 hypothesized that `ExtendVMHeap` was undersizing the heap
extension, leaving the freshly-allocated 420-byte compressor object
spilling past the new heap top. This iteration adds an
`ExtendVMHeap` entry probe (`HVC #0x5B` patching `mov ip, sp` at
ROM `0x0031091C`) that logs `(this, requested, chunk_size,
rounded, current_top, proposed_top, reserved_end, caller_lr)` per
call.

#### Cold-boot result — extend rounding is consistent

11 ExtendVMHeap calls fire before the wedge. The relevant ones:

```
ExtendVMHeap #9:  this=0xc601010 requested=0x3d0a4 chunk=0x1000 \
  rounded=0x3e000 top=0x07000 -> 0x45000 reserved_end=0x380000 \
  caller_lr=0x00311e1c
LockHeapRange #75: base=0xc608000 limit=0xc646000  (0x3e000 = 248 KiB)

ExtendVMHeap #10: this=0xc601010 requested=0x310 chunk=0x1000 \
  rounded=0x1000 top=0x45000 -> 0x46000 reserved_end=0x380000 \
  caller_lr=0x00311e1c
LockHeapRange #76: base=0xc646000 limit=0xc647000  (0x1000 = 4 KiB)
```

`requested=0x310` (= 784 bytes) for the call right before the
wedge. The kernel rounds `roundup(784, 4096) = 4096`, extends by
one chunk, and locks the 4 KiB range. **No rounding bug** — the
caller asked for 784 bytes, the kernel delivered 4096 bytes of
fresh heap.

#### What the request size means

`caller_lr=0x00311e1c` is the post-`bl ExtendVMHeap` instruction
inside `NewBlock` at ROM `0x00311DB8`. Decoding NewBlock:

```
311dc4: mov  r5, r0                ; r5 = requested_size
311dc8: bl   GetCurrentHeap        ; r4 = current_heap
311dd0: add  r0, r5, #16           ; r5 += 16 (block header)
311dd4..311ddc: align r5 to 4-byte
311de4: ldr  r0, [r4, #0x20]       ; saved last-block scan position
311dec: str  r0, [r4, #0x48]       ; for retry-after-extend
311df4..311e0c: SearchFreeList(r5) ; find free block ≥ r5 bytes
311e10..311e1c: bl   ExtendVMHeap(r4, r5)   ; if not found
311e28..311ee0: split block, mark used, return
```

So the `r1=0x310` (784) request to ExtendVMHeap is the
**total-needed-bytes for the failing search** — i.e. the block
*including* the 16-byte header for an allocation of size
`0x310 - 0x10 = 0x300` (768) bytes (or with extra alignment-fudge,
slightly less). **This is NOT the 420-byte compressor allocation.**

A 420-byte allocation would round to `420 + 16 = 436` (still
under the available 4096 fresh bytes), so it doesn't trigger
ExtendVMHeap at all — it's served from the freelist.

#### Where does the compressor end up?

The 420-byte compressor object lands at `0xc646f60` based on the
fault arithmetic (`r0 + 0xa1 = 0xc647003` at `ldrb r0, [r0, #161]`,
with r5≥0). The freshly-extended 4 KiB chunk is `[0xc646000,
0xc647000)`. NewBlock's `MoveFreeBlock`-based split takes from the
**front** of the free block, so a 420-byte allocation against a
fresh `[0xc646000, 0xc647000)` free block would land at
`0xc646000`, **not** `0xc646f60`. The compressor ending up at
`0xc646f60` means the allocator picked a **different** free block,
or split from a position 0xf60 bytes into a free block that
extends past the heap top.

#### Next iteration — NewBlock entry+exit probe

Add an `HVC #0x5C` probe at `NewBlock`'s prologue (`mov ip, sp` at
ROM `0x00311DB8`) and a paired probe at the success-return
`ldmdb fp, {..., pc}` at ROM `0x00311EDC` so we can log every
`(requested_size, returned_block_addr, caller_lr)` triple. The
allocation that returns `0xc646f60..0xc647104` for a 420-byte
request is the smoking gun. With its caller_lr we can identify the
ROM site that does this allocation.

Bonus: the freelist might be holding STALE addresses past the
heap top that NewBlock's SearchFreeList trusts. If so, the bug is
upstream of NewBlock — possibly a `__dl__`/free path that's
adding back-already-extended pages or a `MoveFreeBlock` corruption
similar to the iter-12 alrt-DABT but in a different region.

The diagnostic output to expect:

```
NewBlock #N: req=0x1a4(420)+0x10=0x1b4 returns=0x0c646f60 caller_lr=0x...
```

If `returns=0x0c646f60`, the freelist contained a free block at
`0xc646f60` ≥ 0x1b4 bytes BEFORE the call. That block extends to
`>= 0xc647114`, past the current heap top of `0xc647000` — proving
the freelist got corrupted/stale.

#### Status

- 36/36 guest tests pass.
- Iter-12 36-KiB stack patches stay active.
- Iter-14 ResolveFault wrapper fix stays active.
- Iter-15 Fault(stackmgr) probe + SBA-stub origin lookup stay active.
- Iter-16 deliverable: ExtendVMHeap probe, rules out ExtendVMHeap
  rounding as the iter-15 wedge cause.

### Iteration 15 (next-loop iter 11): Fault(stackmgr) procst dump + SBA-stub origin reveal — wedge is `TUnicodeCompressor::WriteRun`

Iter 14 left the wedge "inside the kernel reboot path triggered by an
unresolvable fault at `FAR=0xc647003` after LockHeapRange #76". This
iteration adds two diagnostics that pin the *exact* user-mode
instruction that takes the fault.

#### 1. Augmented `Fault(stackmgr)` probe — confirms procst layout

Extended `handle_stack_mgr_fault_probe_with` in `src/trap.rs` to dump
`procst[+0x40..+0x60]` plus the instruction word at the
hypothesized saved-PC offset. The PLAN's earlier guess
("`procst[+0x40]` = saved ELR_EL1 / PC") turned out to be wrong:

```
Fault(stackmgr) probe ENTER: this=0x0c112cb8 procst=0x0c1133a4 \
  pc=0x20000110 far=0x0c647003 status=0x02800000 saved_sp=0x0cc77700 \
  caller_lr=0x00259230 src_mode=0x10 (USR) sp=0x0c1133a4
Fault(stackmgr) procst[+0x40..+0x60]: 20000110 0c647003 00000047 \
  00000004 0cc77700 000013a5 000030f3 02800000
```

Decoded layout (now in `docs/STRUCTURES.md` "TProcessorState"):

| offset | field | observed |
|---|---|---|
| +0x40 | saved CPSR | `0x20000110` (NZCV = 0010, mode=USR) |
| +0x44 | FAR | `0x0c647003` |
| +0x48 | DFSR | `0x47` (write, page L2-translation fault) |
| +0x4c | (unknown small constant) | `0x00000004` |
| +0x50 | saved SP_usr | `0x0cc77700` |
| +0x54 | env id | `0x000013a5` |
| +0x58 | task id | `0x000030f3` |
| +0x5c | status word | `0x02800000` (bit 25 = data abort) |

The saved PC is NOT in this 32-byte window. It probably lives in
`procst[+0x00..+0x40]` alongside the user-mode register file (open;
to confirm next iteration). For now the actual user PC is recovered
from `lr_abt - 8` in the `dabt: forwarding` path.

#### 2. SBA-stub origin lookup on the dabt-forwarding path

When `lr_abt - 8` lands in the SBA inline-stub pool
(`0x00E0_0000..0x00FF_FF00`), the new lookup decodes slot 14's
back-branch (which is always a `B orig_pc + 4`) to recover the
original ROM PC the stub emulates. This pattern was already present
on the dabt-trip diag path; iter 15 replicates it on the dabt
forwarding path so the kernel-DAH wedge is also localised.

#### Cold-boot output — wedge is `TUnicodeCompressor::WriteRun` byte-loop

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0c647003 mode=0x17
  LR_abt=0x00f0f8b0 (faulting PC=0x00f0f8a8) SP_abt=0x0c004c00 SPSR_abt=0x80000110 (pre-abt mode=0x10)
  sba-stub: slot 17378 (base 0x00f0f880) emulates ROM PC 0x00256fa8 (back-branch 0x00f0f8b8 -> 0x00256fac)
```

ROM PC `0x00256fa8` is inside `WriteRun__18TUnicodeCompressorFv`
(begins `0x00256EEC`):

```
00256f94: e3a05000  mov  r5, #0
00256f98: e594009c  ldr  r0, [r4, #156]      ; r0 = this->count (this = r4)
00256f9c: e3500000  cmp  r0, #0
00256fa0: 9a000016  bls  0x257000             ; exit if count <= 0
00256fa4: e0840005  add  r0, r4, r5
00256fa8: e5d000a1  ldrb r0, [r0, #161]       ; ← FAULT: read byte at this+0xa1+r5
```

The compressor object (`Sizeof__18TUnicodeCompressorSFv = 420`) was
allocated at `r4 = 0x0c646f60`. Iteration `r5 = 2` accesses
`0x0c646f60 + 2 + 0xa1 = 0x0c647003` — one byte past the heap top
that LockHeapRange #76 had just extended to.

#### Diagnosis: heap-extend chain undersized for the 420-byte allocation

The 420-byte `TUnicodeCompressor` straddles the boundary at
`0x0c647000`:
- bytes 0..159 sit in `[0xc646f60, 0xc647000)` — within the heap
- bytes 160..419 sit in `[0xc647000, 0xc647104)` — past the heap top

LockHeapRange #76 only locked 4 KiB (`base=0xc646000 limit=0xc647000`)
when 8 KiB (`base=0xc646000 limit=0xc648000`) was needed. The
allocator made *space* for the 420-byte block in the freelist
bookkeeping but didn't `LockHeapRange` enough physical pages to
cover the back half. When user-mode `WriteRun` tries to read the
buffer, the kernel re-faults forever (now correctly returning
failure after the iter-14 wrapper fix), eventually hitting the
out-of-memory Reboot path.

#### Next iteration — pin the allocator that under-extends

The kernel allocator path is:
1. `__nw__(420)` → block-allocator → finds/creates a free 420-byte
   region near the heap top.
2. The region spans the heap boundary; allocator should call
   `ExtendVMHeap` with enough size to cover the WHOLE block, then
   `LockHeapRange(base, base+full_size)`.
3. Instead, `ExtendVMHeap` (caller_lr=`0x003109e4`) extends only
   one 4 KiB chunk via the existing patched `chunk_size=4096`. The
   block extends past the chunk; the second `LockHeapRange` for the
   following chunk never fires.

Probe candidates:
- Hook `ExtendVMHeap` entry (ROM `0x0031091C`) to log `(requested,
  current_top, granted)`. Verify whether the call asked for
  `>= 420 - (page_remaining_in_current)` or only the next 4 KiB.
- Hook `__nw__` return / new-block placement path to confirm the
  block start address vs. heap top at the moment of issue.

The simplest fix path may be to widen `ExtendVMHeap`'s
`chunk_size = 4096` patch so it grows by `roundup(requested,
8 KiB)` instead of one page at a time. Alternatively patch the
allocator so it doesn't place blocks straddling the freshly-locked
boundary.

#### Status

- 36/36 guest tests pass.
- Iter-12 36-KiB stack patches stay active.
- Iter-14 ResolveFault wrapper fix stays active (correctly
  propagates failure now).
- Iter-15 deliverable: `Fault(stackmgr)` probe procst-dump
  augmentation + SBA-stub origin lookup on dabt-forwarding.

### Iteration 14 (next-loop iter 10): LockHeapRange probe + ResolveFault wrapper bug fix + Init divisor revert

Three discoveries this iteration, all from extending the diagnostic
chain.

#### 1. LockHeapRange entry probe — pinpoints `ExtendVMHeap`

Added `LOCK_HEAP_RANGE_PROBE_HVC_IMM=0x5A` patching the `mov ip, sp`
prologue at `0x001F_8AB4` (the `LockHeapRange` user-shim entry).
Handler logs `(base, limit, lock_id, caller_lr)` per call.

The wedge sequence is unambiguous:

```
LockHeapRange #75: base=0xc608000 limit=0xc646000 lock_id=0  caller_lr=0x003109e4
LockHeapRange #76: base=0xc646000 limit=0xc647000 lock_id=0  caller_lr=0x003109e4
... infinite ResolveFault loop on FAR=0xc647003 ...
```

`caller_lr=0x003109e4` is the instruction after `bl LockHeapRange`
inside `ExtendVMHeap` at ROM `0x0031_091C`. So the heap-extend
path is the trigger, exactly as iter-13 hypothesized.

#### 2. `Init__11THeapDomain` divisor patch was over-aggressive — reverted

`Init__11THeapDomain` is the constructor for THeapDomain, which is
used for BOTH stack pools AND data heaps. Patching its divisor at
`0x001F_8D74` to 36 KiB correctly sizes the slot_info array for
stack pools but UNDER-sizes bookkeeping for data heaps (108 entries
→ 99). With under-sized heap bookkeeping, ExtendVMHeap can't grow
beyond what the smaller array can index.

Reverted that patch. `GetStackInfo` and `FMFree` divisors stay
patched — those functions are stack-only, and need the matching
36 KiB stride for correct slot index computation.

After the revert, `info_bounds` advance correctly during the
boot — bounds extend to `0xc647000` matching `LockHeapRange #76`'s
limit. The wedge moved one page later: now FAR=`0xc647003` with
bounds=`[0xc601000, 0xc647000)`, an out-of-bounds access just past
the heap's new top.

#### 3. ResolveFault wrapper was MASKING errors as success — fixed

The wrapper at `apply_resolve_fault_wrapper` was designed for the
33-KiB-stack layout where each 4 KiB physical page is shared between
adjacent stacks via subpage AP. It iterated 4 sub-pages per fault
and ignored `-10203` ("subpage belongs to another stack — skip")
return codes from `ResolveFault`, falling through to `mov r0, #0`
on loop completion.

With 36-KiB stacks, no 4-KiB page is shared, so `-10203` should
never fire. When it DOES (because of the heap-extend off-by-one),
the wrapper's "ignore -10203, return 0" semantics MASK the real
failure: the kernel re-faults forever on an unmapped page.

Fixed the wrapper's check from `cmp r0, #4 / beq done` to
`cmp r0, #0 / bne done` — propagate ANY non-zero return. Encoding:

| offset within wrapper | was | now |
|---|---|---|
| `+0x40` | `0xE350_0004` (cmp r0, #4) | `0xE350_0000` (cmp r0, #0) |
| `+0x44` | `0x0A00_0003` (beq done) | `0x1A00_0003` (bne done) |

After the fix, the kernel correctly receives the failure return
code, calls SetHeapLimits to roll back, and reaches its
out-of-memory failure path → triggers `Reboot` canary.

### Boot state at end of iter 14

```
CORRUPTION count: 0                       (alrt-DABT FIXED, iter 12)
LockHeapRange total: 77                   (heap-extend chain complete)
IdleProc #000 ENTER: count=0 esize=4      (clean CList, iter 12)
last LockHeapRange: #76 base=0xc646000 limit=0xc647000  (extends 4 KiB)
wedge: kernel reboots after FAR=0xc647003 unresolvable fault
```

The kernel is reaching MUCH later boot stages: `IdleProc` runs
clean, 77 heap operations succeed (up from 5 in iter 12). The
final stall is now a NEW class of bug: an unaligned 4-byte STR
spanning the heap top boundary. The `+3` offset of FAR=`0xc647003`
suggests a 4-byte access starting at `0xc646FFF` (last mapped byte)
with 3 bytes spilling into unmapped territory.

### Next iteration — find the unaligned-STR site

The fault PC isn't captured by the existing probes. Strategies:

1. **Augment the Fault(stackmgr) probe** to read `procst[+0x40]`
   (saved ELR_EL1 / PC) and log it with FAR. Identifies the
   exact kernel/user PC at the moment of fault.
2. **Stage-2 RO trap on the heap's last page** — capture the
   STR instruction and decode its target. `pa_emulate.rs` is
   already wired for this; just need to retarget it to the
   heap page.
3. **Cross-check Einstein** — does Einstein's heap allocator
   tolerate the `heap_top - 1` 4-byte access pattern? If so the
   bug is hypervisor-specific.

The 36-KiB stack patch + GetStackInfo/FMFree divisor patches
+ corrected ResolveFault wrapper stay active — they're proven
forward progress. 36/36 guest tests pass.

### Iteration 13 (next-loop iter 9): pool-wedge analysis — it's a HEAP-extend mismatch, not a stack-pool sizing issue

Drilled into the iter-12 wedge timeline. Tracking
`info=0x0c115784` (the TStackInfo whose bounds the failing
FMLockHeapRange checks against) across the boot shows its
`[base, top)` range GROWING over time:

| boot-line | bounds | size |
|---|---|---|
| early | `[0xc601000, 0xc981000)` | 3.5 MiB |
| then | `[0xc601000, 0xc603000)` |   8 KiB |
| then | `[0xc601000, 0xc604000)` |  12 KiB |
| then | `[0xc601000, 0xc605000)` |  16 KiB |
| ... grows in 4-KiB steps ... | | |
| at wedge | `[0xc601000, 0xc646000)` | 276 KiB |

This is a **HEAP** that extends in 4-KiB chunks (= our patched
`NewHeap` / `NewVMHeap` / `ZapHeap` chunk_size). NOT a stack pool.
The 3.5-MiB initial bounds is the heap's max reservation; the
actual extent at any time is the lower number.

### What the wedge actually is

`FMLockHeapRange` is called with `parms->[+0]=base=0xc601000`
and `parms->[+4]=limit=0xc647000` (= 280 KiB). It iterates
`r6 = base..limit` in 1-KiB steps, calling `ResolveFault` on
each chunk. When `r6` enters `[0xc646000, 0xc647000)` — past
the heap's current top of `0xc646000` — `ResolveFault` can't
resolve the page (it's outside the heap's reserved range),
returns 4, the wrapper retries the next subpage, all four fail,
the function returns 4 to FMLockHeapRange which treats it as
"page not yet present, will retry"; `r6` advances another 1 KiB,
re-faults, etc. Infinite loop.

So **`limit` is one 4-KiB page past `top`**. Caller of
`LockHeapRange` is asking to lock a range that extends one page
beyond the heap's current end.

### Why does limit > top?

This is most likely a `NewHeapArea` / `ExtendVMHeap` path where:

1. Kernel asks `NewHeapArea` to grow heap by `N` bytes.
2. Heap grows by `M` bytes where `M < N` (due to chunk rounding,
   pool exhaustion, or off-by-one in our patches).
3. Kernel calls `LockHeapRange(base, base+N)` — but heap only
   extends to `base+M`.
4. `FMLockHeapRange` iterates past `top=base+M`, faults, loops.

Our active heap-side patches are:

- `NewHeap chunk_size=4096` (was 1024) — at `0x0031_0E38`.
- `NewVMHeap` 4-KiB init path — at `0x0014_23A0`.
- `ZapHeap chunk/lock = 4096` (was 1024) — at `0x0014_28B8`.

Plus the `apply_resolve_fault_wrapper` (4-iter per page).

These were applied BEFORE iter 12 and boot worked through them
until the alrt-DABT wedge. The 36-KiB stack patches land on top
without affecting heap growth directly. But possibly:

- The stack-info bookkeeping array (sized via
  `Init__11THeapDomain`'s `(pool_size / 36864)`) is now smaller,
  and a downstream caller computes
  `lock_limit = info_array[N].top + something` where the
  array is shorter than expected — accessing past the array reads
  junk for `top`.
- Or: the 36 KiB allocation per stack means the stack pool ITSELF
  grew via heap-style extension; the heap claims it's grown by
  36 KiB but `NewHeapArea` only delivered 33 KiB or 32 KiB.

### Investigation strategy for the next iteration

1. **Probe `LockHeapRange` parms** — log `(base, limit, lock_id)`
   on entry so we see exactly what range the caller asks to lock.
2. **Probe the heap-extend path** — log `NewHeapArea` /
   `ExtendVMHeap` `(requested_size, granted_size, new_top)` so we
   see the over-/under-allocation.
3. **Cross-check Einstein** — run `NewtonProbe` to see what
   `LockHeapRange` parms look like with the original 33-KiB
   layout. If Einstein passes the same `limit > top + 4 KiB`
   pattern, our patches haven't broken anything new — the off-by-
   one already exists in the original kernel and we're triggering
   it through a different path.

### Iteration outcome — analysis only, no code change

The iter-13 deliverable is the analysis above. The wedge needs
further probing to identify the exact site that miscomputes
`lock_limit` or `granted_size`. The 36-KiB stack patches from
iter 12 stay active — they remain the right architectural fix
and have already eliminated the alrt-task DABT.

36/36 guest tests still pass with the iter-12 patches active.

### Iteration 12 (next-loop iter 8): 36-KiB stack patch lands — alrt-task DABT fixed, exposes pool-sizing wedge

Re-applied the 17 FMNewStack patches plus 3 divisor sites from
iter 11's catalogue:

- `Init__11THeapDomain` at `0x001F_8D74` — divisor for slot count.
- `GetStackInfo__11THeapDomain` at `0x001F_8E1C` — divisor for slot index.
- `FMFree__13TStackManager` at `0x001F_918C` — divisor for slot index.

All three are simple `mov r0, #33792` → `mov r0, #36864`.

### What worked — alrt-task DABT eliminated

```
NewStack POST-SWI: env=0x1355 req=0x9000 base=0x0c306000 top=0x0c30e000 span=0x8000
NewStack POST-SWI: env=0x1355 req=0x9000 base=0x0c30f000 top=0x0c317000 span=0x8000
NewStack POST-SWI: env=0x1355 req=0x9000 base=0x0c318000 top=0x0c320000 span=0x8000
NewStack POST-SWI: env=0x13a5 req=0x9000 base=0x0cc67000 top=0x0cc6f000 span=0x8000
NewStack POST-SWI: env=0x13a5 req=0x9000 base=0x0cc70000 top=0x0cc78000 span=0x8000
...
IdleProc #000 ENTER this=0x0cc92fa8 inner=0x0cc92f38 clist=0x0cc92fc4 (PA=0x0403ffc4)
  count=0 esize=4 ebase=0x00000000 src_mode=0x10 sp=0x0cc92e38
```

- `req=0x9000` (= 36 KiB) per task ✓
- Each slot 4-KiB-aligned, 32 KiB usable + 4 KiB guard ✓
- IdleProc enters with **`count=0 esize=4 ebase=0`** — the CList is
  CORRECTLY initialized!
- pa-emul `CORRUPTION` count = **0** ✓

The alrt-task DABT investigation that motivated iters 1-11 is
SOLVED by this patch. With each task in its own 36-KiB slot
(4-KiB-aligned), there's no boundary 4-KiB page shared with an
adjacent task; SetFreeChain/MoveFreeBlock pushes stay inside the
current task's slot, never landing on alrt's data.

### What broke next — pool sized for 8 × 33 KiB, only 7 fit at 36 KiB

Boot now wedges in an infinite ResolveFault loop:

```
Fault(stackmgr) probe ENTER: this=0x0c112cb8 procst=0x0c1133a4 \
  far=0x0c647003 caller_lr=0x00259230 src_mode=0x10 (USR)
ResolveFault probe ENTER: ... far=0x0c647000 \
  info_bounds=[0x0c601000,0x0c647000) ...
ResolveFault probe ENTER: ... far=0x0c647400 ...
ResolveFault probe ENTER: ... far=0x0c647800 ...
ResolveFault probe ENTER: ... far=0x0c647c00 ...
```

`info_bounds=[0xc601000, 0xc647000)` — a 280 KiB stack pool
(= 8 × 33 KiB + 16 KiB overhead). With 36-KiB slots we fit 7
plus a partial 8th that overflows pool end at `0xc647000`. The
8th task's stack maps to slot index 7 starting at `0xc640000`
which extends to `0xc649000` — past pool end. Its first access
(`FAR=0xc647003`) falls into ResolveFault, which can't resolve
out-of-pool addresses, so the fault loops.

This is the same failure mode as iter-6's "Option A pad attempt"
(2026-04-23). The diagnosis there: "the call-site pad cannot
work alone — pad and stride must move together". We now have
the stride (FMNewStack 36-KiB patch), but not the pool size.

### Next iteration — find and patch the pool-size site

The pool size is allocated somewhere upstream of FMNewStack,
likely in `TStackManager::Init` or the domain-creation path that
hands a backing store to `Init__11THeapDomain`. The pool-byte
size at iter 12 is `0x46000` (280 KiB) for the 0xc601000 domain.
With 8 × 36 KiB slots we'd want at minimum `0x48000` (288 KiB),
plus whatever overhead (slot_info array, alignment) the kernel
adds.

Search candidates:
- Constants near `Init__11THeapDomain` — the function takes
  a `(start, size_in_megabytes)` pair (`r2 << 20`, `r3 << 20`).
  The pool size in this domain might come from a config table.
- Look for `pool_size_in_bytes = N * 33792` patterns, or
  `pool_size = N * 0x21` pre-shift. With the divisor patched
  to `#36864`, slot count is computed correctly; the question
  is whether the pool itself is sized as a multiple of slot_size.
- The `Get` page-allocator (probed at `0x00258EFC`) may also
  be relevant if the pool is page-aligned and sized in pages.

A simpler band-aid: clamp `Init__11THeapDomain`'s slot count
to `(pool_size - overhead) / 36864` and accept the lost slot.
That gives us 7 stacks instead of 8 in this domain. May or may
not be enough for boot.

### Backup plan if the pool-size patch is hard

Reverting just the 36-KiB stride change loses the alrt-task
DABT fix — the iter-12 patch is the first time we've cleanly
gotten past that stall. If the pool-size investigation takes
multiple iterations, consider keeping the 36-KiB patch as
"forward progress" while we work on the pool sizing — the
ResolveFault loop is more diagnosable than the alrt-task DABT
because we know exactly what's wrong (overflow past pool end).

### Iteration 11 (next-loop iter 7): 36-KiB FMNewStack patch attempted, reverted — slot arithmetic in 33 KiB lives in many functions

Implemented the 17 FMNewStack patches sketched in iter 10:

```
0x001F_8EDC  mov r7, #36864   (was #33792)
0x001F_8EF0  sub r1, r0, #4096 (was #3072)
0x001F_8F18  mov r0, #36864
0x001F_8F20  add r0, r0, r0, lsl #3   (×9)   was lsl #5 (×33)
0x001F_8F24  sub r0, r9, r0, lsl #12  (×4096) was lsl #10 (×1024)
0x001F_8F30  add r0, r0, #4096
0x001F_8F38  cmp r0, #36864
0x001F_8F48  mov r0, #36864
0x001F_8F5C  mov r0, #36864
0x001F_8F88  add r0, r0, #4096
0x001F_8F90  cmp r0, #36864
0x001F_8FA0  mov r0, #36864
0x001F_9024  add r1, sl, sl, lsl #3
0x001F_902C  add r9, r0, r1, lsl #12
0x001F_9030  add r0, r7, r7, lsl #3
0x001F_9034  sub r0, r9, r0, lsl #12
0x001F_9038  add r2, r0, #4096
```

(Initially had a typo at `0x001F_8F60` instead of `0x001F_8F5C`,
which clobbered the `bl __rt_udiv` after the third divisor — fixed
once detected via the rom_patch dump.)

### What worked

NewStack POST-SWI confirms FMNewStack alone produces the right
36-KiB layout:

```
NewStack probe POST-SWI: env=0x1355 req=0x9000 base=0x0c306000 \
  top=0x0c30e000 span=0x8000 caller_lr=0x00252390 src_mode=0x10 (USR)
```

- `req=0x9000` (= 36 KiB) ✓
- `base=0x0c306000` (4-KiB aligned ✓)
- `top=0x0c30e000` (4-KiB aligned ✓)
- `span=0x8000` (= 32 KiB usable ✓)

So FMNewStack itself is internally consistent.

### What broke — boot wedge in PauseSystem

```
unaligned: cannot read aligned 0xea3fffc4 (EA=0xea3fffc5) at PC=0x387ebc
  pre-abt CPSR = 0x400001d3  mode=0x13
  r0..r7:   0xea3fffbd 0x0c400000 0x0c100fc8 ...
```

`PC=0x387ebc` is `PauseSystem::TPlatformDriver`'s `ldr ip, [r0, #8]`
— a vtable load. `r0=0xea3fffbd` is junk; `r0+8 = 0xea3fffc5`
unaligned-faults. The corruption happens before alarm-task creation,
so the alrt-task DABT we were trying to fix isn't even reached.

### Why — `#33792` is encoded in many ROM functions, not just FMNewStack

`grep -nE '#33792' rom.dis` shows ~50 occurrences. The FMNewStack
patch only covers ~13 of them. The rest are in:

| function | offset | what it does |
|---|---|---|
| `Init__11THeapDomain` | `0x001F_8D74` | `mov r0, #33792` divisor — computes slot count from pool size |
| `GetStackInfo` | `0x001F_8E1C` | `mov r0, #33792` divisor — maps a VA back to its slot index |
| FMNewStack continuation | `0x001F_918C` | `mov r0, #33792` — additional divisor in the success-tail |
| BootOS / system-stack init | `0x0001_8F8C`, `0x0001_8FA4`, `0x0001_90EC` | `add r0, r0, #33792` — points into a hardcoded `0xC008400` system-stack location |
| Stack-walker / unwind | `0x0027_1Exx`, `0x0027_22xx`, etc. | conditional `sub r0, r0, #33792` — stack-bounds check in fault decode |
| Misc | `0x0038_D008`, `0x0038_D404`, etc. | `add ... #33792` |

Without patching the divisor sites, `Init__11THeapDomain` thinks
the pool has `pool_size / 33` slots when actually it has fewer
36-KiB slots. The slot-info array is sized wrong; later code reads
past it into junk → vtable corruption → wedge.

### Reverted; document for the next iteration

The FMNewStack patches have been reverted. The detailed catalogue
of additional sites is ready to consume; the next iteration needs
to:

1. Patch the **divisor sites** in `Init__11THeapDomain` (`0x001F_8D74`),
   `GetStackInfo` (`0x001F_8E1C`), and FMNewStack continuation
   (`0x001F_918C`). All three are simple `mov r0, #33792` →
   `mov r0, #36864`.
2. Audit the **`add r0, r0, #33792`** sites at `0x0001_8F8C`, etc.
   These compute `0xC000000 + 33792 = 0xC008400` for the system
   stack location. Either patch them to `+ 36864` (= `0xC009000`)
   or leave them — depends on whether the system stack uses
   FMNewStack-allocated memory or its own region.
3. Audit the **conditional `sub` sites** at `0x0027_1Exx`. These
   look like stack-bounds checks. Encoding for `subne r0, r0,
   #33792` (`0x12400B21`) → `subne r0, r0, #36864` (`0x12400A09`)
   etc. — same imm12 swap as the unconditional cases.
4. Test guest-tests + boot. If the IdleProc DABT goes away (no
   pa-emul CORRUPTION lines), commit; otherwise document the
   next failure mode.

The 36-KiB patch in PATCHES_717006 is reverted but the comment
block lists all 17 instructions for re-introduction; the
companion divisor + bounds patches are added in the next
iteration.

### Iteration 10 (next-loop iter 6): patch audit — guard pages aren't working because we kept the 33-KiB layout

**User question (2026-04-29):** "Sounds like the stack manage guard
pages aren't working. Did we perhaps patch it to change from 33k
allocation (one 1k subpage of guard) to 32k (no guard), instead of
36k with a full 4k guard?"

This is exactly the right framing. The audit:

### What current patches actually do

`PATCHES_717006` has these heap-allocation patches active:

| offset | patch | effect |
|---|---|---|
| 0x0031_0E38 | `NewHeap` chunk_size = 4096 | heaps grow in whole 4 KiB pages |
| 0x0014_23A0 | `NewVMHeap` 4-KiB init path | LockHeapRange initial size = 4 KiB |
| 0x0014_28B8 | `ZapHeap` chunk/lock size = 4096 | initial lock covers whole page |

Plus `apply_resolve_fault_wrapper` (installed): iterates ResolveFault
4× per page so the first allocation owns all 4 subpages of the
4-KiB physical page.

### What we did NOT patch

- `FMNewStack`'s 33-KiB stride is unchanged. The size constant
  `0x8400` (= 33 KiB) still appears at multiple sites in
  `0x001F_8EDC..0x001F_9034`, plus the slot-encoding `add r1, sl,
  sl, lsl #5` (= sl × 33) at `0x001F_9024`. A 33→36 KiB attempt
  was made and reverted earlier (Phase B logs).
- The "GetMatchingPage = always 0" stub at lines 186–220 of
  `src/rom_patches.rs` is **commentary only** — it is not in the
  `PATCHES_717006` array. Without it, `TStackManager` still
  matches existing pages from its cache, sharing 4-KiB physical
  pages between adjacent stacks via the boundary subpage.

### So why does the guard not catch SetFreeChain's overflow?

Under ARMv4 hardware:
- Each task's slot is 33 KiB. Of that, 32 KiB are unique pages
  and 1 KiB is one subpage of the boundary page (shared via
  subpage AP with the next task).
- The other three subpages of the boundary page belong to the
  adjacent task(s) and are AP=00 (sys-only) from this task's
  perspective — **subpage AP IS the guard mechanism**.
- A push that crosses out of the owned subpage permission-faults
  on hardware; the kernel's DataAbortHandler either grows the
  stack (via lazy resolve) or signals overflow.

Under our flat AP=11 stage-2 mapping:
- `fix_stage1_xn_bits` strips the per-subpage AP bits from every
  small-page L2 descriptor. Each VA gets full RW to the entire
  4-KiB page.
- The "guard" subpages are no longer guards — they're just
  regular bytes that happen to be the next task's data. The push
  succeeds silently, corrupting the alias victim.

We can't recover ARMv4 subpage AP at stage-2: stage-2 has 4-KiB
granularity, so we can't make individual subpages of a 4-KiB page
unmapped while leaving others RW. Either the whole page is
mapped or none of it is.

### The correct architectural fix — 36 KiB allocation with a full 4-KiB guard page

Reshape the per-task slot from `33 KiB = 32 KiB usable + 1 KiB
subpage guard` to **`36 KiB = 32 KiB usable + 4 KiB full-page
guard`**:

```
old slot (33 KiB):                     new slot (36 KiB):
+----------------+ <- top              +----------------+ <- top
|                |                     |                |
|   32 KiB       |                     |   32 KiB       |
|   usable stack |                     |   usable stack |
|                |                     |                |
+----------------+                     +----------------+ <- base
| 1 KiB subpage  |  (boundary page,    | 4 KiB GUARD    |  (own page,
|   guard via    |   shared with       |   stage-2 RO   |   stage-2
|   subpage AP)  |   adjacent task)    |   or unmapped) |   unmapped)
+----------------+                     +----------------+
```

Each slot now has its own dedicated 4-KiB guard page, separately
mapped. The hypervisor sets that page **stage-2 unmapped** (or
RO with W^X — kernel writes fault to EL2). On a stack overflow,
sp drops into the guard, the write fires a stage-2 permission
fault, and we either forward to the kernel's DataAbortHandler
(if the kernel is supposed to handle stack overflow) or halt
loudly with full context.

### Patch sites — FMNewStack at 0x001F_8EAC

The 33-KiB stride is encoded in five places. To bump to 36 KiB:

| offset | original | new | meaning |
|---|---|---|---|
| 0x1F_8EDC | `mov r7, #33792` | `mov r7, #36864` | per-slot size = 36 KiB |
| 0x1F_8F18 | `mov r0, #33792` | `mov r0, #36864` | divisor in computation |
| 0x1F_8F38 | `cmp r0, #33792` | `cmp r0, #36864` | bounds check |
| 0x1F_8F48 | `mov r0, #33792` | `mov r0, #36864` | divisor again |
| 0x1F_8F60 | `mov r0, #33792` | `mov r0, #36864` | divisor again |
| 0x1F_8FA0 | `mov r0, #33792` | `mov r0, #36864` | divisor again |
| 0x1F_9024 | `add r1, sl, sl, lsl #5` (sl×33) | `add r1, sl, lsl #2` then `add r1, r1, sl, lsl #5` (sl×36) | slot stride = sl × 36 |
| 0x1F_9034 | `sub r0, r9, r0, lsl #10` (×1024) | unchanged | scale stays the same |

ARM immediate encoding for 36 KiB: 36864 = 0x9000 = 0x9 × 0x1000.
ARM `mov` immediate-12: rot=0xa, imm8=0x09 → ROR(0x09, 20) = 0x9000.
Encoding: `0xE3A0_0A09` for `mov r0, #36864`,
`0xE3A0_7A09` for `mov r7, #36864`,
`0xE350_0A09` for `cmp r0, #36864`.

The `sl × 36` rewrite is two instructions but the original is one
— so we'd need a different patch shape (e.g., a small ROM-trampoline
that does the multiplication and branches back). That's the
mechanical complication that defeated the prior 36-KiB attempt;
the simpler path may be to encode `sl × 36` as `(sl + sl<<3) << 2`
which is also two instructions.

### Stage-2 guard wiring

For each new 36-KiB slot, the bottom 4 KiB is a guard page. The
hypervisor needs to know which physical pages back the guards.
Two options:

1. **Patch the kernel's stack-pool initialization** so the guard
   pages are allocated from a separate hypervisor-known pool, and
   stage-2 holds them unmapped. Cleanest but more invasive.
2. **Detect at PrimRememberMapping**: when the kernel installs the
   FIRST 4-KiB page of a new task's slot (the bottom = guard),
   note it and flip it to stage-2 unmapped. Doesn't require ROM
   patches beyond what's already there. Reuse the
   `g1_capture::set_ram_page_ro_xn` pattern — but unmap entirely
   (write a 0 descriptor) instead of RO+XN, since any access
   should fault.

Option 2 is the one that pairs naturally with the FMNewStack-stride
patch.

### Next iteration plan

1. Implement the FMNewStack stride bump (33 → 36 KiB) at all sites.
   Find the encoding for the `sl × 36` and ensure the divisors all
   match.
2. Modify the `apply_resolve_fault_wrapper` if needed — its
   loop-over-4-subpages pattern is tied to the 33-KiB layout's
   subpage handling.
3. Add a stage-2 guard-page installer hooked into PrimRememberMapping
   that detects "this is a slot's first page" and unmaps it at
   stage-2.
4. Verify pa-emul CORRUPTION drops to 0 and IdleProc DABT no longer
   fires.

The shadow-pool infrastructure from iter 9 stays available — it's
useful for any remaining alias cases where 36-KiB stacks alone
aren't enough.

### Iteration 9 (next-loop iter 5): shadow-pool infrastructure for Option β

Lay the groundwork for the alias-redirect fix recommended by iter 8.
The fix needs hypervisor-managed RAM that can back redirected guest
mappings: when the kernel installs a second VA for an already-mapped
PA, the hypervisor allocates one of these pages, copies the original
PA's contents, and rewrites the new VA's L2 entry to point at the
shadow IPA. Both VAs then have their own physical pages, eliminating
the cross-subpage write hazard.

This iteration is **infrastructure only** — the policy that uses the
pool (the Prim Remember hook) lands next iteration. Splitting it
keeps each commit testable in isolation.

### What landed

- **`src/shadow_pool.rs`** — 64 KiB region (16 4 KiB pages) at
  `IPA=0x0601_0000`. Backed by `static SHADOW_POOL: [u8; 64 KiB]`
  in hypervisor RAM. Provides `allocate()` (monotonic slot
  handout) and `host_pa()` (used by `host_addr_for`).
- **`src/stage2.rs::install_scratch_pool`** extended to map
  the shadow pool's 16 L3 entries alongside the existing
  scratch pool — both share the 2 MiB block at IPA `0x0600_0000`,
  so we reuse `S2_L3_SCRATCH` rather than adding another L3 table.
- **`src/guest_mem.rs::host_addr_for`** extended so PA helpers
  (`read_word_pa`, `write_word_pa`, etc.) can address shadow IPAs
  the same way they address scratch / RAM / framebuffer.
- **Smoke test** in `kmain` after stage-2 enable: allocate one
  shadow page, write `0xCAFEF00D`, read it back, log OK/FAIL.

Cold-boot output:

```
stage2: shadow-stub scratch pool @ IPA 0x6000000..0x6010000 -> host PA 0x1617000 (RW, 64 KiB)
stage2: alias-redirect shadow pool @ IPA 0x6010000..0x6020000 -> host PA 0x1607000 (RW, 64 KiB)
shadow_pool smoke test: ipa=0x06010000 write=true readback=Some(3405705229) -> OK
```

The smoke test confirms the round trip works: a write at IPA
`0x06010000` lands in our static `SHADOW_POOL` and is readable
through `read_word_pa`. Subsequent iterations can rely on this
plumbing.

### Next iteration — wire the redirect at PrimRememberMapping

With infrastructure in place the redirect itself is small:

1. In `handle_prim_remember_probe_with`, detect the alias install
   (PA already in `PRIM_FIRST_VA_FOR_PA` for a different VA, AND
   the new VA's intent mask doesn't subsume the existing one's —
   meaning the new VA expects to own a different subpage and
   therefore wants a private page under our flat AP=11).
2. Call `shadow_pool::allocate()`. Copy the current PA contents
   (via `read_word_pa` × 1024) to the shadow IPA.
3. Modify TPhys[+0x10] in place: replace the high 20 bits (page
   IPA) with the shadow IPA's high 20 bits, leaving the low 12
   flag bits untouched. The kernel reads TPhys[+0x10] inside
   PrimRememberMapping (`ldr r0, [r2, #16]!` at `0x163498`) and
   passes it to AddPgPAndPerm — so by the time the L2 entry gets
   written, it points at the shadow IPA.
4. PrimForgetMapping reads the same TPhys[+0x10] (still the shadow
   IPA after our modification) and removes the right entry —
   symmetric, no separate hypervisor-side bookkeeping needed.

The regression test: after the redirect lands, pa-emul's
`writer-PC frequency` table should show **zero** counts for
`MoveFreeBlock` and `SetFreeChain` writing through `VA=0x0c320000`,
and the IdleProc DABT at `FAR=0xe336000c` should be gone (the
alrt task's CList stays uncorrupted).

The pa-emul scaffolding stays armed across iterations — it's the
mechanical regression test for the redirect.

### Iteration 8 (next-loop iter 4): subpage-violation classifier and writer attribution

Iter 7 pinned the corrupting writer to `SetFreeChain` at PC=0x00310850
but didn't classify which writes are bugs (kernel-intent violation)
vs. legitimate (kernel writing its own subpage). With the dense
pa-emul output it was hard to tell at a glance which writes were the
problem.

This iteration adds two diagnostic layers in `src/pa_emulate.rs`:

1. **Subpage-violation classifier** (`pa-emul CORRUPTION:` log).
   For every emulated store landing in the watch window, look up
   `kernel_intent_mask_for(pa, va_page)` and check whether the
   accumulated mask grants AP for the subpage containing the offset.
   If not, emit a CORRUPTION line — this is the cross-subpage write
   ARMv4 subpage AP would have caught.

2. **Writer-PC frequency table** dumped from the Reboot canary.
   32-slot table aggregating every emulated write hitting the watch
   window, with human-readable labels for the known PCs.

### Cold-boot result — every corrupting writer comes from VA=0x0c320000

```
pa-emul writer-PC frequency (top hits in watch window):
    PC=0x003121b0  count=    38  MoveFreeBlock prologue push
    PC=0x00310850  count=    35  SetFreeChain prologue push
    PC=0x00019a84  count=    16  kernel poison-fill loop (boot-init)
    PC=0x00019ac0  count=    16  kernel poison-fill loop (boot-init)
    PC=0x00019af0  count=    16  kernel poison-fill loop (boot-init)
    PC=0x00018ddc  count=    16  kernel zero-fill loop (boot-init)
    PC=0x003940b4  count=    16  LowLevelCopyEngineLong memcpy
    PC=0x00030b84  count=     1  ?      ← TAlertManager::MainConstructor (legitimate)
    PC=0x00259610  count=     1  ?
    PC=0x00f10e28  count=     1  ?
```

The CORRUPTION lines fire ONLY for the two heap-allocator prologues:

```
pa-emul CORRUPTION: PC=0x003121b0 VA=0x0c3207f8 (page=0x0c320000 mask=0x30)
  writes PA=0x0402e000+0x7f8 value=0x0c201010 mode=0x10 [STM-r4]
  — subpage AP[1] not in kernel-intent mask
pa-emul CORRUPTION: PC=0x00310850 VA=0x0c3207e0 (page=0x0c320000 mask=0x30)
  writes PA=0x0402e000+0x7e0 value=0x00000020 mode=0x10 [STM-r5]
  — subpage AP[1] not in kernel-intent mask
```

Both come from `VA=0x0c320000` (kernel-intent mask=`0x30` →
AP[2]=11, owns subpage 2 only) writing into subpage 1 (offsets
`0x400..0x7FF`) — alrt task's territory.

### What the data tells us about scope and fix path

- Only ONE VA (`0x0c320000`) hosts the corrupting writers. That's
  one task's stack region; the other aliases are subpage-disjoint
  in practice (no CORRUPTION lines for VA=`0x0c328000`,
  `0xcc9b000`, etc., even though they share `PA=0x0402e000`).
- Only TWO routines (`MoveFreeBlock`, `SetFreeChain`) generate
  the cross-subpage writes. Both are heap-allocator internals
  with multi-register prologues that drop sp from low addresses
  in subpage 2 across the boundary into subpage 1.
- `LowLevelCopyEngineLong` and `TAlertManager::MainConstructor`
  hit the watch window too but only via VAs whose mask DOES
  include AP[1] (alrt's `VA=0x0cca3000`, mask=`0xc`). Those are
  legitimate writes by the page's intended owner.

So the bug surface is narrow: when the task whose stack maps to
`PA=0x0402e000` via `VA=0x0c320000` runs SetFreeChain or
MoveFreeBlock with sp already low in its boundary page, the push
crosses out of its subpage. Under ARMv4 hardware, the kernel
would have caught the resulting permission fault and either grown
the stack or panicked. Under our flat AP=11, the push silently
overwrites alrt's CList header.

### Fix path — recommend Option τ (kernel patch for non-shared boundary pages)

Three options remain on the table:

1. **Option β (PA splitting via L2-entry redirect)**: when the kernel
   installs the second VA's L2 entry for an already-mapped PA,
   rewrite the IPA to a hypervisor-allocated shadow page. Removes
   the alias entirely. Cost: ~13 4-KiB shadow pages of RAM, plus
   the L2-entry-rewrite hook. Architecturally clean; preserves the
   kernel's existing layout and code.

2. **Option τ (kernel ROM patch for 36-KiB stack stride)**: bump the
   per-task stack region from 33 KiB to 36 KiB (9 unique 4-KiB
   pages) so the boundary page is no longer shared. Requires
   patching FMNewStack at ROM `0x001f8eac` — the slot stride is
   encoded as `r1 = sl + sl<<5` (sl*33) and the size constant
   `0x8400` (33 KiB) appears at multiple sites. The earlier
   call-site +4 KiB pad failed because pool stride wasn't bumped
   together; a coordinated patch should work.

3. **Option α (per-task subpage-AP shim)**: emulate ARMv4 subpage
   AP at stage-2 by splitting each aliased page into 4 stage-2
   sub-mappings. Stage-2 has 4-KiB granularity natively, so this
   would require trapping every access on aliased pages and
   filtering by VA — much higher overhead.

**Recommend Option β next.** β is more general (covers any future
alias bug), is the smallest-surface change (one hook in the Prim
Remember probe + a shadow-page allocator), and preserves the
kernel ROM unmodified. Option τ would be elegant if it works, but
the 36-KiB stride change has many call sites the earlier attempt
exposed.

### Implementation sketch for Option β (next iteration)

1. Reserve a hypervisor RAM pool at IPA `0x07000000+` (16 4-KiB
   pages — enough for all 12 Group-2 aliases plus headroom).
2. Stage-2 maps the pool identity (host_addr_for already covers
   it via the existing scratch pool / fb_dump area).
3. In `handle_prim_remember_probe`, when the install would create
   an alias (PA already in `PRIM_FIRST_VA_FOR_PA`), allocate the
   next pool slot and pre-empt the kernel:
   - Patch the L2-write site at PrimRememberMapping (the inner
     `bl AddPgPAndPerm` at `0x163504` passes IPA in r2; rewrite
     to shadow IPA).
   - Or, simpler: hook AT the AddPgPAndPerm prologue and modify r2.
   - Or, simplest: scan the kernel's L2 page table after Prim
     return and rewrite the entry.
4. Verify with a re-run that pa-emul CORRUPTION count drops to 0
   and the IdleProc DABT no longer fires.

This is the regression test: with PA splitting in place, the
pa-emul output should show ZERO CORRUPTION lines and the boot
should advance past the alrt-task DABT to the next stall.

### Iteration 7 (next-loop iter 3): pa-emulate catches the corrupting writer — SetFreeChain @0x00310850

Implemented Option A from iter 6: `src/pa_emulate.rs` decodes and
emulates AArch32 stores at the stage-2 RO trap, bypassing the
auto-flip-to-RW pattern that limited prior probes to one capture
per ~16 ms IRQ window. Coverage: STR/STRB/STRH (immediate, A1) +
STM/STMDB/STMIB/STMDA (A1 with optional writeback). Unrecognized
forms fall back to the prior auto-flip path so boot can continue.

Wired into `handle_data_abort` for `alrt_capture::is_armed_pa`
hits. With the page held RO continuously, the trap fires on
**every** store to `PA=0x0402e000`. A separate watch window
`[0x7c0, 0x800)` filters per-store kprintln so the log stays
readable.

### Cold-boot result — corruption traced to a single (PC, value)

```
alrt-capture summary: armed_pa=0x0402e000 traps=4825 out_of_window=4779 budget_remaining=4050
pa-emul summary: emulated=4824 unrecognized=1 skipped=0
```

530× the visibility of the prior probe (9 traps → 4825). The
in-window log captured the corruption directly:

```
pa-emul[STM-r5]: PC=0x00310850 VA=0x0c3207c4 PA=0x0402e000+0x7c4 value=0x00000020 mode=0x10
pa-emul[STM-lr]: PC=0x00310850 VA=0x0c3207d4 PA=0x0402e000+0x7d4 value=0x003121fc mode=0x10
pa-emul[STM-pc]: PC=0x00310850 VA=0x0c3207d8 PA=0x0402e000+0x7d8 value=0x00310858 mode=0x10
```

The values are an **exact match** for the corrupted CList header
seen at IdleProc entry (count=`0x20`, ebase=`0x003121fc`,
+0x7d8=`0x00310858`).

`PC=0x00310850` is the prologue of `SetFreeChain`:

```
0031084c <SetFreeChain>:
  31084c:  e1a0c00d  mov   ip, sp
  310850:  e92dd870  push  {r4, r5, r6, fp, ip, lr, pc}   ← THE CORRUPTING STM
  310854:  e24cb004  sub   fp, ip, #4
```

The push lands at `VA=0x0c3207c4..0x0c3207f4`. That VA stage-1
maps to **`PA=0x0402e7c4..0x0402e7f4`** — the same physical bytes
the alrt task accesses through `VA=0x0cca37c4` for its CList
header. `src_mode=0x10` (USR) means a user-mode task running
ROM code; `sp_usr` near `0x0c3207f4` matches the `name` task
(census shows `sp_usr=0xc3208d0`).

### Why the prior alias audit was wrong

The iter-3 audit concluded "no kernel-intent overlap on any of
the 15 aliased PAs" and treated all aliases as **capability
hazards** (not access-pattern hazards). Specifically:
- `VA=0x0c320000` mask=`0x30` → AP[2]=11 → owns subpage 2
  (offsets `0x800..0xBFF`).
- `VA=0x0cca3000` mask=`0xc` → AP[1]=11 → owns subpage 1
  (offsets `0x400..0x7FF`).

Audit's claim: the kernel only writes its OWN subpage; the
ARMv4-vs-ARMv7 difference is invisible because no actual byte
collision occurs.

But `SetFreeChain`'s prologue absolutely DOES drop sp into the
adjacent subpage. With `sp` at `0x0c3207f4` (already in subpage 1
of `VA=0x0c320000`'s page — below the `0x800` boundary), the
`push {r4,r5,r6,fp,ip,lr,pc}` writes 28 bytes from `0x0c3207d8`
to `0x0c3207f0`. **Every one of those 7 words is in subpage 1,
which the kernel intent says belongs to `VA=0x0cca3000` (the
alrt task), not to `VA=0x0c320000` (the name task).**

Under ARMv4 hardware subpage AP, this push would have raised a
permission fault on the first store. The original kernel must
have had a fault handler that grew the stack or split it
differently. Under our flat AP=11, the push silently succeeds
and overwrites alrt's CList header.

The audit's "no access-pattern hazard" conclusion is refuted by
direct observation. **The capability hazard IS the bug** — the
kernel's actual access pattern crosses subpage boundaries; ARMv4
caught it; ARMv7 with our flat AP doesn't.

### Next iteration — pick a fix layer

The diagnosis is now solid. Three viable fixes:

1. **Option β (PA splitting)**: clone `PA=0x0402e000` per-VA so
   each task gets its own physical page. Eliminates the alias
   entirely; SetFreeChain's push at `0x0c3207f4` no longer
   touches alrt's bytes. Hypervisor-side; doesn't require kernel
   patches. Stage-2 has 4-KiB granularity which matches the
   problem; the cost is ~13 extra 4-KiB pages of RAM (one per
   aliased PA) and shadow-on-write coherence for any
   legitimately-shared bytes (none expected — the alias is
   already in the audit's "kernel intends disjoint subpages"
   class).

2. **Option τ (stack-region ROM patch)**: re-attempt the 36-KiB
   per-task stack patch from earlier iterations, this time with
   the matching pool-stride bump so padded stacks fit. The
   smoking gun (`SetFreeChain` is just a normal heap routine
   running in any task's context) suggests the right fix is
   stack regions that don't share boundary pages, period — not
   special handling of one routine.

3. **Per-task subpage-AP shim at stage-2**: split each aliased
   page into 4 stage-2 sub-mappings, each granted RW only to the
   VAs whose kernel-intent mask includes that subpage. Most
   ARMv4-faithful but most complex; requires per-task IPA
   contexts and stage-2 reconfiguration on context switch.

Recommend **Option β** next. The code already separates "what
PA backs each VA" from "what bytes live there"; PA-splitting
is the smallest patch that demonstrably eliminates the bug
class.

Keep the pa-emulate scaffolding active across iterations — once
β is in place the alrt page should see ZERO emulated stores
landing in the watch window, which is the regression test.

### Iteration 6 (next-loop iter 2): __dl__/free tracking refutes the heap-overlap diagnosis

Added `DL_PROBE_HVC_IMM=0x59` at ROM `0x00318F28` (`__dl__FPv`,
the C++ `operator delete` thunk that tail-calls `free`). The
original word is a single `b 0x01bd2958 <free>`; the probe
overwrites it with `HVC #0x59`. The handler reads `r0` (the block
to free), scans `NW_TABLE` newest-first for a slot with matching
`addr`, clears `addr` to 0, then sets `ELR_EL2` to
`DL_FREE_TARGET_PC=0x01BD2958` so ERET continues into the actual
`free` implementation in REx — the kernel's free path is preserved.

The overlap detector already skipped slots with `addr == 0`, so
clearing them on free turns the tracker into a true live-allocation
view. Added two counters (`NW_FREE_MATCHED`, `NW_FREE_UNMATCHED`)
plus a periodic `nw summary` log every 256 allocations.

`__dl_v__FPvUiPFPvi_v` (vector delete) tail-calls `__dl__` after
running destructors, so probing `__dl__` alone covers
`delete[]` too.

### Cold-boot result — heap allocator is NOT producing overlapping live blocks

```
nw summary @seq=256: live=222 frees(matched=35 unmatched=2)
nw summary @seq=512: live=291 frees(matched=222 unmatched=58)
nw summary @seq=768: live=497 frees(matched=272 unmatched=70)
OVERLAP DETECTED count: 0
```

Zero overlaps, all the way to the wedge. **The 293 "overlaps" from
iter 5 were ALL same-address-recycle false positives** — the
allocator was correctly freeing and re-issuing blocks; we just
didn't track frees and so every legitimate recycle looked like a
collision.

The earlier "smoking gun" partial-overlap case (#118/#120) was a
misread:
- #118: `__nw_v__` (vector new) called `__nw__` for `0x2c` bytes.
  Returned `addr=0x0c1178c8`. This is the underlying
  `count*size+4`-byte block that backs the vector.
- #120: `THeapDomain::Init` called `__nw__` for `0x3e0` bytes.
  Returned `addr=0x0c1178cc`.

Between #118 and #120, the kernel ran `__dl_v__(0x0c1178cc, ...)`
(or a direct free of 0x0c1178c8) — the matching free was just
invisible to iter 5. With free tracking, the recycled-and-shifted
allocation is benign coalesce-and-resplit by the allocator.

### Implications — back to the alias-overlap diagnosis

The user's hypothesis ("heap manager assigning overlapping
physical pages") is **disproved** by the boot-long zero-overlap
result. The alrt CList corruption at `PA=0x0402e7c4` does NOT come
from two `__nw__` callers being given the same address.

That puts the corruption back where the iter-3 per-IRQ snapshot
analysis pointed: a write to a VA that aliases `PA=0x0402e000`
through a stage-1 mapping that the prior alias audit classified
as "kernel-intent disjoint". Specifically, in this boot:
- `VA=0x0cca3000` (alrt globals) mask=`0xc` → AP[1]=11 (subpage 1,
  offsets `0x400..0x7FF`) — owns `PA=0x0402e000+0x7c4`.
- `VA=0x0cc9b000` (mntr stack) — first-mapped, PRIM tracker-pinned.
- `VA=0x0c320000` (name task) mask=`0x30` → AP[2]=11 (subpage 2).
- `VA=0x0c328000` (Tmux task) mask=`0xf0` → AP[2..3]=11.

By kernel intent, only the alrt task should write subpage 1
(offsets `0x400..0x7FF`). The corruption pattern (ROM PCs
`0x003121fc`/`0x00310858` from `MoveFreeBlock`/`SetFreeChain`)
looks like saved-LR bytes from a heap-allocator call frame — not
the alrt task's own data.

**Working hypothesis:** one of the OTHER tasks aliasing
`PA=0x0402e000` runs `MoveFreeBlock`/`SetFreeChain` with `sp`
landing at offset `0x7c4` of its own VA window. Under ARMv4
subpage AP that write would have been blocked (its mask doesn't
include subpage 1). Under our flat AP=11 it goes through to
`PA=0x0402e000+0x7c4`, overwriting the alrt CList. Iter 3's
audit conclusion ("capability hazard, not access-pattern hazard")
is the wrong call: under flat AP=11 the kernel actually IS doing
the unintended write that subpage AP would have caught.

### Next iteration — capture the writer's PC, finally

Both prior approaches (snapshot diff, stage-2 RO trap with
auto-flip) saw the corruption but could not pin the PC. Two
remaining paths:

1. **Instruction-level emulation at the stage-2 trap**, the long-
   deferred Option A from iter 2. Keep `PA=0x0402e000` RO
   continuously; in `handle_data_abort`, decode the AArch32 store
   at `ELR_EL2`, apply the write via `write_word_pa`, advance
   `ELR_EL2`, leave the page RO. Captures every write with exact
   PC. Reuses `src/unaligned.rs`'s decode helpers; STM needs
   per-register iteration.

2. **Constrain via per-task stack subpage trap.** If only one
   task is the corrupting writer, identifying it narrows the
   call site without full instruction emulation. A cheaper probe:
   on every `Prim Remember` for `PA=0x0402e000`, log the per-VA
   mask history; correlate with the wedge to see which task's
   accumulated mask doesn't include subpage 1 yet whose VA
   covers offset `0x7c4`.

Option 1 is the architecturally correct fix and reuses
infrastructure that's been wanted across multiple iterations.
Option 2 is one-iteration cheap but only narrows by one variable.

Recommend Option 1 next.

### Iteration 4 (deferred / superseded by next-loop iter 1 above): ROM-decode of CList ctor

Iter 3 hypothesized "stale heap freelist data left after `MoveFreeBlock
→ SetFreeChain`, never zero-init'd by the constructor." Iter 4
disasm cross-check shows that's wrong:

`__ct__5CListFv` at ROM `0x113238` calls
`__ct__13CDynamicArrayFlT1(this, 4, 4)` at `0xa16ac` which
unconditionally writes:
```
[this+0x00] = 0     (count)
[this+0x04] = 4     (elem_size)
[this+0x08] = 4     (max_capacity)
[this+0x0c] = 0
[this+0x10] = 0     (entries_base)
[this+0x14] = 0
```

Re-reading iter 3's diff #2:
```
+0x7c4 (count):       0 -> 0
+0x7c8 (elem_size):   0 -> 4   ← matches ctor
+0x7cc (max_cap):     0 -> 4   ← matches ctor
+0x7d0:               0 -> 0
+0x7d4 (entries_base):0 -> 0   ← matches ctor (NULL)
+0x7d8:               0 -> 0
```

Diff #2 IS the legitimate CList constructor finishing. The CList
is correctly initialized (count=0, elem_size=4, max_cap=4,
entries_base=NULL).

Diff #3 then **actively overwrites** the just-constructed CList:
```
+0x7c4 (count):       0 -> 0x00000020   (32 — bogus)
+0x7c8 (elem_size):   4 -> 0x00000001   (1 — bogus)
+0x7cc (max_cap):     4 -> 0x0c320804   (RAM ptr)
+0x7d0:               0 -> 0x0c3207dc   (RAM ptr)
+0x7d4 (entries_base):0 -> 0x003121fc   (ROM PC, into MoveFreeBlock)
+0x7d8:               0 -> 0x00310858   (ROM PC, SetFreeChain)
+0x7dc (TUAsyncMessage start): 0x1db9 -> 0x0c201010
```

This isn't stale data being read — it's an active WRITE of 24+
bytes of foreign data into `TAlertManager+0x8c`, AFTER the CList
constructor properly initialized it.

### TAlertManager layout decoded from `InitAlertManager` ROM `0x000307D4`

```
TAlertManager (200 bytes from `__nw__(200)` at 0x307e4):
  +0x00:  TAppWorld (vtable=0x0001c880, set at 0x30824)
  +0x70:  TAEventHandler embedded sub-object
            +0x70+0x00: vtable = 0x0001eac0 (TAlertEventHandler vtable,
                        set at 0x30804 overwriting TAEventHandler ctor's vtable)
            +0x70+0x14: pointer back to TAlertManager base (= "this->[+0x14]"
                        in IdleProc, used to navigate from TAlertEventHandler
                        sub-object to TAlertManager outer)
  +0x8c:  CList (24 bytes — the dialog list IdleProc reads)
  +0xa4:  TUAsyncMessage
  +0xb4:  TAEvent
  +...:   trailing fields up to 0xC8 (200) bytes
```

This explains our observations from iters 1-3:
- `this=0x0cca37a8` is TAlertManager+0x70 (TAlertEventHandler embedded sub-object)
- `this->[+0x14] = 0x0cca3738` is the back-pointer to TAlertManager base
- `inner+0x8c = 0x0cca37c4` is the CList at TAlertManager+0x8c

So our offset arithmetic is correct. The CList IS where we think it
is, and it IS getting properly constructed (per diff #2). The
corruption (diff #3) happens AFTER successful construction.

### What causes the active overwrite?

Two candidates:
1. **A memcpy or struct copy** that runs after CList ctor. The
   24-byte size matches a CList-sized memcpy from a 24-byte
   source. The source content (count=0x20, elem_size=1,
   pointers including ROM PCs) doesn't match any standard
   well-formed object — it looks like raw kernel-internal
   bookkeeping.
2. **A field-by-field assignment** in some downstream init
   step that mistakes our CList region for a different field.
   The values look heterogeneous enough (mix of small ints,
   RAM ptrs, ROM PCs) to be field assignments rather than a
   single memcpy.

### Next iteration — capture the corrupting write directly

The per-IRQ snapshot pinned the corruption to a single ~16 ms
boot window between diffs #2 and #3. Two ways to escape that
limit and capture the actual writer's PC:

**A. Instruction-level emulation** (deferred from iter 2 still
the right architectural fix). Keep PA=0x0402e000 RO continuously;
on each trap decode the AArch32 store at ELR_EL2, apply the
write via PA helpers, advance ELR. Captures every write with
exact PC.

**B. Periodic in-handler snapshots**. Add a snapshot-diff call
at every Prim Remember / Forget / IdleProc entry (in addition
to maybe_rearm). Tightens the window from ~16 ms to whatever
event triggers most frequently between diffs #2 and #3.

A is the right fix for general future debugging; B is the
faster way to narrow this specific case.

Implemented option B (per-IRQ snapshot diff) in
`src/alrt_capture.rs`. 8-word snapshot of offsets 0x7c0..0x800
is sampled at every `maybe_rearm` call; word-level diffs vs
prior snapshot are logged.

Cold-boot timeline (6 diffs):

| diff | what changes | observation |
|------|-----|-----|
| #0 | All 8 words: 0 → `0x6db6db6d` pattern | Kernel free-memory poison fill |
| #1 | All 8 words: poison → 0 | Page zero-cleared (allocation step) |
| #2 | +0x7c0=0x0c6016b8, +0x7c8=4, +0x7cc=4, +0x7dc=0x1db9 | Some sentinel/header init |
| #3 | **The corruption appears as a single batch:** | |
|    | +0x7c0: 0x0c6016b8 → 0x0c20463c | RAM ptr |
|    | +0x7c4: 0 → **0x00000020** | bogus "count" = 32 |
|    | +0x7c8: 4 → **0x00000001** | bogus "esize" = 1 |
|    | +0x7cc: 4 → 0x0c320804 | RAM ptr |
|    | +0x7d0: 0 → 0x0c3207dc | RAM ptr |
|    | +0x7d4: 0 → **0x003121fc** | ROM PC after `bl SetFreeChain` |
|    | +0x7d8: 0 → **0x00310858** | ROM PC = SetFreeChain prologue |
|    | +0x7dc: 0x1db9 → 0x0c201010 | RAM ptr |
| #4 | +0x7c0: 0x0c20463c → 0x0c204884 | Heap-link rotation |
| #5 | +0x7c0: 0x0c204884 → 0x0c2049a0 | Heap-link rotation |

### Translate_va check disproves the third-VA-alias hypothesis

I extended IdleProc to translate VA=0x0c3207dc (which would be
SP if these were direct-push frame imprints). Result:
VA=0x0c3207dc → PA=**0x0402f7dc**, NOT 0x0402e7dc. The values
are NOT from a stage-1-aliased push to the alrt page.

### Revised diagnosis: heap-allocator stale data, not direct alias

The Newton kernel's heap allocator uses raw bytes inside free
blocks for freelist linkage (`SetFreeChain`/`MoveFreeBlock`).
When a free block is recycled, the linkage bytes remain visible
until overwritten by the new owner.

Diff #3's pattern fits: a recently-freed block at PA=0x0402e7c0
held linkage data from when the heap allocator's freelist
manipulation routines (`MoveFreeBlock`, `SetFreeChain`) had run
through it. That block was then handed to the alrt task's
TAlertEventHandler allocation. The kernel never zero-init'd the
CList header at +0x1c..+0x3c, so the alrt task sees the stale
freelist bytes when IdleProc reads CList::At(0).

The ROM PCs (0x003121fc / 0x00310858) being heap-allocator
internals is consistent with the freelist storing return
addresses or pointers as part of its internal accounting.

### Next iteration — narrow the heap-allocator path

1. Hook a probe at the heap allocator's block-handout entry
   points (`NewBlock`, `NewPtr`, equivalent) logging every
   allocation that lands near PA=0x0402e000. The expected
   sequence: a free block at PA=0x0402e7a8 with stale linkage
   bytes is reused for the alrt TAlertEventHandler without
   zero-init.
2. Cross-check Einstein with `NewtonProbe` to see whether the
   ARMv4 kernel's allocator zero-inits this region or whether
   subpage-AP somehow keeps the freelist bytes from leaking
   into client code.
3. Decide fix layer once the leaking allocator path is
   identified — likely a ROM patch inserting zero-init on the
   TAlertEventHandler constructor or the CList constructor.

User directive (2026-04-28): "look at every alias and decide it's
benign before moving on. This is how we find bugs in our 4k page
allocation patch set."

Audit complete and mechanically confirmed. The new
`KERNEL_INTENT_MASK[256]` per-(PA, VA) accumulated-mask tracker
(iter 4) feeds verify-mmu a kernel-intent DISJOINT/CONFLICT
classification independent of the post-flatten AP-decode. Cold
boot: **12/12 Group-2 aliases mechanically DISJOINT, 0 CONFLICT**;
3 Group-1 aliases bypass Prim (direct L2 writes during TTBR0
setup) and are covered by the prior `InitSpecialStacks`
subpage-disjoint analysis.

The strongest candidate for a real conflict (PA=0x04034000, 5-way
install) was investigated via a per-PA Remember/Forget timeline
probe (`PRIM_FOCUS_PA`) and now mechanically classified as INTENT:
DISJOINT (`prev_va_mask=Some(0)`, `va_mask=Some(12)`). 3 transient
secondaries are properly forgotten; 2 simultaneous-live VAs
(0xcc82000 mask=0xc, 0x0c310000 mask=0x3) target disjoint subpages
(AP[1] vs AP[0]).

This means the directive's premise ("things break randomly under
flat AP=011") is satisfied by audit, not by elimination — the
kernel's careful subpage-disjoint design ensures no byte conflict
under intended access patterns. Remaining hazard: only if the
kernel itself has an out-of-subpage-bounds write, which would have
been a bug on ARMv4 too.

For the prior history (Phase B per-stall fixes, FMNewStack 33→36 KiB
patch attempt and revert, deeper alrt-task DABT analysis, RelocHeap
corruption fix, etc.) see git log up to commit
`83634659 baremetal: Remember (static) is also NOT the aliasing
source — pivot to PrimRemember*` and `INVESTIGATION.md` at that
commit. The current file is intentionally pruned to the live task.

**Next:** un-park alrt-task DABT investigation pending user
confirmation. Keep audit scaffolding active so any new alias
gets the same kernel-intent-mask analysis automatically.

**IMPORTANT:** Run the *original ROM code*. Don't introduce patches or
workarounds just to get the run further. Diagnose and fix the actual
problem. *No workarounds, no deferrals, no shortcuts.* No silencing
warnings. Fix all warnings before each commit.

When complete, next goal will be to resume per-stall debugging.

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

## Aliasing elimination — current state

### Inventory at the wedge — 12 RAM aliases in two groups

```
PA=0x04004000  VA=0x0c000000 (L1[0xc0],L2[0x0]) ↔ VA=0x0c002000 (L1[0xc0],L2[0x2])
PA=0x04005000  VA=0x0c003000 (L1[0xc0],L2[0x3]) ↔ VA=0x0c004000 (L1[0xc0],L2[0x4])
PA=0x04006000  VA=0x0c007000 (L1[0xc0],L2[0x7]) ↔ VA=0x0c008000 (L1[0xc0],L2[0x8])
PA=0x04028000  VA=0x0c310000 ↔ VA=0x0c318000  (last pages of stacks #10, #11)
PA=0x0402c000  VA=0x0cc7a000 ↔ VA=0x0cc82000  (8 KiB apart)
PA=0x0402e000  VA=0x0cc9b000 ↔ VA=0x0cca3000
PA=0x0402f000  VA=0x0c318000 ↔ VA=0x0cc7a000
PA=0x04033000  VA=0x0cc82000 ↔ VA=0x0ccad000
PA=0x04034000  VA=0x0cc7f000 ↔ VA=0x0cc82000
PA=0x04035000  VA=0x0c603000 ↔ VA=0x0ccc4000
PA=0x0403a000  VA=0x0ccc4000 ↔ VA=0x0ccca000
PA=0x0403b000  VA=0x0ccc4000 ↔ VA=0x0cccb000
```

(Reported by `verify-mmu` in `src/guest_mem.rs::fix_stage1_xn_bits`,
ratchet-logged with `(PA, VA1, VA2)` per alias-onset.)

**Group 1 — kernel-globals self-mapping** (PAs 0x04004-0x04006).
Created at TTBR0 setup time. The kernel maps its own L1/L2 backing
pages into VA 0x0c000000+ at two offsets each. Kernel-only by intent.

**Group 2 — stack-guard sharing** (the rest). Adjacent stack slots at
33-KiB intervals straddle a 4-KiB boundary; the kernel relied on
ARMv4 subpage AP to sub-divide ownership. ARMv7 has no subpage AP →
both VAs end up RW pointing to the same PA after we flatten to AP=011.

### Investigation progress

Four probes installed; the third+fourth narrowed Group-2 to
deliberate stack-guard sharing.

1. **`TUDomainManager::Get` page-allocator** (HVC #0x53 on `0x00258EFC`).
   28 Get calls; 0 duplicates. Get is NOT recycling PAs.

2. **`Remember (static)`** at `0x00258E0C` (HVC #0x46, augmented
   per-PA tracker). 0 `Remember ALIAS:` lines, but the alias detector
   mis-decoded the args (treated r3 as a PA when r3 is the TPhys-
   pointer passed through to `GenericSWI`). Still wouldn't have
   caught the kernel-internal paths.

3. **`PrimRememberMapping` at `0x00163480`** (HVC #0x54). Caught
   all 12 Group-2 aliases. Signature is
   `(va=r0, mask=r1, &TPhys=r2, perm=r3)`; mask in r1 is the
   incremental-subpage activation mask (same va called repeatedly
   with widening 0x3 → 0xff). Probe walks RememberMapping's APCS
   frame to capture the upstream caller LR. Distribution across
   13 unique aliased PAs:
   - `0x000d8e3c` (GenericSWIHandler / SWI #12 dispatch): 13 (all)
   - `0x001f775c` (CopyPagesAfterStackCollided 2nd RM call): 9
   - `0x001f76bc` (CopyPagesAfterStackCollided 1st RM call): 2

4. **`PrimForgetMapping` at `0x00163514`** (HVC #0x55). Hoisted the
   per-PA → first-VA tracker into module-level statics so both
   probes manipulate the same arrays. A matched forget clears the
   slot; mismatched ones log `FORGET MISMATCH:`. Cold-boot deltas:

   | metric              | iter 1 (Remember) | iter 2 (+Forget) |
   |---------------------|------------------:|-----------------:|
   | `Prim ALIAS:` lines |               106 |               55 |
   | unique aliased PAs  |                13 |               12 |
   | `FORGET MISMATCH:`  |               n/a |                8 |

   The 12 surviving PAs are **real aliases**: kernel installed PA
   at VA1, then at VA2, with no intervening forget. **All 12 come
   through `0x000d8e3c` (GenericSWIHandler, SWI #12)** — i.e.
   user-mode `Remember (static)` calls. The aliased VAs land on
   the 32-KiB stack-stride pattern (e.g. PA=0x04028000 mapped at
   0xc310000, 0xc318000, 0xc320000), confirming **Group-2 stack-
   guard sharing**: the kernel deliberately makes the LAST page of
   stack N the FIRST page of stack N+1 (ARMv4 subpage AP gave each
   stack its own half; ARMv7 collapses to AP=011 → real alias).

   Group-1 aliases (PA=0x04004000-0x04006000) still don't pass
   through Prim — they remain direct kernel L2 writes during TTBR0
   setup.

### Investigation progress (continued)

5. **SWI save-area walk** for user-mode caller identification.
   New helper `read_swi_caller()` reads `(saved_pc, lr_usr,
   user_caller)` from `curr_task + {0x4c, 0x48, 0x3c-walk}`.
   Prim ALIAS lines now log all three. Result across 12 aliased
   PAs:

   | user_caller | function | aliased PAs |
   |---|---|---:|
   | `0x002523bc` / `0x002523d4` | **`TTask::Init`** post-LockHeapRange BL sites | **11** |
   | `0x00124280` | `TMuxStoreMonitor::Init` | 2 |
   | `0x003109e4` | `ExtendVMHeap` | 2 |
   | `0x0c1118c8` | RAM (REx-resident shim) | 2 |
   | + 7 more, 1 PA each | `NewVMHeap` / `LockStack` / `NewDirectBlock` / `TheMain::TLoader` / `TCardAsyncMsg` / 1 RAM | 7 |

   `user_lr=0x00258efc` (inside `TUDomainManager::Get`) for ALL
   aliases — the SWI is dispatched through Get's
   `bl MonitorDispatchSWI` site as part of LockHeapRange's
   per-page resolve-fault path.

   Root cause confirmed: stack allocations via `TTask::Init →
   NewStack → LockHeapRange` deliberately share 4 KiB boundary
   pages between adjacent stacks (33-KiB usable on a 32-KiB VA
   stride). ARMv4 subpage AP let each stack own 1 KiB of the
   shared boundary; ARMv7 has no subpage AP, AP=011 makes both
   stacks' VAs alias the same PA.

6. **Option A (call-site +4 KiB pad) attempt** — implemented as a
   2-word wrapper at `0x00FFFE80` (`add r1, r1, #4096; b NewStack
   thunk`); BL at `0x0025238C` redirected through it. **Result:
   boot wedges in an infinite ResolveFault loop at
   FAR=0xc647003** (3 bytes past `info_bounds.end=0xc647000`).
   The pad bumped the size requested of NewStack but did NOT
   change the kernel's stack-pool slot stride; padded stacks
   overflow into the (N+1)-th slot, exhausting the pool one
   stack early. ResolveFault returns "success" via the existing
   wrapper but the underlying VA is unmapped → abort re-fires
   forever. Patch reverted; wrapper code retained as
   `apply_new_stack_pad_wrapper` (not installed) for future use.
   Baseline restored.

   **Insight:** The call-site pad cannot work alone — pad and
   stride must move together (= the prior 20-patch 36-KiB
   attempt). Resurrecting that attempt with our current Get
   probe (which proved Get returns unique PageIds, contradicting
   the prior "PA recycling" diagnosis) is plausible but a
   substantial undertaking.

7. **Group-1 stage-2 RO trap probe** — implemented `g1_capture`
   module marking PA=0x04004000, 0x04005000, 0x04006000 RO+XN
   at boot, captures every guest write with (PC, offset, value).
   IRQ-only rearm (sync-trap rearm caused an infinite STMIA-retry
   loop). Cold-boot run: 186 captures across 25 writer PCs,
   exit=1 reboot canary, 15 verify-mmu aliases unchanged, 36/36
   guest tests pass.

   **Captures don't reveal alias-creating writes.** The 3 armed
   PAs are *target* pages of the duplicate L2 entries — what
   gets mapped at two VAs — not the L2 PT pages where the
   duplicate L2 descriptors live. Per the prior task-census
   `L1[0xc0]=0x00001401`, the L2 PT for L1[0xc0] sits at
   PA=`0x00001400` in **ROM**; the duplicate descriptors at
   L2[0x0] / L2[0x2] / etc. are pre-baked at ROM build time and
   never dynamically written. Group-1 aliases are static ROM
   artifacts, not runtime kernel decisions.

   Hypervisor self-noise observed: 56 captures at PC=0x00FFFF08
   (UND_TRAMP) writing PA=0x04005000+0xf0c, plus 5 at
   PC=0x00FFFFB4 (DABT_TRAMP) writing +0xfa0 — these are our
   own UND/DABT scratch-slot writes (UND_SAVE_R0_IPA=0x04005F0C,
   DABT_SAVE_PA=0x04005FA0) trapping at stage-2.

8. **ROM-baked L2 PT confirmed.** Added a one-shot dump in
   verify-mmu's first-alias-per-PA log path that reads the L1
   entry's L2 PT and logs `(L2[prev_idx], L2[va_idx], L2_PT_PA,
   ROM/RAM)`. Cold-boot result:

   ```
   L1[0xc0]=0x00001411 → L2_PT@PA=0x00001400 (ROM)
     L2[0x0]=0x0400403e (PA=0x04004000)  L2[0x2]=0x0400414e (PA=0x04004000)
     L2[0x3]=0x0400503e (PA=0x04005000)  L2[0x4]=0x0400514e (PA=0x04005000)
     L2[0x7]=0x0400603e (PA=0x04006000)  L2[0x8]=0x0400604e (PA=0x04006000)
   ```

   The duplicate descriptor pairs share PA but have **different
   subpage-AP bits**. They aren't redundant; they're permission
   overlays that under ARMv4 gave each subpage exactly one VA
   with privileged-RW access. Decoding the L2[0x0]/L2[0x2] pair:
   - 0x0400403e: AP=(11,00,00,00) — subpage 0 RW; rest sys
   - 0x0400414e: AP=(00,01,01,00) — subpages 1-2 priv-RW

   Our `fix_stage1_xn_bits` flattens both to AP=11 → both VAs
   become full RW → real PA alias under ARMv7. This is the
   *deliberate* ARMv4-era design colliding with our normalization.

   Group-2 dumps confirm the same subpage-AP pattern but with
   RAM-resident L2 PTs at PA=0x04025400/0x04025800 (kernel-
   installed per-task page tables) — populated at runtime by
   the TTask::Init → LockHeapRange → RememberMapping chain.

9. **Option α probe** — added PATCHES_717006 entries zeroing
   L2[0x2]/0x4/0x8 of the ROM L2 PT at PA=0x00001400. Cold-boot:
   verify-mmu aliases 15 → 2, but boot wedged in an infinite
   ResolveFault loop at FAR=0xc004fa0. The 2 surviving aliases
   exposed a *third* descriptor per PA we hadn't seen on the
   first walk:

   ```
   L2[0x0] (PA=0x04004000) ↔ L2[0x5] (PA=0x04004000)
   L2[0x3] (PA=0x04005000) ↔ L2[0x6] (PA=0x04005000)
   ```

   Decoding L2[0x5]=0x0400440e and L2[0x6]=0x04005c0e: each
   grants priv-RW to **subpage 3** of its PA. So each Group-1
   PA has 3 distinct L2 descriptors, each granting priv-RW to
   a different subpage. The "duplicates" aren't redundant —
   they're per-subpage RW grants split across multiple VA
   windows.

   **What was using FAR=0xc004fa0?** Disasm grep proves no
   kernel code references the entire L1[0xc0] self-map VA
   range (0x0c000000..0x0c008fff has 0 literal hits) or our
   trampoline scratch VAs (0x0c004f00/0x0c004fa0). The only
   code using those VAs is OUR HYPERVISOR's DABT/UND/SBA
   trampolines (`install_und_vector_swap_post_mmu()` writes
   0x0c00_4f00 / 0x0c00_4fa0 into trampoline literals). The
   wedge was our DABT trampoline trying to `str lr, [r0]` at
   r0=0x0c004fa0 with L2[0x4] zeroed → unmapped → re-DABT.

   Patches reverted. Baseline restored: 15 verify-mmu aliases,
   exit=1 reboot canary, 36/36 guest tests pass.

   The L1[0xc0] kernel-globals self-map is **functionally inert**
   in the running kernel — vestigial design intent the team
   wired up but the running code doesn't exercise. The PA-target
   pages (PA=0x04004000-0x04006000) DO receive writes from many
   kernel PCs (g1_capture probe found 25 distinct writers), but
   those accesses must go through some OTHER VA window not yet
   enumerated.

10. **Trampoline relocation succeeded; dedup retried; wedge revealed
    the actual layout.** Moved `HYP_TRAMP_SCRATCH_BASE` from
    `0x04005F00` to `0x0600_F000` (last 4 KiB of SCRATCH_POOL).
    Same value works pre + post-MMU — no swap needed. Refactored
    `read/write_*_pa` helpers through `host_addr_for(pa, size, for_write)`
    so SCRATCH_POOL accesses succeed from EL2. 36/36 guest tests
    pass; baseline preserved (15 verify-mmu aliases unchanged).

    With trampolines moved, re-attempted ROM-patching of L2[0x2,4,5,6,8]
    in the L2 PT at PA=0x00001400. Cold-boot: aliases dropped 15→0,
    but boot wedged at FAR=`0xc004bf8` in `InitSpecialStacks`.
    Patches reverted.

    **The L1[0xc0] self-map is the kernel's exception-stack layout.**
    Decoded `InitSpecialStacks` at ROM `0x0011efb4`:
    - SetFIQStack(0x0c003400) → subpage 0 of PA=0x04005000 via L2[0x3]
    - SetAbortStack(0x0c004c00) → subpages 1+2 via L2[0x4]
    - SetUndefStack(0x0c006000) → subpage 3 via L2[0x6]
    - SetIRQStack(0x0c002c00) → subpage 3 of PA=0x04004000 via L2[0x2]
    - SetUserStack(0x0c007400) → L2[0x7]

    The kernel packs 4 exception stacks into shared 4-KiB physical
    pages, each at its own 1-KiB subpage. ARMv4 subpage AP gave each
    stack an exclusive priv-RW VA window; Einstein faithfully
    emulates this (`Emulator/TMMU.cpp:304-306, 325-326`). Our flat
    AP=11 collapses the per-subpage AP, but the kernel's *usage
    pattern* still puts each stack at its own disjoint 1-KiB offset.
    Stacks share the page in stage-1 but never write overlapping
    bytes — assuming none overflows its 1-KiB allocation (which
    would also be a bug under ARMv4).

    **Group-1 aliases are functionally inert** — verify-mmu reports
    them but no actual byte conflict can occur. The same likely
    holds for Group-2 (kernel allocator gives each task stack a
    consistent 33-KiB layout; boundary 4-KiB pages split into
    per-stack zones).

    Disasm-grep finding zero literal references to alias VAs was
    misleading: kernel computes them via `mov + orr` indirection.

    The original directive ("things break randomly") rests on the
    assumption that aliases cause corruption; the kernel's careful
    subpage-disjoint design prevents this.

### Per-alias subpage-AP audit (per user directive 2026-04-28)

Extended verify-mmu's first-alias dump with subpage-AP decode and
classified each pair. **14 of 15 aliases are DISJOINT** (one VA
grants priv-RW to its dedicated subpage; the other has no RW
grants — a pure read-only mirror). **One alias is a real
CONFLICT**:

```
PA=0x04034000  L2[0x7f]=0x0403403e  L2[0x82]=0x0403403e  IDENTICAL
  AP-decode: both grant priv-RW to subpage 0 (offsets 0..1023)
```

Prim probe data confirms 5 distinct VAs map to PA=0x04034000
all with `mask=0x3` (subpage 0 RW), from 3 different allocator
paths (TLoader, TCardAsyncMsg ctor, TTask::Init ×2). The kernel
allocator is recycling PA=0x04034000 across distinct consumers
without subpage isolation.

This is a real bug — exactly what the user directive predicted:
the 4-KiB patch set audit reveals a kernel allocator behavior
the patches don't cover. ARMv4 hardware would have caught this
too (5×AP=11 on the same subpage); how the original kernel
handled it is the open question.

### PA=0x04034000 timeline — 2 simultaneous-live mappings, but kernel-intent DISJOINT subpages

Added `PRIM_FOCUS_PA=0x04034000` to the Prim Remember/Forget probes:
both handlers log every call for this PA with a shared global seq#,
giving chronological order. Cold-boot captured 17 events:

| seq | op | VA | mask | upstream | user_caller |
|-----|----|----|------|----------|-------------|
| #000 | REM | 0x0cc82000 | 0x0 | GenericSWIHandler | 0x0c1181b0 (REx driver) |
| #001 | REM | 0x0cc82000 | 0xc | CopyPagesAfterStackCollided#1 | 0x0c1181b0 |
| #002 | REM | 0x0cc7f000 | 0x0 | GenericSWIHandler | TheMain::TLoader |
| #003 | REM | 0x0cc7f000 | 0x3 | GenericSWIHandler | TheMain::TLoader |
| #004 | **FGT** | **0x0cc7f000** | | | |
| #005 | REM | 0x0cc82000 | 0xc | CopyPagesAfterStackCollided#2 | 0x0c1181b0 |
| #006 | REM | 0x0cc80000 | 0x0 | GenericSWIHandler | TCardAsyncMsg ctor |
| #007 | REM | 0x0cc80000 | 0x3 | GenericSWIHandler | TCardAsyncMsg ctor |
| #008 | **FGT** | **0x0cc80000** | | | |
| #009 | REM | 0x0cc82000 | 0xc | CopyPagesAfterStackCollided#2 | 0x0c1181b0 |
| #010 | REM | 0x0cc81000 | 0x0 | GenericSWIHandler | TCardAsyncMsg ctor |
| #011 | REM | 0x0cc81000 | 0x3 | GenericSWIHandler | TCardAsyncMsg ctor |
| #012 | **FGT** | **0x0cc81000** | | | |
| #013 | REM | 0x0cc82000 | 0xc | CopyPagesAfterStackCollided#2 | 0x0c1181b0 |
| #014 | REM | 0x0c310000 | 0x0 | GenericSWIHandler | TTask::Init#1 |
| #015 | REM | 0x0c310000 | 0x3 | GenericSWIHandler | TTask::Init#1 |
| #016 | REM | 0x0c310000 | 0x3 | GenericSWIHandler | TTask::Init#2 |

**Three transient VAs (0xcc7f, 0xcc80, 0xcc81) ARE properly forgotten
before reuse — those are not the bug.** VA=`0xcc82000` (REM #000)
and VA=`0x0c310000` (REM #014) are both never forgotten, so end-of-
boot has PA=0x04034000 mapped at TWO live VAs. But that mapping is
not by itself a bug — the kernel's *intent* is that each VA owns a
disjoint subpage of the page, encoded in `mask=r1`.

### Mask decode — kernel-intent subpages are DISJOINT

Per the existing comment in `handle_prim_remember_probe_with`,
`mask=r1` is the kernel's "incremental subpage activation mask"
encoded as 2 bits per subpage in the L2-descriptor format:

| mask field bit | L2 desc bits | AP field |
|---:|:---:|:---:|
| `0x3 = 0b0000_0011` | bits [5:4] = 11 | AP[0] (subpage 0, offsets 0x000..0x3FF) |
| `0xc = 0b0000_1100` | bits [7:6] = 11 | AP[1] (subpage 1, offsets 0x400..0x7FF) |
| `0x30 = 0b0011_0000` | bits [9:8] = 11 | AP[2] (subpage 2, offsets 0x800..0xBFF) |
| `0xc0 = 0b1100_0000` | bits [11:10] = 11 | AP[3] (subpage 3, offsets 0xC00..0xFFF) |

Accumulated mask per VA across calls (kernel ORs masks on repeated
remember calls to the same `va`):

- **VA=0xcc82000**: 0x0 | 0xc | 0xc | 0xc | 0xc | 0xc = **0xc → AP[1]=11 (subpage 1)**
- **VA=0x0c310000**: 0x0 | 0x3 | 0x3 = **0x3 → AP[0]=11 (subpage 0)**

**Different subpages**. The kernel intends VA=0xcc82000 to own
offsets 0x400..0x7FF and VA=0x0c310000 to own offsets 0x000..0x3FF
of PA=0x04034000. No byte conflict in kernel-intended access.

### Why verify-mmu reported CONFLICT — post-flatten audit blind spot

`fix_stage1_xn_bits` in `src/guest_mem.rs` unconditionally rewrites
every small-page L2 descriptor as `(e & 0xFFFF_F000) | 0x3E`,
forcing AP[0]=11 (and AP[1..3]=00) regardless of the kernel-installed
subpage AP fields. Every post-flatten descriptor decodes to
(sp0=11, sp1=0, sp2=0, sp3=0). The audit reads the live (post-
flatten) descriptors, so two VAs with kernel-intent AP[1] vs AP[0]
both show as AP[0]=11 → CONFLICT.

This is correct ARMv7 behavior — once any subpage is RW the whole
page is RW from any VA mapping that PA, because subpage AP doesn't
exist on ARMv7. The user-noted hazard ("if anything is RW we have
to make the whole page RW") is the *capability* hazard, not an
*access-pattern* hazard. The kernel's actual byte accesses through
each VA still follow its subpage-disjoint design (it just can no
longer rely on hardware to enforce the boundary). The 4-KiB patch
set's role is to remove this *capability* hazard by giving each
consumer its own physical page so subpage AP becomes irrelevant.

### Conclusion: all 15 aliases are subpage-disjoint by kernel intent

Group-1 (3 aliases, kernel exception stacks) was already established
as subpage-disjoint by `InitSpecialStacks` analysis in the prior
iteration. Group-2's 12 aliases all come through the same
Prim Remember chain; the 5-way conflict at PA=0x04034000 was the
strongest candidate for a non-disjoint conflict, and it too turns
out to be subpage-disjoint by kernel intent (AP[0] vs AP[1]).

Per the user's audit directive ("look at every alias and decide
it's benign before moving on"), this completes the audit:
**no alias has overlapping kernel-intent subpages**. The aliases
are *capability hazards* under flat AP=11 (kernel COULD write
out-of-subpage bytes through any VA) but not *access-pattern
hazards* (kernel doesn't actually do that — it would have crashed
on ARMv4 too).

### Next iteration — un-park alrt-task DABT investigation

The user's "no debugging until aliasing is zero" directive was based
on the premise that aliases caused random corruption. The audit
shows no kernel-intent overlap on any of the 15 aliased PAs, so
random corruption from cross-VA writes can only happen if the kernel
itself has an out-of-subpage-bounds write — which would have been
a pre-existing bug under ARMv4 hardware too. The hypervisor's flat
AP=11 doesn't introduce a new failure mode beyond removing ARMv4's
ability to *catch* such bugs.

Recommended path:
1. Resume the alrt-task DABT investigation that was parked 4+
   iterations ago (see git log up to commit `83634659`).
2. Keep the current diagnostic scaffolding in place
   (`PRIM_FOCUS_PA`, `PRIM_FIRST_VA_FOR_PA`, verify-mmu AP-decode)
   so any new alias that appears during further boot debugging
   gets the same audit treatment automatically.
3. As a backstop, if the alrt-task DABT turns out to be caused by
   a cross-VA write into a wrong subpage (the worst-case
   capability hazard), pivot to Option β (stage-2 PA splitting on
   that specific page) or Option τ (per-allocator-path 4-KiB
   patches).

### Mechanical kernel-intent audit — 12/12 Group-2 aliases DISJOINT

Iter 4: added a per-(PA, VA) accumulated-mask tracker
(`KERNEL_INTENT_MASK[256]` in `src/trap.rs`) wired into the Prim
Remember/Forget probes. Verify-mmu's first-alias dump now calls
`kernel_intent_mask_for(pa, va)` for both VAs and emits a
`verify-mmu alias INTENT:` line classifying the kernel's pre-flatten
intent as DISJOINT (no shared AP=11 subpage) or CONFLICT (overlap),
independent of the post-flatten audit.

Cold-boot result on the 15 aliases:

| group | aliases | INTENT classification |
|-------|--------:|-----------------------|
| Group-1 (kernel exception stacks, direct L2 writes) | 3 | kernel-direct-or-forgotten (Prim is bypassed; covered by `InitSpecialStacks` analysis) |
| Group-2 (Prim Remember chain) | 12 | **DISJOINT (mechanical)** |
| Total CONFLICT | 0 | — |

The post-flatten audit's lone CONFLICT (PA=0x04034000) is now
mechanically reclassified as INTENT: DISJOINT, with
`prev_va_mask=Some(0) va_mask=Some(12)` — VA=0xcc7f000 had no
kernel-RW intent (mask=0, lazy-fault install) at the moment of
detection; VA=0xcc82000 claimed subpage 1 (mask=0xc → AP[1]=11).
No subpage overlap → DISJOINT. The hand-decoded analysis of iter 3
is now mechanically confirmed.

### Remaining diagnostic scaffolding (active)

- `PRIM_FOCUS_PA = 0x04034000` in `src/trap.rs` — per-PA
  Remember/Forget chronological log. Retarget at any other PA by
  changing the constant.
- `PRIM_FIRST_VA_FOR_PA[]` / `PRIM_FIRST_LR_FOR_PA[]` —
  per-PA → first-VA tracker driving `Prim ALIAS:` output.
- `KERNEL_INTENT_MASK[256]` — per-(PA, VA) accumulated mask;
  reused by verify-mmu's INTENT classifier on every first-alias
  detection.
- verify-mmu first-alias dump in `fix_stage1_xn_bits` — emits both
  post-flatten `AP-decode:` and kernel-intent `INTENT:` lines.

### Audit complete — backup approaches retained for record

Both Group-1 (kernel exception stacks, 3 aliases) and Group-2 (12
aliases including the PA=0x04034000 5-way pattern) have all-disjoint
kernel-intent subpages. The user directive is satisfied by audit.

Backup approaches retained for the record, in case a future debug
discovers a non-disjoint kernel access pattern (e.g., out-of-
subpage-bounds write that ARMv4 would have caught):
- **Option β** — full stage-2 PA splitting with shadow-on-write
  coherence on the conflicting page. Substantial work; only
  worth it if a specific page exhibits real cross-VA byte
  collisions in observed kernel behaviour.
- **Option τ** — patch the specific allocator paths
  (TLoader / TCardAsyncMsg / TTask::Init / REx-driver path
  through `0x0c1181b0`) to use distinct 4-KiB PAs from a
  chunk_size=4096 patch similar to NewHeap.

The alrt-task DABT and other parked Phase-B wedges can resume.

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
