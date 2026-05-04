//! AArch32 store-instruction emulator for stage-2 RO traps.
//!
//! When a watched physical page is held RO+XN at stage-2, every guest
//! store fires a permission fault. The original handler (see
//! `g1_capture` / `alrt_capture`) auto-flips the page to RW and lets
//! the kernel retry natively, then re-imposes RO at the next IRQ. That
//! pattern misses every write between the first fault and the IRQ
//! rearm — the page is RW for ~16 ms.
//!
//! This module replaces the auto-flip with in-handler emulation:
//! decode the AArch32 store at `ELR_EL2`, apply the writes via the
//! PA helpers (which bypass stage-2 permissions because EL2 has its
//! own stage-1 mapping), advance `ELR_EL2`, and leave the page RO.
//! Every subsequent store re-faults so we capture the full sequence.
//!
//! Coverage: STR/STRB/STRH (immediate offset, A1) + STM/STMDB/STMIB/
//! STMDA (A1, register-list, with optional writeback). Returns
//! `false` on any unrecognized form so the caller can fall back to
//! the auto-flip path. Vector and exclusive variants are not in scope
//! — the corrupting writer in the alrt-CList investigation is
//! expected to be plain ARM scalar stores.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::guest_mem;
use crate::kprintln;
use crate::trap::TrapContext;

/// Watch window: an (armed_pa, off_lo, off_hi) range to log each
/// emulated store against. When a store's destination PA matches the
/// armed page and the byte offset falls within `[off_lo, off_hi)`,
/// pa_emulate emits one kprintln per word with `(elr, va, pa_off,
/// value, src_mode)`. `0` for `armed_pa` disables logging.
///
/// Set by `arm_watch_window` at boot, e.g. once `alrt_capture` knows
/// the PA backing the CList header. Single-writer, infrequent —
/// AtomicU32 is plenty.
static WATCH_PA:    AtomicU32 = AtomicU32::new(0);
static WATCH_LO:    AtomicU32 = AtomicU32::new(0);
static WATCH_HI:    AtomicU32 = AtomicU32::new(0);
/// Per-emulator-write log budget. Each kprintln decrements; once 0,
/// no further per-store logs (counters still increment).
static WATCH_BUDGET: AtomicU32 = AtomicU32::new(0);

pub fn arm_watch_window(armed_pa: u32, off_lo: u32, off_hi: u32, budget: u32) {
    WATCH_PA.store(armed_pa, Ordering::Relaxed);
    WATCH_LO.store(off_lo, Ordering::Relaxed);
    WATCH_HI.store(off_hi, Ordering::Relaxed);
    WATCH_BUDGET.store(budget, Ordering::Relaxed);
}

fn log_if_in_window(elr: u32, va: u32, value: u32, mode: u32, label: &str) {
    let armed = WATCH_PA.load(Ordering::Relaxed);
    if armed == 0 {
        return;
    }
    let pa = match resolve_pa(va) {
        Some(p) => p,
        None => return,
    };
    if (pa & !0xFFF) != armed {
        return;
    }
    let off = pa & 0xFFF;
    let lo = WATCH_LO.load(Ordering::Relaxed);
    let hi = WATCH_HI.load(Ordering::Relaxed);
    if off < lo || off >= hi {
        return;
    }
    note_pc(elr);

    // Subpage-violation check: the kernel-intent mask for the VA
    // making this write should grant AP for the subpage containing
    // `off`. If it doesn't, the write is corrupting bytes that
    // belong to another VA's task per ARMv4 subpage AP — exactly
    // the bug the audit missed. Promote those to a "CORRUPTION"
    // log line regardless of the per-PC suppression so they always
    // surface.
    let armed_pa = armed;
    let va_page = va & !0xFFF;
    let intended_mask = crate::trap::kernel_intent_mask_for(armed_pa, va_page);
    let owns_subpage = match intended_mask {
        Some(mask) => subpage_owned(mask, off),
        None => true, // No intent recorded — don't flag.
    };

    if !owns_subpage {
        let prev = CORRUPTION_LOG_BUDGET.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            kprintln!(
                "pa-emul CORRUPTION: PC={:#010x} VA={:#010x} (page={:#010x} mask={:#x}) writes PA={:#010x}+{:#x} \
value={:#010x} mode={:#x} [{}] — subpage AP[{}] not in kernel-intent mask, this is the cross-subpage write \
ARMv4 subpage AP would have caught",
                elr, va, va_page,
                intended_mask.unwrap_or(0),
                armed_pa, off, value, mode, label,
                subpage_index(off),
            );
        } else {
            CORRUPTION_LOG_BUDGET.store(0, Ordering::Relaxed);
        }
        // Always advance per-PC budget below so per-write logging
        // continues for non-corruption writers.
    }

    if pc_suppressed(elr) {
        return;
    }
    let prev = WATCH_BUDGET.fetch_sub(1, Ordering::Relaxed);
    if prev == 0 {
        WATCH_BUDGET.store(0, Ordering::Relaxed);
        return;
    }
    if prev > 0 {
        kprintln!(
            "pa-emul[{}]: PC={:#010x} VA={:#010x} PA={:#010x}+{:#x} value={:#010x} mode={:#x}",
            label, elr, va, armed, off, value, mode,
        );
    }
}

