//! ROM-probe handler bodies — the receiving end of the patch sites in
//! `rom_patches.rs` (Hammer print/thunk, StorePermObj, the BootOS
//! reboot canary, and the DAH MRS-SPSR rewrite).

use crate::arch::cpu;
use crate::arch::trap_context::{read_sysreg, TrapContext};
use crate::kprintln;
use crate::hv::trap::und::read_banked_spsr;
use crate::hv::guest_mem::read_cstr_at;


/// Canary handler for `BootOS` / `ROMBoot` (0x0001_8688). The AArch32
/// reset vector at VA 0 branches here, so the first entry after the
/// hypervisor ERETs the guest is legitimate — we emulate the original
/// first instruction (`mov r0, #0xb0`) and advance ELR so the kernel
/// continues. Every SUBSEQUENT entry is a software reset (watchdog,
/// `Reboot`, `PowerOffAndReboot`, or a direct jump to the reset
/// vector); we dump state and halt. Complements the already-canaried
/// `Reboot` / `PowerOffAndReboot` entry points by catching reset
/// paths that bypass them.
pub(crate) fn handle_bootos_canary(ctx: &mut TrapContext) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static ENTRIES: AtomicU32 = AtomicU32::new(0);
    let n = ENTRIES.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 {
        // First boot. Emulate `mov r0, #0xb0` (the word we overwrote
        // with the HVC) and ERET to BootOS + 4 so the kernel runs
        // through its normal boot sequence.
        ctx.x[0] = 0xb0;
        let next_pc = (crate::newton::rom_patches::BOOTOS_PC + 4) as u64;
        // SAFETY: ELR_EL2 controls the post-ERET guest PC.
        unsafe {
            core::arch::asm!(
                "msr elr_el2, {}",
                in(reg) next_pc,
                options(nostack, preserves_flags),
            );
        }
        kprintln!("BootOS canary: first boot — emulated mov r0,#0xb0 and passing through");
        return;
    }

    // Second+ entry — software reset.
    // Close any open tarmac-window capture before further EL2 work runs
    // (the halt message itself would otherwise appear in the trace).
    #[cfg(feature = "platform-fvp-base")]
    crate::host::tarmac::emit_stop();
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    let elr_el2 = read_sysreg!("elr_el2");
    let mode = spsr_el2 & 0x1F;
    let caller_lr = crate::arch::banked::lr_for_mode(ctx, spsr_el2);
    kprintln!();
    kprintln!("*** BootOS canary fired on entry #{} — software reset detected ***", n);
    kprintln!(
        "  ELR_EL2  = {:#010x}  (= BootOS entry PC)",
        elr_el2
    );
    kprintln!(
        "  SPSR_EL2 = {:#010x}  mode={} ({:#x})",
        spsr_el2, crate::arch::arm_decode::aarch32_mode_name(mode), mode
    );
    kprintln!(
        "  R0 = {:#010x}  R1 = {:#010x}  R2 = {:#010x}  R3 = {:#010x}",
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
    );
    kprintln!(
        "  R12={:#010x}  R14_{}={:#010x}  (caller LR via Table D1-79)",
        ctx.x[12] as u32, crate::arch::arm_decode::aarch32_mode_name(mode), caller_lr
    );
    kprintln!();
    kprintln!(
        "  Preceding tracer entries show what the kernel was doing before"
    );
    kprintln!(
        "  the reset. Common triggers: watchdog timeout, Reboot() / "
    );
    kprintln!(
        "  PowerOffAndReboot (separately canaried), or a direct jump to"
    );
    kprintln!(
        "  the reset vector at VA 0."
    );
    cpu::halt();
}

/// Which `PHammerOutTranslator` body patch fired.
#[derive(Clone, Copy)]
pub(crate) enum ThunkKind {
    Putc,
    Flush,
    StackTrace,
    ExceptionNotify,
}

/// Hook at `PHammerOutTranslator::Print` body entry (ROM 0x000E_6A90).
/// The body's `mov ip, sp` prologue has been replaced with HVC; after
/// HVC returns ELR advances by 4 and the patched `mov r0, #0` +
/// `mov pc, lr` tail returns 0 to the caller. We just render args.
///
/// Args follow standard ARM EABI varargs (post-thunk this-adjustment):
///   r0 = (this — ignored by us, overwritten by the patch tail)
///   r1 = format string (const char*)
///   r2 = arg0   r3 = arg1   [sp+0..]+ = arg2..
///
/// The renderer's `VaArgs` pulls args from r2/r3 then walks the
/// source-mode stack.
pub(crate) fn handle_hammer_print(ctx: &mut TrapContext) {
    let spsr_el2 = read_sysreg!("spsr_el2") as u32;
    handle_hammer_print_with(ctx, spsr_el2);
}

