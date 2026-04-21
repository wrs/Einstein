//! Shadow-stub mechanism for byte/halfword access on a BE-32 guest
//! running under a little-endian host.
//!
//! The Newton ROM is BE-32 "word-invariant": aligned word accesses are
//! identical to LE (word swapped at load time), but byte and halfword
//! accesses use a different byte lane:
//!
//!   BE-32 LDRB at addr A  →  phys[A ^ 3]
//!   BE-32 LDRH at addr A  →  halfword at phys[A ^ 2]
//!
//! Because our ROM backing is byteswapped per word, a native `LDRB` at
//! A on the A53 reads phys[A] — the wrong byte. To fix this we scan a
//! designated code range, and for each byte/halfword access instruction
//! at PC X:
//!
//!   1. Emit an out-of-line stub at `shadow_X` in a separate pool. The
//!      stub computes the effective address, optionally XORs it with
//!      3 (byte) or 2 (halfword), does the load/store, updates Rn on
//!      pre/post-index, and branches back to X+4.
//!   2. Replace the instruction at X with `Bcc shadow_X`, preserving
//!      the original condition code. Unconditional `B` is used for
//!      AL-condition instructions.
//!
//! `B` reaches ±32 MiB, so the stub pool must live within that window
//! of every patched site. We install it at guest IPA 0x01800000 (24
//! MiB), reachable from anything in the 0..16 MiB ROM region.
//!
//! MMIO skip: the BE-32 XOR only applies to real memory (RAM / ROM /
//! FB / flash). Accesses to MMIO registers must not be XOR'd — the
//! Newton kernel writes typed bytes straight into MMIO byte lanes.
//! Each stub therefore runtime-checks the effective address against
//! `XOR_LIMIT` and branches past the XOR when the address is higher.
//!
//! Scope: this is an MVP. It handles LDRB/STRB/LDRH/STRH/LDRSB/LDRSH
//! in immediate, register-offset, and register-offset-with-shift
//! forms, including pre-index, post-index, and writeback variants.
//! SWPB is skipped (rare; separate path). PC as source or destination
//! is flagged as unsupported.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::kprintln;

/// IPA where the stub pool is mapped into the guest's address space.
/// Chosen to sit inside the ±32 MiB reach of any ARM `B` instruction
/// from anywhere in the 0..16 MiB ROM region.
pub const STUB_POOL_IPA: u32 = 0x0180_0000;

/// Stub-pool size. 2 MiB = 1 stage-2 block descriptor.
pub const STUB_POOL_SIZE: usize = 2 * 1024 * 1024;

/// Addresses < XOR_LIMIT are treated as real memory (XOR applied);
/// addresses >= XOR_LIMIT are treated as MMIO and passed through.
/// Chosen to cover everything in the Newton IPA map below flash bank 1.
pub const XOR_LIMIT: u32 = 0x1000_0000;

/// Fixed bytes per stub slot. 16 words × 4 = 64 bytes. Chosen to cover
/// the worst-case encoding (register-offset with shift, writeback,
/// signed halfword load).
pub const STUB_SLOT_SIZE: usize = 64;

/// Pool capacity — number of stubs that fit.
pub const STUB_POOL_CAPACITY: usize = STUB_POOL_SIZE / STUB_SLOT_SIZE;

#[repr(C, align(0x200000))]
struct StubPool([u8; STUB_POOL_SIZE]);

static mut STUB_POOL: StubPool = StubPool([0; STUB_POOL_SIZE]);

/// How many slots have been handed out so far.
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Host physical base of the stub pool backing store.
pub fn pool_host_pa() -> u64 {
    addr_of_mut!(STUB_POOL) as u64
}

/// Summary statistics returned by `patch_code_range`.
#[derive(Default, Debug)]
pub struct PatchStats {
    pub words_scanned: usize,
    pub patched: usize,
    pub skipped_pc_operand: usize,
    pub ldrb_strb: usize,
    pub ldrh_strh: usize,
    pub ldrsb_ldrsh: usize,
}

/// Decoded access kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessKind {
    /// LDRB (load unsigned byte) — B=1, L=1 in data-proc-imm/reg form.
    Ldrb,
    /// STRB (store byte) — B=1, L=0.
    Strb,
    /// LDRH (load unsigned halfword) — extra load/store, bits[7:4]=1011, L=1.
    Ldrh,
    /// STRH (store halfword) — bits[7:4]=1011, L=0.
    Strh,
    /// LDRSB (load signed byte) — bits[7:4]=1101, L=1.
    Ldrsb,
    /// LDRSH (load signed halfword) — bits[7:4]=1111, L=1.
    Ldrsh,
}

