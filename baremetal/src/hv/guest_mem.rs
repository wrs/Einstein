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

// (The ROM / REx / guest-test image bytes and the load orchestration
// that fills GUEST_ROM live in `crate::newton::loader`; this module
// owns the backing stores and the IPA/VA access layer.)

/// Per-word "code" bitmap from the classifier (`reach.bitmap`). One bit
/// per 32-bit word across the 16 MiB ROM aperture. Bit set = the word
/// was reached as code by the static analysis; bit clear = data /
/// padding. Used by `load_newton_rom` for selective byteswap-on-load
/// and by the `apply_*_patches` helpers (via `rom_word_is_code`) for
/// runtime patch dispatch.
const REACH_BITMAP: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/reach.bitmap"));

/// True if word index `idx` (= ROM offset / 4) is reachable code per
/// the classifier. Out-of-range indices return false (treated as data).
pub fn rom_word_is_code(idx: usize) -> bool {
    if idx / 8 >= REACH_BITMAP.len() {
        return false;
    }
    (REACH_BITMAP[idx / 8] >> (idx % 8)) & 1 != 0
}

/// Read a guest stage-1 page-table entry (L1 or L2) through a raw host
/// pointer, returning the kernel-intended numerical value.
///
/// Under iter-90+ BE-8 (`SCTLR_EL1.EE=1`) the AArch32 EL1 MMU walks page
/// tables in big-endian byte order. The kernel writes entries with
/// CPSR.E=1 STR (also BE), so memory bytes match the MMU's view. EL2
/// runs AArch64 little-endian; a raw u32 read returns the byteswap of
/// what the MMU sees. Recover the kernel-intended numerical value by
/// swapping bytes.
///
/// Guest-test mode runs the guest LE (CPSR.E=0, SCTLR.EE=0) — the MMU
/// also walks LE, kernel STRs LE, and a host LE read is identity.
///
/// # Safety
///
/// `ptr` must point at a 32-bit page-table entry the caller is allowed
/// to read (e.g. inside `GUEST_RAM` or a non-code ROM word).
#[cfg(not(nh_guest_test))]
#[inline]
pub unsafe fn read_pt_entry(ptr: *const u32) -> u32 {
    unsafe { ptr.read().swap_bytes() }
}
#[cfg(nh_guest_test)]
#[inline]
pub unsafe fn read_pt_entry(ptr: *const u32) -> u32 {
    unsafe { ptr.read() }
}

/// Write a guest stage-1 page-table entry as the kernel-intended numerical
/// value. See `read_pt_entry` for the BE-8 byte-order reasoning.
///
/// # Safety
///
/// `ptr` must point at a 32-bit guest-RAM (or guest-test) location that
/// is safe to write under the paused-guest invariant.
#[cfg(not(nh_guest_test))]
#[inline]
pub unsafe fn write_pt_entry(ptr: *mut u32, value: u32) {
    unsafe { ptr.write(value.swap_bytes()); }
}
#[cfg(nh_guest_test)]
#[inline]
pub unsafe fn write_pt_entry(ptr: *mut u32, value: u32) {
    unsafe { ptr.write(value); }
}

/// Write a 32-bit ARM instruction encoding into the ROM backing at
/// word index `idx`. Under BE-8 the CPU's instruction fetch is always
/// LE, so a native u32 write of the numerical encoding produces host
/// bytes the CPU decodes correctly. Use this for every `apply_*_patch`
/// or `apply_*_wrapper` call site that writes ARM instructions —
/// whether the target is original-ROM code or a synthetic stub region
/// in 0x00FF_FExx.
///
/// SAFETY: `rom_ptr` must point to GUEST_ROM and `idx * 4 + 4` must be
/// within ROM_SIZE.
pub unsafe fn write_rom_code_word(rom_ptr: *mut u32, idx: usize, insn: u32) {
    unsafe {
        rom_ptr.add(idx).write(insn);
    }
}

