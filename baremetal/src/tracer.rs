//! Function-level execution trace (every-call, with argument registers).
//!
//! When the `trace` cargo feature is on, `build.rs` parses
//! `scripts/classify-out/code-symbols.txt` (the curated code-only symbol
//! list the shadow-stub classifier's walker also uses) and emits three
//! blobs into OUT_DIR which we include here:
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
//! No first-touch disabling: every call fires the HVC. The trampoline
//! never removes itself, so a second invocation of the same function
//! produces a second trace line.
//!
//! Changes the snapshot ROM fingerprint (many ROM words move), so runs
//! with `trace` enabled always cold-boot in practice.

use crate::cpu;
use crate::guest_mem;
use crate::kprintln;
use crate::trap::TrapContext;

const FN_ADDRS_RAW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_addrs.bin"));
const FN_NAME_OFFS_RAW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_name_offs.bin"));
const NAME_POOL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_names.bin"));

const FN_COUNT: usize = FN_ADDRS_RAW.len() / 4;

/// HVC immediate used by trampoline slot[0]. Routed from `trap::handle_hvc`
/// to `handle_trace_hvc`. 0x50 chosen to not collide with existing
/// guest-test HVC IDs (0x01..0x05, 0x20, 0x30, 0x40/0x41) or UND/DIAG tags.
pub const TRACE_TAG: u32 = 0x50;

/// Trampoline pool IPA range. Lives inside the ROM backing (which is
/// 16 MiB stage-2 RO, sections 9..F of the guest's stage-1 L1 identity-
/// map it post-MMU; pre-MMU VA=PA) so it's reachable from all ROM call
/// sites with a ±32 MiB `B` instruction AND doesn't require any extra
/// stage-2 mapping. Well past the REx tail at ~0x0088_E500 and well
/// before the UND-trampoline / ROM-patch injection stubs at 0x00FF_FF00.
const TRAMPOLINE_IPA: u32 = 0x0090_0000;
const TRAMPOLINE_END: u32 = 0x00E0_0000;

const SLOT_WORDS: usize = 5;
const SLOT_SIZE: u32 = (SLOT_WORDS as u32) * 4;

/// Sequence counter for the trace log. Bumped on every HVC fire.
static mut TRACE_SEQ: u32 = 0;

/// True once `init()` has installed the trampolines. Prevents double-install
/// if called from multiple boot paths (e.g. cold boot vs. snapshot resume).
static mut INITIALISED: bool = false;

fn read_u32_le(slice: &[u8], i: usize) -> u32 {
    let o = i * 4;
    u32::from_le_bytes([slice[o], slice[o + 1], slice[o + 2], slice[o + 3]])
}

fn fn_addr(i: usize) -> u32 { read_u32_le(FN_ADDRS_RAW, i) }
fn fn_name_off(i: usize) -> usize { read_u32_le(FN_NAME_OFFS_RAW, i) as usize }

fn fn_name(i: usize) -> &'static str {
    let start = fn_name_off(i);
    let mut end = start;
    while end < NAME_POOL.len() && NAME_POOL[end] != 0 {
        end += 1;
    }
    core::str::from_utf8(&NAME_POOL[start..end]).unwrap_or("<non-utf8>")
}

/// ROM ranges we must not overwrite with a `B trampoline`:
///   - VA 0x00..0x20: ARM vector table. The reset vector at 0x00 runs
///     before we've built trampolines, and vectors at 0x04/0x0C/0x10 are
///     claimed by the hypervisor's UND / DIAG patches.
///   - VA 0x00FF_FF00..0x00FF_FF74: UND trampoline + ROM-patch injection
///     stubs (see guest_mem::patch_und_vector and rom_patches::*).
///   - PowerOffAndReboot: rom_patches installs a one-word HVC canary
///     there; the tracer overwriting it would silently mask the trap.
pub fn in_reserved_range(addr: u32) -> bool {
    if addr < 0x0000_0020 { return true; }
    if (0x00FF_FF00..0x00FF_FF80).contains(&addr) { return true; }
    if addr == crate::rom_patches::POWEROFF_REBOOT_PC { return true; }
    false
}

