# Code Review: Newton Hypervisor trap/emulation subsystem

> Review agent report, 2026-06-11, at working copy `somv 8b564c93`.
> Scope: `src/trap.rs`, `unaligned.rs`, `unaligned_inline.rs`, `banked.rs`,
> `hvc_imm.rs`, `guest_bp.rs`, `inline_patch.rs`, `shadow_pool.rs`,
> `trap_hist.rs` (working-copy state, including the WIP "audit silent-default
> guest reads" changes). Line numbers refer to that revision.

## High

### H1. Wedge-probe synthetic sound-DMA IRQ injection still live in the production IRQ path
`src/trap.rs:385-422` (`irq_from_guest`)

The "wedge probe" is explicitly described as testing "the Phase-B hypothesis that the boot wedges after sound init" — but it is compiled in unconditionally (no feature gate). Whenever the sampled guest ELR is identical for 64 consecutive heartbeats and sound DMA IRQs are armed in `int_ctrl` (which is the normal state of a *fully booted* Newton), it calls `vic::inject_sound_dma_irq()` every 32 heartbeats, forever, plus a one-shot `dump_und_history()` + `task_dump::dump()`. On a healthy idle system whose PC happens to park (idle loop, long computation at one trap site), this actively fabricates interrupts the real hardware never raised. This is exactly the kind of Phase-B scaffolding the parent commit ("tear down Phase-B scaffolding") was meant to remove. **Fix:** delete it, or at minimum gate it behind a diagnostic feature (`log_traps`/a new `wedge-probe` feature) and make the injection opt-in.

### H2. `handle_align_fault` silently skips instructions it can't read or decode
`src/unaligned.rs:159-183`

```rust
let insn = read_guest_word(faulting_pc).unwrap_or(0);
...
if faulting_pc & 3 != 0 || decoded_maybe.is_none() {
    ... // log first 40 only
    set_return(ctx, faulting_pc.wrapping_add(4), pre_abt_cpsr);
    return;
}
```
An unreadable faulting PC becomes `insn = 0`, which fails decode, which takes the SKIP path: the load/store is *never performed* and the guest resumes at PC+4 with a stale Rt / unwritten memory. The comment calls it an "early-boot diagnostic", but it is permanent, and after the first 40 events it is completely silent. This is precisely the "silent default on an emulation path" category the current WIP commit is eliminating elsewhere (`handle_und` save slots, `handle_dah_mrs_spsr_patch`, `handle_dabt_dispatch` all now halt loudly). A genuine alignment fault whose instruction we can't emulate is guest-state corruption in flight. **Fix:** halt loudly with the usual context dump (`dump_state`) for both the unreadable-insn and undecodable-insn cases; keep a skip only if there's a documented, reproducible early-boot case that needs it — and then gate and count it.

### H3. Suspected unsound liveness treatment of conditional calls in the scratch-register picker
`src/inline_patch.rs:317-328, 344-358, 715-721`

`analyze_insn` classifies *any* BL (and any SVC/HVC/SMC) as `BranchKind::BLink` regardless of its condition field, and the walker then marks `APCS_CALLER_SAVED & !live` as **written**. For a conditional call (`BLNE foo`, `SVCcc`), the not-taken path preserves R0–R3/R12/R14, so a later read of one of those registers on the not-taken path is a real upward-exposed use — but the analyzer sees it as "already written" and reports the register dead. Per the module's own contract ("false negatives … are correctness bugs and must not happen"), this is a latent wrong-code path: `unaligned_inline` would clobber a live register in its stub. Note the analyzer is otherwise careful about conditions (data-processing/load writes only count when `cond_al`), which makes the BL case look like an oversight rather than a decision. **Fix:** for `cond != AL` calls, don't add the caller-saved clobber to `written` (treat reads-side conservatively as live, same as the conditional-write rule); same for conditional SVC/HVC/SMC. Cheap, strictly conservative.

## Medium

### M1. `guest_bp.rs` is a general facility fused with stale, host-halting investigation probes
`src/guest_bp.rs:376-656`, `src/guest_bp.rs:279`

`handle_user_bp_und` hard-codes six magic PCs from past heap-corruption investigations (NewHeap `0x310e24`, TRefStack `0x1a4948`, SetCurrentHeap `0x142df0`, SearchFreeList `0x313308`, LDRB-post `0x11d844`, PrimGetEnvDomainName exits) — several of which **halt the host** ("diagnostic scaffolding" by their own comments, e.g. lines 597-599, 653-654). A user installing an interactive BP at one of those addresses today gets re-arming, instruction emulation, heap walks, or a halt instead of the documented one-shot stop. `install_locked` also special-cases `ipa != 0x0031_3308` to suppress logging (line 279). The module doc ("One-shot breakpoints. The UND handler restores the original instruction…") no longer matches the code. Additionally, while any of these re-arming probes is installed, `snapshot::maybe_autosave` (via `any_installed()`, snapshot.rs:358) is suppressed for the whole session. **Fix:** delete the six PC-specific arms (they're recoverable from history if the investigation reopens), keep the generic dump-restore-rearm-none path, and re-sync the module doc.

### M2. `trap.rs` is monolithic — and the seams are already visible
`src/trap.rs` (4760 lines)

Concrete split that would carry most of the value:
- **`und.rs`** — `handle_und` + the SWP/FPA/DDK/MRS-SPSR emulators, UND history ring, `return_to_guest_from_und` (~900 lines, self-contained: communicates with the rest via `TrapContext` + the scratch-slot constants).
- **`cp15.rs`** — `handle_cp15_trap` + the existing `mod cp15` + `reseed_flash_checksums_if_needed` (~570 lines).
- **`trap_diag.rs`** — `handle_diag`, `dump_tstacks_and_check_invariants`, `dump_und_history`, the `log_*_budgeted` family, `handle_loud_halt`/BootOS canaries (~900 lines of pure diagnostics).
- **`dabt.rs`** — `handle_data_abort`, `try_emulate_isv0_dabt`, `try_absorb_rom_write`, `drop_flash_write`, `resolve_ipa`.

The dispatcher, IRQ paths, and HVC dispatch can stay. This also makes the "is it scaffolding or load-bearing?" question answerable per-file.

### M3. Six hand-rolled `static mut SEEN` dedup blocks + two Misra-Gries implementations
`src/trap.rs:2243-2276` (`log_fpa_ctrl_reg`), `2283-2312` (`log_fpa_cond_skip`), `3830-3850` (`log_dabt_forward`), `3907-3932` (`log_und_budgeted`), `4043-4058` (`log_debugger_und`), `4148-4168` (CP15 seen-set); plus `TopK` in `trap_hist.rs:122-176` duplicated as `RejTopK` in `unaligned_inline.rs:111-161`.

Each dedup block is the same ~15 lines of `static mut SEEN: [u32; N]; static mut SEEN_N` with an unsafe linear scan, and the two top-K trackers are byte-for-byte the same algorithm at different widths. **Fix:** one `SeenSet<const N: usize>` (`fn first(&mut self, key: u32) -> bool`) and one generic `TopK<const N: usize>` in `trap_hist.rs` or a small `diag_util.rs`; this also concentrates the single-core-`static mut` safety argument in one place instead of six.

### M4. Duplicated ARM-architecture helpers, including a dead "canonical" copy
- Banked register slot mapping: `banked::ctx_slot_for_reg` (`banked.rs:91`, marked `#[allow(dead_code)]`) vs. a second full implementation `unaligned::ctx_slot_for_reg` (`unaligned.rs:303-340`). The canonical module's version is the dead one.
- Condition evaluation: `trap::arm_condition_passed` (`trap.rs:2218`) vs `unaligned::cond_passes` (`unaligned.rs:444`) — identical truth tables.
- Mode names: `trap::aarch32_mode_label` (`trap.rs:1083`), `trap::describe_aarch32_mode` (`trap.rs:3577`), and a third inline match in `handle_instruction_abort` (`trap.rs:1133-1137`).
- Shift evaluation: `trap::arm_shift` (`trap.rs:1057`, RRX approximated *without* carry — admitted in the comment, used for flash-write-drop writeback addresses) vs `unaligned::apply_shift` (`unaligned.rs:469`, carry-correct).
- Stage-1 walk: `trap::guest_translate_va` (`trap.rs:3536`, hardcodes TTBR0=0x04000000) duplicates `guest_mem::translate_va`, which trap.rs *also* calls elsewhere (e.g. line 2052, 3707).

**Fix:** make `banked.rs` the single home for slot mapping (delete the unaligned.rs copy), add `arm_cond_passed`/`arm_shift`/`mode_name` to one decode/util module, delete `trap::guest_translate_va` in favor of `guest_mem::translate_va`. The carry-less RRX in `arm_shift` then disappears for free.

### M5. Mangled, interleaved doc comments from past edits
`src/trap.rs:2330-2366` (handle_loud_halt's doc is fused with `dump_tstacks_and_check_invariants`'s), `2769-2781` (a dangling "Canary handler for `Reboot`…" doc above `handle_bootos_canary`), `2890-2930` (the doc for `handle_unhandled_exception` is *split in half* by `halt_invariant` and `kernel_intent_mask_for`, ending with the orphan line `/// path, true ⇒ kernel/UND path). Halts via halt_invariant.`).

These read as botched block moves. They actively mislead (rustdoc attaches the wrong halves to the wrong items). **Fix:** restitch the three doc comments; while there, decide whether `kernel_intent_mask_for` (a stub that always returns `None`, `trap.rs:2926`) should live in trap.rs at all.

### M6. ~35 lines of copy-pasted run-flush logic in `dump_tstacks_and_check_invariants`
`src/trap.rs:2461-2497` vs `2503-2537`

The TStackInfo run-printing + invariant-check block is duplicated verbatim for the in-loop flush and the trailing flush. Classic off-by-one breeding ground. **Fix:** extract a local `flush_run(last_info, run_first, run_count, …)` closure/fn.

### M7. Stale one-shot investigation tripwires still in the hot paths
- `src/trap.rs:355-373` — "newt" tripwire polls guest PA `0x0402a250` on **every** guest IRQ, referencing a concluded INVESTIGATION.md item.
- `src/unaligned.rs:55-59` and `trap.rs:4244-4251` — disabled tarmac triggers left as `let _ = n; // tarmac::emit_stop() suppressed for iter-85` with iter-numbered comments.
- `src/trap.rs:649-659` — `rex-dabt` per-access logging for ELR ∈ `0x3137dc..0x313960` ("Phase B diagnostic"), unconditional.
- `src/trap.rs:1583-1610` — `handle_und` one-shot "prove handle_und is being reached at all" DIAG block.

Individually cheap; collectively they're the residue the stabilization pass should sweep. The CLAUDE.md convention distinguishes loud trip-wires on *unknown inputs* (keep) from hypothesis probes for *closed investigations* (remove).

## Low

### L1. `BP_UDF_INSN` does not encode the documented immediate
`src/guest_bp.rs:73-75` (also module doc line 38, and baremetal/CLAUDE.md)

Verified with `arm-none-eabi-objdump`: `0xE7FF_F0FE` disassembles as `udf #0xff0e`, not `udf #0xfffe` (that would be `0xE7FF_FFFE`). Functionally harmless — the constant is written and matched as the same word, and 0xFF0E is still far above the tracer's `FN_COUNT` — but the comments and CLAUDE.md assert the wrong immediate, and anyone re-deriving the word from "UDF #0xFFFE" would produce a non-matching instruction. **Fix:** either change the docs to `#0xFF0E` or change the constant to `0xE7FF_FFFE`; if the latter, note it invalidates the documented `bp` workflow only insofar as stale gdb scripts hard-code the old word (they don't appear to).

### L2. ISS register index 31 would panic instead of being treated as WZR
`src/trap.rs:607` (`srt = (iss >> 16) & 0x1F`), `trap.rs:4135` (`rt = (iss >> 5) & 0x1F`)

Both 5-bit fields index `ctx.x[..31]`. For AArch32 traps the architecture reports the mapped AArch64 register (≤30), so this shouldn't fire — but if it ever does, the failure is a Rust panic at EL2 rather than the project-standard context dump. A one-line `if srt == 31` loud-halt (or WZR semantics) would make the trip-wire explicit. (Flagging as a suspicion per the "don't trust memory for encodings" rule — worth a check against `docs/ARM_Reference.txt` ISS.SRT semantics.)

### L3. Stale SAFETY comment on runtime code patching
`src/inline_patch.rs:96-98` — `code_write_word`'s SAFETY says writes are "race-free against the guest before stage2 enable", but `unaligned_inline::try_install_at` calls it at runtime from the alignment-fault handler, long after stage-2 enable. The actual invariant (guest is paused in an EL2 trap on the only core) is fine — the comment justifies the wrong thing.

### L4. Rejected unaligned-PCs re-run full eligibility (incl. CFG liveness walk) on every fault
`src/unaligned_inline.rs:179-287` — a PC that fails `pick_scratches` pays decode + `live_regs_at` (up to 64 blocks × 32 instructions) on *every* subsequent alignment fault, in the path that was identified as the dominant trap source (~3.4 M faults/s). A small "known-rejected PC" cache (even 32 entries) would remove the recurring cost. The existing `REJ_NO_DEAD_PCS` top-K shows the team already measures this.

### L5. `hvc_imm.rs` test-ABI block doc drift
`src/hvc_imm.rs:5` says the guest-test ABI block is "`GuestTestPrintByte..Debugger`", but `GuestInjectPen` (line 75) is also part of the test ABI (`HVC_INJECT_PEN` macro) and sits after `Debugger`. Anyone "appending" a new internal variant by inserting before `GuestInjectPen` would silently renumber a test-ABI immediate. Update the doc to name `GuestInjectPen` as the block terminator.

### L6. `unaligned.rs::set_return` signature carries an unused param with a misleading rationale
`src/unaligned.rs:342-366` — `_pre_abt_cpsr` is kept "for caller clarity" but the function forwards `0` as the SPSR to `return_to_guest_from_und`, whose own `_spsr` *is* read (for the trampoline-target check at `trap.rs:3739`). Passing `0` means the USR-target-in-trampoline diagnostic can never fire for align-fault returns (mode 0 ≠ 0x10). Either pass the real CPSR through or document that the diagnostic is intentionally bypassed on this path.

## What's good (briefly)

- The WIP audit changes are exactly right: `handle_und` save-slot reads, `handle_dah_mrs_spsr_patch`, `handle_dabt_dispatch` L1 read, `try_absorb_rom_write` SWP load, `scan_to_null_word_aligned`, and `guest_bp`'s SetCurrentHeap literal all now halt loudly with context instead of fabricating values — consistent with the project convention.
- `banked.rs` is a model module: the Table D1-79 documentation is the best single artifact in the subsystem.
- `hvc_imm.rs` solves a real class of bug (immediate collisions) with the type system; `HvcImm::insn()` verified correct against the disassembler.
- `unaligned_inline`'s compile-time encoder asserts (`_check_encoders`, verified against objdump) are exactly the right defense for hand-rolled encodings.
- Unsafe hygiene is generally good: `addr_of_mut!` for `static mut`, every asm block carries a SAFETY note, and the single-core EL2 argument is stated (if repetitively) at each site.

## Shape of this subsystem

The trap/emulation core is in solid architectural shape — the dispatch model (EC → handler, mutate `TrapContext`, advance ELR or halt loudly) is consistent, the banked-register handling is correct and unusually well documented, and the loud-halt trip-wire convention is genuinely followed on the emulation paths (and the WIP commit closes most of the remaining silent defaults). What drags it down is **residue**: trap.rs has accreted ~1,500 lines of investigation-specific scaffolding (wedge probe, tripwires, iter-numbered toggles, kernel-structure dumps) interleaved with load-bearing emulation, to the point where the two are hard to tell apart — and one piece of that scaffolding (H1) still actively perturbs a now-working guest. The three refactors with the best payoff: **(1)** sweep the Phase-B scaffolding — H1, M1, M7 — deleting closed-investigation probes and feature-gating anything worth keeping; **(2)** split trap.rs along the natural seams (und / cp15 / dabt / diagnostics), which makes the scaffolding-vs-load-bearing distinction structural; **(3)** consolidate the duplicated ARM helpers and dedup/top-K plumbing into `banked.rs` + one diagnostics util (M3/M4), which both shrinks the file and eliminates the only known divergence between duplicate implementations (the carry-less `arm_shift`). The one item I'd verify before anything else ships is H3 — the conditional-BL liveness assumption — since it's the only finding that can silently produce wrong guest execution through an installed stub.
