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

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

// ---- Inline-stub pool ---------------------------------------------------
//
// Each inline-eligible site gets a 7-word (28-byte) stub inside the ROM
// aperture. The original faulting word is rewritten to `B stub_slot`; the
// stub uses two LIVENESS-PROVED dead registers as scratches (no save/
// restore — they were going to be overwritten anyway), computes the EA
// with the BE-32 XOR correction, does the load/store natively (so a
// cross-page EA takes a real DABT and the kernel's own demand-pager
// handles it), then branches back to `orig_pc + 4`. See
// `encode_inline_stub` for the precise layout.
//
// Sites where liveness analysis can't find 2 dead candidate registers
// halt loudly at install time — the ROM is fixed, so we discover any
// such sites once and address them individually (extend the analyzer
// or special-case the site).
//
// Sits between the tracer pool (0x0090_0000..0x00E0_0000) and the ROM-tail
// trampoline cluster (0x00FF_FF00..0x00FF_FFF0). Tracer's
// `in_reserved_range` excludes this window too.
//
// Slots are 12 words to accommodate two stub variants in a single
// fixed-size pool:
//   - "dead-reg" stub (preferred, when liveness finds 2 dead scratch
//     candidates or 1 dead candidate + dead NZCV): slots 0, 1, 9, 10
//     are NOPs; slots 2–8, 11 carry the body.
//   - "stack" stub (fallback when no dead candidates exist): slots 0/1
//     PUSH scratch_ea / scratch_fl onto the mode-banked SP, slots 9/10
//     POP them back.
//
// The EA-compute step uses TWO ADDs (or SUBs) to handle 12-bit
// immediates that don't fit a single ARM modified-immediate (8-bit
// rotated). Newton ROM byte accesses with offsets > 0xFF (e.g.
// `ldrb r0, [r0, #0x156]` at 0x26fc0) need this split.
pub const SBA_STUB_POOL_IPA: u32 = 0x00E0_0000;
pub const SBA_STUB_POOL_END: u32 = 0x00FF_FF00;
pub const SBA_STUB_WORDS: usize = 16;
pub const SBA_STUB_BYTES: u32 = (SBA_STUB_WORDS as u32) * 4;
pub const SBA_STUB_MAX: usize =
    ((SBA_STUB_POOL_END - SBA_STUB_POOL_IPA) / SBA_STUB_BYTES) as usize;

static NEXT_STUB: AtomicUsize = AtomicUsize::new(0);

// ---- Scratch-VA pool (ScratchVA-variant inline stubs) ------------------
//
// ScratchVA-variant inline stubs save the caller's `scratch_ea` and
// `scratch_fl` registers into a per-stub 8-byte scratch slot at a
// kernel VA inside a 1 MiB carve-out. Each such stub gets its own
// dedicated slot (no contention between concurrent stubs in different
// IRQ contexts).
//
// We identity-map VA == IPA so:
//   * Newton boot (kernel stage-1 on): kernel L1[VA>>20] = section
//     descriptor identity-mapping VA→IPA. Stage-2 maps IPA →
//     SCRATCH_POOL.
//   * Guest-test mode (kernel stage-1 off, runs MMU-off per
//     `test_runtime.S`): stage-1 is bypassed; the CPU emits VA as
//     IPA directly. Stage-2 sees IPA == VA and maps to SCRATCH_POOL.
//
// Identity mapping keeps the per-stub literal usable from both regimes
// without two separate stage-2 mappings.
//
// IPAs 0x3000_0000..0x5000_0000 are PCMCIA peripheral aperture, so
// they're off-limits for a RAM-backed scratch carve-out.
//
// Empirical L1 census of 717006 boot (see qemu_l1_dump.log /
// INVESTIGATION.md): the 717006 kernel populates L1[0x000..0x2FF] for
// kernel-side mappings (contiguous identity sections + a few
// dynamically-allocated coarse tables, e.g. L1[0x1A] backed by an L2
// at ROM PA 0x00018000). One large observed-free gap at L1[0x52..0xBF]
// (110 slots — VA 0x0520_0000..0x0BFF_FFFF). VA = IPA = 0x0600_0000
// sits in the middle of that gap (L1[0x60]) and is also free in the
// existing stage-2 layout (between RAM at 0x0440_0000 and the
// framebuffer at 0x0E00_0000), so its stage-2 L2 slot (L2[0x30]) can
// be refined to a 64 KiB RW carve-out.
pub const SCRATCH_POOL_VA: u32 = 0x0600_0000;
pub const SCRATCH_POOL_IPA: u32 = 0x0600_0000;
pub const SCRATCH_POOL_SIZE: usize = 64 * 1024; // 16 × 4 KiB pages
pub const SCRATCH_BYTES_PER_STUB: usize = 8;
pub const SCRATCH_POOL_STUB_CAP: usize =
    SCRATCH_POOL_SIZE / SCRATCH_BYTES_PER_STUB;

#[repr(C, align(4096))]
pub struct ScratchPool(pub [u8; SCRATCH_POOL_SIZE]);
pub static mut SCRATCH_POOL: ScratchPool = ScratchPool([0; SCRATCH_POOL_SIZE]);

/// Host PA of the scratch pool — used by `stage2::install_scratch_pool`
/// when populating the L3 page table that backs the carve-out IPA.
pub fn scratch_pool_host_pa() -> u64 {
    addr_of_mut!(SCRATCH_POOL) as u64
}

/// Per-ScratchVA-variant slot allocator. Independent of `NEXT_STUB`
/// because DeadReg / Stack stubs don't claim a scratch slot.
static NEXT_SCRATCH_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Compute the kernel VA of the per-stub 8-byte scratch slot for a
/// given allocator index. The returned VA lies inside
/// `[SCRATCH_POOL_VA, SCRATCH_POOL_VA + SCRATCH_POOL_SIZE)` and is
/// 4-byte aligned.
pub fn scratch_slot_va(slot_idx: usize) -> u32 {
    SCRATCH_POOL_VA + (slot_idx as u32) * (SCRATCH_BYTES_PER_STUB as u32)
}

/// Per-site stash of the original instruction word. Indexed by the UDF
/// `imm16 - SBA_UDF_BASE`. 0 marks "slot unused" — 0 is not a valid
/// byte/halfword-access encoding in any form `decode()` accepts.
///
/// Only UDF-path sites allocate an entry here. Inline-stub sites
/// don't need a stash — the stub itself is self-contained, no walk-fail
/// retry path goes through them, and a UND inside an inline stub is a
/// bug (we halt rather than emulate).
static mut SBA_ORIG_INSN: [u32; SBA_MAX_SITES] = [0; SBA_MAX_SITES];

/// Per-site stash of the original guest PC. Cross-checked against
/// `faulting_pc` at trap time; a mismatch means the table was corrupted
/// or the UDF somehow fired at a PC that doesn't match its slot.
static mut SBA_ORIG_PC: [u32; SBA_MAX_SITES] = [u32::MAX; SBA_MAX_SITES];

static NEXT_SITE: AtomicUsize = AtomicUsize::new(0);

// ---- Pre-fault retry state --------------------------------------------
//
// When a UDF-fallback site hits a walk-fail during emulation, the handler
// ERETs into the pre-fault stub at `SBA_PREFAULT_STUB_VA` with the EA in
// R0. The stub does `LDRB r0, [r0]` — if the page is unmapped, the
// kernel's DataAbortHandler pages it in and retries. On success the stub
// HVCs back (`SBA_RETRY_TAG`) and `handle_sba_retry` restores the stashed
// ctx + re-runs the emulator body against the now-mapped EA.
//
// Single-in-flight: the stub HVCs back synchronously before another UDF
// can fire. If a second SBA UDF somehow landed while a retry was pending,
// the pending flag catches it.
static SBA_RETRY_PENDING: AtomicBool = AtomicBool::new(false);
static mut SBA_RETRY_CTX: [u64; 15] = [0; 15];
static mut SBA_RETRY_FAULTING_PC: u32 = 0;
static mut SBA_RETRY_SPSR_UND: u64 = 0;
static mut SBA_RETRY_IDX: usize = 0;

/// Summary statistics returned by `patch_code_range` / `patch_rom_from_bitmap`.
#[derive(Default, Debug)]
pub struct PatchStats {
    pub words_scanned: usize,
    pub patched: usize,
    pub inline_stubs: usize,
    pub udf_fallback: usize,
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

// =======================================================================
// Encoder helpers (inline-stub emission)
// =======================================================================

mod encode {
    pub const AL: u32 = 0xE;
    pub const LO: u32 = 0x3;

    /// Encode a 32-bit value as an ARMv7 modified-immediate (imm8 rotated
    /// right by 2*rot). Returns None if the value isn't representable in
    /// one instruction.
    pub fn arm_imm12(value: u32) -> Option<u32> {
        for rot in 0..16u32 {
            // value = imm8 ROR (2*rot) ⇔ imm8 = value ROL (2*rot).
            let imm8 = value.rotate_left(rot * 2);
            if imm8 < 256 {
                return Some((rot << 8) | imm8);
            }
        }
        None
    }

    /// MRS Rd, CPSR  — read CPSR into Rd.
    pub fn mrs_cpsr(rd: u32) -> u32 {
        (AL << 28) | 0x010F_0000 | (rd << 12)
    }

    /// NOP A1 encoding (`mov r0, r0` is the canonical form, but ARMv7
    /// has a dedicated NOP hint at `0xE320_F000`). Use the hint —
    /// processors recognise it as a true no-op without reading or
    /// writing R0.
    pub fn nop() -> u32 {
        0xE320_F000
    }

    /// STR Rt, [SP, #-4]!  — push one register (pre-index, U=0, W=1).
    pub fn push(rt: u32) -> u32 {
        (AL << 28) | 0x052D_0004 | (rt << 12)
    }

    /// LDR Rt, [SP], #4    — pop one register (post-index, U=1, W=0).
    pub fn pop(rt: u32) -> u32 {
        (AL << 28) | 0x049D_0004 | (rt << 12)
    }

