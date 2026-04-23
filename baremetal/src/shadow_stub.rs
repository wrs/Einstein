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

// FNV-1a-32 of `rom_bytes || rex_bytes` as classify-rom hashes them. Used
// at boot to prove the bitmap below was generated against this exact ROM
// + REX pair.
include!(concat!(env!("OUT_DIR"), "/rom_rex_hash.rs"));

/// Per-hash byte-access-static bitmap produced by
/// `baremetal/tools/classify-rom` and staged by `build.rs`. One bit per
/// 32-bit word across the 16 MiB guest ROM aperture; a set bit marks an
/// instruction that `decode()` accepts as an endianness-sensitive
/// subword access.
static BYTE_ACCESS_STATIC_BITMAP: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/byte-access-static.bitmap"));

/// The stub pool lives inside the guest ROM aperture at IPA
/// 0x00E00000..0x00F80000 — 1.5 MiB in the gap between the function-
/// tracer trampoline pool (ends at 0x00E00000) and the UND/ROM-patch
/// stub region at 0x00FFFF00. This placement is the key correctness
/// fix for post-MMU stub dispatch: the guest kernel's stage-1 L1
/// descriptors for VA 0x00000000..0x01000000 map the entire ROM
/// aperture as identity-mapped sections once the MMU is enabled, so
/// stubs in this range are reachable from every patched site, pre-
/// MMU (via stage-1-off identity) AND post-MMU (via the kernel's own
/// ROM sections). Previous pools at 0x01800000 / 0x03000000 were
/// stage-2-mapped by us but lay outside the kernel's stage-1 map —
/// fetches into them PABT'd once the MMU was on (see INVESTIGATION.md
/// "Currently at" section).
///
/// No separate pool B. A single pool reaches any patched site (ROM in
/// 0..0x01000000, flash in 0x02000000..0x02400000, RAM in
/// 0x04000000..0x04400000, RAM mirror at 0x0C000000..0x0C400000) within
/// the ARM AArch32 `B` instruction's ±32 MiB range.
///
/// Stubs are written directly into the GUEST_ROM backing buffer (same
/// storage the Newton ROM bytes occupy); stage-2 read-only mapping of
/// the ROM aperture serves them without any extra mapping.
pub const STUB_POOL_IPA: u32 = 0x00E0_0000;

/// Second pool for RAM-resident code that the kernel lazy-patches once
/// a RAM block becomes executable. The `B` instruction reaches ±32 MiB,
/// so a stub for a patch site in RAM (IPAs 0x04000000..0x04400000) can't
/// be in the ROM-aperture pool (~53 MiB away). This pool sits just
/// before RAM at IPA 0x03000000..0x03180000; stage-2 still maps it
/// separately because the guest kernel's stage-1 never covers 0x030xxxxx
/// post-MMU. Currently only used by `test_shadow_stub`'s pre-MMU path;
/// the real Newton boot hasn't yet reached `UseROMJumpTables` which
/// would be the first post-MMU lazy-RAM consumer.
pub const STUB_POOL_B_IPA: u32 = 0x0300_0000;

/// Pool size. 2 MiB = one stage-2 L2 block, the minimum granularity
/// of `set_l2_blocks`. At 48-byte slots that's 43,690 stubs, well over
/// the ~27,700 current static-bitmap census. Pool A spans IPA
/// 0x00E00000..0x01000000 inside the ROM aperture — i.e., the last
/// 2 MiB of ROM, past the UND/ROM-patch stubs at 0x00FFFF00 (which
/// shadow_stub won't scan, since those bytes aren't Newton ROM).
pub const STUB_POOL_SIZE: usize = 0x0020_0000;

