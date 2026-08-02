//! HVC (EC=0x12) tag dispatch: the guest-test ABI, the ROM-probe
//! immediates, and the diagnostic / breakpoint tags.

use crate::{arch::cpu, hv::hvc_imm::HvcImm};
use crate::arch::trap_context::{read_sysreg, TrapContext};
use crate::kprintln;
use crate::diag::trap_diag::{handle_diag, handle_loud_halt, handle_unhandled_exception};
use crate::hv::hooks::{ActiveGuest, GuestOs};
use super::und::handle_und;
use crate::hv::guest_mem::log_guest_string;


/// Pen-sample sink for the `GuestInjectPen` test HVC, installed by
/// `main.rs` boot wiring (`host::host_io::queue::enqueue_pen_sample`)
/// so this hv-layer dispatcher stays free of host imports. Raw fn
/// pointer, 0 = uninstalled — [`pen_inject`] halts loudly on use
/// before wiring.
static PEN_INJECT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the pen-sample sink. Called once from `main.rs`.
pub(crate) fn install_pen_inject(sink: fn(u32, u32)) {
    PEN_INJECT.store(sink as usize, core::sync::atomic::Ordering::Release);
}

fn pen_inject() -> fn(u32, u32) {
    let raw = PEN_INJECT.load(core::sync::atomic::Ordering::Acquire);
    if raw == 0 {
        kprintln!(
            "*** hvc: no pen-inject sink — main.rs must install_pen_inject() before use ***"
        );
        cpu::halt();
    }
    // SAFETY: the only writer is install_pen_inject, which stores a
    // valid `fn(u32, u32)`; 0 is filtered above.
    unsafe { core::mem::transmute(raw) }
}

pub(crate) fn handle_hvc(ctx: &mut TrapContext, iss: u32) {
    // Guest-test protocol — see baremetal/guest-tests/README.md.
    let imm = iss & 0xFFFF;
    let r0 = ctx.x[0] as u32;
    // Per-imm HVC histogram. See `crate::diag::trap_hist`.
    crate::diag::trap_hist::record_hvc(imm);
    match imm {
        v if v == HvcImm::GuestTestPrintByte as u32 => {
            let b = r0 as u8;
            if b == b'\n' { crate::raw_wire_byte!(b'\r'); }
            crate::raw_wire_byte!(b);
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
            // r0 = packed sample word, r1 = ticks. Enqueue through the
            // installed sink directly onto the host pen queue,
            // bypassing the backend (which would otherwise insert
            // pen-down/up edge markers based on its own state).
            let sample = ctx.x[0] as u32;
            let ticks = ctx.x[1] as u32;
            (pen_inject())(sample, ticks);
        }
        v if v == HvcImm::Snapshot as u32 => {
            // Save snapshot — see src/hv/snapshot.rs. ctx.x[0..30] is
            // the AArch64 GPR view that aliases AArch32 R0..R12 plus
            // every banked SP/LR per ARM ARM Table D1-79; ELR_EL2 /
            // SPSR_EL2 give the PC and CPSR to resume at.
            let mut gprs = [0u64; 31];
            for i in 0..31 {
                gprs[i] = ctx.x[i];
            }
            if let Err(e) = crate::hv::snapshot::save(&gprs) {
                kprintln!("snapshot: save failed: {}", e);
            }
        }
        v if v == HvcImm::TaskDump as u32 => {
            // Full kernel-state dump on demand. Issued from a guest
            // ROM patch at well-chosen PCs (e.g. just before a
            // suspected stall, or right after Init__5TTask of a task
            // we want to trace) to capture scheduler + ports +
            // monitors in one shot.
            crate::diag::task_dump::dump_full();
        }
        v if v == HvcImm::DumpObjectById as u32 => {
            // Dump one kernel object by id. Guest puts the id in r0.
            let id = ctx.x[0] as u32;
            kprintln!("=== HVC dump_object_by_id({:#x}) ===", id);
            crate::diag::task_dump::dump_object_by_id(id);
        }
        v if v == HvcImm::LoudHalt as u32 => {
            handle_loud_halt(ctx);
        }
        v if v == HvcImm::UnhandledException as u32 => {
            handle_unhandled_exception(ctx, false);
        }
        v if v == HvcImm::UnhandledNumException as u32 => {
            handle_unhandled_exception(ctx, true);
        }
        v if v == HvcImm::Und as u32 => {
            handle_und(ctx);
        }
        v if v == HvcImm::Diag as u32 => {
            handle_diag(ctx);
        }
        v if v == HvcImm::DabtDispatch as u32 => {
            ActiveGuest::handle_dabt_dispatch(ctx);
        }
        v if v == HvcImm::Align as u32 => {
            ActiveGuest::handle_align_fault(ctx);
        }
        #[cfg(nh_guest_test)]
        v if v == HvcImm::GuestTestRepRender as u32 => {
            // Test-only: render a format string through the production
            // `rep_print` interpreter into a guest-supplied buffer and
            // return the rendered length in r0, so `test_rep_print.S`
            // can byte-assert the VaArgs/specifier ABI guest-side.
            //
            //   r0 = format string ptr   r1 = out buffer ptr
            //   r2 = vararg0  r3 = vararg1  [SP+0..] = vararg2..
            //
            // The render writes bytes directly into guest RAM at r1 via
            // the BE-8 store path. VaArgs pulls r2/r3 then walks the
            // source-mode (SVC) stack, exactly as the Hammer Print hook
            // does on a real boot.
            let fmt_ptr = ctx.x[0] as u32;
            let out_ptr = ctx.x[1] as u32;
            let r2 = ctx.x[2] as u32;
            let r3 = ctx.x[3] as u32;
            let spsr_el2 = read_sysreg!("spsr_el2") as u32;
            let sp = crate::arch::banked::sp_for_mode(ctx, spsr_el2);
            let mut buf = [0u8; 512];
            let n = crate::diag::rep_print::render_into(
                &mut buf,
                fmt_ptr,
                crate::diag::rep_print::VaArgs::new(r2, r3, sp),
            );
            for (i, &b) in buf[..n].iter().enumerate() {
                if !crate::hv::guest_endian::guest_write_u8_va(out_ptr.wrapping_add(i as u32), b) {
                    kprintln!(
                        "*** GuestTestRepRender: guest_write_u8_va failed at out+{} ***",
                        i
                    );
                    cpu::halt();
                }
            }
            ctx.x[0] = n as u64;
        }
        #[cfg(feature = "trace")]
        v if v == HvcImm::Trace as u32 => {
            crate::diag::tracer::handle_trace_hvc(ctx);
        }
        _ => {
            // ROM-probe tags (probe bodies, Hammer thunks, GPIO test
            // trigger) are guest-OS-specific — consult the hook before
            // declaring the immediate unknown.
            if !ActiveGuest::handle_hvc_probe(ctx, imm) {
                let elr = read_sysreg!("elr_el2");
                kprintln!();
                kprintln!("*** unknown HVC #{:#x} at ELR={:#x} (halting)", imm, elr);
                cpu::halt();
            }
        }
    }
    // No ELR advance needed: HVC entry sets ELR_EL2 to the PC of the
    // instruction after the HVC (DDI 0487 G1.11.1 "HVC from AArch32"),
    // so ERET returns to the guest's next instruction as-is.
}
