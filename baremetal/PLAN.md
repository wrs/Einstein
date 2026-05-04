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

**Current goal (iter-96 follow-up):** make the classifier walker
treat `cur` as a runtime VA throughout, with a single VA→PA
translation step at the top of the inner loop that consults all the
in-ROM L2 page tables (patch-table family, gROMPublicJumpTable-
PageTable, secondary-jt L2). Once that's in place the roots become a
union of:
  1. The full named-symbol list from `_Data_/symbols.txt` (lifting
     the `addr >= 0x01000000` filter in `classify-symbols.py` for
     entries whose VA falls inside a known JT range).
  2. Every B-AL thunk VA enumerated by walking each in-ROM L2 (the
     unnamed thunks symbols.txt doesn't cover).
With VA-aware decoding `step()` computes B targets against the kernel's
runtime PC, so the walker resolves through chained thunks (gROM-
PublicJumpTable → patch-table → final ROM PA) without any pre-mark.
Indirect-target passes (vtable, fnptr-literal, B-run dispatch, etc.)
push the discovered VA — not the resolved PA — so the walker handles
thunk decoding itself instead of side-stepping it. Open question: an
earlier attempt at the restructure expanded reach from ~880K to
~3.3M words and regressed the boot at MakePrimaryMMUTable
(PC=0x459dc) when the kernel reads bytes we now mark as code as
data; isolating that conflict and patching the kernel side is part
of iter-97.

### Iteration 96: pre-mark patch-table + gROMPublicJumpTable thunks via L2 walk

Goal was to extend classify-rom so the BE-8 atomic flip's load-time
byteswap covers every B-thunk the kernel branches through, including
gROMPublicJumpTable (PA 0x13000..0x15FFF) and its sibling thunk
pages (PA 0x1B000..0x21FFF) that share gROMPublicJumpTablePageTable's
L2 (PA 0x18000) with a few pages of kernel-managed page-table data.

Approach: add a pre-walk pass that walks each in-ROM L2 page table
(`gROMPatchTablePageTable` family at PA 0x16000/16400/16800,
`gROMPublicJumpTablePageTable` at PA 0x18000), filters target pages
by shape (top byte 0xEA on the first 16 words = `B AL`), and pre-
marks the leading B-AL run inside each thunk page. Stop-at-first-non-
B-AL keeps real-code roots from being shadowed where pages mix a
thunk run with kernel-code function bodies (PA 0x21000 has 270
thunks then `TADC` method bodies starting at PA 0x21438).

Result:
- All 16919 patch-table thunk words (covers the full 17 buckets via
  the family's three live L2s).
- 12433 thunk words across gROMPublicJumpTable + the 7 sibling thunk
  pages mapped by gROMPublicJumpTablePageTable.
- GetSample's tail (PA 0x22000..0x22064) reaches via natural walker
  control flow now that the per-word stop-at-first-non-B-AL no
  longer marks the function-body B instructions inside the page.
- `byte-access-static` 27750 (matches iter-95's 27749 baseline).
- Boot reaches the same wedge as iter-95 (PC=0x7a56e4); 36/36 guest
  tests pass.

Limitation acknowledged: pre-marking is a workaround for the walker's
PC=PA assumption. The cleaner design is to make the walker treat
`cur` as a VA and translate via the in-ROM L2s on every step, with
roots seeded from `symbols.txt` plus a per-L2 enumeration of the
unnamed thunks the symbol list doesn't carry. A first attempt at
that restructure ballooned reach from ~880K to ~3.3M words and
regressed the boot — tracking down the data conflict is iter-97
work.

### Iteration 95: classifier resolves secondary jump-table aliasing (BE-8 follow-up)

iter-94 cleared the post-ship patch-table thunks but left the boot
wedged on `PC=0x01E00010` `insn=0xeaa695ad`. The kernel branches via
VA 0x01E00010 to a *secondary* jump-table whose 18 B-thunks live at
PA 0x7EE000..0x7EE048 and target kernel-VA `0xff19xxxx`. Stage-1
maps VA 0x01E0xxxx → PA 0x7EE000+ via a pre-built short-descriptor
L2 at PA 0x7EC000 (256 entries; 224 alias to PA 0x7EE000, 32 alias
to PA 0x7ED000 = 0xFFFFFFFF filler). The kernel installs `L1[0x01E]
= coarse(0x7EC000)` at boot. classify-rom only knew about the
post-ship patch-table aliasing (`jt_va_to_phys`), so the secondary
thunks went unmarked, the BE-8 load-time byteswap skipped them, and
the CPU's LE instruction fetch decoded garbage.

Fix in `tools/classify-rom`: new `secondary_jt_va_to_phys` reads the
L2 at PA 0x7EC000 directly to translate VA 0x01E0xxxx → ROM PA, then
returns the *thunk* PA so `resolve_target_to_rom` (and via it the
walker's Step::Continue / Step::Jump arms) seeds the thunk for
walking. Reading the L2 makes the resolver tolerant of ROM
revisions with different alias counts — it doesn't bake the
thunk-page count or alias span into a constant.

Result: 18/18 secondary jumptable thunks now classified as code (up
from 0); cold boot runs ~1180 log lines, past the iter-94 wedge.
Wedges next on a *new* classifier-coverage gap: kernel init helper
at PA 0x7a56cc..0x7a56f0 that the walker doesn't reach via any
existing pass (no direct B/BL, no PA literal, no surrounding
TClassInfo trampoline includes it). 36/36 guest tests pass.

<!-- Older iteration retrospectives (iter-94 and earlier) live in
     `git log` per the auto-prune maintenance note. -->
<!-- iter-90 deferred shadow_stub deletion: still gated off
     (`patch_rom_from_bitmap` no longer called from `main.rs`); full
     removal + SBA dispatch arms + `unxor_sub_word` guest-test path
     is a follow-up commit. -->



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
