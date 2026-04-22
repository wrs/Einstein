//! EL2 synchronous trap dispatcher.
//!
//! The vector at offset 0x600 (lower-EL AArch32 sync) saves the full x0..x30
//! context, hands us a `*mut TrapContext`, and we dispatch based on ESR_EL2.EC.
//!
//! Handlers that emulate a guest instruction and want to resume mutate the
//! context in place, advance ELR_EL2 past the faulting instruction, then
//! return — the vector trailer restores the context and ERETs. Handlers that
//! don't want to resume never return (they call `cpu::halt`).

use crate::{cpu, guest_mem, kprintln, mmio, peripherals::{native_primitives, vic}, shadow_stub, timer};

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

/// Records the PA where `handle_diag`'s stub deposited its banked-reg
/// dump, so `handle_diag_lr` knows where to read from.
static LR_SAVE_PA_RECORD: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

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
    static mut TRAP_LOG_BUDGET: usize = 500;
    // SAFETY: single-threaded; only core 0 services EL2 traps.
    let should_log = unsafe {
        let go = TRAP_LOG_BUDGET > 0;
        if go { TRAP_LOG_BUDGET -= 1; }
        go
    };
    if should_log {
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "trap: EC={:#x} ({}) ELR={:#x} ESR={:#x}",
            ec, describe_ec(ec), elr, esr
        );
    }

    match ec {
        EC_DATA_ABORT_LOWER => handle_data_abort(ctx, iss),
        EC_INSN_ABORT_LOWER => handle_instruction_abort(ctx, iss),
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
    let should_log = unsafe {
        let go = elr != HB_LAST_PC && HB_PRINT_BUDGET > 0;
        if go { HB_LAST_PC = elr; HB_PRINT_BUDGET -= 1; }
        go
    };
    if should_log {
        let spsr = read_sysreg!("spsr_el2");
        let far = read_sysreg!("far_el1");
        kprintln!("timer_irq: guest ELR={:#x} SPSR={:#x} FAR_EL1={:#x}", elr, spsr, far);
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

    let elr = read_sysreg!("elr_el2") as u32;

    // Phase B diagnostic: log any access from inside the REx-scanner
    // function range with full register context, to understand what
    // addresses it's probing (for pre-MMU first boot).
    if (0x003137dc..0x00313960).contains(&elr) {
        kprintln!(
            "rex-dabt: ELR={:#010x} {} IPA={:#x} FAR={:#x}  r0={:#x} r1={:#x} r2={:#x} r3={:#x} r4={:#x}",
            elr,
            if wnr { "W" } else { "R" },
            ipa, far,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32, ctx.x[4] as u32
        );
    }

    // Shadow-stub abort transparency: if the aborting PC is inside the
    // stub pool, the guest just hit a fault on the real LDRB/STRB/...
    // that the stub is executing on its behalf. Redirect the abort so
    // the guest's own DABT handler sees it at the original PC with
    // an un-XOR'd FAR, exactly as if the site had never been patched.
    if shadow_stub::is_stub_ipa(elr) {
        inject_shadow_stub_abort(ctx, iss, far, elr);
        return;
    }

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

    // Before dispatching an "unknown IPA" write to the MMIO halt path,
    // dump the caller context. Cheap enough (runs once, then halt) and
    // decisive for diagnosing MCR-then-STR patterns where the faulting
    // instruction is in a tight helper far from where the bad address
    // was computed. The check mirrors the regions mmio::write would
    // silently accept — anything outside an MMIO window AND outside
    // the stage-2 RW RAM/flash/FB blocks is obviously unreachable.
    if wnr && is_obviously_unreachable_ipa(ipa) {
        let spsr = read_sysreg!("spsr_el2");
        let mode = (spsr as u32) & 0x1F;
        let mode_label = aarch32_mode_label(mode);
        kprintln!(
            "dabt-trip: PC={:#010x} mode={} writing {:#010x} -> IPA={:#x}",
            elr, mode_label, ctx.x[srt] as u32, ipa
        );
        kprintln!(
            "           r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
        );
        kprintln!(
            "           r4={:#010x} r5={:#010x} r6={:#010x} r7={:#010x}",
            ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32
        );
        kprintln!(
            "           r8={:#010x} r9={:#010x} r10={:#010x} r11={:#010x}",
            ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32
        );
        kprintln!(
            "           r12={:#010x} sp={:#010x} lr={:#010x}",
            ctx.x[12] as u32, ctx.x[13] as u32, ctx.x[14] as u32
        );
    }

    if wnr {
        let value = ctx.x[srt] as u32;
        mmio::write(ipa, sas, value as u32, elr as u64);
    } else {
        let value = mmio::read(ipa, sas, elr as u64);
        // Sign-extension (SSE) is ignored for stub reads — everything we
        // return here is either zero or a known non-negative constant.
        ctx.x[srt] = value as u64;
    }

    // Advance past the 32-bit ARM instruction that faulted.
    advance_elr(4);
}

/// IPA ranges that the stage-2 map intentionally leaves as fault /
/// read-only and that no peripheral module owns. A write here is
/// almost certainly a wild pointer — worth dumping context before
/// halting.
fn is_obviously_unreachable_ipa(ipa: u64) -> bool {
    // Inside ROM (stage-2 RO). Any write is doomed.
    if ipa < 0x0100_0000 { return true; }
    false
}

fn aarch32_mode_label(mode: u32) -> &'static str {
    match mode {
        0x10 => "usr",
        0x11 => "fiq",
        0x12 => "irq",
        0x13 => "svc",
        0x17 => "abt",
        0x1B => "und",
        0x1F => "sys",
        _    => "???",
    }
}

