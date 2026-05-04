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
- All 36 guest tests must pass on every commit that touches hypervisor
  functionality (not merely diagnostics):
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-89):** chase the remaining
`evt.ex.fr.store (-48022)` throws fired during REP's installation of
ROM-based "form parts" (built-in packages). The iter-88
`evt.ex.abt.bus` wedge no longer reproduces (see iter-89 cold-boot
re-test below); the system reaches a stable idle state with all kernel
tasks present. Open question now: **why does the flash store reject
soup-index writes during package install?** User hypothesis to check
first: the package manager demand-pages 1-KiB chunks (compressed in
flash), which is a separate path from heap/stack lazy growth that
the iter-83+ 4-KiB allocator patches were aimed at — so the 4-KiB-
everywhere conversion may have left a 1-KiB-aware code path mismatched.

### Iteration 89: re-baseline after iter-87/88 changes

#### Cold-boot snapshot (this iteration, no code change)

`rm -f /tmp/newton-snapshot-*.bin && cargo run --release` for ~80 s
from a clean build:

- Boot reaches REP user-space and proceeds through the normal early
  startup (alarm setup, store enumeration, Query, EntryRemove, etc.).
- **No `evt.ex.abt.bus` throw**, no `UnhandledException`,
  no halt. The system reaches an idle state with `idle` running,
  `pckm` BLK on `PortReceive`, `scrn` on its sema-op group, and the
  rest of the kernel tasks parked on Receive — the steady state we'd
  expect from a healthy boot with no UI activity.
- 14 throws are fired during REP's package-install phase
  (`InstallFormPart` for 4 ROM packages: `#C61AEA9`, `#C61B9B9`,
  `#C61D79D`, `#C621AF1`), all `evt.ex.fr.store` with the same
  `r1=0xffff446a` payload. Each is **caught** by the
  `EntryReplaceCommon` exception handler at `0x002da580`, which
  aborts the in-progress soup transaction (`Abort__13TStoreWrapperFv`
  + `AbortSoupIndexes`) and re-throws via `NextHandler`. REP's
  top-level handler prints `!!! Exception: evt.ex.fr.store (-48022)`
  and continues.
- After the install attempts fail, REP idles on its event loop.
  The system stays alive; built-in packages are simply not
  installed.

So iter-87/88's combined effect (relocating kernel-patch stubs out
of the UND trampoline window + arena-allocating them with overflow
checks + reserving 32 SCRATCH_POOL slots for hypervisor scratch)
did fix the bus-fault wedge as a side effect — likely because the
SCRATCH_POOL overlap was clobbering an unrelated stub literal area
that participated in a later code path.

#### Throw chain (current, simplified)

Throws are issued at multiple `subgt rN, rN, #48128 ; bl Throw`
sites in the soup-index code (`AlterIndexes` 0x347ba0, plus
0x34825c, 0x348520, 0x34934c, 0x34c6c8, 0x34e8d0, 0x351d4c). Each
fires when the underlying `Add__10TSoupIndexFP4SKeyT1` (0x2E75CC) /
`Delete__10TSoupIndexFP4SKeyT1` (0x2E809C) call returns a positive
error code.

