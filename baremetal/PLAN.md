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

**Current goal (iter-92 follow-up):** chase the post-MMU-enable
prefetch-abort loop. After SCTLR_EL1.M=1 at the second SCTLR write
(BootOS+0x16c, `0x000011b5`), the next AArch32 trap shows
`PC=0xC LR_svc=0x11ec10 mode=0x17 (ABT) FAR_EL1=0xc00000000` and
the system loops on timer-IRQ-with-stuck-PABT. PC=0xC is the AArch32
PABT vector (low-vector base, since SCTLR.V=0); the kernel's
post-MMU L1 (TTBR=0x0400_0000) leaves L1[0] as a fault descriptor
(see "guest L1 first 32 entries" dump — every entry is "fault"),
so the very first PABT redirected to `0xC` faults again, and the
abort handler can't make progress. Likely a real Phase B
gap — vector-page mapping or `gIPCVectorBase` redirection after MMU
enable. Iter-91 fix (literal-pool subtraction) and iter-92 fix
(serial control/IE reads) progressed the boot from
`SaveCPUStateAndStopSystem +0x2bc` (~250 log lines) through
BasicBusControlRegInit, DACR/TTBR programming, MMU enable, and the
L1 dump (~700+ lines).

### Iteration 92: serial control/IE reads + qemu-reaper Stop hook

iter-91 cleared the BE-8 wedge but left the boot stuck at
`*** serial[mdem] UNKNOWN R +0x2800 ***` (PC `0x19cec`,
BasicBusControlRegInit). The actual instruction at that PC is
`STRB R1, [R0]` — a write — but under BE-8 the byte/halfword
write path in `mmio::write` reads the surrounding word first to
splice the byte into the right lane. So a kernel byte-store to
mdem +0x2800 produced an MMIO **read** at the same offset, which
`serial::read` had no handler for.

Fix: extracted the existing write-only "control / IE consumed"
offset list (`0x0000`, `0x0400`, …, `0x8000`) into a
`reg::CONTROL_IE_OFFSETS` slice and made both `read` (return 0,
"register holds zero / idle peripheral") and `write` (no-op)
consult it. No behavioural change for writes; reads of any of
those offsets now return zero instead of halting.

Also wired a project-scoped Claude Code Stop hook
(`.claude/settings.json`) that runs `pkill -9 qemu-system-aarch64`
at session end, so a wedged hypervisor can't leave a zombie QEMU
behind across sessions. The hypervisor halts loudly on its own
unhandled cases and QEMU keeps running until something kills it;
without the hook, every aborted `cargo run` left a process around.

Result: cold boot now runs from `Entering Newton ROM…` through
~700 log lines (BasicBusControlRegInit, DACR write, TTBR0
programming, `fix_stage1_xn_bits` (130 sections de-XN'd, 144 fine
→ fault), L1[0xCD] transition probe, shadow_stub scratch L1[0x60]
install, MMU enable at SCTLR_EL1=0x000011b5) before the
post-MMU-enable PABT loop above. 36/36 guest tests pass.

### Iteration 91: classifier literal-pool subtraction