/// Handle a data abort whose ELR_EL2 is inside the shadow-stub pool.
///
/// Steps:
///   1. Identify the stub slot and its original guest PC.
///   2. If the aborting PC is NOT at the slot's "access offset" (the
///      real LDRB/STRB inside the stub), halt loudly — we shouldn't
///      be faulting anywhere else inside a stub.
///   3. Compute the un-XOR'd guest VA by XOR'ing FAR_EL2 with the
///      stub's xor_mask. That's the address the guest thinks it
///      accessed.
///   4. Write FAR_EL1 (= DFAR from AArch32's view) to the un-XOR'd VA
///      so the guest's abort handler sees its expected address.
///   5. Set ELR_EL2 to the guest's DABT vector (0x10, or 0xFFFF0010
///      if the guest uses high vectors), switch SPSR_EL2 to AArch32
///      ABT mode with IRQs/FIQs masked, and set ctx.x[14] so that
///      after ERET LR_abt = original_pc + 8 — the exact state the
///      guest would have if the faulting instruction had been at
///      the original site and never been patched.
fn inject_shadow_stub_abort(
    ctx: &mut TrapContext, iss: u32, far: u64, elr: u32,
) {
    let (slot, off) = match shadow_stub::ipa_to_slot_offset(elr) {
        Some(v) => v,
        None => {
            kprintln!("*** shadow_stub abort: ELR {:#x} not in pool (impossible)", elr);
            cpu::halt();
        }
    };

    let access_off = shadow_stub::slot_access_offset(slot);
    let original_pc = shadow_stub::slot_original_pc(slot);
    let xor_mask = shadow_stub::slot_xor_mask(slot).unwrap_or(0);

    let original_pc = match original_pc {
        Some(pc) => pc,
        None => {
            kprintln!("*** shadow_stub abort: slot {} has no original PC", slot);
            cpu::halt();
        }
    };

    // Expected abort offset is the inner access instruction. SWPB
    // uses a 2-insn LDRB/STRB pair so both are "the access" — accept
    // `access_off` or `access_off + 4`.
    let expected = match access_off {
        Some(off) => off,
        None => {
            kprintln!("*** shadow_stub abort: slot {} has no access_off", slot);
            cpu::halt();
        }
    };
    if off != expected && off != expected + 4 {
        kprintln!();
        kprintln!("*** shadow_stub abort at UNEXPECTED stub PC ***");
        kprintln!(
            "  ELR={:#x}  slot={}  off={:#x} expected={:#x} (inner access)",
            elr, slot, off, expected
        );
        kprintln!("  original guest PC={:#x}", original_pc);
        kprintln!(
            "  (the faulting instruction in the stub should be the inner"
        );
        kprintln!(
            "   LDRB/STRB/... only. A fault on the save/restore/branch-back"
        );
        kprintln!(
            "   would indicate corrupt stub code or a broken stage-2 mapping.)"
        );
        cpu::halt();
    }

    // Guest-visible faulting VA: reverse the XOR the stub applied.
    // The stub only XORs addresses below XOR_LIMIT (MMIO pass-through);
    // addresses >= XOR_LIMIT reach the real access unchanged, so
    // FAR_EL2 already holds the guest-view address.
    let far_u32 = far as u32;
    let guest_far = if far_u32 >= shadow_stub::XOR_LIMIT {
        far_u32
    } else {
        far_u32 ^ xor_mask
    };

    // Determine the guest DABT vector. AArch32 lays it at
    //   VBAR_EL1 + 0x10     (base vectors)
    //   0xFFFF0000  + 0x10  (high vectors, SCTLR_EL1.V = 1)
    // VBAR_EL1 provides the base for the guest's low-vectors case.
    let sctlr_el1 = read_sysreg!("sctlr_el1");
    let high_vectors = (sctlr_el1 & (1 << 13)) != 0;
    let vbar_el1 = read_sysreg!("vbar_el1");
    let dabt_vector: u32 = if high_vectors {
        0xFFFF_0010
    } else {
        (vbar_el1 as u32).wrapping_add(0x10)
    };

    // AArch32 DABT semantics (ARM ARM B1.9.8): LR_abt = faulting_pc + 8.
    // On QEMU raspi3b, AArch64 ERET to AArch32 ABT mode does NOT
    // propagate x14 to the banked R14_abt reliably (same class of
    // issue documented for the UND/DIAG paths — banked register
    // plumbing across the AArch64<->AArch32 boundary is flaky).
    //
    // Workaround: route through an AArch32 trampoline installed in
    // guest RAM. ERET lands in the *source* mode (SVC / whatever
    // took the stub fault); the trampoline mode-switches to ABT
    // via `msr cpsr_xc` — an AArch32 mode switch doesn't touch any
    // bank's SP, so the guest's SP_abt (set by SetAbortStack) is
    // preserved. The trampoline then writes DFSR, sets R14_abt, and
    // branches to the real DABT vector. See `install_abt_trampoline`
    // for the layout. Note that we don't call ARM's hardware
    // exception-entry path, so SPSR_abt is NOT updated — the guest
    // handler sees its pre-existing SPSR_abt value. For Newton's
    // shadow-stub re-injection consumers that's acceptable (they
    // only read DFSR / DFAR / LR_abt). Document in-place.
    //
    // Build the DFSR value the handler should see. For a stub-stage-2
    // fault we model the fault as an external abort on the faulting
    // address: FS[3:0]=0x8 (external non-linefetch, section) per
    // ARMv5/v7 short-descriptor DFSR encoding, Domain=0, WnR carried
    // over from the original ISS. Bit 10 (FS[4]) is 0. That matches
    // what the CPU would have latched if the abort had hit the raw
    // IPA without the stub in the way.
    let _ = iss; // DFSR not propagated (see install_abt_trampoline)
    let target_lr = original_pc.wrapping_add(8);
    // Capture source CPSR for SPSR_abt before we overwrite spsr_el2.
    let source_cpsr = (read_sysreg!("spsr_el2") & 0xFFFF_FFFF) as u32;
    install_abt_trampoline(target_lr, dabt_vector, source_cpsr);
    // The trampoline runs in the source mode with the guest's
    // original ctx registers. The ERET writes ctx.x[13] into the
    // source mode's banked SP, which is the same bank the guest
    // already owns — no cross-bank clobber. We don't touch
    // ctx.x[13] here.
    let _ = ctx; // ctx values flow through the ERET into AArch32 as-is

    // SPSR_EL2 for the ERET: reuse the source CPSR (mode/flags as
    // the guest had them at stub-fault entry) — the trampoline will
    // switch to ABT mode itself with the right A/I/F once it's in
    // AArch32. We must keep AArch32 state (bit 4 in SPSR_EL2 = 1)
    // and strip the EL2/ARM64-specific PAN/UAO/IL/SS flag residue;
    // SPSR_EL2 at trap entry already carries the AArch32 view so
    // just forward it.
    let spsr_el2 = read_sysreg!("spsr_el2");

    // Write FAR_EL1 so the guest sees the un-XOR'd VA as DFAR.
    // SAFETY: sysreg write.
    unsafe {
        core::arch::asm!(
            "msr far_el1, {}",
            "isb",
            in(reg) guest_far as u64,
            options(nostack, preserves_flags),
        );
    }

    // Log once so the operator sees what happened. Subsequent fires
    // stay quiet to avoid flooding.
    static mut ABORT_LOG_BUDGET: usize = 4;
    // SAFETY: single-threaded.
    let log = unsafe {
        let ok = ABORT_LOG_BUDGET > 0;
        if ok { ABORT_LOG_BUDGET -= 1; }
        ok
    };
    if log {
        kprintln!(
            "shadow_stub: delivering DABT -> guest (original PC={:#x}, \
             stub ELR={:#x}, guest FAR={:#x}, iss={:#x}, \
             vbar_el1={:#x}, dabt_vector={:#x}, spsr={:#x})",
            original_pc, elr, guest_far, iss,
            vbar_el1, dabt_vector, spsr_el2
        );
    }

    // SAFETY: writing EL2 sysregs; on ERET the guest enters ABT mode
    // at the trampoline, which sets R14_abt from its literal pool
    // and branches to the real DABT vector.
    let elr_target = abt_trampoline_va() as u64;
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "msr spsr_el2, {spsr}",
            "isb",
            elr = in(reg) elr_target,
            spsr = in(reg) spsr_el2,
            options(nostack, preserves_flags),
        );
    }
}

