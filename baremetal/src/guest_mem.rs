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
pub const FRAMEBUFFER_SIZE: usize = 2 * 1024 * 1024; // enough for 320x480 several times over

// 2 MiB alignment requirement on the backing stores.
const TWO_MIB: usize = 0x0020_0000;

#[repr(C, align(0x200000))]
struct Rom([u8; ROM_SIZE]);

#[repr(C, align(0x200000))]
struct Ram([u8; RAM_SIZE]);

#[repr(C, align(0x200000))]
struct Framebuffer([u8; FRAMEBUFFER_SIZE]);

static mut GUEST_ROM: Rom = Rom([0; ROM_SIZE]);
static mut GUEST_RAM: Ram = Ram([0; RAM_SIZE]);
static mut GUEST_FB: Framebuffer = Framebuffer([0; FRAMEBUFFER_SIZE]);

// Big-endian ROM dump captured from hardware. Each 32-bit word is stored
// with the MSB first in memory. Guest runs little-endian, so we byteswap
// word-by-word during load.
#[cfg(not(nh_guest_test))]
static ROM_BE: &[u8] = include_bytes!("../roms/newton.rom");

// Einstein's REx goes into the second 8 MB of the 16 MB ROM region, at
// PA 0x00800000..0x01000000. Same big-endian → little-endian byteswap as
// the main ROM. Maps the Newton kernel's high-half VA 0x01000000 onwards
// once the guest programs its stage-1 to point there.
// See Emulator/ROM/TFlatROMImageWithREX.cpp:139-178 for the layout.
#[cfg(not(nh_guest_test))]
static REX_BE: &[u8] = include_bytes!("../../_Data_/Einstein.rex");

// Guest-test mode: `build.rs` picked up $NH_GUEST_TEST and set this cfg.
// The embedded bytes are an AArch32 flat binary with reset vector at
// offset 0, built by baremetal/guest-tests/scripts/build-tests.sh.
#[cfg(nh_guest_test)]
static GUEST_TEST_BIN: &[u8] = include_bytes!(env!("NH_GUEST_TEST_PATH"));

/// Host physical base of the guest ROM backing store.
pub fn rom_host_pa() -> u64 {
    addr_of_mut!(GUEST_ROM) as u64
}

/// Host physical base of the guest RAM backing store.
pub fn ram_host_pa() -> u64 {
    addr_of_mut!(GUEST_RAM) as u64
}

/// Host physical base of the framebuffer RAM. Guest writes land here;
/// `dump_framebuffer_to_uart` prints a summary at any time.
pub fn fb_host_pa() -> u64 {
    addr_of_mut!(GUEST_FB) as u64
}

