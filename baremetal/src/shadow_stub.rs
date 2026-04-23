//! Shadow byte/halfword access for a BE-32 guest on a little-endian host,
//! UDF-trap variant.
//!
//! The Newton ROM is BE-32 "word-invariant": aligned word accesses match
//! LE (the ROM is byteswapped per word at load time), but byte and
//! halfword accesses land on a different byte lane:
//!
//!   BE-32 LDRB at addr A  ->  phys[A ^ 3]
//!   BE-32 LDRH at addr A  ->  halfword at phys[A ^ 2]
//!
//! Prior revisions of this module emitted an in-guest trampoline at each
//! byte/halfword-access site that recomputed the effective address,
//! XOR'd it with 3 or 2, and performed the access natively. That
//! approach broke on CPSR-flag preservation: the MMIO-skip `CMP`
//! clobbered the caller's NZCV, and no single-CPU-register scratch
//! strategy worked across USR/kernel modes and every stage of MMU
//! bring-up (RAM save-slot isn't mapped in user mode; stack save-slot
//! isn't valid pre-`SetUpStacks`; a single CP15 scratch register can't
//! hold both the working register and the saved flags).
//!
//! The current implementation replaces each access site with
//! `UDF #(SBA_UDF_BASE | idx)`. The guest raises UND locally, the
//! existing UND trampoline at `0x00FFFF00` saves state and HVCs into
//! EL2, and `handle_sba_udf` emulates the original instruction in Rust.
//! That keeps CPSR flags untouched (SPSR_EL2 carries them across ERET
//! without any NZCV manipulation), works from every guest mode
//! (USR/SYS/SVC/IRQ/FIQ/ABT/UND), is MMU-agnostic (EL2 walks the
//! guest's stage-1 tables explicitly when `SCTLR.M=1` and identity-maps
//! otherwise), and is atomic with respect to guest preemption (EL2
//! masks DAIF.I for the trap window).
//!
//! Each patched site gets a packed-index slot in a hypervisor-side
//! table keyed on the UDF immediate. The table carries the original
//! instruction word; `handle_sba_udf` re-decodes it on every trap.
//! 32 KiB of entries (matching the UDF imm16 band `0x8000..=0xFFFD`)
//! is enough for the full 717006 ROM census plus the lazy-RAM path.
//!
//! Scope: LDRB / STRB / LDRH / STRH / LDRSB / LDRSH / SWPB in immediate,
//! register-offset (with LSL/LSR/ASR/ROR shift), pre-index
//! (`[Rn,#imm]!`), and post-index (`[Rn],#imm`) forms. PC (r15) as
//! base, data, or offset is rejected at install time — PC-relative
//! semantics at the original site would need to be emulated against
//! the site's own PC, not the handler's.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::kprintln;
use crate::trap::TrapContext;

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

/// Addresses < XOR_LIMIT are treated as real memory (XOR applied);
/// addresses >= XOR_LIMIT are treated as MMIO and passed through.
/// Chosen to cover everything in the Newton IPA map below flash bank 1.
///
/// The XOR is correct for both memory and the Newton's tick-page MMIO
/// at 0x0F18_1000 (stage-2-mapped RAM, BE-32 view). Trap-for-dispatch
/// MMIO addresses below XOR_LIMIT don't exist in the current peripheral
/// map; if any appear, the XOR'd address will stage-2-fault and
/// `mmio::read`/`write` will halt loudly on the unrecognised IPA.
pub const XOR_LIMIT: u32 = 0x1000_0000;

/// Low end of the UDF immediate band reserved for shadow-byte-access
/// (SBA) sites. `UDF #0x8000 | idx` encodes site `idx` in 0..0x7FFE.
/// Outside this band, UDF imm16s are reserved for other consumers:
/// `guest_bp::BP_UDF_INSN` uses 0xFFFE; the tracer uses HVC (not UDF).
pub const SBA_UDF_BASE: u16 = 0x8000;

