//! Lazy inline-stub installer for SA-1100 unaligned-LDR rotate semantics.
//!
//! Companion to `unaligned.rs`. The EL2 emulator there handles the
//! correctness side: every alignment-fault DABT trampoline HVCs into
//! `unaligned::handle_align_fault`, which decodes the faulting LDR/STR
//! and applies the SA-1100 rotate-LDR result. That works, but the
//! per-fault round-trip is the dominant trap source in steady-state UI
//! rendering (~3.4M faults/sec at iter-56 cold boot).
//!
//! This module installs an in-ROM inline stub at each faulting PC the
//! first time we see it, so subsequent executions of the same word LDR
//! run natively in AArch32 without trapping. The mechanism is the same
//! as `shadow_stub` (B-to-stub, body, B-back-to-`orig_pc + 4`, in the
//! shared SBA stub pool); only the stub body differs.
//!
//! Stub body for `LDR{cond} Rt, [Rn, ±#imm]` or `[Rn, ±Rm, shift]`:
//!
//!   slot 0:  ADD/SUB sea, Rn, <off>      ; (or 2-step ADD if imm > 0xFF)
//!   slot 1:  ADD/SUB sea, sea, <off_lo>  ; or NOP
//!   slot 2:  AND     ssh, sea, #3
//!   slot 3:  BIC     sea, sea, #3
//!   slot 4:  LDR{c}  Rt, [sea]
//!   slot 5:  LSL     ssh, ssh, #3
//!   slot 6:  MOV{c}  Rt, Rt, ROR ssh
//!   slot 7:  B       orig_pc + 4
//!   slots 8..15: NOP
//!
//! Aligned EAs see `ssh = 0` → ROR-by-0 = identity, so a single body
//! handles aligned and unaligned cases (no runtime branch). The
//! data-processing ops use S=0 throughout, so NZCV is preserved without
//! an MRS/MSR pair. Conditional LDR/MOV match the original cond, so a
//! cond-fail leaves Rt untouched (matches the original LDR's
//! architectural behaviour). The unconditional ADD/SUB/AND/BIC/LSL
//! clobber the liveness-proved-dead scratches `sea` and `ssh`
//! regardless of cond — fine, they are dead at orig_pc+4 in both paths.
//!
//! Eligibility (anything not eligible falls through to the existing
//! EL2 emulator, no harm done):
//!   - LDR only. STR-unaligned is architecturally UNPREDICTABLE on
//!     ARMv4; SA-1100 strict-aligns. The Newton ROM doesn't actually
//!     issue unaligned STRs in steady-state, so we punt to EL2.
//!   - Pre-index, no writeback (P=1, W=0).
//!   - No PC operand for Rt, Rn, or Rm.
//!   - `orig_pc` must be in the ROM aperture (B-instruction reach to
//!     the stub pool). RAM-resident faulting PCs go through EL2.
//!   - Imm offset ≤ 0xFFF (always true for the ARM A1 form, but the
//!     2-step ADD path needs the high/low split to encode).
//!   - Liveness analysis must find 2 dead scratches in {R0..R3, R12}
//!     that aren't operands of the access.
//!
//! Lazy install means partial coverage already wins. Sites that fail
//! eligibility keep paying the EL2 round-trip; sites that pass pay it
//! exactly once before going inline.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::shadow_stub::{
    alloc_stub_slot, encode, install_inline_at, live_regs_at,
    read_insn_original_first, SBA_STUB_WORDS,
};
use crate::unaligned::{decode, Decoded, OffsetForm};

/// Counter of stubs installed by this module.
static STUBS_INSTALLED: AtomicU32 = AtomicU32::new(0);

/// Counter of install attempts that were rejected (any reason).
static STUBS_REJECTED: AtomicU32 = AtomicU32::new(0);

// Per-rejection-reason counters.
static REJ_NOT_LDR: AtomicU32 = AtomicU32::new(0);
static REJ_OPERAND_PC: AtomicU32 = AtomicU32::new(0);
static REJ_WRITEBACK: AtomicU32 = AtomicU32::new(0);
static REJ_OFFSET_IMM_TOO_BIG: AtomicU32 = AtomicU32::new(0);
static REJ_NO_DEAD_SCRATCHES: AtomicU32 = AtomicU32::new(0);
static REJ_OUTSIDE_ROM: AtomicU32 = AtomicU32::new(0);
static REJ_POOL_FULL: AtomicU32 = AtomicU32::new(0);
static REJ_INSTALL_FAIL: AtomicU32 = AtomicU32::new(0);
static REJ_DECODE_FAIL: AtomicU32 = AtomicU32::new(0);

