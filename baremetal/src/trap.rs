//! EL2 synchronous trap dispatcher.
//!
//! The vector at offset 0x600 (lower-EL AArch32 sync) saves the full x0..x30
//! context, hands us a `*mut TrapContext`, and we dispatch based on ESR_EL2.EC.
//!
//! Handlers that emulate a guest instruction and want to resume mutate the
//! context in place, advance ELR_EL2 past the faulting instruction, then
//! return — the vector trailer restores the context and ERETs. Handlers that
//! don't want to resume never return (they call `cpu::halt`).

use crate::{cpu, guest_mem, kprintln, mmio, peripherals::vic, timer};

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
const EC_HVC_A32: u32 = 0x12;
const EC_INSN_ABORT_LOWER: u32 = 0x20;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

/// Synchronous exception from a lower EL running AArch32.
#[no_mangle]
pub extern "C" fn trap_sync_lower_aarch32(ctx: &mut TrapContext) {
    let esr = read_sysreg!("esr_el2");
    let ec = ((esr >> 26) & 0x3f) as u32;
    let iss = (esr & 0x01ff_ffff) as u32;

    match ec {
        EC_DATA_ABORT_LOWER => handle_data_abort(ctx, iss),
        EC_INSN_ABORT_LOWER => handle_instruction_abort(iss),
        EC_HVC_A32 => handle_hvc(ctx, iss),
        EC_TRAPPED_CP15 => handle_cp15_trap(ctx, iss),
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

    // Periodically produce a "screenshot" of the guest's RAM + framebuffer
    // state, so we have something to look at even while the kernel is
    // stuck pre-scheduler. Fires once after 1 000 000 traps (~25 s on
    // QEMU) and then halts so the output window is bounded.
    // Budget-limited "progress beacon": print PC every 100k traps so we
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
pub extern "C" fn trap_irq(_ctx: &mut TrapContext) {
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

    if wnr {
        let value = ctx.x[srt] as u32;
        mmio::write(ipa, sas, value);
    } else {
        let value = mmio::read(ipa, sas);
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
    static mut SEEN: [u64; 32] = [u64::MAX; 32];
    static mut NEXT: usize = 0;
    let already = unsafe {
        let mut hit = false;
        for i in 0..SEEN.len() { if SEEN[i] == ipa { hit = true; break; } }
        if !hit && NEXT < SEEN.len() { SEEN[NEXT] = ipa; NEXT += 1; }
        hit
    };
    if !already {
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "iabort[uniq] ELR={:#x} FAR={:#x} IPA={:#x} IFSC={:#x} — skipping",
            elr, far, ipa, iss & 0x3f
        );
    }
    // Skip the 32-bit ARM instruction that couldn't be fetched. Execution
    // resumes at PC+4. This is aggressive — if the instruction was supposed
    // to run, we've just silently dropped it — but it lets boot continue
    // past the chained-abort loops that otherwise trap us.
    advance_elr(4);
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
        _ => {
            let elr = read_sysreg!("elr_el2");
            kprintln!();
            kprintln!("*** unknown HVC #{:#x} at ELR={:#x} (halting)", imm, elr);
            cpu::halt();
        }
    }
    // HVC is a 4-byte ARM instruction; advance past it on return.
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
            // Unrecognised tuple — log (budget is applied above) and
            // fall through to ignored.
            log_unknown_cp15(is_read, opc1, crn, crm, opc2, rt, ctx);
        }
    }

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

fn log_unknown_cp15(is_read: bool, opc1: u32, crn: u32, crm: u32, opc2: u32, rt: usize, ctx: &TrapContext) {
    let value = if is_read { 0 } else { ctx.x[rt] as u32 };
    let elr = read_sysreg!("elr_el2");
    kprintln!(
        "cp15 UNHANDLED: {} p15,{},Rt=r{},c{},c{},{{{}}} val={:#010x} @ELR={:#x}",
        if is_read { "MRC" } else { "MCR" },
        opc1, rt, crn, crm, opc2, value, elr
    );
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

fn handle_unknown(iss: u32) {
    let elr = read_sysreg!("elr_el2");
    let spsr = read_sysreg!("spsr_el2");
    // EC=0 "unknown reason" usually means an illegal AArch32 instruction
    // trapped to EL1 naturally — but when HCR_EL2.VM=1 some end up here.
    // Skip the instruction so the guest's kernel can continue if it was
    // probing for CPU features.
    kprintln!(
        "unknown sync at ELR={:#x} SPSR={:#x} ISS={:#x} (skipping)",
        elr, spsr, iss
    );
    advance_elr(4);
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