/// Return `true` iff bit `subpage_index(off)` of `mask` is set in
/// any of its two AP-bit slots. The kernel encodes
/// AP[N]=11 as `mask |= 0x3 << (N*2)`, so subpage N is "owned"
/// iff `mask & (0x3 << (N*2)) != 0`.
fn subpage_owned(mask: u32, off: u32) -> bool {
    let sp = subpage_index(off);
    (mask & (0x3 << (sp * 2))) != 0
}

fn subpage_index(off: u32) -> u32 {
    (off & 0xFFF) >> 10
}

/// PCs that are part of cold-boot RAM-init fills (poison + zero) —
/// these legitimately scribble the page before any task allocation.
/// Suppress per-write logging for them (counters still increment).
const PC_SUPPRESS: &[u32] = &[
    0x00018ddc, // kernel zero-fill loop
    0x00019a84, 0x00019ac0, 0x00019af0, // kernel poison-fill loops
];

fn pc_suppressed(pc: u32) -> bool {
    PC_SUPPRESS.iter().any(|&p| p == pc)
}

/// PC-frequency table for end-of-boot summary. Fixed-size to fit
/// no_std; spillover counts go into a sentinel bucket.
const PC_TABLE_SLOTS: usize = 32;
struct PcSlot {
    pc: AtomicU32,
    count: AtomicU32,
}
static PC_TABLE: [PcSlot; PC_TABLE_SLOTS] = {
    const Z: PcSlot = PcSlot {
        pc: AtomicU32::new(0),
        count: AtomicU32::new(0),
    };
    [Z; PC_TABLE_SLOTS]
};
static PC_OVERFLOW: AtomicU32 = AtomicU32::new(0);

