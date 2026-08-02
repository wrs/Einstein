//! Undefined-instruction (HVC #UND trampoline) handling: the UND
//! history ring, the SWP / FPA-control-reg / DDK / MRS-SPSR emulators,
//! and `return_to_guest_from_und`.

use crate::{arch::cpu, hv::guest_mem, hv::hvc_imm::HvcImm};
use crate::diag::diag_util::SeenSet;
use crate::arch::trap_context::{read_sysreg, TrapContext};
use crate::kprintln;
use core::ptr::addr_of_mut;
use crate::hv::guest_endian::{guest_read_u32_pa as read_guest_word_pa,
                          guest_write_u32_pa as write_guest_word_pa};
use guest_mem::{read_byte_pa as read_guest_byte_pa,
                write_byte_pa as write_guest_byte_pa};
use super::{UND_SAVE_BANKED_LR_IPA, UND_SAVE_LR_IPA, UND_SAVE_R0_IPA, UND_SAVE_R1_IPA, UND_SAVE_SPSR_IPA};
use super::cp15::{self, log_cp15_deprecated_cache_all, log_cp15_strongarm_clock};
use crate::diag::trap_diag::{handle_loud_halt, handle_unhandled_exception};
use crate::hv::hooks::{ActiveGuest, GuestOs, UndHvcOutcome};
use guest_mem::{resolve_guest_pa, scan_to_null_word_aligned};

/// UND-path guest resume through the guest-OS UND-return stub. The
/// staging mechanics (literal write + I-cache publish into the stub
/// the trampoline patcher installed) live guest-side; see
/// `newton::guest_trampolines::return_to_guest_from_und`.
fn return_to_guest_from_und(ctx: &mut TrapContext, elr: u64, spsr: u64) {
    ActiveGuest::und_resume(ctx, elr, spsr);
}


// iter-87 diag: rolling buffer of recent UND faults. The wedge fires
// inside our trampoline (PC=0xffff54) — the trampoline's own HVC,
// caught by handle_und's catch-all. To learn how USR ended up at the
// trampoline's HVC instruction, we need to see the prior UNDs.
#[derive(Copy, Clone)]
struct UndHistEntry {
    faulting_pc: u32,
    insn: u32,
    spsr_und: u32,
    lr_usr: u32,
    sp_for_mode: u32,
    /// Heuristic stack-walked caller LR. For SWP UNDs inside Acquire
    /// we read SP+32; inside Release we read SP+12.
    caller_lr: u32,
    /// Outer-outer caller. For Acquire-from-Grabber::ct (the dominant
    /// case in the Phase-B sound stall), this is the function that
    /// constructed the Grabber — e.g. `TNewInternalFlash::Read`,
    /// `TMuxStore::Read`. Read from SP+92 (Acquire push + Grabber::ct
    /// push + Read's `sub sp,#4` slot + Read's saved LR offset).
    outer_caller_lr: u32,
}
const UND_HIST_LEN: usize = 32;
static mut UND_HISTORY: [UndHistEntry; UND_HIST_LEN] = [
    UndHistEntry {
        faulting_pc: 0, insn: 0, spsr_und: 0, lr_usr: 0,
        sp_for_mode: 0, caller_lr: 0, outer_caller_lr: 0,
    };
    UND_HIST_LEN
];
static mut UND_HIST_NEXT: usize = 0;
static mut UND_HIST_COUNT: u64 = 0;

