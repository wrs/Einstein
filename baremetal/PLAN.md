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

**Current goal (iter-54):** iter-53 fixed the screen.blit wedge:
the rowBytes field at pixmap+4 is packed in the high 16 bits
(per Einstein's `TScreenManager::Blit` reading
`srcRowBytes >> 16`), and the source rect is in the pixmap's
own coordinate space (must be biased back through pixmap+8
`pixmapTopLeft`). Our previous code read the full 32-bit word as
rowBytes, so rowBytes=2621440 (=0x280000) walked the bitmap
pointer off the end of mapped RAM after a few rows. With the
fix, rowBytes=40 (0x28) and the blit completes for the
320×480 screen.

The blit also now inverts each byte (Newton 1-bpp convention vs
host FB) and lays rows out at SCREEN_WIDTH/8 = 40-byte stride
(the fixed FB stride, not the source rowBytes — the previous
code matched src and dst strides which only happened to work
when both were tiny test buffers). `test_screen_blit.S` was
updated to match these semantics.

Boot now progresses to NewBlock #769 and hits a NEW wedge:
the kernel's user-mode code branched to a wild PC = 0xe7f842f0
(an undefined-instruction encoding pattern, not a real ROM
address). The DIAG vector intercept dump shows:

```
*** DIAG vector intercept (HVC #DIAG_TAG from mode ABT) ***
  ELR_EL2   = 0x00000010  (PC of insn after HVC)
  faulting PC 0xe7f842f0 insn=0xdeadbeef
  pre-fault SP=0x0cc7787c LR=0x0035c498
  r0=0x0cc77c40 r1=0xfffffffe r12=0x0cc778bb
```

Pre-fault mode was USR; LR_usr = 0x0035c498. The wild PC
likely came from a corrupted function pointer or an LDM-with-PC
that loaded garbage from a spilled stack slot.

iter-54 should:
1. Backtrace from LR_usr=0x0035c498 to identify the call site
   that branched to the wild PC.
2. Inspect the user-stack contents at SP_usr=0x0cc7787c to find
   the spilled register (likely R12 or PC) that held 0xe7f842f0.
3. Decide if this is yet another shadow_stub mis-pick, a stack-
   corruption upstream, or a kernel-data-structure issue.

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

### Iteration 52: FindSuperceeder wild-r3 root-caused — Direct-branch unreachable targets need APCS_PARAMS

iter-51's fix exposed a new wedge: `TFlashStore::Lookup called
with wild OUT-param r3=0x80000110` from caller PC 0x000c96cc
(right after `bl FindSuperceeder`). The wild value 0x80000110
is the saved CPSR (USR mode + N flag) — same shape as
iter-49's R12 clobber, the unmistakable signature of an
`MRS scratch_fl, CPSR` write hitting a register that's actually
live.

#### Mechanism

`FindSuperceeder @0x001488a0` body:
```
1488a0: mov r3, r1            ; r3 = OUT-param (saved here)
1488a4: ldr r1, [r0, #16]     ; r1 = TFlashStore* (this for Lookup)
1488a8: mov ip, r1            ; ip = TFlashStore*
1488ac: ldrb r1, [r1, #61]    ; ← shadow_stub-patched LDRB
        ...
1488c8: b 0x1afef70 <Lookup-jumptable-thunk>
```

The picker for the LDRB at `0x001488ac` walks forward through
the body and at `1488c8: b 0x1afef70` follows the Direct branch.
0x1afef70 lies in the post-ROM jumptable region (above
ROM_IPA_BASE+ROM_IPA_SIZE = 0x01000000), so `read_insn(target)`
returns `None`. The walker then OR'd `APCS_RETURN_LIVE` into
live and returned — but **omitted `APCS_PARAMS` (R0..R3)**.

Tail-calls in APCS pass arguments in R0..R3, so any jumptable-
routed `b <fn>` should mark R0..R3 live at the call site.
Without it, the picker classified R3 as dead, picked it as
`scratch_fl`, and the stub's `MRS R3, CPSR` clobbered the
OUT-param pointer with 0x80000110.

#### Fix

Hoist `APCS_PARAMS` to a module-level const, and OR it into the
live mask whenever a Direct or Cond branch's target is
unreachable:

```rust
// BranchKind::Direct
if read_insn(target).is_none() {
    live |= (APCS_RETURN_LIVE | APCS_PARAMS) & !written;
    return live;
}
```

This hardens the picker against every jumptable-routed tail-call
in Newton ROM, not just FindSuperceeder. Every Newton ROM
function that ends in a `b <jumptable_slot>` is now treated as
parameter-passing.

#### Verification

- `shadow_stub pick @0x001488ac` now reports
  `ScratchVA sea=R12 sfl=R0 sad=R2` (3 caller-saved regs
  spilled to per-stub slot; no R3 touch).
- All 36 shadow_stub unit tests pass.
- All 36 guest tests pass.
- Cold boot no longer trips the Lookup-wild-r3 invariant.

#### Side fixes (downstream wedges this iteration unblocked)

The boot then ran ~80% further (Tmux task active, NewBlock #758
allocations) and hit:

1. `screen.GetScreenInfo: cannot write 0xcc77e70 @PC=0x801b84`
   — `peripherals/screen.rs::get_screen_info` was using
   `write_word_pa` on a guest VA. Fixed: translate VA → PA via
   `guest_mem::translate_va`, with identity fall-back when
   stage-1 is off (guest-test runtime).

2. `screen: unknown subfn 0x8 @PC=0x801be8 r1=0x4` — REx code
   queries `TMainDisplayDriver::GetFeature(feature_id)`. Added
   the `0x08 GetFeature` and `0x09 SetFeature` handlers per
   `Emulator/TNativePrimitives.cpp:1662`. Returns Einstein-style
   defaults for an un-configured ScreenManager.

3. `screen.blit` `read_byte_pa` was using PA on a VA — fixed
   the same way (translate-or-identity).

The next wedge — `screen.blit: src VA 0xc64d000 outside mapped
regions` — is iter-53 territory.


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
