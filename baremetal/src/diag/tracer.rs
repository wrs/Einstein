//! Function-level execution trace (every-call, with argument registers).
//!
//! `build.rs` parses `scripts/classify-out/code-symbols.txt` (the curated
//! code-only symbol list the shadow-stub classifier's walker also uses)
//! and emits three blobs into OUT_DIR unconditionally — `crate::diag::symbols`
//! includes them for PC→name lookup in halt-path stack traces, and this
//! module consults them for its trampoline pool when `trace` is enabled:
//!
//!   fn_addrs.bin       packed u32 LE — sorted ROM-range entry PAs
//!   fn_name_offs.bin   packed u32 LE — offsets into the name pool
//!   fn_names.bin       NUL-separated names (name pool)
//!
//! Mechanism: per-function trampoline, not in-place UDF. At ROM load time
//! `init()` allocates a 5-word slot per function inside the ROM backing
//! store (at a high IPA range that's stage-1-identity-mapped by the guest
//! and stage-2-mapped read-only as part of the ROM aperture) and rewrites
//! the function's first word to `B trampoline_slot`. Each slot holds:
//!
//!   slot[0]:  HVC #TRACE_TAG
//!   slot[1]:  original first instruction (rewritten if PC-relative)
//!   slot[2]:  LDR PC, [pc, #0]        — loads target from slot[4]
//!   slot[3]:  literal (for LDR pc-rel) OR B target (for B orig)
//!   slot[4]:  branch-back target = orig_pc + 4
//!
//! Call path:
//!   function call → B to slot[0] → HVC fires → EL2 handler logs the
//!   function name + r0..r3 at the moment of entry → ERET back to slot[1]
//!   → orig insn runs natively in guest mode (preserves banked SP, etc.)
//!   → slot[2] LDR PC loads slot[4] → control reaches orig_pc + 4 and
//!   the function continues as if never patched.
//!
//! Rewrite cases for slot[1]:
//!   - LDR Rd, [pc, #imm]        → LDR Rd, [pc, #0]; slot[3] = literal
//!     value (read from ROM at orig_pc + 8 + imm at install time).
//!   - B <label>                 → LDR PC, [pc, #0]; slot[3] = target
//!     (= orig_pc + 8 + offset). slot[2]/slot[4] unreached.
//!   - anything else             → orig verbatim. Works for PUSH, SUB sp,
//!     MOV ip sp, MOV Rd imm, MRC/MCR p15, etc. — none of these depend
//!     on the original PC.
//!
//! No is_known_function_start heuristic: the address list is authoritative
//! because classify-symbols.py already vetted every entry. If a first word
//! can't be safely rewritten (unsupported PC-relative form), that function
//! is skipped and counted; no silent data corruption.
//!
//! Two log modes:
//!   - default (`--features trace`): every call produces a trace line.
//!   - first-touch (`--features trace_once`): each function's main
//!     trace line is emitted once per session, gated on `FIRED_BITMAP`
//!     in `log_trace_at`. The trampoline itself still fires on every
//!     call so targeted debug side-effects (putc buffering) keep
//!     working.
//!
//! `SVC_WATCH` (in `log_trace_at`) is the documented extension point
//! for a stall hunt: add a function's PC literal to the list, rebuild,
//! and each SVC-mode entry prints its banked SP/LR. It is empty in the
//! shipped tree.
//!
//! Changes the snapshot ROM fingerprint (many ROM words move), so runs
//! with `trace` enabled always cold-boot in practice.

use crate::hv::guest_mem;
use crate::kprintln;
use crate::arch::trap_context::TrapContext;

// Symbol-table backing storage lives in `crate::diag::symbols` (always
// available). Re-export the raw helpers here so the rest of this
// file's code reads as it did before the extract.
use crate::hv::hvc_imm::HvcImm;
use crate::diag::symbols::{FN_COUNT, fn_addr, fn_name};

/// Trampoline pool IPA range. Lives inside the ROM backing (which is
/// 16 MiB stage-2 RO, sections 9..F of the guest's stage-1 L1 identity-
/// map it post-MMU; pre-MMU VA=PA) so it's reachable from all ROM call
/// sites with a ±32 MiB `B` instruction AND doesn't require any extra
/// stage-2 mapping. Well past the REx tail at ~0x0088_E500 and well
/// before the UND-trampoline / ROM-patch injection stubs at 0x00FF_FF00.
pub const TRAMPOLINE_IPA: u32 = 0x0090_0000;
pub const TRAMPOLINE_END: u32 = 0x00E0_0000;