Add/Delete tail-calls `_BTEnterKey__10TSoupIndexFP8KeyField` and
either `Commit__10TNodeCacheFP10TSoupIndex` (0x1A4B5B4) or
`Abort__10TNodeCacheFP10TSoupIndex` on failure. The kernel converts
that positive code into `r1 = 106 - 48128 = -48022` ("Frames
internal error") and throws.

`-48022 = kFramesErrInternalError` (per Walter): the generic
"something went wrong inside the object store" sentinel, NOT a
flash-layer code. It's constructed at 7 sites in the soup-index
range (0x347da8, 0x34826c, 0x348558, 0x34934c, 0x34c6c8, 0x34e8d0,
0x351d4c), all with the same encoding `movgt r1, #106; subgt r1,
r1, #48128`. Whatever positive return surfaced from the index/store
op gets folded down to this constant before the throw — so the
throw payload tells us nothing about the underlying cause.

The `TFlashStore::Lookup` probe (HVC 0x6c, installed at 0x000C747C)
captures one Lookup just before Throw #1 with
`r0=0x0c604c04 r1=0x4e r3=0x0cd7c728 lr=0x000c5e74` (returning to
`GetObjectSize__11TFlashStoreFUlPl + 0x80`) — that one *succeeds*
(otherwise the probe halts). So the failure surfaces on a path
*other* than `TFlashStore::Lookup` — possibly inside the
`TStoreObjectWriter` / `Commit__10TNodeCache` chain that touches
the package store layer.

#### Iter-89 in-flight: probe at the soup-index Add/Delete return sites

Patch installed at `0x002E_7654` (`mov r0, r5` in `Add__10TSoupIndex`)
and `0x002E_8140` (same in `Delete__10TSoupIndex`) — HVC #0x81 / #0x82
that capture r5 (= the about-to-be-returned code) and emulate the
original `mov r0, r5`. Captured at cold boot:

| call    | r4 (this)  | root [+16] | retcode | err [+44] |
|---------|-----------:|-----------:|--------:|----------:|
| Add #0  | 0x0cc77a88 |       0x46 |       0 |         0 |
| Add #1  | 0x0c609270 |       0x4a |       0 |         0 |
| Add #2  | 0x0c60922c |       0x4b |       0 |         0 |
| Add #3  | 0x0cc77aac |       0x46 |       0 |         0 |
| Del #0  | 0x0c60a434 |      0x12c |       2 |         2 |
| Del #1  | 0x0c60a434 |      0x12c |       2 |         2 |
| Del #2  | 0x0c60a434 |      0x12c |       2 |         2 |
| Del #3  | 0x0c609270 |       0x4a |       2 |         2 |

Findings:

- **All four Adds succeed** (`_BTEnterKey` → 0). All four Deletes
  return **2** with the same TNodeCache as the Adds (`[+8] =
  0x0c646384` for every call).
- `_BTRemoveKey` returns 2 from one of two sites — `ReadRootNode →
  0` (no root) OR `DeleteKey → 0` (key not found). All four
  Deletes have non-zero `[+16]` (root_node_id), so we're hitting
  the second case: **`DeleteKey` cannot find the entry's key in
  the tree**.
- Del #3 and Add #1 act on the SAME index (`r4=0x0c609270`,
  root=0x4a) with the SAME TNodeCache. The Add wrote a key into
  the cache and returned 0; the later Delete (presumably for a
  different entry in the same soup) couldn't find ITS key.
- Del #0..#2 act on index `root=0x12c`, which we never Added to in
  this run — so its absence makes sense if those entries were
  expected to live there but were never indexed.

#### What this implies, and the next angle (for iter-90)

The kernel's `AlterIndexes` iterates ALL indexes of a soup and
calls `Delete` on each. If the entry being removed was never
indexed in some secondary index, the per-index Delete returns 2
and `AlterIndexes` throws `evt.ex.fr.store(-48022)`.

For these throws to be a real bug (rather than the kernel
encountering "entry not in this secondary index" as expected),
either:

