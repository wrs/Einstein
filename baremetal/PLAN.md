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

**Current goal (iter-104):** iter-103 landed the VA-space classifier
rework — the walker now carries `cur` as a VA throughout, with a
single `va_to_pa` translation step, so JT-thunk page chains decode
their B-AL words against the runtime VA the kernel actually
branches through. Boot now advances dramatically past the iter-100
PC=0x7a56e4 wedge: through `InitGlobalWorld`, `OsBoot`, and into
USR-mode task code, before stalling on a kernel-internal abort
cascade kicked off by an unrecognised SWI:

```
und: DebuggerUND @PC=0x3add50 msg="Undefined SWI " (resume at PC=0x3add64)
dabt: forwarding to kernel DataAbortHandler — DFSC=0x5 FAR=0x9ebdd54a mode=0x17
  LR_abt=0x003add74 (faulting PC=0x003add6c)
  saved-slot SPSR_abt=0x20000393 (pre-abt mode=0x13 = SVC)  *** MRS DIVERGES ***
und: DebuggerUND @PC=0x393898 msg="Non-user-mode abort (deep toast alert)"
unaligned: cannot write aligned 0x00000000 (EA=0x00000003) at PC=0x3940b4
```

`0x003ad750` is the SWI dispatch trampoline; `0x003add50` is the
"Undefined SWI" debug stub the dispatch falls into when the
opcode-indexed handler table maps to it. The garbage FAR
`0x9ebdd54a` and the cascade through "deep toast alert" + the
unaligned write of zero to EA=3 all look like a corrupt register /
stack frame propagated out of an earlier mishandled trap.

Investigation order for iter-104:

1. **Identify the offending SWI.** Add a probe (HVC injected via
   `rom_patches.rs`) on the SWI dispatch around `0x003ad750` that
   logs the SWI number and faulting LR before falling into the
   "Undefined SWI" stub. The SWI number tells us whether this is
   a kernel SWI we haven't installed (which means we missed a
   table-population step earlier in boot) or a user-side SWI we
   should be emulating.
2. **Validate the MRS-divergence path.** The DAH-mrs-patch log
   already flags the saved-slot vs current SPSR mismatch — this
   is the recurring Phase-B pattern where banked-register
   handling at the AArch32↔AArch64 boundary doesn't restore the
   right context. Cross-check against `docs/QEMU_BUGS.md` before
   suspecting hypervisor code (banked SPSR_abt is in
   `ctx.x[20]`, etc., per ARM ARM Table D1-79).
3. **Trace the garbage FAR back to its source.** A register-state
   dump at `und: DebuggerUND @PC=0x3add50` (before the cascade
   starts) gives the SWI handler's input registers — likely r1
   carries the SPSR-shaped value 0x20000393 that surfaced in the
   later abort log, which would point at a kernel-side bug
   accessing a stack slot with the wrong offset / wrong mode.

Iter-103 retired the iter-100 APCS-prologue-scan workaround. The
iter-99 fault-handler LDR byteswap stubs are still present in
`rom_patches.rs`; they remain harmless and may yet be exercised by
a fault path further on — don't revert.

### Iteration 103: VA-space classifier walker

Goal: rebuild the classify-rom walker to operate on virtual
addresses end-to-end so aliased thunk pages (patch-table,
public-jt, secondary-jt) decode their B-AL targets against the
runtime VA the kernel will actually branch to. Previously the
walker pre-resolved JT VAs to their backing PAs, then decoded the
thunk's B against the PA — wrong destination on every aliased page
where many VAs share one PA.

Major changes (`tools/classify-rom/src/main.rs`):
- New `va_to_pa(words, va) -> Option<u32>`: identity for main ROM
  / REx, L2 walk for patch-table / gROMPublicJumpTable /
  secondary-JT, silent `None` otherwise. Used by indirect-pass
  collectors that scan literal-pool words heuristically.
- New `va_to_pa_loud`: same translation, eprintln-once-per-unique
  on miss. The walker's hot path uses this so any unmapped VA the
  walker reaches surfaces immediately as a missing JT window or a
  misdecoded data branch.
- Walker inner loop: `cur` is a VA throughout; `cur_pa =
  va_to_pa_loud(cur)` for bitmap and `words` access only;
  `Step::Continue / Step::Jump` targets stay as VAs.
- `load_symbol_roots` keeps the symbol's VA (was: resolved to
  thunk PA). The walker pops the JT VA itself; `va_to_pa`
  translates; the walker reads the thunk's B word and `Step::Jump`
  dispatches to the next VA along the chain.
- Drop `resolve_target_to_rom`, `resolve_jt_chain`,
  `PURE_THUNK_PAGES` pre-marking, the post-walk chain-thunk-mark
  pass, and `collect_apcs_prologue_scan_roots` (an iter-100
  workaround for functions the broken walker missed — the rebuilt
  walker reaches them naturally).
- Indirect-pass collectors (vtable, fnptr-literal,
  indexed-dispatch, classinfo, vector-table, FDRV) now seed VAs
  via the silent `va_to_pa` + first-word shape gate.

Result:
- 36/36 guest tests pass.
- `byte-access-static` popcount 27786 → 27790 (essentially
  unchanged — the gain is correctness, not coverage).
- Invariant `oracle ⊆ static` still holds (oracle 2155, static
  27790, 0 missing).
- 70 ROM-soup walk-entries (was 35) — all legitimate ROM-driver
  TClassInfo trampolines at 0x7a5xxx; the user-defined ROM-soup
  range is intentionally over-reaching.
- Boot advances from PC=0x7a56e4 to the iter-104 wedge described
  above.

<!-- Older iteration retrospectives (iter-98 through iter-102) live
     in `git log` per the auto-prune maintenance note. -->



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