const SLOT_WORDS: usize = 5;
const SLOT_SIZE: u32 = (SLOT_WORDS as u32) * 4;

/// Sequence counter for the trace log. Bumped on every HVC fire.
static TRACE_SEQ: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Per-function "already logged" bitmap, used by the `trace_once`
/// feature to gate the main trace line. One bit per slot index;
/// `fetch_or` makes the test+set race-free even if a future change
/// makes EL2 multi-entrant.
#[cfg(feature = "trace_once")]
const FIRED_BITMAP_WORDS: usize = (FN_COUNT + 31) / 32;
#[cfg(feature = "trace_once")]
static FIRED_BITMAP: [core::sync::atomic::AtomicU32; FIRED_BITMAP_WORDS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; FIRED_BITMAP_WORDS];

/// Test-and-set the fired bit for slot `idx`. Returns `true` if this
/// function has already logged its main trace line in this session.
/// Always returns `false` when the `trace_once` feature is off, so the
/// caller's `if !already_fired(idx)` reduces to an unconditional log.
#[inline]
fn already_fired(_idx: usize) -> bool {
    #[cfg(feature = "trace_once")]
    {
        let word = _idx / 32;
        let bit = 1u32 << (_idx & 31);
        let prev = FIRED_BITMAP[word]
            .fetch_or(bit, core::sync::atomic::Ordering::Relaxed);
        return (prev & bit) != 0;
    }
    #[cfg(not(feature = "trace_once"))]
    {
        false
    }
}

/// True once `init()` has installed the trampolines. Prevents double-install
/// if called from multiple boot paths (e.g. cold boot vs. snapshot resume).
static mut INITIALISED: bool = false;


/// ROM ranges we must not overwrite with a `B trampoline`:
///   - VA 0x00..0x20: ARM vector table. The reset vector at 0x00 runs
///     before we've built trampolines, and vectors at 0x04/0x0C/0x10 are
///     claimed by the hypervisor's UND / DIAG patches.
///   - VA 0x00E0_0000..0x00FF_FF00: in-ROM stub pool used by
///     `unaligned_inline` to fast-path SA-1100 unaligned-LDR rotate
///     emulation (reachable from every ROM call site via a ±32 MiB
///     `B`). Sits between the tracer pool (0x0090_0000..0x00E0_0000)
///     and the ROM-tail trampoline cluster below.
///   - VA 0x00FF_FF00..0x00FF_FFF0: UND trampoline (0xFF00..0xFF60) +
///     DABT diagnostic trampoline (0xFFA8..0xFFE4) + UND return stub
///     (0xFFE4..0xFFF0). See guest_mem::patch_und_vector,
///     rom_patches::*, and the layout notes at UND_RETURN_STUB_OFFSET.
///   - PowerOffAndReboot: rom_patches installs a one-word HVC canary
///     there; the tracer overwriting it would silently mask the trap.
pub fn in_reserved_range(addr: u32) -> bool {
    if addr < 0x0000_0020 { return true; }
    if (crate::newton::shadow_stub::SBA_STUB_POOL_IPA
        ..crate::newton::shadow_stub::SBA_STUB_POOL_END)
        .contains(&addr)
    {
        return true;
    }
    if (0x00FF_FF00..0x00FF_FFF0).contains(&addr) { return true; }
    // FPA-class UND bypass stub at 0x00FF_FEC0..0x00FF_FEE0 (8 words).
    // Routes FPA UNDs straight to the kernel FPE; falls through to the
    // main UND trampoline for non-FPA UNDs.
    if (0x00FF_FEC0..0x00FF_FEE0).contains(&addr) { return true; }
    if addr == crate::newton::rom_patches::POWEROFF_REBOOT_PC { return true; }
    if addr == crate::newton::rom_patches::REBOOT_PC { return true; }
    if addr == crate::newton::rom_patches::BOOTOS_PC { return true; }
    false
}

