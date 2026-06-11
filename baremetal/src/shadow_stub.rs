//! In-ROM stub-pool + per-stub scratch-pool infrastructure.
//!
//! Provides the shared machinery that sister modules (currently just
//! `unaligned_inline`) use to install per-PC inline stubs in a
//! reserved window of the ROM aperture and to allocate per-stub
//! scratch slots in a kernel-VA carve-out.
//!
//! - **Stub pool** at IPA `0x00E0_0000..0x00FF_FF00`. 16-word slots,
//!   reachable from any ROM call site via a ±32 MiB `B`. Sister
//!   modules call [`alloc_stub_slot`] to grab the next slot, then
//!   [`install_inline_at`] to write the stub body and patch the
//!   originating PC with `B stub_ipa`.
//!
//! - **Scratch pool** at IPA `0x0600_0000`, identity-mapped at
//!   stage-2 to a host-side static buffer ([`SCRATCH_POOL`]). The
//!   first [`RESERVED_SCRATCH_SLOTS`] slots back the EL2 UND/DABT
//!   trampolines' banked-register save area; everything past that is
//!   handed out by [`scratch_slot_va`] for per-stub literals.
//!
//! - **Liveness analysis** ([`live_regs_at`]) walks an APCS-conformant
//!   CFG forward from a given PC to determine which of {R0..R3, R12,
//!   R14} are dead, so a stub can use them as scratches without a
//!   save/restore round-trip.
//!
//! - **`encode` submodule** with the ARM A1 encoders that the stub
//!   builders need. The set is deliberately minimal — extend as new
//!   stub variants land.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

// =======================================================================
// Stub pool
// =======================================================================
//
// Each per-PC inline stub gets a 16-word slot inside the ROM aperture
// at IPA `0x00E0_0000..0x00FF_FF00`. The originating PC is rewritten
// to `B stub_ipa`, so the stub body runs in place of the original
// instruction and branches back to `orig_pc + 4` once it completes.
//
// Sits between the tracer pool (0x0090_0000..0x00E0_0000) and the
// ROM-tail trampoline cluster (0x00FF_FF00..0x00FF_FFF0); tracer's
// `in_reserved_range` excludes this window too.
pub const SBA_STUB_POOL_IPA: u32 = 0x00E0_0000;
pub const SBA_STUB_POOL_END: u32 = 0x00FF_FF00;
pub const SBA_STUB_WORDS: usize = 16;
pub const SBA_STUB_BYTES: u32 = (SBA_STUB_WORDS as u32) * 4;
pub const SBA_STUB_MAX: usize =
    ((SBA_STUB_POOL_END - SBA_STUB_POOL_IPA) / SBA_STUB_BYTES) as usize;

static NEXT_STUB: AtomicUsize = AtomicUsize::new(0);

// =======================================================================
// Scratch pool
// =======================================================================
//
// 384 KiB carve-out at IPA == VA == 0x0600_0000. Identity-mapped so:
//   * Newton boot (kernel stage-1 on): kernel L1[VA>>20] = section
//     descriptor identity-mapping VA→IPA. Stage-2 maps IPA →
//     SCRATCH_POOL.
//   * Guest-test mode (kernel stage-1 off): stage-1 is bypassed; the
//     CPU emits VA as IPA directly. Stage-2 sees IPA == VA and maps
//     to SCRATCH_POOL.
//
// Identity mapping keeps per-slot literals usable from both regimes
// without two separate stage-2 mappings.
//
// L1[0x60] sits in a free gap of the kernel's L1 census (slots
// 0x52..0xBF are unused) and is also free in the existing stage-2
// layout (between RAM at 0x0440_0000 and the framebuffer at
// 0x0E00_0000).
pub const SCRATCH_POOL_VA: u32 = 0x0600_0000;
pub const SCRATCH_POOL_IPA: u32 = 0x0600_0000;
pub const SCRATCH_POOL_SIZE: usize = 384 * 1024; // 96 × 4 KiB pages

#[repr(C, align(4096))]
pub struct ScratchPool(pub [u8; SCRATCH_POOL_SIZE]);
pub static mut SCRATCH_POOL: ScratchPool = ScratchPool([0; SCRATCH_POOL_SIZE]);

/// Host PA of the scratch pool — used by `stage2::install_scratch_pool`
/// when populating the L3 page table that backs the carve-out IPA.
pub fn scratch_pool_host_pa() -> u64 {
    addr_of_mut!(SCRATCH_POOL) as u64
}