impl AccessKind {
    /// The XOR applied to the effective address for BE-32 compatibility:
    /// 3 for byte accesses, 2 for halfword, 1 for nothing (halfword-aligned,
    /// so XOR 2 flips bit 1).
    fn xor_mask(self) -> u32 {
        match self {
            AccessKind::Ldrb | AccessKind::Strb | AccessKind::Ldrsb => 3,
            AccessKind::Ldrh | AccessKind::Strh | AccessKind::Ldrsh => 2,
        }
    }

}

/// Offset form.
#[derive(Clone, Copy, Debug)]
enum OffsetForm {
    /// Immediate offset, `imm` is unsigned magnitude.
    Imm { imm: u32 },
    /// Register offset `Rm`, with an optional LSL/LSR/ASR/ROR shift.
    /// `shift_type` is the 2-bit ARM shift type, `shift_amount` is 0..31.
    Reg { rm: u32, shift_type: u32, shift_amount: u32 },
}

/// Fully decoded byte/halfword access.
#[derive(Clone, Copy, Debug)]
struct Decoded {
    kind: AccessKind,
    cond: u32,
    rn: u32,
    rt: u32,
    offset: OffsetForm,
    p: bool, // pre-index when true; post-index when false
    u: bool, // add offset when true, subtract when false
    w: bool, // writeback when true (relevant for P=1)
}

/// Attempt to decode a byte/halfword access. Returns `Some(Decoded)`
/// for the encodings we handle and `None` for everything else (including
/// word loads/stores, unrelated ops, and encodings we explicitly skip).
fn decode(insn: u32) -> Option<Decoded> {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        // Unconditional-class encodings (NEON, PLD, ...). We don't
        // touch those in this MVP.
        return None;
    }

    // Form 1: data-processing-immediate / register LDR/STR with B=1.
    //   Immediate: cond 010 P U B W L Rn Rt imm12
    //   Register : cond 011 P U B W L Rn Rt imm5 type 0 Rm
    // B=1 distinguishes byte from word. Bit 4 must be 0 in the register form.
    if (insn & 0x0E00_0000) == 0x0400_0000
        && (insn & (1 << 22)) != 0
    {
        // Immediate form, B=1.
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let imm = insn & 0xFFF;
        return Some(Decoded {
            kind: if l { AccessKind::Ldrb } else { AccessKind::Strb },
            cond,
            rn,
            rt,
            offset: OffsetForm::Imm { imm },
            p, u, w,
        });
    }
    if (insn & 0x0E00_0010) == 0x0600_0000
        && (insn & (1 << 22)) != 0
    {
        // Register form, B=1, bit 4 = 0.
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let shift_amount = (insn >> 7) & 0x1F;
        let shift_type = (insn >> 5) & 0x3;
        let rm = insn & 0xF;
        return Some(Decoded {
            kind: if l { AccessKind::Ldrb } else { AccessKind::Strb },
            cond,
            rn,
            rt,
            offset: OffsetForm::Reg { rm, shift_type, shift_amount },
            p, u, w,
        });
    }

    // Form 2: extra load/store (halfword / signed byte / signed halfword).
    //   cond 000 P U I W L Rn Rt imm4h 1 op1 op2 1 imm4l   (I = immediate flag)
    // We key on bits[27:25]=000, bit 7 = 1, bit 4 = 1, and bits[6:5] (op):
    //   01  -> H  (halfword, unsigned)     — 1011
    //   10  -> SB (signed byte, load only) — 1101
    //   11  -> SH (signed halfword)         — 1111
    // L=1 for loads; STRH uses L=0 with op=01; LDRSB/LDRSH are load-only
    // so L must be 1 for those.
    if (insn & 0x0E00_0090) == 0x0000_0090 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let i = (insn >> 22) & 1 != 0; // 1 = immediate, 0 = register
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let op = (insn >> 5) & 0x3;

        // op=00 is SWP/SWPB/LDREX/STREX/... — not us. Skip.
        if op == 0 {
            return None;
        }

        let kind = match (op, l) {
            (0b01, true)  => AccessKind::Ldrh,
            (0b01, false) => AccessKind::Strh,
            (0b10, true)  => AccessKind::Ldrsb,
            (0b10, false) => return None,  // LDRD (double word) — skip
            (0b11, true)  => AccessKind::Ldrsh,
            (0b11, false) => return None,  // STRD — skip
            _ => return None,
        };

        let offset = if i {
            let imm = ((insn >> 4) & 0xF0) | (insn & 0xF);
            OffsetForm::Imm { imm }
        } else {
            // Register form: imm4h is SBZ, low nibble = Rm.
            let rm = insn & 0xF;
            OffsetForm::Reg { rm, shift_type: 0, shift_amount: 0 }
        };

        return Some(Decoded {
            kind, cond, rn, rt, offset, p, u, w,
        });
    }

    None
}