a. **The entry SHOULD be in the index but isn't** — a write that
   was supposed to update the index never landed. Walter's
   endianness hypothesis points here: if the path that BUILDS the
   index from flash on boot uses BE-bytewise reads (or writes via
   misaligned word-LDR/STR with BE assumptions baked in by the SA-
   110 compiler), we'd be reading garbage and inserting the wrong
   keys. Compare `UpdateNode__10TSoupIndex` at `0x002E_9D80`,
   which reads `[r4, #10]; asr #16` — an unaligned word load whose
   semantics differ between BE-32 (Newton's compile target) and
   our LE A53 with SCTLR.A=0.
b. **Einstein hits the same code with the same data and doesn't
   throw**. If true, our hypervisor's flash bytes diverge from
   Einstein's. iter-82 already fixed XOR-3 byte-swizzle on PCMCIA-
   aperture LDRBs; the symmetric write-side path (or write-via-
   word-loop paths like `BasicRead`'s unaligned shift extractor at
   `0x000C_7DD8`) needs verification.

#### Iter-89 follow-up probes installed

Two more probes added at `_BTEnterKey + 0x48` (`0x002E_6EA4` —
just before `bl InsertKey`) and `_BTRemoveKey + 0x3C`
(`0x002E_6F28` — just before `bl DeleteKey`). Both replace
`mov r0, r4` with HVC #0x83 / #0x84; the handler captures
(r4=this, r1=KeyField*, r2=NodeHeader*) and dumps 32 bytes of
the KeyField + 64 bytes of the NodeHeader, then emulates
`mov r0, r4`.

Captured cold-boot data:

- **Add #1** (idx=0x0c609270, root_id=0x4a, NodeHeader at
  `0x0c6465bc`): KeyField encodes string "userConfigurations"
  (BE-pair-swapped per SBA byte-swizzle); NodeHeader is mostly
  zero with `[+0]=0x4a`, `[+8]=0x01ec0000`, `[+12]=0x01fe0000`
  (a fresh root with no keys yet).
- **Delete #3** (same idx=0x0c609270, same root_id=0x4a, but
  NodeHeader now at `0x0c6463ac` — the cache slot was recycled
  between Add and Delete). KeyField is a different key (header
  byte length 0x08, content `e p t a N _ t o s e ...`). The node
  header differs: `[+8]=0x019c0003`, `[+12]=0x01c201b4`,
  `[+16]=0x01d001fe` (a node with multiple-key content).
- **Delete #0..#2** (idx=0x0c60a434, root_id=0x12c, NodeHeader at
  `0x0c6463ac`): an index we never Added to in this run. The node
  has `[+8]=0x01d40001`, `[+12]=0x01e801fe` — non-empty content.

So Delete is reading back a populated B-tree node (loaded fresh
into the cache slot via `ReadANode`/`Read__6TStore` on a cache
miss), not finding the search key in it, and `_BTRemoveKey`
returns 2.

#### Endianness/type-punning hypothesis status

Inventory: there are **1,872 sites** in the ROM matching the
pattern `LDR Rn, [Rm, #imm]; ASR/LSR Rn, Rn, #16` where `imm %
4 != 0` — the canonical BE-mode "halfword in the high half of an
unaligned word fetch" idiom. ARMv4 BE rotates the unaligned word
so the halfword lands at the requested address; ARMv7 LE just
reads the next four bytes — different result.

But the hypervisor already covers this:

- `src/unaligned.rs` — full SA-1100 rotate-LDR emulator,
  triggered by SCTLR_EL1.A=1 raising an alignment fault on
  every unaligned LDR.
- `src/unaligned_inline.rs` — lazy in-ROM stub installer that
  patches each first-faulting unaligned LDR with a native
  AArch32 ROR-based emulation stub so subsequent executions
  don't trap.

Cold-boot log confirms an inline stub installed at
`PC=0x002e9da0` (`UpdateNode`'s suspicious `LDR + ASR #16`),
and 26+ others in the soup-index code range (`0x2E_xxxx`).
So the BE-style unaligned LDRs return the **correct** rotated
value on our LE A53 — that's not where the divergence lives.

The bytes the in-cache NodeHeader is filled with come from
`Read__6TStore` (a memmove of bytes from the flash backing) on
the cache-miss path. If those bytes are corrupted in flash —
either by a previous write that landed at the wrong byte
positions, or by a previous read+rewrite cycle that lost
information — the in-cache node is wrong from the start, and
`KeyInNode`'s search through it can't possibly find a key the
caller is asking for.

#### A/B probes around the flash read/write boundary

Two more probes wired in:

- HVC #0x85 at `UpdateNode + 0x68` (`mov r2, sp` at `0x002E_9DE8`,
  immediately before `bl ReplaceObject`) — captures
  (TStore*, node_id, byte_count, staging_sp) and dumps the first
  64 bytes of the staging buffer.
