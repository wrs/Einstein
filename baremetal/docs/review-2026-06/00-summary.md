# Stabilization review — synthesis summary (2026-06-11)

Six parallel review agents covered the codebase at working copy `somv 8b564c93`
(the silent-default audit): trap/emulation core, memory/MMU/snapshot, modelled
Newton peripherals, host platform layer, diagnostics/tooling, and overall
architecture. Full reports are in this directory (01–06). Finding IDs below are
`<report>-<id>`, e.g. `trap-H3` = report 01, finding H3.

**Overall verdict:** the architecture is sound and several pieces are genuinely
excellent — the build.rs backend-axis resolution, `hvc_imm.rs`, `banked.rs`'s
Table D1-79 treatment, the flash-vs-snapshot persistence split, and the
Einstein-citation discipline in peripherals. The review found no rot in the
load-bearing design. What it found is concentrated in three places: **a handful
of real latent bugs**, **a thick layer of Phase-B investigation residue still
compiled into hot paths** (one piece of which has quietly become load-bearing),
and **comment/doc drift in exactly the files that changed most**.

## The one finding everything else orbits

**The "wedge probe" in `irq_from_guest` is the de facto sound-completion model,
not a diagnostic.** Three agents converged on this independently
(src/trap.rs:385-422). It injects sound-DMA completion IRQs whenever the guest
PC parks for 64 heartbeats with sound IRQs armed — written as a Phase-B
hypothesis test, never moved to a targeted path. On `audio-null` builds (the
QEMU/FVP default), *nothing else ever raises the sound output-completion
interrupt*, so sound on the two primary dev platforms completes only via this
parked-PC heuristic with ~1 s latency, and it can mis-fire on any legitimately
idle guest. Fix: implement buffer completion in `audio/null.rs` (paced "buffer
drained → raise output mask", mirroring Einstein's null sound manager), then
delete the probe and `vic::inject_sound_dma_irq`. This is also the prerequisite
for the documented sound follow-on work.

## High-severity correctness findings

**Snapshot integrity (three related gaps):**
- The transient-PC autosave gate (src/snapshot.rs:452-460) misses two live
  trampoline ranges: the DABT fast trampoline at `0x008FFF00` and the FPA
  bypass stub at `0x00FFFEC0`. Both stash R0/R1/R12 in TPIDRURW/TPIDRURO, which
  the header doesn't capture; physical IRQs reach EL2 regardless of guest
  PSTATE.I, and the DABT fast path handles the dominant fault stream, so a
  poisoned slot over a long session is likely. (mem-H1)
- `SCRATCH_POOL` (384 KiB, guest-visible at IPA `0x0600_0000`) is not
  snapshotted, but the DABT save slot in it is read *later* by the patched
  kernel MRS at `0x393144` — a save in that window resumes with a zeroed slot.
  Cheapest fix: append the pool to the snapshot regions and bump `VERSION`.
  (mem-H2)
- DFAR/DFSR/IFSR (`far_el1`/`esr_el1`/`ifsr32_el2`) aren't in the header; a
  save between DABT forwarding and the kernel's DFSR read resumes with
  cold-boot fault registers. (mem-M1)

**Shadow-stub liveness treats conditional calls as unconditional**
(src/shadow_stub.rs:317-358, trap-H3): `BLNE`/`SVCcc` get the
caller-saved-clobber treatment regardless of condition, so a register live on
the not-taken path can be reported dead and clobbered by an `unaligned_inline`
stub. By the module's own contract this class "must not happen". Fix is cheap
and strictly conservative: don't add the clobber set for `cond != AL` calls.
**Verify this one first — it's the only finding that can silently produce wrong
guest execution.**

**`handle_align_fault` silently skips undecodable instructions**
(src/unaligned.rs:159-183, trap-H2): unreadable PC → `insn = 0` → decode fails
→ instruction skipped, guest resumes at PC+4 with stale state, silent after the
first 40 events. Exactly the category the `8b564c93` commit eliminates
elsewhere — this site got missed.

**`screen::ctx_blit_mode` reads `ctx.x[13]` (SP_usr) instead of the banked SP**
(src/peripherals/screen.rs:473-482, periph-H1) — the precise historical bug
`flash_driver.rs` documents at length and `docs/QEMU_BUGS.md` warns about. If
the kernel blits from SVC mode this reads junk off the user stack, and
`unwrap_or(0)` degrades it silently to srcCopy. Use
`banked::sp_for_mode(ctx, spsr)` and halt on read failure.

**Sub-word MMIO reads return the wrong BE-8 byte lane** (src/mmio.rs:520-526,
periph-H2): writes carefully splice into bits[31:24] for lane 0; reads always
return bits[7:0]. An `LDRB` of any modelled register silently reads the wrong
lane. Mirror the splice or halt loudly on `sas < 2` reads.

**Mailbox DMA buffer is only 16-byte aligned** (src/mailbox.rs:157-166,
platform-H1): the post-response `dc civac` on a cache line shared with adjacent
stack data can write a dirtied line back *over* the VideoCore's reply. Same
reasoning that forced `align(64)` on the MAI and UART rings. Make `Buffer`
`align(64)`.

**`patch_cp15_encodings` scans all 4M ROM words without the code/data bitmap**
(src/guest_mem.rs:2385-2413, mem-H3): a data word matching the MCR/MRC shape
gets silently rewritten. No false hits with the current ROM+REx, but every REx
rebuild re-rolls the dice. Add `rom_word_is_code(i)` to the loop.

**Two instances of by-the-book aliasing UB:** `dma.rs` creates a second
`&mut DmaState` via `drain_tx_channel` while the caller's borrow is live
(src/peripherals/dma.rs:273-323, periph-M2 — pass the borrow down);
`pi_hdmi.rs` writes ring frames through a pointer derived from
`&'static RingState` without `UnsafeCell` (src/audio/pi_hdmi.rs:795-801,
platform-M3 — the file already does it right for `MAI_TX_RING` twenty lines
later).

**Verified non-building feature combinations** (platform-M1): `sd-probe`
without `no-semihost` (E0599) and `input-mtouch` without `host-io-pi-fb`
(E0432), confirmed via `cargo check`; structurally, any FVP ×
real-hw-backend combination also can't build. build.rs enforces the platform
axis rigorously but nothing across axes — add a `validate_feature_matrix()`
panic with actionable messages.

**Other confirmed issues worth fixing in the same pass:** SDHOST
`read_block`/`write_block` leak `DATA_IRPT_EN` on error paths (the DMA variants
restore it; src/sd/sdhost.rs:341,362, platform-M2); the 4 KiB DMA TX-drain cap
can strand a transfer with no completion IRQ and nothing ever resumes the drain
(src/peripherals/dma.rs:362-368, periph-M3); the loud-halt canaries on
`Reboot`/`PowerOffAndReboot`/`StopImage` are unconditional, so the first
user-initiated reset on real hardware halts the hypervisor
(src/rom_patches.rs:1464-1482, mem-M7 — feature-gate them); `rep_print`'s
`%.*` doesn't consume its precision argument, shifting all later varargs
(src/rep_print.rs:121-135, diag-L1); the MMIO sub-word-write splice calls
`read()` on write-only registers — halting with a misattributed "unknown read"
and, worse, side-effecting `ROM_SERIAL_IX` (src/mmio.rs:336-349, periph-M1 —
needs a side-effect-free `peek`).

## Cross-cutting theme 1: Phase-B residue

The `a91c79c8` teardown got the scaffolding's head but not its tail. Still
compiled in unconditionally:

- The **"newt" tripwire exists verbatim twice** (src/trap.rs:353-374 polls a
  hardcoded PA on every timer IRQ; src/tracer.rs:479-507 on every traced call),
  plus the `cdsv`/`0x6e657774` one-shots and hardcoded alias walks in
  `task_dump.rs` — all citing `INVESTIGATION.md`, **which no longer exists in
  the repo**. (trap-M7, diag-H2)
- `guest_bp.rs` hard-codes six magic PCs from the closed heap-corruption hunt,
  several of which **halt the host** if a user installs a BP there today, and
  any of them suppresses snapshot autosave for the whole session
  (src/guest_bp.rs:376-656, trap-M1).
- ~40% of `tracer.rs` is dead probe payload (alloc-sequence watch,
  `SMemCopyToSharedSWI` one-shot, `dump_movefreeblock_entry` keyed on exact
  literal args). (diag-M1)
- `fix_stage1_xn_bits` still runs ~250 lines of dated audits (alias logger,
  INTENT classification — dead since `kernel_intent_mask_for` became a `None`
  stub) **on every M-toggle, i.e. every task switch**, with a 4 KiB stack array
  per walk. (mem-M5)
- `pi_hdmi.rs` carries its full bring-up bisection matrix (five IEC diagnostic
  modes, tone test, force flags) constant-folded to one configuration.
  (platform-M5)
- Orphans: `shadow_pool::allocate` has zero callers but full stage-2 plumbing;
  `usb_probe.rs` is superseded; `tarmac.rs` fires only its stop marker; half of
  `heap_check.rs` is dead behind a module-wide `#![allow(dead_code)]`;
  `scripts/build-sd.sh` still defaults to `pi-probe`.

Most of this is one commit of pure deletion. Report 05 has a full
keep/gate/delete disposition table — the durable capital (task_dump, rep_print,
symbols, trap_hist, the tracer core, pi_probe) is clearly separable from the
residue.

## Cross-cutting theme 2: duplication

- ARM helpers exist in duplicate with one divergence: `trap::arm_shift`
  approximates RRX *without carry* while `unaligned::apply_shift` is
  carry-correct; `ctx_slot_for_reg` is duplicated with the canonical
  `banked.rs` copy marked dead; condition evaluation, mode-name formatting, and
  the stage-1 walk (`trap::guest_translate_va` hardcoding TTBR0 vs
  `guest_mem::translate_va`, which trap.rs *also* calls) all have two-plus
  copies. (trap-M4)
- Six hand-rolled `static mut SEEN` dedup blocks and two byte-identical top-K
  trackers → one `SeenSet<N>` and one `TopK<N>`. (trap-M3)
- Guest-memory read/write helpers are copy-pasted across ~6 peripherals; the
  copies that forgot the halt convention (`screen`, `platform.Log`,
  `network.log_string`) are exactly where the silent-default bugs live.
- Three places hand-maintain the guest region list (stage2, `host_addr_for`,
  snapshot) — which is structurally *why* SCRATCH_POOL ended up guest-visible
  but unsnapshotted. A single region manifest table fixes the class.

## Architecture

The layering (boot → EL2 core → ISA compensation → modelled peripherals → host
backends → diagnostics) is real and holds well at the peripheral boundary.
Three structural issues:

1. **`trap.rs` is a 4,760-line hub** with type-level cycles
   (`trap ↔ peripherals`, `trap ↔ snapshot`) that exist only because
   `TrapContext` lives in the dispatcher. The highest-leverage single move is
   extracting `trap/context.rs` (TrapContext + `advance_elr` + helpers) — that
   breaks every cycle and makes layering visible in the import graph. Then
   split along the natural seams: `dabt.rs`, `und.rs`, `cp15.rs`, `hvc.rs`,
   probe handlers co-located with `rom_patches.rs`, diagnostics in `diag.rs`.
   The dispatch exit-hook sequences (tail of `trap_sync_lower_aarch32` vs
   `irq_from_guest`) are two hand-maintained near-copies worth unifying into
   one explicit list.
2. **"Real hardware" is an emergent condition**:
   `all(no-semihost, platform-raspi3b)` is scattered across ~11 files including
   hot IRQ paths. A build.rs-emitted `cfg(nh_real_hw)` names it once and makes
   the cross-axis validation fall out naturally. Relatedly,
   `peripherals/host_dma.rs` is a *host* driver misfiled in the guest-model
   directory.
3. **The snapshot resume contract is implicit per peripheral** — every
   peripheral state machine must tolerate reset-to-power-on under a mid-flight
   guest, but only `host_io` has an explicit `on_resume()` hook. Worth
   documenting or encoding before PCMCIA/serial state grows.

**Package-work readiness** (the documented gap): the persistence stack is
ready, but native code inside packages arrives at runtime above the ROM
aperture, where the load-time BE-8 classifier and the "bitmap-first triage"
doctrine silently stop applying. Worth a short design note before starting, not
code changes now.

## Doc drift (one batch commit)

`HIGHLEVEL.md §5.4` claims "no AP flattening, no shadow tables" — describing
the hardest-won subsystem as nonexistent; §3/§8 still say Einstein C++ classes
are "reused" rather than ported; status still "draft". Cargo.toml's `trace`
feature comment describes the retired UDF-first-touch mechanism. The FVP EL3
narrative contradicts itself across `fvp_base.rs`/`gicv3.rs` vs `boot.s` (the
ground truth). CLAUDE.md's snapshot section says "x0..x14" and "~14 MiB" (now
all 31 GPRs, ~6 MiB), and documents `UDF #0xFFFE` where the constant actually
encodes `#0xFF0E` (verified with objdump). Plus ~10 smaller stale comments the
agents pinned to exact lines (DABT_SAVE_PA location, sdhost "untested on real
hardware", `task_dump` doc comments severed from their items by later
insertions, etc.).

## Suggested sequencing

1. **Verify-first:** the conditional-BL liveness bug (silent wrong-code risk)
   and the screen banked-SP read.
2. **Behavior fixes:** null-audio completion + delete wedge probe; align-fault
   loud halt; snapshot gaps (regions + sysregs, one `VERSION` bump); mailbox
   alignment; MMIO read lane; cp15-patch bitmap gate; the two aliasing-UB
   fixes.
3. **Deletion commit:** all closed-investigation tripwires, guest_bp magic PCs,
   tracer probes, dead heap_check half, usb_probe, pi_hdmi diagnostic matrix.
4. **Feature matrix:** `validate_feature_matrix()` in build.rs, `nh_real_hw`
   cfg, and a `cargo check` sweep over the supported combinations wired into
   `run-all.sh`.
5. **Structure:** `trap/context.rs` extraction, then the EC-class split; region
   manifest; helper consolidation.
6. **Docs batch commit.**

The phased, agent-executable version of this sequencing is `PLAN.md` in this
directory.