// Snapshot from the previous `log_stats` call, for windowed deltas.
// Single-threaded EL2 access, so plain `static mut` (read-modify-write
// inside the lone `log_stats` site) is fine.
#[cfg(feature = "log_traps")]
struct StatsSnapshot {
    installed: u32,
    rejected: u32,
    rej_not_ldr: u32,
    rej_operand_pc: u32,
    rej_writeback: u32,
    rej_offset_imm: u32,
    rej_no_dead: u32,
    rej_outside_rom: u32,
    rej_pool_full: u32,
    rej_install_fail: u32,
    rej_decode_fail: u32,
}
#[cfg(feature = "log_traps")]
static mut LAST_STATS: StatsSnapshot = StatsSnapshot {
    installed: 0, rejected: 0,
    rej_not_ldr: 0, rej_operand_pc: 0, rej_writeback: 0,
    rej_offset_imm: 0, rej_no_dead: 0, rej_outside_rom: 0,
    rej_pool_full: 0, rej_install_fail: 0, rej_decode_fail: 0,
};

// Misra-Gries top-K of recently-rejected (no_dead_scratches) PCs. The
// dominant rejection reason in practice — the kernel emits LDRs at
// PCs where R0..R3 and R12 are all live at PC+4, so the install
// picker can't find scratches. Reset every dump so the top-K reflects
// the current window, not a since-boot accumulation.
const REJ_TOPK: usize = 16;
struct RejTopK {
    keys: [u32; REJ_TOPK],
    counts: [u32; REJ_TOPK],
}
impl RejTopK {
    const fn new() -> Self {
        Self { keys: [0; REJ_TOPK], counts: [0; REJ_TOPK] }
    }
    fn record(&mut self, key: u32) {
        for i in 0..REJ_TOPK {
            if self.counts[i] > 0 && self.keys[i] == key {
                self.counts[i] = self.counts[i].saturating_add(1);
                return;
            }
        }
        for i in 0..REJ_TOPK {
            if self.counts[i] == 0 {
                self.keys[i] = key;
                self.counts[i] = 1;
                return;
            }
        }
        for c in &mut self.counts {
            *c = c.saturating_sub(1);
        }
    }
    #[cfg(feature = "log_traps")]
    fn snapshot_sorted(&self) -> [(u32, u32); REJ_TOPK] {
        let mut out = [(0u32, 0u32); REJ_TOPK];
        for i in 0..REJ_TOPK {
            out[i] = (self.keys[i], self.counts[i]);
        }
        for k in 0..REJ_TOPK {
            let mut best = k;
            for j in (k + 1)..REJ_TOPK {
                if out[j].1 > out[best].1 {
                    best = j;
                }
            }
            out.swap(k, best);
        }
        out
    }
    #[cfg(feature = "log_traps")]
    fn reset(&mut self) {
        for i in 0..REJ_TOPK {
            self.keys[i] = 0;
            self.counts[i] = 0;
        }
    }
}
static mut REJ_NO_DEAD_PCS: RejTopK = RejTopK::new();

/// Detailed-log budget: log every install up to this count, then
/// summary-only. Matches `unaligned::LOG_FIRST`'s convention.
const LOG_FIRST_INSTALLS: u32 = 40;

/// After the first-N detailed log lines, emit a periodic stats line
/// every N installs. 0 disables the periodic line.
const PERIODIC_STATS_EVERY: u32 = 100;