- HVC #0x86 at `ReadANode + 0x94` (`add sp, sp, #4` at
  `0x002E_9C38`, immediately after `bl Read__6TStore`) —
  captures (result, node_id, NodeHeader*) and dumps the first
  64 bytes of the freshly-read buffer.

**Result**: for node 0x46 specifically, `UpdateNode` #6 wrote
170 bytes whose first 64 bytes were exactly the staging buffer
shown below; the very next `ReadANode` for the same node
(separate cold boot — flash backing intact) returned an identical
64-byte prefix. **The flash round-trip is byte-faithful.**

```
write @ node 0x46, 170 bytes:
  staging+0:  0x00000046 0x00000000
  staging+8:  0x01560005 0x01a6018e
  staging+16: 0x017201e6 0x01bc01fe
  staging+24: 0x00000000 0x00180010
  staging+32: 0x00500061 0x0063006b   ("P" "a" "c" "k" — UTF-16BE)
  staging+40: 0x00610067 0x00650073   ("g" "a" "s" "e")
  staging+48: 0x0000007e 0x00000000
  staging+56: 0x0014000c 0x004f0075

read  @ node 0x46:
  Node+0:     0x00000046 0x00000000  ← same
  Node+8:     0x01560005 0x01a6018e  ← same
  ... (matches byte-for-byte)
```

So Walter's endianness/type-punning hypothesis is NOT the
cause at the flash-store layer. The bytes that landed on flash
are exactly what the kernel staged, and the bytes that came
back are exactly what landed.

#### Where the divergence actually lives (next angle)

A later `ReadANode` for node 0x46 (after intervening updates we
truncated at the log cap) DOES return a different byte pattern,
and that is the one `_BTRemoveKey`'s `DeleteKey` searches in
when the throw fires. So the picture is:

1. `Add` runs, `_BTEnterKey` modifies the in-cache node, Commit
   flushes via UpdateNode → ReplaceObject → flash. ✓ correct.
2. Cache slot eviction recycles the slot for a different node.
3. Some later operation triggers another UpdateNode for node
   0x46 (we see `0x46`'s persisted bytes shift between the
   first read and a later read with no UpdateNode #6+ visible
   under the truncated 16-call probe cap; the uncapped probe
   times out before reaching that phase).
4. `Delete` reads the latest persisted bytes and searches for
   the entry's key, doesn't find it, returns 2 → throw.

So either the kernel is supposed to have indexed the entry into
some secondary index but didn't (a real upstream bug), or the
soup intentionally allows entries to be present in primary but
missing from secondary indexes and the kernel's `AlterIndexes`
handles -47990 gracefully (Einstein-side oracle needed).

The simplest next probe: capture the FAULT chain at the throw
site by adding a probe at AlterIndexes' `bl Throw` (`0x347db4`)
to see the index_id whose Delete returned 2 vs the index_id
the entry was originally indexed in. If they're the same
index, our per-index data is corrupt; if they're different
indexes, we hit a secondary-index path Einstein presumably also
hits without throwing — meaning either Einstein swallows it or
we're constructing the indexes differently at boot.

c. **`AbortSoupIndexes` re-entry.** Throws #2..#5 are contiguous
   and look like the same throw bubbling through nested handlers
   (`EntryReplaceCommon` → `NSSend` → `NextHandler` chain) — not
   four independent failures. Worth confirming once we understand
   the primary cause.

#### Iter-89 idx_id divergence

Probe at `AlterIndexes + 0x204` (`movgt r1, #106` at `0x0034_7DA4`)
fires on the throw path with the failing `idx_id` (= `r4->[+12]`)
in scope. Combined with extending the soup-index Add/Delete
return probes to also capture `[+12]`:

- **Adds touch many idx_ids:** `{0x31, 0x39, 0x3a, 0x4c, 0x4d,
  0x4e, 0x4f, 0x73, 0x74, 0x76, 0xea, 0xeb, 0xec, 0x105, 0x106,
  0x107, 0x10a, 0x10b}` (139 successful Adds total over the
  boot). Includes 0x4f and 0x3a, which are the indexes the later
  failing Deletes touch.
- **Failed Deletes touch** `idx_id=0x4f` (3×, root_id=0x12c)
  and `idx_id=0x3a` (1×, root_id=0x4a) — but the Adds to those
  indexes succeeded earlier with **different keys** than the
  Deletes search for.

The sharpest example: at boot
  - **Add #125** inserts a key into `idx 0x4f` (this=0x0c60a434,
    succeeds, retcode=0).
  - **Delete #0** later searches `idx 0x4f` for a different key
    (same `this=0x0c60a434`), `_BTRemoveKey` returns 2 (key
    not in tree), and AlterIndexes throws.

So Delete is looking for an entry's per-index key that was never
Added to that index in this boot. AlterIndexes already short-
circuits the throw via `if GetEntrySKey returns 0 → success
path` (no key for this index for this entry → fine, skip). The
fact we reach the throw means `GetEntrySKey` returns NON-ZERO
for the entry: the entry "claims" to have a key for this index,
but the index's tree doesn't contain it.

Possible causes:

1. The entry was Added with a code path that didn't reach
   `_BTEnterKey` for `idx 0x4f` / `0x3a` specifically.
2. The entry was Added before the index existed, then the index
   was created without re-indexing pre-existing entries.
3. A B-tree compaction/rebalance lost the key.
4. The soup state at boot is supposed to be pre-populated from
   flash but our flash content diverges from Einstein's at the
   relevant offsets — and REP is operating on a soup whose
   indexes don't actually contain the entries it queries.

#### Iter-89 KeyField comparison + the bigger picture

Probe at `EntryRemoveFromSoup__FRC6RefVar` (`0x002D_A26C`,
HVC #0x88) captured:

- ER #0 / #1: ref `#C61D1ED` (REP removing the same entry
  twice — likely the second-throw retry of an already-thrown
  remove).
- ER #2: ref `#C60A21D`.

Pre-Insert/pre-Delete KeyField dumps (always-on for the failing
TSoupIndex instance `r4=0x0c60a434` / `0x0c609270`):

- Insert #1 (idx 0x3a): KeyField size 42 bytes, content
  UTF-16BE "userConfigurations" — the system-config-symbol
  symbol-keyed entry.
- Insert #117 (idx 0x3a): KeyField size 10 bytes, binary
  content `00 02 00 43 00 00 01 20 00 72`.
- Insert #125 (idx 0x4f): KeyField size 18 bytes, binary
  content starting with `00 00 00 54 00 6f 00 20 00 44 00 6f
  00 00 ...` (could be packed UTF-16BE "To Do\0..." plus
  trailing data).
- **Delete #3 (idx 0x3a)**: KeyField size 8 bytes, binary
  `00 00 00 00 00 00 00 49`.
- **Delete #0..#2 (idx 0x4f)**: KeyField size 8 bytes,
  binary `00 00 00 00 00 00 01 2b`.

Insert and Delete KeyFields are structurally and contentually
different — Insert puts in a 42-byte symbol key, Delete searches
for an 8-byte binary key. They're for different entries.

The B-tree's content (read back from the cache slot at
`0x0c6463ac` = root_id 0x4a, post-cache-evict + Read__6TStore)
does contain Insert #117's payload (visible in the Node header
shape: `Node+8=0x019c0003` etc., post-Insert state). But
Delete's 8-byte search key isn't in there.

#### Boot status (iter-89)

After all the throws fire, REP's exception handler catches each
one and moves on. Boot reaches UI: **16 `screen.blit` events
fire** in this run (rendering pen-track / status-bar regions),
and the system reaches the kernel idle state with `pckm` etc.
parked on PortReceive. So the iter-89 throws are diagnostic
noise rather than a blocker — the system actually boots through
to interactive use.

