//! SA-1100 rotate-LDR emulation for unaligned LDR/STR faults.
//!
//! With `SCTLR_EL1.A` forced to 1 by the CP15 shim in trap.rs, every
//! AArch32 unaligned LDR/STR raises a stage-1 alignment fault at EL1.
//! The kernel's DABT vector at VA 0x10 is patched to a trampoline
//! that detects `DFSR.FS[3:0] == 0b0001` (the ARMv7 short-descriptor
//! code for alignment fault, unique in the FS encoding space) and
//! issues `HVC #ALIGN_TAG` directly. `handle_align_fault` decodes
//! the faulting instruction and emulates it with SA-1100 rotate-LDR
//! semantics.
//!
//! SA-1100 (BE-32 + SCTLR.U=0) unaligned LDR word semantics:
//!   aligned = addr & ~3
//!   result  = word_at(aligned) ROR ((addr & 3) * 8)
//!
//! ARMv7+/ARMv8 AArch32 with SCTLR.U=1 treats the same access as four
//! contiguous bytes (little-endian here), which yields a different
//! result for byte-offsets 1/2/3. The 717006 Newton kernel has ~1300
//! sites depending on rotate-LDR semantics across the ROM
//! (CountMatches, ResolveFault, many more) — hence the hypervisor-
//! wide emulation instead of one-by-one targeted patches.
//!
//! Register recovery at HVC entry:
//!   - R0 original  ← TPIDR_EL0     (stub saved it before clobbering)
//!   - R1 original  ← TPIDRRO_EL0   (stub saved it before clobbering)
//!   - R2..R12      ← ctx.x[2..12]  (stub didn't touch them; non-FIQ
//!                                   modes have R8..R12 ≡ R8_usr..R12_usr
//!                                   in X8..X12 per Table D1-79)
//!   - R14_abt      ← ctx.x[20]     (LR_abt per Table D1-79; = faulting_pc + 8 for ARM)
//!   - SPSR_abt     ← DABT_SAVE_PA + 8 (AArch32-native trampoline stash;
//!                                      `mrs spsr_abt` reads the same value
//!                                      on FVP but historically flaky on
//!                                      QEMU raspi3b)
//!
//! Rt / Rn / Rm uses of R13/R14/R15 are rejected for now (halt with
//! a TODO); the Newton ROM's rotate-LDR sites overwhelmingly use
//! R0-R12 for Rn/Rt/Rm, and the scope-creep of handling banked SP/LR
//! writes is not worth it until we hit one in practice.

use crate::cpu;
use crate::kprintln;
use crate::trap::TrapContext;

