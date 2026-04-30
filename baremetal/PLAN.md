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

**Current goal (iter-51):** iter-50 fixed the iter-49 liveness-analyser
corruption bug (option 2 from iter-49's plan: original-ROM shadow).
Cold-boot confirms `shadow_stub pick @0x001488ac: DeadReg sea=R2
sfl=Some(3)` — picks R2+R3 instead of R12 — and the iter-48 wedge
("Lookup loaded WILD entry") is gone. Boot now progresses past the
flash-block lookup and hits a NEW stall in
`Fault__13TStackManagerFR15TProcessorState`:

```
Throw #0: name="evt.ex.abt.bus" r0=0x000afda0 r1=0x0cc77700 r2=0
          caller_lr=0x001f8538  sp=0x0c113388  mode=0x10
*** invariant violation: kernel reached UnhandledException ***
```

caller_lr=0x001f8538 is right after `bl Throw` at 0x001f8534, inside
TStackManager::Fault. r1=0x0cc77700 looks like a valid RAM VA, NOT
the iter-48-class wild bit-31-set value.

iter-51 must pin the underlying bus-abort site behind this Throw.
Likely a stack-page lazy-fault path that's hitting an MMIO region
or unmapped IPA. The standard recipe: probe DataAbortHandler's
forwarding decision, capture FAR/DFSR/USR_PC, decode whether the
fault is in the peripheral model, MMU mapping, or kernel logic.

### Iteration 49: shadow_stub liveness analyser reads probe-corrupted ROM — picks R12 wrongly

iter-48 hypothesised "DoCommit doesn't initialise sp+148"; the
hypothesis is WRONG. iter-49 added a `*[r0]` wildness check in the
FindSuperceeder ENTRY probe (false positive — TObjRef[+0] is an
object ID like `0xf0000027`, not a pointer; the actual parent
TFlashStore* lives at TObjRef[+16]) and through that diagnostic
trace identified the actual mechanism.

The FindSuperceeder body uses ip (R12) as a save register across
the byte-access stub at 0x001488ac. The picker chose R12 as
scratch_ea because the liveness analyser was looking at the
**already-patched** ROM (rom_patches replaced the LOCAL
`mov r0, ip` at 0x001488c4 with HVC #0x6E for the FINDSUPER_MID
probe). HVC is treated as `BLink` → caller-saved clobber → R12
"dead" → picked. Stub clobbers ip → wild this to Lookup.

This is the same root cause as iter-41 (R14 picked as scratch_fl).
iter-42 worked around iter-41 by excluding R14 from the candidate
pool — a band-aid that doesn't address the analyser's read-the-
post-patch-ROM problem.

#### Iter-49 deliverables

- Tracing in `emit_inline_stub` for known-buggy sites — confirms
  in production cold boot:
  ```
  shadow_stub pick @0x001488ac: DeadReg sea=R12 sfl=None
  ```
- Three regression unit tests in `src/shadow_stub.rs::tests`:
  - `pick_scratch_at_findsuperceeder_does_not_pick_r12` — passes
    when the analyser sees pristine ROM bytes; demonstrates the
    analyser is correct given the right input.
  - `pick_scratch_at_findsuperceeder_when_midprobe_installed_does_not_pick_r12`
    — `#[ignore]`'d; fails on HVC-corrupted ROM. Will turn green
    when iter-50 lands.
  - `pick_scratch_with_local_lr_read_does_not_pick_r14` — passes;
    locks in the iter-41 invariant.

#### Next iteration plan (iter-50)

Pick one of:

1. **Reorder install** — move `shadow_stub::patch_rom_from_bitmap()`
   BEFORE the `apply_717006_patches` call inside `load_rom`. Audit
   that no shadow_stub byte-access PC overlaps a rom_patches probe
   PC (the lists are small; a static assertion at install time is
   easy).

2. **Original-ROM shadow** — keep a side-table of `(PC, original
   instruction)` pairs maintained by `apply_717006_patches`, and
   give shadow_stub's analyser a reader function that consults it
   first. Local change to the analyser; no ordering constraint.

After the fix lands, un-`#[ignore]` the regression test and verify
it turns green. Re-run the cold boot — iter-48's wedge (Lookup
called with wild this) should be gone.

### Iteration 50: original-ROM shadow lands; iter-49 wedge resolved

Implemented option 2 from the iter-49 plan: an
`ORIG_PCS / ORIG_INSNS` side-table in `src/rom_patches.rs` populated
by `patch_probe` at install time, exposed via
`pub fn read_original(pc) -> Option<u32>`. shadow_stub's analyser
now uses a wrapper reader (`code_read_word_original_first`) that
consults `read_original` before falling back to `read_word_pa`.

**Result on cold boot:**

```
shadow_stub pick @0x001488ac: DeadReg sea=R2 sfl=Some(3)   ← was R12!
Lookup-base #0..#N: r4=0x0c604c04 [r4+44]=0x0c605848  (consistently sane)
Lookup-idx  #0..#N: base=0x0c605848 idx=0x0 entry=0x0c605950  (sane)
```

iter-48's wedge (Lookup called with wild this) is gone. The boot
now progresses past the flash-block lookup chain and hits a NEW
stall in `TStackManager::Fault` (a kernel stack-fault throw at
PC=0x001f8534) — that's iter-51 territory.

#### Tests

The previously-`#[ignore]`'d iter-49 regression test was rewritten
into two:

- `pick_scratch_at_findsuperceeder_with_originals_wrapper_does_not_pick_r12`
  — passes; demonstrates the analyser picks R2/R3 correctly when the
  reader simulates the originals-shim.
- `pick_scratch_at_findsuperceeder_without_originals_picks_r12` —
  passes; locks in the BUG behaviour as a regression so a future
  change to the analyser that "happens to fix" this case (without
  the originals shim) makes the test fail loudly.

All 6 `pick_scratch_*` tests in `shadow_stub::tests` pass.

#### Why option 2 over option 1

Option 1 (reorder install) was viable but option 2 has zero
ordering constraint and works regardless of how many future
probes are added. The side-table is bounded (`ORIG_CAP=64`) and
populated at boot only. Production reader `code_read_word` is
unchanged — only `pick_scratch_regs`'s reader path now consults
the side-table.

#### Iter-42 R14 band-aid status

Still in place. The iter-50 fix is the proper layer for
correctness; the R14 candidate-pool exclusion remains as defence
in depth (iter-41's mechanism was the same as iter-49's, so the
analyser fix should make the R14 exclusion unnecessary). Could be
reverted once we confirm shadow_stub picks correctly in all known
cases without the exclusion. Tracked but deferred — not on the
critical path.

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