    /// MSR CPSR_f, Rm — write only the flag field (NZCV) from Rm.
    pub fn msr_cpsr_f(rm: u32) -> u32 {
        (AL << 28) | 0x0128_F000 | rm
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

    /// CMP Rn, #imm12  (modified-immediate encoded).
    pub fn cmp_imm(rn: u32, imm12: u32) -> u32 {
        (AL << 28) | 0x0350_0000 | (rn << 16) | (imm12 & 0xFFF)
    }

    /// EOR[cond] Rd, Rn, #imm12  (modified-immediate encoded).
    pub fn eor_imm_cond(cond: u32, rd: u32, rn: u32, imm12: u32) -> u32 {
        (cond << 28) | 0x0220_0000 | (rn << 16) | (rd << 12) | (imm12 & 0xFFF)
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

    /// Encode the zero-offset access insn at slot 7 for the given
    /// AccessKind. Rn is the scratch-EA register; Rt is the data register
    /// from the original site; cond is the original site's cond.
    pub fn access_zero_offset(
        kind: super::AccessKind, cond: u32, rt: u32, rn: u32,
    ) -> u32 {
        match kind {
            super::AccessKind::Ldrb  => (cond << 28) | 0x05D0_0000 | (rn << 16) | (rt << 12),
            super::AccessKind::Strb  => (cond << 28) | 0x05C0_0000 | (rn << 16) | (rt << 12),
            super::AccessKind::Ldrh  => (cond << 28) | 0x01D0_00B0 | (rn << 16) | (rt << 12),
            super::AccessKind::Strh  => (cond << 28) | 0x01C0_00B0 | (rn << 16) | (rt << 12),
            super::AccessKind::Ldrsb => (cond << 28) | 0x01D0_00D0 | (rn << 16) | (rt << 12),
            super::AccessKind::Ldrsh => (cond << 28) | 0x01D0_00F0 | (rn << 16) | (rt << 12),
            super::AccessKind::Swpb  => unreachable!("SWPB is UDF-fallback, not inline"),
        }
    }

    /// LDR Rt, [PC, #+disp]  — PC-relative literal load (positive offset
    /// only). The hardware's pipeline-PC value is the encoding-time PC + 8;
    /// the caller is responsible for accounting for that. `disp` is in
    /// bytes, must be ≤ 0xFFF.
    pub fn ldr_pc_rel_pos(rt: u32, disp: u32) -> u32 {
        debug_assert!(disp <= 0xFFF);
        (AL << 28) | 0x059F_0000 | (rt << 12) | (disp & 0xFFF)
    }

    /// STR Rt, [Rn, #+imm12]  — pre-index, no writeback, positive offset.
    pub fn str_imm_pos(rt: u32, rn: u32, imm12: u32) -> u32 {
        debug_assert!(imm12 <= 0xFFF);
        (AL << 28) | 0x0580_0000 | (rn << 16) | (rt << 12) | (imm12 & 0xFFF)
    }

    /// LDR Rt, [Rn, #+imm12]  — pre-index, no writeback, positive offset.
    pub fn ldr_imm_pos(rt: u32, rn: u32, imm12: u32) -> u32 {
        debug_assert!(imm12 <= 0xFFF);
        (AL << 28) | 0x0590_0000 | (rn << 16) | (rt << 12) | (imm12 & 0xFFF)
    }

    /// MCR p15, 0, Rt, c13, c0, 2  — write Rt to TPIDRURW.
    pub fn mcr_p15_0_c13_c0_2(rt: u32) -> u32 {
        (AL << 28) | 0x0E0D_0F50 | (rt << 12)
    }

    /// MRC p15, 0, Rt, c13, c0, 2  — read TPIDRURW into Rt.
    pub fn mrc_p15_0_c13_c0_2(rt: u32) -> u32 {
        (AL << 28) | 0x0E1D_0F50 | (rt << 12)
    }
}

// =======================================================================
// Liveness analysis
// =======================================================================
//
// To avoid saving and restoring scratch register values across the inline
// stub, we walk forward from the byte-access site and identify which of
// {R0..R3, R12, R14} are GENUINELY DEAD — i.e. the next reference to
// each is a write, not a read. Two such registers can be used as
// scratch_ea / scratch_flags without preserving their pre-stub values.
//
// The analyzer is deliberately conservative: anything we can't decode,
// any branch instruction, and any "max instructions" bound all mark the
// remaining unwritten registers as live. False positives (claiming
// "live" when actually dead) cost us inline coverage; false negatives
// (claiming "dead" when actually live) are correctness bugs and must
// not happen.

type RegMask = u16;

const REG_PC: u32 = 15;

/// Branch kind returned by `analyze_insn`. Classifies how the
/// instruction transfers (or doesn't transfer) control flow, so the
/// CFG-aware liveness walker can follow branch targets explicitly.
#[derive(Debug, Clone, Copy)]
enum BranchKind {
    /// Linear instruction. No control transfer; analyzer continues at
    /// PC+4. The reported (read, write) masks are exact.
    Linear,
    /// BL or BL-like — eventually returns. APCS-clobbers
    /// {R0..R3, R12, R14}; analyzer continues at PC+4 with those regs
    /// effectively "written" (i.e. dead from the caller's perspective).
    /// `target` is only consumed by `#[cfg(test)]` assertions; the
    /// runtime walker doesn't follow BL targets (continues at PC+4
    /// instead).
    BLink {
        #[cfg_attr(not(test), allow(dead_code))]
        target: u32,
    },
    /// Unconditional branch. Analyzer follows `target` and stops.
    Direct { target: u32 },
    /// Conditional branch (Bcc, no link). Analyzer must consider both
    /// paths: branch-taken to `target`, and fall-through to PC+4.
    Cond { target: u32 },
    /// APCS function return (BX LR, POP {…, PC}, MOV PC, LR, etc.):
    /// control leaves to the caller. The reported `read` mask names
    /// the caller-observable registers (R0 return value, R4–R11
    /// callee-preserved, SP, LR); no other unwritten regs are live.
    Return,
    /// Conditional APCS function return (`MOVNE pc, lr`,
    /// `LDMDBNE fp, {…, pc}`, etc.). When the condition is true, the
    /// instruction returns; when false, control falls through to PC+4.
    /// The walker must merge live-sets from both paths or it will miss
    /// reads that occur only on the fall-through (a real Newton ROM
    /// motif: `MakeObject` at 0x2595c8 has a conditional ldmdbne
    /// followed by `STR r1, …`/`STR r3, …` that the walker would
    /// otherwise never see). Without this, the inline-stub picker
    /// concludes those registers are dead and clobbers them.
    CondReturn,
    /// Indirect branch with unknown target (BX register where we can't
    /// tell it's a return, function-pointer call, jump table, etc.).
    /// CFG stops; remaining unwritten regs conservatively live.
    Indirect,
    /// Unknown / unhandled instruction — give up conservatively.
    Unknown,
}

/// Compute the absolute target of a B/BL/Bcc/BLcc instruction.
fn branch_target(insn: u32, pc: u32) -> u32 {
    let imm24 = insn & 0x00FF_FFFF;
    // Sign-extend imm24 to 32 bits, shift left 2.
    let signed = ((imm24 as i32) << 8) >> 6; // ((<<8)>>8) sign-extends to 32; <<2 for word
    pc.wrapping_add(8).wrapping_add(signed as u32)
}

/// NZCV interaction of a single instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NzcvEffect {
    /// Doesn't read or write the flags.
    None,
    /// Reads NZCV (any conditional instruction except AL/NV, plus Bcc).
    Read,
    /// Unconditionally writes NZCV (CMP/TST/TEQ/CMN, S=1 DP, MSR cpsr_f).
    Write,
}

/// Per-insn NZCV effect derived from the encoding. The walker uses
/// this to decide whether the inline stub's CMP-clobber is observable
/// (i.e. whether NZCV is live at orig_pc+4).
fn analyze_nzcv(insn: u32) -> NzcvEffect {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF { return NzcvEffect::None; }
    let cond_uses_flags = !matches!(cond, 0xE | 0xF);

    // Bcc: cond 101 L imm24
    if (insn & 0x0E00_0000) == 0x0A00_0000 {
        return if cond_uses_flags { NzcvEffect::Read } else { NzcvEffect::None };
    }

    // DP-immediate / DP-reg / DP-reg-shifted: S bit (bit 20) = write NZCV.
    // CMP/TST/TEQ/CMN always write (their opcodes 0b1000..1011 imply S).
    let is_dp_imm = (insn & 0x0E00_0000) == 0x0200_0000;
    let is_dp_reg_imm = (insn & 0x0E00_0010) == 0x0000_0000;
    let is_dp_reg_shf = (insn & 0x0E00_0090) == 0x0000_0010;
    if is_dp_imm || is_dp_reg_imm || is_dp_reg_shf {
        let opc = (insn >> 21) & 0xF;
        let s_bit = (insn >> 20) & 1;
        let cmp_class = matches!(opc, 0b1000 | 0b1001 | 0b1010 | 0b1011);
        // Conditional CMP / S-set: writes NZCV only when condition passes.
        // If cond ≠ AL the write is conditional; we treat that as
        // "may write, may not" → conservatively, the cond evaluation
        // also reads NZCV. Net: Read.
        if cmp_class || s_bit == 1 {
            if cond_uses_flags { return NzcvEffect::Read; }
            return NzcvEffect::Write;
        }
        // Plain DP without S: doesn't touch NZCV. But cond ≠ AL still reads.
        if cond_uses_flags { return NzcvEffect::Read; }
        return NzcvEffect::None;
    }

    // MSR (immediate) cpsr_f / cpsr_fxsc with mask covering F: writes NZCV.
    // cond 0011 0R10 mask SBO imm12 — mask bit 19 = f.
    if (insn & 0x0FB0_F000) == 0x0320_F000 {
        let mask = (insn >> 16) & 0xF;
        if (mask & 0x8) != 0 { return NzcvEffect::Write; }
        return NzcvEffect::None;
    }
    // MSR (register) — same mask layout.
    if (insn & 0x0FB0_FFF0) == 0x0120_F000 {
        let mask = (insn >> 16) & 0xF;
        if (mask & 0x8) != 0 { return NzcvEffect::Write; }
        return NzcvEffect::None;
    }

    // For everything else: any conditional cond reads flags.
    if cond_uses_flags { NzcvEffect::Read } else { NzcvEffect::None }
}

fn nzcv_dead_recursive<R>(
    start_pc: u32, max_instrs: u32, visited: &mut Visited, read_insn: &R,
) -> bool
where R: Fn(u32) -> Option<u32> {
    if visited.contains(start_pc) {
        // Cycle: contributes no new reads / writes. Caller's verdict.
        return true;
    }
    if !visited.push(start_pc) {
        return false; // budget exhausted, conservative
    }
    let mut pc = start_pc;
    for _ in 0..max_instrs {
        let insn = match read_insn(pc) {
            Some(w) => w,
            None => return false,
        };
        match analyze_nzcv(insn) {
            NzcvEffect::Read => return false,
            NzcvEffect::Write => return true,
            NzcvEffect::None => {}
        }
        let (_, _, kind) = analyze_insn(insn, pc);
        match kind {
            BranchKind::Linear | BranchKind::BLink { .. } => {
                pc = pc.wrapping_add(4);
            }
            BranchKind::Direct { target } => {
                if target == pc { return true; }
                return nzcv_dead_recursive(target, max_instrs, visited, read_insn);
            }
            BranchKind::Cond { target } => {
                // Cond branch reads NZCV — but `analyze_nzcv(Bcc)` already
                // reports Read above. We won't reach here if the Bcc had
                // a flag-reading cond (caught earlier in this iteration).
                // For an AL-cond branch (just B), follow the target.
                if target == pc { return true; }
                let taken = nzcv_dead_recursive(target, max_instrs, visited, read_insn);
                let fall = nzcv_dead_recursive(pc.wrapping_add(4), max_instrs, visited, read_insn);
                return taken && fall;
            }
            BranchKind::Return => {
                // Function return: NZCV at the call site isn't part of
                // APCS preserved state, so we can treat it as dead.
                return true;
            }
            BranchKind::CondReturn => {
                // Conditional return: taken-path is dead (return),
                // fall-through must be checked.
                let fall = nzcv_dead_recursive(pc.wrapping_add(4), max_instrs, visited, read_insn);
                return fall;
            }
            BranchKind::Indirect | BranchKind::Unknown => return false,
        }
    }
    false
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
        // Unconditional class (NEON, PLD, etc.) — give up.
        return (0, 0, BranchKind::Unknown);
    }
    let cond_al = cond == 0xE;

    // Branch (B / BL): cond 101 L imm24
    if (insn & 0x0E00_0000) == 0x0A00_0000 {
        let l = (insn >> 24) & 1;
        let target = branch_target(insn, pc);
        let kind = if l == 1 {
            // BL: writes LR; eventually returns. Treat APCS caller-
            // saved {R0..R3, R12, LR} as written-by-call from the
            // caller's view (the callee may clobber them) and continue
            // analysis at PC+4.
            BranchKind::BLink { target }
        } else if cond_al {
            BranchKind::Direct { target }
        } else {
            BranchKind::Cond { target }
        };
        // For BL the returned read/write masks reflect only the BL
        // itself; the BLink walker logic OR's in {R0..R3, R12, R14}.
        return (0, 0, kind);
    }
    // BX / BLX register: cond 0001 0010 SBO 00LM Rm
    if (insn & 0x0FFF_FFD0) == 0x012F_FF10 {
        let rm = insn & 0xF;
        // BX LR is the standard APCS function return.
        if rm == 14 {
            let kind = if cond_al { BranchKind::Return } else { BranchKind::CondReturn };
            return (APCS_RETURN_LIVE, 0, kind);
        }
        // BLX register: writes LR, like a function call.
        let is_blx = (insn & 0x20) != 0;
        if is_blx {
            return (1u16 << rm, 0, BranchKind::BLink { target: pc.wrapping_add(4) });
        }
        // BX to a non-LR register: a tail-call / jump-table / function
        // pointer dispatch. We can't infer the target's reads — drop
        // back to conservative behaviour.
        return (1u16 << rm, 0, BranchKind::Indirect);
    }
    // SVC / SWI: cond 1111 imm24. Functionally a call — handler runs
    // and returns to PC+4. APCS caller-saved are observably clobbered;
    // analyzer continues at PC+4.
    if (insn & 0x0F00_0000) == 0x0F00_0000 {
        return (0, 0, BranchKind::BLink { target: pc.wrapping_add(4) });
    }
    // BKPT (BRK in ARM)
    if (insn & 0x0FF0_00F0) == 0x0120_0070 {
        return (0, 0, BranchKind::Unknown);
    }

    // HVC: cond 0001 0100 imm12 0111 imm4. Treat like a function call:
    // APCS caller-saved are observably clobbered (the EL2 handler can
    // do anything with them; for shadow-stub HVCs in particular, the
    // handler may modify R0..R3 with a return value). Continue analysis
    // at PC+4 since HVC returns there. Caught here BEFORE the DP-reg-
    // shifted matcher, which it would otherwise match.
    if (insn & 0x0FF0_00F0) == 0x0140_0070 {
        return (0, 0, BranchKind::BLink { target: pc.wrapping_add(4) });
    }
    // SMC: cond 0001 0110 imm12 0111 imm4 — same shape as HVC, treat
    // identically.
    if (insn & 0x0FF0_00F0) == 0x0160_0070 {
        return (0, 0, BranchKind::BLink { target: pc.wrapping_add(4) });
    }