/// IPA where we install the ABT-trampoline. Sits in the same 4 KiB
/// small-page as the UND save slot (see UND_SAVE_LR_IPA = 0x0400_5F00),
/// which the Newton kernel's stage-1 L1[0xC0] → L2[0x04] maps from
/// VA 0x0C00_4000..0x0C00_4FFF. Placing the trampoline inside that
/// page means we can hand ERET a VA that translates cleanly through
/// the guest's stage-1 in both modes:
///
///   MMU off (e.g. shadow_stub test): VA == IPA == 0x0400_5A00
///   MMU on  (Newton kernel):         VA 0x0C00_4A00 → IPA 0x0400_5A00
///
/// The ten-word trampoline body fits comfortably below the UND save
/// slots at 0x5F00. `abt_trampoline_va` picks the right view for the
/// current guest SCTLR.M bit.
const ABT_TRAMPOLINE_IPA: u32 = 0x0400_5A00;
const ABT_TRAMPOLINE_VA_MMU_ON: u32 = 0x0C00_4A00;

fn abt_trampoline_va() -> u32 {
    let sctlr = read_sysreg!("sctlr_el1") as u32;
    if (sctlr & 1) != 0 {
        ABT_TRAMPOLINE_VA_MMU_ON
    } else {
        ABT_TRAMPOLINE_IPA
    }
}

/// Write the AArch32 abort-injection trampoline into RAM.
///
/// Called from EL2 before every ERET that delivers a synthesized
/// DABT to the guest. The trampoline starts executing in the
/// guest's source mode (ERET preserves mode from SPSR_EL2), then:
///
///   1. Saves guest R0 to a scratch literal slot.
///   2. Switches CPSR to ABT mode via `msr cpsr_xc` — an AArch32
///      mode switch keeps each mode's banked SP intact, so
///      SP_abt (set by the guest's SetAbortStack) is preserved.
///      Loading the full CPSR via a register (not imm) lets us
///      set A=1, I=1, F=1 together (0x01D7 > imm8-rotatable).
///   3. Sets SPSR_abt = saved source CPSR so the guest handler's
///      MRS SPSR returns the pre-abort mode, matching hardware
///      DABT entry semantics (DDI 0406 §B1.8.13).
///   4. Restores R0 from the scratch slot.
///   5. Loads LR_abt = faulting_pc + 8 from the literal pool.
///   6. Branches to the real DABT vector via LDR PC.
///
/// **Known limitation** — we do NOT set DFSR. The AArch32 MCR that
/// would write it (`mcr p15,0,Rx,c5,c0,0`) is caught by our
/// HCR_EL2.TVM trap and the cp15 write-back path (`cp15::write_dfsr32`)
/// is a no-op because `DFSR32_EL2` itself UNDEFs on A53 under QEMU
/// raspi3b. A real-hardware fix would either (a) propagate the
/// write via DFSR32_EL2 from EL2, or (b) drop TVM around the
/// trampoline so the AArch32 MCR lands on physical DFSR directly.
/// For the shadow-stub test, the value of DFSR the handler reads
/// is indeterminate; the test only validates FAR, LR_abt, SP_abt,
/// and SPSR_abt. Document and move on.
///
/// Layout (8 words + 5 literals = 52 bytes at ABT_TRAMPOLINE_IPA):
///   +0x00: str r0, [pc, #0x28]    ; save guest r0 to +0x30
///   +0x04: ldr r0, [pc, #0x1C]    ; r0 = CPSR value (+0x28)
///   +0x08: msr cpsr_xc, r0        ; switch to ABT (SP_abt stays)
///   +0x0C: ldr r0, [pc, #0x18]    ; r0 = SPSR_abt value (+0x2C)
///   +0x10: msr spsr_xc, r0        ; SPSR_abt = source CPSR
///   +0x14: ldr r0, [pc, #0x18]    ; restore guest r0 from +0x30
///   +0x18: ldr lr, [pc, #0]       ; lr = LR_abt (+0x20)
///   +0x1C: ldr pc, [pc, #0]       ; pc = DABT vector (+0x24)
///   +0x20: <target_lr>
///   +0x24: <target_pc>
///   +0x28: 0x000001D7             ; CPSR: A=I=F=1, mode=ABT, ARM
///   +0x2C: <source_cpsr>          ; written to SPSR_abt
///   +0x30: <saved guest r0>
fn install_abt_trampoline(target_lr: u32, target_pc: u32, source_cpsr: u32) {
    // SAFETY: writing to guest RAM backing from EL2; stage-2 maps the
    // same page, so the guest sees these words after our icache sync.
    unsafe {
        let base = (guest_mem::ram_host_pa() as usize)
            + (ABT_TRAMPOLINE_IPA as usize - 0x0400_0000);
        core::ptr::write_volatile((base +  0) as *mut u32, 0xE58F_0028); // str r0, [pc, #0x28]
        core::ptr::write_volatile((base +  4) as *mut u32, 0xE59F_001C); // ldr r0, [pc, #0x1C]
        core::ptr::write_volatile((base +  8) as *mut u32, 0xE123_F000); // msr cpsr_xc, r0
        core::ptr::write_volatile((base + 12) as *mut u32, 0xE59F_0018); // ldr r0, [pc, #0x18]
        core::ptr::write_volatile((base + 16) as *mut u32, 0xE163_F000); // msr spsr_xc, r0
        core::ptr::write_volatile((base + 20) as *mut u32, 0xE59F_0018); // ldr r0, [pc, #0x18]
        core::ptr::write_volatile((base + 24) as *mut u32, 0xE59F_E000); // ldr lr, [pc, #0]
        core::ptr::write_volatile((base + 28) as *mut u32, 0xE59F_F000); // ldr pc, [pc, #0]
        core::ptr::write_volatile((base + 32) as *mut u32, target_lr);
        core::ptr::write_volatile((base + 36) as *mut u32, target_pc);
        core::ptr::write_volatile((base + 40) as *mut u32, 0x0000_01D7); // ABT mode CPSR
        core::ptr::write_volatile((base + 44) as *mut u32, source_cpsr);
        // +48: scratch slot for guest r0 save/restore.
        // DC CVAU + IC IVAU to publish. 52 bytes spans one cache line
        // on A53 (64-byte line); one flush covers it.
        core::arch::asm!(
            "dc cvau, {0}",
            "ic ivau, {0}",
            "dsb ish",
            "isb",
            in(reg) base as u64,
            options(nostack, preserves_flags),
        );
    }
}

