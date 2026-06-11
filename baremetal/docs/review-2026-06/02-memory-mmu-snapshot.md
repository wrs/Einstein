# Code review — guest memory / MMU / snapshot / ROM-patch subsystem

> Review agent report, 2026-06-11, at working copy `somv 8b564c93`.
> Scope: `src/guest_mem.rs`, `src/stage2.rs`, `src/mmu.rs`, `src/guest.rs`,
> `src/guest_endian.rs`, `src/cpu.rs`, `src/snapshot.rs`, `src/rom_patches.rs`
> (all read in full; cross-checked against `src/trap.rs`, `src/shadow_pool.rs`,
> `src/shadow_stub.rs`, `src/main.rs` where needed).

## High

### H1. Snapshot transient-PC gate misses two live trampoline ranges
`src/snapshot.rs:452-460` (`pc_in_hypervisor_transient_region`) covers `0x00900000..0x00E00000` (tracer pool) and `0x00FFFF00..0x01000000` (UND/DABT trampolines), but **not** the DABT fast trampoline at `0x008FFF00` (`guest_mem.rs:1749`, 41 words) or the FPA bypass stub at `0x00FFFEC0` (`guest_mem.rs:1694`) — both sit just *below* the gated ranges. Both stubs stash R0/R1/R12 in TPIDRURW/TPIDRURO, which the snapshot header does not capture. Physical IRQs route to EL2 via HCR.IMO regardless of guest PSTATE.I, so an autosave can land mid-stub; given the DABT fast path handles the dominant fault stream (the iter-59 comment cites 20.8 M DABTs/30 s), a nontrivial fraction of wall time sits inside this stub, making a poisoned slot likely over a long session. Resume then restores garbage R0/R1/R12 via the stub's `mrc ...c13` restores. **Fix:** extend the gate to `0x008FFF00..0x00900000` and `0x00FFFD80..0x01000000` (the latter also covers the patch-stub arena and FPA stub in one range), and/or add `tpidr_el0`/`tpidrro_el0` to the header as defense in depth.

### H2. SCRATCH_POOL is guest-visible state but is not snapshotted
`save_via_semihost` (`src/snapshot.rs:501-522`) writes only RAM + FB. The 384 KiB `shadow_stub::SCRATCH_POOL` is mapped RW into the guest (stage-2 at IPA `0x0600_0000`, stage-1 L1[0x60]) and holds state that outlives a single trap: the DABT trampolines write LR_abt/SP_abt/SPSR_abt to `DABT_SAVE_PA` (`trap::HYP_TRAMP_SCRATCH_BASE + 0xA0`), and the `HvcImm::DahMrsSpsr` patch at kernel PC `0x393144` reads that slot **later**, from ordinary kernel code that the transient-PC gate does not cover. A snapshot taken between DABT entry and DAH's patched MRS resumes with a zeroed pool, so the substituted SPSR is garbage. This also contradicts the documented contract ("Only guest-visible state … survives", CLAUDE.md). **Fix:** append SCRATCH_POOL (and bump `VERSION`) to the snapshot regions — 384 KiB is noise next to the existing 6 MiB — or gate saves while the DABT save slot is "armed". Including it is simpler and closes the whole class.

### H3. `patch_cp15_encodings` scans every ROM word without the code/data bitmap
`src/guest_mem.rs:2385-2413` pattern-matches all 4 M words of the 16 MiB aperture (`load_newton_rom` calls it with `ROM_SIZE / 4`, line 1551) and rewrites any word matching the MCR/MRC c1/c2/c3/c5/c6 shape — without consulting `rom_word_is_code(i)`. Everywhere else the loader is scrupulous about the code/data split; here a *data* word (stored BE, read back byteswapped) that happens to match the ~15-fixed-bit pattern gets rewritten through `write_rom_code_word`, silently corrupting it. The current ROM+REx pair evidently has no false hits, but every Einstein.rex rebuild re-rolls those dice, and a hit would corrupt one data word with no diagnostic. **Fix:** add `if !rom_word_is_code(i) { continue; }` (and consider logging each patched PC once so a future unexpected hit is visible). Same gate would also harden `patch_native_prim_mcr_lr_to_r12` (`guest_mem.rs:2258`), though its exact-word match makes false positives far less likely.