The remaining work is to determine whether these throws indicate
a real soup-state divergence vs. Einstein (entries that should
have keys in `idx 0x4f` / `0x3a` but don't), or whether Einstein
hits the same throws and we're just observing what was already
working noise. Compare against `NewtonProbe baremetal/roms/
newton.rom _Data_/Einstein.rex 90` — if Einstein also throws
`evt.ex.fr.store` at `AlterIndexes:347db4`, we're already
parity. If not, we have a real divergence to chase upstream of
where the entry's index-key is supposed to be inserted.

#### Found while diagnosing: ORIG_PCS table overflow

The `record_original` side-table that lets `shadow_stub`'s
liveness analyser see the pre-patch instruction at any
patch_probe-rewritten PC was capped at 64 entries. With this
iteration's 5 new probes (HVC #0x81..0x87) we're at 70 — the
warning fired silently, dropping the last 6 entries' originals.
Among the dropped PCs is `0x002E_9C38` (in `ReadANode`), exactly
the function we're investigating, where shadow_stub's liveness
analyser may then mis-classify scratch registers in adjacent
SBA / unaligned-inline stub installations.

Bumped `ORIG_CAP` 64 → 128 and converted the overflow path from
a kprintln warning to a hard halt: silently continuing with a
partial table is exactly the kind of "subtle rotational
divergence later" Walter has seen before.

36/36 guest tests pass (no hypervisor code changed in iter-89);
cold boot reaches REP idle with 14 caught throws.

### Iteration 87 follow-up: arena allocator for stubs + audit fixes

Audit of all fixed-address allocations after iter-87 found two latent
overlaps the manual-constant scheme couldn't have caught:

1. **`NEW_STACK_PAD_WRAPPER_PC` (`0x00FF_FE80`, NOT installed) overlapped
   FTIME_STUB (`0x00FF_FE70..0x00FF_FE84`) and FDATE_STUB
   (`0x00FF_FE84..0x00FF_FE98`).** Iter-87's relocation chose addresses
   inside the `0xFE60..0xFEC0` gap without accounting for the unused
   wrapper slot.

2. **`HYP_TRAMP_SCRATCH_BASE = 0x0600_F000` (slot 7680 of SCRATCH_POOL)
   overlapped `shadow_stub`'s ScratchVA-stub literal area.** Pre-iter-85
   `SCRATCH_POOL_SIZE = 64 KiB` placed HYP_TRAMP_SCRATCH at the *end*
   of the pool — out of `NEXT_SCRATCH_SLOT`'s reach. Iter-85 grew the
   pool to 384 KiB and bumped ScratchVA stubs from 2976 to 10351;
   slots 7680–7701 are now allocated, so every UND/DABT trap silently
   overwrote the literals of any stub at those slot indices.

#### Fix 1: patch-stub arena (`src/rom_patches.rs`)

All kernel-side native-primitive stub addresses are now allocated at
install time from a single arena spanning `0x00FF_FD80..0x00FF_FEC0`
(320 B). Each `apply_*` function calls
`alloc_patch_stub(n_words, name)` and gets back the next free PC.
Overflow halts loudly; the boot log prints every allocation so the
layout is visible. Per-stub constants (`DEBUG_STR_STUB_PC`,
`DEBUGGER_STUB_PC`, `FTIME_STUB_PC`, `FDATE_STUB_PC`,
`RESOLVE_FAULT_WRAPPER_PC`, `LOCK_HEAP_RANGE_WRAPPER_PC`,
`UNLOCK_HEAP_RANGE_WRAPPER_PC`, `NEW_STACK_PAD_WRAPPER_PC`) are
deleted; functions that need to address into their own stub pass
`wrapper_pc` as a local.

Boot-time allocation order under the current configuration:
- DebugStr / Debugger stubs (8 B each) → `0x00FF_FD80`, `0x00FF_FD88`
- FTimeInSeconds / FDateFromSeconds stubs (20 B each) → `0x00FF_FD90`, `0x00FF_FDA4`
- ResolveFault wrapper (96 B) → `0x00FF_FDB8`

Total 152 B; cursor lands at `0x00FF_FE18` with 168 B slack before
the FPA bypass stub at `0x00FF_FEC0`.

#### Fix 2: reserve first 32 slots of SCRATCH_POOL for hypervisor scratch

`shadow_stub::NEXT_SCRATCH_SLOT` initial value bumped from 0 to
`RESERVED_SCRATCH_SLOTS = 32` (256 B). `HYP_TRAMP_SCRATCH_BASE`
relocated from `0x0600_F000` to `SCRATCH_POOL_IPA` (`0x0600_0000`).
The trampoline's footprint (UND saves at `+0x00..0x1C`, DABT saves
at `+0xA0..0xAC`) fits comfortably inside the 256 B reservation.

