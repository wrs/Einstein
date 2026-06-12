# Stabilization implementation plan (2026-06 review)

Phased plan derived from the review reports in this directory. **Coverage is
total: every finding and every recommended refactor in reports 01–06 is
scheduled in a phase below — nothing is deferred or prioritized away.** Each
phase is sized for one implementation agent and lands as one jj commit (or two
where noted). A coordinating session reviews the diff and test results after
every commit before the next phase starts. There is no schedule pressure;
correctness and completeness win every trade.

Finding IDs reference the reports: `trap-H3` = `01-trap-emulation.md` finding
H3, `mem-M5` = `02-memory-mmu-snapshot.md` M5, `periph-…` = 03, `platform-…` =
04, `diag-…` = 05, `arch-§N`/`arch-#N` = 06.

**Line numbers in the reports refer to working copy `somv 8b564c93` and drift
as phases land.** Re-locate by symbol name, not line number.

## Agent contract (applies to every phase)

- **Source control is jj, not git.** Start with
  `jj new -m 'WIP: baremetal: <short description>'`. Finish by writing the full
  commit message with `jj describe --stdin <<'EOF' … EOF` (multi-paragraph;
  standard git commit format; NEVER add Co-Authored-By). Keep the `WIP:` prefix
  if the phase touches real-hardware-only code paths (the phase spec says so) —
  Walter validates those on hardware before the prefix drops. Otherwise drop
  `WIP:` once validation passes.
- **Stay in scope.** Implement only what the phase lists. If you find an
  adjacent problem, note it in your final report; don't fix it. (It will be
  scheduled — nothing gets dropped.)
- **Never trust memory for ARM details.** Check `docs/ARM_Reference.txt` for
  register/encoding semantics; use `scripts/disasm-out/rom.dis` for ROM code
  (see `docs/DISASM.md`); verify hand-rolled instruction encodings with a real
  assembler/disassembler (`arm-none-eabi-objdump` / clang) — the codebase has
  compile-time encoder asserts (`unaligned_inline::_check_encoders`) as the
  model to follow.
- **Loud-halt convention.** Unknown inputs on emulation paths halt with a
  context dump (`kprintln!` + `cpu::halt()`), never a silent default. Routine
  diagnostics go through `dprintln!`/the `log_*` macros, not `kprintln!`.
- **Comments describe current state only** — no "X is now…", no change
  narration. That goes in the commit message.
- **Validation is QEMU-first.** See toolbox below. The FVP is available but
  slow; use it only where the phase says so. Real-hardware flashing is done by
  Walter on request — phases that need it say so explicitly, and hardware
  iteration is available whenever a phase needs it (it is never a reason to
  weaken a fix).

## Validation toolbox

```bash
# Builds (run the ones the phase lists):
cargo build --release                                            # QEMU default
cargo build --release --no-default-features --features "platform-fvp-base quiet"
cargo build --release --no-default-features --features pi-bare-metal-input     # real-hw aggregate
cargo build --release --features "trace quiet"                   # tracer build

# Guest tests (QEMU). Required whenever changes touch src/shadow_stub.rs,
# src/unaligned*.rs, src/peripherals/*, src/banked.rs, src/stage2.rs,
# src/guest.rs, generic SBA/UND/DABT/IRQ paths in src/trap.rs, or guest-tests/:
guest-tests/scripts/run-all.sh

# QEMU boot smoke test. RULES (hard-won; two agents wedged on the old
# form — see the qemu memory note):
#   - NEVER pipe cargo run through tail/head — redirect to a file, then read it.
#   - `timeout N cargo run` orphans qemu, AND timeout itself can hang
#     (cargo survives SIGTERM while waiting on qemu, so a trailing
#     pkill never runs). Use `timeout -k 5` so SIGKILL backstops it.
#   - Pre-flight check before EVERY qemu launch; ONE qemu at a time —
#     concurrent instances (guest-test runs included) throttle each
#     other ~10x and both results are garbage.
#   - Scratch files under /tmp/newton-claude/.
mkdir -p /tmp/newton-claude
pgrep qemu-system >/dev/null && { pkill -9 qemu-system; sleep 2; }   # pre-flight
rm -f /tmp/newton-snapshot-*.bin            # cold boot
timeout -k 5 150 cargo run --release > /tmp/newton-claude/boot-<phase>.log 2>&1 || true
pkill -9 qemu-system                        # always reap
# Compare against the baseline log /tmp/newton-claude/boot-baseline.log:
# boot must reach the same end state (Welcome UI / steady idle), with no new
# "*** " halt lines, no new UNHANDLED/halt dumps. diff the trap-summary shape,
# not byte-for-byte.

# Snapshot round-trip (for phases touching snapshot.rs or stage-2 layout):
# 1. cold boot ≥30 s (autosaves land), kill qemu;
# 2. run again, confirm "Resuming guest from snapshot at PC=…" and that the
#    guest continues (further autosaves, no halt) for ≥30 s.
```

A baseline boot log is captured by the coordinator before Phase 1 and kept at
`/tmp/newton-claude/boot-baseline.log`.

---

## Phase 1 — Emulation-path correctness fixes

QEMU-validatable; guest tests REQUIRED. One commit.

