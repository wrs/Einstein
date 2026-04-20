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
pub const FLASH_SIZE: usize = 8 * 1024 * 1024;       // MP2x00 internal store
pub const FRAMEBUFFER_SIZE: usize = 2 * 1024 * 1024; // enough for 320x480 several times over

// 2 MiB alignment requirement on the backing stores.
const TWO_MIB: usize = 0x0020_0000;

#[repr(C, align(0x200000))]
struct Rom([u8; ROM_SIZE]);

#[repr(C, align(0x200000))]
struct Ram([u8; RAM_SIZE]);

#[repr(C, align(0x200000))]
struct Flash([u8; FLASH_SIZE]);

#[repr(C, align(0x200000))]
struct Framebuffer([u8; FRAMEBUFFER_SIZE]);

static mut GUEST_ROM: Rom = Rom([0; ROM_SIZE]);
static mut GUEST_RAM: Ram = Ram([0; RAM_SIZE]);
static mut GUEST_FLASH: Flash = Flash([0xFF; FLASH_SIZE]); // erased flash reads as 0xFF
static mut GUEST_FB: Framebuffer = Framebuffer([0; FRAMEBUFFER_SIZE]);

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

/// Host physical base of the persistent flash backing store.
pub fn flash_host_pa() -> u64 {
    addr_of_mut!(GUEST_FLASH) as u64
}

/// Host physical base of the framebuffer RAM. Guest writes land here;
/// `dump_framebuffer_to_uart` prints a summary at any time.
pub fn fb_host_pa() -> u64 {
    addr_of_mut!(GUEST_FB) as u64
}

/// Emit a compact hex summary of the framebuffer to the UART for offline
/// inspection. Prints the first 512 bytes plus a histogram of non-zero
/// pages so a reviewer can see if the guest has actually drawn anything.
#[allow(dead_code)]
pub fn dump_framebuffer_to_uart() {
    use crate::kprintln;
    let ptr = addr_of_mut!(GUEST_FB) as *const u8;
    // SAFETY: framebuffer is statically allocated; we only read.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(ptr, FRAMEBUFFER_SIZE) };

    let nonzero_pages = bytes.chunks(4096).filter(|p| p.iter().any(|&b| b != 0)).count();
    let total_pages = FRAMEBUFFER_SIZE / 4096;
    kprintln!(
        "framebuffer: {} of {} pages non-zero ({} KiB with content)",
        nonzero_pages, total_pages, nonzero_pages * 4
    );
    // First 16 rows of 32 bytes, hex.
    for row in 0..16 {
        let off = row * 32;
        let mut s = [0u8; 32];
        s.copy_from_slice(&bytes[off..off + 32]);
        kprintln!(
            "  fb[{:#06x}]: {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x}",
            off,
            s[0],s[1],s[2],s[3], s[4],s[5],s[6],s[7],
            s[8],s[9],s[10],s[11], s[12],s[13],s[14],s[15],
            s[16],s[17],s[18],s[19], s[20],s[21],s[22],s[23],
            s[24],s[25],s[26],s[27], s[28],s[29],s[30],s[31],
        );
    }
}

const _: () = assert!(ROM_SIZE % TWO_MIB == 0);
const _: () = assert!(RAM_SIZE % TWO_MIB == 0);
const _: () = assert!(FLASH_SIZE % TWO_MIB == 0);
const _: () = assert!(FRAMEBUFFER_SIZE % TWO_MIB == 0);

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

    // Bring-up shim #1: patch the ROM's exception vectors (undef/SWI/P-abort/
    // D-abort/IRQ/FIQ) to `movs pc, lr` so early EL1 exceptions silently
    // return to the next instruction. Without this, any UNDEF fired by an
    // unimplemented CP15 op would branch into the ROM jump-table region,
    // which is only reachable via guest stage-1 — itself not yet set up.
    unsafe {
        for i in 1..=6 {
            rom_ptr.add(i).write(0xE1B0_F00E); // movs pc, lr
        }
    }
    kprintln!(
        "guest_mem: patched exception vectors 1..=6 to `movs pc, lr`"
    );

    // Bring-up shim #2: the 717006 kernel uses StrongARM's lax CP15 encoding
    // where CRm == CRn for most system-control registers. On ARMv7+ those
    // encodings are undefined (c1 c1 0, c2 c2 0, c3 c3 0, c5 c5 0, c6 c6 0),
    // so MMU setup silently fails on A53. Rewrite CRm -> 0 wherever we see
    // these patterns so the writes/reads land on the standard ARMv7
    // encoding (c1 c0 0, c2 c0 0, ...), which TVM/TRVM then trap into the
    // CP15 shim, which in turn applies them to real SCTLR_EL1 / TTBR0_EL1 /
    // DACR32_EL2 and so on.
    let patched = unsafe { patch_cp15_encodings(rom_ptr, ROM_SIZE / 4) };
    kprintln!(
        "guest_mem: rewrote {} CP15 c1/c2/c3/c5/c6 encodings (StrongARM CRm=n -> ARMv7 CRm=0)",
        patched
    );
}

/// Scan ROM words and rewrite MCR/MRC to CP15 c{1,2,3,5,6} with non-zero CRm
/// to the equivalent standard ARMv7 encoding with CRm=0. Returns the number
/// of patched words.
///
/// ARM data-processing-coprocessor encoding for MCR/MRC with opc2=0:
///   bits[31:28] = cond (any)
///   bits[27:24] = 0b1110
///   bit 20      = L (0 = MCR, 1 = MRC)
///   bits[23:21] = opc1 (we match 0)
///   bits[19:16] = CRn
///   bits[15:12] = Rt (any)
///   bits[11:8]  = 0b1111 (CP15)
///   bits[7:5]   = opc2 (we match 0)
///   bit 4       = 1
///   bits[3:0]   = CRm
unsafe fn patch_cp15_encodings(rom: *mut u32, word_count: usize) -> usize {
    let mut count = 0usize;
    for i in 0..word_count {
        // SAFETY: i < word_count matches ROM_SIZE/4.
        let w = unsafe { rom.add(i).read() };

        // Quick filter: CP15 coprocessor, opc1=0, opc2=0.
        // mask keeps: [27:20], [11:8], [7:4]; ignore cond, Rt, CRn, CRm.
        // We're matching (w & 0x0F_F0_0F_F0) == 0x0E_00_0F_10 for MCR/MRC.
        if (w & 0x0FE0_0FF0) != 0x0E00_0F10 {
            continue;
        }

        let crn = (w >> 16) & 0xF;
        let crm = w & 0xF;

        let interesting = matches!(crn, 1 | 2 | 3 | 5 | 6);
        if !interesting || crm == 0 {
            continue;
        }

        let new = w & !0xF; // CRm <- 0
        // SAFETY: same index, in-range.
        unsafe { rom.add(i).write(new); }
        count += 1;
    }
    count
}