#### Audit summary (clean after fixes)

- **Trampoline tail (`0x00FF_FD80..0x0100_0000`):** RESOLVE_FAULT
  wrapper, kernel-patch stubs, FPA bypass, UND tramp, SBA pre/post,
  DABT tramp, UND return stub — all back-to-back, no overlaps.
- **Stage-2 IPA layout:** ROM (`0x0..0x1000000`), RAM
  (`0x4M..0x5M`), SCRATCH_POOL with 32-slot hypervisor reservation
  (`0x6000000..0x6060000`), SHADOW_POOL (`0x6060000..0x6070000`),
  framebuffer (`0xE000000+`), MMIO (`0xF000000+`), GIC
  (`0x2F000000+`) — non-overlapping with healthy gaps.
- **HVC immediates:** all unique.

36/36 guest tests pass; cold boot reaches the same iter-88 wedge
(`evt.ex.abt.bus`).

### Iteration 87: relocate kernel-patch stubs out of the UND trampoline window

#### Symptom

After iter-86, boot reaches REP `TimeInSeconds()` then wedges:

```
*** unrecognised UND: insn=0xe1400170 at PC=0xffff54 SPSR_und=0x80000110
  src_mode=0x10 (USR) … SP_und=0xc006000 LR_und=0xffff58
```

`0xffff54` is the UND trampoline's `hvc #UND_TAG`. handle_und's
catch-all fires because USR mode itself ran the HVC (HVC at PL0 → UND).

#### Root cause

The kernel-patch native-primitive stubs `DEBUG_STR_STUB_PC=0xffff30`,
`DEBUGGER_STUB_PC=0xffff38`, `FTIME_STUB_PC=0xffff40`, and
`FDATE_STUB_PC=0xffff60` (in `src/rom_patches.rs`) lived **inside**
the region `patch_und_vector` writes (UND trampoline at
`0xffff00..0xffff60`, SBA pre-fault stub at `0xffff60..0xffff80`).

Install order: `apply_717006_patches` writes the stubs first, then
`patch_und_vector` overwrites them. The kernel-patched BL/B sites
at `0x89b80` (FTimeInSeconds), `0x8A8A8` (FDate), `0x38ce6c/70`
(DebugStr/Debugger) still pointed at the now-clobbered stub
addresses. When REP eventually called `TimeInSeconds()` →
`FTimeInSeconds` → patched `b 0xffff40`, USR jumped into the middle
of the trampoline body (base+16: `ldr r2, [r12, #0x14]`) and ran
forward through `mov r0, lr` (writing LR_usr=0x89b74 to
`[r12+8]`=`0x0cd7c954` — visible in the wedge stack dump), the
two `msr cpsr_c` insns (no-op from USR), and finally the trampoline's
`hvc #0x10` at `0xffff54` — UND from USR, trampoline runs, HVC fires,
handle_und's catch-all halts.