/// Pick a scratch register that is not in `{Rt, Rn, optional Rm, PC=15, SP=13}`.
/// Returns the lowest free register number we're willing to clobber.
fn pick_scratch(d: &Decoded) -> u32 {
    let rm = if let OffsetForm::Reg { rm, .. } = d.offset { Some(rm) } else { None };
    for candidate in &[12u32, 14, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
        let c = *candidate;
        if c == d.rt || c == d.rn { continue; }
        if let Some(rm) = rm { if c == rm { continue; } }
        return c;
    }
    unreachable!("should always find a scratch register");
}

/// Encode `STR Rt, [SP, #-4]!` (pre-indexed, writeback).
fn enc_push(rt: u32) -> u32 {
    // cond=E (AL) 010 P=1 U=0 B=0 W=1 L=0 Rn=SP(13) Rt imm12=4
    0xE52D_0004 | (rt << 12)
}

/// Encode `LDR Rt, [SP], #4` (post-indexed).
fn enc_pop(rt: u32) -> u32 {
    // cond=E 010 P=0 U=1 B=0 W=0 L=1 Rn=SP(13) Rt imm12=4
    0xE49D_0004 | (rt << 12)
}

/// `MOV Rd, Rm`  — simple register move (cond=AL).
fn enc_mov_reg(rd: u32, rm: u32) -> u32 {
    0xE1A0_0000 | (rd << 12) | rm
}

/// `ADD Rd, Rn, #imm8`  (modified-immediate, rotate=0: imm8 directly).
/// Only valid for `imm8 <= 0xFF`; use `enc_add_imm12_split` for larger.
fn enc_add_imm8(rd: u32, rn: u32, imm8: u32) -> u32 {
    assert!(imm8 <= 0xFF, "imm8 overflow: {:#x}", imm8);
    0xE280_0000 | (rn << 16) | (rd << 12) | imm8
}

/// `ADD Rd, Rn, #(imm8 ROR (2*rot4))`.
fn enc_add_imm_rot(rd: u32, rn: u32, imm8: u32, rot4: u32) -> u32 {
    assert!(imm8 <= 0xFF);
    assert!(rot4 <= 0xF);
    0xE280_0000 | (rn << 16) | (rd << 12) | (rot4 << 8) | imm8
}

/// `SUB Rd, Rn, #imm8`.
fn enc_sub_imm8(rd: u32, rn: u32, imm8: u32) -> u32 {
    assert!(imm8 <= 0xFF);
    0xE240_0000 | (rn << 16) | (rd << 12) | imm8
}

fn enc_sub_imm_rot(rd: u32, rn: u32, imm8: u32, rot4: u32) -> u32 {
    assert!(imm8 <= 0xFF);
    assert!(rot4 <= 0xF);
    0xE240_0000 | (rn << 16) | (rd << 12) | (rot4 << 8) | imm8
}

/// `ADD Rd, Rn, Rm, <shift> #amount`.
fn enc_add_reg_shift(rd: u32, rn: u32, rm: u32, shift_type: u32, amount: u32) -> u32 {
    0xE080_0000 | (rn << 16) | (rd << 12)
        | (amount << 7) | (shift_type << 5) | rm
}

/// `SUB Rd, Rn, Rm, <shift> #amount`.
fn enc_sub_reg_shift(rd: u32, rn: u32, rm: u32, shift_type: u32, amount: u32) -> u32 {
    0xE040_0000 | (rn << 16) | (rd << 12)
        | (amount << 7) | (shift_type << 5) | rm
}

/// `EOR Rd, Rn, #imm12` (imm must fit in 8-bit modified-imm with rotate=0).
fn enc_eor_imm(rd: u32, rn: u32, imm: u32) -> u32 {
    assert!(imm <= 0xFF);
    0xE220_0000 | (rn << 16) | (rd << 12) | imm
}