    // MOVW (A2): cond 0011 0000 imm4 Rd imm12 — writes Rd, no GPR reads.
    // bits 27:20 = 0011_0000. Distinct from DP-imm AND (bit 24 differs).
    if (insn & 0x0FF0_0000) == 0x0300_0000 {
        let rd = (insn >> 12) & 0xF;
        let write = if cond_al { 1u16 << rd } else { 0 };
        if rd == REG_PC {
            return (0, 0, BranchKind::Indirect);
        }
        return (0, write, BranchKind::Linear);
    }
    // MOVT (A1): cond 0011 0100 imm4 Rd imm12 — reads Rd (top half preserved low), writes Rd.
    // Equivalent: read the existing low 16 bits of Rd, OR-in imm16 into top.
    if (insn & 0x0FF0_0000) == 0x0340_0000 {
        let rd = (insn >> 12) & 0xF;
        let read = 1u16 << rd; // movt preserves Rd's low 16 bits
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
            // MOV PC, LR (opc=MOV=0b1101, no_rn_read, Rm=14, no shift)
            // is the canonical APCS return. Detect it explicitly so
            // we don't fall back to all-live.
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
            // `LDR PC, [SP], #4` (or any LDR PC sourced from SP) is
            // the single-register pop-return form.
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

    // Extra load/store (LDRH/STRH/LDRSB/LDRSH/LDRD/STRD):
    //   cond 000 P U I W L Rn Rt imm4h 1 op 1 imm4l
    // Bits 7=1, 4=1, op ∈ {01,10,11}.
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

    // LDM / STM: cond 100 P U S W L Rn register_list
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
            // LDM-with-PC is an APCS return. Common forms:
            //   POP {…, PC}              — Rn=SP=13.
            //   LDMDB fp, {…, PC}        — Rn=FP=11 (frame-pointer
            //                              variant emitted by ARM
            //                              compilers; the Newton ROM
            //                              uses this widely).
            //   LDMIA Rn, {…, PC}        — any base; tail-call via
            //                              switch table. Treat as
            //                              Indirect so we don't
            //                              over-claim deadness.
            // For Rn ∈ {SP, FP}, the loaded reglist *is* the caller's
            // saved-reg set; the caller-observable live regs are
            // exactly APCS_RETURN_LIVE. For other Rn, fall back to
            // Indirect (could be a switch jump or a load-from-data
            // tail-call).
            if rn == 13 || rn == 11 {
                let kind = if cond_al { BranchKind::Return } else { BranchKind::CondReturn };
                return (APCS_RETURN_LIVE | read, 0, kind);
            }
            return (read, 0, BranchKind::Indirect);
        }
        return (read, write, BranchKind::Linear);
    }

    // MUL / MLA: cond 0000 00AS Rd Ra Rs 1001 Rm
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
    // UMULL / SMULL / UMLAL / SMLAL: cond 0000 1UAS RdHi RdLo Rs 1001 Rm
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

    // MRS Rd, CPSR/SPSR: cond 0001 0R00 SBZ Rd SBZ
    if (insn & 0x0FBF_0FFF) == 0x010F_0000 {
        let rd = (insn >> 12) & 0xF;
        let write = if cond_al { 1u16 << rd } else { 0 };
        return (0, write, BranchKind::Linear);
    }
    // MSR (immediate): cond 0011 0R10 mask SBO imm12
    if (insn & 0x0FB0_F000) == 0x0320_F000 {
        return (0, 0, BranchKind::Linear);
    }
    // MSR (register): cond 0001 0R10 mask SBO 0000 0000 Rn
    if (insn & 0x0FB0_FFF0) == 0x0120_F000 {
        let rn = insn & 0xF;
        return (1u16 << rn, 0, BranchKind::Linear);
    }

    // MCR / MRC: cond 1110 opc1 L CRn Rt cp opc2 1 CRm
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

    // Unknown.
    (0, 0, BranchKind::Unknown)
}

/// CFG-aware live-out registers starting at `start_pc`.
///
/// Returns the set of registers that may be READ before being WRITTEN
/// on any path of execution from `start_pc`. Walks linearly, following
/// branches with bounded depth and cycle detection. Indirect branches
/// and unrecognised instructions stop the analysis with all-unwritten-
/// regs marked live.
///
/// APCS abstraction for BL: a function call writes {R0..R3, R12, R14}
/// from the caller's perspective — those registers are caller-saved,
/// so any value they held before the BL is observably gone after the
/// BL even if the callee preserves them. We treat BL as a linear
/// instruction that writes those regs and otherwise continues.
///
/// Cycle detection: tracks up to `MAX_VISITED` block-entry PCs. If a
/// branch target lands on an already-visited start, we return 0
/// (the cycle introduces no NEW reads beyond what the linear walk
/// already counted at first visit). This soundly handles tight halt
/// loops (`b .`), retry loops, and any back-edge within the budget.
const APCS_CALLER_SAVED: RegMask =
    (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 12) | (1 << 14);

/// Registers the caller observably depends on at function return: R0
/// (return value), R4–R11 (callee-preserved), R13 (SP), R14 (LR).
/// R1–R3 and R12 are caller-saved; the caller doesn't expect any
/// particular value in them at return, so they're "dead" at BX LR.
const APCS_RETURN_LIVE: RegMask =
    (1 << 0)
    | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7)
    | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11)
    | (1 << 13) | (1 << 14);

const LIVE_AT_MAX_VISITED: usize = 64;

struct Visited {
    pcs: [u32; LIVE_AT_MAX_VISITED],
    n: usize,
}

impl Visited {
    fn new() -> Self {
        Self { pcs: [0; LIVE_AT_MAX_VISITED], n: 0 }
    }
    fn contains(&self, pc: u32) -> bool {
        self.pcs[..self.n].iter().any(|&p| p == pc)
    }
    fn push(&mut self, pc: u32) -> bool {
        if self.n >= LIVE_AT_MAX_VISITED { return false; }
        self.pcs[self.n] = pc;
        self.n += 1;
        true
    }
}

/// Liveness analysis with an injectable instruction reader. Production
/// callers use `code_read_word`; tests pass an in-memory closure.
fn live_at_with_reader<R>(start_pc: u32, max_instrs: u32, read_insn: &R) -> RegMask
where R: Fn(u32) -> Option<u32> {
    let mut visited = Visited::new();
    live_at_recursive(start_pc, max_instrs, &mut visited, read_insn)
}

fn live_at_recursive<R>(
    start_pc: u32, max_instrs: u32, visited: &mut Visited, read_insn: &R,
) -> RegMask
where R: Fn(u32) -> Option<u32> {
    // Cycle detection: revisiting a block we've already analyzed adds
    // no new reads — the original visit already captured them.
    if visited.contains(start_pc) {
        return 0;
    }
    if !visited.push(start_pc) {
        // Visited budget exhausted — ABI-trustful conservative.
        return APCS_RETURN_LIVE;
    }

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
            BranchKind::BLink { .. } => {
                // BL site: the callee reads R0..R3 as parameter
                // registers. We don't know the callee's signature, so
                // conservatively treat all four as live at the call.
                // Without this, a register that's only read by the
                // BL itself (e.g. R1 set up earlier with `mov r1, r3`
                // and consumed by `bl callee`) appears dead in the
                // straight-line walk between its definition and the
                // call — and the stub-scratch picker would happily
                // overwrite it with CPSR. Newton ROM @ 0x13ca08 is
                // the canonical case that motivated this fix.
                const APCS_PARAMS: RegMask = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);
                live |= APCS_PARAMS & !written;
                let bl_clobber = APCS_CALLER_SAVED & !live;
                written |= bl_clobber;
                pc = pc.wrapping_add(4);
                continue;
            }
            BranchKind::Direct { target } => {
                if target == pc {
                    return live;
                }
                // If the target is unreadable (outside our backing
                // store — typically a Newton ROM jump-table entry
                // pointing into the post-load relocated function
                // pool at IPA > ROM_SIZE), treat as a tail-call
                // return: live = APCS_RETURN_LIVE.
                if read_insn(target).is_none() {
                    live |= APCS_RETURN_LIVE & !written;
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
                    APCS_RETURN_LIVE
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
                // Conditional return: merge the return path's live-set
                // (APCS_RETURN_LIVE — already counted via the analyzer's
                // `read` mask) with the fall-through. Without the
                // fall-through walk, reads that occur only after the
                // conditional return are missed entirely.
                let fall = live_at_recursive(pc.wrapping_add(4), max_instrs, visited, read_insn);
                live |= fall & !written;
                return live;
            }
            BranchKind::Indirect | BranchKind::Unknown => {
                // ABI-trustful conservative: switch-table dispatches,
                // function-pointer calls, and unknown instructions
                // ultimately route to APCS-shaped code (Newton ROM is
                // APCS-conformant). Mark APCS_RETURN_LIVE — i.e. R0 +
                // R4..R11 + SP + LR — as live, but trust that R12 +
                // R1..R3 (caller-saved scratch) are dead per ABI. The
                // halt-loud install path catches any divergence at
                // emulation time.
                live |= APCS_RETURN_LIVE & !written;
                return live;
            }
        }
    }
    live |= APCS_RETURN_LIVE & !written;
    live
}

/// Pick (scratch_ea, scratch_flags) — both DEAD at orig_pc+4 — from
/// {R12, R0..R3, R14} \ {Rt, Rn, Rm}. If only ONE dead GPR is found
/// AND NZCV is also dead at orig_pc+4 (the next instruction
/// overwrites flags), return Some((sea, None)) to signal a 1-scratch
/// stub layout (no NZCV save/restore — the CMP's flag clobber is
/// observably harmless).
///
/// Returns None when neither shape is achievable; caller halts loudly.
fn pick_scratch_regs(d: &Decoded, orig_pc: u32) -> Option<(u32, Option<u32>)> {
    pick_scratch_regs_with_reader(d, orig_pc, &code_read_word)
}

fn pick_scratch_regs_with_reader<R>(
    d: &Decoded, orig_pc: u32, read_insn: &R,
) -> Option<(u32, Option<u32>)>
where R: Fn(u32) -> Option<u32> {
    // R14 (LR) was previously in this list, but iter-41 caught a
    // case where the liveness analyzer failed to detect LR as live
    // across a tail-call (`b <fn>` where the target is in the
    // post-ship-patch table at VA > ROM_SIZE — the analyzer's
    // unreadable-target fallback should OR APCS_RETURN_LIVE in but
    // for some sites does not). The result was: shadow_stub picked
    // R14 as scratch_fl, the stub's `MRS R14, CPSR` clobbered LR
    // with the captured CPSR value (= 0x80000110 in the wedge case),
    // and the wild LR propagated as Tmux's caller PC into Lookup,
    // becoming the OUT-param pointer to TObjRef::Set, which faulted
    // on the wild this-pointer. R14 is the link register; using it
    // as a scratch in an in-line stub is fundamentally fragile (any
    // BX LR or tail-call between the stub and the function exit
    // jumps to the wild value). Restricting CANDIDATES to caller-
    // saved scratch GPRs is the safe choice.
    const CANDIDATES: &[u32] = &[12, 0, 1, 2, 3];
    // 32-instruction window: a typical Newton-ROM function body fits
    // within 32 from the byte-access site. Smaller windows hit the
    // conservative fallback ("all unwritten regs live") prematurely
    // and reject sites that genuinely have dead scratch candidates.
    let live = live_at_with_reader(orig_pc.wrapping_add(4), 32, read_insn);
    let operand_mask: RegMask = (1u16 << d.rt) | (1u16 << d.rn) | match d.offset {
        OffsetForm::Reg { rm, .. } => 1u16 << rm,
        _ => 0,
    };
    let mut picks: [u32; 2] = [u32::MAX; 2];
    let mut n = 0;
    for &r in CANDIDATES {
        let rmask: RegMask = 1u16 << r;
        if rmask & operand_mask != 0 { continue; }
        if rmask & live != 0 { continue; }
        picks[n] = r;
        n += 1;
        if n == 2 { return Some((picks[0], Some(picks[1]))); }
    }
    if n == 1
        && nzcv_dead_recursive(
            orig_pc.wrapping_add(4), 32, &mut Visited::new(), read_insn,
        )
    {
        return Some((picks[0], None));
    }
    None
}

/// Operand-exclusion picker for the stack-fallback stub. Picks 2 regs
/// from `[R12, R0..R3, R14]` that aren't operands of the byte access.
/// Always succeeds (operand set has at most 3 members; candidate pool
/// has 6). Stack save/restore preserves their values across the stub
/// regardless of liveness.
///
/// Retained for the regression tests at the bottom of the file but
/// no longer reachable from `emit_inline_stub`'s live fallback path —
/// `ScratchVA` (and `pick_operand_excluded_triple`) replaces it.
#[cfg(test)]
fn pick_operand_excluded_pair(d: &Decoded) -> (u32, u32) {
    const CANDIDATES: &[u32] = &[12, 0, 1, 2, 3, 14];
    let rm = match d.offset {
        OffsetForm::Reg { rm, .. } => rm,
        _ => u32::MAX,
    };
    let mut picks = [u32::MAX; 2];
    let mut n = 0;
    for &r in CANDIDATES {
        if r == d.rt || r == d.rn || r == rm { continue; }
        picks[n] = r;
        n += 1;
        if n == 2 { break; }
    }
    debug_assert!(n == 2, "operand-exclusion picker must always find 2 regs");
    (picks[0], picks[1])
}

/// Operand-exclusion picker for the ScratchVA-fallback stub. Picks 3
/// regs from `[R12, R0..R3, R14]` that aren't operands of the byte
/// access. Always succeeds (operand set has at most 3 members; candidate
/// pool has 6 → at least 3 left). The stub saves all three caller
/// values (scratch_ea + scratch_fl into the per-stub 8-byte scratch
/// slot, scratch_addr into TPIDRURW) and restores them at exit.
fn pick_operand_excluded_triple(d: &Decoded) -> (u32, u32, u32) {
    const CANDIDATES: &[u32] = &[12, 0, 1, 2, 3, 14];
    let rm = match d.offset {
        OffsetForm::Reg { rm, .. } => rm,
        _ => u32::MAX,
    };
    let mut picks = [u32::MAX; 3];
    let mut n = 0;
    for &r in CANDIDATES {
        if r == d.rt || r == d.rn || r == rm { continue; }
        picks[n] = r;
        n += 1;
        if n == 3 { break; }
    }
    debug_assert!(n == 3, "operand-exclusion picker must always find 3 regs");
    (picks[0], picks[1], picks[2])
}