// =======================================================================
// I-cache / code-write helpers
// =======================================================================

/// Write `word` into a backing store that owns this IPA. Supports the
/// ROM backing and the RAM backing.
fn code_write_word(ipa: u32, word: u32) -> Result<(), &'static str> {
    if (ipa as usize) + 4 <= crate::guest_mem::ROM_SIZE {
        let host = crate::guest_mem::rom_host_pa() as usize + ipa as usize;
        // SAFETY: ROM backing is hypervisor-owned and word-sized writes are
        // race-free against the guest before stage2 enable; we're called
        // from the stub installer.
        unsafe { core::ptr::write_volatile(host as *mut u32, word); }
        return Ok(());
    }
    let ram_base = crate::guest_mem::RAM_IPA_BASE as usize;
    if (ipa as usize) >= ram_base
        && (ipa as usize) + 4 <= ram_base + crate::guest_mem::RAM_SIZE
    {
        let host = crate::guest_mem::ram_host_pa() as usize + (ipa as usize - ram_base);
        // SAFETY: as above, against the RAM backing.
        unsafe { core::ptr::write_volatile(host as *mut u32, word); }
        return Ok(());
    }
    Err("code_write_word: IPA outside ROM or RAM backing")
}

/// Read the ORIGINAL pre-patch instruction at `ipa`. The liveness
/// analyser uses this so probe HVCs installed by
/// `rom_patches::apply_717006_patches` don't confuse it.
fn code_read_word_original_first(ipa: u32) -> Option<u32> {
    if let Some(orig) = crate::rom_patches::read_original(ipa) {
        return Some(orig);
    }
    crate::guest_mem::read_word_pa(ipa)
}

