//! EL2 synchronous trap dispatcher.
//!
//! The vector at offset 0x600 (lower-EL AArch32 sync) saves the full x0..x30
//! context, hands us a `*mut TrapContext`, and we dispatch based on ESR_EL2.EC.
//!
//! Handlers that emulate a guest instruction and want to resume mutate the
//! context in place, advance ELR_EL2 past the faulting instruction, then
//! return — the vector trailer restores the context and ERETs. Handlers that
//! don't want to resume never return (they call `cpu::halt`).

use crate::{cpu, kprintln, mmio};

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
        // Without a decodable syndrome we can't safely emulate. Log and halt.
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "*** data abort with ISV=0 at ELR={:#x} IPA={:#x} (can't decode) iss={:#x}",
            elr, ipa, iss
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
    let elr = read_sysreg!("elr_el2");
    let ipa = ((hpfar >> 4) << 12) | (far & 0xFFF);
    kprintln!();
    kprintln!("*** guest instruction abort ***");
    kprintln!("ELR_EL2={:#x}  FAR={:#x}  IPA={:#x}  IFSC={:#x}",
        elr, far, ipa, iss & 0x3f);
    kprintln!("(no code mapped here — halting until M3+ wires the jump-table area)");
    cpu::halt();
}

fn handle_hvc(_ctx: &mut TrapContext, iss: u32) {
    let elr = read_sysreg!("elr_el2");
    kprintln!();
    kprintln!("*** guest HVC #{:#x} at ELR={:#x} (halting)", iss & 0xFFFF, elr);
    cpu::halt();
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
    let crm = ((iss >> 1) & 0xF) as u32;
    let rt = ((iss >> 5) & 0x1F) as usize;
    let crn = ((iss >> 10) & 0xF) as u32;
    let opc1 = ((iss >> 14) & 0x7) as u32;
    let opc2 = ((iss >> 17) & 0x7) as u32;

    static mut CP15_LOG_COUNT: usize = 0;
    // SAFETY: single-threaded bringup context.
    let n = unsafe {
        let n = CP15_LOG_COUNT;
        CP15_LOG_COUNT += 1;
        n
    };
    if n < 32 {
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "cp15[{:2}] {} p15,{},c{},c{},{{{}}} Rt=r{} @ELR={:#x}",
            n,
            if is_read { "MRC" } else { "MCR" },
            opc1,
            crn,
            crm,
            opc2,
            rt,
            elr
        );
    } else if n == 32 {
        kprintln!("cp15: log budget exhausted — silencing further output");
    }

    // Stub responses, matching the probe findings for 717006:
    //   (0, 0, 0, 0, MRC)  = read CPU ID → return StrongARM ID (0x4401A100,
    //                         as TARMProcessor.cpp:94 does).
    //   other reads        → 0
    //   all writes         → accepted but ignored
    if is_read {
        let value = match (opc1, crn, crm, opc2) {
            (0, 0, 0, 0) => 0x4401_A100_u32, // MIDR-equivalent for StrongARM
            _ => 0,
        };
        ctx.x[rt] = value as u64;
    } else {
        // Writes: silently accept; real CP15 shim will land in M3+.
        let _ = ctx.x[rt];
    }

    advance_elr(4);
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