/// Write a 32-bit data value into the ROM backing at word index `idx`
/// such that a subsequent guest LDR reads back `value`.
///
/// Production builds (BE-8 CPSR.E=1): the host bytes must be the BE
/// encoding of `value`, which is what `value.swap_bytes()` then a
/// native LE write produces.
///
/// Guest-test builds (LE CPSR.E=0): a native u32 write is what an LE
/// LDR returns; no swap needed.
///
/// Use this for kernel-data patches (gDebugger=1, gNewtConfig, time-
/// base constants) and for in-stub literals loaded via `LDR Rd, [pc, #imm]`.
pub unsafe fn write_rom_data_word(rom_ptr: *mut u32, idx: usize, value: u32) {
    #[cfg(not(nh_guest_test))]
    let stored = value.swap_bytes();
    #[cfg(nh_guest_test)]
    let stored = value;
    unsafe {
        rom_ptr.add(idx).write(stored);
    }
}

/// Convenience: dispatch by the classifier bitmap. `apply_rom_patches`
/// uses this so each entry's code-vs-data decision is data-driven.
pub unsafe fn write_rom_word_by_kind(rom_ptr: *mut u32, idx: usize, value: u32) {
    if rom_word_is_code(idx) {
        unsafe { write_rom_code_word(rom_ptr, idx, value); }
    } else {
        unsafe { write_rom_data_word(rom_ptr, idx, value); }
    }
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

/// Read a 32-bit word from a guest physical address by resolving the
/// backing store directly. Returns None if `pa` is outside the ROM /
/// RAM / framebuffer / scratch-pool regions we own. Pre-MMU
/// (VA == IPA == PA) this is exactly a guest load; post-MMU callers
/// need to translate VA to PA first (e.g. via `AT S12E1R`).
pub fn read_word_pa(pa: u32) -> Option<u32> {
    let h = host_addr_for(pa as usize, 4, /*for_write=*/ false)?;
    // SAFETY: host_addr_for bounds-checks against the chosen backing.
    Some(unsafe { core::ptr::read_volatile(h as *const u32) })
}

/// Map a guest IPA + size to the host backing pointer. Drives off the
/// single region manifest (`layout::REGIONS`) so the set of
/// EL2-reachable backings has one definition shared with stage-2 and the
/// snapshot. `for_write=true` excludes read-only regions (ROM is RO at
/// the hypervisor backing layer; flash is RO and not reachable here at
/// all). Only manifest entries flagged `host_addr_for` participate —
/// the flash banks are stage-2-mapped but their backing is owned by
/// `peripherals::flash`, so they fall through to `None` here.
/// Public probe used by `stage2::cross_check_manifest` to confirm a
/// manifest region that claims `host_addr_for` actually resolves through
/// this layer. Resolves a 4-byte access at `ipa`.
pub fn host_pa_for_ipa(ipa: u64, for_write: bool) -> Option<usize> {
    host_addr_for(ipa as usize, 4, for_write)
}

fn host_addr_for(pa: usize, size: usize, for_write: bool) -> Option<usize> {
    let r = crate::hv::layout::region_for(pa as u64, size as u64)?;
    if !r.host_addr_for {
        return None;
    }
    if for_write && r.perm == crate::hv::layout::Stage2Perm::ReadOnly {
        return None;
    }
    Some(r.host_pa() as usize + (pa - r.ipa as usize))
}

/// Read one halfword (16 bits) from a guest PA. Alignment is the
/// caller's responsibility; misaligned reads silently split across the
/// host pointer the way the CPU would cross bytes. Reached only through
/// the `audio-pi-hdmi`-only u16 read helpers in `guest_endian`.
#[allow(dead_code)]
pub fn read_halfword_pa(pa: u32) -> Option<u16> {
    let h = host_addr_for(pa as usize, 2, /*for_write=*/ false)?;
    // SAFETY: host_addr_for bounds-checked.
    Some(unsafe { core::ptr::read_volatile(h as *const u16) })
}

/// Read one byte from a guest PA.
pub fn read_byte_pa(pa: u32) -> Option<u8> {
    let h = host_addr_for(pa as usize, 1, /*for_write=*/ false)?;
    // SAFETY: host_addr_for bounds-checked.
    Some(unsafe { core::ptr::read_volatile(h as *const u8) })
}

/// Write a 32-bit word to a guest PA. Returns true on success. Writes
/// to ROM (or unmapped regions) are refused — callers should halt on
/// a false return if the write was supposed to succeed.
pub fn write_word_pa(pa: u32, value: u32) -> bool {
    match host_addr_for(pa as usize, 4, /*for_write=*/ true) {
        Some(h) => {
            // SAFETY: host_addr_for bounds-checked, ROM excluded.
            unsafe { core::ptr::write_volatile(h as *mut u32, value); }
            true
        }
        None => false,
    }
}

/// Write a 32-bit word to a guest VA by walking the live stage-1
/// short-descriptor tables (rooted at TTBR0 = 0x0400_0000 per the
/// 717006 probe) via `translate_va`. Used from EL2 when we need to
/// land a value in a kernel data structure named by a VA the guest
/// passed us (e.g. SFlashChipInformation pointer).
#[allow(dead_code)]
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
#[allow(dead_code)]
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
    // Page-table entries are stored BE under iter-90+ (kernel STR
    // with CPSR.E=1; MMU walker reads BE because SCTLR.EE=1). Use the
    // PT-entry-aware reader so we recover the kernel-intended values
    // — same byte-order convention as the hardware walker.
    let read_pt_pa = |pa: u32| -> Option<u32> {
        let h = host_addr_for(pa as usize, 4, /*for_write=*/ false)?;
        // SAFETY: host_addr_for bounds-checks against the chosen backing.
        Some(unsafe { read_pt_entry(h as *const u32) })
    };
    let l1_idx = (va >> 20) as usize;
    let l1_entry = read_pt_pa(0x0400_0000 + (l1_idx as u32) * 4)?;
    match l1_entry & 3 {
        2 => Some((l1_entry & 0xFFF0_0000) | (va & 0x000F_FFFF)),
        1 => {
            let l2_pa = l1_entry & 0xFFFF_FC00;
            let l2_idx = (va >> 12) & 0xFF;
            let l2_entry = read_pt_pa(l2_pa + l2_idx * 4)?;
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
    match host_addr_for(pa as usize, 1, /*for_write=*/ true) {
        Some(h) => {
            // SAFETY: host_addr_for bounds-checked, ROM excluded.
            unsafe { core::ptr::write_volatile(h as *mut u8, value); }
            true
        }
        None => false,
    }
}

/// (The stage-1 table normalisation walkers — `fix_stage1_xn_bits`,
/// `install_scratch_pool_l1_section` — are Newton MMU archaeology and
/// live in `crate::newton::os`; they drive this module's
/// `read_pt_entry` / `write_pt_entry` accessors.)

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
    let l1 = unsafe { read_pt_entry(ram.add(l1_idx)) };
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
        let l2 = unsafe { read_pt_entry(base.add(l2_off + l2_idx)) };
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

/// Dump an 8-entry window of L1 around the section index for `va`.
/// Useful for diagnosing "section translation fault but the L1 entry
/// has weird bookkeeping bits set" — we want to see whether the
/// neighbours are coarse / section / fault, and what the lazy-state
/// pattern looks like across a kernel-allocated VA range.
#[cfg(feature = "log_mmu")]
pub fn dump_l1_neighbourhood(va: u32) {
    let ram = addr_of_mut!(GUEST_RAM) as *const u32;
    let centre = (va >> 20) as i32;
    kprintln!("    L1 neighbourhood around section {:#x}:", centre);
    for di in -4..=4 {
        let i = centre + di;
        if i < 0 || i >= 4096 { continue; }
        // SAFETY: index bounds-checked.
        let e = unsafe { read_pt_entry(ram.add(i as usize)) };
        let ty = match e & 3 { 0=>"fault", 1=>"coarse", 2=>"section", 3=>"fine", _=>"?" };
        kprintln!("      L1[{:#x}] = {:#010x}  ({})", i, e, ty);
    }
}

/// Dump the first 32 entries of the guest's stage-1 L1 page table, which we
/// assume lives at the start of guest RAM (TTBR0 = 0x0400_0000 per the
/// 717006 probe; stage-2 maps that IPA to the host ram backing). Each
/// entry covers 1 MiB of VA, so this is the VA 0..32 MiB window.
#[cfg(feature = "log_mmu")]
pub fn dump_guest_l1_table() {
    let ram = addr_of_mut!(GUEST_RAM) as *const u32;
    let rom = addr_of_mut!(GUEST_ROM) as *const u32;
    kprintln!("guest L1 (TTBR=0x0400_0000) first 32 entries (each covers 1 MiB):");
    for i in 0..32 {
        // SAFETY: i < 32; guest L1 table is 4 KiB = 1024 entries so well
        // inside GUEST_RAM bounds.
        let entry = unsafe { read_pt_entry(ram.add(i)) };
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
                        let e = unsafe { read_pt_entry(src_ptr.add(off)) };
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

// ---- VA-walk / guest-string utilities ----

use crate::hv::guest_endian::guest_read_u32_pa as read_guest_word_pa;


/// Read up to `max` bytes of an ASCII C-string from guest VA, stopping
/// at NUL or unmapped page. Used for exception-name dumps.
pub(crate) fn read_cstr_at(va: u32, max: usize) -> ([u8; 128], usize) {
    let mut buf = [0u8; 128];
    let cap = max.min(128);
    let mut len = 0;
    let mut i = 0usize;
    while i < cap {
        // Read a 32-bit word at the next word-aligned position so we
        // can extract the relevant bytes — stage-1 translate is
        // word-granular in our helpers.
        let word_va = (va.wrapping_add(i as u32)) & !0x3;
        let off = ((va.wrapping_add(i as u32)) & 0x3) as usize;
        let w = match crate::hv::guest_endian::guest_read_u32_va(word_va) {
            Some(w) => w,
            None    => break,
        };
        // Newton 2.x stores strings in BE-byte-order even in our LE-
        // word view (BE32 kernel built against SA-1100; iter-30 docs).
        // Within a word, byte k of the string is `(w >> ((3-k)*8))`.
        for j in off..4 {
            if i >= cap { break; }
            let shift = (3 - j) * 8;
            let b = ((w >> shift) & 0xFF) as u8;
            if b == 0 { return (buf, len); }
            buf[i] = b;
            len = i + 1;
            i += 1;
        }
    }
    (buf, len)
}


/// Resolve a guest address as seen by an AArch32 load/store instruction
/// into a guest PA. Identity when the stage-1 MMU is off (SCTLR_EL1.M=0);
/// stage-1 walk otherwise. Returns `None` only when the MMU is on and
/// the VA is unmapped.
pub(crate) fn resolve_guest_pa(addr: u32) -> Option<u32> {
    let sctlr: u64;
    // SAFETY: SCTLR_EL1 read has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, sctlr_el1",
            out(reg) sctlr,
            options(nomem, nostack, preserves_flags),
        );
    }
    if sctlr & 1 == 0 {
        Some(addr)
    } else {
        translate_va(addr)
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
/// the u32 the original BE CPU saw (see `newton::loader::load_newton_rom`).
/// Bytes within each 4-byte word end up reversed in host memory: a
/// word originally `0x48 0x65 0x6C 0x6C` ("Hell" in BE) is laid out
/// as `0x6C 0x6C 0x65 0x48` in host LE memory. To recover the
/// original byte sequence we re-swap each loaded word via
/// `to_be_bytes()`.
///
/// Guest-test binaries are LE-native (no ROM byteswap on load), so
/// the bytes in host memory are already in natural order — use
/// `to_le_bytes()`. We pick at compile time via `nh_guest_test`.
pub(crate) fn log_guest_string(prefix: &'static str, addr: u32) {
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


pub(crate) fn scan_to_null_word_aligned(start: u32, max_words: u32) -> u32 {
    let mut va = start & !0x3;
    for _ in 0..max_words {
        // The scan result becomes the guest's resume PC (the word after
        // the DebuggerUND payload's terminator) — fabricating a
        // terminator at an unreadable word would resume at a wrong PC,
        // so halt loudly instead.
        let w = match read_guest_word_pa(va) {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** scan_to_null_word_aligned: PA={:#010x} unreadable \
                     (scan started at {:#010x}) ***",
                    va, start,
                );
                crate::arch::cpu::halt();
            }
        };
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