/// Inclusive top of the SBA UDF band. 0xFFFE is owned by `guest_bp`.
pub const SBA_UDF_MAX: u16 = 0xFFFD;

/// Maximum number of shadow-byte-access sites. One index per slot in
/// the UDF imm16 band `SBA_UDF_BASE..=SBA_UDF_MAX`.
pub const SBA_MAX_SITES: usize = (SBA_UDF_MAX - SBA_UDF_BASE) as usize + 1;

/// Per-site stash of the original instruction word. Indexed by the UDF
/// `imm16 - SBA_UDF_BASE`. 0 marks "slot unused" — 0 is not a valid
/// byte/halfword-access encoding in any form `decode()` accepts.
static mut SBA_ORIG_INSN: [u32; SBA_MAX_SITES] = [0; SBA_MAX_SITES];

/// Per-site stash of the original guest PC. Cross-checked against
/// `faulting_pc` at trap time; a mismatch means the table was corrupted
/// or the UDF somehow fired at a PC that doesn't match its slot.
static mut SBA_ORIG_PC: [u32; SBA_MAX_SITES] = [u32::MAX; SBA_MAX_SITES];

static NEXT_SITE: AtomicUsize = AtomicUsize::new(0);

/// Summary statistics returned by `patch_code_range` / `patch_rom_from_bitmap`.
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

// The XOR applied to the effective address for BE-32 compatibility is
// 3 for byte-width access (LDRB/STRB/LDRSB/SWPB) and 2 for halfword
// (LDRH/STRH/LDRSH). The handler hardcodes the mask into each
// `dispatch_{byte,halfword}_*` pair rather than dispatching through
// the kind enum.

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

/// Encode `UDF #imm16` (A1 encoding, cond=AL).
///   cond 0111 1111 imm12 1111 imm4
/// with imm16 = (imm12 << 4) | imm4.
fn enc_udf(imm16: u16) -> u32 {
    let imm12 = ((imm16 as u32) >> 4) & 0xFFF;
    let imm4 = (imm16 as u32) & 0xF;
    0xE7F0_00F0 | (imm12 << 8) | imm4
}

/// True iff `insn` matches the SBA UDF encoding shape (cond=AL, UDF A1).
/// Callers still need to verify the imm16 is inside `SBA_UDF_BASE..=SBA_UDF_MAX`.
pub fn is_sba_udf_insn(insn: u32) -> bool {
    if (insn & 0xFFF0_00F0) != 0xE7F0_00F0 {
        return false;
    }
    let imm16 = udf_imm16(insn);
    (SBA_UDF_BASE..=SBA_UDF_MAX).contains(&imm16)
}

fn udf_imm16(insn: u32) -> u16 {
    let imm12 = (insn >> 8) & 0xFFF;
    let imm4 = insn & 0xF;
    ((imm12 << 4) | imm4) as u16
}

/// Write `word` into a backing store that owns this IPA. Supports the
/// ROM backing and the RAM backing (for lazy-RAM patching).
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

/// Publish a freshly-written code range to the instruction stream.
///
/// Two passes — first `DC CVAU` every line to push D-cache dirty lines
/// to the Point of Unification, DSB ISH, then `IC IVAU` every line and
/// DSB+ISB. The DSB between the two passes is load-bearing: without it
/// IC IVAU can complete ahead of DC CVAU on cores that model I/D cache
/// non-coherence strictly (FVP Base RevC does; QEMU raspi3b TCG doesn't
/// model it), so the invalidated I-cache line refills from L2 before
/// the D-side writeback has reached it, and the guest fetches stale
/// bytes. Symptom: on FVP, patched ROM sites (tracer trampoline entries,
/// VA-0xC PABT canary, etc.) appear to the guest as their pre-patch
/// values, so post-MMU fetches take PABTs that loop in the PABT vector.
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