pub(crate) fn handle_hammer_print_with(ctx: &mut TrapContext, source_cpsr: u32) {
    let r1 = ctx.x[1] as u32;
    let r2 = ctx.x[2] as u32;
    let r3 = ctx.x[3] as u32;
    let sp = crate::arch::banked::sp_for_mode(ctx, source_cpsr);

    crate::diag::rep_print::render_and_log(
        "REP> ",
        r1,
        crate::diag::rep_print::VaArgs::new(r2, r3, sp),
    );
}

/// Unified handler for `PHammerOutTranslator::{Putc, Flush, StackTrace,
/// ExceptionNotify}` body patches. Putc/Flush bodies are fully replaced
/// (return 0 via the patched tail). StackTrace/ExceptionNotify have
/// only their first word patched (replacing `mov r0, r1`); the
/// untouched second word is `b REPStackTrace`/`b REPExceptionNotify`
/// and runs natively after HVC, so we emulate the displaced
/// `mov r0, r1` here.
pub(crate) fn handle_hammer_thunk(ctx: &mut TrapContext, kind: ThunkKind) {
    let r0 = ctx.x[0] as u32;
    let r1 = ctx.x[1] as u32;
    match kind {
        ThunkKind::Putc => {
            // Route the byte through the same line buffer Print uses
            // so a stream of Putc calls renders as one UART line per
            // newline-terminated fragment.
            crate::diag::rep_print::putc("REP> ", (r1 & 0xFF) as u8);
        }
        ThunkKind::Flush => {
            crate::diag::rep_print::flush_line("REP> ");
        }
        ThunkKind::StackTrace => {
            crate::diag::rep_print::flush_line("REP> ");
            kprintln!(
                "REP> [StackTrace(translator={:#010x}, arg={:#010x})]",
                r0, r1,
            );
            // Emulate the displaced `mov r0, r1` so the natively-
            // executing `b REPStackTrace` at the next word sees
            // r0 = stack-frame pointer (its first arg).
            ctx.x[0] = ctx.x[1];
        }
        ThunkKind::ExceptionNotify => {
            // r1 = Exception*; *r1 = name C-string ptr.
            let name_ptr = crate::hv::guest_endian::guest_read_u32_va(r1).unwrap_or(0);
            let (buf, len) = read_cstr_at(name_ptr, 80);
            let name = core::str::from_utf8(&buf[..len]).unwrap_or("<non-utf8>");
            crate::diag::rep_print::flush_line("REP> ");
            kprintln!(
                "REP> [ExceptionNotify(translator={:#010x}, ex={:#010x}) name={:?}]",
                r0, r1, name,
            );
            // Emulate the displaced `mov r0, r1` so the natively-
            // executing `b REPExceptionNotify` sees r0 = Exception*.
            ctx.x[0] = ctx.x[1];
        }
    }
}

// ---- Remember post-SWI fixup (load-bearing, not a probe) ----
//
// Re-establishes the kernel's `r8 = -10003` sentinel after the SWI
// return inside `TUDomainManager::Remember (static)`. The SWI dispatch
// in the host clobbers r8 in some paths; without this fixup the
// following `teq` at 0x00258E58 misbehaves and the kernel's monitor
// retry path doesn't engage. See `src/newton/rom_patches.rs::apply_l1_cd_probes`.

pub(crate) fn handle_remember_swiret_probe(ctx: &mut TrapContext) {
    // Emulate `mov r8, #237`. Together with the next ROM instruction
    // `sub r8, r8, #10240` this materialises r8 = -10003 (the kernel's
    // sentinel value loaded after the SWI return).
    ctx.x[8] = 237;
}

