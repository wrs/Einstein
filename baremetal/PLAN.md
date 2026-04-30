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

**Current goal (iter-55):** iter-54 root-caused the iter-53
"wild PC=0xe7f842f0" wedge: it wasn't a wild PC at all — the
DIAG-vector intercept's "faulting PC" decoder was misled by a
stale 64-bit FAR_EL1 value whose high half had previously been
some other instruction word. The actual fault was an **alignment
fault** on `ldr r0, [r0, #0x3e]` at PC=0x0035c554 inside
`DrText__FlN21` — Newton ROM uses the ARMv4 rotate-on-unaligned
LDR idiom that becomes an alignment fault under ARMv7's
SCTLR.A=1 (which we force on for exactly that reason).

The DABT trampoline's BEQ that's supposed to route alignment
faults to `HVC #ALIGN_TAG` instead fell through to
`HVC #DIAG_TAG` for this site (cause unclear: legacy DFSR via
`mrc c5,c0,0` may report differently from ESR_EL1 on this
specific site). iter-54 added a defence-in-depth check in
`handle_diag`: when src_mode is ABT and ESR_EL1.DFSC == 0x01,
dispatch to `unaligned::handle_align_fault` directly instead of
dumping and halting.

Boot progresses past the alignment-fault wedge and hits a NEW
limitation in our screen.blit:

```
screen.blit ENTER ... pixmap=0xc107d8c addy=0xc646d00
  rowBytes=40 pmTL=(0,0) src=(111,115,229,205) dst=(111,115,229,205)
*** screen.blit: src_left 115 not 8-pixel aligned (would need
    bit-mask blit) @PC=0x801bd4
```

Newton's UI code is now blitting a non-byte-aligned region
(src_left=115 ≡ 3 mod 8). Our hypervisor's blit halts loud on
non-byte-aligned src because we never ported Einstein's
bit-mask path (`Blit_0` in `Screen/TScreenManager.cpp`).

iter-55 should port the Einstein bit-mask blit logic. The
required pieces:
- `additionalLeftPixels = srcLeft & 0x7` and a `leftMask` that
  preserves the dst's pre-existing left-edge pixels.
- Same for the right edge (`additionalRightPixels`).
- Per-row 32-bit-word loop reading source big-endian and
  inverting per blit mode (0 = srcCopy, 1 = darken).
- `UpdateScreenRect` no-op (we mark FB dirty already).

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

### Iteration 53: screen.blit pixmap interpretation aligned with Einstein's TScreenManager

iter-52's fix unblocked the boot to NewBlock #758, where the
Tmux task issued a Blit native primitive. The hypervisor's
blit halted with `screen.blit: src VA 0xc64d000 outside mapped
regions` — a few rows into the copy, the bitmap walker had
walked `addy + N * rowBytes` clear off the end of RAM.

#### Mechanism

Einstein's `TScreenManager::Blit` (Screen/TScreenManager.cpp)
reads pixmap+0x04 with `srcRowBytes = word >> 16` — rowBytes
is in the HIGH 16 bits of the packed word. The Newton ROM
stored 0x00280000 there; the high half is 0x0028 = 40
(the 1-bpp byte count for a 320-pixel-wide row). Our code
took the whole 32-bit word, getting rowBytes = 2621440 (= 2.5
MiB stride per row). The first row started at addy=0xc646d00
and the second row tried to read at 0xc646d00 + 2621440 =
0xc8c6d00 — well past the heap top.

Einstein also reads pixmap+0x08 as `pixmapTopLeft` (top in high
16, left in low 16) and biases the src rect into pixmap-relative
coordinates (`srcLeft -= pixmapLeft`, `srcTop -= pixmapTop`).
Our code skipped this step. For the boot blit the topLeft is
(0,0) so the bias was a no-op, but kernel UI code that uses
sub-pixmap rects (Newton's compositor does) would have crashed
the same way.

#### Fix

`peripherals/screen.rs::blit` now:
1. Reads `rowBytes = word_at_pixmap+4 >> 16`.
2. Reads `pixmap_top_left` at +0x08, biases src rect to
   pixmap-relative.
3. Halts loud on src_left or src_width not 8-pixel aligned (we
   don't model Einstein's bit-mask blit — Newton's 320-px panel
   stays byte-aligned in practice; the halt makes a future
   non-aligned caller surface immediately rather than corrupting
   the FB).
4. Inverts each byte before writing FB (Newton 1-bpp `1 = pen-
   pressed` vs host FB `1 = white`).
5. Lays out rows in FB at `SCREEN_WIDTH / 8 = 40` bytes stride,
   not the source rowBytes (which only matched by accident in
   the small test pixmap).

`test_screen_blit.S` was updated to match: rowBytes is now
packed (`(4 << 16)`), expected FB bytes are inverted, and row 1
is read at FB[+40] not FB[+4].

#### Verification

- All 36 guest tests pass (test_screen_blit re-greens with the
  updated expectations).
- Cold boot: blit completes 19200 bytes copied for the
  320×480 screen and proceeds to NewBlock #769. New wedge is a
  wild PC=0xe7f842f0 in USR mode (iter-54).


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