## Medium

### M1. Guest fault sysregs (DFAR/DFSR/IFSR) absent from the snapshot header
`Header` (`src/snapshot.rs:191-258`) and `restore_sysregs` (780-814) carry SCTLR/TTBRx/TCR/DACR/VBAR/CPACR/MAIR + banked SPSRs but not `far_el1`, `esr_el1`, `ifsr32_el2` — the AArch64 homes of AArch32 DFAR/DFSR/IFSR. The DABT fast trampoline forwards aborts to kernel DAH, which reads DFSR/DFAR natively several instructions later; an autosave IRQ landing in that window (same probability argument as H1) resumes with cold-boot fault registers and DAH misdispatches. Three more u64s in the header are cheap; add them with the H2 version bump.

### M2. `read_sysreg64` / `write_sysreg64` have silent default arms
`src/snapshot.rs:852` (`_ => 0`) and `:885` (`_ => {}`). A typo'd register name in a future header field would silently read 0 / silently drop the restore — exactly the "silent default" the project convention forbids. Make the fallback `kprintln! + cpu::halt()`, or replace the stringly-typed dispatch with direct `sr_reader!`/`sr_writer!` invocations at the call sites (the macros already exist; the string match adds nothing but the failure mode).

### M3. Inconsistent install-time verification across ROM patches
Three different policies coexist in `src/rom_patches.rs`: (a) `patch_probe` (1189-1218) and the LDR-byteswap installers verify the original word but on mismatch print "ERROR — skipping" and **continue** — for load-bearing patches (SWIBoot byteswap, `DahMrsSpsr`, the PHammerOutTranslator bodies) a skip guarantees a baffling downstream wedge, which is precisely what the halt-loudly convention exists to prevent; (b) `apply_bootos_trap` and the lock/unlock wrappers verify and bail; (c) `apply_loud_halt_traps` (1464-1482), `apply_debug_patches` (1317-1344), `apply_real_clock_seconds_patch` (1365-1398), and the FTime/FDate installers via `write_stub_and_patch` (1718-1744) **don't verify at all** — they blind-overwrite. Pick one policy: verify everywhere, halt on mismatch (with an explicit opt-out for genuinely optional probes).

### M4. `record_original` coverage doesn't match its stated purpose
The side table (`src/rom_patches.rs:1220-1293`) exists so shadow_stub's liveness analyser sees pre-patch instructions at patched PCs. But only `patch_probe`, the pouttranslator bodies, and the LDR-byteswap sites record; the loud-halt HVCs, BootOS HVC, DebugStr/Debugger branch rewrites, RealClockSeconds body, FTime/FDate branch sites, and all `PATCHES_717006` code rewrites do not. If any of those PCs falls inside a region the analyser walks, it sees the HVC/branch and mis-classifies scratch registers — the exact failure mode the table's own comment warns about. Route every code-word overwrite through one helper that records unconditionally (the table has 128-entry headroom and halts on overflow already).

### M5. Phase-B scaffolding still live in the hot MMU path and as orphaned modules
Despite commit a91c79c8 ("tear down Phase-B scaffolding"): (a) `fix_stage1_xn_bits` (`guest_mem.rs:460-850`) still carries ~250 lines of dated verification scaffolding — the subpage-AP heterogeneity audit, the PA→VA alias logger with its `LOGGED_ALIAS_BITMAP`, the INTENT classification, and the "PROBE 2026-04-26" L1[0xCD] tracker — all executed on **every M-toggle** (per the comment, every task switch), including a 4 KiB stack array per walk; (b) the INTENT branch is dead in practice because `trap::kernel_intent_mask_for` (trap.rs:2922-2929) is now a stub returning `None`; (c) `shadow_pool` has full plumbing (stage-2 L3 entries in `stage2.rs:490-511`, a region in `host_addr_for`, a smoke test in `main.rs:145`) but `shadow_pool::allocate` has **zero callers** — the redirect policy was removed. Extract the audits behind `log_mmu`-style cfg or delete them, and either delete `shadow_pool` or note explicitly that it's parked (its dead 64 KiB also muddies the H2 snapshot-coverage question).