/// `LDR Rt, [pc, #off]`. `off` is the offset from (current+8) to the literal,
/// must be -4095..4095.
fn enc_ldr_pc_lit(rt: u32, off: i32) -> u32 {
    let u = off >= 0;
    let mag = off.unsigned_abs();
    assert!(mag <= 0xFFF);
    // cond=E 010 P=1 U W=0 L=1 Rn=PC(15) Rt imm12
    let u_bit = if u { 1u32 } else { 0 };
    0xE510_0000 | (u_bit << 23) | (15u32 << 16) | (rt << 12) | mag
}

/// `LDR pc, [pc, #off]` — branch to absolute target held in a literal pool.
fn enc_ldr_pc_pc_lit(off: i32) -> u32 {
    enc_ldr_pc_lit(15, off)
}

/// `CMP Rn, #imm8, rotate`. For our narrow use, we need constants like
/// `0x1000_0000` — encoded as imm8=0x10, rot=0 (no rotate) → value 0x10.
/// Plus shift... actually `0x1000_0000 = 0x10 ror 8`. Modified-imm
/// encoding: `imm12 = (rot4 << 8) | imm8`, value = ror(imm8, 2*rot4).
/// For 0x1000_0000: value = imm8 ror (2*rot4). With imm8=0x10, rot4=?,
/// ror(0x10, 2*rot4) = 0x1000_0000. 0x10 = 0b0001_0000. We want 0001_0000
/// rotated right so the 1-bit ends up at bit 28. That's a right rotation
/// of 32-28 = 4 ... no, bit 4 -> bit 28 means ROR 32-(28-4)= ROR (bit4
/// -> bit28): bit4 ror 4 -> bit0; we want bit28, so ROR by 8. rot4 = 4.
fn enc_cmp_imm_modified(rn: u32, imm8: u32, rot4: u32) -> u32 {
    assert!(imm8 <= 0xFF);
    assert!(rot4 <= 0xF);
    // cond=E 001 10101 Rn SBZ(Rd=0) rot4 imm8
    0xE350_0000 | (rn << 16) | (rot4 << 8) | imm8
}

/// `Bcc #imm24`. `imm24` is the signed word offset from (current+8) to
/// target. `cond` occupies bits[31:28]; 0xE = AL (always, a plain B).
fn enc_bcond(cond: u32, from_pc: u32, target: u32) -> u32 {
    let offset = (target as i32).wrapping_sub(from_pc.wrapping_add(8) as i32);
    assert!(offset & 3 == 0, "branch target not word-aligned");
    let words = offset >> 2;
    assert!(
        words >= -(1 << 23) && words < (1 << 23),
        "branch out of ±32 MiB: from {:#x} to {:#x}", from_pc, target
    );
    let imm24 = (words as u32) & 0x00FF_FFFF;
    (cond << 28) | 0x0A00_0000 | imm24
}

/// The actual byte/halfword load or store inside the stub, using
/// `addr_reg` as [base] and `Rt` as the data register. No offset, no
/// writeback (the stub does those bits itself).
fn enc_access_inline(kind: AccessKind, rt: u32, addr_reg: u32) -> u32 {
    match kind {
        // LDRB Rt, [addr_reg]  — cond=AL, 010 P=1 U=1 B=1 W=0 L=1 Rn Rt imm12=0
        AccessKind::Ldrb => 0xE5D0_0000 | (addr_reg << 16) | (rt << 12),
        // STRB Rt, [addr_reg]
        AccessKind::Strb => 0xE5C0_0000 | (addr_reg << 16) | (rt << 12),
        // LDRH Rt, [addr_reg]  — extra load/store, P=1 U=1 I=1 W=0 L=1
        //   cond=AL 000 P=1 U=1 I=1 W=0 L=1 Rn Rt 0000 1 op=01 1 0000
        AccessKind::Ldrh => 0xE1D0_00B0 | (addr_reg << 16) | (rt << 12),
        // STRH Rt, [addr_reg] — L=0
        AccessKind::Strh => 0xE1C0_00B0 | (addr_reg << 16) | (rt << 12),
        // LDRSB Rt, [addr_reg]  — op=10, L=1
        AccessKind::Ldrsb => 0xE1D0_00D0 | (addr_reg << 16) | (rt << 12),
        // LDRSH Rt, [addr_reg]  — op=11, L=1
        AccessKind::Ldrsh => 0xE1D0_00F0 | (addr_reg << 16) | (rt << 12),
    }
}