The `LR_usr=0x89b74` and `R0=0x89b74` in the wedge-time register
dump are the smoking gun: that's `mov r0, lr` from trampoline
base+18 having executed in USR, with LR still set by FTimeInSeconds's
`bl 0x1c094b0` at `0x89b70` because the patched stub never ran the
real FTimeInSeconds work and never returned through any other BL.

The bug was latent before iter-86 because earlier ROM init avoided
calling `FTimeInSeconds` / native-primitive paths; REP user-space
boot is the first call site that exercises them.

#### Fix

Relocate all four stub PCs to the gap between
`RESOLVE_FAULT_WRAPPER` (ends at `0x00FF_FE5C`) and `FPA_BYPASS_STUB`
(starts at `0x00FF_FEC0`):

```
DEBUG_STR_STUB_PC = 0x00FF_FE60   // 2 words / 8 B
DEBUGGER_STUB_PC  = 0x00FF_FE68   // 2 words / 8 B
FTIME_STUB_PC     = 0x00FF_FE70   // 5 words / 20 B
FDATE_STUB_PC     = 0x00FF_FE84   // 5 words / 20 B
```

56 B used, well clear of the 64 B FPA bypass and trampoline ranges.

#### Verification

- Boot now sails past `TimeInSeconds()` through REP user-space (many
  `REP>` query lines) before tripping a separate `evt.ex.abt.bus`
  bus-fault wedge — that's iter-88's territory.
- 36/36 guest tests pass.

#### Diagnostics added (kept)

- `record_und_history` / `dump_und_history` in `src/trap.rs` — a
  32-entry rolling buffer of recent UND faults (PC, insn, mode, sp,
  lr_usr). Dumped on the catch-all halt and instrumental in finding
  this bug.
- `return_to_guest_from_und` halts loudly if `elr` lands inside
  `0xffff00..0xffff60` (UND trampoline body) or `0xffec0..0xffefc`
  (FPA bypass) with USR-mode SPSR — those are never legitimate
  ERET targets. Caught by exclusion: SBA_POST_TRAMP at `0xffff80`
  and UND_RETURN_STUB at `0xffffe4` are intentionally allowed.
- USR-stack and JT-thunk dump in handle_und's catch-all — reads via
  stage-1 walk (`guest_mem::translate_va`) so kernel VAs resolve.

---

### Iteration 86: skip the per-test rebuild via semihost-load

#### Problem

`run-all.sh` ran `cargo build --release` once per test (36 times)
because each test's `.bin` was embedded into the hypervisor via
`include_bytes!(env!("NH_GUEST_TEST_PATH"))`. Each rebuild was a
relink (LTO) — ~10s each, ~5 min total wall.

#### Fix

Two delivery modes for the test binary, selected by the value of
`NH_GUEST_TEST`:

- **embed** (`NH_GUEST_TEST=path/to/test.bin`): compile-time
  `include_bytes!` — current behavior, fast for iterating on a
  fixed test where cargo's incremental build only re-emits one
  object + relinks.

- **semihost-load** (`NH_GUEST_TEST=1`): build the hypervisor as
  a generic test image with no embedded bin; load the test
  binary at boot via Arm semihosting. The path is passed in
  QEMU's `-semihosting-config arg=<path>`. iter-86 added
  `load_test_bin_via_semihosting` in `src/guest_mem.rs` that
  calls `SYS_GET_CMDLINE` → `SYS_OPEN` → `SYS_FLEN` → `SYS_READ`
  to fill `GUEST_TEST_BIN_BUF` before stage-2 setup.

`build.rs` sets the `nh_guest_test_embed` / `nh_guest_test_semihost`
sub-cfgs (both also set `nh_guest_test`); `guest_mem.rs` and the
loader pick the right path.

`run-test.sh` and `run-all.sh` default to semihost-load. Set
`NH_GUEST_TEST_EMBED=1` to opt into the legacy embed mode.

#### Result

`run-all.sh` wall time: **~5 minutes → 6.7 seconds**. 36/36 tests
pass under both modes.

---


<!-- Older iteration retrospectives (iter-77 and earlier) live in
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