/// HVC handler for `ALIGN_TAG` (0x13). Called from `handle_hvc` when
/// the DABT trampoline's alignment-fault fast path fires. Emulates the
/// faulting instruction in place and overrides ELR_EL2 / SPSR_EL2 so
/// the subsequent ERET returns the guest to the pre-abt mode at
/// (faulting_pc + 4), skipping the faulted insn.
pub fn handle_align_fault(ctx: &mut TrapContext) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    const LOG_FIRST: u32 = 40;
    // iter-85: tarmac window is now bracketed by the FPE-entry probe
    // (open) and the unrecognised-UND halt (close). The first-alignment-
    // fault stop trigger from the iter-78 investigation would close
    // the window way before the FPE call we want to trace, so it's
    // disabled. Re-enable when the active investigation changes.
    let _ = n; // tarmac::emit_stop() suppressed for iter-85

    // Recover pre-abt R0 / R1 from TPIDR scratch regs.
    let orig_r0: u64;
    let orig_r1: u64;
    // SAFETY: AArch64 sysreg reads without side effects.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0",    out(reg) orig_r0,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, tpidrro_el0",  out(reg) orig_r1,
            options(nomem, nostack, preserves_flags));
    }
    // Restore the stub's clobber so non-Rt entries survive ERET to the
    // pre-abt mode. (`ctx.x[Rt]` gets overwritten by the load result
    // below if Rt ∈ {0, 1}.)
    ctx.x[0] = orig_r0;
    ctx.x[1] = orig_r1;

    // Read LR_abt and SPSR_abt. Per ARM ARM (DDI 0487) D1.21.1 Table
    // D1-79 "Base instruction set register mapping between AArch32
    // state and AArch64 state", the register mapping on AArch32 →
    // AArch64 exception entry is **by bank name, not by active mode**:
    //   x13 = SP_usr           x14 = LR_usr           x15 = SP_hyp
    //   x16 = LR_irq           x17 = SP_irq           x18 = LR_svc
    //   x19 = SP_svc           x20 = LR_abt           x21 = SP_abt
    //   x22 = LR_und           x23 = SP_und           ...
    //   x24..x28 = R8_fiq..R12_fiq, x29 = SP_fiq, x30 = LR_fiq
    // So LR_abt lives in x20 — NOT x14 (which is LR_usr). This was
    // misdiagnosed as a "QEMU banked-reg bug" more than once; see
    // `docs/QEMU_BUGS.md` for the corrected analysis.
    //
    // For SPSR_abt the named AArch64 sysreg `spsr_abt` works on FVP,
    // but QEMU raspi3b's AArch64-side banked-SPSR read actually IS
    // flaky (returns 0). Read from the DABT_SAVE area the trampoline
    // populates instead — that's AArch32-native stores, reliable on
    // both platforms.
    let lr_abt = ctx.x[20] as u32;
    let spsr_abt_save = crate::guest_mem::read_word_pa(
        crate::guest_mem::DABT_SAVE_PA + 0x08,
    ).unwrap_or(0);
    let pre_abt_cpsr = spsr_abt_save;
    let dfar: u64;
    let dfsr_esr: u64;
    // SAFETY: AArch64 sysreg reads with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, far_el1",  out(reg) dfar,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, esr_el1",  out(reg) dfsr_esr,
            options(nomem, nostack, preserves_flags));
    }
    if n <= LOG_FIRST {
        let elr_el2: u64;
        let spsr_el2: u64;
        // SAFETY: sysreg reads, no side effects.
        unsafe {
            core::arch::asm!("mrs {}, elr_el2",  out(reg) elr_el2,
                options(nomem, nostack, preserves_flags));
            core::arch::asm!("mrs {}, spsr_el2", out(reg) spsr_el2,
                options(nomem, nostack, preserves_flags));
        }
        kprintln!(
            "unaligned[{}]: ELR_EL2={:#010x} SPSR_EL2={:#010x} (HVC-entry mode={:#x})",
            n, elr_el2 as u32, spsr_el2 as u32, (spsr_el2 as u32) & 0x1F,
        );
        kprintln!(
            "unaligned[{}]: LR_abt(save)={:#010x} ctx.x[14]={:#018x} ctx.x[13]={:#018x}",
            n, lr_abt, ctx.x[14], ctx.x[13],
        );
        kprintln!(
            "unaligned[{}]: SPSR_abt(save)={:#010x} FAR={:#010x} ESR_EL1={:#010x}",
            n, pre_abt_cpsr, dfar as u32, dfsr_esr as u32,
        );
        kprintln!(
            "unaligned[{}]: orig_r0={:#010x} orig_r1={:#010x}",
            n, orig_r0 as u32, orig_r1 as u32,
        );
    }
    // Thumb-state pre-abt mode: rotate-LDR is an ARM-only idiom in
    // the Newton ROM, and our decoder is ARM-only.
    if (pre_abt_cpsr >> 5) & 1 != 0 {
        kprintln!("unaligned: TODO Thumb alignment fault (SPSR={:#010x})", pre_abt_cpsr);
        dump_state(ctx, pre_abt_cpsr);
        cpu::halt();
    }

    // ARM R14_abt = faulting_pc + 8. (For Thumb it would be +4; we've
    // already ruled Thumb out above.)
    let faulting_pc = lr_abt.wrapping_sub(8);
    let insn = read_guest_word(faulting_pc).unwrap_or(0);
    // Early-boot diagnostic: if the PC looks invalid or we can't
    // decode, log once and skip (advance to raw_r14 - 4 — the
    // "skip-faulting-insn" AArch32 return address). The guest may
    // not recover correctly since we haven't performed the load/
    // store, but this lets boot advance enough to see downstream
    // effects rather than halting on the first weird fault.
    let decoded_maybe = decode(insn);
    if faulting_pc & 3 != 0 || decoded_maybe.is_none() {
        if n <= LOG_FIRST {
            kprintln!(
                "unaligned[{}]: SKIP — PC={:#010x} insn={:#010x} (decode={}, aligned={}) FAR={:#010x}",
                n, faulting_pc, insn,
                decoded_maybe.is_some(),
                faulting_pc & 3 == 0,
                dfar as u32,
            );
        }
        // Skip the faulting instruction: advance pre-abt PC by 4
        // (ARM) and ERET to the pre-abt mode. If the PC wasn't
        // 4-aligned we just advance by 4 anyway to avoid an infinite
        // trap loop in the trampoline.
        set_return(ctx, faulting_pc.wrapping_add(4), pre_abt_cpsr);
        return;
    }
    let decoded = decoded_maybe.unwrap();

    // Sanity: an insn that alignment-faulted must have passed its
    // condition. If it didn't, we shouldn't be here — but skip insn
    // defensively rather than loop.
    if !cond_passes(decoded.cond, pre_abt_cpsr) {
        kprintln!(
            "unaligned: WARN cond fails (cond={:#x}, CPSR={:#010x}) at PC={:#x} — skipping",
            decoded.cond, pre_abt_cpsr, faulting_pc
        );
        set_return(ctx, faulting_pc.wrapping_add(4), pre_abt_cpsr);
        return;
    }

    // R15 (PC) as Rn/Rt for an LDR/STR word is legal but rare; we
    // reject to keep the emulator simple — the ROM's rotate-LDR
    // idioms use R0-R14.
    if decoded.rn == 15 || decoded.rt == 15 {
        kprintln!(
            "unaligned: TODO PC as Rn/Rt (rn={} rt={}) insn={:#010x} PC={:#x}",
            decoded.rn, decoded.rt, insn, faulting_pc
        );
        dump_state(ctx, pre_abt_cpsr);
        cpu::halt();
    }
    if let OffsetForm::Reg { rm, .. } = decoded.offset {
        if rm == 15 {
            kprintln!(
                "unaligned: TODO PC as Rm (rm={}) insn={:#010x} PC={:#x}",
                rm, insn, faulting_pc
            );
            dump_state(ctx, pre_abt_cpsr);
            cpu::halt();
        }
    }

    // Compute effective address. `read_reg` / `write_reg` map AArch32
    // register numbers to AArch64 context slots per Table D1-79.
    let pre_mode = pre_abt_cpsr & 0x1F;
    let rn_val = read_reg(ctx, decoded.rn, pre_mode);
    let offset = match decoded.offset {
        OffsetForm::Imm(imm) => imm,
        OffsetForm::Reg { rm, shift_type, shift_amount } => {
            let rm_val = read_reg(ctx, rm, pre_mode);
            apply_shift(rm_val, shift_type, shift_amount, pre_abt_cpsr)
        }
    };
    let ea_offsetted = if decoded.u {
        rn_val.wrapping_add(offset)
    } else {
        rn_val.wrapping_sub(offset)
    };
    let access_addr = if decoded.p { ea_offsetted } else { rn_val };

    let aligned = access_addr & !3;
    let rotate = (access_addr & 3) * 8;

    if decoded.load {
        // LDR: aligned word + ROR.
        let word = match read_guest_word(aligned) {
            Some(w) => w,
            None => {
                kprintln!(
                    "unaligned: cannot read aligned {:#010x} (EA={:#010x}) at PC={:#x}",
                    aligned, access_addr, faulting_pc
                );
                dump_state(ctx, pre_abt_cpsr);
                cpu::halt();
            }
        };
        let result = if rotate == 0 { word } else { word.rotate_right(rotate) };
        write_reg(ctx, decoded.rt, pre_mode, result);
    } else {
        // STR: ARMv4 unaligned STR is architecturally UNPREDICTABLE;
        // SA-1100 stores to the aligned word without rotation. Match
        // that — the ROM shouldn't actually hit this path for real.
        let val = read_reg(ctx, decoded.rt, pre_mode);
        if !write_guest_word(aligned, val) {
            kprintln!(
                "unaligned: cannot write aligned {:#010x} (EA={:#010x}) at PC={:#x}",
                aligned, access_addr, faulting_pc
            );
            dump_state(ctx, pre_abt_cpsr);
            cpu::halt();
        }
    }

    // Writeback (P=0 post-index, or P=1 W=1 pre-index-with-writeback).
    if !decoded.p || decoded.w {
        write_reg(ctx, decoded.rn, pre_mode, ea_offsetted);
    }

    // Lazy-install an in-ROM inline stub at this PC so the next
    // execution doesn't take the EL2 round-trip. Best-effort: any
    // failure path (STR, writeback, no dead scratches, RAM PC, pool
    // full) just leaves this PC paying the EL2 trap on every fire.
    crate::unaligned_inline::try_install_at(faulting_pc);

    set_return(ctx, faulting_pc.wrapping_add(4), pre_abt_cpsr);
}