1. **trap-H3 (verify first, then fix):** `shadow_stub::analyze_insn` /
   `live_regs_at` treat conditional `BL`/`BLX`/`SVC`/`HVC`/`SMC` as
   unconditional calls — the caller-saved clobber set
   (`APCS_CALLER_SAVED & !live` marked written) is wrong for `cond != AL`,
   so a register live only on the not-taken path can be reported dead and
   clobbered by an `unaligned_inline` stub. First *verify* the bug by reading
   the walker: confirm data-processing writes already require `cond_al` while
   `BranchKind::BLink` does not check the condition field. Then fix: for
   conditional calls, do not add the clobber set to `written` (reads stay
   conservative). Add a unit-style case if the module has a test hook;
   otherwise document the rule next to the existing conditional-write rule.
2. **periph-H1:** `screen::ctx_blit_mode` must read the *banked* SP for the
   trapping mode — read SPSR_EL2, use `crate::banked::sp_for_mode(ctx, spsr)`
   (the same pattern as `trap.rs` and `flash_driver.rs`), and halt loudly on
   translate/read failure instead of `unwrap_or(0)` + silent srcCopy fallback.
3. **trap-H2:** `handle_align_fault` skip path: halt loudly (context dump à la
   the other emulation paths) when the faulting insn is unreadable or
   undecodable, instead of silently skipping the instruction. Remove the
   40-event log budget along with the skip.
4. **diag-L1:** `rep_print.rs` `%.*` precision: consume the argument
   (`args.next()`) like the width path does.
5. **trap-L2:** ISS SRT/Rt field == 31: replace the implicit Rust panic
   (index out of bounds on `ctx.x`) with an explicit loud halt. Check
   `docs/ARM_Reference.txt` for ISS.SRT semantics first; if 31 is genuinely
   architecturally impossible for AArch32 traps, the loud halt documents that.
6. **trap-L6:** `unaligned::set_return` forwards SPSR `0` to
   `return_to_guest_from_und`, silently disabling that function's
   USR-target-in-trampoline diagnostic. Pass the real pre-abort CPSR through.

Validation: `cargo build` (QEMU + FVP), `guest-tests/scripts/run-all.sh`, QEMU
boot smoke vs. baseline. The screen fix changes blit behavior only when the
old code was reading the wrong stack — visually the boot splash/Welcome UI in
the QEMU window should be unchanged or improved; check the log for new halts.

## Phase 2 — Null-audio completion; delete the wedge probe

QEMU-validatable. One commit. (trap-H1 / diag-H1 / arch-§2.)