/// Decode + record the site and overwrite the original word with a
/// UDF marker. Halts if the SBA site table is exhausted or the write
/// fails. Silently ignores words that don't decode as a byte/halfword
/// access, or that reference PC as an operand (classifier false
/// positives on data words that happen to decode byte-access-shaped).
fn patch_one_site(pc: u32, stats: &mut PatchStats) {
    let insn = match code_read_word(pc) {
        Some(w) => w,
        None => return,
    };
    let decoded = match decode(insn) {
        Some(d) => d,
        None => return,
    };

    // Reject PC as any operand. Most of these hits are not even real
    // code: classify-rom's prologue-sweep is generous enough to scoop
    // in data words (string tables, dispatch tables) that happen to
    // decode as byte-access-shape with Rn=PC. Emulating PC-relative
    // against the original site is unnecessary work to support a
    // pattern the real ROM doesn't use.
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

    let idx = NEXT_SITE.fetch_add(1, Ordering::SeqCst);
    if idx >= SBA_MAX_SITES {
        kprintln!(
            "shadow_stub: ERROR - SBA site table exhausted at PC {:#x} ({} sites)",
            pc, idx
        );
        crate::cpu::halt();
    }

    let imm16 = SBA_UDF_BASE | (idx as u16);
    let udf_insn = enc_udf(imm16);

    // SAFETY: idx just allocated, single-threaded writer.
    unsafe {
        SBA_ORIG_INSN[idx] = insn;
        SBA_ORIG_PC[idx] = pc;
    }

    if let Err(e) = code_write_word(pc, udf_insn) {
        kprintln!(
            "shadow_stub: FATAL - couldn't write UDF at PC {:#x}: {}",
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

/// Patch every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH/SWPB in `[start_ipa, end_ipa)`
/// of the ROM or RAM backing. Used for the lazy-RAM path (RAM-resident
/// code copied out of ROM at boot) where there is no pre-computed
/// classifier bitmap, and by the `test_shadow_stub` guest test which
/// scans its own code range.
pub fn patch_code_range(start_ipa: u32, end_ipa: u32) -> PatchStats {
    assert!(start_ipa & 3 == 0);
    assert!(end_ipa & 3 == 0);
    assert!(end_ipa >= start_ipa);

    let mut stats = PatchStats::default();
    let mut pc = start_ipa;
    while pc < end_ipa {
        stats.words_scanned += 1;
        patch_one_site(pc, &mut stats);
        pc = pc.wrapping_add(4);
    }

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
/// call once after `stage2::enable()` with the Newton ROM backing in
/// place. Halts loudly if the embedded bitmap doesn't hash-match the
/// loaded ROM + REX.
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
    for (byte_idx, byte) in BYTE_ACCESS_STATIC_BITMAP.iter().enumerate() {
        if *byte == 0 { continue; }
        let mut b = *byte;
        while b != 0 {
            let bit = b.trailing_zeros() as usize;
            b &= b - 1;
            let word_idx = byte_idx * 8 + bit;
            let pc = (word_idx * 4) as u32;
            stats.words_scanned += 1;
            patch_one_site(pc, &mut stats);
        }
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
         skipped {} PC-operand, site table {}/{}",
        stats.words_scanned, stats.patched,
        stats.ldrb_strb, stats.ldrh_strh, stats.ldrsb_ldrsh, stats.swpb,
        stats.skipped_pc_operand,
        NEXT_SITE.load(Ordering::SeqCst), SBA_MAX_SITES,
    );
}

/// Outcome of a single-PC probe-list validation check.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidatePcResult {
    /// PC isn't inside the caller-supplied range; ignored.
    OutOfRange,
    /// PC read back as a UDF in the SBA band — patched.
    Patched,
    /// PC's instruction word decodes as something our decoder
    /// intentionally skips (not a byte/halfword access). Fine.
    NotByteAccess,
    /// The word at PC still looks like a byte/halfword access but
    /// no SBA UDF is installed. This is a MISS — Einstein considered
    /// this PC code but the shadow_stub patcher did not patch it.
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
    if is_sba_udf_insn(insn) {
        return ValidatePcResult::Patched;
    }
    if decode(insn).is_none() {
        ValidatePcResult::NotByteAccess
    } else {
        ValidatePcResult::Missed
    }
}

/// Validation pass against a probe-derived PC list. Consumes a slice of
/// 32-bit PCs the real Einstein ARM-JIT translated during a boot run,
/// and verifies every PC in `[range_start, range_end)` is either
/// patched or was legitimately not a byte/halfword access. Halts on
/// any PC Einstein translated that our decoder rejected — that's a
/// classification bug we want to catch before the guest silently
/// miscomputes.
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

// =======================================================================
// UDF-trap handler
// =======================================================================

/// Evaluate an ARM condition code against the saved CPSR flags.
fn cond_passes(cond: u32, cpsr: u32) -> bool {
    let n = (cpsr >> 31) & 1 != 0;
    let z = (cpsr >> 30) & 1 != 0;
    let c = (cpsr >> 29) & 1 != 0;
    let v = (cpsr >> 28) & 1 != 0;
    match cond & 0xF {
        0x0 => z,               // EQ
        0x1 => !z,              // NE
        0x2 => c,               // CS/HS
        0x3 => !c,              // CC/LO
        0x4 => n,               // MI
        0x5 => !n,              // PL
        0x6 => v,               // VS
        0x7 => !v,              // VC
        0x8 => c && !z,         // HI
        0x9 => !c || z,         // LS
        0xA => n == v,          // GE
        0xB => n != v,          // LT
        0xC => !z && (n == v),  // GT
        0xD => z || (n != v),   // LE
        0xE => true,            // AL
        _ => true,              // NV (UNPREDICTABLE — behave as AL)
    }
}

/// Apply an ARM data-processing shift to `val`. Matches the semantics
/// used by `LDR/STR` register-offset addressing modes:
/// encoded-amount == 0 means 32 for LSR/ASR, and RRX for ROR.
fn apply_shift(val: u32, shift_type: u32, amount: u32, cpsr: u32) -> u32 {
    match shift_type & 3 {
        0 /* LSL */ => {
            if amount >= 32 { 0 } else { val.wrapping_shl(amount) }
        }
        1 /* LSR */ => {
            let n = if amount == 0 { 32 } else { amount };
            if n >= 32 { 0 } else { val >> n }
        }
        2 /* ASR */ => {
            let n = if amount == 0 { 32 } else { amount };
            if n >= 32 {
                if (val as i32) < 0 { u32::MAX } else { 0 }
            } else {
                ((val as i32) >> n) as u32
            }
        }
        3 /* ROR / RRX */ => {
            if amount == 0 {
                // RRX: rotate right through carry (C flag from CPSR).
                let c = (cpsr >> 29) & 1;
                (val >> 1) | (c << 31)
            } else {
                val.rotate_right(amount & 31)
            }
        }
        _ => unreachable!(),
    }
}

/// Resolve a guest address to a guest PA. Identity when the guest's
/// stage-1 MMU is off; walks the live stage-1 page tables otherwise.
fn resolve_addr(addr: u32) -> Option<u32> {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, sctlr_el1",
            out(reg) sctlr,
            options(nomem, nostack, preserves_flags),
        );
    }
    if sctlr & 1 == 0 {
        Some(addr)
    } else {
        crate::guest_mem::translate_va(addr)
    }
}