/// HVC A1 encoding: `cond 0001 0100 imm12 0111 imm4`, cond=0xE (AL).
/// The 16-bit HVC immediate is split across bits 19:8 (hi 12) and 3:0.
fn encode_hvc(imm16: u16) -> u32 {
    let hi12 = (imm16 as u32) >> 4;
    let lo4 = (imm16 as u32) & 0xF;
    0xE140_0070 | (hi12 << 8) | lo4
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
///   Some((insn_slot1, literal_slot3, sets_pc))
///     - `insn_slot1`: word to place at slot[1].
///     - `literal_slot3`: word to place at slot[3] (0 if unused).
///     - `sets_pc == true`: slot[1] itself transfers control (B rewrite
///       via LDR PC). Slot[2]/slot[4] are unreached in that case.
///   None: the first instruction uses a PC-relative form we can't
///     safely rewrite, or the referenced literal is outside ROM.
fn rewrite_first_insn(orig: u32, orig_pc: u32) -> Option<(u32, u32, bool)> {
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
        // guest_mem::read_word_pa reads the post-swap LE view of ROM,
        // which is exactly what an AArch32 LDR sees at run time.
        let literal = guest_mem::read_word_pa(literal_addr)?;
        // Rewrite to LDR Rd, [pc, #0]. At slot[1] (offset +4 in slot),
        // pc+8 points at slot[3] (offset +12), so imm=0 reads slot[3].
        let new_insn = 0xE59F_0000 | (rd << 12);
        return Some((new_insn, literal, false));
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
        return Some((new_insn, target, true));
    }

    // Everything else: copy verbatim. Handles PUSH / STMFD, SUB sp imm,
    // MOV ip sp, MOV Rd imm, MVN Rd imm, MOV Rd Rm, MRC/MCR p15, STR lr
    // [sp,#-4]!, and any other non-PC-relative function entry insn.
    // Instructions that DO read PC but aren't matched above (e.g. a raw
    // `ADR Rd, #imm` encoded as ADD Rd, pc, #imm) will execute with the
    // trampoline's PC — their semantics shift. These are vanishingly
    // rare as function entries in the Newton kernel; if one shows up,
    // the HVC will log it but subsequent execution will be wrong.
    Some((orig, 0, false))
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

        let (slot1, slot3, sets_pc) = match rewrite_first_insn(orig, orig_pc) {
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
        unsafe {
            let slot = rom_base.add(slot_word_index);
            slot.add(0).write(encode_hvc(TRACE_TAG as u16));
            slot.add(1).write(slot1);
            // slot[2] and slot[4] are only reached when slot[1] doesn't
            // itself transfer control. For the B-rewrite case we still
            // fill them (defensive — unreached but consistent layout).
            slot.add(2).write(0xE59F_F000); // LDR PC, [pc, #0] → slot[4]
            slot.add(3).write(slot3);
            slot.add(4).write(orig_pc.wrapping_add(4));
            let _ = sets_pc;

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

    let seq = unsafe {
        TRACE_SEQ = TRACE_SEQ.wrapping_add(1);
        TRACE_SEQ
    };
    let mode = spsr & 0x1F;
    let mode_label = match mode {
        0x10 => "usr", 0x11 => "fiq", 0x12 => "irq", 0x13 => "svc",
        0x17 => "abt", 0x1B => "und", 0x1F => "sys", _ => "???",
    };

    kprintln!(
        "trace {:5} {:#010x} {} ({}) r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x}",
        seq,
        fn_addr(idx),
        fn_name(idx),
        mode_label,
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ctx.x[14] as u32,
    );

    // Targeted one-shot-style dump: for the specific USR-mode call sites
    // we're investigating (KSRVTask spawn path — TUTask::Init + FMNewStack
    // caller in UserBoot), also dump the guest's stack-top so we can see
    // args 5..7 (env_id, priority, name) that are passed via the stack.
    // ctx.x[13] carries SP_usr in the AArch32→AArch64-on-HVC path; reading
    // it and translating through stage-1 gives us the caller's stack frame.
    // Gated on mode=USR and the function index so we don't spam the log.
    if mode == 0x10 {
        let fa = fn_addr(idx);
        if fa == 0x0025BC14 || fa == 0x0025BBD4 || fa == 0x001F8EAC {
            dump_guest_stack(ctx.x[13] as u32, 8);
        }
    }
    let _ = cpu::halt; // suppress unused-import warning under some cfgs
}

fn dump_guest_stack(sp: u32, words: usize) {
    if sp == 0 {
        kprintln!("  stack dump: sp=0 (banked SP not plumbed)");
        return;
    }
    // Gather into a small fixed-size array so we can print as one line.
    let mut buf = [0u32; 8];
    let n = words.min(8);
    let mut got_any = false;
    for i in 0..n {
        match guest_mem::read_word_va(sp.wrapping_add((i as u32) * 4)) {
            Some(v) => { buf[i] = v; got_any = true; }
            None => { buf[i] = 0xDEAD_BEEF; }
        }
    }
    if !got_any {
        kprintln!("  stack dump @sp={:#010x}: all translations failed", sp);
        return;
    }
    kprintln!(
        "  stack @sp={:#010x}: {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x}",
        sp, buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]
    );
}

/// The HVC #TRACE_TAG instruction as it appears in slot[0] of a tracer
/// trampoline. In USR mode HVC is UNDEFINED, so `trap::handle_und`
/// looks for exactly this encoding and treats it as an out-of-band
/// trace dispatch.
pub const TRACE_HVC_INSN: u32 = 0xE140_0570;

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
