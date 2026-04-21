//! Shadow-stub mechanism for byte/halfword access on a BE-32 guest
//! running under a little-endian host.
//!
//! The Newton ROM is BE-32 "word-invariant": aligned word accesses are
//! identical to LE (word swapped at load time), but byte and halfword
//! accesses use a different byte lane:
//!
//!   BE-32 LDRB at addr A  ->  phys[A ^ 3]
//!   BE-32 LDRH at addr A  ->  halfword at phys[A ^ 2]
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
//! `B` reaches +-32 MiB, so the stub pool must live within that window
//! of every patched site. We install it at guest IPA 0x01800000 (24
//! MiB), reachable from anything in the 0..16 MiB ROM region.
//!
//! MMIO skip: the BE-32 XOR only applies to real memory (RAM / ROM /
//! FB / flash). Accesses to MMIO registers must not be XOR'd — the
//! Newton kernel writes typed bytes straight into MMIO byte lanes.
//! Each stub therefore runtime-checks the effective address against
//! `XOR_LIMIT` and branches past the XOR when the address is higher.
//!
//! SP safety: the stub computes the effective address *before* any SP
//! manipulation. The scratch register is saved to a per-stub PC-relative
//! save slot at the tail of the slot, not to the guest stack. That way
//! `[SP, #imm]`, `[SP, Rm]`, and SP-writeback forms all see the original
//! SP when computing EA.
//!
//! Scope: this module handles LDRB / STRB / LDRH / STRH / LDRSB / LDRSH
//! in immediate, register-offset (including shifted), pre-index
//! (`[Rn,#imm]!`), and post-index (`[Rn],#imm`) forms, plus SWPB. PC
//! (r15) as base, data, or offset register is flagged as unsupported —
//! a correct stub would need to emulate PC-relative addressing from
//! the original site, not the stub site.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::kprintln;

/// Two stub pools cover the full guest-code address range:
///   Pool A at 0x01800000 reaches ROM  [0x00000000..0x01000000]
///                                + flash bank 0 [0x02000000..0x02400000].
///   Pool B at 0x03000000 reaches RAM  [0x04000000..0x04400000]
///                                + the RAM mirror [0x0C000000..0x0C400000].
/// Each pool is 2 MiB (one stage-2 block descriptor) and they share a
/// single backing buffer for simplicity — pool B lives at offset
/// STUB_POOL_SIZE within the buffer.
pub const STUB_POOL_IPA: u32 = 0x0180_0000;
pub const STUB_POOL_B_IPA: u32 = 0x0300_0000;

/// Stub-pool size. 2 MiB = 1 stage-2 block descriptor.
pub const STUB_POOL_SIZE: usize = 2 * 1024 * 1024;

/// Addresses < XOR_LIMIT are treated as real memory (XOR applied);
/// addresses >= XOR_LIMIT are treated as MMIO and passed through.
/// Chosen to cover everything in the Newton IPA map below flash bank 1.
pub const XOR_LIMIT: u32 = 0x1000_0000;

/// Fixed bytes per stub slot. 16 words x 4 = 64 bytes. Worst case
/// instruction count (13 insns including save/restore and branch-back)
/// plus return_pc literal and save_slot fits in 16 words.
pub const STUB_SLOT_SIZE: usize = 64;

/// Words per stub slot.
pub const STUB_SLOT_WORDS: usize = STUB_SLOT_SIZE / 4;

/// Byte offset within a slot of the `return_pc` literal (second-to-last word).
pub const STUB_RETURN_PC_OFF: usize = STUB_SLOT_SIZE - 8;

/// Byte offset within a slot of the scratch save slot (last word).
pub const STUB_SAVE_SLOT_OFF: usize = STUB_SLOT_SIZE - 4;

/// Per-pool capacity — number of stubs that fit.
pub const STUB_POOL_CAPACITY: usize = STUB_POOL_SIZE / STUB_SLOT_SIZE;

/// Total capacity across both pools. Slot indices are packed with pool
/// A in [0..STUB_POOL_CAPACITY), pool B in [STUB_POOL_CAPACITY..TOTAL).
pub const STUB_POOL_TOTAL_CAPACITY: usize = STUB_POOL_CAPACITY * 2;

#[repr(C, align(0x200000))]
struct StubPool([u8; STUB_POOL_SIZE * 2]);

static mut STUB_POOL: StubPool = StubPool([0; STUB_POOL_SIZE * 2]);

/// How many slots have been handed out in pool A (ROM reach).
static NEXT_SLOT_A: AtomicUsize = AtomicUsize::new(0);
/// How many slots have been handed out in pool B (RAM reach).
static NEXT_SLOT_B: AtomicUsize = AtomicUsize::new(0);