fn handle_instruction_abort(ctx: &TrapContext, iss: u32) {
    let far = read_sysreg!("far_el2");
    let hpfar = read_sysreg!("hpfar_el2");
    let ipa = ((hpfar >> 4) << 12) | (far & 0xFFF);
    let elr = read_sysreg!("elr_el2");

    // Lazy shadow-stub discovery for RAM-resident code.
    //
    // RAM is mapped XN at stage-2 so the first fetch into any 2 MiB
    // RAM block traps here. We scan the block for byte/halfword
    // accesses, install stubs, and flip XN off on that block. ERET
    // retries the fetch, which now succeeds.
    //
    // IFSC values (ISS bits [5:0]) we care about:
    //   0b000101  Translation fault, level 1 (page isn't mapped)
    //   0b001111  Permission fault, level 3 (XN)
    //   0b001110..0b001111 various permission-fault levels
    // We act on any permission fault whose IPA is inside a RAM range.
    let ifsc = (iss & 0x3f) as u32;
    let is_permission = (ifsc & 0b111100) == 0b001100; // 0x0C..0x0F
    let ram_base = guest_mem::RAM_IPA_BASE as u64;
    let ram_end = ram_base + guest_mem::RAM_SIZE as u64;
    let in_ram = (ram_base..ram_end).contains(&ipa);

    if is_permission && in_ram {
        let scan_ipa = ipa as u32;

        // Align down to the 2 MiB block boundary; scan the entire
        // block so subsequent fetches within it don't re-fault.
        let block_size = 0x0020_0000u32;
        let block_start = scan_ipa & !(block_size - 1);
        let block_end = block_start.wrapping_add(block_size);

        kprintln!(
            "shadow_stub: lazy RAM patch — block {:#x}..{:#x} (fetch at {:#x})",
            block_start, block_end, ipa
        );
        let stats = shadow_stub::patch_code_range(block_start, block_end);
        shadow_stub::log_stats(&stats);

        // Flip XN off on the RAM block.
        // SAFETY: stage2 TLB maintenance done inside the helper.
        unsafe {
            crate::stage2::clear_xn_for_block(block_start);
        }

        // Retry the fetch — don't advance ELR, just return.
        return;
    }

    kprintln!();
    kprintln!("*** instruction abort from lower EL (no silent skip per Phase A) ***");
    kprintln!(
        "  ELR={:#x}  FAR_EL2={:#x}  IPA={:#x}  IFSC={:#x}",
        elr, far, ipa, ifsc
    );
    let spsr = read_sysreg!("spsr_el2");
    let mode = spsr & 0x1F;
    let mode_name = match mode {
        0x10 => "usr", 0x11 => "fiq", 0x12 => "irq", 0x13 => "svc",
        0x16 => "mon", 0x17 => "abt", 0x1A => "hyp", 0x1B => "und",
        0x1F => "sys", _ => "???",
    };
    kprintln!(
        "  SPSR_EL2={:#x}  mode={}  R14={:#x}  R0={:#x}  R1={:#x}",
        spsr, mode_name, ctx.x[14] as u32, ctx.x[0] as u32, ctx.x[1] as u32
    );
    if mode == 0x1B {
        kprintln!(
            "  (in UND mode: R14 = faulting_pc + 4 = {:#x}; dig there for the real UND)",
            (ctx.x[14] as u32).wrapping_sub(4)
        );
    }
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
        0x40 => {
            // DebugStr ROM-patch trap: the ROM-patched stub at
            // DEBUG_STR_STUB_PC does `MOV r7, LR` before this HVC so we
            // can read LR without relying on AArch64 banked-register
            // accesses (MRS LR_svc is unimplemented on QEMU raspi3b's
            // Cortex-A53 model). r0 is the guest's string pointer; we
            // log it and resume at LR + 4, matching Einstein's callback
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
        0x41 => {
            // Debugger ROM-patch trap. Stub stashed LR into r7 for the
            // same reason as DebugStr above. Einstein's callback breaks
            // into the host debugger and returns PC = LR + 8
            // (TJITGenericROMPatch.cpp:96); we have no host debugger, so
            // log the site and continue.
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
        0x30 => {
            // Shadow-stub patch request: r0=start_ipa, r1=end_ipa (exclusive).
            // Scans that IPA range of the ROM backing (the guest-test image)
            // and patches every LDRB/STRB/LDRH/STRH/LDRSB/LDRSH. Emits stubs
            // into the shadow-stub pool and rewrites originals to Bcc stub.
            let start = ctx.x[0] as u32;
            let end = ctx.x[1] as u32;
            let stats = crate::shadow_stub::patch_code_range(start, end);
            crate::shadow_stub::log_stats(&stats);
            // Echo the patched count back to r0 so the guest can check it.
            ctx.x[0] = stats.patched as u64;
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
    // No ELR advance needed: HVC entry sets ELR_EL2 to the PC of the
    // instruction after the HVC (DDI 0487 G1.11.1 "HVC from AArch32"),
    // so ERET returns to the guest's next instruction as-is.
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
// Old (buggy) slots at 0x0400_0400 — those live inside the kernel's L1
// table (TTBR0 points at PA 0x0400_0000, and 0x0400_0400 is L1[0x100]).
// Writing there both (a) fails post-MMU because the guest's L1[0x40]
// maps VA 0x0400_0400 to PA 0x0000_0400 (ROM, RO under stage-2) and
// (b) would corrupt the guest's own L1 if it ever did succeed. New
// slots live in the RAM-mirror window the DIAG stub also uses.
pub const UND_SAVE_LR_IPA: u32 = 0x0400_5F00;
pub const UND_SAVE_SPSR_IPA: u32 = 0x0400_5F04;
/// LR_svc captured by the trampoline's brief SVC-mode bounce. Only
/// meaningful when SPSR_und's mode field says the caller was SVC
/// (which is the case for all Newton 2.x kernel-internal calls).
pub const UND_SAVE_LR_SVC_IPA: u32 = 0x0400_5F08;

/// Pre-UND R0 and R1. The trampoline persists them here before
/// clobbering R0 (to hold the save-slot VA) and R1 (to read SPSR /
/// LR_svc). `handle_und` restores `ctx.x[0]` and `ctx.x[1]` from
/// these slots at entry so the traced guest sees its arguments
/// intact across the UND round-trip.
pub const UND_SAVE_R0_IPA: u32 = 0x0400_5F0C;
pub const UND_SAVE_R1_IPA: u32 = 0x0400_5F10;

fn handle_und(ctx: &mut TrapContext) {
    // Restore pre-UND R0 and R1 from the RAM slots the trampoline
    // stashed them in. The trampoline unavoidably clobbers R0 (to
    // hold the save-slot VA) and R1 (to carry SPSR_und and then
    // LR_svc through the SVC bounce). Without this restore the
    // guest's function-arg registers get scrambled across every UND
    // round-trip — caught in Phase B as a bogus PA 0x78 write from
    // StoreToPhysAddress, root-caused to R0/R1 surviving into
    // AddPgPAndPermWithPageTable's prologue.
    //
    // R12 is also clobbered by the trampoline (used as the base
    // register for the slot STRs) but is deliberately not restored:
    // every Newton 2.x kernel function we've observed begins with
    // `MOV R12, R13`, so R12 is effectively scratch at function-
    // entry UDF sites. Non-function-entry UND sites (SWP, Einstein
    // UND opcodes, CP15 quirks) are few enough that the tests catch
    // any regression if one of them ends up relying on R12.
    ctx.x[0] = read_guest_word_pa(UND_SAVE_R0_IPA).unwrap_or(ctx.x[0] as u32) as u64;
    ctx.x[1] = read_guest_word_pa(UND_SAVE_R1_IPA).unwrap_or(ctx.x[1] as u32) as u64;

    // DIAG: prove handle_und is being reached at all. Single-shot log.
    static mut UND_ENTRY_LOGGED: bool = false;
    // SAFETY: single-threaded.
    let first = unsafe {
        let was = UND_ENTRY_LOGGED;
        UND_ENTRY_LOGGED = true;
        !was
    };
    if first {
        let elr = read_sysreg!("elr_el2");
        let spsr = read_sysreg!("spsr_el2");
        let far = read_sysreg!("far_el1");
        kprintln!(
            "und: handle_und first entry, ELR_EL2={:#x} SPSR_EL2={:#x} FAR_EL1={:#x}",
            elr, spsr, far
        );
        kprintln!(
            "und:   x13(=SP_<src>)={:#x}  x14(=LR_<src>)={:#x} — x14-4 is pre-UND PC if AArch32 x14 is plumbed",
            ctx.x[13] as u32, ctx.x[14] as u32
        );
        kprintln!(
            "und:   r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
        );
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
    // no-op and advance past it. Mask clears cond (31:28) and Rt
    // (15:12); target encoding is MCR p15,0,Rt,c15,c1,2 (0x_E0F_0F51).
    // The ROM's StrongARM-detect sequence at 0x186a8 uses cond=EQ; the
    // UND only fires when the condition already passed, so any cond
    // is valid here.
    if (insn & 0x0FFF_0FFF) == 0x0E0F_0F51 {
        log_cp15_strongarm_clock(faulting_pc);
        return_to_guest(ctx, (faulting_pc + 4) as u64, spsr_und);
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
        return_to_guest(ctx, (faulting_pc + 4) as u64, spsr_und);
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
            return_to_guest(ctx, msg_end as u64, spsr_und);
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
        // User-driven guest software breakpoint — must be checked
        // before the tracer path because the marker encoding
        // (UDF #0xFFFE) is also a UDF-shape instruction. See
        // `src/guest_bp.rs`.
        _ if insn == crate::guest_bp::BP_UDF_INSN => {
            if !crate::guest_bp::handle_user_bp_und(ctx, faulting_pc, spsr_und, insn) {
                kprintln!(
                    "*** guest_bp: marker at PC={:#x} with no matching table entry — halting",
                    faulting_pc
                );
                cpu::halt();
            }
        }
        #[cfg(feature = "trace")]
        _ if (insn & 0xFFF0_00F0) == 0xE7F0_00F0 => {
            if !crate::tracer::handle_trace_und(ctx, faulting_pc, spsr_und, insn) {
                kprintln!(
                    "*** trace: UDF-shaped insn at PC={:#x} not handled by tracer (insn={:#010x})",
                    faulting_pc, insn
                );
                cpu::halt();
            }
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
    let esr = read_sysreg!("esr_el2");
    let esr_el1 = read_sysreg!("esr_el1");
    let sctlr = read_sysreg!("sctlr_el1");
    let ttbr0 = read_sysreg!("ttbr0_el1");
    let ttbr1 = read_sysreg!("ttbr1_el1");
    let tcr   = read_sysreg!("tcr_el1");
    kprintln!(
        "  ESR_EL2   = {:#010x}  EC={:#x} ISS={:#x}",
        esr, (esr >> 26) & 0x3F, esr & 0x1FFFFFF
    );
    // ESR_EL1 holds the EL1 fault syndrome the CPU wrote when the
    // guest took its own DABT. For AArch32 DABT, EC=0x24 with
    // ISS[5:0] = DFSC (fault class).
    kprintln!(
        "  ESR_EL1   = {:#010x}  EC={:#x} ISS={:#x}  DFSC={:#x}",
        esr_el1, (esr_el1 >> 26) & 0x3F, esr_el1 & 0x1FFFFFF, esr_el1 & 0x3F
    );
    kprintln!(
        "  SCTLR_EL1 = {:#010x}  (M={}, C={}, I={}, V={})",
        sctlr, sctlr & 1, (sctlr >> 2) & 1, (sctlr >> 12) & 1, (sctlr >> 13) & 1
    );
    kprintln!(
        "  TTBR0_EL1 = {:#010x}  TTBR1_EL1 = {:#010x}  TCR_EL1 = {:#010x}",
        ttbr0, ttbr1, tcr
    );
    guest_mem::dump_stage1_walk(far as u32);
    // Also walk a handful of VAs that are relevant to Newton boot —
    // SVC stack, ABT stack target, REx window start, RAM base — so we
    // can tell at a glance whether the kernel's L1 table has the
    // expected mappings in place at the time of the abort.
    // 0x02Axxxxx added because recent DIAG runs show SP_svc and PC
    // both landing there; need to see the stage-1 layout. Also
    // 0x00FFFF00 (our UND trampoline body) to confirm the guest can
    // actually fetch from there post-MMU.
    for va in [0x04004400u32, 0x0C004C00, 0x01000000, 0x04000000, 0x00800000,
               0x02A00000, 0x02A04000, 0x02A04AA4, 0x00FFFF00,
               // 0x0008Exxx: where SP_und / LR_und point per the stub
               // readout. ROM region so identity-mapped through L2, but
               // we want to confirm there's no surprise.
               0x0008EA8C, 0x0008EB00, 0x0008EB08,
               // 0x01000xxx: the faulting VA region. L1[0x10] = fault in
               // Einstein's map; we want to dump L2 subentries only if
               // somehow a fine/coarse table got installed.
               0x0100018B, 0x01000180, 0x01000190, 0x01000193,
               // 0x01A00xxx: IRQ vector target (REx jump table). If
               // L1[0x1A] isn't populated, PABTs from the IRQ path
               // become a hidden source of chained exceptions.
               0x01A00000, 0x01A00004,
               // 0x0C100xxx: kernel domain heap / globals per Einstein's
               // MMU map. L1[0xC1] should be a coarse-into-L2 with small
               // pages; our post-fix_stage1_xn_bits normalisation may
               // have stripped domain bits needed for writes.
               0x0C100000, 0x0C100800, 0x0C104000] {
        guest_mem::dump_stage1_walk(va);
    }

    // Before halting, ERET into an AArch32 stub we plant in guest
    // RAM that dumps banked R13/R14/SPSR of the source mode plus SVC
    // and UND as well. We write them to a scratch area in RAM and
    // hvc back; EL2 then reads the saved words directly. This is more
    // reliable than trying to carry values through ctx.x[0..4] because
    // QEMU raspi3b's AArch32→AArch64 banked register plumbing is
    // already known to be flaky (SPSR_abt reads as 0 from AArch64,
    // x14 doesn't reliably carry LR_<src_mode>).
    //
    // Stub at guest IPA 0x04005F00, reached via VA 0x0C004F00 (the
    // kernel maps 0x0C004000-0x0C004FFF through L1[0xC0] coarse ->
    // L2[0x04] small page -> PA 0x04005xxx). Saves to IPA 0x04005F80+,
    // which is inside the same small page so it's writable from the
    // guest's view.
    //
    //   +0x00: e59f0050   ldr r0, [pc, #0x50]  ; r0 = &SAVE_BASE (VA 0x0C004F80)
    //   +0x04: e1a0100e   mov r1, lr            ; r1 = R14_abt
    //   +0x08: e5801000   str r1, [r0]          ; save LR_abt at +0x00
    //   +0x0C: e1a0100d   mov r1, sp            ; r1 = R13_abt
    //   +0x10: e5801004   str r1, [r0, #4]      ; save SP_abt at +0x04
    //   +0x14: e14f1000   mrs r1, spsr          ; r1 = SPSR_abt
    //   +0x18: e5801008   str r1, [r0, #8]      ; save SPSR_abt at +0x08
    //   +0x1C: e321f0db   msr cpsr_c, #0xdb     ; → UND
    //   +0x20: e1a0100e   mov r1, lr            ; r1 = R14_und
    //   +0x24: e580100c   str r1, [r0, #12]     ; save LR_und at +0x0C
    //   +0x28: e1a0100d   mov r1, sp            ; r1 = R13_und
    //   +0x2C: e5801010   str r1, [r0, #16]     ; save SP_und at +0x10
    //   +0x30: e14f1000   mrs r1, spsr          ; r1 = SPSR_und
    //   +0x34: e5801014   str r1, [r0, #20]     ; save SPSR_und at +0x14
    //   +0x38: e321f0d3   msr cpsr_c, #0xd3     ; → SVC
    //   +0x3C: e1a0100e   mov r1, lr            ; r1 = R14_svc
    //   +0x40: e5801018   str r1, [r0, #24]     ; save LR_svc at +0x18
    //   +0x44: e1a0100d   mov r1, sp            ; r1 = R13_svc
    //   +0x48: e580101c   str r1, [r0, #28]     ; save SP_svc at +0x1C
    //   +0x4C: e321f0d7   msr cpsr_c, #0xd7     ; → ABT
    //   +0x50: ee151f10   mrc p15,0,r1,c5,c0,0  ; r1 = DFSR
    //   +0x54: e5801020   str r1, [r0, #0x20]    ; save DFSR at +0x20
    //   +0x58: ee161f10   mrc p15,0,r1,c6,c0,0   ; r1 = DFAR
    //   +0x5C: e5801024   str r1, [r0, #0x24]    ; save DFAR at +0x24
    //   +0x60: e1400172   hvc #0x12             ; DIAG_LR_TAG
    //   +0x64: eafffffe   b .                   ; trap if returns
    //   +0x68: 0c004f80   .word SAVE_BASE_VA
    const LR_STUB_PA: u32 = 0x0400_5F00;
    const LR_STUB_VA: u32 = 0x0C00_4F00;
    const LR_SAVE_VA: u32 = 0x0C00_4F80;
    const LR_SAVE_PA: u32 = 0x0400_5F80;
    let stub: [u32; 27] = [
        0xE59F_0060, // ldr r0, [pc, #0x60]  (literal at end)
        0xE1A0_100E, // mov r1, lr
        0xE580_1000, // str r1, [r0]
        0xE1A0_100D, // mov r1, sp
        0xE580_1004, // str r1, [r0, #4]
        0xE14F_1000, // mrs r1, spsr
        0xE580_1008, // str r1, [r0, #8]
        0xE321_F0DB, // msr cpsr_c, #0xdb  (UND)
        0xE1A0_100E, // mov r1, lr  (LR_und)
        0xE580_100C, // str r1, [r0, #0xc]
        0xE1A0_100D, // mov r1, sp  (SP_und)
        0xE580_1010, // str r1, [r0, #0x10]
        0xE14F_1000, // mrs r1, spsr  (SPSR_und)
        0xE580_1014, // str r1, [r0, #0x14]
        0xE321_F0D3, // msr cpsr_c, #0xd3  (SVC)
        0xE1A0_100E, // mov r1, lr  (LR_svc)
        0xE580_1018, // str r1, [r0, #0x18]
        0xE1A0_100D, // mov r1, sp  (SP_svc)
        0xE580_101C, // str r1, [r0, #0x1C]
        0xE321_F0D7, // msr cpsr_c, #0xd7  (back to ABT)
        0xEE15_1F10, // mrc p15,0,r1,c5,c0,0 — DFSR
        0xE580_1020, // str r1, [r0, #0x20]
        0xEE16_1F10, // mrc p15,0,r1,c6,c0,0 — DFAR
        0xE580_1024, // str r1, [r0, #0x24]
        0xE140_0172, // hvc #0x12
        0xEAFF_FFFE, // b .
        LR_SAVE_VA,  // literal for first ldr
    ];
    for (i, w) in stub.iter().enumerate() {
        if !guest_mem::write_word_pa(LR_STUB_PA + (i as u32) * 4, *w) {
            kprintln!("  (stub write at +{} failed; halting)", i * 4);
            cpu::halt();
        }
    }
    // Record the save-base PA in a module-private location that the
    // DIAG_LR_TAG handler knows how to find.
    LR_SAVE_PA_RECORD.store(LR_SAVE_PA, core::sync::atomic::Ordering::Relaxed);
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

/// Second-stage diagnostic: the stub installed by `handle_diag` stored
/// banked R13/R14/SPSR for ABT, UND, and SVC modes to fixed RAM slots
/// (base recorded in `LR_SAVE_PA_RECORD`). We read them back here and
/// print a symbolic stack trace. Reading from RAM avoids QEMU's
/// flaky AArch32→AArch64 banked-register plumbing.
fn handle_diag_lr(ctx: &mut TrapContext) -> ! {
    let _ = ctx; // guest x0 was clobbered as stub scratch
    let base = LR_SAVE_PA_RECORD.load(core::sync::atomic::Ordering::Relaxed);
    let read = |off: u32| guest_mem::read_word_pa(base + off).unwrap_or(0xdeadbeef);
    let lr_abt   = read(0x00);
    let sp_abt   = read(0x04);
    let spsr_abt = read(0x08);
    let lr_und   = read(0x0C);
    let sp_und   = read(0x10);
    let spsr_und = read(0x14);
    let lr_svc   = read(0x18);
    let sp_svc   = read(0x1C);
    let dfsr     = read(0x20);
    let dfar     = read(0x24);

    // The "source mode" of the DABT is whichever mode SPSR_abt names.
    // Reconstruct the faulting PC from LR of that mode, with the
    // correct adjustment (+8 for DABT/ARM, +4/T for DABT/Thumb).
    let src_mode = spsr_abt & 0x1F;
    let (lr_src, sp_src, spsr_src) = match src_mode {
        0x1B => (lr_und, sp_und, spsr_und),
        0x13 => (lr_svc, sp_svc, spsr_abt /* SPSR_abt holds pre-abt CPSR */),
        _    => (lr_abt, sp_abt, spsr_abt),
    };
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
    kprintln!(
        "  DFSR      = {:#010x}  (FS[4:0]={:#x}, WnR={}, domain={:#x})",
        dfsr,
        ((dfsr >> 10) & 1) << 4 | (dfsr & 0xF),
        (dfsr >> 11) & 1,
        (dfsr >> 4) & 0xF
    );
    kprintln!("  DFAR      = {:#010x}", dfar);
    kprintln!("  SP_abt    = {:#010x}  LR_abt = {:#010x}  SPSR_abt = {:#010x}",
        sp_abt, lr_abt, spsr_abt);
    kprintln!("  SP_und    = {:#010x}  LR_und = {:#010x}  SPSR_und = {:#010x}",
        sp_und, lr_und, spsr_und);
    kprintln!("  src_mode = {:#x}", src_mode);

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

/// Phase B diagnostic shim used by `tracer::dump_rex_state`: wrapper
/// around `guest_translate_va` exposed with a shorter name so the
/// tracer doesn't need to import `guest_translate_va` directly.
#[cfg(feature = "trace")]
pub fn guest_tl_translate(va: u32) -> Option<u32> {
    guest_translate_va(va)
}

/// Translate a guest VA to its guest PA via the current stage-1
/// tables. Returns None on a fault (unmapped / wrong descriptor type).
/// Uses the same logic as `guest_mem::dump_stage1_walk` but returns
/// the PA instead of printing.
pub fn guest_translate_va(va: u32) -> Option<u32> {
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

/// UND-path version of `return_to_guest`: same sysreg writes, different
/// name so it's obvious at call sites that the caller came from the
/// trampoline-based UND handler. Used by `tracer` and `guest_bp` after
/// they restore the faulting instruction's original word.
pub(crate) fn return_to_guest_from_und(ctx: &mut TrapContext, elr: u64, spsr: u64) {
    return_to_guest(ctx, elr, spsr);
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
    // Dedup SystemBootUND / TapFileCntlUND by PC — only 6 sites in ROM
    // total. Same rationale as log_debugger_und: one log per site gives
    // us clear bring-up breadcrumbs without flooding on tight loops.
    const SEEN_CAP: usize = 16;
    static mut SEEN: [u32; SEEN_CAP] = [0; SEEN_CAP];
    static mut SEEN_N: usize = 0;
    // SAFETY: single-threaded.
    let first = unsafe {
        let mut found = false;
        for i in 0..SEEN_N { if SEEN[i] == pc { found = true; break; } }
        if !found && SEEN_N < SEEN_CAP {
            SEEN[SEEN_N] = pc;
            SEEN_N += 1;
            true
        } else {
            false
        }
    };
    if first {
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

/// Scan guest memory from `start` word-by-word for a null byte in
/// any of the bytes of each word, and return the VA one past the end
/// of the word that contains the null (aligned, since words are
/// 4-byte aligned). `max_words` bounds the search so a missing null
/// doesn't infinite-loop.
/// Log a guest C string pointed to by `addr`.
///
/// The Newton 717006 ROM is stored big-endian in the image file and
/// byteswapped per word at load time so LDR in our LE guest returns
/// the u32 the original BE CPU saw (see `guest_mem::load_newton_rom`).
/// Bytes within each 4-byte word end up reversed in host memory: a
/// word originally `0x48 0x65 0x6C 0x6C` ("Hell" in BE) is laid out
/// as `0x6C 0x6C 0x65 0x48` in host LE memory. To recover the
/// original byte sequence we re-swap each loaded word via
/// `to_be_bytes()`.
///
/// Guest-test binaries are LE-native (no ROM byteswap on load), so
/// the bytes in host memory are already in natural order — use
/// `to_le_bytes()`. We pick at compile time via `nh_guest_test`.
fn log_guest_string(prefix: &'static str, addr: u32) {
    const CAP: usize = 256;
    let mut buf = [0u8; CAP];
    let mut len = 0usize;
    let mut va = addr;
    'outer: while len < CAP {
        let w = match read_guest_word_pa(va & !0x3) {
            Some(v) => v,
            None => break,
        };
        #[cfg(nh_guest_test)]
        let bytes = w.to_le_bytes();
        #[cfg(not(nh_guest_test))]
        let bytes = w.to_be_bytes();
        let first = (va & 0x3) as usize;
        for i in first..4 {
            let b = bytes[i];
            if b == 0 { break 'outer; }
            buf[len] = b;
            len += 1;
            if len == CAP { break 'outer; }
        }
        va = (va & !0x3).wrapping_add(4);
    }
    match core::str::from_utf8(&buf[..len]) {
        Ok(s) => kprintln!("{}: {:?}", prefix, s),
        Err(_) => kprintln!("{}: <{} non-utf8 bytes @ {:#x}>", prefix, len, addr),
    }
}

fn scan_to_null_word_aligned(start: u32, max_words: u32) -> u32 {
    let mut va = start & !0x3;
    for _ in 0..max_words {
        let w = read_guest_word_pa(va).unwrap_or(0);
        // The ROM is stored big-endian (original 1990s Newton bytes)
        // and our load_rom byteswaps each word so LDR in our LE guest
        // returns the same u32 the original BE CPU saw. That means a
        // byte-level string search has to examine the word in BE byte
        // order — the null terminator is *BE-byte-order* inside a
        // word, which is why we use to_be_bytes here, not to_le_bytes.
        let bytes = w.to_be_bytes();
        if bytes[0] == 0 || bytes[1] == 0 || bytes[2] == 0 || bytes[3] == 0 {
            return va.wrapping_add(4);
        }
        va = va.wrapping_add(4);
    }
    // No null found within bound — return (start + max_words*4) as a
    // best-effort stop. Caller will log + the guest may fault on the
    // next fetch, which makes the miss visible.
    va
}

fn log_debugger_und(pc: u32, msg_start: u32, msg_end: u32) {
    // Dedup by PC: each DebuggerUND site in the ROM is a distinct panic
    // message (e.g. "_stack_overflow called - panic!", "Undefined SWI",
    // "SWI from non-user mode (rebooting)"), and the first time the guest
    // hits any one of them tells us something specific about where we've
    // diverged. There are ~22 sites across ROM + REx, so an unfiltered
    // log of first-hits isn't noisy. Repeated hits at the same PC are
    // suppressed.
    const SEEN_CAP: usize = 32;
    static mut SEEN: [u32; SEEN_CAP] = [0; SEEN_CAP];
    static mut SEEN_N: usize = 0;
    // SAFETY: single-threaded.
    let first = unsafe {
        let mut found = false;
        for i in 0..SEEN_N { if SEEN[i] == pc { found = true; break; } }
        if !found && SEEN_N < SEEN_CAP {
            SEEN[SEEN_N] = pc;
            SEEN_N += 1;
            true
        } else {
            false
        }
    };
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

fn log_cp15_deprecated_cache_all(pc: u32) {
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
        kprintln!(
            "und: MCR p15,0,Rt,c7,c7,0 (deprecated invalidate-unified-cache) @PC={:#x} — emulated as ICIALLU",
            pc
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
    let should_log = unsafe {
        let mut found = false;
        for i in 0..CP15_N {
            if CP15_SEEN[i] == key { found = true; break; }
        }
        if !found && CP15_N < 32 {
            CP15_SEEN[CP15_N] = key;
            CP15_N += 1;
            true
        } else {
            false
        }
    };
    if should_log {
        let value_log = if is_read { 0 } else { ctx.x[rt] as u32 };
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "cp15: {} p15,{},Rt=r{},c{},c{},{{{}}} val={:#010x} @ELR={:#x}",
            if is_read { "MRC" } else { "MCR" },
            opc1, rt, crn, crm, opc2, value_log, elr
        );
    }

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
            // Detect M=0→M=1 transitions and re-walk the stage-1 tables
            // then. The TTBR-write pass catches what was reachable at
            // that moment but misses coarse L1 entries populated after.
            // (ARMv4 small-page descriptors use bits[11:4] as four
            // subpage AP fields; ARMv7 reinterprets bit 9 as AP[2] and
            // bits[5:4] as AP[1:0], so entries like 0x04007F0E read as
            // AP[2:0]=100 (reserved) = no-access on A53 and writes
            // permission-fault.) Running fix on every M=1 write would
            // cost ~60k calls/sec under task switching, so we gate it
            // on the rising edge only. The rewrite is idempotent.
            let prev_sctlr = cp15::read_sctlr_el1() as u32;
            let was_off = (prev_sctlr & 1) == 0;
            let now_on = (value & 1) != 0;
            cp15::write_sctlr_el1(value as u64);
            log_sctlr_write(value);
            if was_off && now_on {
                guest_mem::fix_stage1_xn_bits();
                maybe_dump_l1_once();
                // Install function-tracing UDFs now that the guest's
                // stage-1 L1 maps VA 0x0C00_4F00 to the UND-trampoline
                // save slot. Earlier patches would lose LR_und and
                // land the handler at a bogus PC. Idempotent: the
                // tracer gates itself on a one-shot flag.
                #[cfg(feature = "trace")]
                // SAFETY: single-threaded EL2 trap context.
                unsafe { crate::tracer::enable_patches(); }
                // Swap the UND trampoline's save-slot literal to the
                // kernel VA that L1[0xC0] maps to the RAM slot. Done
                // outside `enable_patches()` so a soft-reboot that
                // cycles M=1→0→1 re-applies the swap (the tracer
                // gates its UDF install on a one-shot flag, but the
                // literal needs to track every MMU transition).
                // SAFETY: single-word ROM-backing write under the
                // paused-guest invariant.
                unsafe { guest_mem::install_und_vector_swap_post_mmu(); }
            }
            // M=1→M=0: the guest is turning its stage-1 MMU off
            // (typically the SWIBoot→ROMBoot soft-reset path). Revert
            // the UND trampoline's save-slot literal to the pre-MMU
            // RAM IPA so any UND taken before MMU re-enable lands in
            // a stage-2-mapped IPA. Without this, the first trace-UDF
            // after a soft reboot stores to VA 0x0C00_4F0C with MMU
            // off, which faults at an unmapped IPA.
            if !was_off && !now_on {
                // SAFETY: single-word ROM-backing write under the
                // same paused-guest invariant as the original patch.
                unsafe { guest_mem::install_und_vector_swap_pre_mmu(); }
            }
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

        // Guest VBAR_EL1 write (CP15 c12, c0, opc1=0, opc2=0). Needed
        // so tests that want a non-default exception-vector table can
        // install one; the real Newton ROM never writes VBAR (it uses
        // low vectors at 0), but the shadow_stub abort-transparency
        // test does.
        (0, 12, 0, 0, false) => {
            let value = ctx.x[rt] as u64;
            // SAFETY: VBAR_EL1 is writable at EL2; on ERET the guest
            // sees it as its own CP15 VBAR.
            unsafe {
                core::arch::asm!(
                    "msr vbar_el1, {}",
                    "isb",
                    in(reg) value,
                    options(nostack, preserves_flags),
                );
            }
        }

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

    pub fn invalidate_icache_all() {
        // ARMv8 equivalent of ARMv4 `MCR p15, 0, Rt, c7, c7, 0`
        // (invalidate unified cache). A53 has split I/D caches with
        // broadcast; `IC IALLUIS` covers the inner-shareable domain.
        // The D-cache is handled by A53's native coherency for our
        // config, so no explicit DCCISW loop is needed here.
        // SAFETY: cache maintenance sysreg writes.
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "ic ialluis",
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
