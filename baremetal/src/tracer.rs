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
///   - VA 0x00FF_FF00..0x00FF_FFF0: UND trampoline (0xFF00..0xFF80) +
///     SBA post-emulation trampoline (0xFF80..0xFFA8) + DABT
///     diagnostic trampoline (0xFFA8..0xFFE4, ends at the literal word
///     at `db+14`) + UND return stub (0xFFE4..0xFFF0). See
///     guest_mem::patch_und_vector, rom_patches::*, and the layout
///     notes at UND_RETURN_STUB_OFFSET.
///   - PowerOffAndReboot: rom_patches installs a one-word HVC canary
///     there; the tracer overwriting it would silently mask the trap.
pub fn in_reserved_range(addr: u32) -> bool {
    if addr < 0x0000_0020 { return true; }
    if (0x00FF_FF00..0x00FF_FFF0).contains(&addr) { return true; }
    if addr == crate::rom_patches::POWEROFF_REBOOT_PC { return true; }
    if addr == crate::rom_patches::REBOOT_PC { return true; }
    if addr == crate::rom_patches::BOOTOS_PC { return true; }
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
        return Some((new_insn, literal, false));
    }

    // Everything else: copy verbatim. Handles PUSH / STMFD, SUB sp imm,
    // MOV ip sp, MOV Rd imm, MVN Rd imm, MOV Rd Rm, MRC/MCR p15, STR lr
    // [sp,#-4]!, and any other non-PC-relative function entry insn.
    // Remaining PC-relative forms we DON'T handle (MOV Rd, PC; LDR
    // with register offset involving PC; anything that branches via
    // PC) are very rare as function entries — if one shows up, the
    // HVC will log it but subsequent execution will be wrong.
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
        buffer_putc_char(ctx.x[0] as u32, ctx.x[14] as u32, seq);
        return;
    }

    kprintln!(
        "trace {:5} {:#010x} {} ({}) r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x}",
        seq,
        fn_addr(idx),
        fn_name(idx),
        mode_label,
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ctx.x[14] as u32,
    );

    // Targeted one-shot-style dumps. Gated on function index to avoid log
    // spam.
    if mode == 0x10 {
        if fa == 0x0025BC14 || fa == 0x0025BBD4 || fa == 0x001F8EAC {
            // KSRVTask spawn path — dump guest stack so we see env_id /
            // name / priority stack args passed to TUTask::Init /
            // FMNewStack.
            dump_guest_stack(ctx.x[13] as u32, 8);
        }
    }

    // SVC-mode SP tracking: the Phase-B stall is a stage-1 DABT at
    // DFAR=0x0c001000 with pre-abort mode = SVC. That's the guard
    // page right below the SVC stack at 0x0c002000. Log SP at each
    // SVC-mode function entry in the relevant call chain so we can
    // see exactly where SP_svc crosses the boundary.
    if mode == 0x13 {
        match fa {
            0x003AD698  // SWIBoot
            | 0x001DFDE8 // LowLevelCopyDoneFromKernelGlue
            | 0x001E0754 // ConvertMemOrMsgIdToObj
            | 0x00191E80 // LocalToGlobalId
            | 0x00191F14 // ConvertIdToObj
            | 0x00319F14 // TObjectTable::Get
            | 0x0009C9B0 // TDoubleQContainer::Add
            | 0x0009C9AC // TDoubleQContainer::CheckBeforeAdd
            | 0x0009C7C4 // TDoubleQContainer::RemoveFromQueue
            | 0x001DFA70 // SMemCopyToKernelGlue
                => {
                kprintln!(
                    "  @{} SP_svc={:#010x} LR={:#010x}",
                    fn_name(idx), ctx.x[13] as u32, ctx.x[14] as u32
                );
            }
            _ => {}
        }
    }

    // USR-mode SMemCopyToSharedSWI entry: the Phase-B stall is here.
    // Dump SP, LR, and the stage-1 mappings around SP so we can see
    // which USR stack pages are actually backed. One-shot to avoid
    // flooding if we ever get past the stall and re-enter.
    if fa == 0x003AE3DC && mode == 0x10 {
        static DONE: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !DONE.swap(true, core::sync::atomic::Ordering::Relaxed) {
            let sp = ctx.x[13] as u32;
            let lr = ctx.x[14] as u32;
            kprintln!(
                "  @SMemCopyToSharedSWI entry: SP={:#010x} LR={:#010x}",
                sp, lr
            );
            // Walk the USR task's TT for every 4-KiB slot in
            // 0x0c000000..0x0c010000 so we see the actual stack layout.
            let ttbr: u64;
            unsafe {
                core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr,
                    options(nomem, nostack, preserves_flags));
            }
            let l1_base = (ttbr & 0xFFFF_C000) as u32;
            let l1_c0 = guest_mem::read_word_pa(l1_base + 0xC0 * 4).unwrap_or(0);
            kprintln!(
                "  L1[0xC0] @ {:#010x} = {:#010x}",
                l1_base + 0xC0 * 4, l1_c0
            );
            if (l1_c0 & 3) == 1 {
                let l2_pa = l1_c0 & 0xFFFF_FC00;
                kprintln!("  L2 for 0x0c000000..0x0c010000 (coarse table @ PA {:#010x}):", l2_pa);
                for i in 0..16u32 {
                    let e = guest_mem::read_word_pa(l2_pa + i * 4).unwrap_or(0);
                    let va = 0x0c000000u32 + i * 0x1000;
                    let kind = match e & 3 {
                        0 => "fault",
                        1 => "large",
                        2 | 3 => "small",
                        _ => unreachable!(),
                    };
                    let pa = if (e & 3) == 1 { e & 0xFFFF_0000 }
                             else if (e & 3) != 0 { e & 0xFFFF_F000 }
                             else { 0 };
                    kprintln!(
                        "    L2[{:#04x}] VA={:#010x} raw={:#010x} ({}) -> PA {:#010x}",
                        i, va, e, kind, pa
                    );
                }
            }
        }
    }
    if fa == 0x0011D254 {
        // PrimGetEnvDomainName (kernel-side). We want to observe both
        // the env-config table source AND the byte-level state of the
        // fKernelParams buffer the kernel will read/write. One-shot.
        static DONE: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !DONE.swap(true, core::sync::atomic::Ordering::Relaxed) {
            dump_env_config_table();
            dump_param_buffer(ctx.x[2] as u32, ctx.x[3] as u32);
        }
    }
    if fa == 0x0011D7B8 {
        // USR-side MemObjManager::GetEnvDomainName entry. One-shot.
        static DONE: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !DONE.swap(true, core::sync::atomic::Ordering::Relaxed) {
            let gcg = guest_mem::read_word_va(0x0c10105c).unwrap_or(0);
            let r8 = gcg.wrapping_sub(0x54);
            kprintln!("  USR GetEnvDomainName entry: gCurrentGlobals={:#010x} r8={:#010x}",
                      gcg, r8);
        }
    }
    if fa == 0x0011D544 {
        // First RegisterEnvironmentId — this runs right after USR
        // GetEnvDomainName wrapper returns. Dump the fParams buffer
        // via the LIVE TTBR0 (not the hardcoded 0x04000000 in
        // guest_mem::translate_va) so we see whatever the current task
        // actually has mapped at VA 0x0c111d0c.
        static DONE: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !DONE.swap(true, core::sync::atomic::Ordering::Relaxed) {
            let ttbr: u64;
            let sctlr: u64;
            unsafe {
                core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr,
                    options(nomem, nostack, preserves_flags));
                core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
                    options(nomem, nostack, preserves_flags));
            }
            let gcg = guest_mem::read_word_va(0x0c10105c).unwrap_or(0);
            kprintln!("  @RegisterEnvironmentId: TTBR0_EL1={:#x} SCTLR.M={} gCurrentGlobals={:#010x}",
                      ttbr, sctlr & 1, gcg);
            let fparams = 0x0c111d0cu32;
            for off in [0x00u32, 0x04, 0x08, 0x0C, 0x10] {
                let addr = fparams.wrapping_add(off);
                let word = guest_mem::read_word_va(addr).unwrap_or(0xDEADBEEF);
                kprintln!("    [{:#010x}] = {:#010x}", addr, word);
            }
        }
    }
    let _ = cpu::halt; // suppress unused-import warning under some cfgs
}