/// Map from stub-slot index to the original guest PC that the stub was
/// emitted for. Used by trap.rs when a data abort fires inside a stub:
/// we un-XOR FAR_EL2 and retarget ELR_EL2 to the original PC so the
/// guest's abort handler sees the state it expects.
///
/// Entry is `u32::MAX` for unused slots. Indexed by packed slot index
/// (pool A entries at [0..STUB_POOL_CAPACITY), pool B at
/// [STUB_POOL_CAPACITY..STUB_POOL_TOTAL_CAPACITY)).
static mut SLOT_ORIGINAL_PC: [u32; STUB_POOL_TOTAL_CAPACITY] =
    [u32::MAX; STUB_POOL_TOTAL_CAPACITY];

/// Host physical base of the stub pool A backing store.
pub fn pool_host_pa() -> u64 {
    addr_of_mut!(STUB_POOL) as u64
}

/// Host physical base of the stub pool B backing store (offset by
/// one pool's worth from the base of the combined buffer).
pub fn pool_b_host_pa() -> u64 {
    addr_of_mut!(STUB_POOL) as u64 + STUB_POOL_SIZE as u64
}

/// Is `ipa` inside the shadow-stub pool A range?
fn is_pool_a_ipa(ipa: u32) -> bool {
    ipa >= STUB_POOL_IPA
        && (ipa as usize) < (STUB_POOL_IPA as usize) + STUB_POOL_SIZE
}

/// Is `ipa` inside the shadow-stub pool B range?
fn is_pool_b_ipa(ipa: u32) -> bool {
    ipa >= STUB_POOL_B_IPA
        && (ipa as usize) < (STUB_POOL_B_IPA as usize) + STUB_POOL_SIZE
}

/// Is `ipa` inside either stub pool?
pub fn is_stub_ipa(ipa: u32) -> bool {
    is_pool_a_ipa(ipa) || is_pool_b_ipa(ipa)
}

/// Given an IPA inside either stub pool, return
/// `(packed_slot_index, byte_offset_in_slot)`. Pool-A slots are packed
/// at [0..STUB_POOL_CAPACITY), pool-B slots at
/// [STUB_POOL_CAPACITY..STUB_POOL_TOTAL_CAPACITY).
pub fn ipa_to_slot_offset(ipa: u32) -> Option<(usize, usize)> {
    if is_pool_a_ipa(ipa) {
        let rel = (ipa - STUB_POOL_IPA) as usize;
        return Some((rel / STUB_SLOT_SIZE, rel % STUB_SLOT_SIZE));
    }
    if is_pool_b_ipa(ipa) {
        let rel = (ipa - STUB_POOL_B_IPA) as usize;
        return Some((STUB_POOL_CAPACITY + rel / STUB_SLOT_SIZE,
                     rel % STUB_SLOT_SIZE));
    }
    None
}

/// Look up the original guest PC that owned the stub at this slot.
/// Returns None if the slot is unused. `slot` is the packed index.
pub fn slot_original_pc(slot: usize) -> Option<u32> {
    if slot >= STUB_POOL_TOTAL_CAPACITY {
        return None;
    }
    // SAFETY: slot bounded; single-threaded updates in patch_code_range.
    let pc = unsafe { SLOT_ORIGINAL_PC[slot] };
    if pc == u32::MAX { None } else { Some(pc) }
}

/// Byte offset within a slot of the "inner access" instruction — the
/// real LDRB/STRB/... whose data abort we want to forward to the guest.
/// Built into the stub by `build_stub`; we record it so trap.rs can
/// check whether an ELR lying inside a stub is exactly the access
/// instruction (the only in-stub PC at which a data abort is expected).
///
/// Entry is `u8::MAX` for unused slots.
static mut SLOT_ACCESS_OFF: [u8; STUB_POOL_TOTAL_CAPACITY] =
    [u8::MAX; STUB_POOL_TOTAL_CAPACITY];

pub fn slot_access_offset(slot: usize) -> Option<usize> {
    if slot >= STUB_POOL_TOTAL_CAPACITY { return None; }
    // SAFETY: see SLOT_ORIGINAL_PC.
    let off = unsafe { SLOT_ACCESS_OFF[slot] };
    if off == u8::MAX { None } else { Some(off as usize) }
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
    pub swpb: usize,
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
    /// SWPB (atomic byte swap) — cond 00010100 Rn Rt SBZ 1001 Rm.
    Swpb,
}

impl AccessKind {
    /// The XOR applied to the effective address for BE-32 compatibility:
    /// 3 for byte accesses, 2 for halfword.
    fn xor_mask(self) -> u32 {
        match self {
            AccessKind::Ldrb
            | AccessKind::Strb
            | AccessKind::Ldrsb
            | AccessKind::Swpb => 3,
            AccessKind::Ldrh | AccessKind::Strh | AccessKind::Ldrsh => 2,
        }
    }
}

/// Offset form.
#[derive(Clone, Copy, Debug)]
enum OffsetForm {
    /// No offset (SWPB — the address is just [Rn]).
    None,
    /// Immediate offset, `imm` is unsigned magnitude.
    Imm { imm: u32 },
    /// Register offset `Rm`, with an optional LSL/LSR/ASR/ROR shift.
    Reg { rm: u32, shift_type: u32, shift_amount: u32 },
}

