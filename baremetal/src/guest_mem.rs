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

/// Raw big-endian on-disk bytes of the Newton ROM, pre-byteswap. Used by
/// `shadow_stub::patch_rom_from_bitmap` to verify the embedded classify
/// bitmap matches the current ROM.
#[cfg(not(nh_guest_test))]
pub fn rom_be_bytes() -> &'static [u8] {
    ROM_BE
}

/// Raw big-endian on-disk bytes of the external Einstein.rex, pre-byteswap.
#[cfg(not(nh_guest_test))]
pub fn rex_be_bytes() -> &'static [u8] {
    REX_BE
}

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

/// Framebuffer guest IPA base (stage-2 maps this to `fb_host_pa`).
pub const FB_IPA_BASE: u32 = 0x0E00_0000;
/// Framebuffer size in bytes.
pub const FB_SIZE: usize = FRAMEBUFFER_SIZE;

/// Guest RAM IPA base (stage-2 maps this to `ram_host_pa`).
pub const RAM_IPA_BASE: u32 = 0x0400_0000;

const RAM_BASE_USIZE: usize = RAM_IPA_BASE as usize;
const FB_BASE_USIZE: usize = FB_IPA_BASE as usize;

/// Read a 32-bit word from a guest physical address by resolving the
/// backing store directly. Returns None if `pa` is outside the ROM /
/// RAM / framebuffer regions we own. Pre-MMU (VA == IPA == PA) this
/// is exactly a guest load; post-MMU callers need to translate VA
/// to PA first (e.g. via `AT S12E1R`).
pub fn read_word_pa(pa: u32) -> Option<u32> {
    let pa = pa as usize;
    if pa + 4 <= ROM_SIZE {
        let host = (rom_host_pa() as usize) + pa;
        // SAFETY: bounds-checked against ROM backing.
        return Some(unsafe { core::ptr::read_volatile(host as *const u32) });
    }
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa)
        && pa + 4 <= RAM_BASE_USIZE + RAM_SIZE
    {
        let host = (ram_host_pa() as usize) + (pa - RAM_BASE_USIZE);
        // SAFETY: bounds-checked.
        return Some(unsafe { core::ptr::read_volatile(host as *const u32) });
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa)
        && pa + 4 <= FB_BASE_USIZE + FB_SIZE
    {
        let host = (fb_host_pa() as usize) + (pa - FB_BASE_USIZE);
        // SAFETY: bounds-checked.
        return Some(unsafe { core::ptr::read_volatile(host as *const u32) });
    }
    None
}

/// Read one halfword (16 bits) from a guest PA. Alignment is the
/// caller's responsibility; misaligned reads silently split across the
/// host pointer the way the CPU would cross bytes.
pub fn read_halfword_pa(pa: u32) -> Option<u16> {
    let pa = pa as usize;
    if pa + 2 <= ROM_SIZE {
        let host = (rom_host_pa() as usize) + pa;
        // SAFETY: bounds-checked.
        return Some(unsafe { core::ptr::read_volatile(host as *const u16) });
    }
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa)
        && pa + 2 <= RAM_BASE_USIZE + RAM_SIZE
    {
        let host = (ram_host_pa() as usize) + (pa - RAM_BASE_USIZE);
        // SAFETY: bounds-checked.
        return Some(unsafe { core::ptr::read_volatile(host as *const u16) });
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa)
        && pa + 2 <= FB_BASE_USIZE + FB_SIZE
    {
        let host = (fb_host_pa() as usize) + (pa - FB_BASE_USIZE);
        // SAFETY: bounds-checked.
        return Some(unsafe { core::ptr::read_volatile(host as *const u16) });
    }
    None
}

/// Write one halfword to a guest PA. Returns true on success; writes
/// to ROM / unmapped regions are refused.
pub fn write_halfword_pa(pa: u32, value: u16) -> bool {
    let pa = pa as usize;
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa)
        && pa + 2 <= RAM_BASE_USIZE + RAM_SIZE
    {
        let host = (ram_host_pa() as usize) + (pa - RAM_BASE_USIZE);
        unsafe { core::ptr::write_volatile(host as *mut u16, value); }
        return true;
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa)
        && pa + 2 <= FB_BASE_USIZE + FB_SIZE
    {
        let host = (fb_host_pa() as usize) + (pa - FB_BASE_USIZE);
        unsafe { core::ptr::write_volatile(host as *mut u16, value); }
        return true;
    }
    false
}

/// Read one byte from a guest PA.
pub fn read_byte_pa(pa: u32) -> Option<u8> {
    let pa = pa as usize;
    if pa < ROM_SIZE {
        let host = (rom_host_pa() as usize) + pa;
        return Some(unsafe { core::ptr::read_volatile(host as *const u8) });
    }
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa) {
        let host = (ram_host_pa() as usize) + (pa - RAM_BASE_USIZE);
        return Some(unsafe { core::ptr::read_volatile(host as *const u8) });
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa) {
        let host = (fb_host_pa() as usize) + (pa - FB_BASE_USIZE);
        return Some(unsafe { core::ptr::read_volatile(host as *const u8) });
    }
    None
}

/// Write a 32-bit word to a guest PA. Returns true on success. Writes
/// to ROM (or unmapped regions) are refused — callers should halt on
/// a false return if the write was supposed to succeed.
pub fn write_word_pa(pa: u32, value: u32) -> bool {
    let pa = pa as usize;
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa)
        && pa + 4 <= RAM_BASE_USIZE + RAM_SIZE
    {
        let host = (ram_host_pa() as usize) + (pa - RAM_BASE_USIZE);
        unsafe { core::ptr::write_volatile(host as *mut u32, value); }
        return true;
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa)
        && pa + 4 <= FB_BASE_USIZE + FB_SIZE
    {
        let host = (fb_host_pa() as usize) + (pa - FB_BASE_USIZE);
        unsafe { core::ptr::write_volatile(host as *mut u32, value); }
        return true;
    }
    false
}

/// Write a 32-bit word to a guest VA by walking the live stage-1
/// short-descriptor tables (rooted at TTBR0 = 0x0400_0000 per the
/// 717006 probe). Mirrors `trap::guest_translate_va`. Used from EL2
/// when we need to land a value in a kernel data structure named
/// by a VA the guest passed us (e.g. SFlashChipInformation pointer).
pub fn write_word_va(va: u32, value: u32) -> bool {
    let pa = match translate_va(va) {
        Some(p) => p,
        None => return false,
    };
    write_word_pa(pa, value)
}

/// Read a 32-bit word from a guest VA through the live stage-1 walk.
/// Mirrors `write_word_va`. Returns None when the VA is unmapped or
/// the translated PA lies outside a readable region.
pub fn read_word_va(va: u32) -> Option<u32> {
    let pa = translate_va(va)?;
    read_word_pa(pa)
}