/// Walk the guest's stage-1 L1 table at TTBR=0x0400_0000 and, for every
/// coarse L2 table we can reach, clear the XN (execute-never) bit on
/// entries whose type field is large/small page.
///
/// Rationale: ARMv4 second-level descriptors treat bit 15 as SBZ, but
/// ARMv7/v8 short-descriptor re-interpret the same bit as XN. The
/// 717006 ROM's prebuilt L2 tables happen to have bit 15 set in many
/// entries, so A53's stage-1 walker treats the corresponding ROM code
/// pages as non-executable and every instruction fetch aborts.
///
/// We walk the tables once, when the guest first writes TTBR0 (CP15
/// c2 c0 0). Tables in ROM are modified via our backing store — guests
/// see ROM as stage-2 read-only, but from EL2 we own the bytes.
pub fn fix_stage1_xn_bits() {
    use crate::kprintln;
    let ram = addr_of_mut!(GUEST_RAM) as *mut u32;
    let rom = addr_of_mut!(GUEST_ROM) as *mut u32;

    let mut l2_tables = 0usize;
    let mut patched = 0usize;
    let mut sections_patched = 0usize;
    let mut fine_to_fault = 0usize;

    // L1 sits at the start of guest RAM (TTBR0 = 0x0400_0000 per probe).
    for i in 0..4096 {
        // SAFETY: L1 is 16 KiB = 4096 × 4 bytes, at RAM[0..16384].
        let entry = unsafe { ram.add(i).read() };
        let typ = entry & 3;

        // Rewrite fine-table (0b11) descriptors to fault (0b00). The ARMv4
        // fine-table format was dropped in ARMv7 short descriptors; A53's
        // walker treats it as UNPREDICTABLE. The 717006 ROM installs three
        // fine-table L1 entries at VA 0x78000000 / 0x90000000 / 0xAC000000
        // as PCMCIA placeholders whose L2 slots are all fault (see
        // probe/FINDINGS.md). Converting to an L1 fault preserves intent:
        // any access to those VAs must raise a stage-1 translation fault
        // our abort handler can dispatch.
        if typ == 3 {
            // SAFETY: i < 4096.
            unsafe { ram.add(i).write(0); }
            fine_to_fault += 1;
            continue;
        }

        // Normalise section descriptor to minimal-valid ARMv7 form:
        // preserve PA (bits 31:20) + domain (8:5), clear XN/AP[2]/TEX/S/nG,
        // force AP[1:0] = 0b11 (RW both levels) + C/B = 1.
        if typ == 2 {
            let new = (entry & 0xFFF0_01E0) | 0x0000_0C0E;
            if new != entry {
                // SAFETY: i < 4096.
                unsafe { ram.add(i).write(new); }
                sections_patched += 1;
            }
        }

        // Normalise coarse descriptor: preserve L2 ptr (bits 31:10) + domain
        // (8:5), clear the ARMv4 SBO bits (4) and NS (3).
        if typ == 1 {
            let new = (entry & 0xFFFF_FC00) | (entry & 0x0000_01E0) | 0x01;
            if new != entry {
                // SAFETY: i < 4096.
                unsafe { ram.add(i).write(new); }
            }
        }

        if typ != 1 {
            continue; // only coarse L2 tables for the XN-on-page-entries pass
        }
        let l2_pa = (entry & 0xFFFF_FC00) as usize;
        // Pick backing store pointer by region.
        let (base, region_start, region_size) = if l2_pa < ROM_SIZE {
            (rom, 0usize, ROM_SIZE)
        } else if (0x04000000..0x04000000 + RAM_SIZE as u64)
            .contains(&(l2_pa as u64))
        {
            (ram, 0x04000000usize, RAM_SIZE)
        } else {
            continue;
        };
        let l2_idx_start = (l2_pa - region_start) / 4;
        if l2_idx_start + 256 > region_size / 4 {
            continue;
        }
        l2_tables += 1;

        // Coarse L2 has 256 entries, each 4 bytes. Rewrite each non-fault
        // entry into minimal valid ARMv7 form: preserve the PA, force
        // AP = 0b11 (RW both levels), C = B = 1, XN = 0. This strips the
        // ARMv4 subpage-permission bits which ARMv7 would reinterpret as
        // XN/AP[2]/TEX etc.
        for j in 0..256 {
            // SAFETY: bounds checked above.
            let ptr = unsafe { base.add(l2_idx_start + j) };
            let e = unsafe { ptr.read() };
            let typ = e & 3;
            let new = match typ {
                0 => continue,                         // fault, leave alone
                1 => (e & 0xFFFF_0000) | 0x0000_003D,  // large page, RW/RW, CB
                2 | 3 => (e & 0xFFFF_F000) | 0x0000_003E, // small page, XN=0
                _ => unreachable!(),
            };
            if new != e {
                unsafe { ptr.write(new); }
                patched += 1;
            }
        }
    }

    kprintln!(
        "fix_stage1_xn_bits: {} sections de-XN'd, {} L2 tables walked, {} L2 entries de-XN'd, {} fine -> fault",
        sections_patched, l2_tables, patched, fine_to_fault
    );
}