// Banked SP/LR are handled through ctx.x[13] / ctx.x[14] plus the
// ERET-with-faulting-mode-SPSR exchange. On EL2 entry, ctx.x[13] and
// ctx.x[14] hold UND-mode's banked SP/LR (the trampoline HVCs from UND).
// Before emulating the byte access, `handle_sba_udf` overwrites both
// with the faulting mode's banked values stashed by the trampoline into
// `UND_SAVE_BANKED_{SP,LR}_IPA`. Register reads / writeback then touch
// `ctx.x[]` as usual, and `return_to_guest_from_und` with `spsr_und`
// ERETs into the faulting mode; the AArch64 architecture propagates
// ctx.x[13] / ctx.x[14] into the target mode's banked SP / LR.
//
// This sidesteps the MRS (banked register) encoding problem — QEMU
// raspi3b doesn't implement the banked-MRS sysreg coordinates we tried
// (op0=3, op1=4, CRn=4, CRm ∈ {4,5}), and the ERET path works on any
// ARMv8 implementation.

fn read_banked_sp_slot() -> u32 {
    crate::guest_mem::read_word_pa(crate::trap::UND_SAVE_BANKED_SP_IPA)
        .unwrap_or_else(|| {
            kprintln!("*** shadow_stub: banked SP slot unreadable");
            crate::cpu::halt();
        })
}