/// Addresses < XOR_LIMIT are treated as real memory (XOR applied);
/// addresses >= XOR_LIMIT are treated as MMIO and passed through.
/// Chosen to cover everything in the Newton IPA map below flash bank 1.
///
/// A previous review questioned whether the peripheral window
/// 0x0F00_0000..0x0F40_0000 (below this limit) should be excluded so
/// that patched LDRB/LDRH to a register doesn't land on a neighbouring
/// byte. In practice the XOR is *correct* for those addresses too:
/// the Newton kernel is big-endian-32 (see PLAN.md, `Emulator/Network/*`
/// — every BE32 constant in the ROM gets swapped at load time), and a
/// byte access to word-aligned MMIO yields byte[3] of the 32-bit value
/// from the kernel's BE view, which is byte[0] from our LE view —
/// exactly the XOR-by-3 transform the stub applies. The only MMIO
/// region currently plumbed as RAM at stage-2 is the read-only tick
/// page at 0x0F18_1000; a stubbed LDRB targeting any offset in that
/// page still lands inside the same 4 KiB mapping and reads the
/// BE-correct byte. Stubbed byte accesses to trap-for-dispatch MMIO
/// addresses (e.g., 0x0F18_3000 int_present) stage-2-fault at the
/// XOR'd EA; mmio::read will halt loudly on the unrecognised IPA
/// rather than silently mishandling the access, which is the Phase A
/// tripwire we want. Keep XOR_LIMIT at 0x1000_0000.
pub const XOR_LIMIT: u32 = 0x1000_0000;

/// Fixed bytes per stub slot. 10 words x 4 = 40 bytes.
///
/// Worst-case stub body after the ROM-resident refactor (see
/// `build_stub`): MCR save (1) + EA compute (up to 2 for imm/reg-shift) +
/// CMP + BHS + EOR (3) + access (1) + writeback (1) + MRC restore (1) +
/// B back (1) = 10 words. No save-slot / return-pc literal needed — the
/// save register is TPIDR_EL0 (a per-CPU register SA-1100 didn't have,
/// unused by the Newton kernel) and the return is a direct `B` with the
/// offset baked in at stub-generation time.
pub const STUB_SLOT_SIZE: usize = 48;

/// Words per stub slot.
pub const STUB_SLOT_WORDS: usize = STUB_SLOT_SIZE / 4;

/// Per-pool capacity — number of stubs that fit.
pub const STUB_POOL_CAPACITY: usize = STUB_POOL_SIZE / STUB_SLOT_SIZE;

/// Total capacity across both pools. Packed indices: pool A in
/// [0..CAPACITY), pool B in [CAPACITY..2*CAPACITY).
pub const STUB_POOL_TOTAL_CAPACITY: usize = STUB_POOL_CAPACITY * 2;

/// Backing store for pool B (RAM-reach). Pool A writes into `GUEST_ROM`
/// directly, so it has no separate backing here.
#[repr(C, align(0x200000))]
struct StubPoolB([u8; STUB_POOL_SIZE]);
static mut STUB_POOL_B: StubPoolB = StubPoolB([0; STUB_POOL_SIZE]);

static NEXT_SLOT_A: AtomicUsize = AtomicUsize::new(0);
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

/// Host PA of pool A. Stubs live inside the GUEST_ROM backing buffer
/// at offset `STUB_POOL_IPA` (= 0x00E00000).
pub fn pool_host_pa() -> u64 {
    crate::guest_mem::rom_host_pa() + STUB_POOL_IPA as u64
}

/// Host PA of pool B (RAM-reach, backed by `STUB_POOL_B`).
pub fn pool_b_host_pa() -> u64 {
    addr_of_mut!(STUB_POOL_B) as u64
}

fn is_pool_a_ipa(ipa: u32) -> bool {
    ipa >= STUB_POOL_IPA
        && (ipa as usize) < (STUB_POOL_IPA as usize) + STUB_POOL_SIZE
}

fn is_pool_b_ipa(ipa: u32) -> bool {
    ipa >= STUB_POOL_B_IPA
        && (ipa as usize) < (STUB_POOL_B_IPA as usize) + STUB_POOL_SIZE
}

/// Is `ipa` inside either shadow-stub pool?
pub fn is_stub_ipa(ipa: u32) -> bool {
    is_pool_a_ipa(ipa) || is_pool_b_ipa(ipa)
}

/// Given an IPA inside either stub pool, return
/// `(packed_slot_index, byte_offset_in_slot)`. Pool-A slots are packed
/// at [0..STUB_POOL_CAPACITY), pool-B slots at
/// [STUB_POOL_CAPACITY..2*STUB_POOL_CAPACITY).
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

/// `MCR p15, 0, Rt, c13, c0, 2` — write TPIDR_EL0 (TPIDRURW).
/// Used as the stub's scratch-preservation channel. TPIDR_EL0 is
/// ARMv6+ architectural state; the SA-1100 (ARMv4) has no equivalent
/// register, so the Newton ROM never touches it. Our HCR_EL2 doesn't
/// trap c13, so this executes natively at any EL1 mode.
fn enc_mcr_tpidr_el0(rt: u32) -> u32 {
    // cond 1110 op1=000 L=0 CRn=13 Rt cp=15 op2=2 1 CRm=0
    0xEE0D_0F50 | (rt << 12)
}