/// Fully decoded byte/halfword access.
#[derive(Clone, Copy, Debug)]
struct Decoded {
    kind: AccessKind,
    cond: u32,
    rn: u32,
    rt: u32,
    /// Second data register — only meaningful for SWPB (Rm = source).
    rt2: u32,
    offset: OffsetForm,
    p: bool, // pre-index when true; post-index when false. Always true for SWPB.
    u: bool, // add offset when true, subtract when false.
    w: bool, // writeback when true (relevant for P=1).
}

/// Attempt to decode a byte/halfword access. Returns `Some(Decoded)`
/// for the encodings we handle and `None` for everything else.
fn decode(insn: u32) -> Option<Decoded> {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        // Unconditional-class encodings (NEON, PLD, ...). Not ours.
        return None;
    }

    // Form 1: data-processing-immediate / register LDR/STR with B=1.
    //   Immediate: cond 010 P U B W L Rn Rt imm12
    //   Register : cond 011 P U B W L Rn Rt imm5 type 0 Rm
    if (insn & 0x0E00_0000) == 0x0400_0000
        && (insn & (1 << 22)) != 0
    {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let imm = insn & 0xFFF;
        return Some(Decoded {
            kind: if l { AccessKind::Ldrb } else { AccessKind::Strb },
            cond, rn, rt, rt2: 0,
            offset: OffsetForm::Imm { imm },
            p, u, w,
        });
    }
    if (insn & 0x0E00_0010) == 0x0600_0000
        && (insn & (1 << 22)) != 0
    {
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
            cond, rn, rt, rt2: 0,
            offset: OffsetForm::Reg { rm, shift_type, shift_amount },
            p, u, w,
        });
    }

    // Form 2: extra load/store (halfword / signed byte / signed halfword).
    //   cond 000 P U I W L Rn Rt imm4h 1 op1 op2 1 imm4l
    // Keyed on bits[27:25]=000, bit 7 = 1, bit 4 = 1, and bits[6:5] (op != 00).
    // The op=00 sub-block is the synchronization-primitive family
    // (SWP/SWPB/LDREX/STREX/...); we handle SWPB separately in Form 3
    // and leave the rest alone. We check op != 00 at the match level
    // so SWPB matching Form 2's wider mask still falls through to
    // Form 3.
    if (insn & 0x0E00_0090) == 0x0000_0090
        && ((insn >> 5) & 0x3) != 0
    {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let i = (insn >> 22) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let op = (insn >> 5) & 0x3;

        let kind = match (op, l) {
            (0b01, true)  => AccessKind::Ldrh,
            (0b01, false) => AccessKind::Strh,
            (0b10, true)  => AccessKind::Ldrsb,
            (0b10, false) => return None, // LDRD — verified safe in item 4.
            (0b11, true)  => AccessKind::Ldrsh,
            (0b11, false) => return None, // STRD — verified safe in item 4.
            _ => return None,
        };

        let offset = if i {
            let imm = ((insn >> 4) & 0xF0) | (insn & 0xF);
            OffsetForm::Imm { imm }
        } else {
            let rm = insn & 0xF;
            OffsetForm::Reg { rm, shift_type: 0, shift_amount: 0 }
        };

        return Some(Decoded {
            kind, cond, rn, rt, rt2: 0, offset, p, u, w,
        });
    }

    // Form 3: SWPB.
    //   cond 0001 0100 Rn Rt (SBZ=0000) 1001 Rm
    // Mask zeros cond, Rn, Rt, Rm and SBZ, leaving bits[27:20] and [7:4]
    // to check: 0001_0100 ____ ____ ____ 1001 ____.
    if (insn & 0x0FF0_0FF0) == 0x0140_0090 {
        let rn = (insn >> 16) & 0xF;
        let rt = (insn >> 12) & 0xF;
        let rm = insn & 0xF;
        // AArch32 marks Rt == Rm UNPREDICTABLE. We refuse to stub it —
        // the load/store pair can't preserve the original Rm byte if
        // Rt and Rm alias, and a ROM that hits this is broken anyway.
        if rt == rm {
            return None;
        }
        return Some(Decoded {
            kind: AccessKind::Swpb,
            cond,
            rn,
            rt,
            rt2: rm,
            offset: OffsetForm::None,
            p: true,
            u: true,
            w: false,
        });
    }

    None
}

/// Pick a scratch register that is not in `{Rt, Rn, Rm, Rt2, PC=15}`.
/// We deliberately avoid SP (r13) to keep the stub ABI-clean even though
/// we don't rely on it.
fn pick_scratch(d: &Decoded) -> u32 {
    let rm = if let OffsetForm::Reg { rm, .. } = d.offset { Some(rm) } else { None };
    for candidate in &[12u32, 14, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
        let c = *candidate;
        if c == d.rt || c == d.rn || c == d.rt2 { continue; }
        if c == 13 { continue; }
        if let Some(rm) = rm { if c == rm { continue; } }
        return c;
    }
    unreachable!("should always find a scratch register");
}