pub(crate) fn handle_dah_mrs_spsr_patch(ctx: &mut TrapContext) {
    // The saved SPSR_abt replaces the guest's r1 — a fabricated value
    // here would silently corrupt the kernel's abort-mode decode, so an
    // unreadable slot is a halt, not a default.
    let spsr_abt_save = match crate::hv::guest_endian::guest_read_u32_pa(
        crate::newton::guest_trampolines::DABT_SAVE_PA + 8,
    ) {
        Some(v) => v,
        None => {
            kprintln!(
                "*** handle_dah_mrs_spsr_patch: DABT_SAVE SPSR slot @{:#x} unreadable \
                 (ELR_EL2={:#x}) ***",
                crate::newton::guest_trampolines::DABT_SAVE_PA + 8, read_sysreg!("elr_el2"),
            );
            cpu::halt();
        }
    };
    let lr_abt_save = crate::hv::guest_endian::guest_read_u32_pa(
        crate::newton::guest_trampolines::DABT_SAVE_PA,
    ).unwrap_or(0);
    let r1_in = ctx.x[1] as u32;
    let far = read_sysreg!("far_el1") as u32;
    // Cross-check: also read `mrs spsr_abt` from EL2. If it disagrees
    // with the saved slot, that's the documented QEMU staleness. We
    // always use the saved-slot value (architecturally correct on
    // every platform).
    let mrs_view = read_banked_spsr("abt") as u32;
    // Replace r1 with the trampoline-saved SPSR_abt. Natural ERET
    // resumes at the post-HVC PC (= 0x393148, the kernel's
    // `and r1, r1, #31`).
    ctx.x[1] = (ctx.x[1] & 0xFFFF_FFFF_0000_0000)
        | (spsr_abt_save as u64);
    static FIRED: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(0);
    let n = FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 16 {
        // lr_abt_save here is the original faulting PC + 8 (the slow
        // trampoline doesn't subtract; the kernel's `sub lr, lr, #8`
        // at DAH entry runs *after* the trampoline saves it). The
        // fast trampoline (iter-105) saves it at the same offset
        // pre-DAH-entry, so the value is `faulting_PC + 8` on both
        // paths.
        kprintln!(
            "DAH-mrs-patch[{}]: r1_in={:#010x} mrs={:#010x} saved-slot={:#010x} \
             (pre-abt mode={:#x} = {}) faulting_PC={:#010x} FAR={:#010x}{}",
            n, r1_in, mrs_view, spsr_abt_save, spsr_abt_save & 0x1F,
            crate::arch::arm_decode::aarch32_mode_name(spsr_abt_save & 0x1F),
            lr_abt_save.wrapping_sub(8), far,
            if (mrs_view & 0x1F) != (spsr_abt_save & 0x1F) {
                "  *** MRS DIVERGES ***"
            } else { "" },
        );
    }
}

/// Probe handler for `StorePermObject` entry. R0 is a `RefArg`
/// (`typedef const RefVar& RefArg`) so it's a pointer to a
/// `RefVar`. RefVar is GC-tracked: its first field is a slot
/// pointer (into the rooted-Refs array), and the Ref itself lives
/// at that slot. Two loads — confirmed against `IsString` /
/// `IsFrame` at 0x0031_9874 / 0x0031_9990 which both do
/// `ldr r0, [r0]; ldr r0, [r0]` to fetch the Ref. Read both
/// indirections, log a counted header, and pretty-print the Ref
/// via `newton-objects`.
///
/// Caller is expected to emulate the patched-out `mov ip, sp` in
/// the surrounding dispatch arm (HVC- or UND-path) and advance
/// ELR; this handler only logs.
#[cfg(feature = "log_store")]
pub(crate) fn handle_store_perm_obj_entry_probe(ctx: &mut TrapContext) {
    use core::sync::atomic::{AtomicU32, Ordering};
    let refvar_ptr = ctx.x[0] as u32;
    let slot_ptr =
        crate::hv::guest_endian::guest_read_u32_va(refvar_ptr).unwrap_or(0);
    let ref_value = if slot_ptr != 0 {
        crate::hv::guest_endian::guest_read_u32_va(slot_ptr).unwrap_or(0)
    } else {
        0
    };
    static FIRED: AtomicU32 = AtomicU32::new(0);
    let n = FIRED.fetch_add(1, Ordering::Relaxed);
    let lr = ctx.x[14] as u32;
    let _ = (refvar_ptr, slot_ptr); // available for future detail
    crate::kprint!("StorePermObject[{}]: ", n);
    crate::diag::heap_check::pretty_print_ref_inline(ref_value, 2);
    kprintln!("  lr={:#x}", lr);
}

/// Probe handler for `LoadPermObject`'s return site. R4 holds the
/// Ref returned by `Read__18TStoreObjectReaderFv`; the patched-out
/// `mov r0, r4` is what propagates it into the function's return
/// register. Pretty-print R4 so we can compare what came out of
/// the flash store with what `StorePermObject` had put in.
///
/// Caller is expected to emulate `r0 = r4` and advance ELR.
#[cfg(feature = "log_store")]
pub(crate) fn handle_load_perm_obj_ret_probe(ctx: &mut TrapContext) {
    use core::sync::atomic::{AtomicU32, Ordering};
    let ref_value = ctx.x[4] as u32;
    static FIRED: AtomicU32 = AtomicU32::new(0);
    let n = FIRED.fetch_add(1, Ordering::Relaxed);
    let lr = ctx.x[14] as u32;
    crate::kprint!("LoadPermObject[{}]: ", n);
    crate::diag::heap_check::pretty_print_ref_inline(ref_value, 2);
    kprintln!("  lr={:#x}", lr);
}