/// Map an AArch32 register number (0..14) plus pre-abt mode bits to
/// the AArch64 context slot that holds that register's value, per
/// ARM ARM DDI 0487 D1.21.1 Table D1-79.  Always returns the u32
/// (w-view); upper 32 bits of x15..x30 are CONSTRAINED UNPREDICTABLE
/// on AArch32→AArch64 exception entry (Table D1-85).
///
/// FIQ-mode R8-R12 live in x24..x28 (the banked FIQ regs); for all
/// other modes they share R8_usr..R12_usr in x8..x12.
fn read_reg(ctx: &TrapContext, reg: u32, pre_mode: u32) -> u32 {
    let idx = ctx_slot_for_reg(reg, pre_mode);
    ctx.x[idx] as u32
}

fn write_reg(ctx: &mut TrapContext, reg: u32, pre_mode: u32, value: u32) {
    let idx = ctx_slot_for_reg(reg, pre_mode);
    ctx.x[idx] = value as u64;
}

fn ctx_slot_for_reg(reg: u32, pre_mode: u32) -> usize {
    // R0..R7 and R8..R12 for non-FIQ modes: direct index.
    if reg <= 7 {
        return reg as usize;
    }
    const FIQ: u32 = 0x11;
    if reg <= 12 {
        if pre_mode == FIQ {
            return (24 + (reg - 8)) as usize;  // R8_fiq..R12_fiq → x24..x28
        }
        return reg as usize;                   // R8_usr..R12_usr → x8..x12
    }
    // R13 (SP) and R14 (LR): banked per mode. Table D1-79:
    //   mode       SP_slot  LR_slot
    //   USR/SYS    x13      x14
    //   FIQ        x29      x30
    //   IRQ        x17      x16
    //   SVC        x19      x18
    //   ABT        x21      x20
    //   UND        x23      x22
    //   HYP        x15      (doesn't apply — we're EL1)
    // Fall-through: treat unknown modes as USR (defensive).
    match (reg, pre_mode) {
        (13, 0x10) | (13, 0x1F) => 13,  // SP_usr
        (14, 0x10) | (14, 0x1F) => 14,  // LR_usr
        (13, 0x11) => 29,               // SP_fiq
        (14, 0x11) => 30,               // LR_fiq
        (13, 0x12) => 17,               // SP_irq
        (14, 0x12) => 16,               // LR_irq
        (13, 0x13) => 19,               // SP_svc
        (14, 0x13) => 18,               // LR_svc
        (13, 0x17) => 21,               // SP_abt
        (14, 0x17) => 20,               // LR_abt
        (13, 0x1B) => 23,               // SP_und
        (14, 0x1B) => 22,               // LR_und
        _ => reg as usize,              // fall-through (unknown mode → USR bank)
    }
}