/// Dump the first 32 entries of the guest's stage-1 L1 page table, which we
/// assume lives at the start of guest RAM (TTBR0 = 0x0400_0000 per the
/// 717006 probe; stage-2 maps that IPA to the host ram backing). Each
/// entry covers 1 MiB of VA, so this is the VA 0..32 MiB window.
pub fn dump_guest_l1_table() {
    use crate::kprintln;
    let ram = addr_of_mut!(GUEST_RAM) as *const u32;
    let rom = addr_of_mut!(GUEST_ROM) as *const u32;
    kprintln!("guest L1 (TTBR=0x0400_0000) first 32 entries (each covers 1 MiB):");
    for i in 0..32 {
        // SAFETY: i < 32; guest L1 table is 4 KiB = 1024 entries so well
        // inside GUEST_RAM bounds.
        let entry = unsafe { ram.add(i).read() };
        let kind = match entry & 3 {
            0 => "fault",
            1 => "coarse",
            2 => "section",
            3 => "fine",
            _ => unreachable!(),
        };
        let va_start = (i as u32) << 20;
        if entry != 0 {
            kprintln!(
                "  L1[{:3}] VA {:#010x}+1MB = {:#010x} ({})",
                i, va_start, entry, kind
            );
            if (entry & 3) == 1 {
                let l2_pa = (entry & 0xFFFF_FC00) as usize;
                let src_ptr = if l2_pa < ROM_SIZE { rom }
                              else if (0x04000000..0x04400000).contains(&(l2_pa as u64)) {
                                  ram
                              } else { core::ptr::null() };
                if !src_ptr.is_null() {
                    kprintln!("         L2 table @ PA {:#x}:", l2_pa);
                    // print L2[0x00] and L2[0x18..0x1f] — the range covering
                    // VA 0x18000 where we see the fetches fail.
                    for j in [0usize, 0x18, 0x19, 0x1a, 0x1b] {
                        let off = (l2_pa & 0x00FF_FFFF) / 4 + j;
                        // SAFETY: l2_pa is in-bounds for the region we chose.
                        let e = unsafe { src_ptr.add(off).read() };
                        kprintln!("           L2[{:#04x}] = {:#010x}", j, e);
                    }
                }
            }
        }
    }
}

/// Emit a compact hex summary of a guest memory region to the UART.
#[allow(dead_code)]
pub fn dump_framebuffer_to_uart() {
    use crate::kprintln;
    let ptr = addr_of_mut!(GUEST_FB) as *const u8;
    // SAFETY: framebuffer is statically allocated; we only read.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(ptr, FRAMEBUFFER_SIZE) };
    summarise_region("framebuffer @ IPA 0x0E000000", bytes);
}

/// Dump a histogram + 16 rows of hex for the guest's RAM (at IPA
/// 0x0400_0000). This is our best proxy for a screenshot when the
/// kernel doesn't hand us an explicit framebuffer: whatever data
/// structures the kernel has populated in RAM show up here.
pub fn dump_ram_to_uart() {
    use crate::kprintln;
    let ptr = addr_of_mut!(GUEST_RAM) as *const u8;
    // SAFETY: static allocation.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(ptr, RAM_SIZE) };
    summarise_region("RAM @ IPA 0x04000000", bytes);
    kprintln!();
    kprintln!("First 512 bytes of kernel L1 page table at RAM offset 0:");
    hex_block(&bytes[0..512]);
}

fn summarise_region(label: &str, bytes: &[u8]) {
    use crate::kprintln;
    let page = 4096;
    let total_pages = bytes.len() / page;
    let nonzero = bytes.chunks(page).filter(|p| p.iter().any(|&b| b != 0)).count();
    let ff_pages = bytes.chunks(page).filter(|p| p.iter().all(|&b| b == 0xFF)).count();
    let active = nonzero.saturating_sub(ff_pages);
    kprintln!(
        "{}: {} pages populated ({} KiB), {} pages all-0xFF, {} pages mixed",
        label, nonzero, nonzero * (page / 1024), ff_pages, active
    );
    // 16 rows × 32 bytes at the start.
    hex_block(&bytes[0..(16 * 32)]);

    // If there's interesting content further in, show it.
    for chunk_start in [0x1000usize, 0x4000, 0x10000, 0x40000].iter().copied() {
        if chunk_start + 32 >= bytes.len() { continue; }
        if bytes[chunk_start..chunk_start + 256].iter().any(|&b| b != 0 && b != 0xFF) {
            kprintln!("  ... active at offset {:#x}:", chunk_start);
            hex_block(&bytes[chunk_start..chunk_start + 128]);
        }
    }
}

