//! EL2 synchronous trap dispatcher.
//!
//! The vector at offset 0x600 (lower-EL AArch32 sync) saves the full x0..x30
//! context, hands us a `*mut TrapContext`, and we dispatch based on ESR_EL2.EC.
//!
//! Handlers that emulate a guest instruction and want to resume mutate the
//! context in place, advance ELR_EL2 past the faulting instruction, then
//! return — the vector trailer restores the context and ERETs. Handlers that
//! don't want to resume never return (they call `cpu::halt`).

use crate::{cpu, guest_mem, kprintln, mmio, peripherals::{native_primitives, vic}, timer};

macro_rules! read_sysreg {
    ($reg:literal) => {{
        let v: u64;
        // SAFETY: reading a sysreg has no side effects.
        unsafe {
            core::arch::asm!(
                concat!("mrs {}, ", $reg),
                out(reg) v,
                options(nomem, nostack, preserves_flags),
            );
        }
        v
    }};
}

/// Mirror of the AArch64 GPR layout saved by `vectors.s::save_context`.
/// Index `i` is register `xi` (with `x31` intentionally absent — that slot
/// would be SP).
#[repr(C)]
pub struct TrapContext {
    pub x: [u64; 31],
}

const EC_UNKNOWN: u32 = 0x00;
// EC=0x03: trapped MCR/MRC access to CP15 with opc1==0 (and some other
// combinations). This is what we see when HCR_EL2.TVM/TRVM/TIDCP steer a
// guest CP15 access to EL2 instead of letting it go through on real CP15.
const EC_TRAPPED_CP15: u32 = 0x03;
const EC_FP_SIMD: u32 = 0x07;
const EC_HVC_A32: u32 = 0x12;
const EC_INSN_ABORT_LOWER: u32 = 0x20;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

/// HVC immediate used by the guest's UND-vector trampoline at VA 0x04.
/// ARMv7 AArch32 has no HCR_EL2 bit that traps UND directly to EL2, so
/// we install a one-word `HVC #UND_TAG` at the UND vector and decode
/// the faulting instruction ourselves in `handle_und`.
pub const UND_TAG: u32 = 0x10;

/// Generic "inspect-then-halt" HVC immediate, used by temporary
/// vector-intercept patches during Phase B debugging. When we need to
/// see the CPU state at the moment of an abort we don't otherwise see
/// from EL2 (stage-1 aborts handled entirely by the guest), we patch
/// the relevant guest-mode vector to `HVC #DIAG_TAG`. The handler
/// dumps registers / banked SPSR / FAR and walks the guest stage-1
/// table for the faulting VA, then halts. Remove the patch once the
/// root cause is identified.
pub const DIAG_TAG: u32 = 0x11;

/// Second-stage diagnostic HVC used to read AArch32 banked R14 (LR)
/// from the mode we came from. AArch64 at EL2 does plumb x14 = LR of
/// the source mode on HVC entry in principle, but QEMU raspi3b leaves
/// x14 = 0 when the source was taken into ABT via a stage-1 abort.
/// Workaround: ERET into a small AArch32 stub in guest RAM that does
/// `mov r0, lr; hvc #DIAG_LR_TAG`. At the second HVC, x0 = LR of the
/// source mode (= faulting_pc + 8 for a data abort). Installed by
/// `handle_diag`; see the stub assembly comment in that function.
pub const DIAG_LR_TAG: u32 = 0x12;

/// Synchronous exception from a lower EL running AArch32.
#[no_mangle]
pub extern "C" fn trap_sync_lower_aarch32(ctx: &mut TrapContext) {
    let esr = read_sysreg!("esr_el2");
    let ec = ((esr >> 26) & 0x3f) as u32;
    let iss = (esr & 0x01ff_ffff) as u32;

    // DIAG: log the first N sync traps' EC + ELR + ESR, no dedup, so
    // we can see the guest PC timeline in the window leading up to a
    // stall. Remove once Phase B stall is past.
    static mut TRAP_LOG_BUDGET: usize = 50;
    // SAFETY: single-threaded.
    unsafe {
        if TRAP_LOG_BUDGET > 0 {
            TRAP_LOG_BUDGET -= 1;
            let elr = read_sysreg!("elr_el2");
            kprintln!(
                "trap: EC={:#x} ({}) ELR={:#x} ESR={:#x}",
                ec, describe_ec(ec), elr, esr
            );
        }
    }

    match ec {
        EC_DATA_ABORT_LOWER => handle_data_abort(ctx, iss),
        EC_INSN_ABORT_LOWER => handle_instruction_abort(iss),
        EC_HVC_A32 => handle_hvc(ctx, iss),
        EC_TRAPPED_CP15 => handle_cp15_trap(ctx, iss),
        EC_FP_SIMD => handle_fp_simd(ctx, iss),
        EC_UNKNOWN => handle_unknown(iss),
        _ => {
            kprintln!(
                "*** Unhandled sync trap EC={:#x} ({}), ESR={:#x} ELR={:#x}",
                ec,
                describe_ec(ec),
                esr,
                read_sysreg!("elr_el2")
            );
            cpu::halt();
        }
    }

    // Guest MMIO writes to IntCtrl / FIQMask / IntClear change the
    // effective (`int_present & int_ctrl & ~fiq_mask`) pending set and
    // must be reflected into HCR_EL2.VI / VF before ERET, or a cleared
    // interrupt keeps re-firing (or an unmasked one never delivers).
    update_virq();

    // Budget-limited "progress beacon": print PC every 10k traps so we
    // can see if the guest is making forward progress or looping in one
    // place. Doesn't halt — lets boot continue.
    static mut TRAP_COUNTER: u64 = 0;
    // SAFETY: single-threaded.
    let n = unsafe { TRAP_COUNTER += 1; TRAP_COUNTER };
    if n % 10_000 == 0 {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        kprintln!(
            "beacon: {} traps, ELR={:#x} SPSR={:#x} int_present={:#x}",
            n, elr, spsr, vic::raised()
        );
    }
}