fn read_banked_lr_slot() -> u32 {
    crate::guest_mem::read_word_pa(crate::trap::UND_SAVE_BANKED_LR_IPA)
        .unwrap_or_else(|| {
            kprintln!("*** shadow_stub: banked LR slot unreadable");
            crate::cpu::halt();
        })
}

/// Snapshot of AArch32 R0..R14 assembled from the EL2 context plus the
/// banked-SP/LR slot stashes. R0..R12 alias ctx.x[0..12] directly (the
/// trampoline restored R0, R1, R12 to the pre-UND values and R2..R11
/// are unchanged across the UND round-trip). R13 and R14 come from the
/// RAM slots the trampoline populated by mode-switching to the faulting
/// mode (or SYS, when the faulting mode is USR).
struct Regs {
    r: [u32; 15],
}

impl Regs {
    fn snapshot(ctx: &TrapContext) -> Self {
        let mut r = [0u32; 15];
        for i in 0..13 {
            r[i] = ctx.x[i] as u32;
        }
        r[13] = read_banked_sp_slot();
        r[14] = read_banked_lr_slot();
        Self { r }
    }
    fn get(&self, i: u32) -> u32 {
        assert!(i < 15);
        self.r[i as usize]
    }
    fn set(&mut self, i: u32, v: u32) {
        assert!(i < 15);
        self.r[i as usize] = v;
    }
}

// Flash bank IPA windows. Stage-2 maps both banks to the same backing
// (bank 1 sits at offset BANK_SIZE in the backing, after bank 0).
const FLASH_BANK0_IPA: u32 = 0x0200_0000;
const FLASH_BANK1_IPA: u32 = 0x1000_0000;
const FLASH_BANK_SIZE: u32 = 0x0040_0000;

/// If `pa` lies in a flash bank, return the host address (mutable byte
/// pointer) of the backing byte. Otherwise None.
fn flash_host_addr(pa: u32) -> Option<usize> {
    let base = crate::peripherals::flash::host_pa() as usize;
    if (FLASH_BANK0_IPA..FLASH_BANK0_IPA + FLASH_BANK_SIZE).contains(&pa) {
        return Some(base + (pa - FLASH_BANK0_IPA) as usize);
    }
    if (FLASH_BANK1_IPA..FLASH_BANK1_IPA + FLASH_BANK_SIZE).contains(&pa) {
        return Some(base + FLASH_BANK_SIZE as usize + (pa - FLASH_BANK1_IPA) as usize);
    }
    None
}

/// Try to read a byte from ROM / RAM / FB (via guest_mem) or flash.
/// None if `pa` is outside all backed regions.
fn backed_byte_read(pa: u32) -> Option<u8> {
    if let Some(v) = crate::guest_mem::read_byte_pa(pa) {
        return Some(v);
    }
    if let Some(host) = flash_host_addr(pa) {
        // SAFETY: flash backing is `SIZE` bytes; `flash_host_addr`
        // bounds-checks against `FLASH_BANK_SIZE` per bank.
        return Some(unsafe { core::ptr::read_volatile(host as *const u8) });
    }
    None
}

/// Try to write a byte. Returns true if a backing store accepted.
/// ROM is not writable via this path (guest_mem::write_byte_pa refuses
/// ROM), which matches guest semantics — the kernel uses the flash
/// state machine via the stage-2-mapped flash bank, not direct ROM writes.
fn backed_byte_write(pa: u32, val: u8) -> bool {
    if crate::guest_mem::write_byte_pa(pa, val) {
        return true;
    }
    if let Some(host) = flash_host_addr(pa) {
        // SAFETY: see `backed_byte_read`.
        unsafe { core::ptr::write_volatile(host as *mut u8, val); }
        return true;
    }
    false
}