/// `MOV Rd, Rm`.
fn enc_mov_reg(rd: u32, rm: u32) -> u32 {
    0xE1A0_0000 | (rd << 12) | rm
}

/// `ADD Rd, Rn, #imm8`.
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

/// `EOR Rd, Rn, #imm8`.
fn enc_eor_imm(rd: u32, rn: u32, imm: u32) -> u32 {
    assert!(imm <= 0xFF);
    0xE220_0000 | (rn << 16) | (rd << 12) | imm
}

/// `CMP Rn, #imm8, rotate`. Modified-immediate with a 4-bit rotate field.
fn enc_cmp_imm_modified(rn: u32, imm8: u32, rot4: u32) -> u32 {
    assert!(imm8 <= 0xFF);
    assert!(rot4 <= 0xF);
    0xE350_0000 | (rn << 16) | (rot4 << 8) | imm8
}

/// `STR Rt, [Rn, #+-imm12]` (pre-indexed, no writeback).
/// Used to save scratch to the per-stub save slot with Rn = PC.
fn enc_str_pc_rel(rt: u32, disp: i32) -> u32 {
    assert!(disp.unsigned_abs() <= 0xFFF);
    let u = if disp >= 0 { 1u32 } else { 0 };
    // cond=AL 010 P=1 U B=0 W=0 L=0 Rn=PC(15) Rt imm12
    0xE500_0000
        | (u << 23)
        | (15u32 << 16)
        | (rt << 12)
        | disp.unsigned_abs()
}

/// `LDR Rt, [pc, #+-imm12]`.
fn enc_ldr_pc_rel(rt: u32, disp: i32) -> u32 {
    assert!(disp.unsigned_abs() <= 0xFFF);
    let u = if disp >= 0 { 1u32 } else { 0 };
    // cond=AL 010 P=1 U B=0 W=0 L=1 Rn=PC(15) Rt imm12
    0xE510_0000
        | (u << 23)
        | (15u32 << 16)
        | (rt << 12)
        | disp.unsigned_abs()
}

/// `LDR pc, [pc, #-4]` — branch via a literal immediately before this instruction.
fn enc_ldr_pc_pc_lit(disp: i32) -> u32 {
    enc_ldr_pc_rel(15, disp)
}

/// `Bcc #imm24` from `from_pc` to `target`.
fn enc_bcond(cond: u32, from_pc: u32, target: u32) -> u32 {
    let offset = (target as i32).wrapping_sub(from_pc.wrapping_add(8) as i32);
    assert!(offset & 3 == 0, "branch target not word-aligned");
    let words = offset >> 2;
    assert!(
        words >= -(1 << 23) && words < (1 << 23),
        "branch out of +-32 MiB: from {:#x} to {:#x}", from_pc, target
    );
    let imm24 = (words as u32) & 0x00FF_FFFF;
    (cond << 28) | 0x0A00_0000 | imm24
}

/// The byte/halfword load or store inside the stub. No offset, no writeback.
fn enc_access_inline(kind: AccessKind, rt: u32, addr_reg: u32) -> u32 {
    match kind {
        AccessKind::Ldrb => 0xE5D0_0000 | (addr_reg << 16) | (rt << 12),
        AccessKind::Strb => 0xE5C0_0000 | (addr_reg << 16) | (rt << 12),
        AccessKind::Ldrh => 0xE1D0_00B0 | (addr_reg << 16) | (rt << 12),
        AccessKind::Strh => 0xE1C0_00B0 | (addr_reg << 16) | (rt << 12),
        AccessKind::Ldrsb => 0xE1D0_00D0 | (addr_reg << 16) | (rt << 12),
        AccessKind::Ldrsh => 0xE1D0_00F0 | (addr_reg << 16) | (rt << 12),
        AccessKind::Swpb => {
            // Never used as the "inner access" for a stub — SWPB has a
            // dedicated LDREXB/STREXB loop emitted by build_swpb_access.
            panic!("enc_access_inline called with Swpb");
        }
    }
}

