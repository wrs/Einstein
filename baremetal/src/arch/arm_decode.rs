//! Small AArch32 instruction-decode helpers shared across the emulation
//! paths (`hv::trap`, `newton::unaligned`). Single home for the ARM condition
//! truth table, the immediate-shift evaluator, and the mode-name
//! formatter so the copies can't drift.
//!
//! Banked-register slot mapping lives in `banked.rs`
//! (`ctx_slot_for_reg`); VA/PA guest accessors live in `guest_endian.rs`.

/// Evaluate an ARM condition code (`cond`, low 4 bits) against the
/// NZCV flags in `cpsr`. AL (0xE) and the "never"/unconditional 0xF
/// encoding both pass (0xF is treated defensively as always-execute;
/// the decode paths that reach here have already excluded 0xF as a
/// real condition).
pub fn arm_cond_passed(cond: u32, cpsr: u32) -> bool {
    let n = (cpsr >> 31) & 1 != 0;
    let z = (cpsr >> 30) & 1 != 0;
    let c = (cpsr >> 29) & 1 != 0;
    let v = (cpsr >> 28) & 1 != 0;
    match cond & 0xF {
        0x0 => z,               // EQ
        0x1 => !z,              // NE
        0x2 => c,               // CS / HS
        0x3 => !c,              // CC / LO
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
        _ => true,              // 0xF: defensive
    }
}

/// Apply an ARM immediate shift to `val`. `shift_type` is the 2-bit
/// `type` field (0=LSL, 1=LSR, 2=ASR, 3=ROR/RRX); `amount` is the
/// already-decoded shift amount with the immediate-form convention that
/// `amount == 0` means 32 for LSR/ASR and RRX for ROR. RRX rotates one
/// bit through carry, reading `CPSR.C` from `cpsr` — this is the
/// carry-correct behaviour the offset/writeback math depends on.
pub fn arm_shift(val: u32, shift_type: u32, amount: u32, cpsr: u32) -> u32 {
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
                // RRX: rotate right one bit through CPSR.C.
                let c = (cpsr >> 29) & 1;
                (val >> 1) | (c << 31)
            } else {
                val.rotate_right(amount & 31)
            }
        }
        _ => unreachable!(),
    }
}

/// Lower-case AArch32 mode name for the 5-bit mode field (or full CPSR;
/// only the low 5 bits are used). Used for diagnostic logging.
pub fn aarch32_mode_name(mode: u32) -> &'static str {
    match mode & 0x1F {
        0x10 => "usr",
        0x11 => "fiq",
        0x12 => "irq",
        0x13 => "svc",
        0x16 => "mon",
        0x17 => "abt",
        0x1A => "hyp",
        0x1B => "und",
        0x1F => "sys",
        _ => "???",
    }
}