fn note_pc(pc: u32) {
    for slot in &PC_TABLE {
        let cur = slot.pc.load(Ordering::Relaxed);
        if cur == pc {
            slot.count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if cur == 0 {
            // Try to claim the slot. Race window is small but real;
            // use compare_exchange.
            if slot.pc.compare_exchange(0, pc, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                slot.count.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // Lost the race — re-scan from the start to either find
            // our PC's claimed slot or another empty one.
            return note_pc_scan_after_race(pc);
        }
    }
    PC_OVERFLOW.fetch_add(1, Ordering::Relaxed);
}

fn note_pc_scan_after_race(pc: u32) {
    for slot in &PC_TABLE {
        if slot.pc.load(Ordering::Relaxed) == pc {
            slot.count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    PC_OVERFLOW.fetch_add(1, Ordering::Relaxed);
}

/// Dump the PC frequency table, sorted by count descending. Called
/// from the alrt_capture summary at the Reboot canary.
pub fn dump_pc_table() {
    // Snapshot first to a small local array (no heap, no alloc).
    let mut snap: [(u32, u32); PC_TABLE_SLOTS] = [(0u32, 0u32); PC_TABLE_SLOTS];
    let mut n = 0usize;
    for slot in &PC_TABLE {
        let pc = slot.pc.load(Ordering::Relaxed);
        let count = slot.count.load(Ordering::Relaxed);
        if pc != 0 {
            snap[n] = (pc, count);
            n += 1;
        }
    }
    // Insertion-sort by count descending (n is small).
    for i in 1..n {
        let key = snap[i];
        let mut j = i;
        while j > 0 && snap[j - 1].1 < key.1 {
            snap[j] = snap[j - 1];
            j -= 1;
        }
        snap[j] = key;
    }
    kprintln!("pa-emul writer-PC frequency (top hits in watch window):");
    for &(pc, count) in &snap[..n] {
        let label = pc_label(pc);
        kprintln!("    PC={:#010x}  count={:6}  {}", pc, count, label);
    }
    let overflow = PC_OVERFLOW.load(Ordering::Relaxed);
    if overflow != 0 {
        kprintln!("    (table overflow: {} writes from {} additional PCs)",
            overflow, "untracked");
    }
}

fn pc_label(pc: u32) -> &'static str {
    match pc {
        0x00018ddc => "kernel zero-fill loop (boot-init)",
        0x00019a84 | 0x00019ac0 | 0x00019af0 => "kernel poison-fill loop (boot-init)",
        0x00310850 => "SetFreeChain prologue push",
        0x003121b0 => "MoveFreeBlock prologue push",
        0x003940b4 => "LowLevelCopyEngineLong memcpy",
        _ => "?",
    }
}

static CORRUPTION_LOG_BUDGET: AtomicU32 = AtomicU32::new(64);

fn resolve_pa(va: u32) -> Option<u32> {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read is side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    if sctlr & 1 != 0 {
        guest_mem::translate_va(va)
    } else {
        Some(va)
    }
}

/// Probe-source CPSR for register banking. Stage-2 faults can come
/// from any AArch32 mode; we read SPSR_EL2 in the trap handler and
/// pass it through. `mode` = SPSR_EL2 & 0x1F.
pub fn try_emulate_store(
    ctx: &mut TrapContext,
    elr: u32,
    src_cpsr: u32,
    ipa_first_byte: u32,
) -> EmulationResult {
    // Always advance ELR by 4 on success (ARM A1 instructions are 4 bytes).
    let insn = match read_guest_word(elr) {
        Some(w) => w,
        None => return EmulationResult::Unrecognized,
    };

    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        return EmulationResult::Unrecognized;
    }
    if !cond_passes(cond, src_cpsr) {
        // Condition false — instruction is a no-op. Just skip it.
        unsafe {
            core::arch::asm!(
                "msr elr_el2, {}",
                in(reg) (elr.wrapping_add(4)) as u64,
                options(nostack, preserves_flags),
            );
        }
        return EmulationResult::Skipped;
    }

    let mode = src_cpsr & 0x1F;

    // Try each shape in turn.
    if let Some(result) = decode_str_imm(insn) {
        return apply_str_imm(ctx, elr, mode, result, ipa_first_byte);
    }
    if let Some(result) = decode_strb_imm(insn) {
        return apply_strb_imm(ctx, elr, mode, result);
    }
    if let Some(result) = decode_strh_imm(insn) {
        return apply_strh_imm(ctx, elr, mode, result);
    }
    if let Some(result) = decode_stm(insn) {
        return apply_stm(ctx, elr, mode, result);
    }
    EmulationResult::Unrecognized
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum EmulationResult {
    /// Instruction was decoded and emulated (or skipped on cond-false).
    /// Caller MUST NOT auto-flip the page or advance ELR — both are done.
    Emulated,
    /// Instruction's condition was false; ELR advanced, no store performed.
    Skipped,
    /// Couldn't decode — caller falls back to auto-flip-and-retry.
    Unrecognized,
}

struct StrImm {
    rn: u32,
    rt: u32,
    imm12: u32,
    p: bool,
    u: bool,
    w: bool,
}

fn decode_str_imm(insn: u32) -> Option<StrImm> {
    // STR (immediate, A1): cond 010 P U 0 W 0 Rn Rt imm12
    if (insn & 0x0E50_0000) == 0x0400_0000 {
        return Some(StrImm {
            p: (insn >> 24) & 1 != 0,
            u: (insn >> 23) & 1 != 0,
            w: (insn >> 21) & 1 != 0,
            rn: (insn >> 16) & 0xF,
            rt: (insn >> 12) & 0xF,
            imm12: insn & 0xFFF,
        });
    }
    None
}

fn decode_strb_imm(insn: u32) -> Option<StrImm> {
    // STRB (immediate, A1): cond 010 P U 1 W 0 Rn Rt imm12
    if (insn & 0x0E50_0000) == 0x0440_0000 {
        return Some(StrImm {
            p: (insn >> 24) & 1 != 0,
            u: (insn >> 23) & 1 != 0,
            w: (insn >> 21) & 1 != 0,
            rn: (insn >> 16) & 0xF,
            rt: (insn >> 12) & 0xF,
            imm12: insn & 0xFFF,
        });
    }
    None
}

struct StrhImm {
    rn: u32,
    rt: u32,
    imm8: u32,
    p: bool,
    u: bool,
    w: bool,
}

fn decode_strh_imm(insn: u32) -> Option<StrhImm> {
    // STRH (immediate, A1): cond 000 P U 1 W 0 Rn Rt imm4H 1 011 imm4L
    if (insn & 0x0E40_00F0) == 0x0040_00B0 {
        let imm4h = (insn >> 8) & 0xF;
        let imm4l = insn & 0xF;
        return Some(StrhImm {
            p: (insn >> 24) & 1 != 0,
            u: (insn >> 23) & 1 != 0,
            w: (insn >> 21) & 1 != 0,
            rn: (insn >> 16) & 0xF,
            rt: (insn >> 12) & 0xF,
            imm8: (imm4h << 4) | imm4l,
        });
    }
    None
}

struct Stm {
    rn: u32,
    list: u32,
    p: bool,
    u: bool,
    w: bool,
    s: bool,
}

fn decode_stm(insn: u32) -> Option<Stm> {
    // LDM/STM (A1): cond 100 P U S W L Rn register_list (16 bits)
    // L=0 → store; L=1 → load (skip).
    if (insn & 0x0E10_0000) == 0x0800_0000 {
        return Some(Stm {
            p: (insn >> 24) & 1 != 0,
            u: (insn >> 23) & 1 != 0,
            s: (insn >> 22) & 1 != 0,
            w: (insn >> 21) & 1 != 0,
            rn: (insn >> 16) & 0xF,
            list: insn & 0xFFFF,
        });
    }
    None
}

fn apply_str_imm(
    ctx: &mut TrapContext,
    elr: u32,
    mode: u32,
    op: StrImm,
    _ipa: u32,
) -> EmulationResult {
    // Reject PC as Rn or Rt — adds writeback complexity we don't need
    // and the watched page sees normal kernel scalar stores.
    if op.rn == 15 || op.rt == 15 {
        return EmulationResult::Unrecognized;
    }
    let rn_val = read_reg(ctx, op.rn, mode);
    let off = op.imm12;
    let ea_offsetted = if op.u {
        rn_val.wrapping_add(off)
    } else {
        rn_val.wrapping_sub(off)
    };
    let access_addr = if op.p { ea_offsetted } else { rn_val };
    let val = read_reg(ctx, op.rt, mode);
    log_if_in_window(elr, access_addr, val, mode, "STR");
    if !guest_write_word(access_addr, val) {
        return EmulationResult::Unrecognized;
    }
    if !op.p || op.w {
        write_reg(ctx, op.rn, mode, ea_offsetted);
    }
    advance_elr(elr);
    EmulationResult::Emulated
}

fn apply_strb_imm(
    ctx: &mut TrapContext,
    elr: u32,
    mode: u32,
    op: StrImm,
) -> EmulationResult {
    if op.rn == 15 || op.rt == 15 {
        return EmulationResult::Unrecognized;
    }
    let rn_val = read_reg(ctx, op.rn, mode);
    let off = op.imm12;
    let ea_offsetted = if op.u {
        rn_val.wrapping_add(off)
    } else {
        rn_val.wrapping_sub(off)
    };
    let access_addr = if op.p { ea_offsetted } else { rn_val };
    let val = read_reg(ctx, op.rt, mode) as u8;
    log_if_in_window(elr, access_addr, val as u32, mode, "STRB");
    if !guest_write_byte(access_addr, val) {
        return EmulationResult::Unrecognized;
    }
    if !op.p || op.w {
        write_reg(ctx, op.rn, mode, ea_offsetted);
    }
    advance_elr(elr);
    EmulationResult::Emulated
}

fn apply_strh_imm(
    ctx: &mut TrapContext,
    elr: u32,
    mode: u32,
    op: StrhImm,
) -> EmulationResult {
    if op.rn == 15 || op.rt == 15 {
        return EmulationResult::Unrecognized;
    }
    let rn_val = read_reg(ctx, op.rn, mode);
    let off = op.imm8;
    let ea_offsetted = if op.u {
        rn_val.wrapping_add(off)
    } else {
        rn_val.wrapping_sub(off)
    };
    let access_addr = if op.p { ea_offsetted } else { rn_val };
    let val = read_reg(ctx, op.rt, mode) as u16;
    log_if_in_window(elr, access_addr, val as u32, mode, "STRH");
    if !guest_write_halfword(access_addr, val) {
        return EmulationResult::Unrecognized;
    }
    if !op.p || op.w {
        write_reg(ctx, op.rn, mode, ea_offsetted);
    }
    advance_elr(elr);
    EmulationResult::Emulated
}

fn apply_stm(
    ctx: &mut TrapContext,
    elr: u32,
    mode: u32,
    op: Stm,
) -> EmulationResult {
    if op.s {
        // S=1 → user-mode register transfer. Rare and complex; skip.
        return EmulationResult::Unrecognized;
    }
    if op.rn == 15 {
        return EmulationResult::Unrecognized;
    }
    let count = op.list.count_ones();
    if count == 0 {
        return EmulationResult::Unrecognized;
    }
    let rn_val = read_reg(ctx, op.rn, mode);
    let total_bytes = count * 4;

    // Compute starting effective address per ARM ARM A8.6.189
    // (STM / STMIA / STMEA), A8.6.190 (STMDA), A8.6.191 (STMDB / PUSH),
    // A8.6.192 (STMIB).
    let start_addr = match (op.p, op.u) {
        (false, true)  => rn_val,                                  // STMIA / STM (P=0,U=1)
        (true,  true)  => rn_val.wrapping_add(4),                  // STMIB        (P=1,U=1)
        (false, false) => rn_val.wrapping_sub(total_bytes - 4),    // STMDA        (P=0,U=0)
        (true,  false) => rn_val.wrapping_sub(total_bytes),        // STMDB / PUSH (P=1,U=0)
    };

    let mut addr = start_addr;
    // Walk register list lowest-numbered first; each register
    // stores at addr, addr+4, addr+8, ...
    for r in 0..16u32 {
        if (op.list >> r) & 1 == 0 {
            continue;
        }
        // R15 (PC) in the list stores PC of this STM instruction + 8
        // (ARM). Per A8.6.189 this is the only case we treat
        // specially; other registers go through `read_reg`.
        let val = if r == 15 {
            elr.wrapping_add(8)
        } else {
            read_reg(ctx, r, mode)
        };
        // STM writers are the prime suspect for the alrt CList
        // corruption (function-prologue `push {fp,ip,lr,pc}` lands
        // saved-LR + saved-PC bytes). Tag each emulated word with
        // the source register so the log shows which slot of the
        // STM hit the watch window.
        log_if_in_window(elr, addr, val, mode, stm_label(r));
        if !guest_write_word(addr, val) {
            return EmulationResult::Unrecognized;
        }
        addr = addr.wrapping_add(4);
    }

    // Writeback: ARM ARM specifies the new Rn value depends on
    // P+U+W independent of whether we wrote pre or post; for
    // STMIA/STMIB W=1 → Rn += total_bytes; STMDA/STMDB W=1 →
    // Rn -= total_bytes.
    if op.w {
        let new_rn = if op.u {
            rn_val.wrapping_add(total_bytes)
        } else {
            rn_val.wrapping_sub(total_bytes)
        };
        // Writeback not allowed if Rn is in the list (UNPREDICTABLE);
        // we still apply it because real CPUs do.
        write_reg(ctx, op.rn, mode, new_rn);
    }

    advance_elr(elr);
    EmulationResult::Emulated
}

fn stm_label(r: u32) -> &'static str {
    match r {
        0 => "STM-r0",   1 => "STM-r1",   2 => "STM-r2",   3 => "STM-r3",
        4 => "STM-r4",   5 => "STM-r5",   6 => "STM-r6",   7 => "STM-r7",
        8 => "STM-r8",   9 => "STM-r9",  10 => "STM-r10", 11 => "STM-fp",
       12 => "STM-ip",  13 => "STM-sp", 14 => "STM-lr",  15 => "STM-pc",
        _ => "STM-?",
    }
}

/// Helpers ---------------------------------------------------------

fn advance_elr(elr: u32) {
    let next = elr.wrapping_add(4);
    // SAFETY: ELR_EL2 controls post-ERET PC. We're in the data-abort
    // handler and own the EL2 register state.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {}",
            in(reg) next as u64,
            options(nostack, preserves_flags),
        );
    }
}