/// Emit `ADD Rd, Rn, #imm` or `SUB Rd, Rn, #imm` for a 12-bit immediate.
fn emit_addsub_imm12(
    rd: u32, rn: u32, imm: u32, add: bool,
    idx: &mut usize, out: &mut [u32; STUB_SLOT_WORDS],
) -> Result<(), &'static str> {
    assert!(imm <= 0xFFF, "imm12 overflow: {:#x}", imm);
    if imm == 0 {
        if *idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
        out[*idx] = enc_mov_reg(rd, rn);
        *idx += 1;
        return Ok(());
    }
    let lo = imm & 0xFF;
    let hi = (imm >> 8) & 0xF;

    if hi == 0 {
        if *idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
        out[*idx] = if add { enc_add_imm8(rd, rn, lo) } else { enc_sub_imm8(rd, rn, lo) };
        *idx += 1;
    } else if lo == 0 {
        if *idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
        out[*idx] = if add {
            enc_add_imm_rot(rd, rn, hi, 12)
        } else {
            enc_sub_imm_rot(rd, rn, hi, 12)
        };
        *idx += 1;
    } else {
        if *idx + 1 >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
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

/// Emit the SWPB inner access as a plain LDRB/STRB pair on [ea_reg].
/// Atomicity vs other cores isn't a concern on our uniprocessor guest;
/// EL2 holds DAIF.I masked for the entire stub so the guest itself can't
/// observe the pair as non-atomic.
///
/// Sequence (two words):
///     LDRB Rt,  [ea]        ; Rt = old byte (zero-extended)
///     STRB Rm,  [ea]        ; store new byte (Rm = rt2)
///
/// Requires Rt != Rm (enforced by `decode`).
fn build_swpb_inner(
    d: &Decoded, scratch_ea: u32,
    idx: &mut usize, out: &mut [u32; STUB_SLOT_WORDS],
) -> Result<(), &'static str> {
    if *idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[*idx] = enc_access_inline(AccessKind::Ldrb, d.rt, scratch_ea);
    *idx += 1;
    if *idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[*idx] = enc_access_inline(AccessKind::Strb, d.rt2, scratch_ea);
    *idx += 1;
    Ok(())
}

/// Info returned by `build_stub` about the emitted stub.
struct BuiltStub {
    words: usize,
    access_off: usize, // byte offset of the inner access instruction
}

/// Per-slot metadata kept for abort transparency.
#[derive(Clone, Copy)]
struct SlotMeta {
    /// XOR mask the stub applied to the guest's effective address
    /// (3 for byte accesses / SWPB, 2 for halfword).
    xor_mask: u32,
}

static mut SLOT_META: [Option<SlotMeta>; STUB_POOL_TOTAL_CAPACITY] =
    [None; STUB_POOL_TOTAL_CAPACITY];

pub fn slot_xor_mask(slot: usize) -> Option<u32> {
    if slot >= STUB_POOL_TOTAL_CAPACITY { return None; }
    // SAFETY: slot bounded; single-threaded writer.
    unsafe { SLOT_META[slot].map(|m| m.xor_mask) }
}

/// Build the full set of words for one stub. Returns the number of
/// words actually emitted (into words 0..n-1), plus the byte offset of
/// the inner access instruction. Words from n to STUB_SLOT_WORDS-3 get
/// filled with UDF by the caller. The last two words are the return_pc
/// literal and the scratch save slot.
fn build_stub(d: &Decoded, _stub_pc: u32, return_pc: u32,
              out: &mut [u32; STUB_SLOT_WORDS]) -> Result<BuiltStub, &'static str>
{
    let scratch = pick_scratch(d);
    let ea = scratch;
    let mut idx = 0usize;

    // 1. Save scratch to the per-stub save slot via PC-relative STR.
    //    At the instruction position idx*4, PC=(idx*4 + 8). Save slot
    //    at byte offset STUB_SAVE_SLOT_OFF. Displacement from PC:
    //      STUB_SAVE_SLOT_OFF - (idx*4 + 8).
    let disp = (STUB_SAVE_SLOT_OFF as i32) - (idx as i32 * 4 + 8);
    out[idx] = enc_str_pc_rel(scratch, disp);
    idx += 1;

    // 2. Compute the effective address into `ea`.
    //    Pre-indexed / plain: ea = Rn +- offset.
    //    Post-indexed:        ea = Rn.
    //    SWPB (offset=None, p=true): ea = Rn.
    let computes_ea_from_rn = d.p;
    if computes_ea_from_rn {
        match d.offset {
            OffsetForm::None => {
                out[idx] = enc_mov_reg(ea, d.rn); idx += 1;
            }
            OffsetForm::Imm { imm } => {
                emit_addsub_imm12(ea, d.rn, imm, d.u, &mut idx, out)?;
            }
            OffsetForm::Reg { rm, shift_type, shift_amount } => {
                if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
                out[idx] = if d.u {
                    enc_add_reg_shift(ea, d.rn, rm, shift_type, shift_amount)
                } else {
                    enc_sub_reg_shift(ea, d.rn, rm, shift_type, shift_amount)
                };
                idx += 1;
            }
        }
    } else {
        if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
        out[idx] = enc_mov_reg(ea, d.rn);
        idx += 1;
    }

    // 3. MMIO-skip: if ea >= XOR_LIMIT skip the XOR.
    //    0x1000_0000 = 0x10 ROR 8 -> imm8=0x10, rot4=4.
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = enc_cmp_imm_modified(ea, 0x10, 4);
    idx += 1;
    // BHS skipping the next one instruction (offset 0 in imm24).
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = (0x2u32 << 28) | 0x0A00_0000;
    idx += 1;
    // 4. EOR ea, ea, #xor_mask.
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = enc_eor_imm(ea, ea, d.kind.xor_mask());
    idx += 1;

    // 5. The real load/store inner access (or SWPB's LDRB/STRB pair).
    let access_off = idx * 4;
    if matches!(d.kind, AccessKind::Swpb) {
        build_swpb_inner(d, ea, &mut idx, out)?;
    } else {
        if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
        out[idx] = enc_access_inline(d.kind, d.rt, ea);
        idx += 1;
    }

    // 6. Writeback: Rn := Rn +- offset for pre-W=1 or post-index.
    let writeback = (d.p && d.w) || !d.p;
    if writeback {
        match d.offset {
            OffsetForm::None => {}
            OffsetForm::Imm { imm } => {
                if imm != 0 {
                    emit_addsub_imm12(d.rn, d.rn, imm, d.u, &mut idx, out)?;
                }
            }
            OffsetForm::Reg { rm, shift_type, shift_amount } => {
                if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
                out[idx] = if d.u {
                    enc_add_reg_shift(d.rn, d.rn, rm, shift_type, shift_amount)
                } else {
                    enc_sub_reg_shift(d.rn, d.rn, rm, shift_type, shift_amount)
                };
                idx += 1;
            }
        }
    }

    // 7. Restore scratch via PC-relative LDR.
    let disp_r = (STUB_SAVE_SLOT_OFF as i32) - (idx as i32 * 4 + 8);
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = enc_ldr_pc_rel(scratch, disp_r);
    idx += 1;

    // 8. Branch back: `LDR pc, [pc, #disp]` targeting the return_pc literal.
    let disp_b = (STUB_RETURN_PC_OFF as i32) - (idx as i32 * 4 + 8);
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = enc_ldr_pc_pc_lit(disp_b);
    idx += 1;

    Ok(BuiltStub { words: idx, access_off })
}

