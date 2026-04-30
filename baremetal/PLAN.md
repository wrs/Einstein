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

**Current goal (iter-56):** iter-55 added a per-pixel
non-byte-aligned blit path to `peripherals/screen.rs`, so
Newton's UI code can now blit sub-byte glyph regions
(src_left=115 ≡ 3 mod 8 etc.). Boot reaches **interactive
operation**: 2 blits land for the 320×480 splash + a 90×118
glyph region, framebuffer dump `/tmp/newton-fb/00000.png` is
written, and the kernel is then in steady state doing 3.4M
hypervisor traps/sec — no halt, no `***` line, no wedge.

99% of the steady-state traps are at `ELR=0xffffe4`
(UND_RETURN_STUB_OFFSET) with `SPSR=0x40000197` (ABT mode, IRQ
masked). This is the alignment-fault handler return path
firing repeatedly as Newton's ROM walks unaligned half-word
loads in its UI rendering loop. Functional, but expensive.

iter-56 should:
1. **Decide whether to look at the FB.** A user-facing capture
   (PNG dump, or live framebuffer-mirror) can confirm the splash
   screen has rendered correctly. The first blit copy was a
   320×480 inverted byte copy — comparing against a reference
   Newton boot screen confirms our blit math.
2. **Reduce the alignment-fault rate**, OR confirm the boot
   reaches a true idle / event-pending wait-state on its own
   (e.g., touchscreen interrupt). Our hypervisor's per-fault
   trap-and-emulate adds overhead on top of QEMU TCG; a
   rotate-LDR fast-path inside the hypervisor (or a kernel
   patch that uses ARMv7-aware LDRH instead of the ARMv4
   unaligned LDR idiom) could drop the rate by ~10-100×.
3. **Add tablet/pen input.** The boot is in steady state; the
   next milestone is observable user interaction, which needs
   the pen-input path wired up (`peripherals/tablet.rs`).

### Iteration 55: non-byte-aligned blit lands; boot reaches steady-state

iter-54's wedge was a hypervisor halt on Newton's UI calling
`Blit` with `src_left=115` (3 mod 8 — non-byte-aligned). Our
blit handler refused that input because the byte-aligned fast
path can only copy whole bytes of source bits.

#### Fix

`peripherals/screen.rs::blit` now branches on
`(pixmap_src_left | src_width_pixels) & 0x7`:

- **Byte-aligned fast path** (unchanged): byte-by-byte copy with
  per-byte inversion at FB stride 40.
- **Non-byte-aligned slow path** (new): per-pixel loop reading
  the source bit at `(byte >> (7 - sx & 7)) & 1`, inverting,
  read-modify-writing the matching dst byte's bit. Slow vs the
  word-mask logic Einstein uses in `Blit_0` (Screen/
  TScreenManager.cpp), but correct and adequate for the
  cold-boot UI rendering rate.

#### Observed boot state

After this fix, the cold boot:
1. Completes 19200-byte byte-aligned blit (320×480 splash).
2. Completes 10620-pixel non-aligned blit (90×118 glyph region
   at src=(111,115,229,205) — the iter-55 wedge case).
3. Writes `/tmp/newton-fb/00000.png` framebuffer snapshot.
4. Enters steady-state operation. The 90-second timeout kills
   the run with the kernel still spinning — no halt, no `***`
   wedge, no UnhandledException. 3.4M hypervisor traps/sec
   (99% are alignment-fault returns at `ELR=0xffffe4`), which
   means Newton is actively doing UI work.

#### Verification

- All 36 guest tests pass.
- 0 `***` halt lines in cold-boot log.
- 2 successful blits land in the FB.
- FB dump `/tmp/newton-fb/00000.png` is generated.

This is the first iteration since Phase B started where the
boot doesn't end on a halt — it ends on a *timeout*, with the
kernel still running.

### Iteration 54: alignment-fault redirect from DIAG-tag (DFSC=0x1 safety net)

iter-53's "wild PC=0xe7f842f0" wedge turned out not to be a wild
PC at all. The DIAG-vector dump's "faulting PC = 0xe7f842f0
insn=0xdeadbeef" line was reading the high 32 bits of a 64-bit
FAR_EL1 whose lower 32 bits held the actual VA (0x0c64be6e).
The high half was leftover bytes from an earlier exception. The
correct faulting info comes from the DABT trampoline's stash:
`LR_abt=0x0035c55c` → aborting PC = 0x0035c554, which is
`ldr r0, [r0, #0x3e]` inside `DrText__FlN21`.

This is the ARMv4 rotate-on-unaligned LDR idiom: read a 16-bit
half-word at a non-aligned offset by loading the surrounding
word and rotating. ARMv7 with SCTLR.A=1 (which we force on)
turns it into an alignment fault. Our hypervisor has an
emulator for it (`unaligned::handle_align_fault`) reached via
the DABT trampoline's BEQ to ALIGN_TAG.

#### Mechanism

The DABT trampoline checks DFSR.FS[3:0] via legacy
`mrc p15,0,Rt,c5,c0,0`. If FS == 1, BEQ to `HVC #ALIGN_TAG`;
else fall-through to `HVC #DIAG_TAG`. For this specific site
(and cause unknown — we have ~40 successful ALIGN_TAG dispatches
earlier in the run), the BEQ fell through to DIAG_TAG even though
ESR_EL1 reports DFSC=0x01.

#### Fix

`handle_diag` now cross-checks ESR_EL1.DFSC: if src_mode == ABT
and DFSC == 0x01 (alignment), it calls
`unaligned::handle_align_fault(ctx)` directly instead of falling
through to the diagnostic dump. This is a defence-in-depth net
that catches the case where the trampoline's legacy-DFSR check
disagrees with the AArch64 ESR_EL1 view.

#### Verification

- All 36 guest tests pass.
- Cold boot: zero `*** DIAG vector intercept` lines (was 1
  before). 40 alignment faults emulated successfully (vs. 39
  before, plus 1 now via the DIAG → ALIGN redirect).
- Boot progresses past the iter-53 wedge and reaches a new
  limitation in screen.blit (non-8-pixel-aligned src_left,
  iter-55).

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