fn read_guest_word(addr: u32) -> Option<u32> {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read is side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    if sctlr & 1 != 0 {
        crate::guest_endian::guest_read_u32_va(addr)
    } else {
        crate::guest_endian::guest_read_u32_pa(addr)
    }
}

fn guest_write_word(addr: u32, value: u32) -> bool {
    let sctlr: u64;
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    if sctlr & 1 != 0 {
        crate::guest_endian::guest_write_u32_va(addr, value)
    } else {
        crate::guest_endian::guest_write_u32_pa(addr, value)
    }
}

fn guest_write_byte(addr: u32, value: u8) -> bool {
    let sctlr: u64;
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    let pa = if sctlr & 1 != 0 {
        match guest_mem::translate_va(addr) {
            Some(p) => p,
            None => return false,
        }
    } else {
        addr
    };
    guest_mem::write_byte_pa(pa, value)
}

fn guest_write_halfword(addr: u32, value: u16) -> bool {
    let sctlr: u64;
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    let pa = if sctlr & 1 != 0 {
        match guest_mem::translate_va(addr) {
            Some(p) => p,
            None => return false,
        }
    } else {
        addr
    };
    guest_mem::write_halfword_pa(pa, value)
}

fn read_reg(ctx: &TrapContext, reg: u32, mode: u32) -> u32 {
    let idx = ctx_slot_for_reg(reg, mode);
    ctx.x[idx] as u32
}