/// Dump the env-config table source that the kernel's PrimGetEnvDomainName
/// reads. The kernel's InitCGlobals / PostCGlobalsHWInit selects either the
/// "small" table (0x0c1011bc, ≤1 MiB RAM) or the "large" table (0x0c1012ac,
/// >1 MiB RAM) based on GetRamSize; the selected ROM-resident table pointer
/// lands in *(0x0c1011b8). BuildMemObjDatabase copies entries out of that
/// table into the runtime memobj database. Dumping both the selector value
/// and the first few entries helps localize an init-time divergence.
/// Check whether the shadow_stub patched the byte-access instructions
/// that matter for the GetEnvDomainName loop. A patched site has its
/// word replaced with a branch (B / Bcond). The top byte's low nibble is
/// 0xA for a branch; the high nibble preserves the original condition.
fn check_byte_access_patches() {
    let sites: [(u32, u32, &str); 4] = [
        (0x0011D2BC, 0x05c30000, "PrimGetEnvDomainName: strbeq r0, [r3]"),
        (0x0011D300, 0xe5c36000, "PrimGetEnvDomainName: strb r6, [r3]"),
        (0x0011D304, 0xe5c56000, "PrimGetEnvDomainName: strb r6, [r5]"),
        (0x0011D840, 0xe5d8100d, "MemObjManager::GetEnvDomainName: ldrb r1, [r8, #13]"),
    ];
    for (pa, orig_insn, label) in sites {
        let live = guest_mem::read_word_va(pa).unwrap_or(0xDEAD_BEEF);
        // Patched sites are a branch — bits [27:25] = 0b101 → (w >> 25) & 7 == 5.
        let is_branch = ((live >> 25) & 0x7) == 0x5;
        kprintln!(
            "  byte_access_check: {:#010x}={:#010x} (orig={:#010x}, is_branch={}) {}",
            pa, live, orig_insn, is_branch, label
        );
    }
}