/// Unconditional branch. `from_pc` is the PC of the `B` instruction,
/// `target` is the destination VA/IPA. Returns `None` if the offset
/// doesn't fit in the 24-bit signed imm (×4 → ±32 MiB).
fn encode_b(from_pc: u32, target: u32) -> Option<u32> {
    let pc_plus_8 = from_pc.wrapping_add(8);
    let offset = (target as i64) - (pc_plus_8 as i64);
    if offset & 3 != 0 { return None; }
    let offset_words = offset >> 2;
    if !(-(1i64 << 23)..(1i64 << 23)).contains(&offset_words) {
        return None;
    }
    let imm24 = (offset_words as u32) & 0x00FF_FFFF;
    Some(0xEA00_0000 | imm24)
}

/// Build slot[1] and slot[3] (the literal / branch-target slot) for a
/// function whose original first instruction is `orig` at PA `orig_pc`.
/// Returns:
///   Some((insn_slot1, literal_slot3))
///     - `insn_slot1`: word to place at slot[1].
///     - `literal_slot3`: word to place at slot[3] (0 if unused).
///   None: the first instruction uses a PC-relative form we can't
///     safely rewrite, or the referenced literal is outside ROM.
fn rewrite_first_insn(orig: u32, orig_pc: u32) -> Option<(u32, u32)> {
    // LDR Rd, [pc, #imm], cond=AL, U=1 (add), B=0, W=0, L=1, Rn=PC.
    // Encoding: 1110 0101 1001 1111 Rd  imm12 = 0xE59F_Rxxx.
    // Matches when bits 27:20 == 0x59 and Rn == 0xF. Rd is extracted
    // from bits 15:12; we allow any Rd (the original tracer was R0-only,
    // but there's no reason to restrict).
    if (orig & 0x0FFF_0000) == 0x059F_0000 {
        let rd = (orig >> 12) & 0xF;
        // PC destination LDR isn't a prologue pattern — if we see it
        // here it's likely a thunk/tail jump; skip (we can't both log
        // AND transfer control safely).
        if rd == 0xF { return None; }
        let imm = orig & 0xFFF;
        let literal_addr = orig_pc.wrapping_add(8).wrapping_add(imm);
        if literal_addr >= 0x0100_0000 { return None; }
        // Read the literal as the Newton-side numerical value the
        // AArch32 LDR would see at run time.
        let literal = crate::hv::guest_endian::guest_read_u32_pa(literal_addr)?;
        // Rewrite to LDR Rd, [pc, #0]. At slot[1] (offset +4 in slot),
        // pc+8 points at slot[3] (offset +12), so imm=0 reads slot[3].
        let new_insn = 0xE59F_0000 | (rd << 12);
        return Some((new_insn, literal));
    }

    // B <label>, cond=AL. Encoding: 1110 1010 imm24 = 0xEAxx_xxxx.
    if (orig & 0x0F00_0000) == 0x0A00_0000 {
        let imm24 = orig & 0x00FF_FFFF;
        let offset = if imm24 & 0x0080_0000 != 0 {
            ((imm24 | 0xFF00_0000) as i32).wrapping_mul(4)
        } else {
            (imm24 as i32).wrapping_mul(4)
        };
        let target = (orig_pc.wrapping_add(8) as i32).wrapping_add(offset) as u32;
        // LDR PC, [pc, #0] reads slot[3] at offset +12 from slot[1].
        let new_insn = 0xE59F_F000;
        return Some((new_insn, target));
    }

    // ADD/SUB Rd, PC, #imm (the ADR idiom, ARMv7 data-processing
    // form). Some Newton accessors are just
    //     sub r0, pc, #K
    //     mov pc, lr
    // — compile-time `ClassInfo()` / static-data getters
    // (TClassInfoRegistryImpl::ClassInfo at 0x38607c is an example).
    // Copying verbatim into the trampoline slot silently returns a
    // pointer into the tracer pool instead of the ROM, and that
    // garbage propagates through the whole downstream chain as a
    // corrupted `this`. Rewrite to a pc-relative literal load so
    // slot[1] puts (orig_pc + 8 ± imm12) into Rd.
    //
    // Data-processing immediate encoding:
    //     cccc 001 opcode S Rn    Rd    imm12
    //     ADD: opcode = 0b0100 → bits 24:21 = 0100
    //     SUB: opcode = 0b0010 → bits 24:21 = 0010
    // cond=AL, I=1, S=0, Rn=PC=0xF gives:
    //     ADD:  0xE28F_Rxxx   (mask 0x0FFF_0000 == 0x028F_0000)
    //     SUB:  0xE24F_Rxxx   (mask 0x0FFF_0000 == 0x024F_0000)
    // imm12 is an 8-bit value rotated right by 2*rot4 (bits 11:8),
    // per the modified-immediate encoding.
    let is_add_pc = (orig & 0x0FFF_0000) == 0x028F_0000;
    let is_sub_pc = (orig & 0x0FFF_0000) == 0x024F_0000;
    if is_add_pc || is_sub_pc {
        let rd = (orig >> 12) & 0xF;
        if rd == 0xF { return None; }  // PC destination — bail.
        let imm8 = orig & 0xFF;
        let rot = ((orig >> 8) & 0xF) * 2;
        let imm = imm8.rotate_right(rot);
        let pc_plus_8 = orig_pc.wrapping_add(8);
        let literal = if is_add_pc {
            pc_plus_8.wrapping_add(imm)
        } else {
            pc_plus_8.wrapping_sub(imm)
        };
        // Rewrite to LDR Rd, [pc, #0]; literal goes in slot[3].
        let new_insn = 0xE59F_0000 | (rd << 12);
        return Some((new_insn, literal));
    }

    // Everything else: copy verbatim. Handles PUSH / STMFD, SUB sp imm,
    // MOV ip sp, MOV Rd imm, MVN Rd imm, MOV Rd Rm, MRC/MCR p15, STR lr
    // [sp,#-4]!, and any other non-PC-relative function entry insn.
    // Remaining PC-relative forms we DON'T handle (MOV Rd, PC; LDR
    // with register offset involving PC; anything that branches via
    // PC) are very rare as function entries — if one shows up, the
    // HVC will log it but subsequent execution will be wrong.
    Some((orig, 0))
}