fn hex_block(bytes: &[u8]) {
    use crate::kprintln;
    for (row, chunk) in bytes.chunks(32).enumerate() {
        let off = row * 32;
        let mut line = [0u8; 32];
        let n = chunk.len().min(32);
        line[..n].copy_from_slice(&chunk[..n]);
        kprintln!(
            "  +{:#06x}: {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x}  {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x}",
            off,
            line[0],line[1],line[2],line[3], line[4],line[5],line[6],line[7],
            line[8],line[9],line[10],line[11], line[12],line[13],line[14],line[15],
            line[16],line[17],line[18],line[19], line[20],line[21],line[22],line[23],
            line[24],line[25],line[26],line[27], line[28],line[29],line[30],line[31],
        );
    }
}

const _: () = assert!(ROM_SIZE % TWO_MIB == 0);
const _: () = assert!(RAM_SIZE % TWO_MIB == 0);
const _: () = assert!(FRAMEBUFFER_SIZE % TWO_MIB == 0);

/// Copy the embedded ROM into `GUEST_ROM`, byteswapping each 32-bit word to
/// produce the little-endian view the Newton CPU expects. Any ROM bytes
/// beyond the embedded file's length are left zero (so the 8 MiB "Opt. ROM"
/// half reads as zeros until we start supplying a real REx).
pub unsafe fn load_rom() {
    #[cfg(nh_guest_test)]
    {
        return unsafe { load_guest_test() };
    }
    #[cfg(not(nh_guest_test))]
    {
        unsafe { load_newton_rom() }
    }
}

#[cfg(nh_guest_test)]
pub unsafe fn load_guest_test() {
    let rom_ptr = addr_of_mut!(GUEST_ROM) as *mut u8;
    kprintln!(
        "guest_mem: GUEST-TEST MODE — embedding {} bytes",
        GUEST_TEST_BIN.len()
    );
    for (i, b) in GUEST_TEST_BIN.iter().enumerate() {
        // SAFETY: i < GUEST_TEST_BIN.len() <= ROM_SIZE.
        unsafe { rom_ptr.add(i).write(*b); }
    }
    kprintln!(
        "guest_mem: guest-test @ host PA {:#x}, RAM @ host PA {:#x}",
        rom_host_pa(), ram_host_pa()
    );
    // No vector patching, no CP15 rewriting — guest-test binaries are
    // already ARMv7-correct.
}