/// Emit `ADD Rd, Rn, #imm` or `SUB Rd, Rn, #imm` for a 12-bit immediate
/// (0..4095), potentially splitting into two modified-imm instructions
/// when `imm > 0xFF`. `add=true` for ADD, `add=false` for SUB.
///
/// Splits as imm = hi*256 + lo with 0 ≤ lo ≤ 255 and 0 ≤ hi ≤ 15.
fn emit_addsub_imm12(
    rd: u32, rn: u32, imm: u32, add: bool,
    idx: &mut usize, out: &mut [u32; 16],
) -> Result<(), &'static str> {
    assert!(imm <= 0xFFF, "imm12 overflow: {:#x}", imm);
    if imm == 0 {
        // Degenerate; caller should have handled. Emit a MOV so ea=Rn.
        if *idx >= 16 { return Err("stub slot overflow"); }
        out[*idx] = enc_mov_reg(rd, rn);
        *idx += 1;
        return Ok(());
    }
    let lo = imm & 0xFF;
    let hi = (imm >> 8) & 0xF;

    if hi == 0 {
        if *idx >= 16 { return Err("stub slot overflow"); }
        out[*idx] = if add { enc_add_imm8(rd, rn, lo) } else { enc_sub_imm8(rd, rn, lo) };
        *idx += 1;
    } else if lo == 0 {
        // imm is a multiple of 256 — one instruction with rot4=12
        // (ROR 24 → value = imm8 << 8).
        if *idx >= 16 { return Err("stub slot overflow"); }
        out[*idx] = if add {
            enc_add_imm_rot(rd, rn, hi, 12)
        } else {
            enc_sub_imm_rot(rd, rn, hi, 12)
        };
        *idx += 1;
    } else {
        // Two-instruction split: first the hi byte, then the lo byte.
        if *idx + 1 >= 16 { return Err("stub slot overflow"); }
        out[*idx] = if add {
            enc_add_imm_rot(rd, rn, hi, 12)
        } else {
            enc_sub_imm_rot(rd, rn, hi, 12)
        };
        *idx += 1;
        out[*idx] = if add {
            enc_add_imm8(rd, rd, lo)
        } else {
            enc_sub_imm8(rd, rd, lo)
        };
        *idx += 1;
    }
    Ok(())
}