/// Install trampolines for every function in the embedded table.
/// Called exactly once, at ROM load time (from `guest_mem::load_newton_rom`
/// after all other ROM patches have been applied). Idempotent.
pub fn init() {
    // SAFETY: single-threaded at boot; `INITIALISED` is only consulted/set here.
    unsafe {
        if INITIALISED { return; }
        INITIALISED = true;
    }

    let rom_base = guest_mem::rom_host_pa() as *mut u32;
    let pool_capacity =
        ((TRAMPOLINE_END - TRAMPOLINE_IPA) / SLOT_SIZE) as usize;
    if FN_COUNT > pool_capacity {
        kprintln!(
            "trace: WARNING — {} functions, pool holds only {} slots; truncating",
            FN_COUNT, pool_capacity
        );
    }
    let n = FN_COUNT.min(pool_capacity);

    let mut patched = 0usize;
    let mut skipped_reserved = 0usize;
    let mut skipped_rewrite = 0usize;
    let mut skipped_b_reach = 0usize;

    for i in 0..n {
        let orig_pc = fn_addr(i);
        if in_reserved_range(orig_pc) {
            skipped_reserved += 1;
            continue;
        }
        let word_index = (orig_pc / 4) as usize;
        // SAFETY: build.rs filtered orig_pc to < 0x0100_0000 and word-aligned.
        let orig = unsafe { rom_base.add(word_index).read() };

        let (slot1, slot3) = match rewrite_first_insn(orig, orig_pc) {
            Some(v) => v,
            None => {
                skipped_rewrite += 1;
                continue;
            }
        };

        let slot_ipa = TRAMPOLINE_IPA + (i as u32) * SLOT_SIZE;
        // B from orig_pc to slot[0] must reach. Pool is at 0x00900000+,
        // ROM PCs are in 0..0x00847000ish — worst case ~14 MiB, well
        // inside ±32 MiB.
        let b_insn = match encode_b(orig_pc, slot_ipa) {
            Some(b) => b,
            None => {
                skipped_b_reach += 1;
                continue;
            }
        };

        let slot_word_index = (slot_ipa / 4) as usize;
        // SAFETY: slot_ipa < 0x0100_0000 (bounded by TRAMPOLINE_END),
        // word-aligned by construction; single-threaded boot context.
        //
        // Slots 0/1/2 are instruction words: stored native-LE so the
        // CPU's LE instruction fetch decodes the correct encoding.
        // Slots 3/4 are *data* literals consumed by an AArch32 LDR
        // running under CPSR.E=1 (BE-8); byte-swap them on store so
        // the BE-natural numerical read returns the value we intend.
        // Without the swap, `LDR PC, [pc, #0]` at slot[2] reads the
        // byte-swapped target and the guest jumps to e.g. 0x6c343100
        // instead of 0x0031346c. (slot[3] in the B-rewrite case is
        // also a literal target, hence the same swap.)
        unsafe {
            let slot = rom_base.add(slot_word_index);
            slot.add(0).write(HvcImm::Trace.insn());
            slot.add(1).write(slot1);
            slot.add(2).write(0xE59F_F000); // LDR PC, [pc, #0] → slot[4]
            slot.add(3).write(slot3.swap_bytes());
            slot.add(4).write(orig_pc.wrapping_add(4).swap_bytes());

            rom_base.add(word_index).write(b_insn);
        }
        patched += 1;
    }

    // Publish the stores and flush the entire guest icache so the next
    // fetch from any patched site sees the new `B trampoline`.
    // SAFETY: barrier + IC IALLUIS has no data side effects at EL2.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }

    kprintln!(
        "trace: patched {} / {} entries ({} reserved, {} rewrite-skip, {} B out-of-reach)",
        patched, n, skipped_reserved, skipped_rewrite, skipped_b_reach
    );
}