/// Asynchronous IRQ taken at EL2. On this target the only physical IRQ
/// source we wire up is CNTHP (EL2 physical timer). Any fire is a Newton
/// timer-match deadline expiring: we latch the crossed match bit(s) into
/// `vic::int_present`, rearm CNTHP_CVAL_EL2 to the next pending deadline,
/// and update HCR_EL2.VI so the guest takes a virtual IRQ on ERET.
#[no_mangle]
pub extern "C" fn trap_irq(ctx: &mut TrapContext) {
    // Diagnostic heartbeat: sample guest PC so we can see where it's
    // executing when no MMIO traps are firing. Only print when the PC
    // moves — keeps a steady-state spin from flooding the console,
    // while still flagging every distinct program-counter the guest
    // reaches between timer fires.
    static mut HB_LAST_PC: u64 = u64::MAX;
    static mut HB_PRINT_BUDGET: usize = 16;
    let elr = read_sysreg!("elr_el2");
    // SAFETY: single-threaded.
    unsafe {
        if elr != HB_LAST_PC && HB_PRINT_BUDGET > 0 {
            HB_LAST_PC = elr;
            HB_PRINT_BUDGET -= 1;
            let spsr = read_sysreg!("spsr_el2");
            let far = read_sysreg!("far_el1");
            kprintln!("timer_irq: guest ELR={:#x} SPSR={:#x} FAR_EL1={:#x}", elr, spsr, far);
        }
    }
    timer::on_irq();
    update_virq();
    // Wall-clock-paced snapshot save. Timer IRQ is a cleaner hook
    // than sync traps: it fires regardless of whether the guest is
    // making forward progress, so we keep rolling a fresh snapshot
    // into the ring even when the guest is wedged. See
    // src/snapshot.rs.
    crate::snapshot::maybe_autosave(ctx);
}