/// Walk the guest stage-1 short-descriptor tables rooted at
/// TTBR0=0x0400_0000 and translate `va` to a guest PA. Handles L1
/// sections, L1 coarse-table references, and L2 large/small pages.
/// Returns None when the guest's stage-1 MMU is disabled (SCTLR.M=0)
/// — callers treat `va` as a PA in that case.
pub fn translate_va(va: u32) -> Option<u32> {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read is non-destructive.
    unsafe {
        core::arch::asm!(
            "mrs {}, sctlr_el1",
            out(reg) sctlr,
            options(nomem, nostack, preserves_flags),
        );
    }
    if sctlr & 1 == 0 {
        return None;
    }
    let l1_idx = (va >> 20) as usize;
    let l1_entry = read_word_pa(0x0400_0000 + (l1_idx as u32) * 4)?;
    match l1_entry & 3 {
        2 => Some((l1_entry & 0xFFF0_0000) | (va & 0x000F_FFFF)),
        1 => {
            let l2_pa = l1_entry & 0xFFFF_FC00;
            let l2_idx = (va >> 12) & 0xFF;
            let l2_entry = read_word_pa(l2_pa + l2_idx * 4)?;
            match l2_entry & 3 {
                1 => Some((l2_entry & 0xFFFF_0000) | (va & 0x0000_FFFF)),
                2 | 3 => Some((l2_entry & 0xFFFF_F000) | (va & 0x0000_0FFF)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Write one byte to a guest PA. See `write_word_pa` for semantics.
pub fn write_byte_pa(pa: u32, value: u8) -> bool {
    let pa = pa as usize;
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa) {
        let host = (ram_host_pa() as usize) + (pa - RAM_BASE_USIZE);
        unsafe { core::ptr::write_volatile(host as *mut u8, value); }
        return true;
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa) {
        let host = (fb_host_pa() as usize) + (pa - FB_BASE_USIZE);
        unsafe { core::ptr::write_volatile(host as *mut u8, value); }
        return true;
    }
    false
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

    // Only log when we actually rewrote something, to avoid flooding
    // the serial when the kernel re-enables stage-1 on every task
    // switch and we re-walk idempotently.
    if sections_patched != 0 || patched != 0 || fine_to_fault != 0 {
        crate::dprintln!(
            "fix_stage1_xn_bits: {} sections de-XN'd, {} L2 tables walked, {} L2 entries de-XN'd, {} fine -> fault",
            sections_patched, l2_tables, patched, fine_to_fault
        );
    }
}

/// ARMv7 short-descriptor section attributes for the shadow-stub
/// ScratchVA carve-out installed at the kernel VA
/// `crate::shadow_stub::SCRATCH_POOL_VA`. The section's PA bits encode
/// the IPA `SCRATCH_POOL_IPA`, which stage-2 then translates to the
/// host SCRATCH_POOL backing.
///
///   PA[31:20] = SCRATCH_POOL_IPA[31:20]  (stage-1 outputs this IPA)
///   AP[1:0] = 0b11   (RW from any mode, including USR)
///   AP[2]   = 0
///   domain  = 0      (matches kernel domain 0)
///   TEX     = 0, C/B = 0b11  (Normal cacheable WB)
///   XN      = 1      (instruction fetches PABT — defensive: scratch
///                    is data-only)
///   nG / S / NS = 0  (matches kernel section defaults)
///   bit[1] = 1, bit[0] = 0  (Section, PXN = 0)
///
/// Lower-19 attribute bits are 0x0C1E. Bit-by-bit cross-check against
/// DDI 0406C B3-19.
const SCRATCH_POOL_L1_SECTION_ATTRS: u32 = 0x0000_0C1E;
fn scratch_pool_l1_section() -> u32 {
    crate::shadow_stub::SCRATCH_POOL_IPA | SCRATCH_POOL_L1_SECTION_ATTRS
}

/// Install the kernel-side L1 mapping for the shadow-stub ScratchVA
/// scratch carve-out at VA `crate::shadow_stub::SCRATCH_POOL_IPA`. The
/// section descriptor identity-maps the VA to itself; stage-2 then
/// translates that IPA to the host `SCRATCH_POOL` backing.
///
/// Idempotent: rewrites the slot to `SCRATCH_POOL_L1_SECTION` even if
/// `fix_stage1_xn_bits` has just normalised it (clearing XN), so the
/// XN=1 invariant survives a re-walk.
///
/// Halts loud if the kernel has independently populated L1[0x18] with a
/// non-fault, non-matching entry (would mean a ROM revision actually
/// uses VA 0x0180_0000 — the plan's assumption breaks and a different
/// VA must be picked).
pub fn install_scratch_pool_l1_section() {
    let ram = addr_of_mut!(GUEST_RAM) as *mut u32;
    let idx = (crate::shadow_stub::SCRATCH_POOL_VA >> 20) as usize;

    // SAFETY: idx < 4096; GUEST_RAM holds the kernel L1 at TTBR0 = 0x0400_0000.
    let entry = unsafe { ram.add(idx).read() };

    let installed = scratch_pool_l1_section();
    // Acceptable pre-states:
    //   * Any type-0 (fault) entry — bits[1:0] == 0. The 717006 kernel
    //     leaves stray non-zero bits in unused L1 slots after soft-
    //     reset (e.g. observed `L1[0x18] = 0x00000010` on the second
    //     M=0→M=1 transition); the upper bits of a fault descriptor
    //     are don't-care for translation.
    //   * `installed` — our previous install survived re-walk
    //     untouched.
    //   * Normalised by fix_stage1_xn_bits to (entry & 0xFFF0_01E0) |
    //     0x0C0E — the walker flipped XN=1 → 0 inside our section.
    let normalised_after_walker: u32 =
        (installed & 0xFFF0_01E0) | 0x0000_0C0E;
    let is_fault_entry = (entry & 3) == 0;
    let acceptable =
        is_fault_entry
        || entry == installed
        || entry == normalised_after_walker;

    if !acceptable {
        kprintln!(
            "shadow_stub scratch: FATAL — kernel L1[{:#x}] = {:#010x}, type bits {:#x}; \
             not a fault entry and not our installed section. ROM revision uses VA {:#x}? \
             Pick a different SCRATCH_POOL_VA.",
            idx, entry, entry & 3, crate::shadow_stub::SCRATCH_POOL_VA,
        );
        crate::cpu::halt();
    }

    if entry != installed {
        // SAFETY: idx < 4096.
        unsafe { ram.add(idx).write(installed); }
        crate::dprintln!(
            "shadow_stub scratch: installed kernel L1[{:#x}] = {:#010x} (was {:#010x})",
            idx, installed, entry,
        );
    }
}

/// Manually walk the guest's stage-1 tables for a given VA and print
/// each level. Useful during Phase B debugging of stage-1 aborts we
/// don't see from EL2 — the diagnostic HVC handler calls this to show
/// what the guest's own page-table walker would have produced for the
/// faulting VA.
pub fn dump_stage1_walk(va: u32) {
    let ram = addr_of_mut!(GUEST_RAM) as *const u32;
    let rom = addr_of_mut!(GUEST_ROM) as *const u32;

    let l1_idx = (va >> 20) as usize;
    // L1 sits at the start of guest RAM (TTBR0 = 0x04000000 per probe).
    // SAFETY: l1_idx < 4096 and GUEST_RAM is 4 MiB so the whole 16 KiB
    // L1 table fits.
    let l1 = unsafe { ram.add(l1_idx).read() };
    let ty = l1 & 3;
    let tname = match ty { 0=>"fault", 1=>"coarse", 2=>"section", 3=>"fine", _=>"?" };
    kprintln!(
        "  stage1 walk VA={:#010x}:  L1[{:#x}] = {:#010x}  ({})",
        va, l1_idx, l1, tname
    );
    if ty == 2 {
        let pa_base = l1 & 0xFFF0_0000;
        let pa = pa_base | (va & 0x000F_FFFF);
        kprintln!(
            "    section → PA {:#010x}  AP[1:0]={:#x} AP[2]={} XN={} domain={:#x}",
            pa, (l1 >> 10) & 3, (l1 >> 15) & 1, (l1 >> 4) & 1, (l1 >> 5) & 0xF
        );
    }
    if ty == 1 {
        let l2_pa = (l1 & 0xFFFF_FC00) as usize;
        // Pick backing store.
        let (base, region_start) = if l2_pa < ROM_SIZE {
            (rom, 0usize)
        } else if (0x04000000..0x04000000 + RAM_SIZE as u64).contains(&(l2_pa as u64)) {
            (ram, 0x04000000usize)
        } else {
            kprintln!("    coarse L2 @ PA {:#x} — no backing store mapped", l2_pa);
            return;
        };
        let l2_off = (l2_pa - region_start) / 4;
        let l2_idx = ((va >> 12) & 0xFF) as usize;
        // SAFETY: l2_off + l2_idx < (region_size / 4) for all valid L2 tables
        // we've produced; fix_stage1_xn_bits enforces the same bound.
        let l2 = unsafe { base.add(l2_off + l2_idx).read() };
        let l2_ty = l2 & 3;
        let l2_name = match l2_ty { 0=>"fault", 1=>"large", 2|3=>"small", _=>"?" };
        kprintln!(
            "    coarse L2 @ PA {:#x}, L2[{:#x}] = {:#010x}  ({})",
            l2_pa, l2_idx, l2, l2_name
        );
        let pa = match l2_ty {
            1 => (l2 & 0xFFFF_0000) | (va & 0x0000_FFFF), // large page
            2 | 3 => (l2 & 0xFFFF_F000) | (va & 0x0000_0FFF), // small page
            _ => 0,
        };
        if l2_ty != 0 {
            kprintln!("    → PA {:#010x}", pa);
        }
    }
}

/// Dump the first 32 entries of the guest's stage-1 L1 page table, which we
/// assume lives at the start of guest RAM (TTBR0 = 0x0400_0000 per the
/// 717006 probe; stage-2 maps that IPA to the host ram backing). Each
/// entry covers 1 MiB of VA, so this is the VA 0..32 MiB window.
pub fn dump_guest_l1_table() {
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
    let ptr = addr_of_mut!(GUEST_FB) as *const u8;
    // SAFETY: framebuffer is statically allocated; we only read.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(ptr, FRAMEBUFFER_SIZE) };
    summarise_region("framebuffer @ IPA 0x0E000000", bytes);
}

/// Dump a histogram + 16 rows of hex for the guest's RAM (at IPA
/// 0x0400_0000). This is our best proxy for a screenshot when the
/// kernel doesn't hand us an explicit framebuffer: whatever data
/// structures the kernel has populated in RAM show up here.
#[allow(dead_code)]
pub fn dump_ram_to_uart() {
    let ptr = addr_of_mut!(GUEST_RAM) as *const u8;
    // SAFETY: static allocation.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(ptr, RAM_SIZE) };
    summarise_region("RAM @ IPA 0x04000000", bytes);
    kprintln!();
    kprintln!("First 512 bytes of kernel L1 page table at RAM offset 0:");
    hex_block(&bytes[0..512]);
}

fn summarise_region(label: &str, bytes: &[u8]) {
    let page = 4096;
    let _total_pages = bytes.len() / page;
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
    // Make the freshly-written bytes visible to the guest's instruction
    // fetcher. Without this the I-cache misses, hits memory, and reads
    // pre-init zeros (the writes are still in the D-cache).
    crate::cpu::icache_publish_range(rom_ptr as u64, GUEST_TEST_BIN.len());
    kprintln!(
        "guest_mem: guest-test @ host PA {:#x}, RAM @ host PA {:#x}",
        rom_host_pa(), ram_host_pa()
    );
    // Install the UND trampoline so shadow-byte-access UDF markers,
    // guest_bp UDFs, and tracer USR-fallback UDFs reach EL2. The ROM
    // patching that `load_newton_rom` does to rewrite CP15 encodings
    // is still skipped — guest-test binaries are already ARMv7-correct.
    unsafe {
        patch_und_vector(addr_of_mut!(GUEST_ROM) as *mut u32);
    }
    // Don't install the DABT trampoline here: test_cp15_fault_regs
    // installs its own VA 0x10 handler to probe the CP15 shim's DFAR /
    // DFSR pass-through, and unconditionally patching would break it.
    // Tests that want the hypervisor's alignment-fault emulator (e.g.
    // test_rotate_ldr) hand-roll the trampoline shape inline so the
    // DABT enters EL2 via HVC #ALIGN_TAG the same way the real ROM
    // path does.
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

    // Patch the external REx's id field to one past the last embedded-REx
    // id. Mirrors Einstein/Emulator/ROM/TROMImage.cpp::LookForREXes
    // (line 311-313): "Patch the REx to have a sequential ID, or NewtonOS
    // will be very confused and erase the user's Flash image." The 717006
    // ROM has exactly one embedded REx (id=0) living at base_size
    // 0x71FC4C, so the first external REx at 0x00800000 must claim id=1.
    // Without the patch, NewtonOS's PrimNextRExConfigEntry indexes a
    // per-id config table and never finds our REx — SearchForFlashDrivers
    // therefore never sees the 'fdrv' entry that registers
    // TEinsteinFlashDriver, and the kernel falls back to the built-in
    // T28F016_SA_SVDriver whose Identify fails against our stub flash.
    //
    // REx header layout (offsets from block start):
    //   +0x00 "RExBlock" magic (8 bytes)
    //   +0x08 checksum
    //   +0x0C header version (=1)
    //   +0x10 manufacturer ('Eins')
    //   +0x14 version
    //   +0x18 size
    //   +0x1C id             <-- the field we patch
    //   +0x20 startAddr
    //   +0x24 numEntries
    const NUM_EMBEDDED_REXES_717006: u32 = 1;
    let rex_id_word_index = rex_base_word + (0x1C / 4);
    // SAFETY: rex_id_word_index < rex_base_word + 8 < ROM_SIZE / 4 (checked by assert above).
    unsafe {
        let old_id = rom_ptr.add(rex_id_word_index).read();
        rom_ptr.add(rex_id_word_index).write(NUM_EMBEDDED_REXES_717006);
        kprintln!(
            "guest_mem: Einstein.rex id patch {} -> {} (first free slot after embedded REx)",
            old_id, NUM_EMBEDDED_REXES_717006,
        );
    }

    // Rewrite NATIVE_PRIM call sites in the REx from Rd=LR to Rd=R12.
    //
    // Einstein's Drivers/NativePrimitives.s macro emits:
    //     stmdb sp!, {lr}
    //     mov   lr, #id                ; or: ldr lr, [pc, #4]; .word native_insn
    //     [add  lr, lr, #impl*0x100]
    //     mcr   p10, 0, lr, c0, c0, 0  ; Rd = 14 (LR) — current-mode banked
    //     ldmia sp!, {pc}
    //
    // The Newton kernel makes these calls in SVC mode, so AArch32 R14
    // is R14_svc. Per ARM ARM DDI 0487 D1.21.1 Table D1-79 the AArch64
    // GPR file aliases AArch32 R14_svc as **X18**, not X14 — so an
    // EL2 trap handler that reads `ctx.x[14]` for the MCR's Rd value
    // would get LR_usr (whatever the user-mode return address was),
    // not the native-call ID the preceding MOV wrote into LR_svc.
    //
    // The original `handle_fp_simd` decodes the MCR encoding's Rd
    // field (an AArch32 register number, 0-15) and reads `ctx.x[Rd]`
    // — which is the AArch64 view of R<Rd>_usr, never the source
    // mode's banked R14. So Rd=14 in SVC mode would deliver LR_usr,
    // not LR_svc, and every native primitive would decode as garbage.
    //
    // Fix at load time: rewrite every MCR p10 Rd=LR in the REx to use
    // Rd=R12 (IP, non-banked: R12_usr lives in X12, and X12 ≡ AArch32
    // R12 across all non-FIQ modes per Table D1-79 — also AAPCS call-
    // clobbered, so no caller is disturbed). The 32-bit MCR encoding
    // only changes bits [15:12] (Rd); we also rewrite the matching
    // MOV / ADD / LDR that produced LR's value to target R12 instead
    // (the DP-immediate encodings additionally change Rn bits [19:16]
    // on the ADD form). LR is still pushed/popped by the outer
    // STMDB/LDMIA so control-flow return is unchanged.
    //
    // (A more general fix would be to teach `handle_fp_simd` to map
    // Rd → ctx slot via Table D1-79 using the source mode in
    // SPSR_EL2; the rewrite path is kept because it gives a smaller
    // and more localised hot path on every native-primitive call.)
    //
    // See INVESTIGATION.md for the debug trace that exposed this.
    // SAFETY: operates within the REx window we just loaded; bounds
    // checked against REX_BE.len().
    unsafe {
        let patched = patch_native_prim_mcr_lr_to_r12(
            rom_ptr,
            REX_PA_OFFSET as u32,
            (REX_PA_OFFSET + REX_BE.len()) as u32,
        );
        kprintln!(
            "guest_mem: rewrote {} NATIVE_PRIM MCR/MOV/ADD/LDR sites in REx (Rd=lr → Rd=r12)",
            patched,
        );
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

    // Phase A baseline: Einstein's word-write ROM patches. Skipping
    // these left the kernel in the wrong boot path during Phase B —
    // see src/rom_patches.rs for the list and rationale.
    unsafe { crate::rom_patches::apply_717006_patches(rom_ptr); }

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

    // Install the DABT-vector intercept. See `patch_dabt_vector` below.
    unsafe { patch_dabt_vector(rom_ptr); }

    // TEMPORARY diagnostic — PABT-vector intercept.
    // The stock ROM vector at VA 0x0C branches to 0x01A00010 (a HAL
    // REx address that our image doesn't back). Patch to HVC #DIAG_TAG
    // so any prefetch abort halts with a full banked-reg dump and we
    // can see the faulting fetch PC (= LR_abt − 4 for ARM).
    unsafe { rom_ptr.add(3).write(0xE140_0171); } // hvc #0x11

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

    // Register the tracer; actual ROM patching is deferred until the
    // guest turns on its stage-1 MMU (see src/tracer.rs for why).
    #[cfg(feature = "trace")]
    crate::tracer::init();
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
/// Trampoline body:
///   +0x00: ee0dcf50  mcr p15,0,r12,c13,c0,2 ; TPIDRURW <- R12 (save orig R12)
///   +0x04: e59fc050  ldr r12, [pc, #0x50]  ; literal at +0x5C: save VA
///   +0x08: e58c000c  str r0, [r12, #0x0C]  ; save pre-UND R0      (+0x0C)
///   +0x0C: e58c1010  str r1, [r12, #0x10]  ; save pre-UND R1      (+0x10)
///   +0x10: e58ce000  str lr, [r12]         ; save R14_und         (+0x00)
///   +0x14: e14f0000  mrs r0, SPSR          ; r0 = SPSR_und
///   +0x18: e58c0004  str r0, [r12, #4]     ; save SPSR_und        (+0x04)
///   +0x1C: e58c2014  str r2, [r12, #0x14]  ; save pre-UND R2      (+0x14)
///   +0x20: e200101f  and r1, r0, #0x1F     ; r1 = faulting mode bits
///   +0x24: e38110c0  orr r1, r1, #0xC0     ; r1 |= I/F mask
///   +0x28: e35100d0  cmp r1, #0xD0         ; == USR (0x10) + IF ?
///   +0x2C: 03a010df  moveq r1, #0xDF       ; if USR → use SYS (same bank)
///   +0x30: e129f001  msr cpsr_c, r1        ; switch to faulting mode
///   +0x34: e58cd018  str sp, [r12, #0x18]  ; save banked SP       (+0x18)
///   +0x38: e58ce01c  str lr, [r12, #0x1C]  ; save banked LR       (+0x1C)
///   +0x3C: e321f0db  msr cpsr_c, #0xdb     ; → UND (I/F masked)
///   +0x40: e59c2014  ldr r2, [r12, #0x14]  ; restore pre-UND R2
///   +0x44: e321f0d3  msr cpsr_c, #0xd3     ; → SVC (I/F masked)
///   +0x48: e1a0000e  mov r0, lr            ; r0 = R14_svc
///   +0x4C: e58c0008  str r0, [r12, #8]     ; save LR_svc          (+0x08)
///   +0x50: e321f0db  msr cpsr_c, #0xdb     ; → UND (I/F masked)
///   +0x54: e1400170  hvc #0x10             ; UND_TAG — enter EL2
///   +0x58: eafffffe  b .                   ; trap if we ever return
///   +0x5C: 0c004f00  .word UND_SAVE_BASE_VA (RAM-mirror VA)
///
/// Historical note on the SVC bounce: per ARM ARM Table D1-79,
/// AArch32 R14_svc is the AArch64 X18 register at AArch32→AArch64
/// exception entry (and `ELR_EL1` is an AArch64-only EL1 register
/// with no architectural alias to R14_svc). The trampoline could
/// therefore read LR_svc directly from `ctx.x[18]` at EL2 entry,
/// without the brief `msr cpsr_c, #0xd3` mode bounce. The bounce is
/// kept for now because shadow_stub's faulting-mode SP/LR snapshot
/// also runs from this trampoline path and benefits from in-mode
/// reads when the faulting mode isn't UND/USR; revisiting after
/// Phase B for cleanup.
/// `MRS X, LR_svc` is **NOT** a defined AArch64 sysreg encoding —
/// MRS (Banked register) is AArch32-only per F7.1.115 — so reads of
/// `LR_svc` as if it were a sysreg always come back as 0 / undefined
/// regardless of platform; that was a misdiagnosis.
///
/// Why save R0 and R1 first: the trampoline clobbers R0 (to hold the
/// save-slot VA for the SPSR/LR stores) and R1 (to carry SPSR_und
/// across the mode bounce). Without persisting the pre-UND values,
/// the guest's first two argument registers are scrambled whenever
/// the tracer UDFs a function entry — caught in Phase B as a bogus
/// PA 0x78 write from StoreToPhysAddress, which was actually
/// AddPgPAndPermWithPageTable's prologue shuffling the clobbered R0
/// (0x0C00_4F00) and R1 (LR_svc) into R7 and R4 before using them
/// as a page-table base. `handle_und` restores `ctx.x[0]` and
/// `ctx.x[1]` from these slots at entry; by the time execution ERETs
/// back to the guest the registers are intact. R12 is preserved by the
/// opening `MCR p15,0,r12,c13,c0,2` which stashes the original R12 into
/// TPIDRURW (TPIDR_EL0 in AArch64); `handle_und` reads `tpidr_el0` and
/// restores `ctx.x[12]`. TPIDRURW is ARMv6+ architectural state that
/// SA-1100 (ARMv4) did not have, and the Newton ROM never touches it,
/// so claiming it as the R12 save slot is safe. This matters for the
/// shadow-byte-access UDF-trap path, where the faulting instruction
/// can legitimately use R12 as base/data/offset; the tracer's
/// function-entry assumption (`MOV R12, R13` on every prologue) does
/// not hold for mid-function sites.
///
/// Branch encoding at VA 0x04: `b UND_TRAMP_OFFSET`.
///   imm24 = (UND_TRAMP_OFFSET - (0x04 + 8)) / 4
///
/// Note: the guest's stage-1 L1[0x0F] maps VA 0x00F00000-
/// 0x00FFFFFF identity to the ROM, so VA 0x00FFFF00 is the PC
/// the CPU lands at. The literal holds a VA, which the guest's
/// stage-1 translates through L1[0xC0] coarse -> L2[0x04] small
/// page -> PA 0x04005F00 (RAM). We can't use the raw IPA 0x04005F00
/// as the literal because the guest's L1[0x40] section maps VA
/// 0x0400_xxxx to PA 0x0000_xxxx (ROM, RO under stage-2) post-MMU.
///
/// Safety: caller must hold exclusive access to the ROM backing
/// store. Writes 13 words at the trampoline offset + 1 word at 0x04.
const UND_TRAMP_OFFSET: usize = 0x00FF_FF00;

/// Post-emulation trampoline used by the SBA handler when byte-access
/// writeback targets Rn ∈ {13, 14} (banked SP / LR). AArch64 ERET from
/// EL2 doesn't propagate x13 / x14 into the target mode's banked SP /
/// LR — R0..R12 propagate, R13/R14 retain their banked values across
/// the ERET. So we instead ERET into this trampoline *in the faulting
/// mode*, which writes SP / LR natively (hitting the banked slot for
/// that mode) and then branches to the final PC. NEW_SP / NEW_LR live
/// in the `UND_SAVE_BANKED_{SP,LR}_IPA` RAM slots; the NEW_PC literal
/// lives inline in the trampoline body and the SBA handler rewrites it
/// (plus a DC CVAU flush) before each ERET.
///
/// Trampoline body (7 words + 2 literals):
///   +0x00: ee0dcf50  mcr p15,0,r12,c13,c0,2  ; save R12 → TPIDRURW
///   +0x04: e59fc014  ldr r12, [pc, #0x14]    ; R12 = slot-base literal at +0x20
///   +0x08: e59cd018  ldr sp, [r12, #0x18]    ; SP = NEW_SP slot
///   +0x0C: e59ce01c  ldr lr, [r12, #0x1C]    ; LR = NEW_LR slot
///   +0x10: ee1dcf50  mrc p15,0,r12,c13,c0,2  ; R12 = orig R12 from TPIDRURW
///   +0x14: e59ff008  ldr pc, [pc, #0x08]     ; branch via NEW_PC literal at +0x24
///   +0x18: eafffffe  b .                     ; guard
///   +0x1C: eafffffe  b .                     ; guard
///   +0x20: slot_base_va                      ; set at install, swapped post-MMU
///   +0x24: NEW_PC                            ; dynamically written by SBA handler
pub const SBA_POST_TRAMP_OFFSET: usize = 0x00FF_FF80;
pub const SBA_POST_TRAMP_NEW_PC_OFFSET: usize = SBA_POST_TRAMP_OFFSET + 0x24;

/// DABT-vector trampoline body. Installed at ROM offset 0x00FF_FFA8
/// (past the SBA post-emulation trampoline at 0x00FF_FF80, ends
/// around 0x00FF_FFA8). Saves LR_abt/SP_abt/SPSR_abt natively from
/// ABT mode, then bounces to SVC to save SP_svc/SPSR_svc/LR_svc.
///
/// (Historical note: per Table D1-79, AArch32 R13_svc / R14_svc /
/// SPSR_svc are reachable from AArch64 EL2 as `ctx.x[19]` / `ctx.x[18]`
/// / `spsr_el1` respectively, so the SVC bounce is no longer
/// strictly necessary. The trampoline path is retained because the
/// alignment-fault fast path's HVC-entry handler reads from
/// `DABT_SAVE_PA` directly; refactoring that to use ctx.x[] is a
/// follow-up.)
///
/// The literal at the end of the trampoline is swapped between
/// pre/post-MMU VAs by `install_und_vector_swap_{pre,post}_mmu`.
///
/// Save layout at DABT_SAVE_PA:
///   +0x00: LR_abt
///   +0x04: SP_abt
///   +0x08: SPSR_abt (= pre-abort CPSR)
///   +0x0C: SP_svc
///   +0x10: SPSR_svc
///   +0x14: LR_svc
pub const DABT_TRAMP_OFFSET: usize = 0x00FF_FFA8;
pub const DABT_SAVE_PA: u32 = 0x0400_5FA0;

/// Install the DABT-vector intercept stub at `DABT_TRAMP_OFFSET` and
/// patch VA 0x10 to branch to it. Serves two roles:
///   (1) `HVC #DIAG_TAG` for Phase-B debugging: halt with banked-reg
///       dump on any unexpected DABT the kernel doesn't own.
///   (2) `HVC #ALIGN_TAG` for hypervisor-wide rotate-LDR emulation.
///       SCTLR.A=1 (forced by our CP15 shim) means every unaligned
///       LDR/STR alignment-faults here; the handler decodes+emulates
///       SA-1100 rotate-LDR semantics and ERETs past the faulting insn.
///
/// Stub layout (15 words = 60 bytes). Saves R0 / R1 to TPIDR scratch
/// regs and LR_abt / SP_abt / SPSR_abt to a fixed RAM slot *before*
/// the DFSR check, because the alignment-fault fast path needs the
/// pre-abt mode bits and faulting PC available to AArch64 EL2 from
/// guaranteed-stable storage rather than from `mrs spsr_abt` (which
/// is fine on FVP but historically unreliable on QEMU raspi3b — see
/// Bug #1 in docs/QEMU_BUGS.md). LR_abt / SP_abt themselves are
/// also available in `ctx.x[20]` / `ctx.x[21]` per Table D1-79;
/// keeping the RAM stash simplifies the trampoline → fast-path
/// handoff (the trampoline writes them anyway as part of its
/// ABT-mode-native register save).
///
///   +0x00: ee0d_0f50  mcr p15,0,r0,c13,c0,2  ; save r0 → TPIDRURW
///   +0x04: ee0d_1f70  mcr p15,0,r1,c13,c0,3  ; save r1 → TPIDRRO
///   +0x08: e59f_0028  ldr r0, [pc, #0x28]    ; r0 = DABT_SAVE_VA literal
///   +0x0C: e580_e000  str lr, [r0]           ; save LR_abt  @ +0x00
///   +0x10: e580_d004  str sp, [r0, #4]       ; save SP_abt  @ +0x04
///   +0x14: e14f_1000  mrs r1, spsr           ; r1 = SPSR_abt
///   +0x18: e580_1008  str r1, [r0, #8]       ; save SPSR_abt @ +0x08
///   +0x1C: ee15_0f10  mrc p15,0,r0,c5,c0,0   ; r0 = DFSR
///   +0x20: e200_000f  and r0, r0, #0xF       ; mask FS[3:0]
///   +0x24: e350_0001  cmp r0, #1             ; alignment fault?
///   +0x28: 0a00_0000  beq align_path (+0x30) ; → HVC #ALIGN_TAG
///   +0x2C: e140_0171  hvc #0x11 (DIAG_TAG)
///   +0x30: e140_0173  hvc #0x13 (ALIGN_TAG) — align path target
///   +0x34: eaff_fffe  b .                    ; guard
///   +0x38: literal     DABT_SAVE_VA
///
/// DABT_SAVE layout at IPA 0x0400_5FA0 (pre-MMU) / VA 0x0C00_4FA0
/// (post-MMU):
///   +0x00: LR_abt    (= faulting_pc + 8 for ARM DABT)
///   +0x04: SP_abt
///   +0x08: SPSR_abt  (= pre-abt CPSR)
///
/// The pre/post-MMU literal swap is piggy-backed on the UND
/// trampoline's swap in `install_und_vector_swap_{pre,post}_mmu`.
///
/// SAFETY: writes 1 word at VA 0x10 + 15 words in the ROM tail
/// reserved region; caller must own the ROM backing.
pub unsafe fn patch_dabt_vector(rom_ptr: *mut u32) {
    unsafe {
        let imm24 = ((DABT_TRAMP_OFFSET as u32).wrapping_sub(0x10 + 8) / 4) & 0x00FF_FFFF;
        let branch_insn = 0xEA00_0000 | imm24;
        rom_ptr.add(4).write(branch_insn);              // 0x10: b DABT_TRAMP_OFFSET

        let db = DABT_TRAMP_OFFSET / 4;
        rom_ptr.add(db +  0).write(0xEE0D_0F50);         // mcr p15,0,r0,c13,c0,2
        rom_ptr.add(db +  1).write(0xEE0D_1F70);         // mcr p15,0,r1,c13,c0,3
        rom_ptr.add(db +  2).write(0xE59F_0028);         // ldr r0, [pc, #0x28] → DABT_SAVE_VA
        rom_ptr.add(db +  3).write(0xE580_E000);         // str lr, [r0]           LR_abt
        rom_ptr.add(db +  4).write(0xE580_D004);         // str sp, [r0, #4]       SP_abt
        rom_ptr.add(db +  5).write(0xE14F_1000);         // mrs r1, spsr
        rom_ptr.add(db +  6).write(0xE580_1008);         // str r1, [r0, #8]       SPSR_abt
        rom_ptr.add(db +  7).write(0xEE15_0F10);         // mrc p15,0,r0,c5,c0,0   DFSR
        rom_ptr.add(db +  8).write(0xE200_000F);         // and r0, r0, #0xF
        rom_ptr.add(db +  9).write(0xE350_0001);         // cmp r0, #1
        rom_ptr.add(db + 10).write(0x0A00_0000);         // beq +0x0 (word 12 = ALIGN hvc)
        rom_ptr.add(db + 11).write(0xE140_0171);         // hvc #0x11 (DIAG_TAG)
        rom_ptr.add(db + 12).write(0xE140_0173);         // hvc #0x13 (ALIGN_TAG)
        rom_ptr.add(db + 13).write(0xEAFF_FFFE);         // b . (guard)
        rom_ptr.add(db + 14).write(0x0400_5FA0);         // literal (pre-MMU IPA)
    }
}

/// `movs pc, lr` stub in the ROM trampoline region. See the installation
/// site in `patch_und_vector` and `return_to_guest_from_und` in trap.rs
/// for rationale. Must not overlap the DABT trampoline, which spans
/// `DABT_TRAMP_OFFSET .. DABT_TRAMP_OFFSET + 15*4`  (= 0x00FF_FFA8 ..
/// 0x00FF_FFE4 inclusive of the literal word at `db+14`). Placing the
/// stub at the first aligned word past that literal keeps both
/// trampolines non-overlapping; the stub is 3 words (12 bytes) so it
/// ends at 0x00FF_FFF0, still inside ROM.
///
/// Prior layout placed the stub at 0x00FF_FFE0, which coincided
/// byte-for-byte with the DABT-trampoline's literal slot. On QEMU
/// raspi3b the clobbered first word (0x0400_5FA0 / 0x0C00_4FA0, written
/// by `install_und_vector_swap_*`) happened to decode as an
/// EQ-conditional LDC that the TCG model treated as a NOP. On FVP Base
/// RevC the same encoding raises an UNDEFINED exception, so the UND
/// return path halted with an "unrecognised UND" in early boot.
pub const UND_RETURN_STUB_OFFSET: usize = 0x00FF_FFE4;
pub const UND_RETURN_STUB_VA: u32 = UND_RETURN_STUB_OFFSET as u32;
/// Offset of the target-PC literal inside the stub (written by Rust
/// handler before ERET).
pub const UND_RETURN_STUB_LITERAL_OFFSET: usize = UND_RETURN_STUB_OFFSET + 8;

/// Shadow-byte-access pre-fault stub, sitting in the 32-byte window
/// between the UND trampoline body (ends at 0x00FF_FF60) and the SBA
/// post-emulation trampoline (starts at 0x00FF_FF80). Used by the
/// SBA UDF emulator to drive a natural DABT on behalf of a faulting
/// site whose effective address is on an unmapped page — the kernel's
/// own `DataAbortHandler` grows the page in, the probe retries, and
/// the stub HVCs back to EL2 for the emulator to finish the access.
///
/// Layout (3 words, 12 bytes):
///   +0x00: e5d0_0000   LDRB r0, [r0]       ; probe; faults if page unmapped
///   +0x04: e140_0174   HVC  #SBA_RETRY_TAG ; return to EL2 on success
///   +0x08: eaff_fffe   B .                 ; guard
pub const SBA_PREFAULT_STUB_OFFSET: usize = 0x00FF_FF60;
pub const SBA_PREFAULT_STUB_VA: u32 = SBA_PREFAULT_STUB_OFFSET as u32;

unsafe fn patch_und_vector(rom: *mut u32) {
    // The trampoline's save-slot address is held in the literal at
    // offset 0x30. Pre-MMU we use the RAM *IPA* 0x0400_5F00 directly
    // (since VA == IPA with the MMU off, and 0x0400_5F00 is inside
    // our stage-2 RAM mapping). Once the guest enables its stage-1
    // MMU, VA 0x0400_xxxx aliases ROM (read-only) under the kernel's
    // L1[0x40] section, so `install_und_vector_swap_post_mmu()` swaps
    // the literal to the VA 0x0C00_4F00, which the kernel's
    // L1[0xC0] coarse → L2[0x04] small page maps back to RAM.

    let imm24 = ((UND_TRAMP_OFFSET as u32 - 0x0C) / 4) & 0x00FF_FFFF;
    let branch_insn = 0xEA00_0000 | imm24;

    // SAFETY: offsets below all sit in 0x00FF_FF00..0x00FF_FF60,
    // well under ROM_SIZE (= 16 MiB = 0x0100_0000) and inside the
    // 128-byte reserved window checked by `tracer::in_reserved_range`.
    //
    // SAFETY: offsets below all sit in 0x00FF_FF00..0x00FF_FF60,
    // well under ROM_SIZE (= 16 MiB = 0x0100_0000) and inside the
    // 128-byte reserved window checked by `tracer::in_reserved_range`.
    unsafe {
        rom.add(1).write(branch_insn);              // 0x04: b UND_TRAMP_OFFSET

        let base = UND_TRAMP_OFFSET / 4;
        rom.add(base +  0).write(0xEE0D_CF50);      // mcr p15,0,r12,c13,c0,2
        rom.add(base +  1).write(0xE59F_C050);      // ldr r12, [pc, #0x50]
        rom.add(base +  2).write(0xE58C_000C);      // str r0, [r12, #0x0C]
        rom.add(base +  3).write(0xE58C_1010);      // str r1, [r12, #0x10]
        rom.add(base +  4).write(0xE58C_E000);      // str lr, [r12]
        rom.add(base +  5).write(0xE14F_0000);      // mrs r0, SPSR
        rom.add(base +  6).write(0xE58C_0004);      // str r0, [r12, #4]
        rom.add(base +  7).write(0xE58C_2014);      // str r2, [r12, #0x14]
        rom.add(base +  8).write(0xE200_101F);      // and r1, r0, #0x1F
        rom.add(base +  9).write(0xE381_10C0);      // orr r1, r1, #0xC0
        rom.add(base + 10).write(0xE351_00D0);      // cmp r1, #0xD0
        rom.add(base + 11).write(0x03A0_10DF);      // moveq r1, #0xDF
        rom.add(base + 12).write(0xE129_F001);      // msr cpsr_c, r1
        rom.add(base + 13).write(0xE58C_D018);      // str sp, [r12, #0x18]
        rom.add(base + 14).write(0xE58C_E01C);      // str lr, [r12, #0x1C]
        rom.add(base + 15).write(0xE321_F0DB);      // msr cpsr_c, #0xdb (UND)
        rom.add(base + 16).write(0xE59C_2014);      // ldr r2, [r12, #0x14]
        rom.add(base + 17).write(0xE321_F0D3);      // msr cpsr_c, #0xd3 (SVC)
        rom.add(base + 18).write(0xE1A0_000E);      // mov r0, lr
        rom.add(base + 19).write(0xE58C_0008);      // str r0, [r12, #8]
        rom.add(base + 20).write(0xE321_F0DB);      // msr cpsr_c, #0xdb (UND)
        rom.add(base + 21).write(0xE140_0170);      // hvc #0x10
        rom.add(base + 22).write(0xEAFF_FFFE);      // b . (trap)
        rom.add(base + 23).write(0x0400_5F00);      // literal: RAM IPA (pre-MMU)

        // SBA pre-fault stub. When the shadow-stub byte-access emulator
        // encounters a faulting EA on an unmapped guest page, it stashes
        // retry state and ERETs into this stub (in UND mode) with
        // ctx.x[0] = EA. The LDRB probes the page: if unmapped, the CPU
        // takes a natural DABT, the existing DABT-trampoline + handle_diag
        // forward path invokes the kernel's own DataAbortHandler, the
        // page is paged in, and the kernel's `subs pc, lr, #8` retries
        // the LDRB. On success the stub HVCs back to EL2, where
        // handle_sba_retry restores the stashed context and re-runs the
        // emulator body. Covers SWPB / writeback / post-index / SP-reg
        // UDF-fallback sites that can't use the inline-stub fast path.
        //
        //   +0x00: e5d0_0000  LDRB r0, [r0]       ; probe
        //   +0x04: e140_0174  HVC  #SBA_RETRY_TAG ; back to EL2
        //   +0x08: eaff_fffe  B .                 ; guard
        let di = SBA_PREFAULT_STUB_OFFSET / 4;
        rom.add(di + 0).write(0xE5D0_0000);          // ldrb r0, [r0]
        rom.add(di + 1).write(0xE140_0174);          // hvc #0x14 (SBA_RETRY_TAG)
        rom.add(di + 2).write(0xEAFF_FFFE);          // b . (guard)
        rom.add(di + 3).write(0xEAFF_FFFE);          // padding (guard)
        rom.add(di + 4).write(0xEAFF_FFFE);
        rom.add(di + 5).write(0xEAFF_FFFE);
        rom.add(di + 6).write(0xEAFF_FFFE);
        rom.add(di + 7).write(0xEAFF_FFFE);

        // UND-return stub. See `return_to_guest_from_und` in trap.rs for
        // why this exists — QEMU raspi3b's `msr spsr_el2, <val>` from
        // AArch64 EL2 clobbers SPSR_EL1 (= AArch32 SPSR_svc) as a side
        // effect. The UND-return path must avoid writing SPSR_EL2, so
        // we ERET into this stub while still in UND mode, then
        // architecturally restore CPSR via `movs pc, lr`.
        //
        // Layout: load target PC from a PC-relative literal (which the
        // Rust handler writes before each ERET), then `movs pc, lr`.
        // The literal route avoids relying on AArch64→AArch32 GPR
        // plumbing for the post-ERET R14: per Table D1-79, X14 maps to
        // R14_usr regardless of target mode (R14_und lives in X22), so
        // the obvious "stash return PC in ctx.x[14]" pattern would
        // overwrite R14_usr instead.
        //   +0x00: e59fe000  ldr lr, [pc, #0]    ; lr = *(stub + 8)
        //   +0x04: e1b0f00e  movs pc, lr         ; CPSR = SPSR_und, PC = lr
        //   +0x08: <target PC literal, updated per ERET>
        let stub = UND_RETURN_STUB_OFFSET / 4;
        rom.add(stub + 0).write(0xE59F_E000); // ldr lr, [pc, #0]
        rom.add(stub + 1).write(0xE1B0_F00E); // movs pc, lr
        rom.add(stub + 2).write(0xDEAD_C0DE); // placeholder literal

        // SBA post-emulation trampoline, at SBA_POST_TRAMP_OFFSET.
        let pt = SBA_POST_TRAMP_OFFSET / 4;
        rom.add(pt + 0).write(0xEE0D_CF50);          // mcr p15,0,r12,c13,c0,2
        rom.add(pt + 1).write(0xE59F_C014);          // ldr r12, [pc, #0x14]  → literal at +0x20
        rom.add(pt + 2).write(0xE59C_D018);          // ldr sp, [r12, #0x18]
        rom.add(pt + 3).write(0xE59C_E01C);          // ldr lr, [r12, #0x1C]
        rom.add(pt + 4).write(0xEE1D_CF50);          // mrc p15,0,r12,c13,c0,2
        rom.add(pt + 5).write(0xE59F_F008);          // ldr pc, [pc, #0x08]
        rom.add(pt + 6).write(0xEAFF_FFFE);          // b . (guard)
        rom.add(pt + 7).write(0xEAFF_FFFE);          // b . (guard)
        rom.add(pt + 8).write(0x0400_5F00);          // slot base (pre-MMU)
        rom.add(pt + 9).write(0xDEAD_C0DE);          // NEW_PC placeholder
    }
}

/// Scan the REx window (PA `start` .. `end`) for Einstein's
/// `NATIVE_PRIM` MCR p10 call sites (Rd = LR / R14) and rewrite each
/// triplet to use R12 (IP) instead. See the block comment at the call
/// site in `load_newton_rom` for why.
///
/// Three lead-in patterns are recognised, all targeting LR:
///   1. `MOV LR, #imm`                (`0xE3A0_EXXX`)
///   2. `MOV LR, #imm; ADD LR, LR, #imm` (`0xE3A0_EXXX; 0xE28E_EXXX`)
///   3. `LDR LR, [PC, #imm]`          (`0xE59F_EXXX`)
///
/// Each `MCR p10, 0, LR, ...` word (`0xEE00_EA10`) has its Rd field
/// rewritten to R12 (`0xEE00_CA10`); each identified lead-in word is
/// rewritten to write to R12 instead of LR.
///
/// Returns the number of MCR sites rewritten.
///
/// SAFETY: `rom` is the hypervisor-owned ROM backing and `start`/`end`
/// must bound the REx-loaded range. Reads and writes are word-aligned.
unsafe fn patch_native_prim_mcr_lr_to_r12(rom: *mut u32, start: u32, end: u32) -> usize {
    const MCR_P10_LR: u32 = 0xEE00_EA10;
    const MCR_P10_R12: u32 = 0xEE00_CA10;
    // DP-immediate: cond 001 opc S Rn Rd imm12. We identify MOV and ADD
    // by masking out the imm12 and S bit. Encoding for MOV (opcode 0xD):
    // bits [27:20] = 0b00111010, Rn ignored.
    // For ADD (opcode 0x4): bits [27:20] = 0b00101000.
    const MOV_LR_IMM_MASK: u32 = 0xFFFF_F000;
    const MOV_LR_IMM_BITS: u32 = 0xE3A0_E000; // mov lr, #imm
    const ADD_LR_IMM_MASK: u32 = 0xFFFF_F000;
    const ADD_LR_IMM_BITS: u32 = 0xE28E_E000; // add lr, lr, #imm
    const LDR_LR_PC_MASK:  u32 = 0xFFFF_F000;
    const LDR_LR_PC_BITS:  u32 = 0xE59F_E000; // ldr lr, [pc, #imm]

    let start_idx = (start / 4) as usize;
    let end_idx = (end / 4) as usize;
    let mut patched = 0usize;

    for j in (start_idx + 2)..end_idx {
        // SAFETY: j < end_idx, and end_idx is word-bounded.
        let mcr = unsafe { rom.add(j).read() };
        if mcr != MCR_P10_LR {
            continue;
        }

        // Look at the immediately preceding word(s).
        let prev = unsafe { rom.add(j - 1).read() };
        let (mov_idx, add_idx) = if (prev & MOV_LR_IMM_MASK) == MOV_LR_IMM_BITS {
            (j - 1, None)
        } else if (prev & ADD_LR_IMM_MASK) == ADD_LR_IMM_BITS {
            // Need `mov lr, #id` two words back.
            let prev2 = unsafe { rom.add(j - 2).read() };
            if (prev2 & MOV_LR_IMM_MASK) != MOV_LR_IMM_BITS {
                continue;
            }
            (j - 2, Some(j - 1))
        } else if (prev & LDR_LR_PC_MASK) == LDR_LR_PC_BITS {
            (j - 1, None)
        } else {
            continue;
        };

        // Rewrite Rd field (bits [15:12]) of the lead-in word from E to C.
        // For ADD we also rewrite Rn (bits [19:16]) from E to C so
        // `add lr, lr, #imm` becomes `add r12, r12, #imm`.
        let lead = unsafe { rom.add(mov_idx).read() };
        let new_lead = (lead & !0x0000_F000) | 0x0000_C000;
        unsafe { rom.add(mov_idx).write(new_lead); }

        if let Some(ai) = add_idx {
            let add = unsafe { rom.add(ai).read() };
            let new_add = (add & !0x000F_F000) | 0x000C_C000;
            unsafe { rom.add(ai).write(new_add); }
        }

        let new_mcr = MCR_P10_R12;
        unsafe { rom.add(j).write(new_mcr); }
        patched += 1;
    }

    patched
}

/// Swap the trampoline's save-slot literal from the pre-MMU RAM IPA
/// (0x0400_5F00) to the post-MMU kernel VA (0x0C00_4F00). Called when
/// the guest turns on its stage-1 MMU — past that point, VA
/// 0x0400_xxxx aliases ROM under the kernel's L1[0x40] section and
/// the pre-MMU literal would make the first STR in the trampoline
/// fault on a read-only page.
pub unsafe fn install_und_vector_swap_post_mmu() {
    // SAFETY: single-word write to each trampoline's slot-base literal.
    // Caller must hold exclusive access to the ROM backing. Swaps the
    // UND trampoline, the SBA post-emulation trampoline, and the DABT
    // diagnostic trampoline.
    unsafe {
        let rom = rom_host_pa() as *mut u32;
        let base = UND_TRAMP_OFFSET / 4;
        rom.add(base + 23).write(0x0C00_4F00);
        let pt = SBA_POST_TRAMP_OFFSET / 4;
        rom.add(pt + 8).write(0x0C00_4F00);
        let db = DABT_TRAMP_OFFSET / 4;
        rom.add(db + 14).write(0x0C00_4FA0);
    }
}

/// Revert the trampoline's save-slot literal back to the pre-MMU RAM
/// IPA (0x0400_5F00). Called when the guest turns its stage-1 MMU
/// off — typically the SWIBoot→ROMBoot soft-reset path. Without this,
/// a UND taken before the next MMU re-enable would store to an
/// unmapped IPA via the stale kernel-VA literal.
pub unsafe fn install_und_vector_swap_pre_mmu() {
    // SAFETY: same as the post-MMU swap above.
    unsafe {
        let rom = rom_host_pa() as *mut u32;
        let base = UND_TRAMP_OFFSET / 4;
        rom.add(base + 23).write(0x0400_5F00);
        let pt = SBA_POST_TRAMP_OFFSET / 4;
        rom.add(pt + 8).write(0x0400_5F00);
        let db = DABT_TRAMP_OFFSET / 4;
        rom.add(db + 14).write(0x0400_5FA0);
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