fn record_und_history(faulting_pc: u32, insn: u32, spsr_und: u32, ctx: &TrapContext) {
    // Capture banked SP for the faulting mode, so dumps show where
    // the faulting code's stack was. lr_usr is ctx.x[14]; for non-USR
    // sources it's still informative as the user-space caller LR.
    let sp = crate::arch::banked::sp_for_mode(ctx, spsr_und);
    // Heuristic caller-LR capture: SWP at the TULockingSemaphore::Swap
    // helper (0x003ae204) is the wedge signature in `Phase-B stall after
    // TSoundServer::TheMain stack-collision`. Acquire's prologue pushes
    // 10 words (`push {r4-r9, fp, ip, lr, pc}`) and calls Swap with no
    // intervening stack changes; Release's prologue pushes 5 words.
    // Distinguish by lr_usr (= the bl-Swap return PC):
    //   0x0025a2c8 → Acquire → caller LR at SP+32
    //   0x0025a338 → Release → caller LR at SP+12
    // For Acquire, the immediate caller is TULockingSemaphoreGrabber::ct
    // (RAII helper at 0x0013b6d4). To find the outer function that
    // constructed the Grabber we walk one more frame: Grabber::ct's
    // own pushed LR sits at SP+(40+16) = SP+56, and the function that
    // CALLED that constructor (i.e. TNewInternalFlash::Read, TMuxStore::Read,
    // etc.) lives at SP+(64+24+4) = SP+92 — Read pushes 8 words then
    // `sub sp,#4` before bl Grabber::ct.
    let lr_usr_raw = ctx.x[14] as u32;
    let (caller_lr, outer_caller_lr) = if faulting_pc == 0x003a_e204 {
        if lr_usr_raw == 0x0025_a2c8 {
            // SWP inside Acquire: SP+32 = caller of Acquire (= Grabber::ct),
            // SP+92 = caller of Grabber::ct (= the Read function).
            let c = crate::hv::guest_endian::guest_read_u32_va(sp.wrapping_add(32)).unwrap_or(0);
            let o = crate::hv::guest_endian::guest_read_u32_va(sp.wrapping_add(92)).unwrap_or(0);
            (c, o)
        } else if lr_usr_raw == 0x0025_a338 {
            // SWP inside Release: SP+12 = caller of Release (= Grabber::dt).
            let c = crate::hv::guest_endian::guest_read_u32_va(sp.wrapping_add(12)).unwrap_or(0);
            (c, 0)
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };
    let entry = UndHistEntry {
        faulting_pc,
        insn,
        spsr_und,
        lr_usr: ctx.x[14] as u32,
        sp_for_mode: sp,
        caller_lr,
        outer_caller_lr,
    };
    // SAFETY: single-threaded EL2.
    unsafe {
        let i = UND_HIST_NEXT;
        UND_HISTORY[i] = entry;
        UND_HIST_NEXT = (i + 1) % UND_HIST_LEN;
        UND_HIST_COUNT = UND_HIST_COUNT.wrapping_add(1);
    }
}

pub(crate) fn dump_und_history() {
    // SAFETY: single-threaded EL2.
    let (count, next) = unsafe { (UND_HIST_COUNT, UND_HIST_NEXT) };
    let n = if count < UND_HIST_LEN as u64 { count as usize } else { UND_HIST_LEN };
    kprintln!("UND history (last {} of {} total UNDs, oldest first):", n, count);
    for k in 0..n {
        let i = (next + UND_HIST_LEN - n + k) % UND_HIST_LEN;
        // SAFETY: index in range, single-threaded.
        let e = unsafe { UND_HISTORY[i] };
        let mode = e.spsr_und & 0x1F;
        kprintln!(
            "  #{:>3}  PC={:#010x} insn={:#010x} mode={:#x}({}) sp={:#010x} lr_usr={:#010x} caller={:#010x} outer={:#010x}",
            (count - n as u64 + k as u64),
            e.faulting_pc, e.insn, mode, crate::arch::arm_decode::aarch32_mode_name(mode),
            e.sp_for_mode, e.lr_usr, e.caller_lr, e.outer_caller_lr,
        );
    }
}

pub(crate) fn handle_und(ctx: &mut TrapContext) {
    // Restore pre-UND R0, R1, R12 from the stash slots the trampoline
    // populated at entry. R0/R1 go through RAM slots (the trampoline
    // unavoidably clobbers R0 to hold the save-slot VA and R1 across
    // the SVC bounce). R12 goes through TPIDR_EL0 (= AArch32
    // TPIDRURW), which the trampoline writes with `MCR p15,0,r12,...`
    // as its very first instruction before clobbering R12 to hold the
    // save-slot base. TPIDRURW is ARMv6+ state the SA-1100-era Newton
    // ROM never touches, so using it as the R12 save slot is safe.
    //
    // Restoring R12 matters for the shadow-byte-access UDF-trap path,
    // where the faulting instruction can legitimately use R12 as base
    // / data / offset. The tracer's function-entry UDF sites don't
    // need R12 (every Newton 2.x prologue begins `MOV R12, R13`), but
    // doing the restore unconditionally is cheaper than branching on
    // the UDF kind.
    ctx.x[0] = match read_guest_word_pa(UND_SAVE_R0_IPA) {
        Some(v) => v as u64,
        None => {
            kprintln!("*** handle_und: UND_SAVE_R0 slot @{:#x} unreadable", UND_SAVE_R0_IPA);
            cpu::halt();
        }
    };
    ctx.x[1] = match read_guest_word_pa(UND_SAVE_R1_IPA) {
        Some(v) => v as u64,
        None => {
            kprintln!("*** handle_und: UND_SAVE_R1 slot @{:#x} unreadable", UND_SAVE_R1_IPA);
            cpu::halt();
        }
    };
    ctx.x[12] = read_sysreg!("tpidr_el0");

    let lr_und = match read_guest_word_pa(UND_SAVE_LR_IPA) {
        Some(v) => v,
        None => {
            kprintln!("*** handle_und: UND_SAVE_LR slot unreadable");
            cpu::halt();
        }
    };
    let spsr_und = match read_guest_word_pa(UND_SAVE_SPSR_IPA) {
        Some(v) => v as u64,
        None => {
            kprintln!("*** handle_und: UND_SAVE_SPSR slot @{:#x} unreadable", UND_SAVE_SPSR_IPA);
            cpu::halt();
        }
    };
    let faulting_pc = lr_und.wrapping_sub(4);

    // The faulting PC is a kernel VA (post-MMU); for non-identity-mapped
    // VAs (e.g. the gROMPublicJumpTable aliased at 0x01E00000) the IPA
    // differs from the VA. Try a PA-direct read first, then fall through
    // to a stage-1-walked VA read so the decoder picks up bytes from
    // the actual backing PA when the kernel has set up an aliasing
    // L2 entry.
    let insn = match read_guest_word_pa(faulting_pc)
        .or_else(|| crate::hv::guest_endian::guest_read_u32_va(faulting_pc))
    {
        Some(w) => w,
        None => {
            kprintln!(
                "*** handle_und: faulting PC {:#x} is outside mapped guest memory",
                faulting_pc
            );
            guest_mem::dump_stage1_walk(faulting_pc);
            cpu::halt();
        }
    };

    record_und_history(faulting_pc, insn, spsr_und as u32, ctx);

    // USR-mode HVC probe re-route: HVC is UNDEFINED at EL0, so a guest
    // ROM probe patched as `HVC #imm` and executed from USR mode raises
    // UND and arrives here instead of `handle_hvc`. The probe set (and
    // the BOOTOS_PC guard) is guest-OS knowledge — consult the hook
    // first; its instruction set is disjoint from every generic arm
    // below, so `NotMine` falls through with unchanged semantics.
    match ActiveGuest::handle_und_hvc(ctx, insn, faulting_pc, spsr_und) {
        UndHvcOutcome::Resume { pc, spsr } => {
            return_to_guest_from_und(ctx, pc, spsr);
            return;
        }
        UndHvcOutcome::Done => return,
        UndHvcOutcome::NotMine => {}
    }

    // StrongARM CP15 clock-control write (MCR p15, 0, Rt, c15, c1, 2).
    // ARMv8 doesn't define that register, so the instruction raises UND
    // locally at EL1 rather than trapping via HCR_EL2.TIDCP — which is
    // why we handle it here and not in handle_cp15_trap. Fires exactly
    // once during 717006 boot (probe/FINDINGS.md §16.4); treat as a
    // no-op and advance past it. Mask clears cond (31:28) and Rt
    // (15:12); target encoding is MCR p15,0,Rt,c15,c1,2 (0x_E0F_0F51).
    // The ROM's StrongARM-detect sequence at 0x186a8 uses cond=EQ; the
    // UND only fires when the condition already passed, so any cond
    // is valid here.
    if (insn & 0x0FFF_0FFF) == 0x0E0F_0F51 {
        log_cp15_strongarm_clock(faulting_pc);
        return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Deprecated ARMv4 "Invalidate Unified Cache" encoding
    // `MCR p15, 0, Rt, c7, c7, 0` — ARMv7+/A53 UND this, but the ROM
    // emits it from FlushTheCache at 0x18924 (see the 717006 BootOS
    // flow; Einstein treats this as a valid deprecated cache op and
    // no-ops it). On A53 the JIT probe showed this opcode firing
    // exactly once at boot, from inside FlushTheCache. Emulate as a
    // cache-clean-all via `dsb ish; ic ialluis; isb` and advance past
    // it. Mask clears Rt (15:12).
    if (insn & 0xFFFF_0FFF) == 0xEE07_0F17 {
        log_cp15_deprecated_cache_all(faulting_pc);
        cp15::invalidate_icache_all();
        return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return;
    }

    // Deprecated ARMv4 "Invalidate Entire Data Cache" encoding
    // `MCR p15, 0, Rt, c7, c6, 0` — same family as c7,c7,0 above
    // (which the kernel emits from FlushTheCache); A53 also UNDs
    // this one. Seen at PC=0x189C0 in the 717006 boot path during
    // FlushDCache. Emulate as a no-op (A53 maintains coherency
    // natively for our config) and advance past it.
    if (insn & 0xFFFF_0FFF) == 0xEE07_0F16 {
        log_cp15_deprecated_cache_all(faulting_pc);
        return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
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
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        0xE6000510 => {
            // DebuggerUND: opcode followed by a null-terminated ASCII
            // string (typically the debug-log message), padded to the
            // next 4-byte boundary. Einstein's TEmulator::DebuggerUND
            // reads the string byte-by-byte starting at inPAddr+4
            // until it hits a null. We do the same and advance PC past
            // the final word containing the null. If we got this wrong
            // (advance only by 8), the CPU would fall into the middle
            // of the ASCII payload and UND on a random "instruction"
            // (what we saw as insn=0x2d757365 at 0x3ae1ac — "esu-" bytes
            // of "non-user mode.").
            let msg_start = faulting_pc + 4;
            let msg_end = scan_to_null_word_aligned(msg_start, 256);
            log_debugger_und(faulting_pc, msg_start, msg_end);
            return_to_guest_from_und(ctx, msg_end as u64, spsr_und);
        }
        // Newton DDK debug-primitive function-entry UNDs. Each
        // `0xE60000XX10` opcode sits at a symbol in the ROM (ExitToShell,
        // Debugger, DebugStr, SendTestResult, TapFileCntl, RawDebugStr,
        // RawDebugger — see 0x38ce6c..0x38ce84 in rom.dis) and is called
        // via `BL <symbol>`. Einstein's JIT (TJITGeneric_Other.cpp)
        // emulates TapFileCntl with `POPNIL(); SETPC(GETCALLER() + 4)` —
        // i.e. return to the caller's LR. The rest fall through Einstein's
        // generic UndefinedInstruction path and take a real ARM UND
        // exception; on our guest that wedges because `gDebugger = 1`
        // makes the ROM's 0x38ce88 handler jump to ReportException →
        // StopImage. So every one of these must be emulated in EL2 as
        // a "log and return to caller" NOP.
        //
        // The caller's LR is captured by the UND trampoline into the
        // `UND_SAVE_BANKED_LR_IPA` RAM slot (the trampoline briefly
        // switches to the faulting mode — SYS for USR — and stores that
        // mode's banked LR there). ERETing to that address resumes the
        // caller's instruction stream after the BL.
        0xE6000110 | 0xE6000210 | 0xE6000310 | 0xE6000710 | 0xE6000810 => {
            let name = match insn {
                0xE6000110 => "ExitToShell",
                0xE6000210 => "Debugger",
                0xE6000310 => "DebugStr",
                0xE6000710 => "SendTestResult",
                0xE6000810 => "TapFileCntl",
                _ => "DDK-UND",
            };
            let r0 = ctx.x[0] as u32;
            log_und_budgeted(name, faulting_pc, Some(r0));
            // Each of these UND opcodes is a Newton-DDK function entry,
            // called from ROM code via `BL <symbol>` (see rom.dis around
            // 0x38ce6c..0x38ce84). Einstein's JIT returns to the caller
            // via `POPNIL; SETPC(GETCALLER()+4)` for TapFileCntl and the
            // same shape applies to the rest. The UND trampoline
            // captures the faulting mode's banked LR (via its mode-
            // switch dance — see `patch_und_vector` in `guest_mem.rs`)
            // into the `UND_SAVE_BANKED_LR_IPA` RAM slot so we can ERET
            // there.
            let banked_lr = read_guest_word_pa(UND_SAVE_BANKED_LR_IPA).unwrap_or(0);
            if banked_lr == 0 {
                kprintln!(
                    "*** {} @PC={:#x}: banked LR slot @{:#x} is 0 — UND trampoline must \
                     stage the faulting mode's LR before HVC (see ROM trampoline mode-\
                     switch dance in patch_und_vector). Halting.",
                    name, faulting_pc, UND_SAVE_BANKED_LR_IPA,
                );
                cpu::halt();
            }
            // TapFileCntl has an Einstein-modelled dispatch table
            // (do_sys_open / read / write / …) — we don't implement the
            // file protocol, so write -1 to R0 as a "call failed" result
            // that the caller can observe. The other primitives leave R0
            // alone.
            if insn == 0xE6000810 {
                ctx.x[0] = 0xFFFF_FFFFu32 as u64;
            }
            return_to_guest_from_und(ctx, banked_lr as u64, spsr_und);
        }
        _ if is_swp_encoding(insn) => {
            emulate_swp(ctx, insn, faulting_pc);
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        // `MRS Rd, SPSR` executed in USR mode. On ARMv4 / SA-1100 this
        // returns the CPSR (no SPSR exists for USR); the A53 UNDs it.
        // Einstein models this at `TARMProcessor::GetSPSR()`
        // (TARMProcessor.cpp:774-781): "At MonitorEntryGlue and
        // elsewhere, the OS accesses SPSR in User mode and apparently
        // gets CPSR." Emulate by writing the pre-UND CPSR (i.e.
        // `spsr_und`, which the UND trampoline captured from the
        // hardware-saved SPSR_und) into Rd and advancing PC by 4. Rd
        // is extracted from bits[15:12]; per the MRS encoding, r15 is
        // UNPREDICTABLE here, so bail if the guest asked for it.
        _ if (insn & 0x0FFF_0FFF) == 0x014F_0000
            && (spsr_und & 0x1F) == 0x10 =>
        {
            let rd = ((insn >> 12) & 0xF) as usize;
            if rd == 15 {
                kprintln!(
                    "*** MRS R15, SPSR (USR): UNPREDICTABLE at PC={:#x}",
                    faulting_pc
                );
                cpu::halt();
            }
            ctx.x[rd] = spsr_und;
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        // `MOVS PC, LR` (cond=AL) executed in USR mode. On ARMv4 /
        // SA-1100 this is a standard function-return idiom: in
        // privileged modes it returns from an exception (PC=LR,
        // CPSR=SPSR); in USR mode there is no SPSR (Einstein's
        // TARMProcessor::GetSPSR returns CPSR for USR, so the
        // CPSR<-SPSR copy is a no-op). ARMv8 UNDs this in USR mode
        // because the encoding is UNPREDICTABLE there. The Newton
        // FPE library (rom.dis 0x0038_d000..0x0039_3b80) ends nearly
        // every helper with this exact opcode (e.g. _rintM at
        // 0x0038_d8c4, _sinM at 0x0039_2cd0, etc.), and the kernel's
        // CP15 init at 0x0001_9428 uses it as well. Emulate as a
        // plain return: ERET to LR_usr (ctx.x[14] per Table D1-79)
        // with SPSR_und unchanged so we stay in USR mode.
        0xe1b0_f00e if (spsr_und & 0x1F) == 0x10 => {
            let lr_usr = ctx.x[14] as u32;
            return_to_guest_from_und(ctx, lr_usr as u64, spsr_und);
        }
        // Tracer trampoline slot[0] executed in USR mode. HVC is
        // UNDEFINED at EL0, so the trampoline's `hvc #TRACE_TAG`
        // raises an UND exception instead of entering EL2 directly.
        // Log the entry (same content as the normal HVC path) and
        // resume at slot[1] — the original first instruction copy —
        // restoring the USR-mode CPSR. Without this, any traced
        // function the Newton kernel calls in user mode (e.g. OsBoot
        // per the `code-symbols.txt` classification) halts here.
        #[cfg(feature = "trace")]
        _ if insn == HvcImm::Trace.insn()
            && crate::diag::tracer::in_trampoline_pool(faulting_pc) =>
        {
            crate::diag::tracer::log_trace_at(ctx, faulting_pc, spsr_und as u32);
            return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        }
        // LoudHalt canary (Reboot, PowerOffAndReboot, StopImage).
        // The kernel calls these from USR mode on UnhandledException
        // / idle; HVC from EL0 is UNDEFINED, so our patched
        // `HVC #LoudHalt` lands here. Route into the same halt
        // handler the HVC path uses.
        _ if insn == HvcImm::LoudHalt.insn() => {
            handle_loud_halt(ctx);
        }
        // (The USR-mode probe HVCs — BootOs, RememberSwiret, the
        // PHammerOutTranslator thunks, the log_store probes — are
        // claimed by `ActiveGuest::handle_und_hvc` before this match.)
        _ if insn == HvcImm::UnhandledException.insn() => {
            handle_unhandled_exception(ctx, false);
            // Never returns: handle_unhandled_exception halts.
        }
        _ if insn == HvcImm::UnhandledNumException.insn() => {
            handle_unhandled_exception(ctx, true);
            // Never returns: handle_unhandled_exception halts.
        }
        // User-driven guest software breakpoint — must be checked
        // before the tracer path because the marker encoding
        // (UDF #0xFFFE) is also a UDF-shape instruction. See
        // `src/diag/guest_bp.rs`.
        _ if insn == crate::diag::guest_bp::BP_UDF_INSN => {
            if !crate::diag::guest_bp::handle_user_bp_und(ctx, faulting_pc, spsr_und, insn) {
                kprintln!(
                    "*** guest_bp: marker at PC={:#x} with no matching table entry — halting",
                    faulting_pc
                );
                cpu::halt();
            }
        }
        // FPA control/status register access: RFS / WFS / RFC / WFC.
        // These UND on A53 (no FPA coprocessor) and — per ARMv8 B2.2.4 —
        // may UND even when their condition is false. Emulate as a NOP:
        // reads return 0 in Rt, writes are discarded. Nothing Newton boot
        // actually runs exercises the FPA control/status registers —
        // FPE_Install's helper at 0x392704 uses `rfceq`/`wfceq` to init
        // the emulator state, and the context-word semantic (rounding
        // mode, trap enables) is never consulted by integer-math boot
        // code.
        _ if is_fpa_ctrl_reg_insn(insn) => {
            emulate_fpa_ctrl_reg(ctx, insn, faulting_pc, spsr_und);
        }
        // FPA load/store/arithmetic UNDs. The IPA-0x04 → bypass-stub
        // path at `FPA_BYPASS_STUB_OFFSET` (see guest_mem.rs) was meant
        // to catch these and `b FPE_JT` straight from UND mode without
        // an EL2 round trip. Empirically the stub doesn't fire (every
        // post-MMU FPA UND reaches handle_und via UND_TRAMP), and the
        // halt-on-arrival behaviour from iter-83/84/85 era is now the
        // boot stall. Replicate the bypass-stub semantics from EL2:
        // ERET into UND mode at FPE_JT (= 0x0038_D874).
        //
        // SPSR_EL2 is left as the natural HVC-from-UND-mode capture,
        // so the ERET drops back to AArch32 EL1 in UND mode. ELR_EL2
        // overrides the post-HVC ELR (= UND_TRAMP base+22 = `b .`
        // guard) with the FPE_JT entry. ctx.x[12] (= R12) was already
        // restored from TPIDRURW at handle_und entry; ctx.x[22] (=
        // R14_und) carries `faulting_pc + 4` from the trampoline's
        // banked save, which is exactly what FPE_JT expects to find
        // in LR_und so its `subs pc, lr, #4` epilog returns to the
        // faulting site. The kernel's FPE then emulates the FPA insn
        // and returns to source mode at faulting_pc+4 via `movs pc,
        // lr` (the architectural movs-pc consumes SPSR_und, restoring
        // the source-mode CPSR).
        _ if is_fpa_insn(insn) => {
            // ARMv8 Cortex-A53 deprecates conditional execution of
            // coprocessor instructions and effectively executes them
            // unconditionally — a conditional FPA insn UND-traps even
            // when its cond field would have failed on ARMv4. The
            // Newton FPE was written for ARMv4 and relies on cond-
            // false coprocessor insns being skipped silently (e.g.,
            // the decimal-conversion encoder's `dvfple`/`mufmie` at
            // 0x0038F5B4/B8 must only fire on the correct sign of
            // the binary exponent — otherwise both fire and corrupt
            // the digit-extraction path, producing the calc bug:
            // 0.2 → 0.02, 10 → 100, etc.). Restore ARMv4 semantics
            // here: if cond fails, return to source mode at
            // faulting_pc+4 without entering the FPE.
            let cond = (insn >> 28) & 0xF;
            if !crate::arch::arm_decode::arm_cond_passed(cond, spsr_und as u32) {
                log_fpa_cond_skip(faulting_pc, insn);
                return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
                return;
            }
            log_fpa_bypass_miss(faulting_pc, insn);
            const FPE_JT_VA: u64 = 0x0038_D874;
            // SAFETY: ELR_EL2 is the AArch64 system register that the
            // sync-trap dispatcher's ERET stub will consume. SPSR_EL2
            // is unchanged (still the AArch32-UND mode the HVC
            // captured), so the ERET re-enters UND mode at FPE_JT.
            unsafe {
                core::arch::asm!(
                    "msr elr_el2, {pc}",
                    "isb",
                    pc = in(reg) FPE_JT_VA,
                    options(nostack, preserves_flags),
                );
            }
            return;
        }
        _ => {
            // Close any open tarmac window before the diagnostic
            // kprintln!'s below run, so they don't bloat the trace.
            #[cfg(feature = "platform-fvp-base")]
            crate::diag::tarmac::emit_stop();
            kprintln!(
                "*** unrecognised UND: insn={:#010x} at PC={:#x} SPSR_und={:#x}",
                insn, faulting_pc, spsr_und
            );
            kprintln!(
                "  src_mode={:#x} ({})  r0..r7:   {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                (spsr_und as u32) & 0x1F,
                crate::arch::arm_decode::aarch32_mode_name((spsr_und as u32) & 0x1F),
                ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
                ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32,
            );
            kprintln!(
                "                       r8..r15:  {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
                ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32,
                ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32, ctx.x[15] as u32,
            );
            kprintln!(
                "                       SP_und=ctx.x[23]={:#x} LR_und=ctx.x[22]={:#x}",
                ctx.x[23] as u32, ctx.x[22] as u32,
            );
            kprintln!(
                "    (extend handle_und in trap.rs to handle this opcode)"
            );
            dump_und_history();
            // iter-87 diag: dump the USR stack near SP_usr (via stage-1
            // walk) — if USR reached PC=0xffff54 via POP {pc} or LDM,
            // the popped value should still be visible just below SP_usr.
            let sp_usr = ctx.x[13] as u32;
            let read_va = |va: u32| -> Option<u32> {
                let pa = guest_mem::translate_va(va)?;
                read_guest_word_pa(pa)
            };
            kprintln!("USR stack (SP_usr={:#010x}, words at sp-32..sp+96):", sp_usr);
            for i in 0..32i32 {
                let addr = sp_usr.wrapping_add((i.wrapping_sub(8) * 4) as u32);
                let v = read_va(addr)
                    .map(|w| w as i64)
                    .unwrap_or(-1);
                if v < 0 {
                    kprintln!("  [{:#010x}] = (unmapped)", addr);
                } else {
                    kprintln!("  [{:#010x}] = {:#010x}", addr, v as u32);
                }
            }
            // Also resolve the BL target chain to spot a corrupt JT thunk:
            // the most-recent BL was at LR_usr-4. Show its insn and decoded
            // target, then the word at the target (the JT thunk's `b`).
            let lr_usr = ctx.x[14] as u32;
            let bl_pc = lr_usr.wrapping_sub(4);
            kprintln!("BL site (LR_usr-4 = {:#010x}):", bl_pc);
            let bl_insn = read_va(bl_pc).unwrap_or(0xDEAD_BEEF);
            kprintln!("  insn = {:#010x}", bl_insn);
            if (bl_insn & 0xFF00_0000) == 0xEB00_0000 {
                let imm24 = bl_insn & 0x00FF_FFFF;
                let signed = ((imm24 << 8) as i32) >> 8;
                let target =
                    bl_pc.wrapping_add(8).wrapping_add((signed as u32).wrapping_shl(2));
                kprintln!("  decoded BL target = {:#010x}", target);
                let target_insn = read_va(target).unwrap_or(0xDEAD_BEEF);
                kprintln!("  insn at target = {:#010x}", target_insn);
                // If the target is a `b imm24` (JT thunk), follow it.
                if (target_insn & 0xFF00_0000) == 0xEA00_0000 {
                    let imm24b = target_insn & 0x00FF_FFFF;
                    let signedb = ((imm24b << 8) as i32) >> 8;
                    let target2 = target
                        .wrapping_add(8)
                        .wrapping_add((signedb as u32).wrapping_shl(2));
                    kprintln!("  jt target follows-> {:#010x}", target2);
                    let target2_insn = read_va(target2).unwrap_or(0xDEAD_BEEF);
                    kprintln!("  insn at jt target = {:#010x}", target2_insn);
                    // And the next 3 insns of the function body.
                    for off in [4u32, 8, 12, 16] {
                        let v = read_va(target2.wrapping_add(off))
                            .unwrap_or(0xDEAD_BEEF);
                        kprintln!("  insn at {:#010x} = {:#010x}",
                                  target2.wrapping_add(off), v);
                    }
                }
            }
            // Also dump the trampoline area so we can verify the HVC
            // is at 0xffff54.
            kprintln!("trampoline area:");
            for off in [0u32, 4, 8, 0x40, 0x44, 0x50, 0x54, 0x58, 0x5C].iter() {
                let addr = 0x00FF_FF00u32.wrapping_add(*off);
                let v = read_va(addr).unwrap_or(0xDEAD_BEEF);
                kprintln!("  insn at {:#010x} = {:#010x}", addr, v);
            }
            cpu::halt();
        }
    }
}

/// Does `insn` match one of the four FPA control/status register
/// encodings — RFS, WFS, RFC, WFC — targeting CP1?
///
///   RFS: cccc 1110 0011 0000 Rt 0001 0001 0000  (MRC p1, 1, Rt, c0, c0, 0)
///   WFS: cccc 1110 0010 0000 Rt 0001 0001 0000  (MCR p1, 1, Rt, c0, c0, 0)
///   RFC: cccc 1110 0101 0000 Rt 0001 0001 0000  (MRC p1, 2, Rt, c0, c0, 0)
///   WFC: cccc 1110 0100 0000 Rt 0001 0001 0000  (MCR p1, 2, Rt, c0, c0, 0)
///
/// The common bits fix the shape as `0x?E00_?110` with bits 23:20 ∈
/// {2, 3, 4, 5}. Mask 0x0F0F_0FFF preserves everything except cond
/// (31:28), opc1/L (23:20), and Rt (15:12); the fixed pattern is
/// 0x0E00_0110. We then require bits 23:20 to be one of the four
/// control/status register values — this leaves FPA data ops (ADF, LDF,
/// …) and non-CP1 accesses to halt loudly, which is the right Phase-A
/// trip-wire behaviour.
fn is_fpa_ctrl_reg_insn(insn: u32) -> bool {
    if (insn & 0x0F0F_0FFF) != 0x0E00_0110 {
        return false;
    }
    matches!((insn >> 20) & 0xF, 2 | 3 | 4 | 5)
}

/// Does `insn` match an FPA-class encoding targeting cp1 or cp2?
///
/// Covers: LDF/STF (LDC/STC, bits[27:24]=0xC,0xD with the N bit selecting
/// the LFM/SFM multi-register variants), CDP (FPA arithmetic — ADF, MUF,
/// MVF, CMF, …; bits[27:24]=0xE, bit[4]=0), and MCR/MRC (FIX/FLT/etc.;
/// bits[27:24]=0xE, bit[4]=1). The Newton kernel's FPA emulator at ROM
/// 0x38d8dc handles every shape in this family.
///
/// `cond == 0xF` (unconditional) is excluded — that encoding is reserved
/// for VFP/Advanced SIMD on ARMv5+ and never appears in 717006 ROM. The
/// existing `is_fpa_ctrl_reg_insn` arm runs first and catches RFS/WFS/RFC/
/// WFC as in-EL2 NOPs, so this returns true for those too but is harmless
/// (the ctrl-reg arm matches earlier in the dispatch chain).
/// FPA bypass-miss counter. The in-ROM `FPA_BYPASS_STUB_OFFSET` should
/// catch every FPA-class UND and `b FPE_JT` directly without reaching
/// EL2. Empirically (iter-107) the stub fires inconsistently — the
/// classifier marks the high-ROM stub region as data, so the loader
/// leaves bytes BE-natural and the AArch32 I-cache cold-fetches stale
/// memory bytes for the stub site, falling through into UND_TRAMP and
/// arriving here. Each miss is handled by EL2 ERETing into FPE_JT
/// directly (option (b) per PLAN.md iter-107). The first 4 misses log;
/// later misses bump the counter silently. A high count after a long
/// boot is a sign the in-ROM bypass needs investigation.
fn log_fpa_bypass_miss(faulting_pc: u32, insn: u32) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static FIRED: AtomicU32 = AtomicU32::new(0);
    let n = FIRED.fetch_add(1, Ordering::Relaxed);
    if n < 4 {
        kprintln!(
            "fpa-bypass-miss[{}]: insn={:#010x} faulting_pc={:#x} \
             — EL2 redirects to FPE_JT",
            n, insn, faulting_pc,
        );
    }
}

fn is_fpa_insn(insn: u32) -> bool {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        return false;
    }
    let coproc = (insn >> 8) & 0xF;
    if coproc != 1 && coproc != 2 {
        return false;
    }
    matches!((insn >> 24) & 0xF, 0xC | 0xD | 0xE)
}

/// Emulate an FPA control/status register access (RFS / WFS / RFC /
/// WFC) as a NOP: reads return 0 in Rt, writes are discarded, PC
/// advances by 4. Respects the ARM condition code — an FVP-taken UND
/// on a false-condition `rfceq` etc. leaves Rt alone, matching the
/// architecturally-specified NOP behaviour (ARMv8 B2.2.4).
fn emulate_fpa_ctrl_reg(
    ctx: &mut TrapContext,
    insn: u32,
    faulting_pc: u32,
    spsr_und: u64,
) {
    let cond = (insn >> 28) & 0xF;
    let passed = crate::arch::arm_decode::arm_cond_passed(cond, spsr_und as u32);
    if passed {
        let is_read = ((insn >> 20) & 1) != 0;
        let rt = ((insn >> 12) & 0xF) as usize;
        // Rt == r15 is UNPREDICTABLE for RFS/RFC on FPA; ignore the
        // write rather than scribble on the AArch64 context's x15.
        if is_read && rt < 15 {
            ctx.x[rt] = 0;
        }
        // Write path: discard the source value. The FPA control word
        // holds rounding mode + trap enables, neither observable under
        // our emulation.
    }
    log_fpa_ctrl_reg(faulting_pc, insn, passed);
    return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
}

fn log_fpa_ctrl_reg(pc: u32, insn: u32, cond_passed: bool) {
    static mut SEEN: SeenSet<u32, 16> = SeenSet::new(0);
    // SAFETY: single-core EL2; see diag_util module docs.
    let first = unsafe { (*addr_of_mut!(SEEN)).first_time(pc) };
    if first {
        let name = match (insn >> 20) & 0xF {
            2 => "WFS",
            3 => "RFS",
            4 => "WFC",
            5 => "RFC",
            _ => "FPA-CR?",
        };
        let rt = (insn >> 12) & 0xF;
        kprintln!(
            "und: FPA {} r{} @PC={:#x} — NOP (cond {})",
            name,
            rt,
            pc,
            if cond_passed { "passed" } else { "failed" },
        );
    }
}

/// Log (dedupe-first-N) a conditional FPA insn whose cond field
/// failed against source CPSR.NZCV. Without the cond-skip emulation
/// in `handle_und`, the FPE would have executed the operation
/// unconditionally and produced wrong results — see the calc-bug
/// analysis (0.2 → 0.02 via decimal-encoder's dvfple/mufmie).
fn log_fpa_cond_skip(pc: u32, insn: u32) {
    static mut SEEN: SeenSet<u32, 16> = SeenSet::new(0);
    // SAFETY: single-core EL2; see diag_util module docs.
    let first = unsafe { (*addr_of_mut!(SEEN)).first_time(pc) };
    if first {
        let cond = (insn >> 28) & 0xF;
        let cond_name = match cond {
            0x0 => "EQ", 0x1 => "NE", 0x2 => "CS", 0x3 => "CC",
            0x4 => "MI", 0x5 => "PL", 0x6 => "VS", 0x7 => "VC",
            0x8 => "HI", 0x9 => "LS", 0xA => "GE", 0xB => "LT",
            0xC => "GT", 0xD => "LE", _ => "??",
        };
        kprintln!(
            "und: FPA cond-{} insn={:#010x} @PC={:#x} — cond failed, ARMv4 skip emulated",
            cond_name, insn, pc,
        );
    }
}

pub(crate) fn read_banked_spsr(which: &'static str) -> u64 {
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

    let va = ctx.x[rn] as u32;
    let new_value = ctx.x[rm] as u32;

    // The SWP target is a VA when the guest stage-1 MMU is on — the only
    // in-ROM SWP site is `Swap` at PC 0x3ae204, reached from kernel code
    // that hands us user/kernel VAs (e.g. 0x0c1xxxxx, which stage-1
    // remaps into RAM per TMemoryConsts). Pre-MMU it's identity and we
    // can feed `va` straight through.
    let addr = match resolve_guest_pa(va) {
        Some(pa) => pa,
        None => {
            kprintln!(
                "*** SWP{} [r{}]={:#x} — stage-1 translation failed at PC={:#x}",
                if is_byte { "B" } else { "" }, rn, va, faulting_pc
            );
            cpu::halt();
        }
    };

    if is_byte {
        let old = match read_guest_byte_pa(addr) {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** SWPB [r{}]={:#x} (PA={:#x}) — address not readable",
                    rn, va, addr
                );
                cpu::halt();
            }
        };
        if !write_guest_byte_pa(addr, new_value as u8) {
            kprintln!(
                "*** SWPB [r{}]={:#x} (PA={:#x}) — address not writable",
                rn, va, addr
            );
            cpu::halt();
        }
        ctx.x[rd] = old as u64;
    } else {
        if addr & 3 != 0 {
            kprintln!(
                "*** SWP with unaligned address r{}={:#x} (ignored, guest may fault)",
                rn, va
            );
        }
        let old = match read_guest_word_pa(addr) {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** SWP [r{}]={:#x} (PA={:#x}) — address not readable",
                    rn, va, addr
                );
                cpu::halt();
            }
        };
        if !write_guest_word_pa(addr, new_value) {
            kprintln!(
                "*** SWP [r{}]={:#x} (PA={:#x}) — address not writable",
                rn, va, addr
            );
            cpu::halt();
        }
        ctx.x[rd] = old as u64;
    }

    log_swp_budgeted(faulting_pc, is_byte, rn, rd, rm, addr);
}