/// Publish a freshly-written code range to the instruction stream.
///
/// Two passes — first `DC CVAU` every line to push D-cache dirty lines
/// to the Point of Unification, DSB ISH, then `IC IVAU` every line and
/// DSB+ISB. The DSB between the two passes is load-bearing: without it
/// IC IVAU can complete ahead of DC CVAU on cores that model I/D cache
/// non-coherence strictly (FVP Base RevC does; QEMU raspi3b TCG doesn't
/// model it), so the invalidated I-cache line refills from L2 before
/// the D-side writeback has reached it, and the guest fetches stale
/// bytes.
pub fn icache_sync_range(host_va: u64, length: usize) {
    let start = host_va & !0x3F;
    let end = host_va + length as u64;
    let mut addr = start;
    while addr < end {
        // SAFETY: cache-maintenance only touches caches.
        unsafe {
            core::arch::asm!(
                "dc cvau, {0}",
                in(reg) addr,
                options(nostack, preserves_flags),
            );
        }
        addr += 64;
    }
    // SAFETY: barrier.
    unsafe {
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
    addr = start;
    while addr < end {
        // SAFETY: cache-maintenance only touches caches.
        unsafe {
            core::arch::asm!(
                "ic ivau, {0}",
                in(reg) addr,
                options(nostack, preserves_flags),
            );
        }
        addr += 64;
    }
    // SAFETY: barrier.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

// =======================================================================
// Encoder helpers
// =======================================================================

pub(crate) mod encode {
    pub const AL: u32 = 0xE;

    /// Encode a 32-bit value as an ARMv7 modified-immediate (imm8 rotated
    /// right by 2*rot). Returns None if the value isn't representable in
    /// one instruction.
    pub fn arm_imm12(value: u32) -> Option<u32> {
        for rot in 0..16u32 {
            let imm8 = value.rotate_left(rot * 2);
            if imm8 < 256 {
                return Some((rot << 8) | imm8);
            }
        }
        None
    }

    /// NOP A1 hint encoding `0xE320_F000`.
    pub fn nop() -> u32 {
        0xE320_F000
    }

    /// ADD Rd, Rn, #imm12  (modified-immediate encoded).
    pub fn add_imm(cond: u32, rd: u32, rn: u32, imm12: u32) -> u32 {
        (cond << 28) | 0x0280_0000 | (rn << 16) | (rd << 12) | (imm12 & 0xFFF)
    }

    /// SUB Rd, Rn, #imm12  (modified-immediate encoded).
    pub fn sub_imm(cond: u32, rd: u32, rn: u32, imm12: u32) -> u32 {
        (cond << 28) | 0x0240_0000 | (rn << 16) | (rd << 12) | (imm12 & 0xFFF)
    }

    /// ADD Rd, Rn, Rm, <shift_type> #shift_amount.
    pub fn add_reg_shifted(
        cond: u32, rd: u32, rn: u32, rm: u32, shift_type: u32, shift_amount: u32,
    ) -> u32 {
        (cond << 28)
            | 0x0080_0000
            | (rn << 16)
            | (rd << 12)
            | ((shift_amount & 0x1F) << 7)
            | ((shift_type & 3) << 5)
            | (rm & 0xF)
    }

    /// SUB Rd, Rn, Rm, <shift_type> #shift_amount.
    pub fn sub_reg_shifted(
        cond: u32, rd: u32, rn: u32, rm: u32, shift_type: u32, shift_amount: u32,
    ) -> u32 {
        (cond << 28)
            | 0x0040_0000
            | (rn << 16)
            | (rd << 12)
            | ((shift_amount & 0x1F) << 7)
            | ((shift_type & 3) << 5)
            | (rm & 0xF)
    }

    /// B <label>. `from_pc` is the PC of the B itself; `target` is the
    /// destination. None if out of ±32 MiB range.
    pub fn b(from_pc: u32, target: u32) -> Option<u32> {
        let pc_plus_8 = from_pc.wrapping_add(8);
        let off = (target as i64) - (pc_plus_8 as i64);
        if off & 3 != 0 { return None; }
        let off_words = off >> 2;
        if !(-(1i64 << 23)..(1i64 << 23)).contains(&off_words) {
            return None;
        }
        let imm24 = (off_words as u32) & 0x00FF_FFFF;
        Some((AL << 28) | 0x0A00_0000 | imm24)
    }
}

// =======================================================================
// Liveness analysis
// =======================================================================
//
// To avoid saving and restoring scratch register values across an
// inline stub, we walk forward from the originating PC and identify
// which of {R0..R3, R12, R14} are GENUINELY DEAD — i.e. the next
// reference to each is a write, not a read.
//
// The analyzer is deliberately conservative: anything we can't decode,
// any indirect branch with an unknown target, and any "max
// instructions" bound all mark the remaining unwritten registers as
// live. False positives (claiming "live" when actually dead) cost us
// inline coverage; false negatives (claiming "dead" when actually
// live) are correctness bugs and must not happen.

type RegMask = u16;

const REG_PC: u32 = 15;

/// Branch kind returned by `analyze_insn`. Classifies how the
/// instruction transfers (or doesn't transfer) control flow, so the
/// CFG-aware liveness walker can follow branch targets explicitly.
#[derive(Debug, Clone, Copy)]
enum BranchKind {
    /// Linear instruction. No control transfer; analyzer continues at PC+4.
    Linear,
    /// BL or BL-like — eventually returns. APCS-clobbers
    /// {R0..R3, R12, R14}; analyzer continues at PC+4 with those regs
    /// effectively "written".
    BLink,
    /// Conditional BL/BLX/SVC/HVC/SMC. The taken path is an APCS call
    /// (reads {R0..R3}, clobbers {R0..R3, R12, R14}); the not-taken path
    /// preserves those registers. Mirrors the conditional-write rule:
    /// reads are counted conservatively (so call params stay live), but
    /// the caller-saved clobber is NOT added to `written`, because a
    /// downstream read can be upward-exposed through the not-taken edge.
    CondBLink,
    /// Unconditional branch. Analyzer follows `target` and stops.
    Direct { target: u32 },
    /// Conditional branch (Bcc, no link). Analyzer must consider both
    /// paths: branch-taken to `target`, and fall-through to PC+4.
    Cond { target: u32 },
    /// APCS function return (BX LR, POP {…, PC}, MOV PC, LR, etc.):
    /// control leaves to the caller.
    Return,
    /// Conditional APCS function return.
    CondReturn,
    /// Indirect branch with unknown target.
    Indirect,
    /// Unknown / unhandled instruction — give up conservatively.
    Unknown,
}

/// Compute the absolute target of a B/BL/Bcc/BLcc instruction.
fn branch_target(insn: u32, pc: u32) -> u32 {
    let imm24 = insn & 0x00FF_FFFF;
    let signed = ((imm24 as i32) << 8) >> 6; // sign-extend then <<2 for word
    pc.wrapping_add(8).wrapping_add(signed as u32)
}

/// Decode reads / writes / branch-classification for a single ARM
/// A1-encoded instruction. Reads / writes are GPR bitmasks (R0..R15);
/// the branch classification tells the CFG walker how to proceed.
///
/// Conservative for anything we don't handle: returns Unknown so the
/// walker treats remaining unwritten regs as live.
fn analyze_insn(insn: u32, pc: u32) -> (RegMask, RegMask, BranchKind) {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        return (0, 0, BranchKind::Unknown);
    }
    let cond_al = cond == 0xE;

    // Branch (B / BL): cond 101 L imm24
    if (insn & 0x0E00_0000) == 0x0A00_0000 {
        let l = (insn >> 24) & 1;
        let target = branch_target(insn, pc);
        let kind = if l == 1 {
            if cond_al { BranchKind::BLink } else { BranchKind::CondBLink }
        } else if cond_al {
            BranchKind::Direct { target }
        } else {
            BranchKind::Cond { target }
        };
        return (0, 0, kind);
    }
    // BX / BLX register: cond 0001 0010 SBO 00LM Rm
    if (insn & 0x0FFF_FFD0) == 0x012F_FF10 {
        let rm = insn & 0xF;
        if rm == 14 {
            let kind = if cond_al { BranchKind::Return } else { BranchKind::CondReturn };
            return (APCS_RETURN_LIVE, 0, kind);
        }
        let is_blx = (insn & 0x20) != 0;
        if is_blx {
            let kind = if cond_al { BranchKind::BLink } else { BranchKind::CondBLink };
            return (1u16 << rm, 0, kind);
        }
        return (1u16 << rm, 0, BranchKind::Indirect);
    }
    // SVC / SWI: cond 1111 imm24 — APCS-call shape.
    if (insn & 0x0F00_0000) == 0x0F00_0000 {
        let kind = if cond_al { BranchKind::BLink } else { BranchKind::CondBLink };
        return (0, 0, kind);
    }
    // BKPT
    if (insn & 0x0FF0_00F0) == 0x0120_0070 {
        return (0, 0, BranchKind::Unknown);
    }
    // HVC: cond 0001 0100 imm12 0111 imm4. APCS-call shape.
    if (insn & 0x0FF0_00F0) == 0x0140_0070 {
        let kind = if cond_al { BranchKind::BLink } else { BranchKind::CondBLink };
        return (0, 0, kind);
    }
    // SMC: same shape as HVC.
    if (insn & 0x0FF0_00F0) == 0x0160_0070 {
        let kind = if cond_al { BranchKind::BLink } else { BranchKind::CondBLink };
        return (0, 0, kind);
    }

    // MOVW (A2): cond 0011 0000 imm4 Rd imm12 — writes Rd.
    if (insn & 0x0FF0_0000) == 0x0300_0000 {
        let rd = (insn >> 12) & 0xF;
        let write = if cond_al { 1u16 << rd } else { 0 };
        if rd == REG_PC {
            return (0, 0, BranchKind::Indirect);
        }
        return (0, write, BranchKind::Linear);
    }
    // MOVT (A1): cond 0011 0100 imm4 Rd imm12 — reads Rd, writes Rd.
    if (insn & 0x0FF0_0000) == 0x0340_0000 {
        let rd = (insn >> 12) & 0xF;
        let read = 1u16 << rd;
        let write = if cond_al { 1u16 << rd } else { 0 };
        if rd == REG_PC {
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }

    // Data-processing (immediate): cond 001 opc S Rn Rd imm12
    if (insn & 0x0E00_0000) == 0x0200_0000 {
        let opc = (insn >> 21) & 0xF;
        let rn = (insn >> 16) & 0xF;
        let rd = (insn >> 12) & 0xF;
        let no_writeback = matches!(opc, 0b1000 | 0b1001 | 0b1010 | 0b1011);
        let no_rn_read = matches!(opc, 0b1101 | 0b1111);
        let read = if no_rn_read { 0 } else { 1u16 << rn };
        let write = if !no_writeback && cond_al { 1u16 << rd } else { 0 };
        if !no_writeback && rd == REG_PC {
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }

    // Data-processing (register, immediate shift): cond 000 opc S Rn Rd imm5 type 0 Rm
    if (insn & 0x0E00_0010) == 0x0000_0000 {
        let opc = (insn >> 21) & 0xF;
        let rn = (insn >> 16) & 0xF;
        let rd = (insn >> 12) & 0xF;
        let rm = insn & 0xF;
        let no_writeback = matches!(opc, 0b1000 | 0b1001 | 0b1010 | 0b1011);
        let no_rn_read = matches!(opc, 0b1101 | 0b1111);
        let read = (if no_rn_read { 0 } else { 1u16 << rn }) | (1u16 << rm);
        let write = if !no_writeback && cond_al { 1u16 << rd } else { 0 };
        if !no_writeback && rd == REG_PC {
            // MOV PC, LR — the canonical APCS return.
            if opc == 0b1101 && rm == 14 && (insn & 0x0000_0F80) == 0 {
                let kind = if cond_al { BranchKind::Return } else { BranchKind::CondReturn };
                return (APCS_RETURN_LIVE, 0, kind);
            }
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }
    // Data-processing (register-shifted): cond 000 opc S Rn Rd Rs 0 type 1 Rm
    if (insn & 0x0E00_0090) == 0x0000_0010 {
        let opc = (insn >> 21) & 0xF;
        let rn = (insn >> 16) & 0xF;
        let rd = (insn >> 12) & 0xF;
        let rs = (insn >> 8) & 0xF;
        let rm = insn & 0xF;
        let no_writeback = matches!(opc, 0b1000 | 0b1001 | 0b1010 | 0b1011);
        let no_rn_read = matches!(opc, 0b1101 | 0b1111);
        let read = (if no_rn_read { 0 } else { 1u16 << rn })
                 | (1u16 << rm) | (1u16 << rs);
        let write = if !no_writeback && cond_al { 1u16 << rd } else { 0 };
        if !no_writeback && rd == REG_PC {
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }

    // LDR/STR (immediate): cond 010 P U B W L Rn Rt imm12
    if (insn & 0x0E00_0000) == 0x0400_0000 {
        let l = (insn >> 20) & 1;
        let p = (insn >> 24) & 1;
        let w = (insn >> 21) & 1;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let writes_rn = (p == 0) || (w == 1);
        let read = (1u16 << rn) | if l == 0 { 1u16 << rt } else { 0 };
        let mut write = 0u16;
        if cond_al {
            if l == 1 { write |= 1u16 << rt; }
            if writes_rn { write |= 1u16 << rn; }
        }
        if l == 1 && rt == REG_PC {
            // `LDR PC, [SP], #4` — single-register pop-return form.
            if rn == 13 {
                let kind = if cond_al { BranchKind::Return } else { BranchKind::CondReturn };
                return (read | APCS_RETURN_LIVE, 0, kind);
            }
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }
    // LDR/STR (register): cond 011 P U B W L Rn Rt imm5 type 0 Rm
    if (insn & 0x0E00_0010) == 0x0600_0000 {
        let l = (insn >> 20) & 1;
        let p = (insn >> 24) & 1;
        let w = (insn >> 21) & 1;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let rm = insn & 0xF;
        let writes_rn = (p == 0) || (w == 1);
        let read = (1u16 << rn) | (1u16 << rm)
                 | if l == 0 { 1u16 << rt } else { 0 };
        let mut write = 0u16;
        if cond_al {
            if l == 1 { write |= 1u16 << rt; }
            if writes_rn { write |= 1u16 << rn; }
        }
        if l == 1 && rt == REG_PC {
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }

    // Extra load/store (LDRH/STRH/LDRSB/LDRSH/LDRD/STRD).
    if (insn & 0x0E00_0090) == 0x0000_0090 && ((insn >> 5) & 0x3) != 0 {
        let p = (insn >> 24) & 1;
        let i_bit = (insn >> 22) & 1;
        let w = (insn >> 21) & 1;
        let l = (insn >> 20) & 1;
        let op = (insn >> 5) & 0x3;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let writes_rn = (p == 0) || (w == 1);
        let is_ldrd_strd = l == 0 && (op == 0b10 || op == 0b11);
        let mut read = 1u16 << rn;
        if i_bit == 0 {
            read |= 1u16 << (insn & 0xF);
        }
        let mut write = 0u16;
        if cond_al {
            if l == 1 { write |= 1u16 << rt; }
            if is_ldrd_strd {
                if op == 0b10 {
                    write |= 1u16 << rt;
                    if rt + 1 < 16 { write |= 1u16 << (rt + 1); }
                } else {
                    read |= 1u16 << rt;
                    if rt + 1 < 16 { read |= 1u16 << (rt + 1); }
                }
            } else if l == 0 {
                read |= 1u16 << rt;
            }
            if writes_rn { write |= 1u16 << rn; }
        } else if l == 0 && !is_ldrd_strd {
            read |= 1u16 << rt;
        }
        return (read, write, BranchKind::Linear);
    }

    // LDM / STM
    if (insn & 0x0E00_0000) == 0x0800_0000 {
        let l = (insn >> 20) & 1;
        let w = (insn >> 21) & 1;
        let rn = (insn >> 16) & 0xF;
        let reglist = (insn & 0xFFFF) as u16;
        let mut read = 1u16 << rn;
        if l == 0 { read |= reglist; }
        let mut write = 0u16;
        if cond_al {
            if l == 1 { write |= reglist; }
            if w == 1 { write |= 1u16 << rn; }
        }
        if l == 1 && (reglist & (1u16 << 15)) != 0 {
            // POP {…, PC} / LDMDB fp, {…, PC} are APCS returns.
            if rn == 13 || rn == 11 {
                let kind = if cond_al { BranchKind::Return } else { BranchKind::CondReturn };
                return (APCS_RETURN_LIVE | read, 0, kind);
            }
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }

    // MUL / MLA
    if (insn & 0x0FC0_00F0) == 0x0000_0090 {
        let a_bit = (insn >> 21) & 1;
        let rd = (insn >> 16) & 0xF;
        let ra = (insn >> 12) & 0xF;
        let rs = (insn >> 8) & 0xF;
        let rm = insn & 0xF;
        let mut read = (1u16 << rs) | (1u16 << rm);
        if a_bit == 1 { read |= 1u16 << ra; }
        let write = if cond_al { 1u16 << rd } else { 0 };
        return (read, write, BranchKind::Linear);
    }
    // UMULL / SMULL / UMLAL / SMLAL
    if (insn & 0x0F80_00F0) == 0x0080_0090 {
        let a_bit = (insn >> 21) & 1;
        let rdhi = (insn >> 16) & 0xF;
        let rdlo = (insn >> 12) & 0xF;
        let rs = (insn >> 8) & 0xF;
        let rm = insn & 0xF;
        let mut read = (1u16 << rs) | (1u16 << rm);
        if a_bit == 1 { read |= (1u16 << rdhi) | (1u16 << rdlo); }
        let write = if cond_al { (1u16 << rdhi) | (1u16 << rdlo) } else { 0 };
        return (read, write, BranchKind::Linear);
    }

    // MRS Rd, CPSR/SPSR
    if (insn & 0x0FBF_0FFF) == 0x010F_0000 {
        let rd = (insn >> 12) & 0xF;
        let write = if cond_al { 1u16 << rd } else { 0 };
        return (0, write, BranchKind::Linear);
    }
    // MSR (immediate)
    if (insn & 0x0FB0_F000) == 0x0320_F000 {
        return (0, 0, BranchKind::Linear);
    }
    // MSR (register)
    if (insn & 0x0FB0_FFF0) == 0x0120_F000 {
        let rn = insn & 0xF;
        return (1u16 << rn, 0, BranchKind::Linear);
    }

    // MCR / MRC
    if (insn & 0x0F00_0010) == 0x0E00_0010 {
        let l = (insn >> 20) & 1;
        let rt = (insn >> 12) & 0xF;
        let read = if l == 0 { 1u16 << rt } else { 0 };
        let write = if l == 1 && cond_al { 1u16 << rt } else { 0 };
        return (read, write, BranchKind::Linear);
    }

    // SWP / SWPB
    if (insn & 0x0FB0_0FF0) == 0x0100_0090 {
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let rm = insn & 0xF;
        let read = (1u16 << rn) | (1u16 << rm);
        let write = if cond_al { 1u16 << rt } else { 0 };
        return (read, write, BranchKind::Linear);
    }

    // CLZ
    if (insn & 0x0FFF_0FF0) == 0x016F_0F10 {
        let rd = (insn >> 12) & 0xF;
        let rm = insn & 0xF;
        return (1u16 << rm, if cond_al { 1u16 << rd } else { 0 }, BranchKind::Linear);
    }

    (0, 0, BranchKind::Unknown)
}

/// APCS caller-saved set: R0..R3, R12, R14. Effectively "written" by
/// any function call from the caller's perspective.
const APCS_CALLER_SAVED: RegMask =
    (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 12) | (1 << 14);

/// APCS parameter registers — read by any callee.
const APCS_PARAMS: RegMask = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);

/// Registers the caller observably depends on at function return.
const APCS_RETURN_LIVE: RegMask =
    (1 << 0)
    | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7)
    | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11)
    | (1 << 13) | (1 << 14);

const LIVE_AT_MAX_VISITED: usize = 64;

/// Per-block analysis state.
struct Visited {
    pcs: [u32; LIVE_AT_MAX_VISITED],
    /// `live[i]` is the cached live_in mask for `pcs[i]`. `LIVE_IN_PROGRESS`
    /// means we're still inside the walk for that PC.
    live: [RegMask; LIVE_AT_MAX_VISITED],
    n: usize,
}

const LIVE_IN_PROGRESS: RegMask = u16::MAX;

impl Visited {
    fn new() -> Self {
        Self { pcs: [0; LIVE_AT_MAX_VISITED], live: [0; LIVE_AT_MAX_VISITED], n: 0 }
    }
    fn find(&self, pc: u32) -> Option<(usize, RegMask)> {
        for i in 0..self.n {
            if self.pcs[i] == pc {
                return Some((i, self.live[i]));
            }
        }
        None
    }
    fn start(&mut self, pc: u32) -> Option<usize> {
        if self.n >= LIVE_AT_MAX_VISITED { return None; }
        let idx = self.n;
        self.pcs[idx] = pc;
        self.live[idx] = LIVE_IN_PROGRESS;
        self.n += 1;
        Some(idx)
    }
    fn finish(&mut self, idx: usize, live: RegMask) {
        self.live[idx] = live;
    }
}

fn live_at_with_reader<R>(start_pc: u32, max_instrs: u32, read_insn: &R) -> RegMask
where R: Fn(u32) -> Option<u32> {
    let mut visited = Visited::new();
    live_at_recursive(start_pc, max_instrs, &mut visited, read_insn)
}

fn live_at_recursive<R>(
    start_pc: u32, max_instrs: u32, visited: &mut Visited, read_insn: &R,
) -> RegMask
where R: Fn(u32) -> Option<u32> {
    if let Some((_idx, cached)) = visited.find(start_pc) {
        if cached == LIVE_IN_PROGRESS {
            return 0;
        }
        return cached;
    }
    let idx = match visited.start(start_pc) {
        Some(i) => i,
        None => {
            return APCS_RETURN_LIVE;
        }
    };
    let live = live_at_walk(start_pc, max_instrs, visited, read_insn);
    visited.finish(idx, live);
    live
}

fn live_at_walk<R>(
    start_pc: u32, max_instrs: u32, visited: &mut Visited, read_insn: &R,
) -> RegMask
where R: Fn(u32) -> Option<u32> {
    let mut live: RegMask = 0;
    let mut written: RegMask = 0;
    let mut pc = start_pc;
    for _ in 0..max_instrs {
        let insn = match read_insn(pc) {
            Some(w) => w,
            None => {
                live |= APCS_RETURN_LIVE & !written;
                return live;
            }
        };
        let (read, write, kind) = analyze_insn(insn, pc);
        let new_live = read & !written & !live;
        live |= new_live;
        let new_written = write & !live & !written;
        written |= new_written;

        match kind {
            BranchKind::Linear => {
                pc = pc.wrapping_add(4);
                continue;
            }
            BranchKind::BLink => {
                live |= APCS_PARAMS & !written;
                let bl_clobber = APCS_CALLER_SAVED & !live;
                written |= bl_clobber;
                pc = pc.wrapping_add(4);
                continue;
            }
            BranchKind::CondBLink => {
                // Conditional call: the taken edge reads the param regs and
                // clobbers the caller-saved set, but the not-taken edge
                // preserves them. Count the param reads conservatively, but
                // do NOT add the clobber to `written` — otherwise a
                // downstream read that is upward-exposed through the
                // not-taken path would be masked out and the register
                // wrongly reported dead (false negative).
                live |= APCS_PARAMS & !written;
                pc = pc.wrapping_add(4);
                continue;
            }
            BranchKind::Direct { target } => {
                if target == pc {
                    return live;
                }
                if read_insn(target).is_none() {
                    live |= (APCS_RETURN_LIVE | APCS_PARAMS) & !written;
                    return live;
                }
                let tgt_live = live_at_recursive(target, max_instrs, visited, read_insn);
                live |= tgt_live & !written;
                return live;
            }
            BranchKind::Cond { target } => {
                if target == pc {
                    let fall = live_at_recursive(pc.wrapping_add(4), max_instrs, visited, read_insn);
                    live |= fall & !written;
                    return live;
                }
                let taken_live = if read_insn(target).is_none() {
                    APCS_RETURN_LIVE | APCS_PARAMS
                } else {
                    live_at_recursive(target, max_instrs, visited, read_insn)
                };
                let fall = live_at_recursive(pc.wrapping_add(4), max_instrs, visited, read_insn);
                live |= (taken_live | fall) & !written;
                return live;
            }
            BranchKind::Return => {
                return live;
            }
            BranchKind::CondReturn => {
                let fall = live_at_recursive(pc.wrapping_add(4), max_instrs, visited, read_insn);
                live |= fall & !written;
                return live;
            }
            BranchKind::Indirect | BranchKind::Unknown => {
                live |= APCS_RETURN_LIVE & !written;
                return live;
            }
        }
    }
    live |= APCS_RETURN_LIVE & !written;
    live
}

// =======================================================================
// Public surface for sister modules
// =======================================================================

/// Compute the live-register set (R0..R14 as bits 0..14 of a u16) at
/// `start_pc`. Reads ROM via the original-first reader so probe HVCs
/// installed earlier in boot don't confuse the analyser.
pub fn live_regs_at(start_pc: u32, max_instrs: u32) -> u16 {
    live_at_with_reader(start_pc, max_instrs, &code_read_word_original_first)
}

/// Allocate the next free slot in the inline-stub pool. Returns
/// `(slot_idx, stub_ipa)`. None if the pool is exhausted.
pub fn alloc_stub_slot() -> Option<(usize, u32)> {
    let slot_ix = NEXT_STUB.fetch_add(1, Ordering::SeqCst);
    if slot_ix >= SBA_STUB_MAX {
        return None;
    }
    let stub_ipa = SBA_STUB_POOL_IPA + (slot_ix as u32) * SBA_STUB_BYTES;
    Some((slot_ix, stub_ipa))
}

/// Install an inline stub at a previously-allocated slot.
///
/// `words.len()` must be ≤ `SBA_STUB_WORDS`; trailing slots are filled
/// with NOPs. Writes the stub words first, then patches `orig_pc` with
/// `B stub_ipa`, then icache-flushes both ranges.
///
/// Returns Err if the B from `orig_pc` to `stub_ipa` is out of imm24
/// range, or if either write fails. Never halts — caller decides
/// whether to fall back.
pub fn install_inline_at(
    orig_pc: u32, stub_ipa: u32, words: &[u32],
) -> Result<(), &'static str> {
    if words.len() > SBA_STUB_WORDS {
        return Err("install_inline_at: words exceeds SBA_STUB_WORDS");
    }
    let br = match encode::b(orig_pc, stub_ipa) {
        Some(w) => w,
        None => return Err("install_inline_at: B out of imm24 range"),
    };

    // Write the full slot — supplied words first, NOPs for the rest.
    let nop = encode::nop();
    for i in 0..SBA_STUB_WORDS {
        let w = words.get(i).copied().unwrap_or(nop);
        let ipa = stub_ipa.wrapping_add((i as u32) * 4);
        code_write_word(ipa, w)?;
    }

    // Then redirect the original site to the stub.
    code_write_word(orig_pc, br)?;

    // Flush both regions to the point of unification so the guest's
    // next fetch sees the freshly-written stub and B.
    let stub_host = match orig_pc_to_host(stub_ipa) {
        Some(h) => h,
        None => return Err("install_inline_at: stub_ipa not in ROM/RAM backing"),
    };
    icache_sync_range(stub_host, SBA_STUB_WORDS * 4);
    let orig_host = match orig_pc_to_host(orig_pc) {
        Some(h) => h,
        None => return Err("install_inline_at: orig_pc not in ROM/RAM backing"),
    };
    icache_sync_range(orig_host, 4);
    Ok(())
}

fn orig_pc_to_host(ipa: u32) -> Option<u64> {
    if (ipa as usize) + 4 <= crate::guest_mem::ROM_SIZE {
        return Some(crate::guest_mem::rom_host_pa() + ipa as u64);
    }
    let ram_base = crate::guest_mem::RAM_IPA_BASE as usize;
    if (ipa as usize) >= ram_base
        && (ipa as usize) + 4 <= ram_base + crate::guest_mem::RAM_SIZE
    {
        return Some(
            crate::guest_mem::ram_host_pa() + (ipa as u64 - ram_base as u64),
        );
    }
    None
}

/// Read the original (pre-patch) instruction at `ipa`, falling back to
/// the live ROM/RAM word if no original is recorded. Exposed for sister
/// modules that need to read instructions in patch-aware contexts.
pub fn read_insn_original_first(ipa: u32) -> Option<u32> {
    code_read_word_original_first(ipa)
}
