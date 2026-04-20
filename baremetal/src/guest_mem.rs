//! Guest physical memory: ROM + RAM regions backing the Newton's address map.
//!
//! Guest physical layout we implement (first iteration):
//!
//!   0x0000_0000 .. 0x00FF_FFFF  ROM (16 MiB: 8 MiB low + 8 MiB "Opt. ROM")
//!   0x0400_0000 .. 0x043F_FFFF  RAM (4 MiB, MP2x00 default)
//!
//! The backing stores below are 2 MiB-aligned so stage-2 L2 block descriptors
//! can map them directly. All other guest physical regions are left unmapped
//! at stage-2 and fault into the EL2 trap handler.

use core::ptr::addr_of_mut;

use crate::kprintln;

// Size of each region, in bytes. Must be multiples of 2 MiB for the stage-2
// block-descriptor mapping strategy.
pub const ROM_SIZE: usize = 16 * 1024 * 1024;
pub const RAM_SIZE: usize = 4 * 1024 * 1024;

// 2 MiB alignment requirement on the backing stores.
const TWO_MIB: usize = 0x0020_0000;

#[repr(C, align(0x200000))]
struct Rom([u8; ROM_SIZE]);

#[repr(C, align(0x200000))]
struct Ram([u8; RAM_SIZE]);

static mut GUEST_ROM: Rom = Rom([0; ROM_SIZE]);
static mut GUEST_RAM: Ram = Ram([0; RAM_SIZE]);

// Big-endian ROM dump captured from hardware. Each 32-bit word is stored
// with the MSB first in memory. Guest runs little-endian, so we byteswap
// word-by-word during load.
static ROM_BE: &[u8] = include_bytes!("../roms/newton.rom");

/// Host physical base of the guest ROM backing store.
pub fn rom_host_pa() -> u64 {
    addr_of_mut!(GUEST_ROM) as u64
}

/// Host physical base of the guest RAM backing store.
pub fn ram_host_pa() -> u64 {
    addr_of_mut!(GUEST_RAM) as u64
}

const _: () = assert!(ROM_SIZE % TWO_MIB == 0);
const _: () = assert!(RAM_SIZE % TWO_MIB == 0);

/// Copy the embedded ROM into `GUEST_ROM`, byteswapping each 32-bit word to
/// produce the little-endian view the Newton CPU expects. Any ROM bytes
/// beyond the embedded file's length are left zero (so the 8 MiB "Opt. ROM"
/// half reads as zeros until we start supplying a real REx).
pub unsafe fn load_rom() {
    let rom_ptr = addr_of_mut!(GUEST_ROM) as *mut u32;
    let be_words = ROM_BE.len() / 4;

    kprintln!(
        "guest_mem: loading {} bytes of ROM (byteswap big-endian -> little-endian)",
        ROM_BE.len()
    );

    for i in 0..be_words {
        let off = i * 4;
        let word_be = u32::from_ne_bytes([
            ROM_BE[off],
            ROM_BE[off + 1],
            ROM_BE[off + 2],
            ROM_BE[off + 3],
        ]);
        let word_le = word_be.swap_bytes();
        // SAFETY: rom_ptr covers ROM_SIZE bytes; i*4 < ROM_BE.len() <= ROM_SIZE.
        unsafe { rom_ptr.add(i).write(word_le); }
    }

    kprintln!(
        "guest_mem: ROM @ host PA {:#x}, RAM @ host PA {:#x}",
        rom_host_pa(),
        ram_host_pa()
    );

    // First few decoded words, for sanity-checking that we installed the
    // vector table correctly. The reset vector is at guest PA 0.
    let first: u32 = unsafe { rom_ptr.read() };
    let second: u32 = unsafe { rom_ptr.add(1).read() };
    kprintln!(
        "guest_mem: ROM[0..2] (LE after swap) = {:#010x} {:#010x}",
        first, second
    );

    // Bring-up shim: patch the ROM's exception vectors (undef/SWI/P-abort/
    // D-abort/IRQ/FIQ) to `movs pc, lr` so early EL1 exceptions silently
    // return to the next instruction. Without this, any UNDEF fired by an
    // unimplemented CP15 op would branch into the ROM jump-table region,
    // which is only reachable via guest stage-1 — itself not yet set up.
    //
    // Ideal path once the CP15 shim is more complete: keep vectors pristine
    // and route everything to EL2 via a combination of HCR_EL2 trap bits
    // + an emulated guest stage-1 setup.
    unsafe {
        for i in 1..=6 {
            rom_ptr.add(i).write(0xE1B0_F00E); // movs pc, lr
        }
    }
    kprintln!(
        "guest_mem: patched exception vectors 1..=6 to `movs pc, lr` for bring-up"
    );
}