fn backed_halfword_read(pa: u32) -> Option<u16> {
    if let Some(v) = crate::guest_mem::read_halfword_pa(pa) {
        return Some(v);
    }
    if let Some(host) = flash_host_addr(pa) {
        // SAFETY: bounds established by flash_host_addr.
        return Some(unsafe { core::ptr::read_volatile(host as *const u16) });
    }
    None
}

fn backed_halfword_write(pa: u32, val: u16) -> bool {
    if crate::guest_mem::write_halfword_pa(pa, val) {
        return true;
    }
    if let Some(host) = flash_host_addr(pa) {
        // SAFETY: see above.
        unsafe { core::ptr::write_volatile(host as *mut u16, val); }
        return true;
    }
    false
}

/// Dispatch a byte load: try backed memory (with XOR) first; fall back
/// to MMIO for IPAs outside our backing stores, or unconditionally for
/// `ea >= XOR_LIMIT`.
fn dispatch_byte_read(ea: u32, faulting_pc: u32) -> u8 {
    if ea < XOR_LIMIT {
        let addr = ea ^ 3;
        if let Some(pa) = resolve_addr(addr) {
            if let Some(v) = backed_byte_read(pa) {
                return v;
            }
        }
    } else {
        // MMIO-range read: no XOR, try backed stores first (flash bank 1
        // lives at IPA 0x10000000, which is >= XOR_LIMIT but is real
        // memory), then fall through to mmio dispatch.
        if let Some(pa) = resolve_addr(ea) {
            if let Some(v) = backed_byte_read(pa) {
                return v;
            }
        }
    }
    let pa = match resolve_addr(ea) {
        Some(p) => p,
        None => {
            kprintln!(
                "*** shadow_stub: byte read walk-fail ea={:#x} pc={:#x}",
                ea, faulting_pc
            );
            crate::cpu::halt();
        }
    };
    crate::mmio::read(pa as u64, 0, faulting_pc as u64) as u8
}

fn dispatch_byte_write(ea: u32, val: u8, faulting_pc: u32) {
    if ea < XOR_LIMIT {
        let addr = ea ^ 3;
        if let Some(pa) = resolve_addr(addr) {
            if backed_byte_write(pa, val) {
                return;
            }
        }
    } else {
        if let Some(pa) = resolve_addr(ea) {
            if backed_byte_write(pa, val) {
                return;
            }
        }
    }
    let pa = match resolve_addr(ea) {
        Some(p) => p,
        None => {
            kprintln!(
                "*** shadow_stub: byte write walk-fail ea={:#x} pc={:#x}",
                ea, faulting_pc
            );
            crate::cpu::halt();
        }
    };
    crate::mmio::write(pa as u64, 0, val as u32, faulting_pc as u64);
}

fn dispatch_halfword_read(ea: u32, faulting_pc: u32) -> u16 {
    if ea < XOR_LIMIT {
        let addr = ea ^ 2;
        if let Some(pa) = resolve_addr(addr) {
            if let Some(v) = backed_halfword_read(pa) {
                return v;
            }
        }
    } else {
        if let Some(pa) = resolve_addr(ea) {
            if let Some(v) = backed_halfword_read(pa) {
                return v;
            }
        }
    }
    let pa = match resolve_addr(ea) {
        Some(p) => p,
        None => {
            kprintln!(
                "*** shadow_stub: halfword read walk-fail ea={:#x} pc={:#x}",
                ea, faulting_pc
            );
            crate::cpu::halt();
        }
    };
    crate::mmio::read(pa as u64, 1, faulting_pc as u64) as u16
}

