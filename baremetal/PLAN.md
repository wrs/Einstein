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

**Current goal (iter-94 follow-up):** investigate the unrecognised UND
at `PC=0x01E00010` `insn=0xeaa695ad`. The kernel branches via VA
0x01E00010 to a *secondary* jump-table — distinct from the post-ship
patch-table iter-94's classifier fix covered. Stage-1 walk shows
VA 0x01E00010 → IPA 0x7EE010 via an L2 table at PA 0x7EC000 (256
entries; entries 0..31 alias to PA 0x7ED000, entries 32..255 alias
to PA 0x7EE000). The 18 B-thunks at PA 0x7EE000..0x7EE048 target
kernel-VA `0xff19xxxx`. Hard-coded ROM branches at PA 0x7a5618 /
0x7a5680..0x7a568c jump straight to VA 0x01E0000x. classify-rom
currently only knows about the post-ship patch-table aliasing
(`jt_va_to_phys`); to mark these thunks as code, the walker needs a
new resolver for VA 0x01E00000+ → PA 0x7EE000+ that parses the L2
at PA 0x7EC000 the same way the kernel's stage-1 will use it. Once
that's in, the boot should clear the `PC=0x01E00010` UND.

### Iteration 94: classifier follows patch-table thunks (BE-8 follow-up)

iter-93 cleared the byteswap of guest page-table accesses, tick page,
and flash seed — boot reached `InitCGlobals+0x18c → ReserveContiguous-
Memory` and tripped a kernel-side DABT at FAR=0xfef80150 inside the
kernel's own DataAbortHandler. Diagnosis: the faulting PC was
0x01a68430 — a post-ship patch-table thunk slot for
`ReleaseIRQTimer`. The classifier's walker had `resolve_jt_va` follow
patch-table VAs (0x01A00000..0x01C20000) directly to the real ROM
target *and skip the thunk slot itself*, so all 16920 B-thunk words at
PA 0x02000..0x12860 stayed unmarked in `reach.bitmap`. The BE-8
atomic flip's load-time byteswap is gated on that bitmap; thunks left
unmarked stayed in BE byte order in physical memory; the CPU's LE
instruction fetch then decoded each thunk as a misaligned coprocessor
/ data-processing instruction, the kernel's `B FindHighROMProtocol`
thunk drifted into garbage, and downstream behaviour produced the
spurious DABT.

Fix in `tools/classify-rom`: `resolve_target_to_rom` now returns the
*thunk* PA for patch-table VA targets (instead of resolving to the
final-target PA via `resolve_jt_va`). The walker's `Step::Continue` /
`Step::Jump` arms push that thunk PA into the worklist, which causes
the walker to visit the thunk word, set its reach bit, and then
follow the B naturally — landing at the same final target it would
have reached via the old shortcut. Net effect: 14507/16920 patch-table
thunks now classified as code (vs 3 before); the load-time byteswap
covers them; the CPU fetches valid B instructions. The unreached
remainder is genuinely-unused slots — the classifier remains purely
root-driven.

`handle_und` in `src/trap.rs` also got a small upgrade: the failing
read of the faulting instruction falls back to a stage-1-walked
`guest_endian::guest_read_u32_va` so the diagnostic picks up bytes
from the actual backing PA when the kernel has set up an aliasing L2
entry (e.g. VA 0x01E00010 → PA 0x7EE010). On a halt we also dump the
stage-1 walk for the faulting PC.

Result: cold boot now runs ~1170 log lines (up from 1103); past the
`ReserveContiguousMemory` deep-toast alert, through the
`gROMPublicJumpTable` aliasing setup, through 22 unaligned-LDR
faults from `TPrivatePackageIterator`. Wedges next on a *secondary*
jump-table at VA 0x01E00010 (PA 0x7EE000..0x7EE048) that uses an L2
aliasing scheme `jt_va_to_phys` doesn't yet handle — that's the
iter-95 follow-up. 36/36 guest tests pass.

### Iteration 93: byteswap guest page-table accesses + tick page + flash seed (BE-8 fix)

iter-92 cleared the serial[mdem] +0x2800 wedge. Boot reached the
second SCTLR write (M=1, MMU on) and immediately tripped a
recursive prefetch-abort loop at `PC=0xC LR_svc=0x11ec10`. The L1
dump showed all entries as "fault" with absurd values like
`0x11040000`, `0x1e081000` — the `byteswap` of the kernel-intended
descriptors `0x00000411`, `0x0010081e`. The hypervisor's L1/L2
walkers (`fix_stage1_xn_bits`, `dump_guest_l1_table`,
`dump_stage1_walk`, `install_scratch_pool_l1_section`,
`translate_va`) read RAM via raw `ram.add(i).read()` LE; under BE-8
the kernel STRs entries with CPSR.E=1 in BE byte order (matching
what the MMU walker reads, since SCTLR.EE=1 makes both the kernel
and the MMU use BE for page-table memory). EL2 is AArch64 LE — a
raw read returns byteswapped values.

Worse, `fix_stage1_xn_bits` was matching false-positive "section"
entries (kernel descriptors whose byteswap happened to have
bits[1:0]==10) and rewriting them with bogus normalised values via
raw LE writes — corrupting the L1 such that the MMU saw garbage
section descriptors pointing to PA 0x1e000000+. That's why the
PABT loop fired immediately after MMU enable: the very first
instruction fetch hit a "fault" entry and bounced into the PABT
vector, whose own fetch hit another fault, ad infinitum.

Fix in `src/guest_mem.rs`: introduce `read_pt_entry` /
`write_pt_entry` inline helpers that always byteswap (under
non-`nh_guest_test`) — matching what the AArch32 EL1 MMU walker
sees with `SCTLR.EE=1`. Routed every guest L1/L2 raw access
through them: `fix_stage1_xn_bits` (L1 + L2 reads/writes),
`dump_guest_l1_table`, `dump_stage1_walk`, `dump_l1_neighbourhood`,
`install_scratch_pool_l1_section`, the L1[0xCD] probe, and
`translate_va`'s closure walk. ~10 sites in one file.

Sweep for other raw guest-RAM/ROM accesses turned up two more
byte-order bugs the kernel reads with CPSR.E=1:

- `src/stage2.rs::publish` writes the synthetic tick / calendar
  words into TICK_PAGE via raw LE; kernel BE-LDR returned
  byteswapped values. Fixed: `swap_bytes()` before the volatile
  write.
- `src/peripherals/flash.rs::write_u32` (constructor seeds:
  "DLDS", "OSCD", checksums, etc.) and
  `flash::program_word` (kernel masked-write trap). Both raw LE;
  kernel BE-LDRs from the stage-2-mapped flash region got
  byteswapped headers. Fixed: route both through `swap_bytes()`
  for BE-8, identity for `nh_guest_test`.

Result: cold boot now runs ~1100 log lines (up from ~700); MMU
enable cycles complete cleanly across multiple soft reboots; the
L1 dump shows correct descriptors (sections at 0x100c0e..0xf00c0e,
coarse L2[0]@0x400 with identity-map small pages); the boot
reaches `InitCGlobals+0x18c → ReserveContiguousMemory +0x34
LR_svc=0x313600` before tripping a real kernel data abort at
FAR=0xfef80150 that the kernel's DataAbortHandler can't resolve
(lazy-coarse mapping for L1[0xfef] not yet installed). 36/36 guest
tests pass.

<!-- Older iteration retrospectives (iter-92 and earlier) live in
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