#[cfg(not(nh_guest_test))]
pub unsafe fn load_newton_rom() {
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

    // Load Einstein's REx at PA 0x00800000 (= the second 8 MB of the 16 MB
    // ROM region). The kernel's stage-1 MMU maps this to VA 0x01000000
    // once it programs its page tables. Same BE->LE byteswap as the main
    // ROM, because the guest runs little-endian.
    const REX_PA_OFFSET: usize = 0x00800000;
    let rex_words = REX_BE.len() / 4;
    kprintln!(
        "guest_mem: loading {} bytes of Einstein.rex at PA {:#x} (byteswap BE->LE)",
        REX_BE.len(), REX_PA_OFFSET,
    );
    assert!(REX_BE.len() <= ROM_SIZE - REX_PA_OFFSET);
    let rex_base_word = REX_PA_OFFSET / 4;
    for i in 0..rex_words {
        let off = i * 4;
        let word_be = u32::from_ne_bytes([
            REX_BE[off],
            REX_BE[off + 1],
            REX_BE[off + 2],
            REX_BE[off + 3],
        ]);
        let word_le = word_be.swap_bytes();
        // SAFETY: rex_base_word + i stays below ROM_SIZE / 4 via the assert above.
        unsafe { rom_ptr.add(rex_base_word + i).write(word_le); }
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

    // UND vector (VA 0x04) + trampoline body: overwrite the ROM's
    // branch-to-REx-handler with a branch to a small AArch32 stub we
    // install at ROM offset 0x80. The stub saves R14_und and SPSR_und
    // to fixed RAM slots (0x04000400 / 0x04000404), then issues
    // HVC #UND_TAG so src/trap.rs::handle_und can decode and emulate
    // the faulting instruction. Without this the A53-only CP15 UNDs
    // (c15 c1 op2=2) and the Einstein UND opcodes would take the
    // REx handler's path, which our hypervisor isn't set up to
    // service. Phase A.2 of PLAN.md.
    // SAFETY: rom_ptr covers ROM_SIZE bytes; patch_und_vector writes
    // 4 bytes at offset 0x04 and 36 bytes starting at offset 0x80 —
    // both in the first 256 bytes of ROM, confirmed zero from offset
    // 0x58 onwards on Newton 2.x ROMs.
    unsafe { patch_und_vector(rom_ptr); }

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

/// Install the AArch32 UND-vector trampoline.
///
/// The trampoline body lives in the 16 MiB ROM region at offset
/// `UND_TRAMP_OFFSET` — well past the REx tail (Einstein.rex ends
/// ~0x0084_7000) and in guaranteed-zero padding that the kernel
/// can't plausibly touch. A 64-byte ROM region this deep is free
/// game for us. The vector at VA 0x04 branches to it.
///
/// An earlier iteration parked the body at ROM offset 0x80 (inside
/// the 256-byte header that reads as zeros in the raw dump). That
/// broke boot: the 717006 kernel reads that region as a table, so
/// turning zeros into instructions shifted the DABT/PABT loop the
/// boot gets stuck in. Moving the body far beyond the REx tail
/// avoids any such aliasing.
///
/// Trampoline body (arm-none-eabi-as disassembly):
///   +0x00:  e92d0003   push {r0, r1}
///   +0x04:  e59f0014   ldr r0, [pc, #20]      ; literal at +0x20
///   +0x08:  e580e000   str lr, [r0]            ; LR_und -> UND_SAVE_LR_IPA
///   +0x0C:  e14f1000   mrs r1, SPSR            ; r1 = SPSR_und
///   +0x10:  e5801004   str r1, [r0, #4]        ; SPSR -> UND_SAVE_SPSR_IPA
///   +0x14:  e8bd0003   pop {r0, r1}
///   +0x18:  e1400170   hvc #0x10               ; UND_TAG — enter EL2
///   +0x1C:  eafffffe   b .                     ; trap if we ever return
///   +0x20:  04000400   .word UND_SAVE_LR_IPA
///
/// Branch encoding at VA 0x04: `b UND_TRAMP_OFFSET`.
///   imm24 = (UND_TRAMP_OFFSET - (0x04 + 8)) / 4
///
/// Safety: caller must hold exclusive access to the ROM backing
/// store. Writes 9 words at the trampoline offset + 1 word at 0x04.
const UND_TRAMP_OFFSET: usize = 0x00FF_FF00;

unsafe fn patch_und_vector(rom: *mut u32) {
    let imm24 = ((UND_TRAMP_OFFSET as u32 - 0x0C) / 4) & 0x00FF_FFFF;
    let branch_insn = 0xEA00_0000 | imm24;

    // SAFETY: offsets below all sit in 0x00FF_FF00..0x00FF_FF24,
    // well under ROM_SIZE (= 16 MiB = 0x0100_0000).
    unsafe {
        rom.add(1).write(branch_insn);              // 0x04: b UND_TRAMP_OFFSET

        let base = UND_TRAMP_OFFSET / 4;
        rom.add(base).write(0xE92D_0003);           // push {r0, r1}
        rom.add(base + 1).write(0xE59F_0014);       // ldr r0, [pc, #20]
        rom.add(base + 2).write(0xE580_E000);       // str lr, [r0]
        rom.add(base + 3).write(0xE14F_1000);       // mrs r1, SPSR
        rom.add(base + 4).write(0xE580_1004);       // str r1, [r0, #4]
        rom.add(base + 5).write(0xE8BD_0003);       // pop {r0, r1}
        rom.add(base + 6).write(0xE140_0170);       // hvc #0x10
        rom.add(base + 7).write(0xEAFF_FFFE);       // b . (trap)
        rom.add(base + 8).write(0x0400_0400);       // literal: UND_SAVE_LR_IPA
    }
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
