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

**Current goal (iter-95 follow-up):** boot now reaches a *new* class
of classifier-coverage gap. The kernel calls into PA 0x7a56cc — a
real init helper (function prologue `mov ip, sp; push {fp, ip, lr,
pc}; sub fp, ip, #4; ...; mov r2, #0x13000; ldr r1, [pc, #4]; bl
0x7a56f8`) that registers `gROMPublicJumpTable` (PA 0x13000) under
the magic tag 'lcdd'. The CPU UNDs at PC 0x7a56e4 because
classify-rom marks none of PA 0x7a56cc..0x7a56f0 as code, so the
BE-8 atomic flip leaves the bytes in BE order; CPU LE-fetch reads
0x132aa0e3 (NE-TEQ-without-S, ARMv7+ UND) instead of `mov r2,
#0x13000`. The function isn't reached by any walker pass: no
`B 0x007a56cc` exists; the literal `0x007a56cc` doesn't appear as a
4-byte BE word anywhere in ROM; the surrounding TClassInfo
trampolines (PA 0x7a563c / 0x7a57ec) describe 60-byte structs that
don't span this range. The kernel must reach it through an indirect
mechanism we haven't characterised yet — likely a high-VA alias
(the secondary jumptable's `B 0xff1936cc` chain maps somewhere into
PA 0x7a5xxx via a stage-1 mapping we haven't decoded). Two paths
forward: (a) characterise the 0xff1xxxxx → PA 0x7a5xxx alias and add
a third resolver to classify-rom; (b) extend the
`collect_classinfo_roots` heuristic to walk past the 60-byte struct
when adjacent functions are tightly packed against it. Pick once
the alias mechanism is clearer.

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

<!-- Older iteration retrospectives (iter-93 and earlier) live in
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