/// Dump 32 bytes around a kernel-params buffer address to see what byte
/// value the kernel actually sees in the flag slot. PrimGetEnvDomainName
/// receives r2 = &fParams[domain_name_out] and r3 = &fParams[byte_flag_out].
/// We print both regions so we can compare against what the USR wrapper
/// will read via LDRB offset+13.
fn dump_param_buffer(r2: u32, r3: u32) {
    kprintln!("  param buffers: r2={:#010x} r3={:#010x} delta={}",
              r2, r3, r3 as i64 - r2 as i64);
    for (label, addr) in [("r2", r2), ("r3", r3)] {
        if addr == 0 { continue; }
        let base = addr & !0x1F;  // round down to 32-byte line
        let mut buf = [0u32; 8];
        for i in 0..8 {
            buf[i] = guest_mem::read_word_va(base.wrapping_add((i as u32) * 4))
                .unwrap_or(0xDEAD_BEEF);
        }
        kprintln!(
            "  buf({}) @{:#010x}: {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
            label, base, buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]
        );
    }
}

fn dump_env_config_table() {
    check_byte_access_patches();
    // PrimGetEnvDomainName's lookup: iterate `*(0x0c10143c + idx*24)` treating
    // each 24-byte row as (env_name, ?, ?, ?, list_ptr, list_ptr2). When env
    // matches, dereference field +16 to get a pointer to a NUL-terminated 4cc
    // list of domain names.
    let base = 0x0c10143cu32;
    let mut buf = [0u32; 96];
    for i in 0..96 {
        buf[i] = guest_mem::read_word_va(base.wrapping_add((i as u32) * 4))
            .unwrap_or(0xDEAD_BEEF);
    }
    kprintln!("  env_config: flat table at {:#010x}:", base);
    for row in 0..8 {
        let addr = base.wrapping_add((row as u32) * 24);
        let off = (row * 24) / 4;
        if off + 5 >= buf.len() { break; }
        kprintln!(
            "  [{:#010x}] env={:08x} d0={:08x} d1={:08x} d2={:08x} list={:08x} list2={:08x}",
            addr, buf[off], buf[off+1], buf[off+2], buf[off+3], buf[off+4], buf[off+5],
        );
    }

    // Now dereference each entry's list pointer (field +16) and dump the
    // domain-name list until we hit a zero terminator. Limit per-list dump
    // to avoid runaway reads.
    for row in 0..8 {
        let off = (row * 24) / 4;
        if off + 4 >= buf.len() { break; }
        let env = buf[off];
        let list_ptr = buf[off + 4];
        if env == 0 || list_ptr == 0 || list_ptr == 0xDEAD_BEEF { continue; }
        let mut names = [0u32; 10];
        for i in 0..10 {
            names[i] = guest_mem::read_word_va(list_ptr.wrapping_add((i as u32) * 4))
                .unwrap_or(0);
            if names[i] == 0 { break; }
        }
        kprintln!(
            "  env {:#010x} list@{:#010x}: {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
            env, list_ptr,
            names[0], names[1], names[2], names[3],
            names[4], names[5], names[6], names[7],
        );
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
            kprintln!(
                "putc {:5}..{:5} lr={:#010x}: {}",
                FIRST_SEQ, seq, FIRST_LR, s
            );
            LEN = 0;
        }
    }
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