/// True when the site's operand shape is a simple non-writeback
/// `[Rn, ±imm]` or `[Rn, ±Rm, shift]`. Writeback, post-index, SWPB
/// always go through the UDF emulator.
fn is_inline_eligible(d: &Decoded) -> bool {
    if matches!(d.kind, AccessKind::Swpb) { return false; }
    // No writeback, no post-index.
    if !d.p || d.w { return false; }
    match d.offset {
        OffsetForm::None => false, // SWPB only, handled above
        OffsetForm::Imm { .. } => true,
        OffsetForm::Reg { .. } => true,
    }
}

/// BE-32 XOR mask for this access kind.
fn xor_mask(kind: AccessKind) -> u32 {
    match kind {
        AccessKind::Ldrb | AccessKind::Strb | AccessKind::Ldrsb | AccessKind::Swpb => 3,
        AccessKind::Ldrh | AccessKind::Strh | AccessKind::Ldrsh => 2,
    }
}

/// Stub variant — determines whether scratch regs need stack push/pop.
#[derive(Debug, Clone, Copy)]
enum StubVariant {
    /// Both scratch_ea and scratch_flags are LIVENESS-PROVED dead at
    /// orig_pc+4. No save/restore needed; the stub clobbers them
    /// freely. `sfl == None` indicates NZCV is also dead, so the
    /// MRS/MSR pair is omitted (NOPs in slots 2 and 6).
    DeadReg { sfl: Option<u32> },
    /// Liveness analysis didn't find 2 dead candidates. Operand-
    /// exclusion picker chose scratch_ea / scratch_fl (which may be
    /// genuinely live), and the stub PUSHes them onto the mode-banked
    /// SP at entry and POPs at exit so the caller's values are
    /// preserved across the access.
    ///
    /// Retained for the regression tests at the bottom of the file but
    /// no longer reachable from the live `emit_inline_stub` fallback —
    /// `ScratchVA` replaces it. The fallback used to alias guest stack
    /// pages that Einstein's run leaves unmapped — that hypothesis was
    /// tested via the ScratchVA swap (see plan
    /// `docs/plans/shadow-stub-scratch-va.md`) and exonerated; the
    /// 717006 wedge still fires at the same canary signature with
    /// stack-touching removed.
    #[allow(dead_code)]
    Stack { sfl: u32 },
    /// Liveness analysis didn't find 2 dead candidates. Operand-
    /// exclusion picker chose 3 regs (scratch_ea, scratch_fl,
    /// scratch_addr — none of which is an operand of the byte access).
    /// The stub stores caller scratch_ea / scratch_fl into a per-stub
    /// 8-byte slot in `SCRATCH_POOL` (stage-2-mapped via the L1[0x18]
    /// carve-out at IPA `SCRATCH_POOL_IPA`), and saves caller
    /// scratch_addr into TPIDRURW (one slot, shared across all
    /// ScratchVA stubs — see "Why this is IRQ-safe" in the plan).
    /// The per-stub slot's VA is loaded via `LDR scratch_addr,
    /// [pc, #disp]` from a literal at slot 15.
    ///
    /// `sfl` is always present (NZCV save is required because slot 7's
    /// CMP unconditionally writes flags). `sad` is the third scratch
    /// register that holds the per-stub scratch slot VA.
    ScratchVA { sfl: u32, sad: u32, scratch_slot_idx: usize },
}

/// Build the 16-word inline stub. Three variants share the slot layout
/// — slots 14 (back-branch) and 15 (literal) are at fixed positions;
/// slots 0–13 hold the body.
///
/// Common slots (5–10):
///   5:  <ADD|SUB> scratch_ea, Rn,         #imm_high
///   6:  <ADD|SUB> scratch_ea, scratch_ea, #imm_low | NOP
///   7:  CMP scratch_ea, #XOR_LIMIT
///   8:  EORLO scratch_ea, scratch_ea, #<xor>
///   9:  MSR cpsr_f, scratch_fl  (mirror of slot 4 — variant-dependent)
///   10: <access>[cond] Rt, [scratch_ea]   ; native — may DABT naturally
///   14: B orig_pc + 4
///
/// DeadReg-variant frame:
///   0..3:  NOP
///   4:     MRS scratch_fl, cpsr     (when sfl=Some)  | NOP
///   9:     MSR cpsr_f, scratch_fl   (when sfl=Some)  | NOP
///   11..13:NOP
///   15:    NOP (literal slot unused)
///
/// Stack-variant frame (regression-test only; not reachable from
/// `emit_inline_stub` — see `StubVariant::Stack`):
///   0:     PUSH scratch_ea  (SP -= 4)
///   1:     PUSH scratch_fl  (SP -= 4)
///   2..3:  NOP
///   4:     MRS scratch_fl, cpsr
///   9:     MSR cpsr_f, scratch_fl
///   11:    POP scratch_fl   (SP += 4)
///   12:    POP scratch_ea   (SP += 4)
///   13:    NOP
///   15:    NOP (literal slot unused)
///
/// ScratchVA-variant frame (live fallback for sites where liveness
/// analysis can't find 2 dead candidates):
///   0:     MCR p15,0,scratch_addr,c13,c0,2  ; TPIDRURW <- caller scratch_addr
///   1:     LDR scratch_addr, [PC, #+48]     ; load per-stub scratch slot VA
///   2:     STR scratch_ea, [scratch_addr]   ; save caller scratch_ea
///   3:     STR scratch_fl, [scratch_addr,#4]; save caller scratch_fl
///   4:     MRS scratch_fl, cpsr             ; save NZCV
///   9:     MSR cpsr_f, scratch_fl           ; restore NZCV
///   11:    LDR scratch_fl, [scratch_addr,#4]; restore caller scratch_fl
///   12:    LDR scratch_ea, [scratch_addr]   ; restore caller scratch_ea
///   13:    MRC p15,0,scratch_addr,c13,c0,2  ; restore caller scratch_addr
///   15:    literal: SCRATCH_POOL_IPA + scratch_slot_idx * 8
///
/// Two-step EA compute (slots 5+6) handles 12-bit immediates that
/// don't fit a single ARM modified-immediate (8-bit-rotated). Newton
/// ROM has byte accesses with offsets up to 0xFFF (e.g. 0x156 at
/// 0x26fc0); a single ADD with #0x156 doesn't encode, but
/// (ADD #0x100; ADD #0x56) does.
fn encode_inline_stub(
    d: &Decoded,
    orig_pc: u32,
    stub_ipa: u32,
    sea: u32,
    variant: StubVariant,
) -> Result<[u32; 16], &'static str> {
    // Slots 5+6: compute EA into `sea` via two-step ADD/SUB. Stack-
    // variant + Rn == SP needs a +8 fudge because the two PUSHes
    // displaced SP by -8 from the value the original `[SP, #imm]`
    // access expected.
    let sp_fudge = matches!(variant, StubVariant::Stack { .. }) && d.rn == 13;
    let nop = encode::nop();
    let (slot5, slot6) = match d.offset {
        OffsetForm::Imm { imm } => {
            let signed_imm: i64 = if d.u { imm as i64 } else { -(imm as i64) };
            let adjusted: i64 = signed_imm + if sp_fudge { 8 } else { 0 };
            let abs = if adjusted < 0 { -adjusted } else { adjusted } as u32;
            if abs > 0xFFF {
                return Err("imm > 0xFFF (out of 12-bit range)");
            }
            let is_add = adjusted >= 0;
            // Prefer a single ADD/SUB if the immediate is encodable as
            // a modified-immediate. Otherwise split into (high, low) =
            // (abs & 0xF00, abs & 0xFF) — both are guaranteed
            // encodable since 12-bit values decompose into one 4-bit
            // shifted by 8 and one 8-bit raw.
            if let Some(enc) = encode::arm_imm12(abs) {
                let s5 = if is_add {
                    encode::add_imm(encode::AL, sea, d.rn, enc)
                } else {
                    encode::sub_imm(encode::AL, sea, d.rn, enc)
                };
                (s5, nop)
            } else {
                let high = abs & !0xFFu32;
                let low = abs & 0xFFu32;
                let high_enc = encode::arm_imm12(high)
                    .ok_or("imm_high not encodable (split)")?;
                let low_enc = encode::arm_imm12(low)
                    .ok_or("imm_low not encodable (split)")?;
                let (s5, s6) = if is_add {
                    (
                        encode::add_imm(encode::AL, sea, d.rn, high_enc),
                        encode::add_imm(encode::AL, sea, sea, low_enc),
                    )
                } else {
                    (
                        encode::sub_imm(encode::AL, sea, d.rn, high_enc),
                        encode::sub_imm(encode::AL, sea, sea, low_enc),
                    )
                };
                (s5, s6)
            }
        }
        OffsetForm::Reg { rm, shift_type, shift_amount } => {
            // Slot 5: scratch_ea = Rn ± Rm (with optional shift).
            let s5 = if d.u {
                encode::add_reg_shifted(encode::AL, sea, d.rn, rm, shift_type, shift_amount)
            } else {
                encode::sub_reg_shifted(encode::AL, sea, d.rn, rm, shift_type, shift_amount)
            };
            // Slot 6: only used for stack-variant SP+Rm to apply the
            // +8 push fudge. For all other reg-offset cases it's NOP.
            let s6 = if sp_fudge {
                let enc8 = encode::arm_imm12(8).expect("8 always encodes");
                encode::add_imm(encode::AL, sea, sea, enc8)
            } else {
                nop
            };
            (s5, s6)
        }
        OffsetForm::None => return Err("OffsetForm::None not inline-eligible"),
    };

    let xor_limit_imm = encode::arm_imm12(XOR_LIMIT).ok_or("XOR_LIMIT not encodable")?;
    let slot7 = encode::cmp_imm(sea, xor_limit_imm);

    let xor_enc = encode::arm_imm12(xor_mask(d.kind)).ok_or("xor mask not encodable")?;
    let slot8 = encode::eor_imm_cond(encode::LO, sea, sea, xor_enc);

    let slot10 = encode::access_zero_offset(d.kind, d.cond, d.rt, sea);

    let slot14_pc = stub_ipa.wrapping_add(14 * 4);
    let slot14 = encode::b(slot14_pc, orig_pc.wrapping_add(4))
        .ok_or("back-branch out of B-imm24 range")?;

    // Per-variant frame (slots 0..4, 9, 11..13, 15).
    let mut slot0 = nop;
    let mut slot1 = nop;
    let mut slot2 = nop;
    let mut slot3 = nop;
    let mut slot4 = nop;
    let mut slot9 = nop;
    let mut slot11 = nop;
    let mut slot12 = nop;
    let mut slot13 = nop;
    let mut slot15 = nop;

    match variant {
        StubVariant::DeadReg { sfl: Some(sfl) } => {
            slot4 = encode::mrs_cpsr(sfl);
            slot9 = encode::msr_cpsr_f(sfl);
        }
        StubVariant::DeadReg { sfl: None } => {}
        StubVariant::Stack { sfl } => {
            slot0 = encode::push(sea);
            slot1 = encode::push(sfl);
            slot4 = encode::mrs_cpsr(sfl);
            slot9 = encode::msr_cpsr_f(sfl);
            slot11 = encode::pop(sfl);
            slot12 = encode::pop(sea);
        }
        StubVariant::ScratchVA { sfl, sad, scratch_slot_idx } => {
            // Encoding-time PC at slot 1 = stub_ipa + 4 + 8 = stub_ipa + 12.
            // Literal lives at slot 15 = stub_ipa + 60. Distance = 48.
            const LDR_PC_REL_DISP: u32 = 48;
            slot0 = encode::mcr_p15_0_c13_c0_2(sad);
            slot1 = encode::ldr_pc_rel_pos(sad, LDR_PC_REL_DISP);
            slot2 = encode::str_imm_pos(sea, sad, 0);
            slot3 = encode::str_imm_pos(sfl, sad, 4);
            slot4 = encode::mrs_cpsr(sfl);
            slot9 = encode::msr_cpsr_f(sfl);
            slot11 = encode::ldr_imm_pos(sfl, sad, 4);
            slot12 = encode::ldr_imm_pos(sea, sad, 0);
            slot13 = encode::mrc_p15_0_c13_c0_2(sad);
            if scratch_slot_idx >= SCRATCH_POOL_STUB_CAP {
                return Err("scratch_slot_idx exceeds pool capacity");
            }
            slot15 = scratch_slot_va(scratch_slot_idx);
        }
    };

    Ok([
        slot0,  // 0
        slot1,  // 1
        slot2,  // 2
        slot3,  // 3
        slot4,  // 4
        slot5,  // 5  EA-compute high
        slot6,  // 6  EA-compute low | NOP
        slot7,  // 7  CMP scratch_ea, XOR_LIMIT
        slot8,  // 8  EORLO scratch_ea, scratch_ea, #xor
        slot9,  // 9  MSR cpsr_f, scratch_fl | NOP
        slot10, // 10 native access
        slot11, // 11
        slot12, // 12
        slot13, // 13
        slot14, // 14 back-branch
        slot15, // 15 literal | NOP
    ])
}