/// Set HCR_EL2.VI / VF according to whether the VIC has any enabled IRQ
/// or FIQ pending. Sampled on every trap exit.
fn update_virq() {
    let irq = vic::irq_pending();
    let fiq = vic::fiq_pending();
    let mut hcr: u64;
    // SAFETY: sysreg access at EL2.
    unsafe {
        core::arch::asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
    }
    let mut new = hcr & !((1u64 << 6) | (1u64 << 7)); // clear VF and VI
    if irq { new |= 1u64 << 7; }
    if fiq { new |= 1u64 << 6; }
    if new != hcr {
        // SAFETY: writing HCR_EL2.VI/VF toggles virtual IRQ/FIQ pending.
        unsafe {
            core::arch::asm!(
                "msr hcr_el2, {}",
                "isb",
                in(reg) new,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Generic fatal handler for vectors we don't expect to take.
#[no_mangle]
pub extern "C" fn trap_unexpected(_ctx: &mut TrapContext) -> ! {
    let esr = read_sysreg!("esr_el2");
    let elr = read_sysreg!("elr_el2");
    let spsr = read_sysreg!("spsr_el2");
    kprintln!();
    kprintln!("*** UNEXPECTED TRAP AT EL2 ***");
    kprintln!("ESR_EL2  = {:#018x}", esr);
    kprintln!(
        "  EC     = {:#x}  ({})",
        (esr >> 26) & 0x3f,
        describe_ec(((esr >> 26) & 0x3f) as u32)
    );
    kprintln!("ELR_EL2  = {:#018x}", elr);
    kprintln!("SPSR_EL2 = {:#018x}", spsr);
    cpu::halt();
}

// ----------------- individual handlers -----------------

fn handle_data_abort(ctx: &mut TrapContext, iss: u32) {
    let far = read_sysreg!("far_el2");
    let hpfar = read_sysreg!("hpfar_el2");
    let ipa = ((hpfar >> 4) << 12) | (far & 0xFFF);

    let isv = (iss >> 24) & 1;
    let wnr = ((iss >> 6) & 1) != 0;
    let sas = ((iss >> 22) & 3) as u8;
    let srt = ((iss >> 16) & 0x1F) as usize;

    if isv == 0 {
        // No decodable syndrome — typically LDM/STM or exclusive access.
        // Log enough to diagnose and halt.
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        let sctlr_el1 = read_sysreg!("sctlr_el1");
        kprintln!(
            "*** data abort ISV=0 at ELR={:#x} SPSR={:#x} IPA={:#x} FAR={:#x} iss={:#x}",
            elr, spsr, ipa, far, iss
        );
        kprintln!(
            "    SCTLR_EL1 (guest) M-bit = {} (stage-1 {})",
            sctlr_el1 & 1,
            if (sctlr_el1 & 1) != 0 { "ON" } else { "OFF" }
        );
        cpu::halt();
    }

    let elr = read_sysreg!("elr_el2");
    if wnr {
        let value = ctx.x[srt] as u32;
        mmio::write(ipa, sas, value, elr);
    } else {
        let value = mmio::read(ipa, sas, elr);
        // Sign-extension (SSE) is ignored for stub reads — everything we
        // return here is either zero or a known non-negative constant.
        ctx.x[srt] = value as u64;
    }

    // Advance past the 32-bit ARM instruction that faulted.
    advance_elr(4);
}

fn handle_instruction_abort(iss: u32) {
    let far = read_sysreg!("far_el2");
    let hpfar = read_sysreg!("hpfar_el2");
    let ipa = ((hpfar >> 4) << 12) | (far & 0xFFF);
    let elr = read_sysreg!("elr_el2");
    kprintln!();
    kprintln!("*** instruction abort from lower EL (no silent skip per Phase A) ***");
    kprintln!(
        "  ELR={:#x}  FAR_EL2={:#x}  IPA={:#x}  IFSC={:#x}",
        elr, far, ipa, iss & 0x3f
    );
    kprintln!(
        "  (guest tried to fetch an instruction at an IPA our stage-2 doesn't map."
    );
    kprintln!(
        "   Either widen the stage-2 map to cover this IPA, or figure out why the"
    );
    kprintln!(
        "   guest's PC went here — the instruction preceding this is a suspect.)"
    );
    cpu::halt();
}

fn handle_hvc(ctx: &mut TrapContext, iss: u32) {
    // Guest-test protocol — see baremetal/guest-tests/README.md.
    let imm = iss & 0xFFFF;
    let r0 = ctx.x[0] as u32;
    match imm {
        0x01 => {
            // Print one ASCII byte from r0.
            let b = r0 as u8;
            if b == b'\n' { crate::uart::write_byte(b'\r'); }
            crate::uart::write_byte(b);
        }
        0x02 => {
            kprintln!("guest-hex: {:#010x}", r0);
        }
        0x03 => {
            kprintln!();
            kprintln!("*** guest test PASSED (r0={:#x}) ***", r0);
            cpu::halt();
        }
        0x04 => {
            kprintln!();
            kprintln!("*** guest test FAILED (code={:#x}) ***", r0);
            cpu::halt();
        }
        0x05 => {
            kprintln!("guest-mark: {:#010x}", r0);
        }
        0x20 => {
            // Save snapshot — see src/snapshot.rs. The guest's x0..x14
            // at HVC entry alias the active AArch32 mode's R0..R14;
            // ELR_EL2 / SPSR_EL2 give the PC and CPSR to resume at.
            let gprs: [u64; 15] = [
                ctx.x[0],  ctx.x[1],  ctx.x[2],  ctx.x[3],
                ctx.x[4],  ctx.x[5],  ctx.x[6],  ctx.x[7],
                ctx.x[8],  ctx.x[9],  ctx.x[10], ctx.x[11],
                ctx.x[12], ctx.x[13], ctx.x[14],
            ];
            if let Err(e) = crate::snapshot::save(&gprs) {
                kprintln!("snapshot: save failed: {}", e);
            }
        }
        v if v == UND_TAG => {
            handle_und(ctx);
        }
        v if v == DIAG_TAG => {
            handle_diag(ctx);
        }
        v if v == DIAG_LR_TAG => {
            handle_diag_lr(ctx);
        }
        _ => {
            let elr = read_sysreg!("elr_el2");
            kprintln!();
            kprintln!("*** unknown HVC #{:#x} at ELR={:#x} (halting)", imm, elr);
            cpu::halt();
        }
    }
    // HVC is a 4-byte ARM instruction; advance past it on return.
}

/// Trampoline-based undefined-instruction handler at EL2.
///
/// Flow: the guest's UND vector at VA 0x04 branches to a small AArch32
/// stub (see `UND_CTX_SAVE_*` constants below). The stub runs in UND
/// mode, saves R14_und (faulting_pc + 4) and SPSR_und (pre-UND CPSR)
/// to fixed RAM slots, then issues `HVC #UND_TAG` to enter EL2. We
/// decode the faulting instruction from guest memory, emulate, then
/// override ELR_EL2 / SPSR_EL2 so ERET resumes in the original mode
/// at the correct address.
///
/// Why the RAM-save stub: reading the AArch32 banked registers
/// (LR_und / SPSR_und) from AArch64 EL2 via MRS returns 0 under QEMU
/// raspi3b — the banked state doesn't propagate into the AArch64 view
/// even though it's set correctly on the AArch32 side (verified with
/// a pure-AArch32 probe; see the commit). So the trampoline persists
/// what we need before bouncing to EL2.
///
/// Covered instructions (PLAN.md Phase A.2):
/// - SWP / SWPB (any encoding). Emulated by plain load-store on the
///   translated guest PA; no atomic primitive needed because we hold
///   DAIF.I at EL2 for the entire emulation and the guest is single-
///   core.
/// - `0xE6000010` SystemBootUND: ELR += 8 (opcode + payload slot).
/// - `0xE6000510` DebuggerUND:   ELR += 8, log the payload word.
/// - `0xE6000810` TapFileCntlUND: ELR += 8, log the payload word.
///   (Einstein's JIT uses GETCALLER()+4 for TapFileCntl; we match the
///   JIT's page-compilation step for now — Phase B revisit when the
///   ROM actually exercises filesystem UNDs.)
/// - Anything else: log opcode + PC, halt loudly.
///
/// Fixed RAM slots used by the trampoline (must match guest tests and,
/// eventually, the ROM's patch_und_vector):
///   0x04000400  — saved LR_und (faulting_pc + 4)
///   0x04000404  — saved SPSR_und (pre-UND CPSR)
pub const UND_SAVE_LR_IPA: u32 = 0x0400_0400;
pub const UND_SAVE_SPSR_IPA: u32 = 0x0400_0404;

fn handle_und(ctx: &mut TrapContext) {
    // DIAG: prove handle_und is being reached at all. Single-shot log.
    static mut UND_ENTRY_LOGGED: bool = false;
    // SAFETY: single-threaded.
    unsafe {
        if !UND_ENTRY_LOGGED {
            UND_ENTRY_LOGGED = true;
            let elr = read_sysreg!("elr_el2");
            kprintln!("und: handle_und first entry, ELR_EL2={:#x}", elr);
        }
    }

    let lr_und = match read_guest_word_pa(UND_SAVE_LR_IPA) {
        Some(v) => v,
        None => {
            kprintln!("*** handle_und: UND_SAVE_LR slot unreadable");
            cpu::halt();
        }
    };
    let spsr_und = read_guest_word_pa(UND_SAVE_SPSR_IPA).unwrap_or(0) as u64;
    let faulting_pc = lr_und.wrapping_sub(4);

    let insn = match read_guest_word_pa(faulting_pc) {
        Some(w) => w,
        None => {
            kprintln!(
                "*** handle_und: faulting PC {:#x} is outside mapped guest memory",
                faulting_pc
            );
            cpu::halt();
        }
    };

    // StrongARM CP15 clock-control write (MCR p15, 0, Rt, c15, c1, 2).
    // ARMv8 doesn't define that register, so the instruction raises UND
    // locally at EL1 rather than trapping via HCR_EL2.TIDCP — which is
    // why we handle it here and not in handle_cp15_trap. Fires exactly
    // once during 717006 boot (probe/FINDINGS.md §16.4); treat as a
    // no-op and advance past it. Mask clears Rt (bits 15:12); the
    // encoding otherwise matches MCR p15,0,Rt,c15,c1,2 (0xEE0F_0F51).
    if (insn & 0xFFFF_0FFF) == 0xEE0F_0F51 {
        log_cp15_strongarm_clock(faulting_pc);
        return_to_guest(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Einstein's JIT (TJITGenericPage.cpp) advances PC by 8 past each
    // of these three UNDs — opcode + a 4-byte payload slot. We mirror
    // that; the payload interpretation varies per UND (debugger logs
    // a string, TapFileCntl takes a command word in R0) but early-boot
    // just needs the PC advance + budgeted visibility.
    match insn {
        0xE6000010 => {
            log_und_budgeted("SystemBootUND", faulting_pc, None);
            return_to_guest(ctx, (faulting_pc + 8) as u64, spsr_und);
        }
        0xE6000510 => {
            let payload = read_guest_word_pa(faulting_pc + 4).unwrap_or(0);
            log_und_budgeted("DebuggerUND", faulting_pc, Some(payload));
            return_to_guest(ctx, (faulting_pc + 8) as u64, spsr_und);
        }
        0xE6000810 => {
            let payload = read_guest_word_pa(faulting_pc + 4).unwrap_or(0);
            log_und_budgeted("TapFileCntlUND", faulting_pc, Some(payload));
            return_to_guest(ctx, (faulting_pc + 8) as u64, spsr_und);
        }
        _ if is_swp_encoding(insn) => {
            emulate_swp(ctx, insn, faulting_pc);
            return_to_guest(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        _ => {
            kprintln!(
                "*** unrecognised UND: insn={:#010x} at PC={:#x} SPSR_und={:#x}",
                insn, faulting_pc, spsr_und
            );
            kprintln!(
                "    (extend handle_und in trap.rs to handle this opcode)"
            );
            cpu::halt();
        }
    }
}

/// Generic "inspect-then-halt" diagnostic HVC handler.
///
/// Invoked when a vector (typically 0x10 DABT or 0x0C PABT) has been
/// patched to `HVC #DIAG_TAG` during Phase B debugging. Dumps:
/// - ELR_EL2 (PC after HVC), SPSR_EL2, ESR (via caller's trap path)
/// - FAR_EL1 (original faulting VA, preserved across HVC)
/// - Banked SPSR_<mode> for all non-current exception modes
/// - Guest x0..x14 (= AArch32 R0..R14 of the mode that executed HVC,
///   where R13/R14 are banked)
/// - Guest stage-1 translation walk for FAR_EL1
///
/// Then halts loudly. Useful for any abort we don't see at EL2 because
/// the guest handles it at EL1; patching the vector and running lets
/// us catch the abort context once before the guest's own handler
/// clobbers it.
fn handle_diag(ctx: &mut TrapContext) {
    let far = read_sysreg!("far_el1");
    let spsr_el2 = read_sysreg!("spsr_el2");
    let elr_el2 = read_sysreg!("elr_el2");

    let spsr_abt = read_banked_spsr("abt");
    let spsr_und = read_banked_spsr("und");
    let spsr_irq = read_banked_spsr("irq");
    let spsr_fiq = read_banked_spsr("fiq");

    let pre_abort_mode = spsr_el2 & 0x1F;
    let mode_name = describe_aarch32_mode(pre_abort_mode as u32);

    kprintln!();
    kprintln!("*** DIAG vector intercept (HVC #DIAG_TAG from mode {}) ***", mode_name);
    kprintln!(
        "  ELR_EL2   = {:#010x}  (PC of insn after HVC)",
        elr_el2
    );
    kprintln!(
        "  SPSR_EL2  = {:#010x}  (CPSR at HVC entry)",
        spsr_el2
    );
    kprintln!(
        "  FAR_EL1   = {:#010x}  (most-recent EL1 faulting VA)",
        far
    );
    kprintln!(
        "  SPSR_abt  = {:#010x}  SPSR_und = {:#010x}  SPSR_irq = {:#010x}  SPSR_fiq = {:#010x}",
        spsr_abt, spsr_und, spsr_irq, spsr_fiq
    );
    kprintln!("  guest regs at HVC entry (x13/x14 are banked for the current mode):");
    for chunk in 0..3 {
        let base = chunk * 5;
        kprintln!(
            "    r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x} r{:<2}={:#010x}",
            base, ctx.x[base] as u32,
            base+1, ctx.x[base+1] as u32,
            base+2, ctx.x[base+2] as u32,
            base+3, ctx.x[base+3] as u32,
            base+4, ctx.x[base+4] as u32,
        );
    }
    guest_mem::dump_stage1_walk(far as u32);
    // Also walk a handful of VAs that are relevant to Newton boot —
    // SVC stack, ABT stack target, REx window start, RAM base — so we
    // can tell at a glance whether the kernel's L1 table has the
    // expected mappings in place at the time of the abort.
    for va in [0x04004400u32, 0x0C004C00, 0x01000000, 0x04000000, 0x00800000] {
        guest_mem::dump_stage1_walk(va);
    }

    // Before halting, try to recover LR / SP of the pre-abort mode
    // and the SVC banked SP/LR (the kernel runs in SVC, so its stack
    // lives there) by ERET'ing into an AArch32 stub we plant in guest
    // RAM. On entry the stub runs in whatever mode SPSR_EL2 describes
    // (ABT for a DABT intercept); it captures the current mode's LR/SP
    // and SPSR, briefly switches to SVC to capture SP_svc / LR_svc,
    // then HVCs back with everything in r0..r5.
    //
    // Stub (at guest IPA 0x04005F00, reached via VA 0x0C004F00 which
    // the kernel maps through L1[0xC0] coarse -> L2[0x04] small page):
    //   +0x00: e1a0000e   mov r0, lr       ; r0 = LR_<src_mode>
    //   +0x04: e1a0100d   mov r1, sp       ; r1 = SP_<src_mode>
    //   +0x08: e14f4000   mrs r4, spsr     ; r4 = SPSR_<src_mode>
    //   +0x0C: e321f0d3   msr cpsr_c, #0xd3 ; switch to SVC
    //   +0x10: e1a0200d   mov r2, sp       ; r2 = SP_svc
    //   +0x14: e1a0300e   mov r3, lr       ; r3 = LR_svc
    //   +0x18: e321f0d7   msr cpsr_c, #0xd7 ; switch back to ABT
    //   +0x1C: e1400172   hvc #0x12        ; DIAG_LR_TAG
    const LR_STUB_PA: u32 = 0x0400_5F00;
    const LR_STUB_VA: u32 = 0x0C00_4F00;
    let stub: [u32; 8] = [
        0xE1A0_000E, 0xE1A0_100D, 0xE14F_4000, 0xE321_F0D3,
        0xE1A0_200D, 0xE1A0_300E, 0xE321_F0D7, 0xE140_0172,
    ];
    for (i, w) in stub.iter().enumerate() {
        if !guest_mem::write_word_pa(LR_STUB_PA + (i as u32) * 4, *w) {
            kprintln!("  (stub write at +{} failed; halting)", i * 4);
            cpu::halt();
        }
    }
    kprintln!("  ERET'ing to LR/stack-trace stub at VA {:#x} ...", LR_STUB_VA);
    // SAFETY: single-use ERET. ELR_EL2 set to the stub VA, SPSR_EL2
    // kept as-is so the stub runs in the pre-abort mode. Caller is
    // halted on return from handle_diag_lr.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "isb",
            "eret",
            elr = in(reg) LR_STUB_VA as u64,
            options(noreturn),
        );
    }
}

/// Second-stage diagnostic: the stub installed by `handle_diag` runs
/// in the source AArch32 mode, captures that mode's LR/SP plus the
/// SVC banked SP/LR, then HVCs back with:
///   x0 = LR_<src_mode>  (for DABT: faulting_pc + 8, bit 0 = pre-abort T)
///   x1 = SP_<src_mode>
///   x2 = SP_svc
///   x3 = LR_svc
///   x4 = SPSR_<src_mode>  (raw AArch32 SPSR — more reliable than QEMU's
///                          AArch64 view, which returned 0 for us)
/// Prints all of that, the faulting instruction bytes, walks the
/// kernel's SVC stack (fp-chain + raw dump + return-address
/// heuristic), then halts.
fn handle_diag_lr(ctx: &mut TrapContext) -> ! {
    let lr_src   = ctx.x[0] as u32;
    let sp_src   = ctx.x[1] as u32;
    let sp_svc   = ctx.x[2] as u32;
    let lr_svc   = ctx.x[3] as u32;
    let spsr_src = ctx.x[4] as u32;
    let thumb = (lr_src & 1) != 0;
    let faulting_pc = if thumb { lr_src.wrapping_sub(4) & !1 }
                      else      { lr_src.wrapping_sub(8) };

    kprintln!();
    kprintln!("*** DIAG stage 2 (LR + stack-trace recovery) ***");
    kprintln!(
        "  LR_<src>  = {:#010x}  SPSR_<src> = {:#010x}  (T={})",
        lr_src, spsr_src, thumb as u32
    );
    kprintln!(
        "  SP_<src>  = {:#010x}  SP_svc  = {:#010x}  LR_svc = {:#010x}",
        sp_src, sp_svc, lr_svc
    );
    let mode_name = describe_aarch32_mode(spsr_src);
    kprintln!(
        "  source mode from SPSR = {:#x} ({}); T={}, I={}, F={}",
        spsr_src & 0x1F, mode_name,
        (spsr_src >> 5) & 1, (spsr_src >> 7) & 1, (spsr_src >> 6) & 1
    );
    kprintln!(
        "  faulting PC  = {:#010x}  ({})",
        faulting_pc, if thumb { "Thumb" } else { "ARM" }
    );

    // Dump the faulting instruction(s). For Thumb at an aligned addr,
    // two halfwords. For ARM, one word.
    if let Some(w) = guest_mem::read_word_pa(faulting_pc) {
        if thumb {
            kprintln!(
                "  insn halfwords @ {:#x} = {:#06x} {:#06x}",
                faulting_pc, w & 0xFFFF, (w >> 16) & 0xFFFF
            );
        } else {
            kprintln!("  insn word @ {:#x} = {:#010x}", faulting_pc, w);
        }
    }

    // Walk the SVC stack symbolically. `lr_svc` is the return
    // address of whoever is currently executing in SVC — i.e. the BL
    // that led us here. From SP_svc we scan upward (growing address)
    // looking for values that plausibly point back into the ROM's
    // executable range; each such word is a likely saved-LR. This is
    // a cheap substitute for a full fp-chain walk when fp = 0 (which
    // BootOS deliberately sets at 0x187d4).
    kprintln!("  symbolic stack trace (SVC):");
    kprintln!(
        "    #0  {:#010x}  ({})   <- faulting PC",
        faulting_pc, if thumb { "Thumb" } else { "ARM" }
    );
    kprintln!(
        "    #1  {:#010x}  ARM    <- LR_svc (caller of faulting fn)",
        lr_svc & !1
    );

    // Scan 64 words up from SP_svc; any word that looks like a return
    // address (points into ROM, word-aligned modulo Thumb bit, and
    // the preceding word is a plausible BL / BLX) is printed. The
    // word-before-BL filter cuts almost all false positives.
    let mut frame = 2usize;
    for i in 0..64u32 {
        let va = sp_svc.wrapping_add(i * 4);
        let pa_opt = guest_translate_va(va);
        if pa_opt.is_none() { continue; }
        let pa = pa_opt.unwrap();
        let w = match guest_mem::read_word_pa(pa) {
            Some(x) => x, None => continue,
        };
        // Heuristic for "plausible return address": points to ROM
        // (< 0x0100_0000 after stripping Thumb bit) and aligned.
        let tgt = w & !1;
        if tgt == 0 || tgt >= 0x0100_0000 { continue; }
        if tgt & 3 != 0 { continue; }
        // Preceding word should look like a BL (`cond_101L_...`) —
        // which means bits[27:24] = 0b101_ (any BL/B).
        if let Some(prev) = guest_mem::read_word_pa(tgt.wrapping_sub(4)) {
            let is_bl = ((prev >> 24) & 0xF) == 0xB;       // BL (unconditional)
            let is_blx_imm = ((prev >> 25) & 0x7F) == 0x7D; // BLX imm (v5+)
            if is_bl || is_blx_imm {
                kprintln!(
                    "    #{}  {:#010x}  (called via {:#010x} @ {:#x})",
                    frame, tgt, prev, tgt - 4
                );
                frame += 1;
                if frame >= 8 { break; }
            }
        }
    }
    kprintln!("  (end of trace — cross-reference PCs against _Data_/symbols.txt)");
    cpu::halt();
}

/// Translate a guest VA to its guest PA via the current stage-1
/// tables. Returns None on a fault (unmapped / wrong descriptor type).
/// Uses the same logic as `guest_mem::dump_stage1_walk` but returns
/// the PA instead of printing.
fn guest_translate_va(va: u32) -> Option<u32> {
    // Assume TTBR0 = 0x04000000 (per probe findings) and walk the
    // short-descriptor tables via guest_mem's PA accessors.
    let l1_idx = (va >> 20) as usize;
    let l1_entry = guest_mem::read_word_pa(0x0400_0000 + (l1_idx as u32) * 4)?;
    let ty = l1_entry & 3;
    match ty {
        2 => Some((l1_entry & 0xFFF0_0000) | (va & 0x000F_FFFF)),
        1 => {
            let l2_pa = l1_entry & 0xFFFF_FC00;
            let l2_idx = (va >> 12) & 0xFF;
            let l2_entry = guest_mem::read_word_pa(l2_pa + l2_idx * 4)?;
            match l2_entry & 3 {
                1 => Some((l2_entry & 0xFFFF_0000) | (va & 0x0000_FFFF)),
                2 | 3 => Some((l2_entry & 0xFFFF_F000) | (va & 0x0000_0FFF)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn read_banked_spsr(which: &'static str) -> u64 {
    // SAFETY: these are defined AArch64 system registers at EL2.
    unsafe {
        let v: u64;
        match which {
            "abt" => core::arch::asm!("mrs {}, spsr_abt", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            "und" => core::arch::asm!("mrs {}, spsr_und", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            "irq" => core::arch::asm!("mrs {}, spsr_irq", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            "fiq" => core::arch::asm!("mrs {}, spsr_fiq", out(reg) v,
                options(nomem, nostack, preserves_flags)),
            _ => { v = 0; }
        }
        v
    }
}

fn describe_aarch32_mode(mode: u32) -> &'static str {
    match mode & 0x1F {
        0x10 => "USR",
        0x11 => "FIQ",
        0x12 => "IRQ",
        0x13 => "SVC",
        0x16 => "MON",
        0x17 => "ABT",
        0x1A => "HYP",
        0x1B => "UND",
        0x1F => "SYS",
        _    => "?",
    }
}

fn is_swp_encoding(insn: u32) -> bool {
    // ARMv7 A8.8.229: SWP  cond 0001_0000 Rn Rd SBZ 1001 Rm  (word)
    //                 SWPB cond 0001_0100 Rn Rd SBZ 1001 Rm  (byte)
    // Mask zeros cond (bits 31:28), Rn (19:16), Rd (15:12), SBZ (11:8),
    // Rm (3:0). Leaves bits [27:20] + [7:4] for the opcode check.
    (insn & 0x0FB0_0FF0) == 0x0100_0090
}

/// Emulate a SWP / SWPB instruction. The CPU took UND on A53 (SCTLR.SW
/// = 0 by default on ARMv8). AArch32 R0..R12 are non-banked for the
/// USR/SYS/SVC/ABT/UND/IRQ modes the Newton kernel actually uses and
/// map directly to ctx.x[0..12], so we can read/write the operand regs
/// through the saved context.
fn emulate_swp(ctx: &mut TrapContext, insn: u32, faulting_pc: u32) {
    let is_byte = (insn & 0x0040_0000) != 0;
    let rn = ((insn >> 16) & 0xF) as usize;
    let rd = ((insn >> 12) & 0xF) as usize;
    let rm = (insn & 0xF) as usize;

    // FIQ-mode and banked-SP/LR operands would need the banked-register
    // machinery. The Newton kernel's one SWP site (probe/FINDINGS.md
    // §16.5) uses low regs, and our tests stay below r13.
    if rn >= 13 || rd >= 13 || rm >= 13 {
        kprintln!(
            "*** SWP with banked reg operand: insn={:#010x} PC={:#x} Rn=r{} Rd=r{} Rm=r{}",
            insn, faulting_pc, rn, rd, rm
        );
        cpu::halt();
    }

    let addr = ctx.x[rn] as u32;
    let new_value = ctx.x[rm] as u32;

    if is_byte {
        let old = match read_guest_byte_pa(addr) {
            Some(v) => v,
            None => {
                kprintln!("*** SWPB [r{}]={:#x} — address not writable", rn, addr);
                cpu::halt();
            }
        };
        if !write_guest_byte_pa(addr, new_value as u8) {
            kprintln!("*** SWPB [r{}]={:#x} — address not writable", rn, addr);
            cpu::halt();
        }
        ctx.x[rd] = old as u64;
    } else {
        if addr & 3 != 0 {
            kprintln!(
                "*** SWP with unaligned address r{}={:#x} (ignored, guest may fault)",
                rn, addr
            );
        }
        let old = match read_guest_word_pa(addr) {
            Some(v) => v,
            None => {
                kprintln!("*** SWP [r{}]={:#x} — address not readable", rn, addr);
                cpu::halt();
            }
        };
        if !write_guest_word_pa(addr, new_value) {
            kprintln!("*** SWP [r{}]={:#x} — address not writable", rn, addr);
            cpu::halt();
        }
        ctx.x[rd] = old as u64;
    }

    log_swp_budgeted(faulting_pc, is_byte, rn, rd, rm, addr);
}

fn return_to_guest(_ctx: &mut TrapContext, elr: u64, spsr: u64) {
    // SAFETY: writing EL2 sysregs; restore tail ERETs using these values.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "msr spsr_el2, {spsr}",
            "isb",
            elr = in(reg) elr,
            spsr = in(reg) spsr,
            options(nostack, preserves_flags),
        );
    }
}

// Guest-PA memory accessors live in guest_mem; this was an earlier
// in-module stub. Use `guest_mem::read_word_pa` etc. directly.
use guest_mem::{read_byte_pa as read_guest_byte_pa,
                read_word_pa as read_guest_word_pa,
                write_byte_pa as write_guest_byte_pa,
                write_word_pa as write_guest_word_pa};

fn log_und_budgeted(name: &str, pc: u32, payload: Option<u32>) {
    static mut UND_LOG_BUDGET: usize = 16;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if UND_LOG_BUDGET > 0 {
            UND_LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        match payload {
            Some(p) => kprintln!("und: {} @PC={:#x} payload={:#010x}", name, pc, p),
            None => kprintln!("und: {} @PC={:#x}", name, pc),
        }
    }
}

fn log_cp15_strongarm_clock(pc: u32) {
    static mut LOG_BUDGET: usize = 2;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if LOG_BUDGET > 0 {
            LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!("und: MCR p15,0,Rt,c15,c1,2 (StrongARM clock) @PC={:#x} — no-op", pc);
    }
}

fn log_swp_budgeted(pc: u32, is_byte: bool, rn: usize, rd: usize, rm: usize, addr: u32) {
    static mut SWP_LOG_BUDGET: usize = 8;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if SWP_LOG_BUDGET > 0 {
            SWP_LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!(
            "und: SWP{} @PC={:#x} r{} <- [r{}={:#x}] <- r{}",
            if is_byte { "B" } else { "" }, pc, rd, rn, addr, rm
        );
    }
}

// ISS layout for EC=0x03 (trapped MCR/MRC to CP15):
//   [19:17]  Opc2
//   [16:14]  Opc1
//   [13:10]  CRn
//   [9:5]    Rt   (guest register operand)
//   [4:1]    CRm
//   [0]      Direction: 0 = write (MCR), 1 = read (MRC)
fn handle_cp15_trap(ctx: &mut TrapContext, iss: u32) {
    let is_read = (iss & 1) != 0;
    let _crm = ((iss >> 1) & 0xF) as u32;
    let rt = ((iss >> 5) & 0x1F) as usize;
    let crn = ((iss >> 10) & 0xF) as u32;
    let opc1 = ((iss >> 14) & 0x7) as u32;
    let opc2 = ((iss >> 17) & 0x7) as u32;
    let crm = _crm;

    // Budget-limited CP15 logging for bring-up diagnostics. Prints only the
    // first N unique (CRn, CRm, Opc1, Opc2, dir) tuples.
    static mut CP15_SEEN: [u32; 32] = [0; 32];
    static mut CP15_N: usize = 0;
    let key = ((is_read as u32) << 13)
        | (crn << 9)
        | (crm << 5)
        | (opc1 << 2)
        | opc2;
    // SAFETY: single-threaded.
    let already = unsafe {
        let mut found = false;
        for i in 0..CP15_N {
            if CP15_SEEN[i] == key { found = true; break; }
        }
        if !found && CP15_N < 32 {
            CP15_SEEN[CP15_N] = key;
            CP15_N += 1;
            let value_log = if is_read { 0 } else { ctx.x[rt] as u32 };
            let elr = read_sysreg!("elr_el2");
            kprintln!(
                "cp15: {} p15,{},Rt=r{},c{},c{},{{{}}} val={:#010x} @ELR={:#x}",
                if is_read { "MRC" } else { "MCR" },
                opc1, rt, crn, crm, opc2, value_log, elr
            );
            true
        } else {
            found
        }
    };
    let _ = already;

    // Dispatch on the full (opc1, CRn, CRm, opc2, dir) tuple. The
    // surface is fixed at 15 tuples for the 717006 ROM (see
    // probe/FINDINGS.md §16.4). The load-time CP15 patcher in
    // guest_mem.rs rewrites the StrongARM lax CRm=CRn encodings for
    // CRn ∈ {1,2,3,5,6} to the ARMv7 canonical CRm=0 form before the
    // guest runs, so we only see the ARMv7 encodings here; the three
    // cache and TLB groups (CRn=7, CRn=8) and the one-off StrongARM
    // clock-control write (CRn=15, CRm=1, opc2=2) keep their native
    // encodings.
    // Writes to virtual-memory CP15 regs (SCTLR/TTBR/DACR/FSR/FAR)
    // trap via HCR_EL2.TVM. Reads of the same registers are NOT
    // trapped (we don't set TRVM): the hardware already holds the
    // right values — for SCTLR/TTBR/DACR because we synced them on
    // the trapped write, for DFSR/DFAR because the CPU writes them
    // when it takes an EL1 stage-1 abort. Guest MRC reads go straight
    // to hardware and return the real values.
    //
    // Cache-maintenance (CRn=7) and TLB invalidation (CRn=8) are not
    // covered by TVM; they trap via HCR_EL2.TIDCP / TSW.
    let tuple = (opc1, crn, crm, opc2, is_read);
    match tuple {
        // --- writes to virtual-memory CP15 regs ---
        (0, 1, 0, 0, false) => {
            let value = ctx.x[rt] as u32;
            cp15::write_sctlr_el1(value as u64);
            log_sctlr_write(value);
            if (value & 1) != 0 { maybe_dump_l1_once(); }
        }
        (0, 2, 0, 0, false) => {
            let value = ctx.x[rt] as u32;
            cp15::write_ttbr0_el1(value as u64);
            // First TTBR write locks in the guest's stage-1 table
            // location. Walk it once and normalise the XN / SBZ bits
            // before the guest turns stage-1 on.
            static mut TTBR_FIXED: bool = false;
            // SAFETY: single-threaded.
            let already = unsafe {
                let v = TTBR_FIXED;
                TTBR_FIXED = true;
                v
            };
            if !already { guest_mem::fix_stage1_xn_bits(); }
        }
        (0, 3, 0, 0, false) => cp15::write_dacr32(ctx.x[rt]),
        (0, 5, 0, 0, false) => {
            // Guest writes to DFSR — pass through to hardware so
            // subsequent guest reads see the intended value.
            cp15::write_dfsr32(ctx.x[rt]);
        }
        (0, 6, 0, 0, false) => {
            // Guest writes to DFAR — pass through to FAR_EL1.
            cp15::write_far_el1(ctx.x[rt]);
        }

        // Cache maintenance (CRn=7). Per probe/FINDINGS.md §16.7:
        //   c7, c6, op2=0  Invalidate entire data cache
        //   c7, c6, op2=1  Clean+invalidate DC line (MVA)
        //   c7, c7, op2=0  Invalidate unified cache
        //   c7, c10, op2=1 Clean DC line (MVA)
        //   c7, c10, op2=4 Drain write buffer / DSB
        // A53 handles coherency natively for our config, so a DSB is
        // the only operation we actually need to preserve ordering
        // the guest expects. The other c7 ops are no-ops.
        (0, 7, _, _, false) => cp15::cache_maintenance_barrier(),

        // TLB invalidation (CRn=8):
        //   c8, c5, op2=0  ITLB invalidate all
        //   c8, c6, op2=1  DTLB invalidate by MVA
        //   c8, c7, op2=0  TLB invalidate all
        (0, 8, _, _, false) => cp15::invalidate_tlb(),

        // StrongARM-specific clock-control write (c15, c1, op1=0, op2=2).
        // Fires exactly once at boot; no observable effect from EL2.
        (0, 15, 1, 2, false) => { /* nop */ }

        _ => {
            // Unrecognised tuple — Phase A contract: halt loudly so
            // we model it here rather than silently returning zero /
            // dropping the write. probe/FINDINGS.md §16.4 enumerates
            // the 15 tuples 717006 uses.
            halt_unknown_cp15(is_read, opc1, crn, crm, opc2, rt, ctx);
        }
    }

    advance_elr(4);
}

/// FP / SIMD access trap from a lower EL (EC=0x07), routed to EL2 by
/// CPTR_EL2.TFP. On Newton this is how native-primitive calls arrive:
/// the guest executes `MCR p10, 0, Rd, cN, cM, {opc2}` and Einstein's
/// convention is that the CPU register Rd holds the "native call code"
/// (driver ID << 8 | sub-function). We decode the faulting instruction
/// from guest memory, read the named register, and hand it to
/// peripherals::native_primitives::execute.
///
/// MRC reads from CP10/CP11 (and any other FP/SIMD shape we don't
/// expect from Newton OS) halt loudly — extend the handler when a
/// ROM boot trips one.
fn handle_fp_simd(ctx: &mut TrapContext, _iss: u32) {
    let elr = read_sysreg!("elr_el2") as u32;
    let insn = match read_guest_word_pa(elr) {
        Some(w) => w,
        None => {
            kprintln!(
                "*** fp_simd: faulting PC {:#x} unreadable from EL2 backing stores",
                elr
            );
            cpu::halt();
        }
    };

    // Decode ARMv7 MCR / MRC (load/store to coprocessor, single
    // register). Encoding: cond 1110 opc1 L CRn Rd coproc opc2 1 CRm
    // Mask for (MCR or MRC) with bit 4 = 1 and the fixed 1110 prefix
    // is (insn & 0x0F00_0010) == 0x0E00_0010.
    let is_mcr_mrc = (insn & 0x0F00_0010) == 0x0E00_0010;
    let cop = (insn >> 8) & 0xF;
    let l_bit = (insn >> 20) & 1; // 0 = MCR, 1 = MRC

    if !(is_mcr_mrc && (cop == 10 || cop == 11)) {
        kprintln!(
            "*** fp_simd trap on unexpected instruction {:#010x} @PC={:#x}, halting",
            insn, elr
        );
        cpu::halt();
    }

    let rd = ((insn >> 12) & 0xF) as usize;
    let crn = (insn >> 16) & 0xF;
    let crm = insn & 0xF;
    let opc1 = (insn >> 21) & 0x7;
    let opc2 = (insn >> 5) & 0x7;

    if l_bit != 0 {
        kprintln!(
            "*** MRC from CP{} not supported: insn={:#010x} @PC={:#x} (opc1={} Rd=r{} CRn=c{} CRm=c{} opc2={})",
            cop, insn, elr, opc1, rd, crn, crm, opc2
        );
        cpu::halt();
    }

    // Einstein's NativeCoprocRegisterTransfer reads CPU register Rd as
    // the "native call" code. ARMv4 MCR with Rd=PC reads PC+12, but
    // the Newton kernel never uses PC there; flag it if we ever see
    // one so we can match Einstein's quirk.
    if rd == 15 {
        kprintln!(
            "*** MCR p{}: Rd=PC is an Einstein quirk (mCurrentRegisters[15]+4); halting to investigate",
            cop
        );
        cpu::halt();
    }

    let native_insn = ctx.x[rd] as u32;
    native_primitives::execute(ctx, native_insn, elr);

    advance_elr(4);
}

fn log_sctlr_write(value: u32) {
    static mut SCTLR_N: usize = 0;
    // SAFETY: single-threaded.
    let n = unsafe { let v = SCTLR_N; SCTLR_N += 1; v };
    if n < 6 {
        let sctlr_now = cp15::read_sctlr_el1();
        kprintln!(
            "cp15.sctlr[{}] wrote {:#010x} (M={} V={} C={} I={})",
            n, value,
            value & 1,
            (value >> 13) & 1,
            (value >> 2) & 1,
            (value >> 12) & 1,
        );
        kprintln!("   SCTLR_EL1 after write = {:#018x}", sctlr_now);
    }
}

fn maybe_dump_l1_once() {
    static mut L1_DUMPS: usize = 0;
    // SAFETY: single-threaded.
    let n = unsafe { let v = L1_DUMPS; L1_DUMPS += 1; v };
    if n < 10 {
        guest_mem::dump_guest_l1_table();
    }
}

fn halt_unknown_cp15(is_read: bool, opc1: u32, crn: u32, crm: u32, opc2: u32, rt: usize, ctx: &TrapContext) -> ! {
    let value = if is_read { 0 } else { ctx.x[rt] as u32 };
    let elr = read_sysreg!("elr_el2");
    kprintln!();
    kprintln!("*** unhandled CP15 access halted (no silent stub per Phase A) ***");
    kprintln!(
        "  {} p15,{},Rt=r{},c{},c{},{{{}}}  val={:#010x}  @ELR={:#x}",
        if is_read { "MRC" } else { "MCR" },
        opc1, rt, crn, crm, opc2, value, elr
    );
    kprintln!(
        "  (extend handle_cp15_trap in trap.rs to service this tuple; cross-reference"
    );
    kprintln!(
        "   probe/FINDINGS.md §16.4 for the 15 tuples the 717006 ROM exercises.)"
    );
    cpu::halt();
}

// Small inline module with the raw sysreg touches, kept close to the
// dispatch above so the trap handler stays readable.
mod cp15 {
    // Only the write paths are used by the hypervisor: we intercept
    // guest MCRs to these CP15 registers via HCR_EL2.TVM and mirror
    // the value into the corresponding EL2 sysreg. Guest reads are
    // not trapped (we don't set TRVM) so they go straight to hardware
    // and return the current value, which is either what we synced
    // on the last trapped write (SCTLR/TTBR/DACR) or what the CPU
    // wrote on the last EL1 abort (DFSR/DFAR).

    pub fn write_sctlr_el1(v: u64) { sysreg_write!("sctlr_el1", v); sync(); }
    pub fn write_ttbr0_el1(v: u64) { sysreg_write!("ttbr0_el1", v); sync(); }
    pub fn write_dacr32(v: u64) { sysreg_write!("dacr32_el2", v); sync(); }

    /// AArch32 DFSR via DFSR32_EL2 (op0=3 op1=4 CRn=5 CRm=0 op2=0,
    /// ARM ARM D10.2.32). Both MRS and MSR to this register take an
    /// EC=0 (UNDEFINED) exception at EL2 on Cortex-A53 under QEMU
    /// raspi3b, despite the ARM ARM saying it should be accessible
    /// from EL2 AArch64 when a lower EL supports AArch32 (which
    /// ID_AA64PFR0_EL1.EL1=0x2 confirms it does). So `write_dfsr32`
    /// is a no-op — we swallow the write. The hardware maintains
    /// DFSR correctly at EL1 when it takes an abort, which is what
    /// a kernel's abort handler needs. Guest writes are rare and
    /// typically just attempts to clear the register; losing them
    /// has no functional impact since the next abort will overwrite.
    pub fn write_dfsr32(_v: u64) { /* DFSR32_EL2 MSR UNDEFs on A53 */ }

    pub fn write_far_el1(v: u64) { sysreg_write!("far_el1", v); sync(); }

    pub fn read_sctlr_el1() -> u64 { sysreg_read!("sctlr_el1") }

    pub fn cache_maintenance_barrier() {
        // StrongARM c7 cache ops don't all map cleanly to A53 encodings
        // and A53 handles coherency natively for our configuration. A
        // `dsb ish` covers the write-buffer-drain encoding the guest
        // issues most often; the rest are no-ops.
        sync();
    }

    pub fn invalidate_tlb() {
        // SAFETY: TLBI variants are defined sysreg writes.
        unsafe {
            core::arch::asm!(
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    fn sync() {
        // SAFETY: barrier instructions only.
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    macro_rules! sysreg_read {
        ($reg:literal) => {{
            let v: u64;
            unsafe {
                core::arch::asm!(
                    concat!("mrs {}, ", $reg),
                    out(reg) v,
                    options(nomem, nostack, preserves_flags),
                );
            }
            v
        }};
    }
    macro_rules! sysreg_write {
        ($reg:literal, $val:expr) => {{
            unsafe {
                core::arch::asm!(
                    concat!("msr ", $reg, ", {}"),
                    in(reg) $val,
                    options(nostack, preserves_flags),
                );
            }
        }};
    }
    pub(crate) use {sysreg_read, sysreg_write};
}

fn handle_unknown(iss: u32) -> ! {
    let elr = read_sysreg!("elr_el2");
    let spsr = read_sysreg!("spsr_el2");
    // EC=0 "unknown reason" — an illegal / undefined AArch32 instruction.
    // Phase A contract: halt loudly with the faulting PC so we can see
    // what instruction the guest tried to execute and add handling for
    // it. No silent skip.
    kprintln!();
    kprintln!("*** EC=0 'unknown' trap halted (no silent skip per Phase A) ***");
    kprintln!("  ELR={:#x}  SPSR={:#x}  ISS={:#x}", elr, spsr, iss);
    if let Some(w) = guest_mem::read_word_pa(elr as u32) {
        kprintln!("  insn at ELR = {:#010x}", w);
    }
    cpu::halt();
}

// ----------------- helpers -----------------

fn advance_elr(bytes: u64) {
    let elr = read_sysreg!("elr_el2");
    // SAFETY: single-word write to EL2 sysreg; next ERET uses the new value.
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {}",
            "isb",
            in(reg) elr + bytes,
            options(nostack, preserves_flags),
        );
    }
}

pub fn describe_ec(ec: u32) -> &'static str {
    match ec {
        0x00 => "Unknown reason",
        0x07 => "SIMD/FP access trap (CPTR_EL2.TFP)",
        0x0E => "Illegal execution state",
        0x11 => "SVC from AArch32",
        0x12 => "HVC from AArch32",
        0x13 => "SMC from AArch32",
        0x15 => "SVC from AArch64",
        0x16 => "HVC from AArch64",
        0x17 => "SMC from AArch64",
        0x18 => "Trapped MSR/MRS/system instruction",
        0x20 => "Instruction abort from lower EL",
        0x21 => "Instruction abort from current EL",
        0x22 => "PC alignment fault",
        0x24 => "Data abort from lower EL",
        0x25 => "Data abort from current EL",
        0x26 => "SP alignment fault",
        0x3C => "BRK instruction",
        _ => "other",
    }
}