fn dispatch_halfword_write(ea: u32, val: u16, faulting_pc: u32) {
    if ea < XOR_LIMIT {
        let addr = ea ^ 2;
        if let Some(pa) = resolve_addr(addr) {
            if backed_halfword_write(pa, val) {
                return;
            }
        }
    } else {
        if let Some(pa) = resolve_addr(ea) {
            if backed_halfword_write(pa, val) {
                return;
            }
        }
    }
    let pa = match resolve_addr(ea) {
        Some(p) => p,
        None => {
            kprintln!(
                "*** shadow_stub: halfword write walk-fail ea={:#x} pc={:#x}",
                ea, faulting_pc
            );
            crate::cpu::halt();
        }
    };
    crate::mmio::write(pa as u64, 1, val as u32, faulting_pc as u64);
}

/// UDF handler entry point. Called from `handle_und` in `trap.rs` when
/// the faulting instruction matches the SBA UDF encoding shape.
/// Returns `true` if handled (ELR_EL2 / SPSR_EL2 set for ERET); `false`
/// on an unrecognisable slot so the caller can fall through.
pub fn handle_sba_udf(
    ctx: &mut TrapContext,
    faulting_pc: u32,
    spsr_und: u64,
    insn: u32,
) -> bool {
    if !is_sba_udf_insn(insn) {
        return false;
    }
    let imm16 = udf_imm16(insn);
    let idx = (imm16 - SBA_UDF_BASE) as usize;

    // SAFETY: idx bounded by the SBA_UDF_MAX check in is_sba_udf_insn.
    let (orig_insn, orig_pc) = unsafe {
        (SBA_ORIG_INSN[idx], SBA_ORIG_PC[idx])
    };

    if orig_insn == 0 {
        kprintln!(
            "*** shadow_stub: SBA UDF at {:#x} hits empty slot {}",
            faulting_pc, idx
        );
        return false;
    }
    if orig_pc != faulting_pc {
        kprintln!(
            "*** shadow_stub: SBA UDF at {:#x} slot {} has orig_pc {:#x} (mismatch)",
            faulting_pc, idx, orig_pc
        );
        return false;
    }

    let d = match decode(orig_insn) {
        Some(d) => d,
        None => {
            kprintln!(
                "*** shadow_stub: stored orig_insn {:#010x} at slot {} no longer decodes",
                orig_insn, idx
            );
            return false;
        }
    };

    let cpsr = spsr_und as u32;

    // Condition-code check.
    if !cond_passes(d.cond, cpsr) {
        crate::trap::return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return true;
    }

    let mut regs = Regs::snapshot(ctx);

    // Compute offset amount.
    let offset = match d.offset {
        OffsetForm::None => 0u32,
        OffsetForm::Imm { imm } => imm,
        OffsetForm::Reg { rm, shift_type, shift_amount } => {
            let rm_val = regs.get(rm);
            apply_shift(rm_val, shift_type, shift_amount, cpsr)
        }
    };

    let rn_val = regs.get(d.rn);
    let ea_offsetted = if d.u {
        rn_val.wrapping_add(offset)
    } else {
        rn_val.wrapping_sub(offset)
    };
    // Pre-index / plain (P=1): access uses rn +- offset.
    // Post-index (P=0): access uses rn, writeback stores rn +- offset.
    let access_addr = if d.p { ea_offsetted } else { rn_val };

    // Perform the access.
    match d.kind {
        AccessKind::Ldrb => {
            let v = dispatch_byte_read(access_addr, faulting_pc);
            regs.set(d.rt, v as u32);
        }
        AccessKind::Strb => {
            let v = regs.get(d.rt) as u8;
            dispatch_byte_write(access_addr, v, faulting_pc);
        }
        AccessKind::Ldrsb => {
            let v = dispatch_byte_read(access_addr, faulting_pc) as i8 as i32 as u32;
            regs.set(d.rt, v);
        }
        AccessKind::Ldrh => {
            let v = dispatch_halfword_read(access_addr, faulting_pc);
            regs.set(d.rt, v as u32);
        }
        AccessKind::Strh => {
            let v = regs.get(d.rt) as u16;
            dispatch_halfword_write(access_addr, v, faulting_pc);
        }
        AccessKind::Ldrsh => {
            let v = dispatch_halfword_read(access_addr, faulting_pc) as i16 as i32 as u32;
            regs.set(d.rt, v);
        }
        AccessKind::Swpb => {
            let old = dispatch_byte_read(access_addr, faulting_pc);
            let new = regs.get(d.rt2) as u8;
            dispatch_byte_write(access_addr, new, faulting_pc);
            regs.set(d.rt, old as u32);
        }
    }

    // Writeback Rn for pre-W=1 or post-index (P=0).
    let writeback = (d.p && d.w) || !d.p;
    if writeback {
        regs.set(d.rn, ea_offsetted);
    }

    // Commit R0..R12 into ctx.x[] — the AArch64 ERET tail writes
    // those back into the guest as AArch32 R0..R12 regardless of
    // target mode. R13 / R14 of the target mode are NOT touched by
    // ERET: instead we route through the post-emulation trampoline
    // (see `dispatch_return`) whenever R13 or R14 changed so the
    // updated value lands in the faulting mode's banked slot.
    for i in 0..13 {
        ctx.x[i] = regs.r[i] as u64;
    }

    dispatch_return(ctx, faulting_pc, spsr_und, regs.r[13], regs.r[14]);
    true
}

