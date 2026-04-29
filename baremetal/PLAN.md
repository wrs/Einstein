# Plan — Drive Newton OS to interactive use

## Status

**Larger context:** We've tried to patch the kernel so it no longer
needs 1k subpage protections in the MMU (ARMv4 feature no longer
available). There are still corruptions happening, so we haven't
accomplished that yet.

**Current goal: alrt-task DABT — CList header corruption confirmed,
identify the writer.**

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