fn set_return(ctx: &mut TrapContext, next_pc: u32, _pre_abt_cpsr: u32) {
    // Avoid `msr spsr_el2, x` from EL2: per docs/QEMU_BUGS.md Bug #1
    // that write leaks into AArch32 SPSR_svc (banked_spsr[1]) on QEMU
    // raspi3b. If the alignment fault fires while the guest is mid-SVC
    // handler (which happens on ~every UstrlenPrivate call — the ROM's
    // unaligned LDRs over UTF-16 strings), the leak corrupts SPSR_svc
    // to the SVC pre-fault CPSR. The eventual `movs pc, lr` at the SVC
    // handler tail then restores CPSR=SVC instead of CPSR=USR, and the
    // post-svc `mov pc, lr` at GenericSWI 0x3ae1bc self-loops in SVC
    // mode (LR_svc=0x3ae1bc). Confirmed against a probe that read
    // SPSR_EL1 before/after the `msr spsr_el2` and saw the leak land
    // there exactly as Bug #1 predicts.
    //
    // Workaround: ERET into the existing UND_RETURN_STUB while leaving
    // SPSR_EL2 unchanged from its HVC-entry auto-saved value (= ABT
    // mode CPSR, since the alignment-DABT trampoline HVCs from ABT).
    // The stub's `ldr lr, [pc, #0]; movs pc, lr` is mode-agnostic — it
    // transitions architecturally via whichever banked SPSR is in
    // scope (here SPSR_abt, untouched by any EL2 code, so it still
    // holds the pre-fault CPSR set by hardware on DABT entry).
    //
    // `_pre_abt_cpsr` equals SPSR_abt by construction, so the stub
    // doesn't need it — kept on the signature for caller clarity.
    crate::trap::return_to_guest_from_und(ctx, next_pc as u64, 0);
}