1. Implement buffer completion in `src/audio/null.rs`: `schedule_output`
   records the channel/output mask (the same subfn 0x1F data `pi_hdmi`
   consumes) and arms a completion; the completion raises the sound-DMA
   IRQ(s) via the same `vic` path the wedge probe used. Pace it from the
   timer tick (`trap_irq`'s existing per-tick pump sequence) — "buffer
   duration elapsed → raise mask" is the goal; derive the duration from the
   schedule_output args/sample-rate contract in `src/audio/mod.rs`.
   Cross-check Einstein's null sound manager as the oracle. Route the new
   call through the `audio::` seam (cfg-dispatched in `audio/mod.rs`), NOT
   from trap.rs directly.
2. Delete the wedge probe block in `irq_from_guest` (`src/trap.rs`, the
   64-heartbeat parked-PC detector + `dump_und_history`/`task_dump` one-shot)
   and `vic::inject_sound_dma_irq` if it has no remaining callers.
3. Keep `peripherals/sound.rs`'s modelling untouched except where it needs to
   expose the armed output mask to the audio seam (if it doesn't already).

Validation: QEMU boot smoke vs. baseline — pay attention to boot reaching the
Welcome UI *with the boot chime path completing* (previously the wedge probe
fired ~1 s after PC parked; now completion should be prompt). Run
`guest-tests/scripts/run-all.sh` (sound guest test exists). Also build FVP.
If the QEMU boot stalls where it previously progressed, the ROM is genuinely
waiting on a completion the null backend isn't raising — fix the backend, do
not reintroduce a parked-PC heuristic.

## Phase 3 — Snapshot integrity + ROM-load hardening

QEMU-validatable (snapshot round-trip). One commit. Bump snapshot `VERSION`
once for all changes.

1. **mem-H1:** extend `pc_in_hypervisor_transient_region` to cover the DABT
   fast trampoline (`0x008FFF00..0x00900000`) and the FPA bypass stub /
   patch-stub arena (`0x00FFFD80..0x01000000` — verify the exact lower bound
   against the constants in `guest_mem.rs` before hardcoding). Additionally
   add `tpidr_el0`/`tpidrro_el0` to the snapshot header (defense in depth —
   the stubs stash live state there).
2. **mem-H2:** snapshot the 384 KiB `shadow_stub::SCRATCH_POOL` region
   (guest-visible at IPA 0x0600_0000) alongside RAM + FB.
3. **mem-M1:** add `far_el1`, `esr_el1`, `ifsr32_el2` to the header and
   restore path (AArch32 DFAR/DFSR/IFSR homes).
4. **mem-L3:** eliminate the uninitialized padding hole in `Header`
   (explicit `_pad` field, as the existing `_pad0`/`_pad1`).
5. **mem-M2:** replace the silent `_ => 0` / `_ => {}` arms in
   `read_sysreg64`/`write_sysreg64` with loud halts (or eliminate the
   stringly-typed dispatch entirely).
6. **mem-H3:** gate `patch_cp15_encodings` on `rom_word_is_code(i)`; log each
   patched PC once (count + first few PCs) so an unexpected future hit is
   visible. Apply the same gate to `patch_native_prim_mcr_lr_to_r12`.
7. **mem-M6:** add one `icache_publish_range` sweep over the patched ROM
   ranges at the end of `load_newton_rom` (after all patching, including
   rom_patches and cp15 rewrites). Simplest correct form: publish the whole
   16 MiB ROM aperture once — measure boot-time cost first; if it's
   negligible (likely), prefer it over a recorded-ranges list. The
   per-range publishes in `patch_und_vector` then become redundant — remove
   them only if the sweep provably covers them (same cache-op, wider range).
8. **mem-M7:** feature-gate `apply_loud_halt_traps` (the
   StopImage/Reboot/PowerOffAndReboot/busError canaries): keep them ON for
   semihost/dev builds (QEMU/FVP default), OFF under `no-semihost` (real
   hardware), via a build.rs-emitted cfg or an existing feature. A user
   reset on hardware must not halt the hypervisor.
9. **mem-L2 / mem-L4:** assert TTBR0 == 0x0400_0000 in the CP15 TTBR0 write
   shim (loud halt on anything else); zero `vbar_el1` in
   `zero_el1_guest_state`.
10. **mem-L1:** the alias-audit L2-descriptor reads use `read_word_pa`
    instead of the BE-8 `read_pt_entry` path. Fix the reader now so the
    diagnostic prints truthfully until Phase 6 deletes the audit (a one-line
    change; do it rather than leaving a known-lying diagnostic in tree).

Validation: QEMU cold boot ≥30 s; snapshot round-trip per toolbox (resume
must print the new version and continue ≥30 s); guest tests (snapshot test
exists); FVP build. Old snapshots must be *rejected* (version bump), not
misparsed — verify the "cold boot fallback" line appears when resuming over
a pre-change slot.

## Phase 4 — MMIO / guest-DMA / serial correctness

QEMU-validatable; guest tests REQUIRED (peripherals). One commit.

1. **periph-H2:** BE-8 sub-word MMIO *reads*: extract the lane the write
   splice writes — for `sas < 2`, read the aligned word and return
   `(value >> (24 - 8*lane)) & 0xFF` (halfword analogous). Implement once in
   `mmio.rs` next to `splice_byte` so read and write lane math share
   constants.
2. **periph-M1:** add a side-effect-free `peek_word(ipa) -> Option<u32>` to
   the peripheral dispatch, used by the sub-word write splice (and by the new
   sub-word read path where the underlying register has read side effects —
   audit `ROM_SERIAL_IX` specifically). Write-only registers peek as
   `Some(0)`. The loud-halt for genuinely unknown addresses stays.
3. **periph-M2:** fix the aliased `&mut DmaState`: thread the existing
   `&mut DmaState` borrow into `drain_tx_channel(s, ch_idx)`.
4. **periph-M3:** add `dma::poll_tx()` driven from the same `trap_irq` pump
   site as `poll_rx()`, continuing the drain of armed TX channels past the
   4 KiB per-trap cap and raising the completion event/IRQ exactly when
   Einstein's countdown reaches 0 (`TPtySerialPortManager::HandleDMA` is the
   oracle).
5. **periph-M6:** forward `serial::TX_BYTE` PIO writes for port 0 to
   `uart::write_byte` (matching the DMA path); keep a budgeted diagnostic
   log; add a dropped-byte counter for ports 1–3, surfaced in a diagnostic
   dump (not silently discarded).
6. **periph-M8:** make pcmcia's unreachable `owns()`-then-`None` arms halt
   loudly like vic/dma do.
7. **periph-M4:** split log budgets: routine/expected stub traffic through
   `dprintln!` (or a tight budget), unknown offsets on their own budget so
   discovery never goes silent. pcmcia + dma.
8. **periph-M7:** gate the `TEST_SCRATCH` window at 0x1200_0000 behind
   `cfg(nh_guest_test)`.
9. **periph-M5:** convert `vic::write`'s `static mut LOG_N` and
   `sound::handle`'s `static mut SEEN/SUBFN_COUNT` to atomics; document the
   real borrow invariant on `VicCell`/`DmaCell` ("no `&mut` borrow live
   across an EL2 IRQ-unmask window — currently only `pause_system`'s WFI
   loop").
10. **periph-L6:** `platform::log_message` and `network::log_string` guest
    read failures: align with the halt convention (these are the only two
    native-prim guest reads that don't halt; Einstein completes the path, so
    a failed read is an emulation bug, not a guest bug — halt loudly).
11. **Low cluster:** periph-L1 (`raised` vs `int_present_raw` — keep one,
    migrate callers), periph-L3 (`let _ = value;`), periph-L4 (`is_flash_pa`
    via `pa_to_offset`), periph-L7 (`tick_advance` alias — rename the one
    caller, delete the alias; also resolve the `tick_page::update` shim
    question from mem-L7 by migrating its callers), diag-L6's sound.rs
    `kprintln!` → `dprintln!`/budgeted, periph-L2 (fix the RTC seed comments
    to match the actual value 2026-05-16T00:00:00Z — verify with `date -r`).

Validation: full guest-test run (dma, dma_irq, serial, serial_driver, pcmcia,
sound, mmio_regs tests are directly in play), QEMU boot smoke vs. baseline,
FVP build. The sub-word read change is behavior-visible: watch the boot log
for *new* loud halts at sub-word reads — if one fires, the guest genuinely
does lane-0 byte reads and the extraction path (not a halt) must handle it.

## Phase 5 — Real-hardware fixes (KEEP `WIP:` until hardware validation)

Compile-validated here; Walter flashes to the Pi Zero 2 W to validate —
**plan on at least one flash/test cycle, more for the CTS work; hardware
iteration is available on request.** One commit (split CTS into its own
commit if iteration demands it), `WIP:` prefix stays until hardware sign-off.

1. **platform-H1:** `mailbox::Buffer` → `#[repr(C, align(64))]`, size already
   a 64-byte multiple; fix the `dc_civac_range` doc comment (inbound DMA
   buffers must be line-aligned and line-padded).
2. **platform-M2:** restore `SDHCFG = hcfg_base` on `read_block`/`write_block`
   error paths (small RAII guard or single-exit restructure, matching the DMA
   variants' discipline).
3. **platform-M3:** wrap `RingState.frames` in `UnsafeCell`, route the
   `schedule_output` write through `.get()` (mirror the `MAI_TX_RING`
   pattern in the same file).
4. **platform-M5:** collapse `pi_hdmi.rs`'s constant-folded diagnostic matrix
   to the shipped configuration: remove `TONE_TEST_48_KHZ`, the five-mode
   `IEC_DIAGNOSTIC_MODE` machinery and dead preamble branches,
   `ENABLE_MAI_AFTER_INFOFRAME` / `USE_MAI_CTL_PAREN` /
   `FORCE_AUDIO_SAMPLE_PRESENT` / `FORCE_AUDIO_B_FRAME` /
   `SKIP_AUDIO_INFOFRAME` toggles and the `#[allow(dead_code)]` `mai_ctl_*`
   helpers. Preserve the knowledge as one prose paragraph ("alternatives
   tried and why they lost"). The compiled configuration must be bit-for-bit
   what the constants currently select — derive it mechanically from the
   current constant values, do not re-decide anything.
5. **platform-M4 (full fix):** derive CTS from the mailbox-measured pixel
   clock when the reading is sane, falling back to `PANEL_PIXEL_CLOCK_HZ`
   only for the documented known-bad reading (the 85.5 MHz case) — and
   comment exactly which readings are considered bad and why. The boot log
   must print the value actually used for CTS, labelled as such. This is the
   item most likely to need hardware iteration (audio quality check on the
   panel + ideally a second HDMI sink); coordinate flashes with Walter.
6. **platform-L2:** mailbox poll-loop exhaustion → `MailboxError::Timeout`
   (new variant), not fall-through to `FirmwareError`; fix the `BUS_UNCACHED`
   "ANDing" doc.
7. **platform-L3:** replace `sdhost::delay_us`'s nop loop with a
   CNTPCT-based implementation (pattern: `cpu::delay_ms`). Also do the
   one-time cross-check against Linux's bcm2835-sdhost for the CRC7-on-R3
   (`SEND_OP_COND`) exemption and apply it if our init path can issue R3
   commands that legitimately carry 0xFF CRC.
8. **platform-L4:** decode CSD (CMD9 response is already fetched) and report
   real `num_blocks` instead of `u32::MAX`, restoring embedded-sdmmc's
   whole-device bounds checks.
9. **platform-L5:** mtouch: distinct error/message for VID/PID mismatch vs
   controller-not-ready.
10. **platform-L6:** EL2 stack guard: place a canary word at the stack limit
    (set up in early boot), check it in `trap_irq` and on the halt paths,
    loud-halt on corruption. Applies to both linker scripts. (A faulting
    guard page is the stronger fix; implement it instead if the EL2 MMU
    setup in `mmu.rs` makes a no-access page at the stack base
    straightforward — judge on the actual code, document the choice.)
11. **platform-M6 (subsystem-local doc fixes only):** fvp_base/gicv3 EL3
    narrative rewritten against boot.s; timer.rs FVP "TODO"; sdhost
    "untested on real hardware" + R1b/CMD12 comment; pi_hdmi
    `set_audio_info_frame` doc; `flash_persist::maybe_save` stale cfg_attr.

Validation: `cargo build` for all four `pi-bare-metal*` aggregates +
QEMU default + FVP. QEMU boot smoke (mailbox/sdhost/pi_hdmi are compiled out
there, so this only proves no collateral damage). Then **request a hardware
flash from Walter**: boot to Welcome UI, confirm display + touch + audio
(boot chime quality — the CTS/IEC path was touched) + SD autosave lines;
re-flash per CTS iteration as needed. The commit drops `WIP:` only after
hardware sign-off.

## Phase 6 — Phase-B residue deletion sweep

QEMU-validatable. One commit, overwhelmingly deletions. Use the disposition
table in `05-diagnostics-tooling.md` as the checklist.

1. **diag-H2 / trap-M7:** delete every concluded-investigation tripwire:
   both `0x0402a250` "newt" tripwires (trap.rs heartbeat + tracer.rs), the
   `FAR == 0x6e657774` one-shot + `"cdsv"` dumps in `handle_dabt_dispatch`,
   `dump_phys_for_pa(0x0402_a000)` in `task_dump::dump_full`, the
   per-blocked-task newt stack window, the `0x0c602e2c` alias walk in
   `dump_save_area`, the `rex-dabt` ELR-range logging, the `handle_und`
   one-shot DIAG block, and the iter-85 suppressed-tarmac comment stubs.
2. **trap-M1:** strip `guest_bp.rs` to the generic one-shot facility: delete
   the six magic-PC arms in `handle_user_bp_und` and the `0x0031_3308`
   logging special case; re-sync the module doc with the actual behavior.
3. **diag-M1 / diag-L2:** strip `tracer.rs` to its durable core (trampoline
   install, `rewrite_first_insn`, `log_trace_at` main line, putc
   line-buffering). Keep `SVC_WATCH` as the documented extension point.
   Remove the lint-suppression hacks (`let _ = cpu::halt;`, `let _ = sets_pc;`
   — drop the unused tuple field).
4. **diag-M3:** heap_check: delete `force_kernel_diag_on` and the unused
   `log_ref`/`classify_ptr`/`dump_object`/`print_object`/`pretty_print_ref`
   half; remove the module-wide `#![allow(dead_code)]`; fix or drop the
   contradictory endianness comment. Also diag-L3: drop the VA→PA fallback
   for the cached bounds read (or don't cache on the fallback path).
5. **diag-M4:** gate `tarmac.rs` and its call sites behind
   `platform-fvp-base`; rewrite the stale module header.
6. **diag-L4:** delete `usb_probe.rs`, its `[[bin]]` stanza and `usb-probe`
   feature.
7. **mem-M5:** strip `fix_stage1_xn_bits` to the normalization it needs on
   the task-switch path: delete the subpage-AP heterogeneity audit, the
   PA→VA alias logger + `LOGGED_ALIAS_BITMAP`, the dead INTENT branch (and
   `trap::kernel_intent_mask_for`, the always-`None` stub), and the
   "PROBE 2026-04-26" tracker. Delete `shadow_pool.rs` entirely (zero
   callers of `allocate`): its stage-2 entries, `host_addr_for` region, and
   the `main.rs` smoke test go with it.
8. **mem-L7:** delete the never-installed `apply_new_stack_pad_wrapper` /
   `apply_lock_heap_range_wrapper` machinery (keep the two-line "why not"
   comment); narrow `guest_endian.rs`'s blanket `#![allow(dead_code)]`.
9. **diag-L5:** flip `scripts/build-sd.sh` default kernel to
   `newton-hypervisor` (pi-bare-metal-input), keep `pi-probe` as documented
   override.
10. **diag-L7:** convert the remaining `static mut` one-line counters touched
    by this sweep (`TRACE_SEQ`, surviving heartbeat counters) to atomics
    while their files are open. (The full SeenSet consolidation is Phase 8.)

Validation: build QEMU + FVP + `trace quiet` + all `pi-bare-metal*`
aggregates (deletions love to break gated builds); full guest-test run;
QEMU boot smoke vs. baseline (must be indistinguishable modulo deleted log
lines); QEMU snapshot round-trip (stage-2 layout changed if shadow_pool's
entries go — confirm whether that requires a snapshot VERSION bump and do it
if stage-2-visible state changed). Also do a tracer run
(`trace quiet`, cold boot, confirm trace lines still flow).

## Phase 7 — Feature-matrix enforcement

Build-system phase; validation is the matrix itself. One commit.

1. **platform-M1:** `validate_feature_matrix()` in build.rs, panicking with
   actionable messages: `input-mtouch requires host-io-pi-fb`,
   `sd-probe requires no-semihost + platform-raspi3b`,
   `host-io-pi-fb / flash-persist-sd / audio-pi-hdmi / input-mtouch require
   platform-raspi3b`. Follow the existing `select_platform_linker_script`
   error style.
2. **platform-M7 / arch-§3:** emit `cfg(nh_real_hw)` from build.rs
   (definition: `no-semihost` + `platform-raspi3b`, i.e. BCM2835 DMA and real
   peripherals exist) and replace every scattered
   `all(feature = "no-semihost", feature = "platform-raspi3b")` with it
   (sdhost ×6+, uart ×4, input/audio/flash_persist mod.rs, trap.rs,
   host_dma). Register the new cfg with `cargo::rustc-check-cfg` as build.rs
   already does for the other nh_ cfgs.
3. **platform-L1:** map `host-io-pico` explicitly onto the null backend
   (mirroring `flash-persist-pico`) or make the resolver panic "reserved,
   not implemented" — pick whichever `flash-persist-pico` precedent suggests.
4. **arch-#5:** add `scripts/check-matrix.sh`: `cargo check` over the
   supported set — default; `platform-fvp-base` (+quiet); the four
   `pi-bare-metal*` aggregates; `trace,quiet`; `host-io-semihost`;
   `sd-probe` aggregate form; guest-test cfg if cheap. Wire it as an opt-in
   step at the top of `guest-tests/scripts/run-all.sh` (env-var gated so the
   normal test loop doesn't pay ~8 cargo invocations) and document it in
   CLAUDE.md's test section.

Validation: run `scripts/check-matrix.sh` — every listed combination must
pass; deliberately test one forbidden combo to confirm the validator panics
with the intended message. QEMU boot smoke (no runtime change expected).

## Phase 8 — Helper consolidation (duplication)

QEMU-validatable; guest tests REQUIRED. One commit.

1. **trap-M4:** single home for ARM helpers:
   - banked-register slot mapping: keep `banked::ctx_slot_for_reg`, delete
     `unaligned::ctx_slot_for_reg`, migrate callers.
   - one `arm_cond_passed` (truth table currently duplicated as
     `trap::arm_condition_passed` / `unaligned::cond_passes`).
   - one `arm_shift` — keep the carry-correct `unaligned::apply_shift`
     semantics; delete trap.rs's carry-less RRX version and migrate its
     flash-write-drop caller. **This is a behavior change for RRX writeback
     addresses — note it in the commit message; guest tests + boot smoke
     cover it.**
   - one mode-name formatter (three copies in trap.rs today).
   - delete `trap::guest_translate_va` in favor of `guest_mem::translate_va`.
   Suggested home: `banked.rs` for the slot map; a small `arm_decode.rs` (or
   an existing decode-adjacent module) for cond/shift/mode-name.
2. **trap-M3:** one `SeenSet<const N: usize>` and one `TopK<const N: usize>`
   (in `trap_hist.rs` or a new `diag_util.rs`), replacing the six hand-rolled
   `static mut SEEN` blocks and the duplicated `TopK`/`RejTopK`.
3. **periph refactor #3:** a `LogBudget` utility (`LogBudget::new(N)` +
   `.log(args…)`, atomic counter inside) with the expected-stub vs
   unknown-input split from periph-M4 built in; migrate the ~8 hand-rolled
   budget patterns across peripherals (and the Phase-4 split sites) onto it.
4. **periph refactor #1:** `peripherals::guest_access` (or additions to
   `guest_endian`): `read_word_or_halt(addr, what, pc)` / `write_…` /
   `read_byte_…` with VA-first/PA-fallback semantics, replacing the ~6
   copy-pasted private helpers (flash_driver, platform, battery, tablet,
   screen, network). The loud-halt convention is the default.
5. **trap-M6:** extract the duplicated TStackInfo run-flush block in
   `dump_tstacks_and_check_invariants` into one local fn/closure.
6. **trap-M5:** restitch the three mangled doc comments in trap.rs
   (handle_loud_halt / handle_bootos_canary / handle_unhandled_exception);
   diag-M5: reunite the severed task_dump.rs docs and give `jt_target` its
   own doc line.
7. **trap-L3:** fix `code_write_word`'s stale SAFETY comment (real invariant:
   guest paused in an EL2 trap on the only core).
8. **trap-L4:** add the known-rejected-PC cache (~32 entries) in
   `unaligned_inline` so a PC that failed `pick_scratches` doesn't re-run
   decode + the CFG liveness walk on every subsequent alignment fault.
9. **mem-L5:** single `is_hypervisor_code_region(pa)` predicate shared by
   `guest_endian::pa_is_rom_code` and
   `snapshot::pc_in_hypervisor_transient_region` so the two range lists can't
   drift (Phase 3 already aligned their contents). Cover ALL runtime-written
   code regions (UND/DABT trampolines, FPA stub, DABT fast trampoline,
   UND-return stub, patch-stub arena, tracer pool).
10. **trap-L1 + hvc-L5:** `BP_UDF_INSN`: fix the docs to `#0xFF0E` (do NOT
    change the constant — see Phase 12 docs item for CLAUDE.md);
    `hvc_imm.rs` module doc: the test-ABI block terminator is
    `GuestInjectPen`, not `Debugger`.

Validation: full guest tests, QEMU boot smoke vs. baseline, FVP build,
`scripts/check-matrix.sh`.

## Phase 9 — trap.rs decomposition + host-layer re-homing

Mechanical moves, minimal rewrites. Three commits, guest tests after each.

1. **Commit A (arch-#1 first move):** create `trap/context.rs` (or
   `trap_context.rs` if a directory module fights the existing layout):
   `TrapContext`, `advance_elr`, the `read_sysreg!`-style macros,
   `describe_ec`, the (now single, post-Phase-8) mode-label helper. Update
   every importer (`peripherals/*`, `banked`, `unaligned*`, `tracer`,
   `guest_bp`, `snapshot`) to import the context module, not `trap`. After
   this commit no module outside `trap*` may import the dispatcher.
2. **Commit B:** split by EC class along the report-01-M2 / report-06-§2
   seams: `trap/dabt.rs` (handle_data_abort, resolve_ipa, ISV=0 emulation,
   ROM-write absorb, flash-write drop, DABT forwarding), `trap/und.rs`
   (handle_und + UND history + SWP/FPA/DDK/MRS-SPSR emulators +
   return_to_guest_from_und), `trap/cp15.rs` (handle_cp15_trap + cp15 mod +
   flash-checksum reseed), `trap/hvc.rs` (tag dispatch), `probes.rs` next to
   `rom_patches.rs` (Hammer*/StorePermObj/canary/DAH handler bodies),
   `trap/diag.rs` (heartbeat, budgeted loggers, tstack invariant dump,
   loud-halt rendering), VA-walk utilities (`resolve_guest_pa`,
   `read_cstr_at`, `scan_to_null_word_aligned`) merged into `guest_mem`.
   Dispatch + IRQ paths stay in `trap.rs`, and the two trap-exit hook tails
   (`trap_sync_lower_aarch32` vs `irq_from_guest`) are unified into one
   explicit, ordered exit-hook sequence both paths call. Move code verbatim;
   the only edits are visibility (`pub(crate)`) and imports.
3. **Commit C (arch-#3 remainder):** re-home the host layer: move
   `peripherals/host_dma.rs` to `src/host_dma.rs` (or `src/host/dma.rs` —
   match the existing flat-vs-directory style of `sd/`/`mailbox.rs`); push
   the BCM2835 pending-register IRQ dispatch out of `trap_irq` /
   `irq_from_guest` and behind `platform::` (which already owns
   `irq_ack`/`irq_eoi`), so the IRQ path is free of platform cfg blocks.
   This untangles the `uart ↔ peripherals` triangle: after the move,
   `peripherals/dma.rs` (guest model) depends on `uart` only via the
   existing RX feed, and `uart`'s DMA TX depends on the relocated host
   driver, not on `peripherals/`.

Validation after each commit: full guest tests, QEMU boot smoke vs.
baseline, FVP build, `scripts/check-matrix.sh`, QEMU snapshot round-trip
(commit A touches snapshot's imports). `jj diff --stat` should show trap.rs
shrinking by roughly the moved line counts — large unexplained deltas mean
rewriting crept in. Commit C touches real-hw IRQ routing: pi aggregates must
build, and a hardware flash is REQUIRED before its `WIP:` drops (can be
batched with the Phase 5 flash if phases land close together, otherwise its
own flash).

## Phase 10 — Region manifest + unified patch installer + trampoline extraction

The structural fixes for the "three hand-maintained region lists" and "three
generations of install conventions" classes. Two commits.

1. **Commit A (mem refactor #1 — region manifest):** one table of
   `(name, ipa, size, host_pa, stage2 perms, stage1 mapping?, snapshot:
   yes/no)` as the single source of truth, consumed by `stage2::init`,
   `guest_mem::host_addr_for`, `snapshot::{save,load}`, and the Phase-8
   `is_hypervisor_code_region` predicate. Every region currently
   hand-listed in those places moves into the manifest; a region present in
   stage-2 but absent from the manifest is a compile- or boot-time error,
   not a silent omission. Snapshot `VERSION` bumps if the serialized region
   set/order changes.
2. **Commit B (mem refactor #2 + #3):**
   - **Unified patch installer (fixes mem-M3 + mem-M4 structurally):** a
     small `aarch32_emit` module (the branch/literal encoders currently
     scattered as `arm_b`/`arm_bl`/`arm_b_cond` in rom_patches and the
     `beq`/`b_far`/`ldr_r0_lit` closures in guest_mem) + one
     `install_patch { expected_orig, words, record }` API that verifies the
     original word (loud halt on mismatch — with an explicit opt-out flag
     for the genuinely optional probes, used sparingly and visibly), records
     originals into the shadow-stub side table unconditionally, and
     I-cache-publishes. Migrate ALL installers onto it: `patch_probe`,
     `write_stub_and_patch`, `write_stub_words`, `apply_loud_halt_traps`,
     `apply_debug_patches`, `apply_real_clock_seconds_patch`, the
     FTime/FDate installers, `apply_bootos_trap`, the lock/unlock wrappers,
     and the `PATCHES_717006` rewrites. Verify encoder output against a real
     disassembler (compile-time asserts à la `_check_encoders`).
   - **Extract `guest_trampolines.rs`:** move `patch_und_vector`,
     `patch_dabt_vector`, `install_dabt_fast_trampoline` and their offset
     constants (~700 lines of hand-assembled AArch32) out of `guest_mem.rs`;
     the module owns its address-range constants and feeds them to the
     manifest/predicate from Commit A.

Validation per commit: full guest tests (rom_patches test exists), QEMU boot
smoke vs. baseline, QEMU snapshot round-trip, FVP **boot** (not just build —
stage-2/cache-maintenance changes are exactly where FVP catches what QEMU
forgives; use `scripts/fvp --timeout=180`), `scripts/check-matrix.sh`.

## Phase 11 — Dispatch-contract traits + slim-ISR state ownership

One commit. (periph refactor #2 + arch-§4's targeted abstraction.)

1. **`MmioPeripheral` trait** (`owns`, `read`, `write`, `peek_word` from
   Phase 4) implemented by vic, dma, pcmcia, serial, flash, screen(+mmio
   parts) — whatever currently sits in `mmio.rs`'s router; and a
   **`NativeDriver` trait** (`DRIVER_ID`, `handle(ctx, subfn, pc)`) for the
   native-primitive peripherals. Shared `halt_unreachable` /
   `halt_unknown_subfn` helpers so every peripheral gets the context dump
   and "extend file X" hint for free; delete the per-file drifted copies.
   Keep dispatch static (match on a trait-object-free table or explicit
   match arms — no dyn dispatch needed in a fixed peripheral set; pick the
   form that keeps `mmio.rs` readable).
2. **Slim-ISR state ownership (arch-§4):** make the
   `cpu::with_irqs_unmasked` ↔ `irq_from_el2` contract mechanical: collect
   the slim-ISR-touchable state into one module (or wrap it in a marker
   type) such that touching anything else from the slim ISR no longer
   compiles, and document the rule once at the definition instead of in two
   doc comments. Judge the lightest encoding that achieves "compiler
   enforces it" on the actual code; do not over-engineer.

Validation: full guest tests, QEMU boot smoke vs. baseline, FVP build,
`scripts/check-matrix.sh`, pi aggregates build (uart/host_dma are in the
slim-ISR set).

## Phase 12 — Test-coverage closure

One commit (guest-tests + scripts only; src changes limited to what tests
need). Closes the diag-report coverage gaps.

1. **`test_rep_print.S`:** exercise the Hammer Print/Putc/Flush HVC path —
   pin the VaArgs/format-rendering ABI including `%s`, width `%*d`, and the
   Phase-1-fixed `%.*s`.
2. **Snapshot resume test:** automate the manual workflow — run 1 boots and
   saves via `HVC #0x20` at a known guest PC, run 2 must print "Resuming
   guest from snapshot" and then hit a guest-side checkpoint that proves
   execution continued correctly past the resume (registers/memory pattern
   check in the test guest, not just the resume banner). Wire into
   `run-all.sh` as a two-run test case.
3. **host_dma path test:** assess QEMU raspi3b's BCM2835 DMA emulation
   fidelity first. If QEMU emulates the DMA engine well enough to exercise
   `host_dma`'s CB-chain arming (a `pi-bare-metal`-style build under QEMU
   raspi3b), add a minimal test that arms a UART-TX DMA and checks
   completion. If QEMU's emulation genuinely cannot exercise it, document
   that in the test manifest with the specific missing QEMU behavior, and
   add the equivalent check to the `sd-probe`-style hardware probe so the
   path has *a* documented validation route — this is a platform limit, not
   a deferral, and the write-up must make the difference checkable.
4. **diag-M6 closure:** record the ~700 KiB symbols-blob decision in
   `symbols.rs` (deliberate: halt-path backtraces on hardware are worth the
   bytes) — and verify the claim: measure the actual blob size in a
   `pi-bare-metal-input` image and put the measured number in the comment.

Validation: `guest-tests/scripts/run-all.sh` with the new tests green on
QEMU **and** the FVP variant the harness supports; `scripts/check-matrix.sh`.

## Phase 13 — Documentation reconciliation + design notes

Final phase — runs last so it documents the post-stabilization reality. One
commit. No code changes (comment-only edits in source are allowed).

1. **HIGHLEVEL.md:** fix §5.4 (AP flattening / shadow tables exist and are
   load-bearing — describe what `fix_stage1_xn_bits` and `shadow_stub`
   actually do post-Phase-6), §3/§8 "reused" → "ported" + the cxx-core
   decision pointer to IMPLEMENTATION §1.2, drop "Status: draft".
2. **Cargo.toml:** rewrite the `trace`/`trace_once` feature comments to match
   `tracer.rs`'s module doc; fix `build.rs:6`'s claim that trace tables are
   conditional. Present Walter the option of trimming the default `log_*`
   feature set (diag-L6) with a concrete proposal — coordinator asks; agent
   implements the answer.
3. **CLAUDE.md:** snapshot section — all-31-GPRs, ~6 MiB + SCRATCH_POOL
   (post-Phase-3), and `UDF #0xFF0E` (trap-L1; constant unchanged). Update
   the test-running section for `check-matrix.sh`. Remove stale
   INVESTIGATION.md references repo-wide.
4. **Source comment fixes** not already covered by Phases 5/6/8/10: mem-L6's
   list (DABT_SAVE_PA location, load_newton_rom SAFETY trampoline claim,
   snapshot UND-return-stub address — verify each against post-Phase-10
   reality, the trampoline extraction may have mooted some), guest_endian
   module doc → BE-8, periph-L5 (flash_driver `do_write` doc).
5. **arch-#4:** write the snapshot resume contract (CLAUDE.md or `docs/`):
   which peripheral statics are guest-visible-but-not-saved and why
   reset-on-resume is safe for each (VIC int_ctrl/present, tablet queue,
   DMA channels, sound masks…), including the new audio-null completion
   state from Phase 2. Where reset-on-resume is NOT obviously safe, say so
   and file the concern in the doc rather than hand-waving.
6. **arch-§6:** write the package-native-code design note (`docs/`): which
   invariants of `shadow_stub`'s "real code" definition extend to
   runtime-loaded package code above the ROM aperture, what the dynamic
   RW+XN ↔ RO+X rescan path guarantees, and the triage recipe when a wedge
   PC is above the aperture (the bitmap-first doctrine doesn't apply there).
7. **IMPLEMENTATION.md §2.2:** one line noting the crate candidates were not
   adopted (hand-rolled instead).
8. Check `docs/peripherals.md` / `README.md` / `PLAN.md` (top-level) /
   `docs/REAL_HW_BRINGUP.md` against everything Phases 1–12 changed (wedge
   probe removal, audio-null behavior, host_dma relocation, traits,
   check-matrix, new tests) and update. PLAN.md's "Debug-scaffolding
   teardown" section gets rewritten to match the actual post-Phase-6 state.

Validation: builds only (`cargo build` QEMU; doc-comment edits can break
compilation). Proofread pass by coordinator against the landed diffs.