/// Handle an HVC #TRACE_TAG firing from trampoline slot[0]. Called from
/// `trap::handle_hvc`. Logs the function name plus r0..r3 and returns;
/// ELR_EL2 points at slot[1] and natural ERET resumes the trampoline.
pub fn handle_trace_hvc(ctx: &TrapContext) {
    let elr = read_elr_el2() as u32;
    // HVC sets ELR_EL2 to (hvc_pc + 4) = slot[1]. Slot base is elr - 4.
    let slot_base = elr.wrapping_sub(4);
    let spsr = unsafe {
        let v: u64;
        core::arch::asm!("mrs {}, spsr_el2", out(reg) v,
            options(nomem, nostack, preserves_flags));
        v as u32
    };
    log_trace_at(ctx, slot_base, spsr);
}

/// Log a trace entry for a trampoline hit at `slot_base`, given the
/// pre-HVC CPSR in `spsr`. Shared between the normal HVC path (where
/// slot_base = ELR_EL2 - 4) and the USR-mode UND fallback in
/// `trap::handle_und`: HVC in USR mode is UNDEFINED, so the tracer's
/// slot[0] `hvc #TRACE_TAG` raises an UND instead of entering EL2
/// directly. Callers in either path need the same log line; the caller
/// is responsible for advancing the PC past slot[0] afterwards.
pub fn log_trace_at(ctx: &TrapContext, slot_base: u32, spsr: u32) {
    if slot_base < TRAMPOLINE_IPA || slot_base >= TRAMPOLINE_END {
        kprintln!(
            "trace: slot_base={:#x} outside trampoline pool ({:#x}..{:#x})",
            slot_base, TRAMPOLINE_IPA, TRAMPOLINE_END
        );
        return;
    }
    let slot_offset = slot_base - TRAMPOLINE_IPA;
    if slot_offset % SLOT_SIZE != 0 {
        kprintln!(
            "trace: slot_base={:#x} not aligned (offset={:#x})",
            slot_base, slot_offset
        );
        return;
    }
    let idx = (slot_offset / SLOT_SIZE) as usize;
    if idx >= FN_COUNT {
        kprintln!("trace: slot index {} >= FN_COUNT {}", idx, FN_COUNT);
        return;
    }

    let seq = TRACE_SEQ
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        .wrapping_add(1);
    let mode = spsr & 0x1F;
    let mode_label = match mode {
        0x10 => "usr", 0x11 => "fiq", 0x12 => "irq", 0x13 => "svc",
        0x17 => "abt", 0x1B => "und", 0x1F => "sys", _ => "???",
    };

    // Mode-aware SP / LR. Per ARM ARM Table D1-79 the AArch64 GPR
    // file aliases AArch32 banked R13/R14 by bank name, not by source
    // mode — `ctx.x[13]/[14]` are *always* SP_usr/LR_usr regardless
    // of which mode the function-entry UDF fired in. Use
    // `banked::sp_for_mode/lr_for_mode` so SVC-entry logs see SP_svc
    // (= ctx.x[19]) and LR_svc (= ctx.x[18]), etc.
    let cur_sp = crate::arch::banked::sp_for_mode(ctx, spsr);
    let cur_lr = crate::arch::banked::lr_for_mode(ctx, spsr);

    // putc (0x0034F7E0) is called once per character by the ROM's
    // `_vfprintf` / printf family — most commonly when UnhandledException
    // formats an unhandled-exception message, but any guest printf
    // ultimately lands here. Accumulate the characters and emit a
    // `putc:` line on each newline (or when the buffer fills); suppress
    // the per-call trace line since the buffered view is strictly more
    // useful than 100+ one-char-each trace entries. The caller of putc
    // is shown once per emitted line via the LR captured on first char.
    let fa = fn_addr(idx);
    if fa == 0x0034F7E0 {
        buffer_putc_char(ctx.x[0] as u32, cur_lr, seq);
        return;
    }

    if !already_fired(idx) {
        kprintln!(
            "trace {:5} {:#010x} {} ({}) r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x}",
            seq,
            fn_addr(idx),
            fn_name(idx),
            mode_label,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
            cur_lr,
        );
    }

    // SVC-mode SP-watch hook: on every entry into a listed function
    // running in SVC mode, emit `@<name> SP_svc=… LR_svc=…` so you can
    // track stack high-water and call-chain across an investigation.
    // Watchlist intentionally empty — add `0xPC` literals here during
    // a stall hunt, recompile, and the lines fire on each entry. This
    // is the documented extension point for targeted probing.
    if mode == 0x13 {
        const SVC_WATCH: &[u32] = &[];
        if SVC_WATCH.contains(&fa) {
            kprintln!(
                "  @{} SP_svc={:#010x} LR_svc={:#010x}",
                fn_name(idx), cur_sp, cur_lr,
            );
        }
    }
}

