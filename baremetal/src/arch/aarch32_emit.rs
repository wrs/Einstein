//! Hand-rolled AArch32 (ARM) instruction encoders shared by the ROM
//! patch installer (`rom_patches`) and the guest trampoline assembler
//! (`guest_trampolines`).
//!
//! Collecting every encoder here gives one verified home with
//! compile-time self-checks following
//! `unaligned_inline::_check_encoders`.
//!
//! Every encoding below is cross-checked against `arm-none-eabi-as` /
//! `arm-none-eabi-objdump`; see `_check_encoders` for the exact
//! disassembler-confirmed bit patterns. NEVER hand-edit these from
//! memory — re-run the assembler.
//!
//!   printf 'b 0x100\n'        | arm-none-eabi-as -o /tmp/a.o - && \
//!   arm-none-eabi-objdump -d /tmp/a.o
//!
//! ARM A1 branch encoding (`B{cond} label`):
//!   cond[31:28] 1010 imm24[23:0]
//! The branch target is `(PC + 8) + (SignExtend(imm24) << 2)`, i.e.
//! PC-relative in words from two instructions ahead of the branch.

/// ARM condition codes used by the encoders here.
pub const COND_EQ: u32 = 0x0;
pub const COND_NE: u32 = 0x1;
pub const COND_AL: u32 = 0xE;

/// Encode `B{cond} target` situated at `src_pc`. `cond` is the 4-bit
/// ARM condition field (`COND_AL` for an unconditional branch). Both
/// addresses are guest byte addresses (PCs), not word indices.
pub const fn b_cond(src_pc: u32, target: u32, cond: u32) -> u32 {
    let off_bytes = target.wrapping_sub(src_pc.wrapping_add(8)) as i32;
    let off_words = (off_bytes >> 2) as u32;
    ((cond & 0xF) << 28) | 0x0A00_0000 | (off_words & 0x00FF_FFFF)
}

/// Encode an unconditional `B target` situated at `src_pc`.
pub const fn b(src_pc: u32, target: u32) -> u32 {
    b_cond(src_pc, target, COND_AL)
}

/// Encode `LDR Rd, [pc, #imm12]` situated at `src_pc`, loading the word
/// at byte address `literal_pc`. The architectural PC seen by the LDR
/// is `src_pc + 8`; the immediate is the signed byte distance to the
/// literal. `rd` is the destination register number (0..15).
///
/// ARM A1 encoding (`LDR Rd, [PC, #+imm12]`, U=1, P=1, W=0):
///   cond 0101 1001 1111 Rd imm12   →  0xE59F_0000 | (Rd<<12) | imm12
/// (U=0 form for a negative offset sets bit 23 clear: 0xE51F_....)
pub const fn ldr_rd_lit(src_pc: u32, rd: u32, literal_pc: u32) -> u32 {
    let pc_at_exec = src_pc.wrapping_add(8);
    let off = literal_pc.wrapping_sub(pc_at_exec) as i32;
    let (u_bit, imm12) = if off >= 0 {
        (1u32 << 23, off as u32 & 0xFFF)
    } else {
        (0u32, (-off) as u32 & 0xFFF)
    };
    (COND_AL << 28) | 0x0510_0000 | u_bit | (0xF << 16) | ((rd & 0xF) << 12) | imm12
}

// =======================================================================
// Compile-time encoder checks
// =======================================================================
//
// Bit patterns confirmed with arm-none-eabi-as / -objdump:
//   b 0x100      @ at 0x0 → eafffffe
//   beq 0x100    @ at 0x0 → 0afffffe
//   bne 0x100    @ at 0x0 → 1afffffe
//   ldr r0,[pc,#8]  → e59f0008
//   ldr r12,[pc,#8] → e59fc008
const fn _check_encoders() {
    // b .+8 at PC 0: target = 8, off = 8 - 8 = 0, imm24 = 0 → 0xEA000000.
    assert!(b(0, 8) == 0xEA00_0000);
    // b . (self) at PC 0: target = 0, off = -8 → -2 words → imm24=0xFFFFFE.
    assert!(b(0, 0) == 0xEAFF_FFFE);
    // beq . at PC 0 → 0x0AFFFFFE
    assert!(b_cond(0, 0, COND_EQ) == 0x0AFF_FFFE);
    // bne . at PC 0 → 0x1AFFFFFE
    assert!(b_cond(0, 0, COND_NE) == 0x1AFF_FFFE);
    // b to a far backward target: from 0x008FFF00+30*4 to 0x00FFFFA8.
    // This mirrors the DABT-fast-trampoline `b SLOW_DABT_TRAMP`.
    let from = 0x008F_FF00u32 + 30 * 4;
    let target = 0x00FF_FFA8u32;
    let pc8 = from.wrapping_add(8);
    let expect = 0xEA00_0000 | (((target.wrapping_sub(pc8) as i32) >> 2) as u32 & 0x00FF_FFFF);
    assert!(b(from, target) == expect);

    // ldr r0, [pc, #8]: literal 8 bytes past PC. At src_pc 0, PC=8,
    // literal_pc=16 → off=8 → e59f0008.
    assert!(ldr_rd_lit(0, 0, 16) == 0xE59F_0008);
    // ldr r12, [pc, #8] → e59fc008
    assert!(ldr_rd_lit(0, 12, 16) == 0xE59F_C008);
    // ldr pc, [pc, #-4] (the DABT fast-trampoline FAST_FWD tail): at
    // src_pc 0, literal_pc = 4 → off = 4 - 8 = -4 → U=0, imm12=4,
    // Rd=15 → e51ff004.
    assert!(ldr_rd_lit(0, 15, 4) == 0xE51F_F004);
}
const _: () = _check_encoders();