fn log_und_budgeted(name: &str, pc: u32, payload: Option<u32>) {
    // Dedup SystemBootUND / TapFileCntlUND by PC — only 6 sites in ROM
    // total. Same rationale as log_debugger_und: one log per site gives
    // us clear bring-up breadcrumbs without flooding on tight loops.
    static mut SEEN: SeenSet<u32, 16> = SeenSet::new(0);
    // SAFETY: single-core EL2; see diag_util module docs.
    let first = unsafe { (*addr_of_mut!(SEEN)).first_time(pc) };
    if first {
        match payload {
            Some(p) => kprintln!("und: {} @PC={:#x} payload={:#010x}", name, pc, p),
            None => kprintln!("und: {} @PC={:#x}", name, pc),
        }
    }
}

fn log_debugger_und(pc: u32, msg_start: u32, msg_end: u32) {
    // Dedup by PC: each DebuggerUND site in the ROM is a distinct panic
    // message (e.g. "_stack_overflow called - panic!", "Undefined SWI",
    // "SWI from non-user mode (rebooting)"), and the first time the guest
    // hits any one of them tells us something specific about where we've
    // diverged. There are ~22 sites across ROM + REx, so an unfiltered
    // log of first-hits isn't noisy. Repeated hits at the same PC are
    // suppressed.
    static mut SEEN: SeenSet<u32, 32> = SeenSet::new(0);
    // SAFETY: single-core EL2; see diag_util module docs.
    let first = unsafe { (*addr_of_mut!(SEEN)).first_time(pc) };
    if first {
        // Extract the string (first up to 120 bytes) for the log.
        // See scan_to_null_word_aligned for why we iterate bytes in
        // BE order — the ROM's strings are laid out that way within
        // each 32-bit word on an LE host.
        let mut buf = [0u8; 120];
        let mut n = 0;
        let mut va = msg_start;
        'outer: while n < buf.len() && va < msg_end {
            let w = match read_guest_word_pa(va) {
                Some(v) => v,
                None => break,
            };
            for byte in w.to_be_bytes() {
                if byte == 0 { break 'outer; }
                buf[n] = byte;
                n += 1;
                if n >= buf.len() { break 'outer; }
            }
            va = va.wrapping_add(4);
        }
        let s = core::str::from_utf8(&buf[..n]).unwrap_or("<bad utf-8>");
        kprintln!(
            "und: DebuggerUND @PC={:#x} msg={:?} (resume at PC={:#x})",
            pc, s, msg_end
        );
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