### M6. I-cache publication of patched ROM is asymmetric and relies on incidental eviction
`patch_und_vector` ends with explicit `icache_publish_range` (DC CVAU per line) for the vector words and FPA/UND/DABT trampolines (`guest_mem.rs:2217-2238`) — added, per its own comment, because stale fetches were *observed* without it. But everything patched after that point — `patch_dabt_vector`'s VA-0x10 word, the DABT fast trampoline at `0x008FFF00`, the entire `rom_patches` HVC/stub set, and all `patch_cp15_encodings` rewrites — gets only the `dsb ish; ic iallu` in `eret_to_guest` (`guest.rs:210-216`), which invalidates the I-cache but does not clean dirty D-cache lines to PoU. It works today, plausibly because the 16 MiB load loop evicts most lines naturally, but that is luck, not architecture, and the project already paid for this lesson once on FVP. **Fix:** one `icache_publish_range` sweep over the ROM backing (or over a recorded list of patched ranges) at the end of `load_newton_rom`, then the per-range publishes in `patch_und_vector` become redundant and can go.

### M7. Loud-halt canaries are baked into the production build
`apply_loud_halt_traps` (`rom_patches.rs:1464-1482`) unconditionally replaces `StopImage`, `Reboot`, `PowerOffAndReboot`, and the busError `bl Throw` with halt-on-entry HVCs. For a system that now "boots to Welcome UI and the builtin apps work" — including real hardware — these mean the first idle/sleep entry or user-initiated soft reset halts the hypervisor. These are Phase-B debugging tripwires, not product behavior; for stabilization they should be feature-gated (default off on `no-semihost`/real-hardware builds at minimum) or replaced with log-and-continue where the path is now understood.

## Low

### L1. Alias-audit diagnostics decode byteswapped L2 entries
`guest_mem.rs:639-640` re-reads the two aliasing L2 descriptors with `read_word_pa` (raw LE read) instead of the `read_pt_entry` BE-8 path used everywhere else in the walk, then feeds them to `decode_subpage_ap` and the CONFLICT/DISJOINT classifier. In production (BE-8) builds the printed descriptors, PAs, and AP decodes are byteswapped garbage — anyone debugging from this log would be misled. (Moot if M5 deletes the audit; otherwise switch to the PT-aware reader.)

### L2. TTBR0 = 0x0400_0000 is hardcoded with no runtime assertion
`translate_va` (`guest_mem.rs:378-416`), `fix_stage1_xn_bits`, `dump_stage1_walk`, and the L1-table dumpers all assume the kernel L1 lives at the start of guest RAM "per the 717006 probe" rather than reading `TTBR0_EL1`. If a ROM revision or boot path ever programs a different root, every EL2-side walk silently reads the wrong table. One check in the CP15 TTBR0-write shim (`halt if value != 0x04000000`) — or simply masking the live TTBR0_EL1 — converts the assumption into an enforced invariant.

### L3. Snapshot header writes a stack-garbage padding hole to disk
`Header` is `repr(C)`; between `dacr32_el2: u32` and `vbar_el1: u64` there is a 4-byte alignment hole that `save_via_semihost` (`snapshot.rs:489-496`) serializes via `from_raw_parts(&header …)` — an uninitialized-memory read (UB-by-the-book) and nondeterministic file bytes. Add an explicit `_pad: u32` field (as already done for `_pad0`/`_pad1`) or zero the struct before field assignment.

### L4. Cold-boot EL1 init leaves VBAR_EL1 (and DACR32_EL2) at firmware values
`zero_el1_guest_state` (`guest.rs:151-180`) clears SCTLR/TCR/TTBRx but not `vbar_el1`. The Newton guest needs legacy vectors at VA 0; a firmware that leaves junk in VBAR_EL1 would send the first guest exception into the weeds. Snapshot resume restores it; cold boot should zero it explicitly rather than trusting reset state.

### L5. `pa_is_rom_code` special-cases two runtime-code regions but not the rest
`guest_endian.rs:54-82` short-circuits the tracer pool and patch-stub arena, but the UND/DABT trampolines, FPA bypass stub, DABT fast trampoline, and UND-return stub (all runtime-written LE instruction words in bitmap-"data" territory) are not covered. No current caller decodes those PCs through `guest_read_u32_pa`, but the asymmetry is a trap for the next EL2 decode path. A single `is_hypervisor_code_region(pa)` predicate shared with `snapshot::pc_in_hypervisor_transient_region` (H1) would keep both lists from drifting.

