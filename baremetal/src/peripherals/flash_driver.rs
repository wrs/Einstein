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

use crate::{hv::guest_mem, peripherals::flash, peripherals::guest_access, arch::trap_context::TrapContext};
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct FlashDriver;

impl NativeDriver for FlashDriver {
    /// Flash-driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0000;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

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

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        0x01 => identify(ctx, pc),
        // CleanUp / Init / InitializeDriverData / CleanUpDriverData /
        // StartReadingArray / DoneReadingArray / ResetBlockStatus /
        // LockBlock / ReportWriteResult — Einstein returns r0=0 with
        // no further work (all flash-chip state that the real hardware
        // would care about is hidden behind the native-prim layer on
        // emulation).
        0x02 | 0x03 | 0x04 | 0x05 | 0x06 | 0x07 | 0x0A | 0x0C | 0x0E => {
            ctx.x[0] = 0;
        }
        0x08 => write(ctx, pc),
        0x09 => start_erase(ctx, pc),
        0x0B => is_erase_complete(ctx, pc),
        0x0D => begin_write(ctx, pc),
        0x0F => do_write(ctx, pc),
        0x10 => do_erase(ctx, pc),
        _ => crate::diag::diag_util::halt_unknown_subfn(
            "flash_driver", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
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
        guest_access::write_word_or_halt(
            id_struct_addr + off, val, "flash_driver.Identify", pc);
    }
    ctx.x[0] = 1;
}

/// TEinsteinFlashDriver::Write(word=r1, mask=r2, addr=r3).
/// `flashRange` read from [sp+4] to detect 16-vs-32-bit access.
fn write(ctx: &mut TrapContext, pc: u32) {
    let word = ctx.x[1] as u32;
    let mask = ctx.x[2] as u32;
    let addr = ctx.x[3] as u32;

    // Einstein's TNativePrimitives reads `flashRange` from `*(r13 + 4)`
    // — the 5th argument the caller pushed before BL'ing into the ROM
    // vtable trampoline. The kernel makes that call from SVC mode, so
    // R13 there is R13_svc; per ARM ARM Table D1-79 R13_svc lives in
    // **X19** (`ctx.x[19]`), NOT X13 (which is SP_usr regardless of
    // source mode) and NOT SP_EL0/SP_EL1 (which are AArch64-only EL0/
    // EL1 stack pointers with no architectural alias to AArch32
    // banked R13). Reading the wrong slot was the historical bug
    // here.
    //
    // Workaround: every caller of the `TFlashDriver::Write` vtable
    // trampoline at 0x00384790 is a `T{8,16,32}BitFlashRange::DoWrite`
    // method whose prologue saves `this` into r4 (`mov r4, r0`). `this`
    // is the TFlashRange instance — the flashRange pointer. r4 is
    // callee-saved per AAPCS, so it survives the intermediate vtable
    // BL and the NATIVE_PRIM `stmdb sp!, {lr}` and still holds the
    // flashRange at MCR trap entry. Read it from ctx.x[4]. (Reading
    // *(R13_svc + 4) directly is also possible now via ctx.x[19] +
    // a guest_mem walk; the r4-cache approach is faster and less
    // tied to the kernel's stack layout.) If a future caller outside
    // DoWrite invokes TFlashDriver::Write, the vtable first-word
    // check below will catch the mismatch.
    let flash_range = ctx.x[4] as u32;
    let v_table = guest_access::read_word_or_halt(
        flash_range, "flash_driver.Write virtualTable", pc);
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
        // 16-bit path. Real Newton hardware with 16-bit flash wires
        // the CPU's 32-bit write-address bus to addresses flash at
        // 2x stride: `T16BitFlashRange::DoWrite` issues halfword
        // writes every 4 bytes because the memory controller only
        // accepts the upper-half lane of each 32-bit word. The
        // kernel's corresponding READ path (via `TFlashRange::Read`
        // /`memcpy` over the 0x30000000 alias) reads linearly from
        // the flash byte stream — so each 4-byte-stride write must
        // land at 2 consecutive flash bytes, NOT at one halfword
        // within a 4-byte word with the other halfword left 0xFF.
        //
        // Einstein handles this by mapping the 0x34000000 write
        // aperture through `TMemory::WriteToFlash16Bits`, which
        // divides the incoming physical address by 2 before writing
        // the halfword to the flash backing. We mirror that contraction
        // here: the halfword from the kernel lands at flash byte
        // `(pa - flash_base) / 2`, preserving a dense layout that
        // the subsequent linear read will match.
        let bank0_base = flash::BANK0_PA_BASE;
        let bank1_base = flash::BANK1_PA_BASE;
        let bank_base = if pa >= bank0_base && pa < bank0_base + flash::BANK_SIZE as u32 {
            bank0_base
        } else if pa >= bank1_base && pa < bank1_base + flash::BANK_SIZE as u32 {
            bank1_base
        } else {
            ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
            return;
        };
        let byte_off = (pa - bank_base) / 2;
        let hw = (word & 0x0000_FFFF) as u16;
        let m = (mask & 0x0000_FFFF) as u16;
        let contracted_pa = bank_base + (byte_off & !0x3);
        let (w, m32) = if byte_off & 0x2 != 0 {
            (hw as u32, m as u32)
        } else {
            ((hw as u32) << 16, (m as u32) << 16)
        };
        flash::program_word(contracted_pa, w, m32)
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

    let v_table = guest_access::read_word_or_halt(
        flash_range, "flash_driver.StartErase virtualTable", pc);
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
    guest_access::write_word_or_halt(
        result_addr, 0, "flash_driver.IsEraseComplete result", pc);
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

/// TEinsteinFlashDriver::DoWrite(word=r1, mask=r2, addr=r3,
/// startOfBlock=[sp+4]). Einstein's case 0x0F just logs and returns
/// r0=0 — `startOfBlock` is informational only. We mirror that
/// behaviour: the actual masked-word programming happens in the
/// `write` primitive (subfn 0x08); `begin_write` (0x0D) only
/// range-checks, and DoWrite is the kernel-side bookkeeping wrapper
/// around the per-word loop. Our state has nothing to update.
fn do_write(ctx: &mut TrapContext, _pc: u32) {
    ctx.x[0] = 0;
}

/// TEinsteinFlashDriver::DoErase(start=r1, size=r2). Erase a range
/// of bytes (set to 0xFF). Mirrors Einstein's case 0x10 in
/// `TNativePrimitives::ExecuteFlashDriverNative`: returns 0 on success,
/// `kError_Flash_AddressOutOfRange` if `start` is outside a flash bank
/// or the range doesn't fit.
fn do_erase(ctx: &mut TrapContext, _pc: u32) {
    let start = ctx.x[1] as u32;
    let size = ctx.x[2] as u32;

    let pa = match resolve_flash_pa(start) {
        Some(p) => p,
        None => {
            ctx.x[0] = ERR_FLASH_ADDR_OUT_OF_RANGE as u64;
            return;
        }
    };

    if flash::erase_block(pa, size) {
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