fn dump_state(ctx: &TrapContext, pre_abt_cpsr: u32) {
    kprintln!(
        "  pre-abt CPSR = {:#010x}  mode={:#x}",
        pre_abt_cpsr, pre_abt_cpsr & 0x1F
    );
    kprintln!(
        "  r0..r7:   {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x}",
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32,
    );
    kprintln!(
        "  r8..r14:  {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x}",
        ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32,
        ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32,
    );
}

pub(crate) struct Decoded {
    pub load: bool,   // true = LDR, false = STR
    pub cond: u32,
    pub rn: u32,
    pub rt: u32,
    pub offset: OffsetForm,
    pub p: bool,      // pre-index
    pub u: bool,      // add
    pub w: bool,      // writeback
}

pub(crate) enum OffsetForm {
    Imm(u32),
    Reg { rm: u32, shift_type: u32, shift_amount: u32 },
}

pub(crate) fn decode(insn: u32) -> Option<Decoded> {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        return None;
    }

    // LDR/STR (immediate, A1): cond 010 P U 0 W L Rn Rt imm12   (B=0 for word)
    if (insn & 0x0E00_0000) == 0x0400_0000 && (insn & (1 << 22)) == 0 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        return Some(Decoded {
            load: l,
            cond,
            rn: (insn >> 16) & 0xF,
            rt: (insn >> 12) & 0xF,
            offset: OffsetForm::Imm(insn & 0xFFF),
            p, u, w,
        });
    }
    // LDR/STR (register, A1): cond 011 P U 0 W L Rn Rt imm5 type 0 Rm
    if (insn & 0x0E00_0010) == 0x0600_0000 && (insn & (1 << 22)) == 0 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let l = (insn >> 20) & 1 != 0;
        return Some(Decoded {
            load: l,
            cond,
            rn: (insn >> 16) & 0xF,
            rt: (insn >> 12) & 0xF,
            offset: OffsetForm::Reg {
                rm: insn & 0xF,
                shift_type: (insn >> 5) & 0x3,
                shift_amount: (insn >> 7) & 0x1F,
            },
            p, u, w,
        });
    }
    None
}

fn cond_passes(cond: u32, cpsr: u32) -> bool {
    let n = (cpsr >> 31) & 1 != 0;
    let z = (cpsr >> 30) & 1 != 0;
    let c = (cpsr >> 29) & 1 != 0;
    let v = (cpsr >> 28) & 1 != 0;
    match cond & 0xF {
        0x0 => z,
        0x1 => !z,
        0x2 => c,
        0x3 => !c,
        0x4 => n,
        0x5 => !n,
        0x6 => v,
        0x7 => !v,
        0x8 => c && !z,
        0x9 => !c || z,
        0xA => n == v,
        0xB => n != v,
        0xC => !z && (n == v),
        0xD => z || (n != v),
        0xE => true,
        _ => true,
    }
}

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
                let c = (cpsr >> 29) & 1;
                (val >> 1) | (c << 31)
            } else {
                val.rotate_right(amount & 31)
            }
        }
        _ => unreachable!(),
    }
}

fn read_guest_word(addr: u32) -> Option<u32> {
    // VA when stage-1 MMU is on; treat as PA otherwise.
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read is side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    if sctlr & 1 != 0 {
        crate::guest_mem::read_word_va(addr)
    } else {
        crate::guest_mem::read_word_pa(addr)
    }
}

fn write_guest_word(addr: u32, value: u32) -> bool {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read is side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    if sctlr & 1 != 0 {
        crate::guest_mem::write_word_va(addr, value)
    } else {
        crate::guest_mem::write_word_pa(addr, value)
    }
}