### L6. Stale comments that actively mislead
- `guest_mem.rs:1751-1755`: says `DABT_SAVE_PA` was relocated to "IPA=0x0600_F0A0 (last 4 KiB of the SCRATCH_POOL)" — it is `SCRATCH_POOL_IPA + 0xA0` = `0x0600_00A0`, the *first* page (`trap.rs:1415`).
- `guest_mem.rs:1527-1530`: `load_newton_rom`'s SAFETY comment still claims the UND trampoline is "36 bytes starting at offset 0x80"; it moved to `0x00FFFF00` long ago.
- `snapshot.rs:447`: lists the UND return stub at `0x00FFFFE0`; it is `0x00FFFFE4` (`guest_mem.rs:2073`).
- `shadow_pool.rs:13`: doc says mapped at "IPA 0x0601_0000..0x0602_0000"; the constant is `0x0606_0000`.
- `guest_endian.rs:1-29`: module doc still describes the pre-migration Phase-1 identity/XOR-3 contract; the body implements post-Phase-2c BE-8.
- CLAUDE.md still says "x0..x14 of the currently-active mode" and "~14 MiB (RAM + FB + flash)"; v3+ saves all 31 GPRs and v6 dropped flash (~6 MiB).

### L7. Dead-code retention idioms
`rom_patches.rs:856-867` keeps `apply_new_stack_pad_wrapper` and `apply_lock_heap_range_wrapper` (~125 lines of known-broken, never-installed machinery) alive via `let _ = fn;` fake-uses; git history preserves them, so deleting (keeping the two-line "why not" comment) is cleaner — failing that, `#[allow(dead_code)]` states the intent honestly. `guest_endian.rs:35`'s blanket `#![allow(dead_code)]` dates to the migration and should be narrowed now that Phase 2 is done. `stage2.rs:558` (`tick_page::update` "back-compat shim") still has callers including `install_tick_page` itself — either migrate them or drop the deprecation note.

## Shape of this subsystem

The core architecture is genuinely solid: stage-2 table construction, the BE-8 code/data discipline (classifier bitmap → `write_rom_code_word`/`write_rom_data_word` → `read_pt_entry`), the Table D1-79-based GPR snapshot model, and the patch-stub arena are all carefully reasoned and unusually well documented. The weaknesses are accretion artifacts: guest_mem.rs is three modules in a trench coat (memory access layer + stage-1 walker, in-guest trampoline assembler, Phase-B audit scaffolding), rom_patches.rs has three generations of install conventions coexisting, and the snapshot's notion of "guest-visible state" has drifted from what stage-2 actually exposes to the guest.

The three refactors with the best payoff:

1. **Single region manifest.** One table of `(name, ipa, size, host_pa, perms, snapshot: yes/no)` consumed by `stage2::init`, `guest_mem::host_addr_for`, and `snapshot::{save,load}`. Today those three places each hand-maintain the region list, which is exactly how SCRATCH_POOL ended up guest-visible but unsnapshotted (H2) and how dead shadow_pool wiring lingers (M5).
2. **One patch installer.** Collapse `patch_probe`, `write_stub_and_patch`, `write_stub_words`, the blind-overwrite installers, and the four scattered branch encoders (`arm_b`/`arm_bl`/`arm_b_cond` in rom_patches plus the `beq`/`b_far`/`ldr_r0_lit` closures in guest_mem) into a small `aarch32_emit` + `install_patch{expected_orig, words, record}` API that verifies, records originals, publishes I-cache, and halts on mismatch — fixing M3, M4, and M6 structurally instead of site by site.
3. **Extract the trampoline assembler and audits from guest_mem.** Move `patch_und_vector`/`patch_dabt_vector`/`install_dabt_fast_trampoline` (~700 lines of hand-assembled AArch32 with their offset constants) into a `guest_trampolines.rs` that owns the H1/L5 address-range predicate, and strip `fix_stage1_xn_bits` back to the ~80 lines of normalization it actually needs on the task-switch hot path.