fn pool_write_word(pool_b: bool, off: usize, word: u32) {
    assert!(off + 4 <= STUB_POOL_SIZE);
    let host = if pool_b {
        pool_b_host_pa() as usize + off
    } else {
        pool_host_pa() as usize + off
    };
    // SAFETY: bounds-checked.
    unsafe { core::ptr::write_volatile(host as *mut u32, word); }
}

/// Pick the right stub pool for a given source IPA. ROM + flash
/// bank 0 use pool A; RAM + mirror use pool B. Anything else halts.
fn select_pool(source_ipa: u32) -> bool {
    // Pool A reaches 0x00000000..about 0x03800000.
    // Pool B reaches about 0x01000000..0x05000000.
    // ROM < 0x01000000, flash0 0x02000000..0x02400000 -> pool A.
    // RAM 0x04000000..0x04400000 -> pool B.
    // Mirror 0x0C000000..0x0C400000 is FAR from both — won't work.
    if (source_ipa as usize) < crate::guest_mem::ROM_SIZE {
        return false; // pool A
    }
    let ram_base = crate::guest_mem::RAM_IPA_BASE;
    let ram_end = ram_base + crate::guest_mem::RAM_SIZE as u32;
    if source_ipa >= ram_base && source_ipa < ram_end {
        return true; // pool B
    }
    // Flash bank 0.
    if source_ipa >= 0x0200_0000 && source_ipa < 0x0240_0000 {
        return false; // pool A
    }
    // Unsupported source — caller has already validated this is a
    // code region, so halt.
    kprintln!(
        "shadow_stub: select_pool — unsupported source IPA {:#x}",
        source_ipa
    );
    crate::cpu::halt();
}

/// Write `word` into a backing store that owns this IPA. Used to replace
/// an original guest instruction with a Bcc to the stub. Supports the
/// ROM backing and the RAM backing (item 5).
fn code_write_word(ipa: u32, word: u32) -> Result<(), &'static str> {
    if (ipa as usize) + 4 <= crate::guest_mem::ROM_SIZE {
        let host = crate::guest_mem::rom_host_pa() as usize + ipa as usize;
        unsafe { core::ptr::write_volatile(host as *mut u32, word); }
        return Ok(());
    }
    let ram_base = crate::guest_mem::RAM_IPA_BASE as usize;
    if (ipa as usize) >= ram_base
        && (ipa as usize) + 4 <= ram_base + crate::guest_mem::RAM_SIZE
    {
        let host = crate::guest_mem::ram_host_pa() as usize + (ipa as usize - ram_base);
        unsafe { core::ptr::write_volatile(host as *mut u32, word); }
        return Ok(());
    }
    Err("code_write_word: IPA outside ROM or RAM backing")
}

fn code_read_word(ipa: u32) -> Option<u32> {
    crate::guest_mem::read_word_pa(ipa)
}