/// Emit an inline stub into the pool and overwrite `orig_pc` with
/// `B stub_slot`. Halts at install time on any encoding or pool failure
/// — the ROM is fixed, so an install-time failure means we discovered
/// a site that needs a code change to handle, not a runtime fallback.
fn emit_inline_stub(d: &Decoded, orig_pc: u32) {
    // Iter-49 diagnostic: log scratch picks at sites known to have been
    // mis-picked in production. Add more PCs here as bugs surface.
    // Compile-time-cheap (just an array compare); narrow output.
    const TRACE_PICK_SITES: &[u32] = &[
        0x0014_88AC, // FindSuperceeder body — iter-49 R12-misclassification
    ];
    let trace_pick = TRACE_PICK_SITES.contains(&orig_pc);

    let (sea, variant) = match pick_scratch_regs(d, orig_pc) {
        Some((sea, sfl)) => {
            if trace_pick {
                kprintln!(
                    "shadow_stub pick @{:#010x}: DeadReg sea=R{} sfl={:?}",
                    orig_pc, sea, sfl,
                );
            }
            (sea, StubVariant::DeadReg { sfl })
        }
        None => {
            // Liveness analysis didn't find 2 dead candidates (or 1 +
            // dead-NZCV). Fall back to the ScratchVA-based stub: the
            // operand-exclusion picker gives us 3 free registers
            // (always — at most 3 operand registers out of a 6-element
            // candidate pool). The stub saves caller scratch_ea +
            // scratch_fl into a per-stub 8-byte slot in `SCRATCH_POOL`
            // (stage-2-mapped via the L1[0x18] carve-out at IPA
            // `SCRATCH_POOL_IPA`); caller scratch_addr goes into
            // TPIDRURW.
            //
            // The previous fallback (`StubVariant::Stack`) PUSH/POP'd
            // onto the mode-banked SP, which lazily mapped guest stack
            // pages and masked a kernel-mode `TStackManager::Fault`
            // chain Einstein's run takes. ScratchVA touches a hyper-
            // visor-owned VA so it has no observable side effect on
            // the guest's stack page accounting. See
            // `docs/plans/shadow-stub-scratch-va.md` for details.
            let (sea, sfl, sad) = pick_operand_excluded_triple(d);
            if trace_pick {
                kprintln!(
                    "shadow_stub pick @{:#010x}: ScratchVA sea=R{} sfl=R{} sad=R{}",
                    orig_pc, sea, sfl, sad,
                );
            }
            let scratch_slot_idx = NEXT_SCRATCH_SLOT.fetch_add(1, Ordering::SeqCst);
            if scratch_slot_idx >= SCRATCH_POOL_STUB_CAP {
                kprintln!(
                    "shadow_stub: FATAL — ScratchVA scratch pool exhausted at PC={:#x} ({} slots)",
                    orig_pc, scratch_slot_idx
                );
                crate::cpu::halt();
            }
            (sea, StubVariant::ScratchVA { sfl, sad, scratch_slot_idx })
        }
    };

    let slot_ix = NEXT_STUB.fetch_add(1, Ordering::SeqCst);
    if slot_ix >= SBA_STUB_MAX {
        kprintln!(
            "shadow_stub: FATAL — inline stub pool exhausted at PC={:#x} ({} slots)",
            orig_pc, slot_ix
        );
        crate::cpu::halt();
    }
    let stub_ipa = SBA_STUB_POOL_IPA + (slot_ix as u32) * SBA_STUB_BYTES;

    let br = match encode::b(orig_pc, stub_ipa) {
        Some(w) => w,
        None => {
            kprintln!(
                "shadow_stub: FATAL — forward B from PC={:#x} to stub IPA={:#x} \
                 out of imm24 range",
                orig_pc, stub_ipa
            );
            crate::cpu::halt();
        }
    };
    let stub_words = match encode_inline_stub(d, orig_pc, stub_ipa, sea, variant) {
        Ok(ws) => ws,
        Err(reason) => {
            kprintln!(
                "shadow_stub: FATAL — encode_inline_stub at PC={:#x}: {}",
                orig_pc, reason
            );
            crate::cpu::halt();
        }
    };

    for (i, w) in stub_words.iter().enumerate() {
        let ipa = stub_ipa.wrapping_add((i as u32) * 4);
        if let Err(e) = code_write_word(ipa, *w) {
            kprintln!(
                "shadow_stub: FATAL — couldn't write stub word at IPA {:#x}: {}",
                ipa, e
            );
            crate::cpu::halt();
        }
    }
    if let Err(e) = code_write_word(orig_pc, br) {
        kprintln!(
            "shadow_stub: FATAL — couldn't write B->stub at PC {:#x}: {}",
            orig_pc, e
        );
        crate::cpu::halt();
    }
}

/// Install a UDF marker at `pc` for the site. Halts if the UDF table is
/// exhausted.
fn emit_udf_site(pc: u32, insn: u32) {
    let idx = NEXT_SITE.fetch_add(1, Ordering::SeqCst);
    if idx >= SBA_MAX_SITES {
        kprintln!(
            "shadow_stub: ERROR - SBA UDF table exhausted at PC {:#x} ({} sites)",
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
}

/// Decode + install a stub at `pc`. Picks inline vs UDF based on the
/// site's operand shape, `force_udf` (RAM-resident blocks), and the
/// inline stub pool's encoding constraints.
fn patch_one_site(pc: u32, force_udf: bool, stats: &mut PatchStats) {
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

    let use_inline = !force_udf && is_inline_eligible(&decoded);
    if use_inline {
        // Halts at install time on any failure (liveness, encoding,
        // pool-full, branch-range). The ROM is fixed, so a failure
        // means we discovered a site that needs handler work — not a
        // condition to silently fall back.
        emit_inline_stub(&decoded, pc);
        stats.inline_stubs += 1;
    } else {
        emit_udf_site(pc, insn);
        stats.udf_fallback += 1;
    }

    match decoded.kind {
        AccessKind::Ldrb | AccessKind::Strb => stats.ldrb_strb += 1,
        AccessKind::Ldrh | AccessKind::Strh => stats.ldrh_strh += 1,
        AccessKind::Ldrsb | AccessKind::Ldrsh => stats.ldrsb_ldrsh += 1,
        AccessKind::Swpb => stats.swpb += 1,
    }
    stats.patched += 1;
}

/// Flush the inline-stub pool byte range covering `[first, first+count)`
/// slots to the point of unification so the guest can fetch freshly-
/// emitted stubs natively.
fn flush_stub_pool(first_slot: usize, count: usize) {
    if count == 0 { return; }
    let start_ipa = SBA_STUB_POOL_IPA + (first_slot as u32) * SBA_STUB_BYTES;
    let byte_len = (count as u32) * SBA_STUB_BYTES;
    let host = crate::guest_mem::rom_host_pa() + start_ipa as u64;
    icache_sync_range(host, byte_len as usize);
}

/// Patch every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH/SWPB in `[start_ipa, end_ipa)`
/// of the ROM or RAM backing. Used for the lazy-RAM path (RAM-resident
/// code copied out of ROM at boot) where there is no pre-computed
/// classifier bitmap, and by the `test_shadow_stub` guest test which
/// scans its own code range.
///
/// RAM-aperture ranges go through the UDF emulator unconditionally — the
/// inline-stub pool in ROM is out of B-instruction range from RAM, and
/// the UDF path has an equivalent pre-fault retry round-trip for cross-
/// page DABTs (see `handle_sba_udf`).
pub fn patch_code_range(start_ipa: u32, end_ipa: u32) -> PatchStats {
    assert!(start_ipa & 3 == 0);
    assert!(end_ipa & 3 == 0);
    assert!(end_ipa >= start_ipa);

    let force_udf = (start_ipa as usize) >= crate::guest_mem::ROM_SIZE;

    let first_stub = NEXT_STUB.load(Ordering::SeqCst);
    let mut stats = PatchStats::default();
    let mut pc = start_ipa;
    while pc < end_ipa {
        stats.words_scanned += 1;
        patch_one_site(pc, force_udf, &mut stats);
        pc = pc.wrapping_add(4);
    }
    let last_stub = NEXT_STUB.load(Ordering::SeqCst);
    if last_stub > first_stub {
        flush_stub_pool(first_stub, last_stub - first_stub);
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

    let first_stub = NEXT_STUB.load(Ordering::SeqCst);
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
            patch_one_site(pc, /*force_udf=*/ false, &mut stats);
        }
    }
    let last_stub = NEXT_STUB.load(Ordering::SeqCst);
    if last_stub > first_stub {
        flush_stub_pool(first_stub, last_stub - first_stub);
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
         (inline={}, UDF={}; LDRB/STRB={}, LDRH/STRH={}, LDRSB/LDRSH={}, SWPB={}), \
         skipped {} PC-operand, \
         site table {}/{}, inline pool {}/{}, scratch slots {}/{}",
        stats.words_scanned, stats.patched,
        stats.inline_stubs, stats.udf_fallback,
        stats.ldrb_strb, stats.ldrh_strh, stats.ldrsb_ldrsh, stats.swpb,
        stats.skipped_pc_operand,
        NEXT_SITE.load(Ordering::SeqCst), SBA_MAX_SITES,
        NEXT_STUB.load(Ordering::SeqCst), SBA_STUB_MAX,
        NEXT_SCRATCH_SLOT.load(Ordering::SeqCst), SCRATCH_POOL_STUB_CAP,
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

// Banked SP/LR for the **faulting** AArch32 mode (the mode that
// executed the SBA-UDF) are handled via the trampoline's pre-HVC
// stash slots `UND_SAVE_BANKED_{SP,LR}_IPA`, NOT via `ctx.x[]`.
//
// Why the RAM slots: the trampoline runs in UND mode, so per ARM ARM
// DDI 0487 D1.21.1 Table D1-79 the AArch64 GPR file at HVC entry
// holds X22 = LR_und, X23 = SP_und — which are NOT the faulting
// mode's R13/R14. (X13 = SP_usr, X14 = LR_usr, also unrelated.) The
// trampoline therefore mode-switches to the faulting mode (or SYS
// when faulting mode = USR), reads R13/R14 in-mode, and persists
// them to RAM before issuing HVC #UND_TAG. `Regs::snapshot` reads
// those slots; writeback to R13/R14 routes through the post-
// emulation trampoline (`dispatch_return`) so the new values land
// in the target mode's banked R13/R14 natively, since AArch64 ERET
// to AArch32 places X13/X14 into R13_usr/R14_usr regardless of the
// SPSR mode field.
//
// (R13 and R14 of the faulting mode would also be reachable as
// X13..X23 per Table D1-79 if the trampoline HVC'd in the faulting
// mode directly — but the UND trampoline necessarily runs in UND,
// so the X-register snapshot reflects UND/USR banks, not the
// faulting mode's. The RAM-slot route is what makes the snapshot
// faulting-mode-correct.)

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
    let pa = resolve_addr(ea).unwrap_or_else(|| {
        // Unreachable: handle_sba_udf pre-filters walk-fails through the
        // pre-fault retry path before reaching dispatch_*. Defensive halt.
        kprintln!(
            "*** shadow_stub: byte read walk-fail ea={:#x} pc={:#x} (retry path bug)",
            ea, faulting_pc
        );
        crate::cpu::halt();
    });
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
    let pa = resolve_addr(ea).unwrap_or_else(|| {
        kprintln!(
            "*** shadow_stub: byte write walk-fail ea={:#x} pc={:#x} (retry path bug)",
            ea, faulting_pc
        );
        crate::cpu::halt();
    });
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
    let pa = resolve_addr(ea).unwrap_or_else(|| {
        kprintln!(
            "*** shadow_stub: halfword read walk-fail ea={:#x} pc={:#x} (retry path bug)",
            ea, faulting_pc
        );
        crate::cpu::halt();
    });
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
    let pa = resolve_addr(ea).unwrap_or_else(|| {
        kprintln!(
            "*** shadow_stub: halfword write walk-fail ea={:#x} pc={:#x} (retry path bug)",
            ea, faulting_pc
        );
        crate::cpu::halt();
    });
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

    emulate_sba_site(ctx, faulting_pc, spsr_und, orig_insn, idx, /*is_retry=*/ false)
}

/// Continuation of `handle_sba_udf` after a pre-fault retry round-trip.
/// Called from `handle_hvc` on `SBA_RETRY_TAG`. Restores the stashed
/// ctx and resumes the emulator body.
pub fn handle_sba_retry(ctx: &mut TrapContext) {
    if !SBA_RETRY_PENDING.swap(false, Ordering::SeqCst) {
        kprintln!("*** shadow_stub: SBA_RETRY HVC without pending retry — halting");
        crate::cpu::halt();
    }
    // SAFETY: pending flag just consumed; single-threaded.
    let (faulting_pc, spsr_und, idx, orig_insn) = unsafe {
        for i in 0..15 {
            ctx.x[i] = SBA_RETRY_CTX[i];
        }
        (
            SBA_RETRY_FAULTING_PC,
            SBA_RETRY_SPSR_UND,
            SBA_RETRY_IDX,
            SBA_ORIG_INSN[SBA_RETRY_IDX],
        )
    };
    if !emulate_sba_site(ctx, faulting_pc, spsr_und, orig_insn, idx, /*is_retry=*/ true) {
        kprintln!(
            "*** shadow_stub: retry emulator failed at PC={:#x} insn={:#010x}",
            faulting_pc, orig_insn
        );
        crate::cpu::halt();
    }
}

/// Stash the emulator state and ERET into the pre-fault stub. Returns
/// control to the vector trailer; the trailer's ERET lands in UND mode
/// at `SBA_PREFAULT_STUB_VA` with `ctx.x[0] = access_addr`. The stub's
/// `LDRB r0, [r0]` either succeeds immediately (EA already mapped
/// post-kernel-pager), or takes a natural DABT that the kernel's
/// handler pages in and retries via `subs pc, lr, #8`. The stub's
/// subsequent `HVC #SBA_RETRY_TAG` returns to `handle_sba_retry`.
fn trigger_pre_fault_retry(
    ctx: &mut TrapContext,
    access_addr: u32,
    faulting_pc: u32,
    spsr_und: u64,
    idx: usize,
) {
    if SBA_RETRY_PENDING.swap(true, Ordering::SeqCst) {
        kprintln!(
            "*** shadow_stub: nested SBA retry at PC={:#x} (pending already) — halting",
            faulting_pc
        );
        crate::cpu::halt();
    }
    // SAFETY: single-threaded EL2; pending flag just flipped.
    unsafe {
        for i in 0..15 {
            SBA_RETRY_CTX[i] = ctx.x[i];
        }
        SBA_RETRY_FAULTING_PC = faulting_pc;
        SBA_RETRY_SPSR_UND = spsr_und;
        SBA_RETRY_IDX = idx;
    }
    ctx.x[0] = access_addr as u64;
    // ERET target: the pre-fault stub. SPSR_EL2 is left as the HVC's
    // auto-saved value (= UND), so the stub runs in UND mode at PL1.
    // SAFETY: ELR_EL2 write only — vector trailer does the actual ERET.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "isb",
            elr = in(reg) crate::guest_mem::SBA_PREFAULT_STUB_VA as u64,
            options(nostack, preserves_flags),
        );
    }
}

