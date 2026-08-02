//! AArch32 ↔ AArch64 banked-register mapping (ARM DDI 0487 D1.21.1
//! Table D1-79, "Mapping of the general-purpose registers between the
//! Execution states").
//!
//! At any AArch32→AArch64 exception entry, the AArch64 GPR file aliases
//! AArch32 banked registers **by bank name, not by the source mode**:
//!
//! ```text
//!   R0..R7            ↔ X0..X7    (always shared)
//!   R8_usr..R12_usr   ↔ X8..X12   (for non-FIQ source modes these are R8..R12)
//!   SP_usr            ↔ X13       (NOT the source mode's SP)
//!   LR_usr            ↔ X14       (NOT the source mode's LR)
//!   SP_hyp            ↔ X15
//!   LR_irq            ↔ X16       SP_irq ↔ X17
//!   LR_svc            ↔ X18       SP_svc ↔ X19
//!   LR_abt            ↔ X20       SP_abt ↔ X21
//!   LR_und            ↔ X22       SP_und ↔ X23
//!   R8_fiq..R12_fiq   ↔ X24..X28
//!   SP_fiq            ↔ X29       LR_fiq ↔ X30
//! ```
//!
//! There is **no** AArch64 named sysreg path to AArch32 banked R13/R14:
//! `MRS (Banked register)` is A32/T32-only, and `SP_EL0 / SP_EL1 /
//! ELR_EL1` are AArch64-only registers with no architectural alias to
//! any AArch32 R13_<mode> / R14_<mode>. The only access from AArch64
//! EL2 is the X-register mapping above, captured by `vectors.s` into
//! `TrapContext`.
//!
//! Per ARM ARM D1.21.2 Table D1-85, the upper 32 bits of `X16..X30`
//! on AArch32→AArch64 exception entry are CONSTRAINED UNPREDICTABLE,
//! so all reads here truncate to `u32`.

use crate::arch::trap_context::TrapContext;

/// AArch32 mode field values (low 5 bits of CPSR / SPSR_EL2).
pub const MODE_USR: u32 = 0x10;
pub const MODE_FIQ: u32 = 0x11;
pub const MODE_IRQ: u32 = 0x12;
pub const MODE_SVC: u32 = 0x13;
pub const MODE_ABT: u32 = 0x17;
pub const MODE_UND: u32 = 0x1B;
pub const MODE_SYS: u32 = 0x1F;

/// AArch64 GPR slot holding `R13_<mode>` for the AArch32 mode bits in
/// the low 5 bits of `cpsr`. Defaults to the USR slot for unrecognised
/// modes (defensive — only HYP/MON would land here, neither reachable
/// from EL1 AArch32 in our config).
pub fn sp_slot_for_mode(cpsr: u32) -> usize {
    match cpsr & 0x1F {
        MODE_USR | MODE_SYS => 13, // SP_usr
        MODE_FIQ           => 29, // SP_fiq
        MODE_IRQ           => 17, // SP_irq
        MODE_SVC           => 19, // SP_svc
        MODE_ABT           => 21, // SP_abt
        MODE_UND           => 23, // SP_und
        _                  => 13, // fall-through
    }
}

/// AArch64 GPR slot holding `R14_<mode>`. Same conventions as
/// `sp_slot_for_mode`.
pub fn lr_slot_for_mode(cpsr: u32) -> usize {
    match cpsr & 0x1F {
        MODE_USR | MODE_SYS => 14, // LR_usr
        MODE_FIQ           => 30, // LR_fiq
        MODE_IRQ           => 16, // LR_irq
        MODE_SVC           => 18, // LR_svc
        MODE_ABT           => 20, // LR_abt
        MODE_UND           => 22, // LR_und
        _                  => 14, // fall-through
    }
}

/// Read R13 (SP) of the AArch32 mode encoded in `cpsr`.
pub fn sp_for_mode(ctx: &TrapContext, cpsr: u32) -> u32 {
    ctx.x[sp_slot_for_mode(cpsr)] as u32
}

/// Read R14 (LR) of the AArch32 mode encoded in `cpsr`.
pub fn lr_for_mode(ctx: &TrapContext, cpsr: u32) -> u32 {
    ctx.x[lr_slot_for_mode(cpsr)] as u32
}

/// Map an AArch32 register number (0..14) plus AArch32 mode bits to
/// the AArch64 context slot per Table D1-79. R15 (PC) is not in the
/// X file; callers must handle PC reads separately.
///
/// FIQ-mode R8..R12 live in X24..X28 (FIQ-banked); for all other modes
/// they alias R8_usr..R12_usr in X8..X12.
pub fn ctx_slot_for_reg(reg: u32, cpsr: u32) -> usize {
    if reg <= 7 {
        return reg as usize;
    }
    let mode = cpsr & 0x1F;
    if reg <= 12 {
        if mode == MODE_FIQ {
            return (24 + (reg - 8)) as usize;
        }
        return reg as usize;
    }
    match reg {
        13 => sp_slot_for_mode(cpsr),
        14 => lr_slot_for_mode(cpsr),
        _ => reg as usize, // R15 / out-of-range — caller's problem
    }
}
