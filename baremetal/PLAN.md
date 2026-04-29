# Plan — Drive Newton OS to interactive use

## Status

**Current goal: alrt-task DABT — CList header corruption confirmed,
identify the writer.**

The alias audit (prior loop) proved all 15 verify-mmu aliases are
kernel-intent subpage-disjoint. Resumed alrt-task DABT
investigation per the deferred plan.

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

### Next iteration

1. **Add `__dl__`/free tracking** to the live-allocation tracker.
   Eliminates the "same-address recycle" false positives, leaving
   only the genuine partial-overlap and same-address-no-free cases.
2. **Probe `NewBlock`/`NewPtr`** in addition to `__nw__` — these
   allocator entry points may catch what `__nw__` misses (e.g.,
   if InitAlertManager-equivalent uses a different allocator path).
3. **Trace the FIRST partial-overlap** (#118/#120) back via
   `caller_lr` to identify the kernel call sequence. The
   immediate fix layer is wherever that bug lives.

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