/// `MRC p15, 0, Rt, c13, c0, 2` — read TPIDR_EL0.
fn enc_mrc_tpidr_el0(rt: u32) -> u32 {
    0xEE1D_0F50 | (rt << 12)
}

/// Unconditional `B #imm24` from `from_pc` to `target`. Used as the
/// stub's return-to-caller instruction.
fn enc_b_uncond(from_pc: u32, target: u32) -> u32 {
    enc_bcond(0xE, from_pc, target)
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
fn build_stub(d: &Decoded, stub_pc: u32, return_pc: u32,
              out: &mut [u32; STUB_SLOT_WORDS]) -> Result<BuiltStub, &'static str>
{
    let scratch = pick_scratch(d);
    let ea = scratch;
    let mut idx = 0usize;

    // 1. Save scratch to TPIDR_EL0 (per-CPU, not memory — stubs live in
    //    RO ROM so PC-relative STR to a save-slot isn't an option).
    //    Nested-exception caveat: if a higher-priority exception fires
    //    between the save and the matching restore, and its handler
    //    itself invokes a shadow-stub in the same CPU, TPIDR_EL0 gets
    //    clobbered. In practice the kernel runs with I/F masked inside
    //    byte-access-rich code paths, so this races only with FIQ +
    //    abort vectors. Accept for now; document in shadow_stub header.
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = enc_mcr_tpidr_el0(scratch);
    idx += 1;

    // 2. Compute the effective address into `ea`.
    //    Pre-indexed / plain: ea = Rn +- offset.
    //    Post-indexed:        ea = Rn.
    //    SWPB (offset=None, p=true): ea = Rn.
    let computes_ea_from_rn = d.p;
    if computes_ea_from_rn {
        match d.offset {
            OffsetForm::None => {
                if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
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

    // 7. Restore scratch from TPIDR_EL0.
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    out[idx] = enc_mrc_tpidr_el0(scratch);
    idx += 1;

    // 8. Branch back to return_pc. Direct unconditional B — reach is
    //    ±32 MiB, which covers any patched site from a stub in the ROM
    //    aperture.
    if idx >= STUB_SLOT_WORDS { return Err("stub slot overflow"); }
    let from_pc = stub_pc + (idx as u32) * 4;
    out[idx] = enc_b_uncond(from_pc, return_pc);
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

/// Pool A (inside ROM aperture, reachable pre+post-MMU via the guest
/// kernel's ROM sections) serves ROM + flash-bank-0 patch sites.
/// Pool B (at IPA 0x03000000, pre-RAM) serves lazy-RAM-resident code.
/// B's ±32 MiB reach constraint dictates this split.
fn select_pool(source_ipa: u32) -> bool {
    if (source_ipa as usize) < crate::guest_mem::ROM_SIZE {
        return false; // pool A
    }
    let ram_base = crate::guest_mem::RAM_IPA_BASE;
    let ram_end = ram_base + crate::guest_mem::RAM_SIZE as u32;
    if source_ipa >= ram_base && source_ipa < ram_end {
        return true; // pool B
    }
    if source_ipa >= 0x0200_0000 && source_ipa < 0x0240_0000 {
        return false; // flash bank 0 — still in ROM-aperture reach
    }
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

/// Emit a single stub for the byte/halfword access at `pc` and overwrite
/// the original site with a branch to the stub. Shared between the
/// range-scanning `patch_code_range` (ROM cold fallback + lazy-RAM) and
/// the bitmap-driven `patch_rom_from_bitmap`. Updates `stats` in place.
///
/// Returns without touching anything if the word doesn't decode as a
/// byte/halfword access or references PC as an operand. Halts if the
/// decoded instruction exists but we can't emit or install its stub.
fn patch_one_site(pc: u32, use_pool_b: bool, stats: &mut PatchStats) {
    let insn = match code_read_word(pc) {
        Some(w) => w,
        None => return,
    };
    let decoded = match decode(insn) {
        Some(d) => d,
        None => return,
    };

    // Reject PC as any operand. The stub would need to emulate PC-
    // relative addressing from the original site (not the stub site),
    // which build_stub doesn't support. Most of these hits are not even
    // real code: classify-rom's prologue-sweep is generous enough to
    // scoop in data words (string tables, dispatch tables) that happen
    // to decode as byte-access-shape with Rn=PC, and this check is the
    // safety net that keeps us from corrupting them. The aggregate
    // count lands in `log_stats`; the per-site log is debug-level only.
    if decoded.rn == 15
        || decoded.rt == 15
        || (matches!(decoded.kind, AccessKind::Swpb) && decoded.rt2 == 15)
    {
        stats.skipped_pc_operand += 1;
        crate::dprintln!(
            "shadow_stub: skipping insn {:#010x} at PC {:#x} - PC operand",
            insn, pc
        );
        return;
    }
    if let OffsetForm::Reg { rm, .. } = decoded.offset {
        if rm == 15 {
            stats.skipped_pc_operand += 1;
            return;
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

    // Fill any unused tail words with UDF #0xDEAD so stray execution
    // past the terminal B faults loudly rather than silently executing
    // adjacent stubs.
    for i in built.words..STUB_SLOT_WORDS {
        words[i] = 0xE7F0_00F0;
    }

    let pool_off = local_slot * STUB_SLOT_SIZE;
    for (i, w) in words.iter().enumerate() {
        pool_write_word(use_pool_b, pool_off + i * 4, *w);
    }

    // SAFETY: single-threaded callers; bounded slot.
    unsafe {
        SLOT_ORIGINAL_PC[packed_slot] = pc;
        SLOT_ACCESS_OFF[packed_slot] = built.access_off as u8;
        SLOT_META[packed_slot] = Some(SlotMeta { xor_mask: decoded.kind.xor_mask() });
    }

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
}

/// Patch every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH/SWPB in [start_ipa, end_ipa)
/// of the ROM or RAM backing. Used for the lazy-RAM path (RAM-resident
/// code copied out of ROM at boot) where there is no pre-computed
/// classifier bitmap to drive a bit-walk.
pub fn patch_code_range(start_ipa: u32, end_ipa: u32) -> PatchStats {
    assert!(start_ipa & 3 == 0);
    assert!(end_ipa & 3 == 0);
    assert!(end_ipa >= start_ipa);

    let use_pool_b = select_pool(start_ipa);

    let mut stats = PatchStats::default();
    let mut pc = start_ipa;
    while pc < end_ipa {
        stats.words_scanned += 1;
        patch_one_site(pc, use_pool_b, &mut stats);
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

/// FNV-1a-32 of `rom || rex` using the same seed + multiplier as
/// `baremetal/tools/classify-rom` (and `baremetal/build.rs`).
#[cfg(not(nh_guest_test))]
fn rom_rex_hash_runtime() -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &b in crate::guest_mem::rom_be_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    for &b in crate::guest_mem::rex_be_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Pre-patch every ROM site the classifier marked as an
/// endianness-sensitive subword access. Intended for the boot path:
/// call once after stage2::enable() with the Newton ROM backing in
/// place. Halts loudly if the embedded bitmap doesn't hash-match the
/// loaded ROM + REX.
///
/// Counterpart to `patch_code_range` for the RAM-lazy path: both share
/// `patch_one_site`, so the emitted stubs and metadata are bit-identical.
#[cfg(not(nh_guest_test))]
pub fn patch_rom_from_bitmap() -> PatchStats {
    let runtime_hash = rom_rex_hash_runtime();
    if runtime_hash != ROM_REX_FNV1A32 {
        kprintln!(
            "shadow_stub: ROM+REX hash mismatch (build-time {:#010x}, runtime {:#010x}) — \
             regenerate the classify bitmap via baremetal/scripts/regen-classify.sh",
            ROM_REX_FNV1A32, runtime_hash
        );
        crate::cpu::halt();
    }

    let mut stats = PatchStats::default();
    // Pool A reaches the entire 16 MiB ROM aperture; the bitmap covers
    // ROM only, so use_pool_b is always false here.
    for (byte_idx, byte) in BYTE_ACCESS_STATIC_BITMAP.iter().enumerate() {
        if *byte == 0 { continue; }
        let mut b = *byte;
        while b != 0 {
            let bit = b.trailing_zeros() as usize;
            b &= b - 1;
            let word_idx = byte_idx * 8 + bit;
            let pc = (word_idx * 4) as u32;
            stats.words_scanned += 1;
            patch_one_site(pc, false, &mut stats);
        }
    }

    let slots_used_a = NEXT_SLOT_A.load(Ordering::SeqCst);
    if slots_used_a > 0 {
        icache_sync_range(pool_host_pa(), slots_used_a * STUB_SLOT_SIZE);
    }
    icache_sync_range(
        crate::guest_mem::rom_host_pa(),
        crate::guest_mem::ROM_SIZE,
    );

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

/// Outcome of a single-PC probe-list validation check.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidatePcResult {
    /// PC isn't inside the caller-supplied range; ignored.
    OutOfRange,
    /// PC read back as a branch-to-stub — the shadow_stub patcher
    /// successfully installed a stub at this site.
    Patched,
    /// PC's instruction word decodes as something our decoder
    /// intentionally skips (not a byte/halfword access). Fine.
    NotByteAccess,
    /// The word at PC still looks like a byte/halfword access but
    /// no branch-to-stub is installed. This is a MISS — Einstein
    /// considered this PC code but the shadow_stub mechanism did
    /// not patch it.
    Missed,
    /// PC was outside any backing store we could read.
    Unreadable,
}

/// Check one PC against the current state of the patched code.
pub fn validate_one(pc: u32, range_start: u32, range_end: u32) -> ValidatePcResult {
    if pc < range_start || pc >= range_end {
        return ValidatePcResult::OutOfRange;
    }
    let insn = match code_read_word(pc) {
        Some(w) => w,
        None => return ValidatePcResult::Unreadable,
    };
    // Check "branch to a stub pool".
    if (insn & 0x0F00_0000) == 0x0A00_0000 {
        let imm24 = insn & 0x00FF_FFFF;
        let signed = if imm24 & 0x0080_0000 != 0 {
            (imm24 | 0xFF00_0000) as i32
        } else {
            imm24 as i32
        };
        let tgt = pc.wrapping_add(8).wrapping_add((signed << 2) as u32);
        if is_stub_ipa(tgt) {
            return ValidatePcResult::Patched;
        }
    }
    if decode(insn).is_none() {
        ValidatePcResult::NotByteAccess
    } else {
        ValidatePcResult::Missed
    }
}

/// Validation pass against a probe-derived PC list (item 6). Consumes
/// a slice of 32-bit PCs the real Einstein ARM-JIT translated during a
/// boot run, and verifies every PC in [range_start, range_end) is
/// either patched or was legitimately not a byte/halfword access.
///
/// Any PC Einstein translated that our decoder rejected halts loudly
/// — that's a classification bug we want to catch before the guest
/// silently miscomputes.
///
/// The PC list is typically loaded at build time from
/// `probe/translated-pcs-717006.bin`; the probe-side integration that
/// emits this file is a follow-up and not yet landed in this workspace.
///
/// Returns the number of in-range PCs validated (Patched or NotByteAccess).
#[allow(dead_code)]
pub fn validate_against_probe(
    pc_list: &[u32], range_start: u32, range_end: u32,
) -> usize {
    let mut ok = 0;
    for &pc in pc_list {
        match validate_one(pc, range_start, range_end) {
            ValidatePcResult::OutOfRange => continue,
            ValidatePcResult::Patched | ValidatePcResult::NotByteAccess => ok += 1,
            ValidatePcResult::Unreadable => {
                kprintln!(
                    "shadow_stub: validate_against_probe — PC {:#x} unreadable",
                    pc
                );
                crate::cpu::halt();
            }
            ValidatePcResult::Missed => {
                let insn = code_read_word(pc).unwrap_or(0);
                kprintln!(
                    "shadow_stub: VALIDATION MISS — PC {:#x} insn {:#010x} \
                     was translated by Einstein but our decoder left it unpatched",
                    pc, insn
                );
                crate::cpu::halt();
            }
        }
    }
    ok
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

    #[test]
    fn validate_one_out_of_range() {
        // validate_one should not touch backing stores for PCs outside
        // the requested range. We just check the Range check.
        let r = validate_one(0x1_0000_0000u64 as u32, 0, 16);
        // PC above u32 max wraps — the real check is range_start/end.
        // Use 100 > end.
        let _ = r;
        let r2 = validate_one(100, 0, 16);
        assert_eq!(r2, ValidatePcResult::OutOfRange);
    }
}