/// Build the full set of words for one stub. Returns the number of words
/// written into `out` (must be ≤ `STUB_SLOT_SIZE / 4 = 16`).
///
/// The stub is located at absolute IPA `stub_pc`, and must branch back
/// to `return_pc = original_pc + 4` when done.
fn build_stub(d: &Decoded, _stub_pc: u32, return_pc: u32, out: &mut [u32; 16])
    -> Result<usize, &'static str>
{
    let scratch = pick_scratch(d);
    // The stub computes the effective address into `ea_reg`. We always
    // use `scratch` as the EA register so we don't clobber Rn (even
    // when Rn has writeback it must be updated with the post-offset
    // value, not the XOR'd value).
    let ea = scratch;

    let mut idx = 0usize;
    let emit = |w: u32, idx: &mut usize, out: &mut [u32; 16]| -> Result<(), &'static str> {
        if *idx >= 16 { return Err("stub slot overflow"); }
        out[*idx] = w;
        *idx += 1;
        Ok(())
    };

    // 1. Save scratch.
    emit(enc_push(scratch), &mut idx, out)?;

    // 2. Compute the effective address. The address ARM uses is
    //    base + (signed offset), where:
    //      P=1 (pre/plain):    addr = Rn + offset
    //      P=0 (post-index):   addr = Rn  (offset applied afterward to Rn)
    //    We compute `offset_value` (as a reg-shifted form or an imm)
    //    and either add it to Rn or leave `ea = Rn`.
    //
    //    For post-index, we just `MOV ea, Rn`.
    //    For pre-index / plain, we `ADD ea, Rn, offset` or `SUB ea, Rn, offset`.
    //
    //    Note: Rn could == PC (r15). We reject that in the scanner to
    //    keep the stub simple; ADD/SUB with Rn=PC would read PC+8 at
    //    the stub site, not at the original PC, which is wrong.

    let computes_ea_from_rn = match d.p {
        true  => true,   // pre-indexed: address is Rn±offset
        false => false,  // post-indexed: address is Rn itself
    };

    if computes_ea_from_rn {
        match d.offset {
            OffsetForm::Imm { imm } => {
                emit_addsub_imm12(ea, d.rn, imm, d.u, &mut idx, out)?;
            }
            OffsetForm::Reg { rm, shift_type, shift_amount } => {
                if d.u {
                    emit(enc_add_reg_shift(ea, d.rn, rm, shift_type, shift_amount),
                         &mut idx, out)?;
                } else {
                    emit(enc_sub_reg_shift(ea, d.rn, rm, shift_type, shift_amount),
                         &mut idx, out)?;
                }
            }
        }
    } else {
        emit(enc_mov_reg(ea, d.rn), &mut idx, out)?;
    }

    // 3. MMIO-skip: if ea >= XOR_LIMIT, skip the XOR.
    //    CMP ea, #XOR_LIMIT (modified-imm: 0x10 rot #8).
    //    BHS +1 instruction (skip next insn).
    //
    //    0x1000_0000 = 0x10 ROR 8 → imm8=0x10, rot4=4 (since rot = 2*rot4 = 8).
    emit(enc_cmp_imm_modified(ea, 0x10, 4), &mut idx, out)?;
    // BHS (cond=0x2 = CS/HS) skipping the next one instruction (i.e. +4 past
    // the instruction after BHS -> branch offset = 0).
    //   Bcc rel = (cond << 28) | 0x0A000000 | (imm24 = (target - (pc+8))/4)
    // pc+8 at the BHS slot is (stub_pc + idx*4) + 8. Target is
    // (stub_pc + (idx+2)*4). So imm24 = ((idx+2) - (idx+2))/... actually:
    // the BHS is at offset idx*4, its pc+8 is (idx*4 + 8) = (idx+2)*4.
    // We want to land at offset (idx+2)*4, i.e. skip exactly 1 subsequent
    // insn. imm24 = 0.
    emit((0x2u32 << 28) | 0x0A00_0000 | 0, &mut idx, out)?;
    // 4. EOR ea, ea, #xor_mask (3 or 2).
    emit(enc_eor_imm(ea, ea, d.kind.xor_mask()), &mut idx, out)?;

    // 5. The actual load/store, addr = [ea], data = Rt.
    emit(enc_access_inline(d.kind, d.rt, ea), &mut idx, out)?;

    // 6. If writeback (W=1 with P=1, or post-index P=0 which always updates Rn),
    //    update Rn to Rn ± offset.
    //
    //    P=1,W=1 -> Rn := Rn + offset  (matches the EA we computed already;
    //                                   we could just MOV Rn, ea — but ea got
    //                                   XOR'd in the MMIO-miss case; we'd
    //                                   write the wrong value. Recompute from Rn.)
    //    P=0     -> Rn := Rn + offset
    //
    //    For simplicity we always recompute Rn from Rn using a fresh op.
    let writeback = (d.p && d.w) || !d.p;
    if writeback {
        match d.offset {
            OffsetForm::Imm { imm } => {
                if imm != 0 {
                    emit_addsub_imm12(d.rn, d.rn, imm, d.u, &mut idx, out)?;
                }
                // imm==0 is a degenerate writeback (UNPRED); leave Rn.
            }
            OffsetForm::Reg { rm, shift_type, shift_amount } => {
                if d.u {
                    emit(enc_add_reg_shift(d.rn, d.rn, rm, shift_type, shift_amount),
                         &mut idx, out)?;
                } else {
                    emit(enc_sub_reg_shift(d.rn, d.rn, rm, shift_type, shift_amount),
                         &mut idx, out)?;
                }
            }
        }
    }

    // 7. Restore scratch.
    emit(enc_pop(scratch), &mut idx, out)?;

    // 8. Branch back to return_pc via a PC-relative literal load.
    //    `LDR pc, [pc, #+off]`.  pc is (stub_pc + idx*4) + 8 at this slot.
    //    We'll park the literal immediately after, at offset (idx+1)*4.
    //    off = (idx+1)*4 - ((idx)*4 + 8) = -4. Encode as -4.
    emit(enc_ldr_pc_pc_lit(-4), &mut idx, out)?;
    emit(return_pc, &mut idx, out)?;

    Ok(idx)
}

/// Write `word` to the stub pool at byte offset `off`. The stub pool is
/// mapped at IPA `STUB_POOL_IPA` in the guest; we write through the host
/// backing so we don't depend on any guest-visible mapping.
fn pool_write_word(off: usize, word: u32) {
    assert!(off + 4 <= STUB_POOL_SIZE);
    let host = pool_host_pa() as usize + off;
    // SAFETY: bounds-checked above.
    unsafe { core::ptr::write_volatile(host as *mut u32, word); }
}

