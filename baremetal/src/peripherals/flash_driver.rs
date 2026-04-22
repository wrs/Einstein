//! Flash driver — rust port of Einstein's `TEinsteinFlashDriver`.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000000. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteFlashDriverNative`
//! (`Emulator/TNativePrimitives.cpp:263-528`):
//!
//!   0x01  Identify            — mask-based chip ID response (r0=1 on hit)
//!   0x02  CleanUp             — r0=0
//!   0x03  Init                — r0=0
//!   0x04  InitializeDriverData — r0=0
//!   0x05  CleanUpDriverData    — r0=0
//!   0x06  StartReadingArray    — r0=0
//!   0x07  DoneReadingArray     — r0=0
//!   0x08  Write(word, mask, addr)
//!   0x09  StartErase(flashRange, addr)
//!   0x0A  ResetBlockStatus     — r0=0
//!   0x0B  IsEraseComplete      — r0=1, *r3=0
//!   0x0C  LockBlock            — r0=0
//!   0x0D  BeginWrite           — r0=0 if addr in flash, else kError_Flash_AddressOutOfRange
//!
//! Writes and erases call into `peripherals::flash` which owns the
//! backing bytes (same backing stage-2 maps RW).

use crate::{cpu, guest_mem, kprintln, peripherals::flash, trap::TrapContext};

/// Flash-driver class ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_0000;

/// `kError_Flash_AddressOutOfRange` (NewtonErrors.h; Einstein uses -10562).
const ERR_FLASH_ADDR_OUT_OF_RANGE: u32 = (-10562i32) as u32;

/// Virtual-table addresses Einstein uses to detect 32-bit flash
/// access vs. 16-bit, keyed on the first word at the flash-range
/// descriptor. `TNativePrimitives.cpp:380-386, 432-440`.
///
/// Entries are Einstein-verbatim; the 717006 MP2x00US ROM picks
/// `0x0001E3D4` (32-bit) or `0x0001E3BC` (16-bit) — the others
/// cover EM300 and MP2100D.
const VTABLES_32BIT: [u32; 3] = [0x0001_E3D4, 0x0001_E3E0, 0x0001_E180];

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        0x01 => identify(ctx, pc),
        0x02 | 0x03 | 0x04 | 0x05 | 0x06 | 0x07 | 0x0A | 0x0C => {
            ctx.x[0] = 0;
        }
        0x08 => write(ctx, pc),
        0x09 => start_erase(ctx, pc),
        0x0B => is_erase_complete(ctx, pc),
        0x0D => begin_write(ctx, pc),
        _ => {
            kprintln!(
                "*** flash_driver: unknown subfn {:#x} @PC={:#x} r1={:#x} r2={:#x} r3={:#x}",
                subfn, pc, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            cpu::halt();
        }
    }
}

/// TEinsteinFlashDriver::Identify(chipAddr, mask, idStructAddr)
///
/// Writes a six-word chip-info struct at the guest VA/PA in r3 and
/// sets r0=1 when the mask matches a lane we model; r0=0 otherwise.
/// Struct layout copied verbatim from Einstein (TNativePrimitives.cpp:
/// 284-299): manufacturer=0x89 (Intel), deviceID=0, size=0x00200000,
/// blockSize=0x00010000.
fn identify(ctx: &mut TrapContext, pc: u32) {
    let _chip_addr = ctx.x[1] as u32;
    let mask = ctx.x[2] as u32;
    let id_struct_addr = ctx.x[3] as u32;

    let recognised = matches!(mask, 0xFF00_0000 | 0x00FF_0000 | 0x0000_FF00 | 0x0000_00FF);
    if !recognised {
        ctx.x[0] = 0;
        return;
    }

    let fields: [(u32, u32); 6] = [
        (0x00, 0x0000_0089), // manufacturer — Intel
        (0x04, 0x0000_0000), // device
        (0x08, 0x0000_0002),
        (0x0C, 0x0000_0002),
        (0x10, 0x0020_0000), // chip size
        (0x14, 0x0001_0000), // block size
    ];
    for (off, val) in fields {
        if !write_guest_word(id_struct_addr + off, val) {
            kprintln!(
                "*** flash_driver.Identify: cannot write at addr={:#x} @PC={:#x}",
                id_struct_addr + off, pc
            );
            cpu::halt();
        }
    }
    ctx.x[0] = 1;
}