/// Return from the SBA handler to the guest. If the faulting-mode
/// banked SP / LR need to be updated (writeback to R13 / R14), route
/// through the post-emulation trampoline; otherwise a direct ERET
/// back to `faulting_pc + 4` is enough.
fn dispatch_return(
    ctx: &mut TrapContext,
    faulting_pc: u32,
    spsr_und: u64,
    new_sp: u32,
    new_lr: u32,
) {
    // If new_sp / new_lr match what we started with (no writeback to
    // R13 / R14), the guest's banked SP / LR are already correct and
    // we can ERET straight back.
    let orig_sp = read_banked_sp_slot();
    let orig_lr = read_banked_lr_slot();
    if new_sp == orig_sp && new_lr == orig_lr {
        crate::trap::return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Writeback: stash new values into the banked slots, update the
    // post-emulation trampoline's NEW_PC literal, flush the dcache
    // line, and ERET into the trampoline in the faulting mode. The
    // trampoline writes SP / LR natively (they're banked to that mode),
    // restores R12 from TPIDRURW, and branches to NEW_PC.
    crate::guest_mem::write_word_pa(crate::trap::UND_SAVE_BANKED_SP_IPA, new_sp);
    crate::guest_mem::write_word_pa(crate::trap::UND_SAVE_BANKED_LR_IPA, new_lr);

    // Write NEW_PC literal into the ROM-backed trampoline.
    let literal_host =
        crate::guest_mem::rom_host_pa() as usize + crate::guest_mem::SBA_POST_TRAMP_NEW_PC_OFFSET;
    // SAFETY: bounded write inside ROM backing; we own the backing from EL2.
    unsafe {
        core::ptr::write_volatile(literal_host as *mut u32, faulting_pc.wrapping_add(4));
        core::arch::asm!(
            "dc cvau, {0}",
            "dsb ish",
            in(reg) literal_host as u64,
            options(nostack, preserves_flags),
        );
    }

    crate::trap::return_to_guest_from_und(
        ctx,
        crate::guest_mem::SBA_POST_TRAMP_OFFSET as u64,
        spsr_und,
    );
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
        let r2 = validate_one(100, 0, 16);
        assert_eq!(r2, ValidatePcResult::OutOfRange);
    }

    #[test]
    fn enc_udf_shape() {
        // imm16 = 0xFFFE (guest_bp marker) should match the known encoding.
        assert_eq!(enc_udf(0xFFFE), 0xE7FF_F0FE);
        // imm16 = 0x8000 (first SBA slot).
        let w = enc_udf(0x8000);
        assert!(is_sba_udf_insn(w));
        assert_eq!(udf_imm16(w), 0x8000);
    }
}
