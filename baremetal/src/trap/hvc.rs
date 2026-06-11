//! HVC (EC=0x12) tag dispatch: the guest-test ABI, the ROM-probe
//! immediates, and the diagnostic / breakpoint tags.

use crate::{cpu, hvc_imm::HvcImm, peripherals::vic};
use crate::trap_context::{read_sysreg, TrapContext};
use crate::kprintln;
use super::dabt::handle_dabt_dispatch;
use super::diag::{handle_diag, handle_loud_halt, handle_unhandled_exception};
use super::und::handle_und;
use crate::probes::{ThunkKind, handle_bootos_canary, handle_dah_mrs_spsr_patch, handle_hammer_print, handle_hammer_thunk, handle_remember_swiret_probe};
#[cfg(feature = "log_store")]
use crate::probes::{handle_load_perm_obj_ret_probe, handle_store_perm_obj_entry_probe};
use crate::guest_mem::log_guest_string;


pub(crate) fn handle_hvc(ctx: &mut TrapContext, iss: u32) {
    // Guest-test protocol — see baremetal/guest-tests/README.md.
    let imm = iss & 0xFFFF;
    let r0 = ctx.x[0] as u32;
    // Per-imm HVC histogram. See `crate::trap_hist`.
    crate::trap_hist::record_hvc(imm);
    match imm {
        v if v == HvcImm::GuestTestPrintByte as u32 => {
            let b = r0 as u8;
            if b == b'\n' { crate::uart::write_byte(b'\r'); }
            crate::uart::write_byte(b);
        }
        v if v == HvcImm::GuestTestPrintHex as u32 => {
            kprintln!("guest-hex: {:#010x}", r0);
        }
        v if v == HvcImm::GuestTestPass as u32 => {
            kprintln!();
            kprintln!("*** guest test PASSED (r0={:#x}) ***", r0);
            cpu::halt();
        }
        v if v == HvcImm::GuestTestFail as u32 => {
            kprintln!();
            kprintln!("*** guest test FAILED (code={:#x}) ***", r0);
            cpu::halt();
        }
        v if v == HvcImm::GuestMark as u32 => {
            kprintln!("guest-mark: {:#010x}", r0);
        }
        v if v == HvcImm::DebugStr as u32 => {
            // DebugStr ROM-patch trap: the ROM-patched stub does
            // `MOV r7, LR` before this HVC so we can read LR without
            // relying on AArch64 banked-register accesses (MRS LR_svc
            // is unimplemented on QEMU raspi3b's Cortex-A53 model).
            // r0 is the guest's string pointer; we log it and resume
            // at LR + 4, matching Einstein's callback
            // (Emulator/JIT/Generic/TJITGenericROMPatch.cpp:76).
            let addr = r0;
            log_guest_string("DebugStr", addr);
            let lr = ctx.x[7] as u32;
            // SAFETY: ELR_EL2 controls the post-ERET guest PC.
            unsafe {
                core::arch::asm!(
                    "msr elr_el2, {}",
                    in(reg) lr.wrapping_add(4) as u64,
                    options(nostack, preserves_flags),
                );
            }
            return;
        }
        v if v == HvcImm::Debugger as u32 => {
            // Debugger ROM-patch trap. Stub stashed LR into r7 for the
            // same reason as DebugStr above. Einstein's callback breaks
            // into the host debugger and returns PC = LR + 8
            // (TJITGenericROMPatch.cpp:96); we have no host debugger,
            // so log the site and continue.
            let elr = read_sysreg!("elr_el2");
            kprintln!("Debugger trap @ELR={:#x}", elr);
            let lr = ctx.x[7] as u32;
            unsafe {
                core::arch::asm!(
                    "msr elr_el2, {}",
                    in(reg) lr.wrapping_add(8) as u64,
                    options(nostack, preserves_flags),
                );
            }
            return;
        }
        v if v == HvcImm::GuestInjectPen as u32 => {
            // r0 = packed sample word, r1 = ticks. Enqueue directly,
            // bypassing the backend (which would otherwise insert
            // pen-down/up edge markers based on its own state).
            let sample = ctx.x[0] as u32;
            let ticks = ctx.x[1] as u32;
            crate::host_io::queue::enqueue_pen_sample(sample, ticks);
        }
        v if v == HvcImm::Snapshot as u32 => {
            // Save snapshot — see src/snapshot.rs. ctx.x[0..30] is
            // the AArch64 GPR view that aliases AArch32 R0..R12 plus
            // every banked SP/LR per ARM ARM Table D1-79; ELR_EL2 /
            // SPSR_EL2 give the PC and CPSR to resume at.
            let mut gprs = [0u64; 31];
            for i in 0..31 {
                gprs[i] = ctx.x[i];
            }
            if let Err(e) = crate::snapshot::save(&gprs) {
                kprintln!("snapshot: save failed: {}", e);
            }
        }
        v if v == HvcImm::TaskDump as u32 => {
            // Full kernel-state dump on demand. Issued from a guest
            // ROM patch at well-chosen PCs (e.g. just before a
            // suspected stall, or right after Init__5TTask of a task
            // we want to trace) to capture scheduler + ports +
            // monitors in one shot.
            crate::task_dump::dump_full();
        }
        v if v == HvcImm::DumpObjectById as u32 => {
            // Dump one kernel object by id. Guest puts the id in r0.
            let id = ctx.x[0] as u32;
            kprintln!("=== HVC dump_object_by_id({:#x}) ===", id);
            crate::task_dump::dump_object_by_id(id);
        }
        v if v == HvcImm::LoudHalt as u32 => {
            handle_loud_halt(ctx);
        }
        v if v == HvcImm::BootOs as u32 => {
            handle_bootos_canary(ctx);
        }
        v if v == HvcImm::RememberSwiret as u32 => {
            handle_remember_swiret_probe(ctx);
        }
        v if v == HvcImm::DahMrsSpsr as u32 => {
            handle_dah_mrs_spsr_patch(ctx);
        }
        #[cfg(feature = "log_store")]
        v if v == HvcImm::StorePermObjEntry as u32 => {
            handle_store_perm_obj_entry_probe(ctx);
            // Emulate the patched-out `mov ip, sp` (R12 = SP for
            // the source AArch32 mode). HVC entry already advanced
            // ELR_EL2 past the trap, so no ELR adjustment needed.
            let spsr_el2 = read_sysreg!("spsr_el2") as u32;
            ctx.x[12] = crate::banked::sp_for_mode(ctx, spsr_el2) as u64;
        }
        #[cfg(feature = "log_store")]
        v if v == HvcImm::LoadPermObjRet as u32 => {
            handle_load_perm_obj_ret_probe(ctx);
            // Emulate the patched-out `mov r0, r4`. R0/R4 are not
            // banked across modes, so a direct GPR copy is correct
            // regardless of source mode.
            ctx.x[0] = ctx.x[4];
        }
        v if v == HvcImm::UnhandledException as u32 => {
            handle_unhandled_exception(ctx, false);
        }
        v if v == HvcImm::UnhandledNumException as u32 => {
            handle_unhandled_exception(ctx, true);
        }
        v if v == HvcImm::HammerPrint as u32 => {
            handle_hammer_print(ctx);
        }
        v if v == HvcImm::HammerPutc as u32 => {
            handle_hammer_thunk(ctx, ThunkKind::Putc);
        }
        v if v == HvcImm::HammerFlush as u32 => {
            handle_hammer_thunk(ctx, ThunkKind::Flush);
        }
        v if v == HvcImm::HammerStackTrace as u32 => {
            handle_hammer_thunk(ctx, ThunkKind::StackTrace);
        }
        v if v == HvcImm::HammerExceptionNotify as u32 => {
            handle_hammer_thunk(ctx, ThunkKind::ExceptionNotify);
        }
        v if v == HvcImm::Und as u32 => {
            handle_und(ctx);
        }
        v if v == HvcImm::Diag as u32 => {
            handle_diag(ctx);
        }
        v if v == HvcImm::DabtDispatch as u32 => {
            handle_dabt_dispatch(ctx);
        }
        v if v == HvcImm::Align as u32 => {
            crate::unaligned::handle_align_fault(ctx);
        }
        v if v == HvcImm::GpioTrigger as u32 => {
            vic::raise(vic::INT_GPIO);
        }
        #[cfg(feature = "trace")]
        v if v == HvcImm::Trace as u32 => {
            crate::tracer::handle_trace_hvc(ctx);
        }
        _ => {
            let elr = read_sysreg!("elr_el2");
            kprintln!();
            kprintln!("*** unknown HVC #{:#x} at ELR={:#x} (halting)", imm, elr);
            cpu::halt();
        }
    }
    // No ELR advance needed: HVC entry sets ELR_EL2 to the PC of the
    // instruction after the HVC (DDI 0487 G1.11.1 "HVC from AArch32"),
    // so ERET returns to the guest's next instruction as-is.
}