/// TEinsteinFlashDriver::Write(word=r1, mask=r2, addr=r3).
/// `flashRange` read from [sp+4] to detect 16-vs-32-bit access.
fn write(ctx: &mut TrapContext, pc: u32) {
    let word = ctx.x[1] as u32;
    let mask = ctx.x[2] as u32;
    let addr = ctx.x[3] as u32;

    // flashRange pointer at [sp+4]; first word of *flashRange is
    // the virtual table address — 32-bit vs. 16-bit lane detection.
    //
    // QEMU raspi3b doesn't reliably propagate the AArch32 banked R13
    // to AArch64 x13 on exception entry (see the similar workaround
    // comment in trap.rs for the UND/DIAG paths). Read SP_svc through
    // the AArch64 banked-register alias instead — native primitives
    // are only ever called from the Newton kernel in SVC mode.
    let sp = read_sp_svc() as u32;
    let flash_range = match read_guest_word(sp + 4) {
        Some(v) => v,
        None => {
            kprintln!(
                "*** flash_driver.Write: cannot read flashRange at SP+4 = {:#x} @PC={:#x}",
                sp + 4, pc
            );
            cpu::halt();
        }
    };
    let v_table = match read_guest_word(flash_range) {
        Some(v) => v,
        None => {
            kprintln!(
                "*** flash_driver.Write: cannot read virtualTable via flashRange={:#x} @PC={:#x}",
                flash_range, pc
            );
            cpu::halt();
        }
    };
    let is_32bit = VTABLES_32BIT.contains(&v_table);

    let pa = match resolve_flash_pa(addr) {
        Some(p) => p,
        None => {
            ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
            return;
        }
    };

    let ok = if is_32bit {
        flash::program_word(pa, word, mask)
    } else {
        // 16-bit path: TMemory::WriteToFlash16Bits
        // (TMemory.cpp:2616-2668). Splits the word into the high
        // or low 16-bit lane based on (PA & 2), then programs the
        // containing u32 with masked data.
        let aligned_pa = pa & !0x3;
        let (w, m) = if pa & 0x2 != 0 {
            (word & 0x0000_FFFF, mask & 0x0000_FFFF)
        } else {
            ((word & 0x0000_FFFF) << 16, (mask & 0x0000_FFFF) << 16)
        };
        flash::program_word(aligned_pa, w, m)
    };

    if ok {
        ctx.x[0] = 0;
    } else {
        ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
    }
}

/// TEinsteinFlashDriver::StartErase(flashRange=r1, addr=r2).
/// Block size derived from virtualTable: 0x20000 for 32-bit parts,
/// 0x10000 for 16-bit.
fn start_erase(ctx: &mut TrapContext, pc: u32) {
    let flash_range = ctx.x[1] as u32;
    let addr = ctx.x[2] as u32;

    let v_table = match read_guest_word(flash_range) {
        Some(v) => v,
        None => {
            kprintln!(
                "*** flash_driver.StartErase: cannot read virtualTable via flashRange={:#x} @PC={:#x}",
                flash_range, pc
            );
            cpu::halt();
        }
    };
    let block_size = if VTABLES_32BIT.contains(&v_table) { 0x2_0000 } else { 0x1_0000 };

    let pa = match resolve_flash_pa(addr) {
        Some(p) => p,
        None => {
            ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
            return;
        }
    };

    if flash::erase_block(pa, block_size) {
        ctx.x[0] = 0;
    } else {
        ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
    }
}

/// TEinsteinFlashDriver::IsEraseComplete — erases are synchronous in
/// Einstein and here, so set r0=1 (complete) and *r3=0 (no error).
fn is_erase_complete(ctx: &mut TrapContext, pc: u32) {
    let result_addr = ctx.x[3] as u32;
    if !write_guest_word(result_addr, 0) {
        kprintln!(
            "*** flash_driver.IsEraseComplete: cannot write result at {:#x} @PC={:#x}",
            result_addr, pc
        );
        cpu::halt();
    }
    ctx.x[0] = 1;
}

/// TEinsteinFlashDriver::BeginWrite(r1, addr=r2, r3). Success = 0 if
/// addr is within a flash bank; `kError_Flash_AddressOutOfRange` otherwise.
fn begin_write(ctx: &mut TrapContext, _pc: u32) {
    let addr = ctx.x[2] as u32;
    if resolve_flash_pa(addr).is_some() {
        ctx.x[0] = 0;
    } else {
        ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
    }
}

/// Resolve a flash address (either a kernel VA or a flash PA) to the
/// guest PA within one of the two flash windows. Tries stage-1
/// translation first; falls back to treating `addr` as a PA if that
/// fails. Returns None if the resolved PA is not in a flash bank.
fn resolve_flash_pa(addr: u32) -> Option<u32> {
    let pa = guest_mem::translate_va(addr).unwrap_or(addr);
    flash::pa_to_offset(pa).map(|_| pa)
}

/// Helper: try writing `value` at a guest address, first by VA
/// translation then by treating it as a PA. Returns whether the
/// backing accepted the store.
fn write_guest_word(addr: u32, value: u32) -> bool {
    if guest_mem::write_word_va(addr, value) {
        return true;
    }
    guest_mem::write_word_pa(addr, value)
}

/// Read a word at a guest address with the same VA-first / PA-fallback
/// semantics as `write_guest_word`. Lets MMU-off callers (guest tests)
/// pass PAs directly.
fn read_guest_word(addr: u32) -> Option<u32> {
    if let Some(v) = guest_mem::read_word_va(addr) {
        return Some(v);
    }
    guest_mem::read_word_pa(addr)
}

/// Read the AArch32 banked SP_svc through the AArch64 alias. Used by
/// TEinsteinFlashDriver::Write to reach `[sp+4]` (flashRange pointer)
/// — QEMU raspi3b doesn't propagate R13 into x13 for AArch32→AArch64
/// exceptions, but the banked register reads back the right value.
///
/// LLVM AArch64 assembler doesn't accept the mnemonic `sp_svc`; we
/// emit the encoding directly (op0=3 op1=4 CRn=c4 CRm=c1 op2=0, Arm
/// ARM §D13.2.21 "SP_svc, Banked Stack Pointer, Supervisor mode").
fn read_sp_svc() -> u64 {
    let v: u64;
    // SAFETY: MRS from a defined banked system register at EL2.
    unsafe {
        core::arch::asm!(
            "mrs {}, S3_4_C4_C1_0",
            out(reg) v,
            options(nomem, nostack, preserves_flags),
        );
    }
    v
}