/// DC CVAC + IC IVAU + DSB/ISB on a host VA range.
pub fn icache_sync_range(host_va: u64, length: usize) {
    let mut addr = host_va & !0x3F;
    let end = host_va + length as u64;
    while addr < end {
        // SAFETY: cache-maintenance only touches caches.
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

/// Patch every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH/SWPB in [start_ipa, end_ipa)
/// of the ROM or RAM backing.
pub fn patch_code_range(start_ipa: u32, end_ipa: u32) -> PatchStats {
    assert!(start_ipa & 3 == 0);
    assert!(end_ipa & 3 == 0);
    assert!(end_ipa >= start_ipa);

    let use_pool_b = select_pool(start_ipa);

    let mut stats = PatchStats::default();
    let mut pc = start_ipa;
    while pc < end_ipa {
        stats.words_scanned += 1;
        let insn = match code_read_word(pc) {
            Some(w) => w,
            None => { pc = pc.wrapping_add(4); continue; }
        };

        let decoded = match decode(insn) {
            Some(d) => d,
            None => { pc = pc.wrapping_add(4); continue; }
        };

        // Reject PC as any operand.
        if decoded.rn == 15 || decoded.rt == 15
            || (matches!(decoded.kind, AccessKind::Swpb) && decoded.rt2 == 15)
        {
            stats.skipped_pc_operand += 1;
            kprintln!(
                "shadow_stub: skipping insn {:#010x} at PC {:#x} - PC operand",
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

        let (local_slot, stub_base_ipa) = if use_pool_b {
            let s = NEXT_SLOT_B.fetch_add(1, Ordering::SeqCst);
            (s, STUB_POOL_B_IPA)
        } else {
            let s = NEXT_SLOT_A.fetch_add(1, Ordering::SeqCst);
            (s, STUB_POOL_IPA)
        };
        if local_slot >= STUB_POOL_CAPACITY {
            kprintln!(
                "shadow_stub: ERROR - stub pool {} exhausted at PC {:#x} ({} stubs)",
                if use_pool_b { "B" } else { "A" }, pc, local_slot
            );
            crate::cpu::halt();
        }
        let stub_ipa = stub_base_ipa + (local_slot * STUB_SLOT_SIZE) as u32;
        let packed_slot = if use_pool_b {
            STUB_POOL_CAPACITY + local_slot
        } else {
            local_slot
        };

        let mut words = [0u32; STUB_SLOT_WORDS];
        let built = match build_stub(&decoded, stub_ipa, pc.wrapping_add(4), &mut words) {
            Ok(b) => b,
            Err(e) => {
                kprintln!(
                    "shadow_stub: FATAL - couldn't build stub for {:#010x} at PC {:#x}: {}",
                    insn, pc, e
                );
                crate::cpu::halt();
            }
        };

        // Place the return_pc literal at STUB_RETURN_PC_OFF and zero
        // the scratch save slot.
        words[STUB_RETURN_PC_OFF / 4] = pc.wrapping_add(4);
        words[STUB_SAVE_SLOT_OFF / 4] = 0;
        // Fill any gap between the emitted code and the return_pc
        // literal with UDF #0xDEAD so stray execution past the
        // branch-back faults loudly.
        for i in built.words..(STUB_RETURN_PC_OFF / 4) {
            words[i] = 0xE7F0_00F0;
        }

        let pool_off = local_slot * STUB_SLOT_SIZE;
        for (i, w) in words.iter().enumerate() {
            pool_write_word(use_pool_b, pool_off + i * 4, *w);
        }

        // Record slot metadata for abort transparency.
        // SAFETY: single-threaded callers; bounded slot.
        unsafe {
            SLOT_ORIGINAL_PC[packed_slot] = pc;
            SLOT_ACCESS_OFF[packed_slot] = built.access_off as u8;
            SLOT_META[packed_slot] = Some(SlotMeta { xor_mask: decoded.kind.xor_mask() });
        }

        // Patch original site.
        let patched = enc_bcond(decoded.cond, pc, stub_ipa);
        if let Err(e) = code_write_word(pc, patched) {
            kprintln!(
                "shadow_stub: FATAL - couldn't write patched insn at PC {:#x}: {}",
                pc, e
            );
            crate::cpu::halt();
        }

        match decoded.kind {
            AccessKind::Ldrb | AccessKind::Strb => stats.ldrb_strb += 1,
            AccessKind::Ldrh | AccessKind::Strh => stats.ldrh_strh += 1,
            AccessKind::Ldrsb | AccessKind::Ldrsh => stats.ldrsb_ldrsh += 1,
            AccessKind::Swpb => stats.swpb += 1,
        }
        stats.patched += 1;

        pc = pc.wrapping_add(4);
    }

    let slots_used_a = NEXT_SLOT_A.load(Ordering::SeqCst);
    if slots_used_a > 0 {
        icache_sync_range(pool_host_pa(), slots_used_a * STUB_SLOT_SIZE);
    }
    let slots_used_b = NEXT_SLOT_B.load(Ordering::SeqCst);
    if slots_used_b > 0 {
        icache_sync_range(pool_b_host_pa(), slots_used_b * STUB_SLOT_SIZE);
    }
    // Sync the patched code region, whether ROM or RAM.
    if (end_ipa as usize) <= crate::guest_mem::ROM_SIZE {
        let rom_host = crate::guest_mem::rom_host_pa() + start_ipa as u64;
        icache_sync_range(rom_host, (end_ipa - start_ipa) as usize);
    } else {
        let ram_base = crate::guest_mem::RAM_IPA_BASE;
        if start_ipa >= ram_base {
            let ram_host = crate::guest_mem::ram_host_pa()
                + (start_ipa - ram_base) as u64;
            icache_sync_range(ram_host, (end_ipa - start_ipa) as usize);
        }
    }

    stats
}

pub fn log_stats(stats: &PatchStats) {
    kprintln!(
        "shadow_stub: scanned {} words, patched {} insns \
         (LDRB/STRB={}, LDRH/STRH={}, LDRSB/LDRSH={}, SWPB={}), \
         skipped {} PC-operand, pool A {}/{}, pool B {}/{}",
        stats.words_scanned, stats.patched,
        stats.ldrb_strb, stats.ldrh_strh, stats.ldrsb_ldrsh, stats.swpb,
        stats.skipped_pc_operand,
        NEXT_SLOT_A.load(Ordering::SeqCst), STUB_POOL_CAPACITY,
        NEXT_SLOT_B.load(Ordering::SeqCst), STUB_POOL_CAPACITY,
    );
}

/// Validation entry point (item 6). Consumes a list of 32-bit PCs the
/// real Einstein JIT translated during a boot run, and checks that
/// every PC in [range_start, range_end) is either patched (i.e., was
/// decoded as byte/halfword/SWPB by our decoder) or was not a
/// byte/halfword access in the first place. Any PC Einstein translated
/// that our decoder rejected halts loudly.
///
/// The PC list is in little-endian u32 units. `pc_list` is typically
/// the contents of `probe/translated-pcs-717006.bin`; we embed it at
/// compile time via the probe-side integration (deferred; see writeup).
///
/// Returns the number of PCs validated. Halts on any miss.
#[cfg(feature = "validate_with_probe")]
pub fn validate_against_probe(
    pc_list: &[u32], range_start: u32, range_end: u32,
) -> usize {
    let mut n = 0;
    for &pc in pc_list {
        if pc < range_start || pc >= range_end { continue; }
        let insn = match code_read_word(pc) {
            Some(w) => w,
            None => continue,
        };
        // After patching, the site holds a Bcc to the stub pool. Read
        // from an "unpatched" shadow copy if available — for now we
        // accept "Bcc stub_ipa" as proof the site was patched.
        let is_patched_branch = {
            let op = insn & 0x0F00_0000;
            let target_in_stub_range = {
                // Decode imm24, sign-extend, compute target.
                let imm24 = insn & 0x00FF_FFFF;
                let signed = if imm24 & 0x0080_0000 != 0 {
                    (imm24 | 0xFF00_0000) as i32
                } else {
                    imm24 as i32
                };
                let tgt = pc.wrapping_add(8).wrapping_add((signed << 2) as u32);
                is_stub_ipa(tgt)
            };
            op == 0x0A00_0000 && target_in_stub_range
        };
        if is_patched_branch { n += 1; continue; }
        // Not a branch-to-stub. Re-decode to confirm it was legitimately
        // skipped.
        if decode(insn).is_none() {
            // Fine - decoder doesn't consider this a byte/halfword access.
            n += 1;
            continue;
        }
        kprintln!(
            "shadow_stub: VALIDATION MISS - PC {:#x} insn {:#010x} \
             was translated by Einstein but our decoder left it unpatched",
            pc, insn
        );
        crate::cpu::halt();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_ldrb_immediate() {
        let d = decode(0xE5D1_0004).unwrap();
        assert_eq!(d.kind, AccessKind::Ldrb);
        assert_eq!(d.rn, 1);
        assert_eq!(d.rt, 0);
    }

    #[test]
    fn decode_strb_register() {
        let d = decode(0xE7C1_3102).unwrap();
        assert_eq!(d.kind, AccessKind::Strb);
    }

    #[test]
    fn decode_swpb() {
        // SWPB r0, r1, [r2] -> E142_0091
        let d = decode(0xE142_0091).unwrap();
        assert_eq!(d.kind, AccessKind::Swpb);
        assert_eq!(d.rn, 2);
        assert_eq!(d.rt, 0);
        assert_eq!(d.rt2, 1);
    }

    #[test]
    fn decode_swp_word_not_matched() {
        // SWP (word) -> E102_0091
        assert!(decode(0xE102_0091).is_none());
    }

    #[test]
    fn decode_ldrh_immediate() {
        let d = decode(0xE1D1_00B4).unwrap();
        assert_eq!(d.kind, AccessKind::Ldrh);
    }
}