/// Buffer a single character passed to the guest's `putc` (0x0034F7E0).
/// Emitted as a `putc:` line when we see '\n', a non-printable control,
/// or the buffer fills. This turns the UnhandledException / vfprintf /
/// developer printf paths into human-readable output on our UART
/// without re-implementing the ROM's FILE buffering.
///
/// `lr` is recorded on the first char of each line so the emitted line
/// can name its caller (`__vfprintf` for printf-family, plus whatever
/// called that).
fn buffer_putc_char(ch: u32, lr: u32, seq: u32) {
    const CAP: usize = 512;
    static mut BUF: [u8; CAP] = [0u8; CAP];
    static mut LEN: usize = 0;
    static mut FIRST_LR: u32 = 0;
    static mut FIRST_SEQ: u32 = 0;

    let byte = (ch & 0xff) as u8;
    // SAFETY: single-threaded on core 0 under EL2.
    unsafe {
        if LEN == 0 {
            FIRST_LR = lr;
            FIRST_SEQ = seq;
        }
        let newline = byte == b'\n' || byte == b'\r';
        let printable = byte >= 0x20 && byte < 0x7f;
        if !newline && printable && LEN < CAP {
            BUF[LEN] = byte;
            LEN += 1;
        }
        // Flush on newline, on unprintable-control (other than CR/LF),
        // or when the buffer is about to overflow.
        let should_flush = newline
            || (!printable && !newline)
            || LEN == CAP;
        if should_flush && LEN > 0 {
            let s = core::str::from_utf8(&BUF[..LEN]).unwrap_or("<non-utf8>");
            let first_seq = FIRST_SEQ;
            let first_lr = FIRST_LR;
            kprintln!(
                "putc {:5}..{:5} lr={:#010x}: {}",
                first_seq, seq, first_lr, s
            );
            LEN = 0;
        }
    }
}

// (The trampoline-slot[0] instruction encoding is `HvcImm::Trace.insn()`;
//  `trap::handle_und` matches against that directly when a USR-mode HVC
//  raises UND.)

/// Check whether an address lies inside the tracer trampoline pool.
/// Used by `trap::handle_und` to disambiguate a USR-mode UND of
/// HVC #TRACE_TAG from a stray UND that happens to match the same
/// 32-bit encoding elsewhere in the guest.
pub fn in_trampoline_pool(pc: u32) -> bool {
    pc >= TRAMPOLINE_IPA && pc < TRAMPOLINE_END
}

fn read_elr_el2() -> u64 {
    let v: u64;
    // SAFETY: reading a sysreg has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, elr_el2",
            out(reg) v,
            options(nomem, nostack, preserves_flags),
        );
    }
    v
}