/// Try to install an inline stub at `faulting_pc`. The EL2 emulator
/// must still complete *this* fault; the installed stub takes effect
/// on the next execution of `faulting_pc`.
///
/// All failure paths bump a per-reason counter and return false. The
/// caller (`unaligned::handle_align_fault`) doesn't care — partial
/// coverage is fine.
pub fn try_install_at(faulting_pc: u32) -> bool {
    // The faulting PC must be in the Newton ROM/REX region — i.e.
    // strictly below the tracer trampoline pool (0x00900000) and the
    // SBA stub pool (0x00E00000). Patching code in those pools would
    // tangle our stub mechanism with the tracer's, or modify the SBA
    // pool itself. Newton 717006 + REX fits in the first 8-9 MiB of
    // the ROM aperture.
    const ALIGN_INLINE_PC_LIMIT: u32 = 0x0090_0000;
    if faulting_pc & 3 != 0 || faulting_pc >= ALIGN_INLINE_PC_LIMIT {
        REJ_OUTSIDE_ROM.fetch_add(1, Ordering::Relaxed);
        STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    let insn = match read_insn_original_first(faulting_pc) {
        Some(w) => w,
        None => {
            REJ_DECODE_FAIL.fetch_add(1, Ordering::Relaxed);
            STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };

    let d = match decode(insn) {
        Some(d) => d,
        None => {
            REJ_DECODE_FAIL.fetch_add(1, Ordering::Relaxed);
            STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };

    if !d.load {
        REJ_NOT_LDR.fetch_add(1, Ordering::Relaxed);
        STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    if !d.p || d.w {
        REJ_WRITEBACK.fetch_add(1, Ordering::Relaxed);
        STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    if d.rt == 15 || d.rn == 15 {
        REJ_OPERAND_PC.fetch_add(1, Ordering::Relaxed);
        STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if let OffsetForm::Reg { rm, .. } = d.offset {
        if rm == 15 {
            REJ_OPERAND_PC.fetch_add(1, Ordering::Relaxed);
            STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    }

    let (sea, ssh) = match pick_scratches(&d, faulting_pc) {
        Some(p) => p,
        None => {
            REJ_NO_DEAD_SCRATCHES.fetch_add(1, Ordering::Relaxed);
            STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: single-threaded; only access site besides the
            // dump's snapshot+reset is this line.
            unsafe {
                (*core::ptr::addr_of_mut!(REJ_NO_DEAD_PCS)).record(faulting_pc);
            }
            return false;
        }
    };

    let (slot_idx, stub_ipa) = match alloc_stub_slot() {
        Some(s) => s,
        None => {
            REJ_POOL_FULL.fetch_add(1, Ordering::Relaxed);
            STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };

    let words = match build_stub_words(&d, faulting_pc, stub_ipa, sea, ssh) {
        Ok(ws) => ws,
        Err(_) => {
            REJ_OFFSET_IMM_TOO_BIG.fetch_add(1, Ordering::Relaxed);
            STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };

    if let Err(_e) = install_inline_at(faulting_pc, stub_ipa, &words) {
        REJ_INSTALL_FAIL.fetch_add(1, Ordering::Relaxed);
        STUBS_REJECTED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    let n = STUBS_INSTALLED.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= LOG_FIRST_INSTALLS {
        crate::kprintln!(
            "unaligned_inline[{}]: installed (slot_ix={}, PC={:#010x}, sea=R{} ssh=R{})",
            n, slot_idx, faulting_pc, sea, ssh
        );
    } else if PERIODIC_STATS_EVERY != 0 && n % PERIODIC_STATS_EVERY == 0 {
        crate::kprintln!(
            "unaligned_inline: {} stubs installed (latest PC={:#010x})",
            n, faulting_pc
        );
    }
    true
}

/// Liveness-aware scratch picker. Two regs in {R12, R0..R3} that are
/// not operands of the access AND are dead at `orig_pc + 4`.
fn pick_scratches(d: &Decoded, orig_pc: u32) -> Option<(u32, u32)> {
    const CANDIDATES: &[u32] = &[12, 0, 1, 2, 3];
    let live = live_regs_at(orig_pc.wrapping_add(4), 32);
    let mut operand_mask: u16 = (1u16 << d.rt) | (1u16 << d.rn);
    if let OffsetForm::Reg { rm, .. } = d.offset {
        operand_mask |= 1u16 << rm;
    }
    let mut picks = [u32::MAX; 2];
    let mut n = 0;
    for &r in CANDIDATES {
        let rmask: u16 = 1u16 << r;
        if rmask & operand_mask != 0 {
            continue;
        }
        if rmask & live != 0 {
            continue;
        }
        picks[n] = r;
        n += 1;
        if n == 2 {
            return Some((picks[0], picks[1]));
        }
    }
    None
}

/// Build the stub words. Layout fits in `SBA_STUB_WORDS == 16` slots;
/// trailing slots are NOPs.
fn build_stub_words(
    d: &Decoded, orig_pc: u32, stub_ipa: u32, sea: u32, ssh: u32,
) -> Result<[u32; SBA_STUB_WORDS], &'static str> {
    let mut out = [encode::nop(); SBA_STUB_WORDS];

    // Slots 0/1: compute EA into `sea`.
    let (slot0, slot1) = match d.offset {
        OffsetForm::Imm(imm) => encode_ea_imm(d.u, sea, d.rn, imm)?,
        OffsetForm::Reg { rm, shift_type, shift_amount } => {
            let s0 = if d.u {
                encode::add_reg_shifted(
                    encode::AL, sea, d.rn, rm, shift_type, shift_amount,
                )
            } else {
                encode::sub_reg_shifted(
                    encode::AL, sea, d.rn, rm, shift_type, shift_amount,
                )
            };
            (s0, encode::nop())
        }
    };
    out[0] = slot0;
    out[1] = slot1;

    out[2] = encode_and_imm(encode::AL, ssh, sea, 3);
    out[3] = encode_bic_imm(encode::AL, sea, sea, 3);
    out[4] = encode_ldr_zero(d.cond, d.rt, sea);
    out[5] = encode_mov_reg_lsl_imm(encode::AL, ssh, ssh, 3);
    out[6] = encode_mov_reg_ror_reg(d.cond, d.rt, d.rt, ssh);

    // Slot 7: B orig_pc + 4. Slot's PC is `stub_ipa + 7*4`.
    let slot7_pc = stub_ipa.wrapping_add(7 * 4);
    let target = orig_pc.wrapping_add(4);
    let b = encode::b(slot7_pc, target).ok_or("B back out of imm24 range")?;
    out[7] = b;

    Ok(out)
}

/// Encode AND Rd, Rn, #imm — with cond. S=0.
fn encode_and_imm(cond: u32, rd: u32, rn: u32, imm: u32) -> u32 {
    let imm12 = encode::arm_imm12(imm).expect("imm not encodable as modified-imm");
    (cond << 28) | 0x0200_0000 | (rn << 16) | (rd << 12) | (imm12 & 0xFFF)
}

/// Encode BIC Rd, Rn, #imm — with cond. S=0.
fn encode_bic_imm(cond: u32, rd: u32, rn: u32, imm: u32) -> u32 {
    let imm12 = encode::arm_imm12(imm).expect("imm not encodable as modified-imm");
    (cond << 28) | 0x03C0_0000 | (rn << 16) | (rd << 12) | (imm12 & 0xFFF)
}

/// Encode LDR{cond} Rt, [Rn] — pre-index, U=1, W=0, P=1, imm=0.
fn encode_ldr_zero(cond: u32, rt: u32, rn: u32) -> u32 {
    (cond << 28) | 0x0590_0000 | (rn << 16) | (rt << 12)
}

/// Encode MOV{cond} Rd, Rm, LSL #shamt — register form, S=0, type=00.
fn encode_mov_reg_lsl_imm(cond: u32, rd: u32, rm: u32, shamt: u32) -> u32 {
    (cond << 28)
        | 0x01A0_0000
        | (rd << 12)
        | ((shamt & 0x1F) << 7)
        | (rm & 0xF)
}

/// Encode MOV{cond} Rd, Rm, ROR Rs — register-shifted-register form,
/// S=0, type=11 (ROR).
fn encode_mov_reg_ror_reg(cond: u32, rd: u32, rm: u32, rs: u32) -> u32 {
    (cond << 28)
        | 0x01A0_0000
        | (rd << 12)
        | ((rs & 0xF) << 8)
        | (3 << 5)
        | (1 << 4)
        | (rm & 0xF)
}

/// Encode the EA-compute instruction(s) for an immediate offset.
/// Single ADD/SUB if `imm` is encodable as a modified-immediate,
/// otherwise (high & 0xF00, low & 0xFF) — both always encodable.
fn encode_ea_imm(
    u: bool, sea: u32, rn: u32, imm: u32,
) -> Result<(u32, u32), &'static str> {
    if imm > 0xFFF {
        return Err("imm > 0xFFF");
    }
    let nop = encode::nop();
    if let Some(enc) = encode::arm_imm12(imm) {
        let s0 = if u {
            encode::add_imm(encode::AL, sea, rn, enc)
        } else {
            encode::sub_imm(encode::AL, sea, rn, enc)
        };
        return Ok((s0, nop));
    }
    let high = imm & 0xF00;
    let low = imm & 0xFF;
    let high_enc = encode::arm_imm12(high).ok_or("high imm not encodable")?;
    let low_enc = encode::arm_imm12(low).ok_or("low imm not encodable")?;
    let s0 = if u {
        encode::add_imm(encode::AL, sea, rn, high_enc)
    } else {
        encode::sub_imm(encode::AL, sea, rn, high_enc)
    };
    let s1 = if u {
        encode::add_imm(encode::AL, sea, sea, low_enc)
    } else {
        encode::sub_imm(encode::AL, sea, sea, low_enc)
    };
    Ok((s0, s1))
}

/// Public stats dump — called from `trap_hist::dump_and_reset` every
/// ~2 s of wall time. Prints the since-boot cumulative `installed=` and
/// `rejected=` totals on a header line, then a "Δ since last dump" line
/// for the per-reason counters so the reader can tell at a glance
/// whether new failures are still piling up or things have plateau'd.
/// Finally, dumps the top-K guest PCs that hit the `no_dead_scratches`
/// rejection in this window — the picker can't find two dead scratches
/// in {R0..R3, R12} for these LDR sites, so they keep paying the EL2
/// trap on every fire.
#[cfg(feature = "log_traps")]
pub fn log_stats() {
    let installed = STUBS_INSTALLED.load(Ordering::Relaxed);
    let rejected = STUBS_REJECTED.load(Ordering::Relaxed);
    let decode_fail = REJ_DECODE_FAIL.load(Ordering::Relaxed);
    let not_ldr = REJ_NOT_LDR.load(Ordering::Relaxed);
    let operand_pc = REJ_OPERAND_PC.load(Ordering::Relaxed);
    let writeback = REJ_WRITEBACK.load(Ordering::Relaxed);
    let no_dead = REJ_NO_DEAD_SCRATCHES.load(Ordering::Relaxed);
    let outside_rom = REJ_OUTSIDE_ROM.load(Ordering::Relaxed);
    let pool_full = REJ_POOL_FULL.load(Ordering::Relaxed);
    let install_fail = REJ_INSTALL_FAIL.load(Ordering::Relaxed);
    let imm_too_big = REJ_OFFSET_IMM_TOO_BIG.load(Ordering::Relaxed);

    // SAFETY: single-threaded EL2 dump, no overlapping caller.
    let prev = unsafe {
        let p = core::ptr::addr_of_mut!(LAST_STATS);
        let snap = StatsSnapshot {
            installed: (*p).installed,
            rejected: (*p).rejected,
            rej_not_ldr: (*p).rej_not_ldr,
            rej_operand_pc: (*p).rej_operand_pc,
            rej_writeback: (*p).rej_writeback,
            rej_offset_imm: (*p).rej_offset_imm,
            rej_no_dead: (*p).rej_no_dead,
            rej_outside_rom: (*p).rej_outside_rom,
            rej_pool_full: (*p).rej_pool_full,
            rej_install_fail: (*p).rej_install_fail,
            rej_decode_fail: (*p).rej_decode_fail,
        };
        (*p).installed = installed;
        (*p).rejected = rejected;
        (*p).rej_not_ldr = not_ldr;
        (*p).rej_operand_pc = operand_pc;
        (*p).rej_writeback = writeback;
        (*p).rej_offset_imm = imm_too_big;
        (*p).rej_no_dead = no_dead;
        (*p).rej_outside_rom = outside_rom;
        (*p).rej_pool_full = pool_full;
        (*p).rej_install_fail = install_fail;
        (*p).rej_decode_fail = decode_fail;
        snap
    };

    let d_installed = installed.wrapping_sub(prev.installed);
    let d_rejected = rejected.wrapping_sub(prev.rejected);
    let d_decode = decode_fail.wrapping_sub(prev.rej_decode_fail);
    let d_not_ldr = not_ldr.wrapping_sub(prev.rej_not_ldr);
    let d_operand_pc = operand_pc.wrapping_sub(prev.rej_operand_pc);
    let d_writeback = writeback.wrapping_sub(prev.rej_writeback);
    let d_no_dead = no_dead.wrapping_sub(prev.rej_no_dead);
    let d_outside_rom = outside_rom.wrapping_sub(prev.rej_outside_rom);
    let d_pool_full = pool_full.wrapping_sub(prev.rej_pool_full);
    let d_install_fail = install_fail.wrapping_sub(prev.rej_install_fail);
    let d_imm = imm_too_big.wrapping_sub(prev.rej_offset_imm);

    crate::kprintln!(
        "unaligned_inline: installed={} (+{}) rejected={} (+{}) since boot",
        installed, d_installed, rejected, d_rejected
    );
    if d_rejected != 0 {
        crate::kprintln!(
            "  Δ window: decode={} not_ldr={} operand_pc={} writeback={} \
             no_dead_scratches={} outside_rom={} pool_full={} install_fail={} \
             imm_too_big={}",
            d_decode, d_not_ldr, d_operand_pc, d_writeback,
            d_no_dead, d_outside_rom, d_pool_full, d_install_fail, d_imm,
        );
    }

    // SAFETY: single-threaded; the only other access to REJ_NO_DEAD_PCS
    // is the record site in `try_install_at`.
    let pcs = unsafe {
        let p = core::ptr::addr_of_mut!(REJ_NO_DEAD_PCS);
        let snap = (*p).snapshot_sorted();
        (*p).reset();
        snap
    };
    if pcs[0].1 > 0 {
        crate::kprintln!("  no_dead_scratches PC top (window):");
        let print_top = 8.min(REJ_TOPK);
        for k in 0..print_top {
            let (pc, c) = pcs[k];
            if c == 0 { break; }
            crate::kprintln!("    PC={:#010x}: >={}", pc, c);
        }
    }
}

// =======================================================================
// Compile-time encoder checks
// =======================================================================
//
// Verifies the bit patterns we emit match the ARM A1 encodings of the
// canonical insns. Cross-check with the disassembler if you suspect a
// bug:  printf '\x03\x00\x03\xE2' | aarch64-elf-objdump -b binary \
//       -m arm -D - → "and r0, r3, #3".
//
// Compile-time asserts run at every build; no test runner required.
const fn _check_encoders() {
    // and r0, r3, #3 → 0xE2030003
    // (encode_and_imm(AL, 0, 3, 3) — but const fn can't call non-const
    // funcs, so reproduce the encoding inline.)
    let and_r0_r3_3: u32 = (0xE << 28) | 0x0200_0000 | (3 << 16) | (0 << 12) | 3;
    assert!(and_r0_r3_3 == 0xE203_0003);

    // bic r3, r3, #3 → 0xE3C33003
    let bic_r3_r3_3: u32 = (0xE << 28) | 0x03C0_0000 | (3 << 16) | (3 << 12) | 3;
    assert!(bic_r3_r3_3 == 0xE3C3_3003);

    // ldr r0, [r3] → 0xE5930000
    let ldr_r0_r3: u32 = (0xE << 28) | 0x0590_0000 | (3 << 16) | (0 << 12);
    assert!(ldr_r0_r3 == 0xE593_0000);

    // mov r0, r0, lsl #3 → 0xE1A00180
    let mov_lsl: u32 = (0xE << 28) | 0x01A0_0000 | (0 << 12) | ((3 & 0x1F) << 7);
    assert!(mov_lsl == 0xE1A0_0180);

    // mov r0, r0, ror r1 → 0xE1A00170
    let mov_ror: u32 = (0xE << 28) | 0x01A0_0000 | (0 << 12) | (1 << 8) | (3 << 5) | (1 << 4);
    assert!(mov_ror == 0xE1A0_0170);

    // movne r0, r0, ror r1 → 0x11A00170
    let mov_ror_ne: u32 = (0x1 << 28) | 0x01A0_0000 | (0 << 12) | (1 << 8) | (3 << 5) | (1 << 4);
    assert!(mov_ror_ne == 0x11A0_0170);
}
const _: () = _check_encoders();