/// Core emulator body. Called both from `handle_sba_udf` (first-time
/// emulation) and from `handle_sba_retry` (after the pre-fault retry
/// has paged the EA in). On a walk-fail it triggers the retry round-
/// trip — unless already on the retry path, in which case it halts.
fn emulate_sba_site(
    ctx: &mut TrapContext,
    faulting_pc: u32,
    spsr_und: u64,
    orig_insn: u32,
    idx: usize,
    is_retry: bool,
) -> bool {
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

    // Walk the guest's stage-1 for the EA. If the walk fails, the
    // unpatched site would have taken a DABT on hardware. Route through
    // the pre-fault retry stub so the kernel's own DataAbortHandler
    // grows the heap / stack / on-demand-paging range; the retry HVC
    // resumes this emulator body with the same idx, but now
    // `resolve_addr` succeeds. The XOR inside `dispatch_*` only touches
    // the low two bits (same 4 KiB page), so this single check covers
    // both the raw EA and its BE-32 alias.
    if access_addr < XOR_LIMIT && resolve_addr(access_addr).is_none() {
        if is_retry {
            kprintln!(
                "*** shadow_stub: retry probe succeeded but EA {:#x} still unmapped at pc={:#x}",
                access_addr, faulting_pc
            );
            crate::cpu::halt();
        }
        trigger_pre_fault_retry(ctx, access_addr, faulting_pc, spsr_und, idx);
        return true;
    }

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
        // imm16 = 0xFFFE (guest_bp marker) — UDF A1 encoding fixes
        // bits 7:4 to 0xF (imm12<<8 covers bits 19:8, imm4 covers
        // bits 3:0; the SBO at bits 7:4 is part of the opcode).
        assert_eq!(enc_udf(0xFFFE), 0xE7FF_FFFE);
        // imm16 = 0x8000 (first SBA slot).
        let w = enc_udf(0x8000);
        assert!(is_sba_udf_insn(w));
        assert_eq!(udf_imm16(w), 0x8000);
    }

    #[test]
    fn arm_imm12_roundtrip() {
        use super::encode::arm_imm12;
        // 0, small positives, the XOR_LIMIT constant, and a few rotated
        // values should all encode.
        assert!(arm_imm12(0).is_some());
        assert!(arm_imm12(3).is_some());
        assert!(arm_imm12(255).is_some());
        assert!(arm_imm12(XOR_LIMIT).is_some());         // 0x10000000 = 1 ROR 4
        assert!(arm_imm12(0x0000_FF00).is_some());       // 0xFF << 8
        assert!(arm_imm12(0x00FF_0000).is_some());       // 0xFF << 16
        // 12-bit all-ones doesn't encode as a rotated 8-bit value.
        assert!(arm_imm12(0x0000_0FFF).is_none());
        // 0x101 isn't either (two separate bits).
        assert!(arm_imm12(0x0000_0101).is_none());
    }

    #[test]
    fn inline_stub_dead_reg_layout() {
        // LDRB r0, [r1, #4] with sfl=Some(R0): MRS slot 4, MSR slot 9,
        // NOPs in all the other variant-frame slots. Slot 6 is NOP
        // since imm=4 fits a single ADD.
        let d = decode(0xE5D1_0004).unwrap();
        let stub = encode_inline_stub(
            &d, 0x0004_0000, 0x00E0_0000, 12, StubVariant::DeadReg { sfl: Some(0) },
        ).expect("stub");
        let nop = encode::nop();
        for i in [0usize, 1, 2, 3, 6, 11, 12, 13, 15] {
            assert_eq!(stub[i], nop, "slot {} should be NOP", i);
        }
        assert_eq!(stub[4] & 0xFFFF_0FFF, 0xE10F_0000);  // MRS
        assert_eq!(stub[9] & 0xFFFF_FFF0, 0xE128_F000);  // MSR
        let decoded_access = decode(stub[10]).expect("access decodes");
        assert_eq!(decoded_access.kind, AccessKind::Ldrb);
        let slot14_pc = 0x00E0_0000u32 + 14 * 4;
        let expected = encode::b(slot14_pc, 0x0004_0004).unwrap();
        assert_eq!(stub[14], expected);
    }

    #[test]
    fn inline_stub_dead_reg_nzcv_dead_layout() {
        // sfl=None (NZCV-dead): MRS/MSR slots also NOP.
        let d = decode(0xE5D1_0004).unwrap();
        let stub = encode_inline_stub(
            &d, 0x0004_0000, 0x00E0_0000, 12, StubVariant::DeadReg { sfl: None },
        ).expect("stub");
        let nop = encode::nop();
        for i in [0usize, 1, 2, 3, 4, 6, 9, 11, 12, 13, 15] {
            assert_eq!(stub[i], nop, "slot {} should be NOP", i);
        }
    }

    #[test]
    fn inline_stub_stack_layout() {
        // Stack variant (regression-only): slots 0/1 PUSH, 11/12 POP.
        let d = decode(0xE5D1_0004).unwrap();
        let stub = encode_inline_stub(
            &d, 0x0004_0000, 0x00E0_0000, 12, StubVariant::Stack { sfl: 0 },
        ).expect("stub");
        assert_eq!(stub[0] & 0xFFFF_0FFF, 0xE52D_0004);  // PUSH scratch_ea
        assert_eq!(stub[1] & 0xFFFF_0FFF, 0xE52D_0004);  // PUSH scratch_fl
        assert_eq!(stub[11] & 0xFFFF_0FFF, 0xE49D_0004); // POP scratch_fl
        assert_eq!(stub[12] & 0xFFFF_0FFF, 0xE49D_0004); // POP scratch_ea
    }

    #[test]
    fn inline_stub_scratch_va_layout() {
        // ScratchVA variant: slot 0 = MCR (save sad → TPIDRURW),
        // slot 1 = LDR sad,[pc,#48], slots 2/3 = STR sea/sfl,[sad,#0/4],
        // slot 4 = MRS, slot 9 = MSR, slots 11/12 = LDR sfl/sea, slot 13 = MRC,
        // slot 14 = back-branch, slot 15 = literal scratch VA.
        let d = decode(0xE5D1_0004).unwrap();
        let stub = encode_inline_stub(
            &d, 0x0004_0000, 0x00E0_0000, 12,
            StubVariant::ScratchVA { sfl: 0, sad: 1, scratch_slot_idx: 7 },
        ).expect("stub");
        // slot 0: MCR p15,0,r1,c13,c0,2 = 0xEE0D_1F50
        assert_eq!(stub[0], 0xEE0D_1F50);
        // slot 1: LDR r1, [pc, #48] = 0xE59F_1030
        assert_eq!(stub[1], 0xE59F_1030);
        // slot 2: STR r12, [r1] = 0xE581_C000
        assert_eq!(stub[2], 0xE581_C000);
        // slot 3: STR r0, [r1, #4] = 0xE581_0004
        assert_eq!(stub[3], 0xE581_0004);
        // slot 4: MRS r0, cpsr  (mrs_cpsr(0))
        assert_eq!(stub[4] & 0xFFFF_0FFF, 0xE10F_0000);
        // slot 9: MSR cpsr_f, r0
        assert_eq!(stub[9] & 0xFFFF_FFF0, 0xE128_F000);
        // slot 10: LDRB r0, [r12]
        let decoded_access = decode(stub[10]).expect("access decodes");
        assert_eq!(decoded_access.kind, AccessKind::Ldrb);
        // slot 11: LDR r0, [r1, #4] = 0xE591_0004
        assert_eq!(stub[11], 0xE591_0004);
        // slot 12: LDR r12, [r1] = 0xE591_C000
        assert_eq!(stub[12], 0xE591_C000);
        // slot 13: MRC p15,0,r1,c13,c0,2 = 0xEE1D_1F50
        assert_eq!(stub[13], 0xEE1D_1F50);
        // slot 14: back-branch
        let slot14_pc = 0x00E0_0000u32 + 14 * 4;
        let expected_branch = encode::b(slot14_pc, 0x0004_0004).unwrap();
        assert_eq!(stub[14], expected_branch);
        // slot 15: literal = SCRATCH_POOL_VA + 7 * 8
        assert_eq!(stub[15], SCRATCH_POOL_VA + 7 * (SCRATCH_BYTES_PER_STUB as u32));
    }

    #[test]
    fn inline_stub_imm_split_for_large_offset() {
        // LDRB r0, [r1, #0x156] — encoding e5d10156. 0x156 doesn't fit
        // a single ARM modified-immediate, so the EA compute splits
        // into ADD #0x100 (slot 5) + ADD #0x56 (slot 6).
        let insn: u32 = 0xE5D1_0156;
        let d = decode(insn).expect("decode");
        let stub = encode_inline_stub(
            &d, 0x0004_0000, 0x00E0_0000, 12, StubVariant::DeadReg { sfl: None },
        ).expect("stub with split imm");
        // Slot 5: ADD scratch_ea, Rn(=R1), #0x100.
        let s5 = stub[5];
        assert_eq!((s5 >> 28) & 0xF, encode::AL);
        assert_eq!(s5 & 0x0FE0_0000, 0x0280_0000); // ADD imm
        assert_eq!((s5 >> 16) & 0xF, 1); // Rn=R1
        assert_eq!(s5 & 0xFFF, encode::arm_imm12(0x100).unwrap());
        // Slot 6: ADD scratch_ea, scratch_ea, #0x56.
        let s6 = stub[6];
        assert_eq!(s6 & 0x0FE0_0000, 0x0280_0000);
        assert_eq!(s6 & 0xFFF, encode::arm_imm12(0x56).unwrap());
    }

    #[test]
    fn inline_stub_stack_sp_imm_fudges_by_8() {
        // LDRB r0, [SP, #4] stack variant: +8 fudge → ADD #12 in
        // slots 5+6. Since 12 fits a single ADD, slot 6 is NOP.
        let mut w: u32 = 0;
        w |= 0xE << 28;
        w |= 0b010 << 25;
        w |= 1 << 24;
        w |= 1 << 23;
        w |= 1 << 22;
        w |= 1 << 20;
        w |= 13 << 16;
        w |= 0 << 12;
        w |= 4;
        let d = decode(w).unwrap();
        let stub = encode_inline_stub(
            &d, 0x0004_0000, 0x00E0_0000, 12, StubVariant::Stack { sfl: 1 },
        ).expect("stub");
        let s5 = stub[5];
        assert_eq!((s5 >> 16) & 0xF, 13);
        assert_eq!(s5 & 0x0FE0_0000, 0x0280_0000);
        assert_eq!(s5 & 0xFFF, encode::arm_imm12(12).unwrap());
        // Slot 6 is NOP since 12 fits one step.
        assert_eq!(stub[6], encode::nop());
    }

    /// Helper: classify whether a BranchKind ended the basic block.
    fn is_block_terminator(kind: BranchKind) -> bool {
        !matches!(kind, BranchKind::Linear | BranchKind::BLink { .. })
    }

    #[test]
    fn analyze_dp_immediate() {
        // MOV r0, #0 — writes r0, no reads.
        let (read, write, kind) = analyze_insn(0xE3A0_0000, 0);
        assert_eq!(read, 0);
        assert_eq!(write, 1u16 << 0);
        assert!(!is_block_terminator(kind));

        // CMP r0, #0 — reads r0, no write (CMP is no-writeback opcode).
        let (read, write, kind) = analyze_insn(0xE350_0000, 0);
        assert_eq!(read, 1u16 << 0);
        assert_eq!(write, 0);
        assert!(!is_block_terminator(kind));
    }

    #[test]
    fn analyze_dp_register() {
        // MOV r0, r1 — reads r1, writes r0.
        let (read, write, kind) = analyze_insn(0xE1A0_0001, 0);
        assert_eq!(read, 1u16 << 1);
        assert_eq!(write, 1u16 << 0);
        assert!(!is_block_terminator(kind));

        // ADD r0, r1, r2 — reads r1, r2; writes r0.
        let (read, write, kind) = analyze_insn(0xE081_0002, 0);
        assert_eq!(read, (1u16 << 1) | (1u16 << 2));
        assert_eq!(write, 1u16 << 0);
        assert!(!is_block_terminator(kind));
    }

    #[test]
    fn analyze_branch_classification() {
        // BL <imm24>=0 — call to PC+8.
        let (_, _, kind) = analyze_insn(0xEB00_0000, 0x100);
        assert!(matches!(kind, BranchKind::BLink { target } if target == 0x108));
        // B <imm24>=0 — direct branch to PC+8.
        let (_, _, kind) = analyze_insn(0xEA00_0000, 0x100);
        assert!(matches!(kind, BranchKind::Direct { target } if target == 0x108));
        // BNE <imm24>=0 — conditional branch.
        let (_, _, kind) = analyze_insn(0x1A00_0000, 0x100);
        assert!(matches!(kind, BranchKind::Cond { target } if target == 0x108));
        // BX LR — APCS return.
        let (read, _, kind) = analyze_insn(0xE12F_FF1E, 0);
        assert!(matches!(kind, BranchKind::Return));
        assert_eq!(read & APCS_RETURN_LIVE, APCS_RETURN_LIVE);
        // BX r3 (non-LR) — indirect.
        let (read, _, kind) = analyze_insn(0xE12F_FF13, 0);
        assert!(matches!(kind, BranchKind::Indirect));
        assert_eq!(read, 1u16 << 3);
        // BLX r3 — like a function call.
        let (read, _, kind) = analyze_insn(0xE12F_FF33, 0x100);
        assert!(matches!(kind, BranchKind::BLink { target } if target == 0x104));
        assert_eq!(read, 1u16 << 3);
    }

    #[test]
    fn analyze_returns() {
        // MOV PC, LR (e1a0_f00e) — APCS return via DP-reg.
        let (_, _, kind) = analyze_insn(0xE1A0_F00E, 0);
        assert!(matches!(kind, BranchKind::Return));
        // POP {r0, pc} — LDM SP!, {r0, pc}.
        let (_, _, kind) = analyze_insn(0xE8BD_8001, 0);
        assert!(matches!(kind, BranchKind::Return));
        // LDR PC, [SP], #4 — single-reg pop-return.
        let (_, _, kind) = analyze_insn(0xE49D_F004, 0);
        assert!(matches!(kind, BranchKind::Return));
        // LDMDB FP, {r4-r11, sp, pc} = e91baff0 — frame-pointer
        // variant of POP {…, PC}. Newton ROM uses this widely.
        let (_, _, kind) = analyze_insn(0xE91B_AFF0, 0);
        assert!(matches!(kind, BranchKind::Return));
    }

    #[test]
    fn analyze_loads_stores() {
        // LDR r0, [r1] — reads r1, writes r0.
        let (read, write, kind) = analyze_insn(0xE591_0000, 0);
        assert_eq!(read, 1u16 << 1);
        assert_eq!(write, 1u16 << 0);
        assert!(!is_block_terminator(kind));
        // STR r0, [r1] — reads r0, r1; no GPR write.
        let (read, write, kind) = analyze_insn(0xE581_0000, 0);
        assert_eq!(read, (1u16 << 0) | (1u16 << 1));
        assert_eq!(write, 0);
        assert!(!is_block_terminator(kind));
        // LDR r0, [r1, #4]! — pre-index writeback.
        let (read, write, kind) = analyze_insn(0xE5B1_0004, 0);
        assert_eq!(read, 1u16 << 1);
        assert_eq!(write, (1u16 << 0) | (1u16 << 1));
        assert!(!is_block_terminator(kind));
        // LDR r0, [r1], #4 — post-index.
        let (read, write, kind) = analyze_insn(0xE491_0004, 0);
        assert_eq!(read, 1u16 << 1);
        assert_eq!(write, (1u16 << 0) | (1u16 << 1));
        assert!(!is_block_terminator(kind));
    }

    #[test]
    fn analyze_movw_movt() {
        // MOVW r4, #0x3000 — `e3034000`. Writes r4, no GPR reads.
        // (The imm4h in bits 19:16 must NOT be misread as Rn.)
        let (read, write, kind) = analyze_insn(0xE303_4000, 0);
        assert_eq!(read, 0);
        assert_eq!(write, 1u16 << 4);
        assert!(matches!(kind, BranchKind::Linear));
        // MOVT r4, #0x400 — `e3404400`. Reads r4 (preserves low half), writes r4.
        let (read, write, kind) = analyze_insn(0xE340_4400, 0);
        assert_eq!(read, 1u16 << 4);
        assert_eq!(write, 1u16 << 4);
        assert!(matches!(kind, BranchKind::Linear));
    }

    #[test]
    fn analyze_hvc_classification() {
        // HVC #2 — should NOT be misclassified as DP-reg-shifted.
        let (read, write, kind) = analyze_insn(0xE140_0072, 0x100);
        assert!(matches!(kind, BranchKind::BLink { .. }));
        assert_eq!(read, 0);
        assert_eq!(write, 0);
    }

    #[test]
    fn analyze_svc_is_blink() {
        // SVC #0 — function-call-shaped. Caller's APCS-saved regs are
        // observably clobbered; analyzer must continue at PC+4 rather
        // than bailing conservatively.
        let (read, write, kind) = analyze_insn(0xEF00_0000, 0x100);
        assert!(matches!(kind, BranchKind::BLink { target } if target == 0x104));
        assert_eq!(read, 0);
        assert_eq!(write, 0);
    }

    #[test]
    fn liveness_linear_finds_dead_reg() {
        // Synthetic insn stream:
        //   MOV r12, #0   ; writes r12 (no read)
        //   BX LR         ; APCS return; reads APCS_RETURN_LIVE
        // → r12 is dead at start (written before any read).
        let stream = [0xE3A0_C000u32, 0xE12F_FF1Eu32];
        let live = live_at_with_reader(0, 16, &|pc| stream.get((pc / 4) as usize).copied());
        assert_eq!(live & (1u16 << 12), 0, "r12 should be dead");
        // R0 should be live (return value reg).
        assert_ne!(live & (1u16 << 0), 0, "r0 should be live (return value)");
        // R4..R11 should be live (callee-preserved).
        for r in 4..=11 {
            assert_ne!(live & (1u16 << r), 0, "r{} should be live (callee-preserved)", r);
        }
        // R1, R2, R3 should be dead (caller-saved scratch, not preserved).
        assert_eq!(live & (1u16 << 1), 0, "r1 should be dead");
        assert_eq!(live & (1u16 << 2), 0, "r2 should be dead");
        assert_eq!(live & (1u16 << 3), 0, "r3 should be dead");
    }

    #[test]
    fn liveness_bl_param_regs_live() {
        // Stream:
        //   MOV r4, #0   ; r4 dead
        //   BL +0        ; reads R0..R3 as params; clobbers R12, LR
        //   BX LR        ; return
        // R0..R3 are LIVE because the callee reads them as parameter
        // registers (we don't know the signature, so all four are
        // assumed live). R12 and LR are dead — written by BL, never
        // read in the post-BL fragment.
        let stream = [0xE3A0_4000u32, 0xEB00_0000u32, 0xE12F_FF1Eu32];
        let live = live_at_with_reader(0, 16, &|pc| stream.get((pc / 4) as usize).copied());
        for r in [0u16, 1, 2, 3] {
            assert_ne!(live & (1u16 << r), 0,
                "r{} should be live (BL parameter reg)", r);
        }
        for r in [4u16, 12, 14] {
            assert_eq!(live & (1u16 << r), 0,
                "r{} should be dead (clobbered before any read)", r);
        }
    }

    #[test]
    fn liveness_bl_param_set_just_before_call() {
        // The Newton ROM @ 0x13ca08 pattern:
        //   MOV r1, r3    ; set up param r1 from local r3
        //   ... (linear straight-line code that doesn't touch r1) ...
        //   BL callee     ; consumes r1 as a param
        // r1 must be reported as LIVE at start (the BL consumes the
        // value `mov r1, r3` placed there). Pre-fix, the walker missed
        // this and the inline-stub picker would happily clobber r1
        // with CPSR.
        let stream = [
            0xE1A0_1003u32, // MOV r1, r3
            0xE3A0_5000u32, // MOV r5, #0   (filler — doesn't touch r1)
            0xE3A0_6000u32, // MOV r6, #0
            0xEB00_0000u32, // BL +0
            0xE12F_FF1Eu32, // BX LR
        ];
        let live = live_at_with_reader(0, 16, &|pc| stream.get((pc / 4) as usize).copied());
        // r1 was written by `mov r1, r3` so r1 is dead at start
        // (the BL's read of r1 is satisfied by the local write).
        assert_eq!(live & (1u16 << 1), 0,
            "r1 dead at start — `mov r1, r3` writes it before the BL");
        // But r3 IS live at start: it was read by `mov r1, r3` and
        // then the BL's read of r1 (which now == the original r3) is
        // not what we're tracking. r3 is live because the local
        // instruction `mov r1, r3` reads it.
        assert_ne!(live & (1u16 << 3), 0, "r3 live at start (read by `mov r1, r3`)");
        // r0, r2 are param-live (BL reads them, never written locally).
        assert_ne!(live & (1u16 << 0), 0, "r0 param-live");
        assert_ne!(live & (1u16 << 2), 0, "r2 param-live");
    }

    #[test]
    fn liveness_cond_return_walks_fallthrough() {
        // Newton ROM @ 0x2595c8 motif: a conditional return is followed
        // by code that reads parameter registers (used by the function
        // tail). The walker must walk the fall-through, or those reads
        // are missed and the inline-stub picker concludes the regs are
        // dead. Stream:
        //   pc= 0: TEQ r0, #0
        //   pc= 4: LDMDBNE fp, {r4,fp,sp,pc}   ; cond return
        //   pc= 8: STR r1, [r4, #8]            ; reads r1 (and r4)
        //   pc=12: STR r3, [r4]                ; reads r3
        //   pc=16: BX LR                       ; return
        let stream = [
            0xE330_0000u32, // TEQ r0, #0
            0x191B_A810u32, // LDMDBNE fp, {r4, fp, sp, pc}
            0xE584_1008u32, // STR r1, [r4, #8]
            0xE584_3000u32, // STR r3, [r4]
            0xE12F_FF1Eu32, // BX LR
        ];
        let live = live_at_with_reader(0, 16, &|pc| stream.get((pc / 4) as usize).copied());
        assert_ne!(live & (1u16 << 1), 0,
            "r1 must be live — read on the post-cond-return fall-through");
        assert_ne!(live & (1u16 << 3), 0,
            "r3 must be live — read on the post-cond-return fall-through");
        assert_ne!(live & (1u16 << 4), 0,
            "r4 must be live — read on both paths");
    }

    #[test]
    fn liveness_cycle_handled() {
        // Branch-to-self halt:
        //   B .   ; cycle, no reads
        let stream = [0xEAFF_FFFEu32];
        let live = live_at_with_reader(0, 16, &|pc| stream.get((pc / 4) as usize).copied());
        assert_eq!(live, 0, "b . should yield no reads");
    }

    #[test]
    fn liveness_conditional_split_unions_paths() {
        // Stream layout:
        //   pc=0: BNE +1        ; cond → target=PC+8+(1<<2)=0x10
        //   pc=4: MOV r0, #0    ; fall-through writes r0
        //   pc=8: BX LR         ; fall-through return
        //   pc=12: nop
        //   pc=16 (target): MOV r1, #0  ; taken-path writes r1
        //   pc=20: BX LR        ; taken-path return
        // Both paths return without reading r12 → r12 is dead.
        // Fall-through path writes r0 before BX LR → r0 dead on that path.
        // Taken path doesn't write r0 → BX LR reads r0 (return value live) → r0 live on that path.
        // Union: r0 live (live on at least one path).
        let stream = [
            0x1A00_0001u32, // BNE +1 (target = 0+8+4 = 0xC) — let's recompute
            // Actually with imm24=1, target=pc+8+(1<<2)=0+8+4=0xC. So target=0xC.
            // Adjust stream layout:
            0xE3A0_0000u32, // MOV r0, #0 (pc=4)
            0xE12F_FF1Eu32, // BX LR (pc=8)
            0xE3A0_1000u32, // MOV r1, #0 (pc=0xC, target)
            0xE12F_FF1Eu32, // BX LR (pc=0x10)
        ];
        let live = live_at_with_reader(0, 16, &|pc| stream.get((pc / 4) as usize).copied());
        // r12 should be dead on both paths (neither writes nor reads it).
        assert_eq!(live & (1u16 << 12), 0, "r12 dead on both paths");
        // r1 dead on taken path (it's the path's own write target),
        // dead on fall-through (not used). Either way dead.
        // r0 dead on fall-through (written before return), live on taken
        // path (taken path's BX LR reads return-value-live r0). Union: live.
        assert_ne!(live & (1u16 << 0), 0, "r0 live (taken path needs it)");
    }

    #[test]
    fn pick_scratch_at_rom_0x257080_does_not_pick_r0() {
        // ROM 0x00257080: ldrb r1, [r4, #160] — the LDRB iter-25
        // suspected of clobbering r0 via the shadow-stub. The
        // surrounding basic block (from rom.dis) is:
        //   0x257080: ldrb r1, [r4, #160]      <- the access
        //   0x257084: teq  r1, sl
        //   0x257088: bne  0x2570c0            <- cond, target writes r0 first
        //   0x25708c: add  r1, r0, #1          <- READS r0 on fall-through
        //   0x257090: str  r1, [r4, #156]
        //   0x257094: add  r0, r0, r4          <- READS r0
        //   0x257098: strb r6, [r0, #161]      <- READS r0
        //   0x25709c: ldr  r0, [r4, #156]      <- writes r0
        //   ... then bl WriteRun, returns
        //   0x2570c0: mov  r0, r4              <- BNE target writes r0
        //   0x2570c4: bl   WriteRun
        // Fall-through path reads r0 → r0 LIVE at 0x257084. Picker must
        // pick R12 + R2 (both dead by APCS_RETURN_LIVE) and leave R0
        // alone. If this test fails, the shadow-stub IS clobbering r0.
        //
        // Stream is laid out so PC=0 corresponds to ROM 0x257080.
        // BNE imm24=10: target = pc+8+(10<<2) = 8+8+40 = 0x38 → stream[14].
        let stream = [
            0xE5D4_10A0u32, // [0]  pc=0x00 LDRB r1, [r4, #160]
            0xE131_000Au32, // [1]  pc=0x04 TEQ  r1, sl
            0x1A00_000Au32, // [2]  pc=0x08 BNE  +10 (target = 0x38)
            0xE280_1001u32, // [3]  pc=0x0c ADD  r1, r0, #1
            0xE584_109Cu32, // [4]  pc=0x10 STR  r1, [r4, #156]
            0xE080_0004u32, // [5]  pc=0x14 ADD  r0, r0, r4
            0xE5C0_60A1u32, // [6]  pc=0x18 STRB r6, [r0, #161]
            0xE594_009Cu32, // [7]  pc=0x1c LDR  r0, [r4, #156]
            0xE12F_FF1Eu32, // [8]  pc=0x20 BX   LR    (terminate fall-through)
            0xE12F_FF1Eu32, // [9]  pc=0x24 (unused)
            0xE12F_FF1Eu32, // [10] pc=0x28 (unused)
            0xE12F_FF1Eu32, // [11] pc=0x2c (unused)
            0xE12F_FF1Eu32, // [12] pc=0x30 (unused)
            0xE12F_FF1Eu32, // [13] pc=0x34 (unused)
            0xE1A0_0004u32, // [14] pc=0x38 MOV  r0, r4   (BNE target)
            0xE12F_FF1Eu32, // [15] pc=0x3c BX   LR
        ];
        let read = |pc: u32| stream.get((pc / 4) as usize).copied();
        let d = decode(stream[0]).expect("decode LDRB");
        assert_eq!(d.kind, AccessKind::Ldrb);
        assert_eq!(d.rt, 1);
        assert_eq!(d.rn, 4);

        let live = live_at_with_reader(4, 32, &read);
        assert_ne!(live & (1u16 << 0), 0,
            "r0 must be live at 0x257084 — fall-through reads it via `add r1, r0, #1`; live={:#x}", live);

        let picks = pick_scratch_regs_with_reader(&d, 0, &read)
            .expect("picker should find 2 dead regs");
        let (sea, sfl) = picks;
        assert_ne!(sea, 0, "scratch_ea must not be r0; got {}", sea);
        if let Some(s) = sfl {
            assert_ne!(s, 0, "scratch_flags must not be r0; got {}", s);
        }
        // The deterministic answer (CANDIDATES order [12,0,1,2,3,14],
        // operand_mask=R1|R4, live includes R0 and APCS_RETURN_LIVE):
        // first pick R12 (dead), skip R0 (live), skip R1 (operand),
        // pick R2 (dead). Lock that in so future regressions surface.
        assert_eq!(sea, 12, "scratch_ea expected R12; got {}", sea);
        assert_eq!(sfl, Some(2), "scratch_flags expected R2; got {:?}", sfl);
    }

    /// Iter-49 regression: FindSuperceeder body at ROM 0x001488ac.
    /// Body uses IP (R12) as a save register across the byte-access stub:
    ///
    ///   0x1488a8: mov ip, r1            ; save TFlashStore* in ip
    ///   0x1488ac: ldrb r1, [r1, #61]    ← byte access (patched to UDF)
    ///   0x1488b0: teq r1, #0
    ///   0x1488b4: moveq r2, #13
    ///   0x1488b8: movne r2, #6
    ///   0x1488bc: ldr r0, [r0]
    ///   0x1488c0: bic r1, r0, #0xf0000000
    ///   0x1488c4: mov r0, ip            ← READ of R12!
    ///   0x1488c8: b Lookup_thunk
    ///
    /// The picker MUST detect R12 as live at PC=0x1488b0 (= orig_pc+4),
    /// because the local read at 0x1488c4 consumes the value of ip set
    /// at 0x1488a8 (before the byte access).
    ///
    /// In production, this case fires a real wedge: the picker chose R12
    /// as scratch_ea, the stub's `ADD R12, R1, #61` clobbered ip with
    /// `TFlashStore* + 0x3d` (XOR'd to 0x3e), and the subsequent
    /// `mov r0, ip` at 0x1488c4 fed Lookup a wild this-pointer.
    ///
    /// Stream layout: PC=0 corresponds to the byte-access site (= 0x1488ac).
    #[test]
    fn pick_scratch_at_findsuperceeder_does_not_pick_r12() {
        // Body sequence in pristine ROM (no rom_patches HVCs).
        let stream = [
            0xE5D1_103Du32, // [0]  pc=0x00  ldrb r1, [r1, #61]    ← byte access
            0xE331_0000u32, // [1]  pc=0x04  teq r1, #0
            0x03A0_200Du32, // [2]  pc=0x08  moveq r2, #13
            0x13A0_2006u32, // [3]  pc=0x0c  movne r2, #6
            0xE590_0000u32, // [4]  pc=0x10  ldr r0, [r0]
            0xE3C0_120Fu32, // [5]  pc=0x14  bic r1, r0, #0xf0000000
            0xE1A0_000Cu32, // [6]  pc=0x18  mov r0, ip   ← READS R12
            0xE12F_FF1Eu32, // [7]  pc=0x1c  bx lr (stand-in for tail-call)
        ];
        let read = |pc: u32| stream.get((pc / 4) as usize).copied();
        let d = decode(stream[0]).expect("decode LDRB");
        assert_eq!(d.kind, AccessKind::Ldrb);
        assert_eq!(d.rt, 1);
        assert_eq!(d.rn, 1);

        let live = live_at_with_reader(4, 32, &read);
        assert_ne!(live & (1u16 << 12), 0,
            "R12 must be live at orig_pc+4 — `mov r0, ip` at PC=0x18 reads it; live={:#x}", live);

        let picks = pick_scratch_regs_with_reader(&d, 0, &read)
            .expect("picker should find dead reg(s)");
        let (sea, sfl) = picks;
        assert_ne!(sea, 12, "scratch_ea must NOT be R12 — body reads ip; got {}", sea);
        if let Some(s) = sfl {
            assert_ne!(s, 12, "scratch_flags must NOT be R12; got {}", s);
        }
    }

    /// Iter-49 regression (production reality): same body as above, but
    /// with PC=0x1488c4 (the `mov r0, ip` site) replaced by an HVC —
    /// because rom_patches installs the FINDSUPER_MID probe there at boot.
    ///
    /// In the production install order (rom_patches BEFORE shadow_stub),
    /// shadow_stub's analyzer reads ROM and sees HVC at PC=0x1488c4
    /// instead of `mov r0, ip`. HVC is treated as BLink (function call):
    /// the analyzer marks R0..R3 + R12 + R14 as caller-saved-clobbered,
    /// missing the LOCAL read of R12 from the original instruction.
    /// Result: picker picks R12 → stub clobbers ip → wild this to Lookup.
    ///
    /// This test reproduces the production bug. Currently EXPECTED TO
    /// FAIL — it documents the bug and will turn green once iter-50 fixes
    /// the install order (or makes the analyzer original-ROM-aware).
    #[test]
    #[ignore = "documents iter-49 bug; will pass once iter-50 fixes install order"]
    fn pick_scratch_at_findsuperceeder_when_midprobe_installed_does_not_pick_r12() {
        // hvc_insn for FINDSUPER_MID_PROBE_HVC_IMM=0x6E:
        //   cond=AL (0xE), op=0001_0100, imm12=0x006, op2=0111, imm4=0xE
        //   = 0xE140_067E
        let hvc_06e = 0xE140_067Eu32;
        let stream = [
            0xE5D1_103Du32, // [0]  pc=0x00  ldrb r1, [r1, #61]
            0xE331_0000u32, // [1]  pc=0x04  teq r1, #0
            0x03A0_200Du32, // [2]  pc=0x08  moveq r2, #13
            0x13A0_2006u32, // [3]  pc=0x0c  movne r2, #6
            0xE590_0000u32, // [4]  pc=0x10  ldr r0, [r0]
            0xE3C0_120Fu32, // [5]  pc=0x14  bic r1, r0, #0xf0000000
            hvc_06e,        // [6]  pc=0x18  ★ HVC (probe replaces `mov r0, ip`)
            0xE12F_FF1Eu32, // [7]  pc=0x1c  bx lr
        ];
        let read = |pc: u32| stream.get((pc / 4) as usize).copied();
        let d = decode(stream[0]).expect("decode LDRB");

        let picks = pick_scratch_regs_with_reader(&d, 0, &read)
            .expect("picker should find dead reg(s)");
        let (sea, sfl) = picks;
        // The original `mov r0, ip` at PC=0x18 read R12. After rom_patches
        // replaces it with HVC, that read is invisible to shadow_stub's
        // ROM-byte-driven analyzer. The bug is that the analyzer happily
        // picks R12 as scratch.
        assert_ne!(sea, 12,
            "scratch_ea must NOT be R12 even when PC=0x18 is HVC — the
             original `mov r0, ip` at that PC reads ip; got {}", sea);
        if let Some(s) = sfl {
            assert_ne!(s, 12, "scratch_flags must NOT be R12; got {}", s);
        }
    }

    /// Iter-41 regression: a function uses LR (R14) as a save register
    /// across a byte-access stub. The byte access at PC=0 is followed
    /// by a use of LR, then a tail-call. The picker MUST detect R14 as
    /// live (because the local `mov r0, lr` reads it before the tail-call,
    /// AND APCS LR is observably live at function return).
    #[test]
    fn pick_scratch_with_local_lr_read_does_not_pick_r14() {
        // Synthetic but representative: byte access, then read R14 to a
        // scratch reg, then return (BX LR consumes LR).
        let stream = [
            0xE5D1_103Du32, // [0]  pc=0x00  ldrb r1, [r1, #61]
            0xE331_0000u32, // [1]  pc=0x04  teq r1, #0
            0xE1A0_400Eu32, // [2]  pc=0x08  mov r4, lr     ← READS R14
            0xE12F_FF1Eu32, // [3]  pc=0x0c  bx lr
        ];
        let read = |pc: u32| stream.get((pc / 4) as usize).copied();
        let d = decode(stream[0]).expect("decode LDRB");

        let live = live_at_with_reader(4, 32, &read);
        assert_ne!(live & (1u16 << 14), 0,
            "R14 must be live at orig_pc+4 — `mov r4, lr` at PC=0x08 reads it; live={:#x}", live);

        // Sanity: with the iter-42 R14-exclusion fix in CANDIDATES, R14
        // is never picked anyway. This test verifies the analyzer
        // correctness independently of the band-aid candidate exclusion.
    }

    #[test]
    fn pick_scratch_finds_dead_pair() {
        // Stream where R12 and R1 are dead at PC=4 (start of analysis):
        //   pc=4: MOV r12, r4
        //   pc=8: MOV r1, r4
        //   pc=12: BX LR
        // After pc=4 and pc=8, both r12 and r1 are written before any read.
        // At BX LR, APCS_RETURN_LIVE doesn't include r1 or r12.
        let stream = [
            0xDEAD_DEADu32, // pc=0 (the byte-access site, ignored by walker)
            0xE1A0_C004u32, // MOV r12, r4
            0xE1A0_1004u32, // MOV r1, r4
            0xE12F_FF1Eu32, // BX LR
        ];
        // Decoded site (placeholder — operand-exclusion uses Rt, Rn, Rm).
        // Pretend Rt=6, Rn=4 (matches the failing test's case).
        let d = Decoded {
            kind: AccessKind::Ldrb, cond: 0xE, rn: 4, rt: 6, rt2: 0,
            offset: OffsetForm::Imm { imm: 0 },
            p: true, u: true, w: false,
        };
        let live = live_at_with_reader(4, 16,
            &|pc| stream.get((pc / 4) as usize).copied());
        let candidates: &[u32] = &[12, 0, 1, 2, 3, 14];
        let operand_mask: u16 = (1u16 << d.rt) | (1u16 << d.rn);
        let mut picks: [u32; 2] = [u32::MAX; 2];
        let mut n = 0;
        for &r in candidates {
            let rmask: u16 = 1u16 << r;
            if rmask & operand_mask != 0 { continue; }
            if rmask & live != 0 { continue; }
            picks[n] = r;
            n += 1;
            if n == 2 { break; }
        }
        assert_eq!(n, 2, "should find 2 dead regs; live mask was {:#x}", live);
    }
}