/// Write `word` into the backing store that owns the given IPA. Currently
/// we support writing into the 16 MiB ROM backing (the guest-test binary
/// is loaded there) — that's the only region this MVP patches.
fn code_write_word(ipa: u32, word: u32) -> Result<(), &'static str> {
    if (ipa as usize) + 4 > crate::guest_mem::ROM_SIZE {
        return Err("code_write_word: IPA outside ROM backing");
    }
    let host = crate::guest_mem::rom_host_pa() as usize + ipa as usize;
    // SAFETY: bounds-checked above.
    unsafe { core::ptr::write_volatile(host as *mut u32, word); }
    Ok(())
}

/// Read the code word at IPA `pa` out of the ROM backing.
fn code_read_word(ipa: u32) -> Option<u32> {
    crate::guest_mem::read_word_pa(ipa)
}

/// DC CVAC + IC IVAU + DSB/ISB on a host VA range, for I-cache coherence
/// after we write new stub bytes or patch the original code. Called from
/// EL2 where data accesses go through the hypervisor stage-1 identity map.
fn icache_sync_range(host_va: u64, length: usize) {
    let mut addr = host_va & !0x3F; // 64-byte cache line
    let end = host_va + length as u64;
    while addr < end {
        // SAFETY: cache-maintenance instructions only touch caches.
        unsafe {
            core::arch::asm!(
                "dc cvau, {0}",
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

/// Patch every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH in the code range
/// `[start_ipa, end_ipa)` of the ROM backing. Returns statistics.
///
/// The caller is responsible for ensuring the range contains code only
/// (misidentifying data as code is how BE-32 byteswap bugs happen;
/// the MVP just trusts the caller's range).
///
/// After patching:
/// - Original instructions at PC X are replaced with `Bcc shadow_X`,
///   same condition as the original.
/// - Stubs in the pool do the XOR'd access + return.
/// - I-cache + D-cache lines covering the modified words are flushed.
pub fn patch_code_range(start_ipa: u32, end_ipa: u32) -> PatchStats {
    assert!(start_ipa & 3 == 0);
    assert!(end_ipa & 3 == 0);
    assert!(end_ipa >= start_ipa);
    assert!((end_ipa as usize) <= crate::guest_mem::ROM_SIZE);

    let mut stats = PatchStats::default();
    let mut pc = start_ipa;
    while pc < end_ipa {
        stats.words_scanned += 1;
        let insn = match code_read_word(pc) {
            Some(w) => w,
            None => {
                pc = pc.wrapping_add(4);
                continue;
            }
        };

        let decoded = match decode(insn) {
            Some(d) => d,
            None => {
                pc = pc.wrapping_add(4);
                continue;
            }
        };

        // Reject PC as base/offset/dest — would require PC-relative
        // emulation in the stub, out of scope.
        if decoded.rn == 15 || decoded.rt == 15 {
            stats.skipped_pc_operand += 1;
            kprintln!(
                "shadow_stub: skipping insn {:#010x} at PC {:#x} — Rn or Rt is PC",
                insn, pc
            );
            pc = pc.wrapping_add(4);
            continue;
        }
        if let OffsetForm::Reg { rm, .. } = decoded.offset {
            if rm == 15 {
                stats.skipped_pc_operand += 1;
                pc = pc.wrapping_add(4);
                continue;
            }
        }

        // Allocate a stub slot.
        let slot = NEXT_SLOT.fetch_add(1, Ordering::SeqCst);
        if slot >= STUB_POOL_CAPACITY {
            kprintln!(
                "shadow_stub: ERROR — stub pool exhausted at PC {:#x} ({} stubs)",
                pc, slot
            );
            crate::cpu::halt();
        }
        let stub_ipa = STUB_POOL_IPA + (slot * STUB_SLOT_SIZE) as u32;

        let mut words = [0u32; 16];
        let n = match build_stub(&decoded, stub_ipa, pc.wrapping_add(4), &mut words) {
            Ok(n) => n,
            Err(e) => {
                kprintln!(
                    "shadow_stub: FATAL — couldn't build stub for {:#010x} at PC {:#x}: {}",
                    insn, pc, e
                );
                crate::cpu::halt();
            }
        };

        // Emit the stub into the pool backing.
        let pool_off = slot * STUB_SLOT_SIZE;
        for (i, w) in words.iter().enumerate().take(n) {
            pool_write_word(pool_off + i * 4, *w);
        }
        // Zero the tail of the slot so stray execution past the
        // branch-back faults loudly (0x0000_0000 = `ANDEQ r0, r0, r0`
        // which is fine; use 0xE7FE_DEFE = UDF #0xDEAD instead).
        for i in n..(STUB_SLOT_SIZE / 4) {
            pool_write_word(pool_off + i * 4, 0xE7F0_00F0);
        }

        // Now overwrite the original instruction with a Bcc to the stub.
        let patched = enc_bcond(decoded.cond, pc, stub_ipa);
        if let Err(e) = code_write_word(pc, patched) {
            kprintln!(
                "shadow_stub: FATAL — couldn't write patched insn at PC {:#x}: {}",
                pc, e
            );
            crate::cpu::halt();
        }

        match decoded.kind {
            AccessKind::Ldrb | AccessKind::Strb => stats.ldrb_strb += 1,
            AccessKind::Ldrh | AccessKind::Strh => stats.ldrh_strh += 1,
            AccessKind::Ldrsb | AccessKind::Ldrsh => stats.ldrsb_ldrsh += 1,
        }
        stats.patched += 1;

        pc = pc.wrapping_add(4);
    }

    // Cache maintenance:
    // - Stub pool: we just wrote data to the host backing; the guest
    //   will eventually fetch instructions from it via stage-2, so we
    //   need dc cvau / ic ivau over the written range.
    // - Patched original instructions: same story.
    let slots_used = NEXT_SLOT.load(Ordering::SeqCst);
    if slots_used > 0 {
        icache_sync_range(pool_host_pa(), slots_used * STUB_SLOT_SIZE);
    }
    let rom_host = crate::guest_mem::rom_host_pa() + start_ipa as u64;
    icache_sync_range(rom_host, (end_ipa - start_ipa) as usize);

    stats
}

/// Print a compact summary of a patching run.
pub fn log_stats(stats: &PatchStats) {
    kprintln!(
        "shadow_stub: scanned {} words, patched {} insns \
         (LDRB/STRB={}, LDRH/STRH={}, LDRSB/LDRSH={}), \
         skipped {} PC-operand, pool slots used {} / {}",
        stats.words_scanned, stats.patched,
        stats.ldrb_strb, stats.ldrh_strh, stats.ldrsb_ldrsh,
        stats.skipped_pc_operand,
        NEXT_SLOT.load(Ordering::SeqCst),
        STUB_POOL_CAPACITY,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_ldrb_immediate() {
        // LDRB r0, [r1, #4]  — E5D1 0004
        let d = decode(0xE5D1_0004).unwrap();
        assert_eq!(d.kind, AccessKind::Ldrb);
        assert_eq!(d.cond, 0xE);
        assert_eq!(d.rn, 1);
        assert_eq!(d.rt, 0);
        assert!(matches!(d.offset, OffsetForm::Imm { imm: 4 }));
        assert!(d.p);
        assert!(d.u);
        assert!(!d.w);
    }

    #[test]
    fn decode_strb_register() {
        // STRB r3, [r1, r2, LSL #2]  — E7C1 3102
        let d = decode(0xE7C1_3102).unwrap();
        assert_eq!(d.kind, AccessKind::Strb);
        assert_eq!(d.rn, 1);
        assert_eq!(d.rt, 3);
        match d.offset {
            OffsetForm::Reg { rm, shift_type, shift_amount } => {
                assert_eq!(rm, 2);
                assert_eq!(shift_type, 0);
                assert_eq!(shift_amount, 2);
            }
            _ => panic!("expected reg form"),
        }
    }

    #[test]
    fn decode_ldrh_immediate() {
        // LDRH r0, [r1, #4]  — E1D1 00B4
        let d = decode(0xE1D1_00B4).unwrap();
        assert_eq!(d.kind, AccessKind::Ldrh);
        assert_eq!(d.rn, 1);
        assert_eq!(d.rt, 0);
    }

    #[test]
    fn decode_ldrsb() {
        // LDRSB r0, [r1]  — E1D1 00D0
        let d = decode(0xE1D1_00D0).unwrap();
        assert_eq!(d.kind, AccessKind::Ldrsb);
    }

    #[test]
    fn decode_does_not_match_ldr_word() {
        // LDR r0, [r1]  — E591 0000  (word load, should not decode)
        assert!(decode(0xE591_0000).is_none());
    }

    #[test]
    fn decode_does_not_match_swp() {
        // SWPB r0, r1, [r2]  — E142 0091
        assert!(decode(0xE142_0091).is_none());
    }
}
