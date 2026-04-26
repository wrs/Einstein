# Plan — replace `StubVariant::Stack` with a scratch-VA variant

Self-contained execution plan. The next session should be able to pick this
up without re-reading the prior conversation. Read this in full before
making any changes.

## Why we're doing this

`INVESTIGATION.md` "Currently at — root cause narrowed" pinned the Phase B
TCardServer wedge to a `TPhys` aliasing decision driven by an
unobserved-on-our-side stack fault in `name` task's `MoveFreeBlock` at
trace ~147 k. The leading hypothesis is that `shadow_stub`'s
`StubVariant::Stack` PUSH/POPs onto the user task's mode-banked SP, and
those writes lazily map a stack page that Einstein's run leaves
unmapped — masking the kernel-mode stack-fault chain Einstein takes
(`TStackManager::Fault → STKF → kernel-side TStackPage` allocation at
VA `0x0c318000` backed by PA `0x0402b000`). Without that allocation,
PA `0x0402b000` ends up recycled into the TCardMessage write region
and corrupts the user heap.

`INVESTIGATION.md` "Shadow-stub Stack-variant experiment" tested the
direct hypothesis by forcing all `Stack`-variant sites to UDF
emulation. That broke very early in BootOS (the SBA pre-fault probe
recurs into the kernel's not-yet-ready DABT handler), so the swap
isn't usable as-is — but it confirmed the variant is depended on for
1 694 ROM sites (~6 % of all byte-access patches), all in PC range
`0x000225d8 .. 0x003ad334`.

A non-stack-touching variant is the cleanest test: keep the inline
fast path (no UDF round-trip in BootOS), drop the stack-touching side
effect, see whether the alias still happens. If the wedge moves, the
hypothesis is confirmed and we have a fix; if not, we've eliminated
the suspect and look elsewhere (most likely the heap-allocator
divergence past Einstein's 1 063-call recording cap).

## Prior attempts (don't re-do these without changes)

Search `jj log` for these change IDs:

- **`kwmklzru` (2026-04-20) — PC-relative save inside the stub** worked
  and is the closest precedent to this plan. Pool was at IPA
  `0x0180_0000` (stage-2 RW), each stub had a 4-byte save slot at the
  tail, accessed via `STR scratch, [PC, #disp]`. **Superseded by
  `lwxxwtnp` because of post-MMU dispatch failure**: a `B` from a
  patched ROM site (e.g. `0x0181C180`) hit the kernel L1 hole at
  `L1[0x18] = 0` and PABT'd. That's the constraint we're reviving and
  this plan addresses.
- **`lwxxwtnp` (2026-04-22) — TPIDR_EL0 save** moved the pool to ROM
  aperture (RO) and used `MCR p15,0,Rt,c13,c0,2` (TPIDRURW) for the
  scratch save. Only 1 slot; can't preserve CPSR alongside, so sites
  that need both can't use it.
- **`mvoossru` (2026-04-24) — Stack fallback** added the current
  `StubVariant::Stack` because TPIDR-only couldn't cover sites where
  every dead-reg candidate is genuinely live.

The PMU-reg ideas (PMCCNTR, PMSELR via `PMUSERENR.EN=1`) were
considered and rejected during prior investigation: PMCCNTR leaks
through preemptive context switches (kernel doesn't save/restore),
and PMSELR is too narrow (5 bits).

## Chosen design — option 1(b)

**Stub code in ROM aperture (RO, current `SBA_STUB_POOL_IPA`); per-stub
scratch data in a new RW carve-out reachable from the stub via PC-rel
addressing through a literal in the stub itself.**

The stub gets the scratch-data VA into a register by `LDR Rd, [PC,
#lit]` from a literal stored in the (RO) stub. Then `STR/LDR` via that
register touches the RW carve-out. The pool of saved-context bytes
lives at a VA we add to the kernel's L1 — NOT inside the kernel's own
data region.

### Address-space carve-out

- **Carve-out IPA**: `0x0180_0000` (1 MiB), the same VA `kwmklzru`
  used. The kernel L1 has `L1[0x18] = 0` natively, so it's ours to
  populate without conflict. (Verify with `dump_stage1_walk(0x01800000)`
  before proceeding.)
- **Carve-out PA**: a new static `SCRATCH_POOL: [u8; SCRATCH_POOL_SIZE]`
  in `src/shadow_stub.rs`. 1 694 sites × 8 B/site = 13.6 KiB minimum;
  round up to **64 KiB** to leave room for ROM revisions and to align
  to a stage-2 L3 boundary cleanly. Stage-2 maps it RW.

### Stage-1 (kernel L1) integration

`fix_stage1_xn_bits` (`src/guest_mem.rs:302`) already runs on every
TTBR0 write. Extend it (or add a sibling `install_scratch_pool_l1_entry`)
to write `L1[0x18]` to a 1 MiB section descriptor:
```
section: PA = 0x0180_0000  (= our carve-out IPA)
         AP[1:0] = 0b11    (RW from any mode, including USR)
         AP[2]   = 0
         domain  = 0       (matches kernel domain 0; manager access)
         XN      = 1       (data-only — no instruction fetch needed)
         C/B     = 0b11    (Normal cacheable WB, matches kernel ROM)
         encoding: 0x0180_0C1E  (verify against ARM ARM B3.5.1 short
                                 desc, "section descriptor")
```
**XN = 1** is the safety belt: an accidental `B` to this region would
PABT loudly instead of executing scratch data.

The patch must be re-applied on every M=0→M=1 transition (same hook
as `fix_stage1_xn_bits`), and on guest TTBR0 rewrites if observed.
Also handle the M=1→M=0 case: BootOS's soft-reset path may zero the
L1, so we re-patch on the next M=0→M=1 — already the existing pattern.

### Stage-2 mapping

The 1 MiB carve-out is in the ROM-aperture IPA range (0..16 MiB), so
it currently falls under the 2 MiB ROM RO L2 block. Refine that 2 MiB
slot to an L3 table (256 valid 4 KiB pages) the same way
`install_tick_page` (`src/stage2.rs:417`) does for the tick page:

- New static `S2_L3_SCRATCH: PageTable = PageTable([0; 512])`.
- For each 4 KiB page in `0x0180_0000..0x0181_0000` (16 pages for a
  64 KiB scratch pool), set `l3[i] = scratch_host_pa + i * 0x1000 |
  PAGE_NORMAL_RW`.
- For pages outside the scratch range but inside the same 2 MiB block,
  leave invalid (faults to `handle_data_abort` like before, which is
  fine because nothing should access those addresses).
- Replace `S2_L2[0x18 / 1] = ...` with a table descriptor pointing at
  `S2_L3_SCRATCH`. (L2 index = `0x0180_0000 / 0x0020_0000` = `0xC`.)
- Issue stage-2 TLB invalidation: `tlbi vmalls12e1; dsb ish; isb`.

### Stub layout (16 words, was 12)

Per-stub layout — slot index in parens:
```
(0)  MCR  p15,0,scratch_addr,c13,c0,2   ; TPIDRURW <- caller scratch_addr
(1)  LDR  scratch_addr, [pc, #lit_off]  ; scratch_addr <- per-stub scratch VA
(2)  STR  scratch_ea, [scratch_addr]    ; save caller scratch_ea
(3)  STR  scratch_fl, [scratch_addr,#4] ; save caller scratch_fl
(4)  MRS  scratch_fl, cpsr              ; save NZCV (when sfl-saving needed)
(5)  ADD/SUB scratch_ea, Rn, #imm_high  ; EA compute, slot 1 of 2
(6)  ADD/SUB scratch_ea, scratch_ea, #imm_low | NOP (reg-offset / single-step)
(7)  CMP  scratch_ea, #XOR_LIMIT
(8)  EORLO scratch_ea, scratch_ea, #xor
(9)  MSR  cpsr_f, scratch_fl            ; restore NZCV
(10) <access>[cond] Rt, [scratch_ea]    ; native byte/halfword access
(11) LDR  scratch_fl, [scratch_addr,#4] ; restore caller scratch_fl
(12) LDR  scratch_ea, [scratch_addr]    ; restore caller scratch_ea
(13) MRC  p15,0,scratch_addr,c13,c0,2   ; restore caller scratch_addr <- TPIDRURW
(14) B    orig_pc + 4                   ; back-branch
(15) <literal: per-stub scratch VA = SCRATCH_POOL_IPA + slot_idx * 8>
```

PC at slot N has value `stub_ipa + N*4 + 8`. The literal at slot 15
is at offset `+60` from the stub base; the `LDR` at slot 1 has
`PC = stub_ipa + 12`, so `disp = 60 - 12 = +48` (bytes). Within the
±4095 byte range, encoded with `U=1, imm12=0x30`.

Per-stub scratch VA = `SCRATCH_POOL_IPA + slot_idx * 8` (each stub
gets a unique 8-byte slot). Computed at install time and baked into
the literal.

### Why this is IRQ-safe

Each stub has its OWN scratch slot (`slot_idx`-keyed), not a shared
slot. If an IRQ fires inside one stub and the IRQ handler triggers a
different byte-access stub, that other stub uses its own slot. No
race.

The TPIDRURW use IS shared, but it's used for one register only and
is bracketed by the stub's MCR (slot 0) and MRC (slot 13). The window
between MCR and MRC is short (15 instructions). If an IRQ fires in
that window AND the IRQ handler executes a stub that touches
TPIDRURW, the IRQ-mode stub's MCR clobbers our save, MRC restores it
to the IRQ-mode value. On return to the outer stub, its MRC reads the
wrong value.

**This is a real hole.** Mitigations, in increasing order of effort:

1. **Document and tolerate.** Per `lwxxwtnp`'s own caveat, this same
   risk exists in the current TPIDR-only DeadReg variant ("if a
   higher-priority exception handler itself fires a shadow-stub the
   saved value is clobbered; in practice the kernel runs with I/F
   masked in the byte-access-heavy paths and this hasn't surfaced").
   Same applies here.
2. **Two RAM slots, no TPIDRURW.** Use `scratch_ea` to hold the
   address pointer too: load addr first into `scratch_ea` (clobbering
   caller's value, but we save it to `[addr]` immediately). Walk the
   slot-arithmetic carefully — the chicken-and-egg of clobbering
   `scratch_ea` before saving it is avoided by using the load AS the
   first save (the value being loaded is the target address; the
   caller's `scratch_ea` is then immediately written via `STR
   scratch_ea_prev_value`... but this needs a third register to bridge.
   Probably needs to allocate 3 scratch registers from the
   operand-excluded picker; verify `pick_operand_excluded_pair` can be
   extended to `pick_operand_excluded_triple` in all observed sites).
3. **Disable IRQs across the stub.** Save CPSR.I, set I=1, do the
   work, restore. Adds 2 instructions and burns a register for the
   CPSR save — but eliminates the race.

**Start with mitigation 1** (document the risk, match prior precedent).
If guest tests or boot reveals a TPIDRURW race, escalate to
mitigation 2 or 3.

## Implementation steps (in order)

Each step ends in a working tree that builds + passes
`baremetal/guest-tests/scripts/run-all.sh`. **Do not skip** the test
runs between steps — a regression caught at step 3 with step 4
already piled on is much harder to debug.

### Step 1 — verify the address-space assumption

Before writing any code, confirm the carve-out VA is actually free in
the kernel's L1.

- Build with `--features quiet trace`, cold-boot, snapshot at any
  late point (HVC #0x20 from a guest-test, or use the existing
  autosave).
- Read the snapshot's RAM to inspect L1 at TTBR0 base (typically
  `0x0400_0000`):
  ```bash
  rg --no-filename -P 'GUEST_RAM' baremetal/src/snapshot.rs  # confirm offset
  ```
- Confirm `L1[0x18]` (4-byte word at offset `0x60` into the L1 table)
  reads as `0`. If non-zero, pick a different VA (try `0x0190_0000`,
  `0x01A0_0000` — verify `L1[0x19]`, `L1[0x1A]` are 0).
- Also confirm post-`fix_stage1_xn_bits` state — that pass might
  rewrite the entry, which would invalidate the assumption.

If the assumption holds, proceed. If not, document the finding in
`INVESTIGATION.md` and stop — this plan needs the carve-out VA.

### Step 2 — add the static + stage-2 mapping

In `src/shadow_stub.rs`:
```rust
pub const SCRATCH_POOL_IPA: u32 = 0x0180_0000;
pub const SCRATCH_POOL_SIZE: usize = 64 * 1024;       // 16 4 KiB pages
pub const SCRATCH_BYTES_PER_STUB: usize = 8;
pub const SCRATCH_POOL_STUB_CAP: usize =
    SCRATCH_POOL_SIZE / SCRATCH_BYTES_PER_STUB;       // 8192

#[repr(align(4096))]
pub struct ScratchPool([u8; SCRATCH_POOL_SIZE]);
static mut SCRATCH_POOL: ScratchPool = ScratchPool([0; SCRATCH_POOL_SIZE]);

pub fn scratch_pool_host_pa() -> u64 {
    addr_of_mut!(SCRATCH_POOL) as u64
}
```

In `src/stage2.rs`, add `install_scratch_pool` modeled on
`install_tick_page`:
- New static `S2_L3_SCRATCH: PageTable`.
- L2 index = `0x0180_0000 / 0x0020_0000` = `0xC`. (Sanity-check: that
  L2 currently maps the ROM aperture's 2 MiB block covering
  `0x0180_0000..0x01A0_0000`.)
- For each 4 KiB page `i` in `0..16`:
  - `l3[i] = scratch_pool_host_pa + i*0x1000 | PAGE_NORMAL_RW;`
- L2[0xC] = `S2_L3_SCRATCH | DESC_VALID | DESC_TABLE`.
- Stage-2 TLB invalidation.
- Call from `init` after `install_tick_page`.

Wire a small guest test (`test_shadow_stub` already exists; extend
it) that does `STR Rt, [VA]` / `LDR Rt, [VA]` to confirm the
carve-out is RW from USR mode. Run pre- and post-MMU.

### Step 3 — add the L1 entry maintenance

In `src/guest_mem.rs`, extend `fix_stage1_xn_bits` (or add a separate
helper invoked from the same callers — cleaner) to:
- After the existing L1 walk, write `L1[0x18] = 0x0180_0C1E`
  (or whatever encoding the verify step in Step 2 confirmed). Use
  `unsafe { ram.add(0x18).write(SCRATCH_L1_DESC); }`.
- The `dprintln!` budget already gates re-walk noise; the new line
  should also be gated.

Verify by reading the L1 from a fresh boot snapshot — `L1[0x18]`
should now be the expected section descriptor. Walk
`dump_stage1_walk(0x01800000)` from EL2 and confirm it lands on PA
`0x01800000` with AP=0b11.

### Step 4 — extend the stub builder

In `src/shadow_stub.rs`:
- Add `SBA_STUB_WORDS = 16` (was 12). Recompute `SBA_STUB_BYTES`,
  `SBA_STUB_MAX`. Pool capacity drops from
  `(0x00FF_FF00 - 0x00E0_0000) / 48 ≈ 43 685` to
  `(0x00FF_FF00 - 0x00E0_0000) / 64 ≈ 32 764`. We need ≥ 27 633
  inline stubs total. Verify the new cap is sufficient; if marginal,
  grow `SBA_STUB_POOL_END` by extending the carve-out (the hole at
  `0x00FF_FF00..0x00FFFF60` is currently used by trampoline body —
  don't touch it).
- New variant: `StubVariant::ScratchVA { sfl: u32 }`. Replace the
  `Stack { sfl }` arm in `emit_inline_stub`'s liveness-fail branch
  with `ScratchVA { sfl }`.
- Decision on `pick_operand_excluded_pair`: confirm 2 regs is enough
  (yes — same as Stack variant).
- New helper `encode::str_pc_rel` / `ldr_pc_rel` (cribbed from
  `kwmklzru` — see the `enc_str_pc_rel` / `enc_ldr_pc_rel` helpers
  in that change). Add unit tests for the encoding.
- New `encode_inline_stub` arm for `StubVariant::ScratchVA`. The
  literal at slot 15 is `SCRATCH_POOL_IPA + slot_idx * 8`. The
  `LDR scratch_addr, [PC, #lit]` displacement is fixed: from slot 1
  PC (`= stub_ipa + 12`) to literal (`= stub_ipa + 60`) is `+48`
  bytes — bake it as `U=1, imm12=0x30`. Verify with the encoder unit
  tests.
- Keep `StubVariant::Stack` enum variant + `encode_inline_stub` arm
  alive but unreachable from the fallback (used only by tests at
  lines 2677/2723) — preserves the regression coverage of the old
  encoder.

### Step 5 — slot accounting

Each ScratchVA stub needs a per-slot scratch slot in `SCRATCH_POOL`.
The simplest mapping: `scratch_slot_idx = stub_slot_idx`. With the
stub pool cap dropping to ~32 764 and the scratch pool cap at 8 192,
we need to either:
- Grow the scratch pool to ≥ 32 764 × 8 = 256 KiB (trivial — just
  bump `SCRATCH_POOL_SIZE` and the L3 page count in step 2);
- OR allocate scratch slots only for `ScratchVA` stubs (separate
  `NEXT_SCRATCH_SLOT: AtomicUsize`). DeadReg stubs don't need a
  scratch slot. Saves memory; needs a small bookkeeping helper.

**Pick the second** — it's tighter and matches the actual ScratchVA
site count (~1 694 in the live ROM, capped at 8 192 to leave headroom
for future revisions).

### Step 6 — guest test

Extend `guest-tests/tests/test_shadow_stub.S`:
- A subtest that explicitly exercises a Stack-variant-eligible site
  (one where every R0..R3 / R12 / R14 is live across the access).
  After the patch, that site uses ScratchVA. Test that:
  - The byte access reads/writes the correct value (existing pattern).
  - Caller's R0..R3, R12, R14 round-trip through the stub unchanged
    (already covered by existing subtests but verify).
  - CPSR NZCV is preserved (stash flags before, check after).
- Run from USR mode (existing) AND from a PL1 mode (SVC) — each
  exercises a different banked SP, but ScratchVA doesn't touch SP, so
  the test value is in confirming no regression.

### Step 7 — boot the ROM

```bash
rm -f /tmp/newton-snapshot-*.bin
mkdir -p /tmp/phaseB-2026-04-26-scratchva
cargo build --release --features quiet
timeout 120 cargo run --release --features quiet 2>&1 \
  | tee /tmp/phaseB-2026-04-26-scratchva/qemu_notrace.log

# Then with trace, for the first-occurrence and AddPgPAndPerm comparisons:
rm -f /tmp/newton-snapshot-*.bin
cargo build --release --features trace,quiet
timeout 180 cargo run --release --features trace,quiet 2>&1 \
  | tee /tmp/phaseB-2026-04-26-scratchva/qemu_trace.log
```

Compare against the prior baseline:

- **Wedge marker**: does the BootOS canary still fire at trace
  ~169 986 with R0=0x0cc80c80? If yes, the alias is not driven by
  Stack-variant — pivot to the heap-allocator-divergence angle. If
  no, where do we wedge instead?
- **AddPgPAndPerm divergence**: re-run the audit
  ```bash
  cat > /tmp/extract_va_pa.awk <<'EOF'
  ...same as before, see INVESTIGATION.md "Currently at"...
  EOF
  ```
  and compare `qemu_trace.va_pa` against
  `/tmp/phaseB-2026-04-25/einstein.va_pa`. The first-divergent
  `Remember` call should move (or the divergence may be gone).
- **`name` task fault**: grep for
  `0x001f83e4 TStackManager::Fault` in the trace. Currently we hit
  it at trace 57 886 then 169 462. If we now also hit it at ~147 k
  (matching Einstein's 147 584), the hypothesis is confirmed.

### Step 8 — commit + write up

If the experiment confirms the hypothesis (boot advances past trace
170 k, or `TStackManager::Fault` fires at the Einstein-matching
point), land the change as a real commit (drop the WIP prefix). The
commit message should record:
- The mechanism (Stack-variant PUSHes lazy-mapped a stack page,
  masking the fault Einstein triggers).
- The variant choice (ScratchVA + L1 patch + stage-2 carve-out).
- The TPIDRURW IRQ-race risk (mitigation level).
- The before/after wedge points.

If the experiment does NOT change the wedge, keep the variant in tree
behind a feature flag (so future experiments can re-toggle) and
update `INVESTIGATION.md` to record that Stack-variant is exonerated.
The remaining suspect list:
- Heap-allocator divergence past trace 1 063 (Einstein's recording cap).
- TPhys descriptor selection (`gPhysAllocator` ordering).
- Some other QEMU-vs-FVP-vs-Einstein behavioural difference.

## Critical files

- `src/shadow_stub.rs` — variant enum, encoder, install path,
  liveness picker.
- `src/stage2.rs` — `install_scratch_pool` (new), `install_tick_page`
  (model).
- `src/guest_mem.rs` — `fix_stage1_xn_bits` extension or sibling for
  the L1[0x18] section.
- `guest-tests/tests/test_shadow_stub.S` — new subtest for ScratchVA.
- `INVESTIGATION.md` — wedge state delta, hypothesis verdict.
- `IMPLEMENTATION.md` §8.5 — design notes on the variant
  (cross-reference, don't duplicate).

## Risks and open questions

1. **TPIDRURW IRQ race** — see "Why this is IRQ-safe" above. Start
   with documented tolerance; escalate if guest tests / boot reveals
   it.
2. **Kernel mutates L1[0x18] later** — we re-apply on M=0→M=1, but if
   the kernel actively writes `L1[0x18] = 0` mid-run, we'd lose the
   mapping. Add a stage-1 walk + dump on first ScratchVA
   trap-from-stub (no traps expected — but if one fires, it's
   diagnostic). Mitigation: trap CR2 writes and re-apply (existing
   `HCR_EL2.TVM` infrastructure).
3. **Multiple TTBR0s** — Newton may switch TTBR0 across tasks. If so,
   the L1 patch must be re-applied per TTBR0. Verify by logging
   distinct TTBR0 values across a boot. Existing `fix_stage1_xn_bits`
   re-walk hook covers this if the kernel calls into it on switch
   (need to verify).
4. **Stage-2 TLB invalidation** — `tlbi vmalls12e1` covers the IPA
   change. Sanity-check that no ROM aperture access is in flight at
   the moment we refine the L2 (very unlikely during init, but worth
   ordering carefully).
5. **Slot count headroom** — 1 694 ScratchVA sites observed in
   717006. Future ROM revisions could grow this. The 8 192-slot pool
   gives 4.8× headroom. If install hits the cap, halt loud (matches
   existing `SBA_STUB_MAX` exhaust handling).
6. **Pre-MMU first hit** — verify with the diagnostic logging from
   the prior conversation that no ScratchVA site fires before the
   kernel L1 patch is in place. The first observed Stack-variant PC
   was `0x000225d8` (in `TADSPEndpoint::nSnd`), well past BootOS, so
   this should be safe — but log it on first hit and halt-loud if it
   fires before the L1 patch is alive.

## Validation gate

Don't claim success unless **all** of these pass:
- `baremetal/guest-tests/scripts/run-all.sh` → 35/35 pass.
- The new ScratchVA subtest passes.
- Cold boot reaches **at least** the same trace count as the parent
  commit (no early-boot regression).
- The `Stack-variant @ PC=...` log (re-add the diagnostic from the
  prior session) shows zero hits — every fallback site is going
  through ScratchVA.

If the hypothesis is confirmed:
- Boot advances past the prior wedge point (trace 169 986).
- OR: `TStackManager::Fault` fires at the Einstein-matching trace
  (~147 584) when it previously didn't until trace 169 462.

## What NOT to do

- Don't change `SBA_STUB_POOL_IPA`. Moving the executable stubs out
  of the ROM aperture re-opens the post-MMU dispatch wound that
  drove `lwxxwtnp` in the first place.
- Don't try to thread the scratch through TPIDRURW alone. We've
  established only one slot fits and we need two.
- Don't enable PMU user access for scratch slots. Same prior-session
  rejection — kernel doesn't save/restore PMU regs across context
  switch.
- Don't add a generic "in-guest stub" for the SBA pre-fault probe
  recursion fix. Different problem; out of scope.
- Don't touch `StubVariant::DeadReg` paths. They're correct as-is
  and any change risks the bulk of the byte-access install.