fn write_reg(ctx: &mut TrapContext, reg: u32, mode: u32, value: u32) {
    let idx = ctx_slot_for_reg(reg, mode);
    ctx.x[idx] = value as u64;
}

/// AArch32 register → AArch64 ctx slot map per ARM ARM Table D1-79.
/// Mirrors `unaligned::ctx_slot_for_reg` — kept inline so the two
/// modules stay decoupled.
fn ctx_slot_for_reg(reg: u32, mode: u32) -> usize {
    if reg <= 7 {
        return reg as usize;
    }
    const FIQ: u32 = 0x11;
    if reg <= 12 {
        if mode == FIQ {
            return (24 + (reg - 8)) as usize;
        }
        return reg as usize;
    }
    match (reg, mode) {
        (13, 0x10) | (13, 0x1F) => 13,
        (14, 0x10) | (14, 0x1F) => 14,
        (13, 0x11) => 29,
        (14, 0x11) => 30,
        (13, 0x12) => 17,
        (14, 0x12) => 16,
        (13, 0x13) => 19,
        (14, 0x13) => 18,
        (13, 0x17) => 21,
        (14, 0x17) => 20,
        (13, 0x1B) => 23,
        (14, 0x1B) => 22,
        _ => reg as usize,
    }
}

fn cond_passes(cond: u32, cpsr: u32) -> bool {
    let n = (cpsr >> 31) & 1 != 0;
    let z = (cpsr >> 30) & 1 != 0;
    let c = (cpsr >> 29) & 1 != 0;
    let v = (cpsr >> 28) & 1 != 0;
    match cond & 0xF {
        0x0 => z,
        0x1 => !z,
        0x2 => c,
        0x3 => !c,
        0x4 => n,
        0x5 => !n,
        0x6 => v,
        0x7 => !v,
        0x8 => c && !z,
        0x9 => !c || z,
        0xA => n == v,
        0xB => n != v,
        0xC => !z && (n == v),
        0xD => z || (n != v),
        0xE => true,
        _ => true,
    }
}

/// Counters for diagnostic dumps. `EMULATED` = per-instruction count
/// of successful in-handler emulations on watched pages;
/// `UNRECOGNIZED` = we fell back to auto-flip; `SKIPPED` = condition
/// was false.
pub static EMULATED:     AtomicU32 = AtomicU32::new(0);
pub static UNRECOGNIZED: AtomicU32 = AtomicU32::new(0);
pub static SKIPPED:      AtomicU32 = AtomicU32::new(0);

pub fn note(result: EmulationResult) {
    let counter = match result {
        EmulationResult::Emulated => &EMULATED,
        EmulationResult::Skipped => &SKIPPED,
        EmulationResult::Unrecognized => &UNRECOGNIZED,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}