iter-90 left the cold boot stuck at `SaveCPUStateAndStopSystem +0x2bc`
(BootOS' fatal-init halt). Root cause: the classifier's
`reach.bitmap` flagged 76 literal-pool words as code, so the BE-8
loader byteswapped them. Smoking gun: the `LDR Rd, [pc, #-136]` at
PC `0x186c8` reads the literal at `0x18648`, whose on-disk word is
`0x0f242400` (a PCMCIA MMIO IPA). Byteswapped on load it lands in
host bytes `00 24 24 0f`, so a CPSR.E=1 LDR returns `0x0024240f` —
which the next instruction (`LDR R0, [R0]`) dereferences as an
unmapped IPA, panicking init.

The literal pool at `0x18644..0x18687` is dual-purpose at the
encoding level — `DiagHook`'s `beq 0x1862c` at `0x185a4` lands in
the pool, so `tools/classify-rom`'s static walker reaches it as
code. Under BE-32 word-invariant the dual-purpose was harmless
(word reads of the same bytes returned the same numerical value
either way). Under BE-8 it can't be both.

Fix in `tools/classify-rom`: post-walker pass
`clear_literal_pool_targets_from_reach`. Iterates the reached
bitmap; for each `LDR Rt, [pc, #±imm12]` (cond=AL) it computes the
literal-pool target and clears that word from `reach`. In our boot
the dead-code branch into the literal pool never fires, so treating
those 76 words as data is safe and load-bearing.

Same logic in `Bitmap::clear_word`. `WalkStats` gains a
`literal_targets_cleared` counter, surfaced in `summary.txt`.

Result: cold boot now runs ~5 KiB further; guest tests stay 36/36.
Next iteration: model `serial[mdem] +0x2800` (cross-ref
`Emulator/Serial/TVoyagerSerialPort.cpp`).

### Iteration 90: BE-8 atomic flip (PLAN_BE8_MIGRATION.md)

Migrated guest data accesses from "BE-32 word-invariant via load-time
word swap + UDF-trap byte-lane emulator" to "BE-8 (CPSR.E=1) data
accesses + selective code-only byteswap at load". Five commits across
mpt..pt:

1. **Phase 0 — sweep diagnostic probes.** Removed every iter-50..89
   probe HVC scaffolding (immediates 0x46, 0x48–0x4E, 0x53–0x68,
   0x6B–0x7F, 0x81–0x91) — `*_PROBE_HVC_IMM` constants in
   `rom_patches.rs`, dispatch arms in `trap.rs`, handler functions,
   ring buffers, and orphan helper modules (`dosend_ring.rs`,
   `rep_print.rs`). Kept `FPE 0x80` (load-bearing) and DAH /
   UnhandledException tripwires. ~7000 LOC removed; 36/36 guest tests
   pass.

2. **Phase 1 — `guest_endian.rs` accessors with identity behavior.**
   Added `guest_read_u32_va/pa`, `guest_write_u32_va/pa`,
   `guest_read_u8_va/pa`, `guest_read_u16_va/pa`, `guest_write_u8_pa`,
   `guest_write_u16_pa`, `guest_read_bytes_va`. Migrated ~120
   call sites across `trap.rs`, `peripherals/*`, `task_dump.rs`,
   `heap_check.rs`, etc. through the new helpers. Snapshot, ROM
   patches, and shadow_stub host-byte manipulation paths kept
   unchanged.

3. **Phase 2 — atomic flip to BE-8.** `load_newton_rom` consults the
   classifier `reach.bitmap` per-word: code → byteswap on load,
   data → byte-copy verbatim. Helpers `write_rom_code_word` /
   `write_rom_data_word` / `write_rom_word_by_kind` route every
   ROM patch through the right encoding. `eret_to_guest` SPSR sets
   E=1 (`0x000003D3`); `zero_el1_guest_state` programs SCTLR_EL1
   with `EE | E0E`. The CP15 SCTLR shim masks `EE | E0E` to `1` so
   kernel writes can't drop us back into LE mode. `guest_endian`
   helpers byteswap on read/write for data PAs and pass through for
   ROM code PAs (bitmap-aware dispatch). MMIO byte/halfword writes
   splice into the BE-8 lane (lane 0 = bits[31:24]). Snapshot
   format version bumped 3 → 4. Guest-test mode (`nh_guest_test`
   cfg) keeps LE semantics so the existing flat-binary corpus
   continues to exercise the hypervisor's other mechanisms.

4. **Phase 4 — diagnostics simplification.** `heap_check::dump_object`
   updated comment around `guest_read_bytes_va` (the byte-order
   gymnastics is now internal to the helper).

#### Result

- **36/36 guest tests pass** on QEMU (covers byte/halfword access
  emulation, unaligned LDR rotate, ROM aperture SWP, snapshot
  resume, alignment fault, etc.).
- **Cold boot reaches BootOS init code**: BootOS canary fires,
  SCTLR write succeeds (`0x000010b0` → hw `0x030010b2` with
  EE/E0E held by the shim), StrongARM CP15 clock UND at `0x186a8`
  decoded and no-op'd, several MMIO writes (`0x0F18_3800`,
  `0x0F18_3C00`, etc.) proceed, then the kernel hits a fatal init
  check and enters `SaveCPUStateAndStopSystem +0x2bc`. Full
  `evt.ex.fr.store` success-oracle path not yet exercised — wedges
  in early init. (iter-91 cleared this — see retrospective above.)

#### Phase 2d / 5 deferred

`shadow_stub.rs` is gated off (`patch_rom_from_bitmap` no longer
called from `main.rs`) but the module still compiles. Full deletion
+ removal of `SBA_RETRY_TAG` / SBA dispatch arms + `unxor_sub_word`
guest-test path is a follow-up commit.

<!-- Older iteration retrospectives (iter-89 and earlier) live in
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
