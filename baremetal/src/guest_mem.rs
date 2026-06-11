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

use crate::hvc_imm::HvcImm;
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
// The test binary is an AArch32 flat binary with reset vector at offset
// 0, built by baremetal/guest-tests/scripts/build-tests.sh.
//
// Two delivery modes, selected by the value of `$NH_GUEST_TEST`:
//
// 1. **Path** (`NH_GUEST_TEST=path/to/test.bin`): embed the bytes into
//    the image at compile time via `include_bytes!`. The hypervisor
//    boots straight into the test with no runtime load step. Fast for
//    single-test iteration when cargo's incremental build only has to
//    re-emit one object + relink.
//
// 2. **Semihost** (`NH_GUEST_TEST=1`): build the hypervisor as a
//    test-mode image with no embedded test, and load the test binary at
//    boot time via Arm semihosting. The path is passed by the host as a
//    semihosting cmdline arg (`qemu-system-aarch64 ... -semihosting-config
//    arg=path/to/test.bin`). One hypervisor build serves N tests — used
//    by `run-all.sh` to skip the per-test relink that dominates the
//    36-test wall time.
//
// build.rs sets `nh_guest_test_embed` for mode 1 and `nh_guest_test_semihost`
// for mode 2; both also set `nh_guest_test`.
#[cfg(nh_guest_test_embed)]
static GUEST_TEST_BIN: &[u8] = include_bytes!(env!("NH_GUEST_TEST_PATH"));

// Semihost mode: a buffer the early-boot loader fills via SYS_READ.
// Sized at GUEST_ROM's full 16 MiB so any practical test binary fits.
#[cfg(nh_guest_test_semihost)]
static mut GUEST_TEST_BIN_BUF: [u8; ROM_SIZE] = [0u8; ROM_SIZE];
#[cfg(nh_guest_test_semihost)]
static mut GUEST_TEST_BIN_LEN: usize = 0;

#[cfg(nh_guest_test_semihost)]
fn guest_test_bin() -> &'static [u8] {
    // SAFETY: GUEST_TEST_BIN_LEN is only written by `load_test_bin_via_semihosting`
    // before any reader runs, and EL2 boot is single-threaded.
    unsafe {
        let len = GUEST_TEST_BIN_LEN;
        let ptr = addr_of_mut!(GUEST_TEST_BIN_BUF) as *const u8;
        core::slice::from_raw_parts(ptr, len)
    }
}

#[cfg(nh_guest_test_embed)]
fn guest_test_bin() -> &'static [u8] {
    GUEST_TEST_BIN
}

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

/// Convenience: dispatch by the classifier bitmap. `apply_717006_patches`
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

const RAM_BASE_USIZE: usize = RAM_IPA_BASE as usize;
const FB_BASE_USIZE: usize = FB_IPA_BASE as usize;

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

/// Map a guest IPA + size to the host backing pointer. Centralises the
/// region table used by the typed PA helpers so regions added here
/// (e.g. the shadow-stub `SCRATCH_POOL`) are visible to all callers
/// in one shot. `for_write=true` excludes ROM (read-only at the
/// hypervisor backing layer).
fn host_addr_for(pa: usize, size: usize, for_write: bool) -> Option<usize> {
    if !for_write && pa + size <= ROM_SIZE {
        return Some(rom_host_pa() as usize + pa);
    }
    if (RAM_BASE_USIZE..RAM_BASE_USIZE + RAM_SIZE).contains(&pa)
        && pa + size <= RAM_BASE_USIZE + RAM_SIZE
    {
        return Some(ram_host_pa() as usize + (pa - RAM_BASE_USIZE));
    }
    if (FB_BASE_USIZE..FB_BASE_USIZE + FB_SIZE).contains(&pa)
        && pa + size <= FB_BASE_USIZE + FB_SIZE
    {
        return Some(fb_host_pa() as usize + (pa - FB_BASE_USIZE));
    }
    // Shadow-stub scratch pool. Covers the hypervisor-managed RW IPA
    // window at `SCRATCH_POOL_IPA..+SCRATCH_POOL_SIZE`. Behaves like
    // RAM at this layer — full RW from EL2 via the host backing.
    let scratch_base = crate::shadow_stub::SCRATCH_POOL_IPA as usize;
    let scratch_end = scratch_base + crate::shadow_stub::SCRATCH_POOL_SIZE;
    if (scratch_base..scratch_end).contains(&pa) && pa + size <= scratch_end {
        return Some(crate::shadow_stub::scratch_pool_host_pa() as usize + (pa - scratch_base));
    }
    None
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
///
/// Returns `true` iff this call actually wrote bytes into the ROM
/// backing store (an L2 entry inside ROM was rewritten). The flash
/// ROM/REx checksums only need re-seeding when ROM has changed, so
/// callers gate `reseed_flash_checksums_if_needed` on the return.
pub fn fix_stage1_xn_bits() -> bool {
    let ram = addr_of_mut!(GUEST_RAM) as *mut u32;
    let rom = addr_of_mut!(GUEST_ROM) as *mut u32;

    let mut rom_writes = 0usize;

    let scratch_l1_idx = (crate::shadow_stub::SCRATCH_POOL_VA >> 20) as usize;

    // L1 sits at the start of guest RAM (TTBR0 = 0x0400_0000 per probe).
    for i in 0..4096 {
        // Skip the shadow-stub scratch L1 slot — it's owned by
        // `install_scratch_pool_l1_section`, which installs a section
        // with XN=1. The section-normalisation block below would clear
        // XN every M-toggle, forcing the installer to re-set it on
        // each task switch. Leave the slot alone; the installer
        // handles it.
        if i == scratch_l1_idx {
            continue;
        }

        // SAFETY: L1 is 16 KiB = 4096 × 4 bytes, at RAM[0..16384].
        let entry = unsafe { read_pt_entry(ram.add(i)) };
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
            unsafe { write_pt_entry(ram.add(i), 0); }
            continue;
        }

        // Normalise section descriptor to minimal-valid ARMv7 form:
        // preserve PA (bits 31:20) + domain (8:5), clear XN/AP[2]/TEX/S/nG,
        // force AP[1:0] = 0b11 (RW both levels) + C/B = 1.
        if typ == 2 {
            let new = (entry & 0xFFF0_01E0) | 0x0000_0C0E;
            if new != entry {
                // SAFETY: i < 4096.
                unsafe { write_pt_entry(ram.add(i), new); }
            }
        }

        // Normalise coarse descriptor: preserve L2 ptr (bits 31:10) + domain
        // (8:5), clear the ARMv4 SBO bits (4) and NS (3).
        if typ == 1 {
            let new = (entry & 0xFFFF_FC00) | (entry & 0x0000_01E0) | 0x01;
            if new != entry {
                // SAFETY: i < 4096.
                unsafe { write_pt_entry(ram.add(i), new); }
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
        let is_rom = region_start == 0;
        let l2_idx_start = (l2_pa - region_start) / 4;
        if l2_idx_start + 256 > region_size / 4 {
            continue;
        }

        // Coarse L2 has 256 entries, each 4 bytes. Rewrite each non-fault
        // entry into minimal valid ARMv7 form: preserve the PA, force
        // AP = 0b11 (RW both levels), C = B = 1, XN = 0. This strips the
        // ARMv4 subpage-permission bits which ARMv7 would reinterpret as
        // XN/AP[2]/TEX etc.
        for j in 0..256 {
            // SAFETY: bounds checked above.
            let ptr = unsafe { base.add(l2_idx_start + j) };
            let e = unsafe { read_pt_entry(ptr) };
            let typ = e & 3;
            let new = match typ {
                0 => continue,                         // fault, leave alone
                1 => (e & 0xFFFF_0000) | 0x0000_003D,  // large page, RW/RW, CB
                2 | 3 => (e & 0xFFFF_F000) | 0x0000_003E, // small page, XN=0
                _ => unreachable!(),
            };

            if new != e {
                unsafe { write_pt_entry(ptr, new); }
                if is_rom {
                    rom_writes += 1;
                }
            }
        }
    }

    rom_writes > 0
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
    let entry = unsafe { read_pt_entry(ram.add(idx)) };

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
        unsafe { write_pt_entry(ram.add(idx), installed); }
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

/// Load the test binary into `GUEST_TEST_BIN_BUF` via Arm semihosting.
///
/// The path is the first non-binary-name word of the cmdline, which QEMU
/// populates from `-semihosting-config arg=<path>`. iter-86 introduced
/// this to skip the per-test hypervisor rebuild that dominated
/// `run-all.sh`'s 5-minute wall time. With this loader the hypervisor
/// is built once with `NH_GUEST_TEST=1` and each test run only changes
/// the QEMU cmdline arg.
#[cfg(nh_guest_test_semihost)]
unsafe fn load_test_bin_via_semihosting() {
    use core::arch::asm;
    const SYS_OPEN: u64 = 0x01;
    const SYS_CLOSE: u64 = 0x02;
    const SYS_READ: u64 = 0x06;
    const SYS_FLEN: u64 = 0x0C;
    const SYS_GET_CMDLINE: u64 = 0x15;
    const MODE_READ_BINARY: u64 = 0x01;

    unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
        let result: u64;
        // SAFETY: HLT #0xF000 is the AArch64 semihosting trap; QEMU
        // intercepts and returns to EL2 without touching state beyond x0.
        unsafe {
            asm!(
                "hlt #0xF000",
                inout("x0") op => result,
                in("x1") arg as u64,
                options(nostack, preserves_flags),
            );
        }
        result as i64
    }

    // Buffer for the cmdline. QEMU's cmdline format on raspi3b semihosting
    // is "<binary_name> <arg1> <arg2> ..." — for our use, arg1 is the
    // test bin path. 256 bytes is comfortably more than any /tmp path.
    const CMDLINE_CAP: usize = 256;
    static mut CMDLINE_BUF: [u8; CMDLINE_CAP] = [0; CMDLINE_CAP];
    // SYS_GET_CMDLINE: in: ptr, len; out: writes path to ptr, len-out at
    // arg[1]. Returns 0 on success, -1 on failure.
    let cmdline_args: [u64; 2] = [
        addr_of_mut!(CMDLINE_BUF) as u64,
        (CMDLINE_CAP as u64) - 1,
    ];
    let rc = unsafe { semihost(SYS_GET_CMDLINE, cmdline_args.as_ptr()) };
    if rc != 0 {
        kprintln!("guest_mem: SYS_GET_CMDLINE failed (rc={}) — no test bin", rc);
        crate::cpu::halt();
    }

    // Parse out the second whitespace-separated word from the cmdline.
    // The first word is the binary name (or "newton-hypervisor"), the
    // second is our test bin path.
    let cmdline = unsafe {
        let ptr = addr_of_mut!(CMDLINE_BUF) as *const u8;
        // Find NUL terminator or full buffer.
        let mut n = 0;
        while n < CMDLINE_CAP && core::ptr::read(ptr.add(n)) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(ptr, n)
    };
    // QEMU's semihosting cmdline is exactly the `arg=...` value (no
    // binary-name prefix as POSIX execve would have). Take the whole
    // string, trimmed of leading/trailing whitespace.
    let mut start = 0;
    let mut end = cmdline.len();
    while start < end && (cmdline[start] == b' ' || cmdline[start] == b'\t') {
        start += 1;
    }
    while end > start && (cmdline[end - 1] == b' ' || cmdline[end - 1] == b'\t' || cmdline[end - 1] == b'\n') {
        end -= 1;
    }
    let path_bytes = &cmdline[start..end];
    if path_bytes.is_empty() {
        kprintln!(
            "guest_mem: cmdline empty — expected QEMU \
             `-semihosting-config arg=<test-bin-path>`"
        );
        crate::cpu::halt();
    }

    // SYS_OPEN takes a NUL-terminated path; copy into a static buffer.
    const PATH_CAP: usize = 256;
    static mut PATH_BUF: [u8; PATH_CAP] = [0; PATH_CAP];
    if path_bytes.len() >= PATH_CAP - 1 {
        kprintln!("guest_mem: test path too long ({} bytes)", path_bytes.len());
        crate::cpu::halt();
    }
    // SAFETY: single-threaded EL2 init; bounded write under PATH_BUF.len().
    unsafe {
        let dst = addr_of_mut!(PATH_BUF) as *mut u8;
        for (i, &b) in path_bytes.iter().enumerate() {
            dst.add(i).write(b);
        }
        dst.add(path_bytes.len()).write(0);
    }

    let open_args: [u64; 3] = [
        addr_of_mut!(PATH_BUF) as u64,
        MODE_READ_BINARY,
        path_bytes.len() as u64,
    ];
    let fh = unsafe { semihost(SYS_OPEN, open_args.as_ptr()) };
    if fh < 0 {
        kprintln!(
            "guest_mem: SYS_OPEN failed (rc={}) for path {:?} (len={})",
            fh,
            core::str::from_utf8(path_bytes).unwrap_or("<non-utf8>"),
            path_bytes.len(),
        );
        crate::cpu::halt();
    }
    let fh = fh as u64;

    let flen_args: [u64; 1] = [fh];
    let flen = unsafe { semihost(SYS_FLEN, flen_args.as_ptr()) };
    let buf_cap = ROM_SIZE; // GUEST_TEST_BIN_BUF is sized at ROM_SIZE
    if flen < 0 || (flen as usize) > buf_cap {
        kprintln!(
            "guest_mem: SYS_FLEN={} (test bin too large or error)",
            flen
        );
        crate::cpu::halt();
    }
    let flen = flen as usize;

    // SYS_READ: ptr, len. Returns bytes-NOT-read (0 on success).
    let read_args: [u64; 3] = [
        fh,
        addr_of_mut!(GUEST_TEST_BIN_BUF) as u64,
        flen as u64,
    ];
    let unread = unsafe { semihost(SYS_READ, read_args.as_ptr()) };
    if unread != 0 {
        kprintln!("guest_mem: SYS_READ left {} bytes unread", unread);
        crate::cpu::halt();
    }
    let close_args: [u64; 1] = [fh];
    let _ = unsafe { semihost(SYS_CLOSE, close_args.as_ptr()) };

    // SAFETY: single-threaded EL2 init.
    unsafe { GUEST_TEST_BIN_LEN = flen; }
}

#[cfg(nh_guest_test)]
pub unsafe fn load_guest_test() {
    #[cfg(nh_guest_test_semihost)]
    unsafe { load_test_bin_via_semihosting(); }

    let rom_ptr = addr_of_mut!(GUEST_ROM) as *mut u8;
    let bin = guest_test_bin();
    let mode = if cfg!(nh_guest_test_semihost) { "semihost-loaded" } else { "embedded" };
    kprintln!(
        "guest_mem: GUEST-TEST MODE ({}) — copying {} bytes into GUEST_ROM",
        mode, bin.len()
    );
    for (i, b) in bin.iter().enumerate() {
        // SAFETY: i < bin.len() <= ROM_SIZE.
        unsafe { rom_ptr.add(i).write(*b); }
    }
    // Make the freshly-written bytes visible to the guest's instruction
    // fetcher. Without this the I-cache misses, hits memory, and reads
    // pre-init zeros (the writes are still in the D-cache).
    crate::cpu::icache_publish_range(rom_ptr as u64, bin.len());
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
        "guest_mem: loading {} bytes of ROM (BE-8: code words byteswapped, data verbatim)",
        ROM_BE.len()
    );

    for i in 0..be_words {
        let off = i * 4;
        let on_disk = [
            ROM_BE[off],
            ROM_BE[off + 1],
            ROM_BE[off + 2],
            ROM_BE[off + 3],
        ];
        // SAFETY: rom_ptr covers ROM_SIZE bytes; i*4 < ROM_BE.len() <= ROM_SIZE.
        if rom_word_is_code(i) {
            // Code: CPU LE fetch must decode the original BE numerical
            // instruction. The numerical value is from_be_bytes(on_disk);
            // a native LE write of that produces host bytes = LE encoding
            // of the instruction.
            let insn = u32::from_be_bytes(on_disk);
            unsafe { rom_ptr.add(i).write(insn); }
        } else {
            // Data: under BE-8 CPSR.E=1, LDR reads the original BE
            // numerical value when host bytes equal the on-disk (BE-
            // encoded) bytes. Write each byte verbatim.
            unsafe {
                let dst = rom_ptr.add(i) as *mut u8;
                dst.add(0).write(on_disk[0]);
                dst.add(1).write(on_disk[1]);
                dst.add(2).write(on_disk[2]);
                dst.add(3).write(on_disk[3]);
            }
        }
    }

    // Load Einstein's REx at PA 0x00800000 (= the second 8 MB of the 16 MB
    // ROM region). The kernel's stage-1 MMU maps this to VA 0x01000000
    // once it programs its page tables. Same BE->LE byteswap as the main
    // ROM, because the guest runs little-endian.
    const REX_PA_OFFSET: usize = 0x00800000;
    let rex_words = REX_BE.len() / 4;
    kprintln!(
        "guest_mem: loading {} bytes of Einstein.rex at PA {:#x} (BE-8: code-vs-data per bitmap)",
        REX_BE.len(), REX_PA_OFFSET,
    );
    assert!(REX_BE.len() <= ROM_SIZE - REX_PA_OFFSET);
    let rex_base_word = REX_PA_OFFSET / 4;
    for i in 0..rex_words {
        let off = i * 4;
        let on_disk = [
            REX_BE[off],
            REX_BE[off + 1],
            REX_BE[off + 2],
            REX_BE[off + 3],
        ];
        // SAFETY: rex_base_word + i stays below ROM_SIZE / 4 via the assert above.
        if rom_word_is_code(rex_base_word + i) {
            let insn = u32::from_be_bytes(on_disk);
            unsafe { rom_ptr.add(rex_base_word + i).write(insn); }
        } else {
            unsafe {
                let dst = rom_ptr.add(rex_base_word + i) as *mut u8;
                dst.add(0).write(on_disk[0]);
                dst.add(1).write(on_disk[1]);
                dst.add(2).write(on_disk[2]);
                dst.add(3).write(on_disk[3]);
            }
        }
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
    // The REx id field is data — under BE-8 the kernel reads it via LDR
    // and must see the BE-encoded value, so dispatch through the
    // bitmap-aware helper. (The bitmap should mark this word as data,
    // but using `write_rom_word_by_kind` is robust either way.)
    unsafe {
        let old_id = rom_ptr.add(rex_id_word_index).read();
        write_rom_word_by_kind(rom_ptr, rex_id_word_index, NUM_EMBEDDED_REXES_717006);
        kprintln!(
            "guest_mem: Einstein.rex id patch host_was={:#010x} -> id={} (first free slot after embedded REx)",
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
    unsafe { write_rom_code_word(rom_ptr, 3, HvcImm::Diag.insn()); } // hvc #0x11

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

    // Publish every byte of the patched ROM aperture to the Point of
    // Unification in one sweep. `write_rom_code_word` / the load loop
    // write instruction bytes through Normal-WB into EL2's D-cache; on
    // Cortex-A53 / AEMv8-A the I-cache is non-coherent, so a guest fetch
    // can cold-read stale memory unless the dirty D-cache lines are
    // cleaned to PoU (DC CVAU) and the I-cache lines invalidated
    // (IC IVAU). The `ic iallu` in `eret_to_guest` invalidates the
    // I-cache but does NOT clean dirty D-cache lines — it works today
    // only because the 16 MiB load loop evicts most lines incidentally.
    // This sweep makes the guarantee explicit and supersedes the
    // narrower per-range publishes formerly in `patch_und_vector`
    // (same DC CVAU; DSB; IC IVAU; DSB; ISB per line, over a wider
    // range, run strictly after every patcher). Cost is measured below
    // and printed so a future change can re-check it.
    let (icache_t0, icache_freq): (u64, u64);
    // SAFETY: MRS of RO timer sysregs, no side effects.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) icache_t0,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) icache_freq,
            options(nomem, nostack, preserves_flags));
    }
    crate::cpu::icache_publish_range(rom_ptr as u64, ROM_SIZE);
    let icache_t1: u64;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) icache_t1,
            options(nomem, nostack, preserves_flags));
    }
    let icache_dt = icache_t1.wrapping_sub(icache_t0);
    kprintln!(
        "guest_mem: icache_publish_range over {} MiB ROM aperture: {} ticks (~{} us @ {} Hz)",
        ROM_SIZE / (1024 * 1024),
        icache_dt,
        if icache_freq != 0 { icache_dt * 1_000_000 / icache_freq } else { 0 },
        icache_freq,
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
/// without the brief `msr cpsr_c, #0xd3` mode bounce.
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

/// FPA-class UND bypass stub at `0x00FF_FEC0`. The UND vector at IPA
/// 0x04 branches here first; the stub checks if the faulting instruction
/// is FPA-class (coprocessor field = 1 or 2) and, if so, branches
/// directly to `FP_UndefHandlers_Start_JT` at `0x38d874` — exactly the
/// path SA-110 hardware took on the original Newton (UND vector held
/// `b 0x1a031f4` which thunked through 0x38d874 to FP_UndefHandlers_Start
/// at 0x38d8dc). Non-FPA UNDs fall through to the existing trampoline
/// at `UND_TRAMP_OFFSET`, which captures source-mode banked state and
/// HVCs into EL2 for the rest of our handlers (tracer UDFs, shadow-byte-
/// access, software breakpoints, deprecated CP15, etc.).
///
/// Stub layout (16 words = 64 bytes):
///   +0x00: ee0d_cf50  mcr p15,0,r12,c13,c0,2  ; save R12 → TPIDRURW
///   +0x04: e51e_c004  ldr r12, [lr, #-4]      ; R12 = faulting insn
///   +0x08: e20c_c40f  and r12, r12, #0x0F000000 ; isolate bits[27:24]
///   +0x0C: e35c_040c  cmp r12, #0x0C000000    ; LDC/STC variant?
///   +0x10: 135c_040d  cmpne r12, #0x0D000000  ; LDC/STC variant?
///   +0x14: 135c_040e  cmpne r12, #0x0E000000  ; CDP/MCR/MRC?
///   +0x18: 1a00_xxxx  bne UND_TRAMP_OFFSET    ; bits[27:24] not FPA-class
///   +0x1C: e51e_c004  ldr r12, [lr, #-4]      ; reload insn (was masked above)
///   +0x20: e20c_cc0f  and r12, r12, #0xF00    ; isolate cp_num bits[11:8]
///   +0x24: e35c_0c01  cmp r12, #0x100         ; cp_num == 1?
///   +0x28: 135c_0c02  cmpne r12, #0x200       ; cp_num == 2?
///   +0x2C: 1a00_xxxx  bne UND_TRAMP_OFFSET    ; cp_num not 1 or 2
///   +0x30: ee1d_cf50  mrc p15,0,r12,c13,c0,2  ; restore R12 (FPA path)
///   +0x34: ea00_xxxx  b FPE_JT (= 0x38d874)
///   +0x38: ee1d_cf50  mrc p15,0,r12,c13,c0,2  ; restore R12 (non-FPA path)
///   +0x3C: ea00_xxxx  b UND_TRAMP_OFFSET
///
/// The two `bne UND_TRAMP_OFFSET` early-outs at +0x18 and +0x2C share
/// the not-FPA exit at +0x38; consolidating saves 8 bytes vs branching
/// to a per-failure restore-and-jump pair.
///
/// Why route through 0x38d874 (the JT slot) rather than 0x38d8dc
/// (FP_UndefHandlers_Start directly): preserves the post-ship-patch
/// indirection — if the kernel later patches the FPE entry to a REx
/// override, the JT slot picks up the override automatically.
///
/// The stub uses TPIDRURW as the R12 save slot (matching what the
/// existing trampoline does), so the kernel's FPE sees R12 with its
/// original USR value when it executes its own `mcr p15,0,r12,...`-
/// less prologue. SP_und / LR_und / SPSR_und are untouched — exactly
/// the architectural state SA-110 hardware delivered to the FPE.
pub const FPA_BYPASS_STUB_OFFSET: usize = 0x00FF_FEC0;

/// DABT-vector trampoline body. Installed at ROM offset 0x00FF_FFA8.
/// Saves LR_abt/SP_abt/SPSR_abt natively from
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

/// iter-59: AArch32-side fast-forward DABT trampoline. Installed in
/// the head of the SBA stub pool (which has plenty of free space).
///
/// Routes by DFSC straight from AArch32 ABT mode without an EL2 round
/// trip in the common kernel-handled cases:
///
///   DFSC == 0x07 (translation, page)     → branch DAH @ 0x00393114
///   DFSC == 0x0F (permission, page)      → branch DAH @ 0x00393114
///   DFSC == 0x0D (permission, section)   → branch DAH @ 0x00393114
///   DFSC == 0x06 (access flag, page)     → branch DAH @ 0x00393114
///   DFSC == 0x03 (access flag, section)  → branch DAH @ 0x00393114
///   anything else                        → fall through to DABT_TRAMP_OFFSET
///                                          (slow EL2 path: DFSC=0x05
///                                          translation-section needs
///                                          DFSR.Domain synthesis from
///                                          L1[FAR>>20][8:5] — see
///                                          install_dabt_fast_trampoline
///                                          docs; alignment, domain,
///                                          external, recursive aborts
///                                          all also fall through)
///
/// VA 0x10 branches here instead of directly at DABT_TRAMP_OFFSET so
/// the fast path doesn't pay the DABT_TRAMP's lr/sp/spsr saves either.
/// The slow-fall-through invokes the existing DABT_TRAMP_OFFSET (which
/// does the saves and HVCs into EL2 for the rare cases).
///
/// Located in the unused tail between Einstein.rex (ends ~0x847000)
/// and the tracer trampoline pool (starts at 0x900000). 64 words
/// reserved; the trampoline body uses ~16.
pub const DABT_FAST_TRAMP_OFFSET: usize = 0x008F_FF00;
pub const DABT_FAST_TRAMP_DAH_TARGET: u32 = 0x0039_3114;
/// 2026-04-28: relocated from PA=0x04005FA0 to IPA=0x0600_F0A0
/// (last 4 KiB of the SCRATCH_POOL). See `trap::HYP_TRAMP_SCRATCH_BASE`
/// for the rationale. The same value works pre-MMU and post-MMU,
/// so no swap is required.
pub const DABT_SAVE_PA: u32 = crate::trap::HYP_TRAMP_SCRATCH_BASE + 0xA0;

/// Upper bound of the contiguous patch-stub / FPA-bypass / UND-return /
/// UND-trampoline code region in the ROM aperture tail. The region runs
/// from `rom_patches::PATCH_STUB_ARENA_BASE` (0x00FF_FD80) up to the top
/// of the 16 MiB ROM aperture; it holds, in order, the patch-stub arena,
/// the FPA bypass stub (`FPA_BYPASS_STUB_OFFSET`), the UND-return stub
/// (`UND_RETURN_STUB_OFFSET`), and the UND trampoline (0x00FF_FF00).
pub const ROM_TAIL_STUBS_END: u32 = 0x0100_0000;

/// True if `pa` lies in a region that the hypervisor populates at
/// runtime with native (little-endian) AArch32 instruction words rather
/// than guest-authored data. Single source of truth shared by:
///   * `guest_endian::pa_is_rom_code` — these words must NOT be
///     byte-swapped on a guest read (they're already host-LE code), and
///   * `snapshot::pc_in_hypervisor_transient_region` — a guest PC parked
///     in one of these regions is mid-trampoline and must not anchor an
///     autosave (the EL2-side code at that IPA is rebuilt every boot).
///
/// Covers every runtime-written code region: the DABT fast trampoline,
/// the tracer trampoline pool, and the contiguous patch-stub arena / FPA
/// bypass stub / UND-return stub / UND trampoline tail. None of these
/// regions hold guest data the hypervisor reads back through
/// `guest_endian`, so the byte-order predicate and the autosave-gating
/// predicate want exactly the same set.
pub fn is_hypervisor_code_region(pa: u32) -> bool {
    // Tracer trampoline pool, `tracer::TRAMPOLINE_IPA..TRAMPOLINE_END`.
    // Hardcoded here (not via `crate::tracer`) because the `tracer`
    // module is `#[cfg(feature = "trace")]`-only, while this predicate
    // must compile in every build. The pool is empty ROM tail when the
    // feature is off, so applying the range unconditionally is harmless.
    const TRACER_POOL_BASE: u32 = 0x0090_0000;
    const TRACER_POOL_END: u32 = 0x00E0_0000;

    // DABT fast trampoline (between Einstein.rex tail and tracer pool).
    if (DABT_FAST_TRAMP_OFFSET as u32..TRACER_POOL_BASE).contains(&pa) {
        return true;
    }
    if (TRACER_POOL_BASE..TRACER_POOL_END).contains(&pa) {
        return true;
    }
    // Patch-stub arena → FPA bypass stub → UND-return stub → UND
    // trampoline, all contiguous in the ROM aperture tail.
    if (crate::rom_patches::PATCH_STUB_ARENA_BASE..ROM_TAIL_STUBS_END).contains(&pa) {
        return true;
    }
    false
}

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
        // VA 0x10 → branch to the iter-59 fast trampoline (which
        // dispatches by DFSC; common cases jump straight to kernel
        // DAH; uncommon cases fall through to the slow DABT_TRAMP).
        let imm24 = ((DABT_FAST_TRAMP_OFFSET as u32).wrapping_sub(0x10 + 8) / 4) & 0x00FF_FFFF;
        let branch_insn = 0xEA00_0000 | imm24;
        write_rom_code_word(rom_ptr, 4, branch_insn);    // 0x10: b DABT_FAST_TRAMP_OFFSET

        install_dabt_fast_trampoline(rom_ptr);

        let db = DABT_TRAMP_OFFSET / 4;
        write_rom_code_word(rom_ptr, db +  0, 0xEE0D_0F50); // mcr p15,0,r0,c13,c0,2
        write_rom_code_word(rom_ptr, db +  1, 0xEE0D_1F70); // mcr p15,0,r1,c13,c0,3
        write_rom_code_word(rom_ptr, db +  2, 0xE59F_0028); // ldr r0, [pc, #0x28] → DABT_SAVE_VA
        write_rom_code_word(rom_ptr, db +  3, 0xE580_E000); // str lr, [r0]           LR_abt
        write_rom_code_word(rom_ptr, db +  4, 0xE580_D004); // str sp, [r0, #4]       SP_abt
        write_rom_code_word(rom_ptr, db +  5, 0xE14F_1000); // mrs r1, spsr
        write_rom_code_word(rom_ptr, db +  6, 0xE580_1008); // str r1, [r0, #8]       SPSR_abt
        write_rom_code_word(rom_ptr, db +  7, 0xEE15_0F10); // mrc p15,0,r0,c5,c0,0   DFSR
        write_rom_code_word(rom_ptr, db +  8, 0xE200_000F); // and r0, r0, #0xF
        write_rom_code_word(rom_ptr, db +  9, 0xE350_0001); // cmp r0, #1
        write_rom_code_word(rom_ptr, db + 10, 0x0A00_0000); // beq +0x0 (word 12 = ALIGN hvc)
        write_rom_code_word(rom_ptr, db + 11, HvcImm::DabtDispatch.insn()); // DABT-trampoline fall-through
        write_rom_code_word(rom_ptr, db + 12, HvcImm::Align.insn()); // hvc #0x13 (ALIGN_TAG)
        write_rom_code_word(rom_ptr, db + 13, 0xEAFF_FFFE); // b . (guard)
        // Literal slot — read by the LDR at db+2 under BE-8 (CPSR.E=1),
        // so write as data (BE-encoded host bytes).
        write_rom_data_word(rom_ptr, db + 14, DABT_SAVE_PA);
    }
}

/// Install the iter-59 fast-forward DABT trampoline at
/// `DABT_FAST_TRAMP_OFFSET` (in the unused tail between Einstein.rex
/// and the tracer pool). VA 0x10's branch (set by `patch_dabt_vector`)
/// targets here. Layout:
///
///   ft+0:  mcr p15,0,r0,c13,c0,2     ; TPIDRURW = R0 (save)
///   ft+1:  mcr p15,0,r1,c13,c0,3     ; TPIDRRO = R1 (save)
///   ; iter-105 (FVP fix #1): mirror the slow trampoline's save of
///   ; LR_abt / SP_abt / SPSR_abt into the DABT_SAVE_PA scratch slots.
///   ; The DAH-MRS-SPSR HVC patch reads SPSR_abt from the slot, so
///   ; if only fast-path DABTs fire (the common case on FVP, where
///   ; DFSC=5 section faults are rare during early boot) the slot
///   ; stays at 0 and the kernel's `mrs r1, SPSR` substitution gets
///   ; bogus data.
///   ft+2:  ldr r0, [pc, #L_SAVE_PA]  ; r0 = DABT_SAVE_PA literal
///   ft+3:  str lr, [r0]              ; LR_abt
///   ft+4:  str sp, [r0, #4]          ; SP_abt
///   ft+5:  mrs r1, spsr              ; r1 = SPSR_abt
///   ft+6:  str r1, [r0, #8]          ; SPSR_abt
///   ; iter-105 (FVP fix #2): c7 cache-maintenance MCR filter. The
///   ; kernel's `CleanPageInDcache` etc. issue DCCMVAC/DCIMVAC/...
///   ; with VAs that may be unmapped at the time. Cortex-A53 silently
///   ; no-ops these (per its TRM); FVP_Base_RevC's strict AEM raises
///   ; a translation fault. Forwarding the fault to DAH is fatal —
///   ; DAH treats any SVC-mode DABT as "deep toast". Filter here:
///   ; if the faulting instruction is `MCR p15, 0, Rt, c7, CRm, opc2`,
///   ; just advance past it and return to the pre-abt context. The
///   ; cache invalidation itself is a no-op on a coherent A53/AEM,
///   ; matching Einstein's `TARMProcessor` case-7 silent-no-op.
///   ;
///   ; The faulting word is read in BE-8 (CPSR.E=1) and so returns
///   ; the byteswap of the kernel's compiled-for encoding — REV
///   ; brings it back to the kernel-format word, which we mask
///   ; against the c7-MCR pattern.
///   ;
///   ; Pattern (any cond, opc1=0, MCR, CRn=7, p15, RES1 bit 4):
///   ;   bits[27:16] == 0xE07
///   ;   bits[11:8]  == 0xF
///   ;   bit[4]      == 1
///   ;   mask 0x0FFF_0F10, expected 0x0E07_0F10
///   ft+7:  ldr r1, [lr, #-8]         ; r1 = faulting instr (BE-8 view)
///   ft+8:  rev r1, r1                ; r1 = kernel-format word
///   ft+9:  ldr r0, [pc, #L_C7_MASK]  ; r0 = 0x0FFF_0F10
///   ft+10: and r1, r1, r0            ; r1 &= mask
///   ft+11: ldr r0, [pc, #L_C7_PATT]  ; r0 = 0x0E07_0F10
///   ft+12: teq r1, r0
///   ft+13: beq C7_NOOP               ; → ft+34
///   ; existing DFSC dispatch
///   ft+14: mrc p15,0,r0,c5,c0,0      ; R0 = DFSR
///   ft+15: and r0, r0, #0xF          ; R0 = DFSC[3:0]
///   ft+16: cmp r0, #7                ; translation, page (most common)
///   ft+17: beq FAST_FWD              ; → ft+30
///   ft+18: cmp r0, #15               ; permission, page
///   ft+19: beq FAST_FWD
///   ft+20: nop                       ; (was: cmp r0, #5 — see iter-60 below)
///   ft+21: nop                       ; (was: beq FAST_FWD — see iter-60 below)
///   ft+22: cmp r0, #13               ; permission, section
///   ft+23: beq FAST_FWD
///   ft+24: cmp r0, #6                ; access flag, page
///   ft+25: beq FAST_FWD
///   ft+26: cmp r0, #3                ; access flag, section
///   ft+27: beq FAST_FWD
///   ; Slow-path fall-through: both R0 and R1 were clobbered (R0 by
///   ; the DABT_SAVE_PA load + DFSR read, R1 by the SPSR_abt save and
///   ; the c7-MCR check). Restore both before branching so the slow
///   ; trampoline's own `mcr ...,c13,c0,2/3` sees the original
///   ; pre-abt values when it re-saves them.
///   ft+28: mrc p15,0,r0,c13,c0,2     ; restore R0 from TPIDRURW
///   ft+29: mrc p15,0,r1,c13,c0,3     ; restore R1 from TPIDRRO
///   ft+30: b SLOW_DABT_TRAMP         ; → DABT_TRAMP_OFFSET (slow EL2 path)
///   ; FAST_FWD:
///   ft+31: mrc p15,0,r0,c13,c0,2     ; restore R0 from TPIDRURW
///   ft+32: mrc p15,0,r1,c13,c0,3     ; restore R1 from TPIDRRO
///   ft+33: ldr pc, [pc, #-4]         ; pc+8-4 = ft+33+4 → literal at ft+34
///   ft+34: literal: 0x00393114       ; kernel DataAbortHandler VA
///   ; C7_NOOP:
///   ft+35: mrc p15,0,r0,c13,c0,2     ; restore R0
///   ft+36: mrc p15,0,r1,c13,c0,3     ; restore R1
///   ft+37: subs pc, lr, #4           ; ERET to faulting_PC + 4
///                                    ;   (LR_abt = faulting_PC + 8)
///   ; literals
///   ft+38: literal: DABT_SAVE_PA     ; for `ldr r0` at ft+2
///   ft+39: literal: 0x0FFF_0F10      ; for `ldr r0` at ft+9
///   ft+40: literal: 0x0E07_0F10      ; for `ldr r0` at ft+11
///
/// 41 words × 4 = 164 bytes; reserved region is 256 bytes so 92
/// bytes of slack remain.
///
/// Cost reduction: at iter-58 the DABT trampoline took an EL2 round-
/// trip on every fault. iter-59 measured 20.8 M HVC #DIAG_TAG hits in
/// ~30 s of wall (DFSCs 0x07/0x0F dominating, all forwarded to kernel
/// DAH). Bypassing the EL2 round-trip for those cases is a direct
/// win — same kernel-side execution, no hypervisor overhead. The
/// iter-105 LR/SP/SPSR save adds ~5 instructions to the fast path,
/// negligible relative to the EL2 round-trip we're avoiding.
///
/// iter-60: DFSC=0x05 (translation, section) is *deliberately
/// excluded* from the fast path (slots ft+13/ft+14 left as NOPs). For
/// section-level translation faults ARMv7 leaves DFSR.Domain UNK
/// (= 0); the kernel's `GetDomainAndFaultMonitorFromDomainNumber(0)`
/// then returns no monitor and DAH throws `evt.ex.abt.bus`. Pre-iter-
/// 59 the EL2 path (`handle_dabt_dispatch` today) synthesised
/// DFSR.Domain from L1[FAR>>20][8:5] before forwarding to DAH; iter-
/// 59 bypassed that path. The minimal fix is to let DFSC=5 fall
/// through to the slow EL2 path, which still does the synthesis.
/// Section-level faults fire only on first touch of a 1 MiB section
/// (~tens of times per boot for freshly-allocated stacks), so the
/// slow-path cost is negligible.
///
/// SAFETY: writes 41 words in the reserved range
/// `DABT_FAST_TRAMP_OFFSET .. + 41*4`. Caller owns the ROM backing.
pub unsafe fn install_dabt_fast_trampoline(rom_ptr: *mut u32) {
    unsafe {
        let ft = DABT_FAST_TRAMP_OFFSET / 4;

        // Helper: encode `beq imm24` from offset_within_trampoline `from`
        // to slot `to`. ARMv7 imm24 is in WORDS, signed; PC-relative target
        // = (PC + 8) + (imm24 << 2). PC at slot N is `OFFSET + N*4`, so
        // imm24 = (to - from - 2).
        let beq = |from: usize, to: usize| -> u32 {
            let imm24 = ((to as i32) - (from as i32) - 2) as u32 & 0x00FF_FFFF;
            0x0A00_0000 | imm24
        };
        // Helper: encode `b imm24` to a far target (DABT_TRAMP_OFFSET).
        let b_far = |from_byte_offset: u32, to_byte_offset: u32| -> u32 {
            let pc_plus_8 = from_byte_offset.wrapping_add(8);
            let off_bytes = (to_byte_offset as i32) - (pc_plus_8 as i32);
            let imm24 = ((off_bytes >> 2) as u32) & 0x00FF_FFFF;
            0xEA00_0000 | imm24
        };
        // `ldr r0, [pc, #imm12]` literal-load. `from` and `to` are
        // word indices within the trampoline. PC at execute time of
        // an LDR at slot `from` is `(from + 2) * 4` bytes; the
        // architectural offset is the literal slot's byte offset
        // minus that PC value.
        let ldr_r0_lit = |from: usize, to: usize| -> u32 {
            let pc_at_exec = ((from as u32) + 2) * 4;
            let target_byte = (to as u32) * 4;
            let imm12 = target_byte.wrapping_sub(pc_at_exec) & 0xFFF;
            0xE59F_0000 | imm12
        };

        // Slot writes (instructions, native-LE u32 = LE encoding for
        // BE-8 instruction fetch):
        write_rom_code_word(rom_ptr, ft +  0, 0xEE0D_0F50); // mcr p15,0,r0,c13,c0,2
        write_rom_code_word(rom_ptr, ft +  1, 0xEE0D_1F70); // mcr p15,0,r1,c13,c0,3
        // iter-105: save LR_abt / SP_abt / SPSR_abt to DABT_SAVE_PA so
        // the DAH-MRS-SPSR HVC handler reads a current value, not a
        // stale-or-zero leftover from a long-ago slow-path DABT.
        write_rom_code_word(rom_ptr, ft +  2, ldr_r0_lit(2, 38));  // ldr r0, [pc, #..] = DABT_SAVE_PA
        write_rom_code_word(rom_ptr, ft +  3, 0xE580_E000); // str lr, [r0]
        write_rom_code_word(rom_ptr, ft +  4, 0xE580_D004); // str sp, [r0, #4]
        write_rom_code_word(rom_ptr, ft +  5, 0xE14F_1000); // mrs r1, spsr
        write_rom_code_word(rom_ptr, ft +  6, 0xE580_1008); // str r1, [r0, #8]
        // iter-105: c7 cache-maintenance MCR filter — see header.
        write_rom_code_word(rom_ptr, ft +  7, 0xE51E_1008); // ldr r1, [lr, #-8]
        write_rom_code_word(rom_ptr, ft +  8, 0xE6BF_1F31); // rev r1, r1
        write_rom_code_word(rom_ptr, ft +  9, ldr_r0_lit(9, 39));  // ldr r0, [pc, #..] = mask
        write_rom_code_word(rom_ptr, ft + 10, 0xE001_1000); // and r1, r1, r0
        write_rom_code_word(rom_ptr, ft + 11, ldr_r0_lit(11, 40)); // ldr r0, [pc, #..] = pattern
        write_rom_code_word(rom_ptr, ft + 12, 0xE131_0000); // teq r1, r0
        write_rom_code_word(rom_ptr, ft + 13, beq(13, 35)); // beq C7_NOOP
        // existing DFSC dispatch
        write_rom_code_word(rom_ptr, ft + 14, 0xEE15_0F10); // mrc p15,0,r0,c5,c0,0 (DFSR)
        write_rom_code_word(rom_ptr, ft + 15, 0xE200_000F); // and r0, r0, #0xF
        write_rom_code_word(rom_ptr, ft + 16, 0xE350_0007); // cmp r0, #7
        write_rom_code_word(rom_ptr, ft + 17, beq(17, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 18, 0xE350_000F); // cmp r0, #15
        write_rom_code_word(rom_ptr, ft + 19, beq(19, 31)); // beq FAST_FWD
        // DFSC=0x09 (Domain fault, first level / section). The Newton
        // kernel raises this deliberately at TCardSocket::GetChipInfo
        // (PC ~0x55714+): the PCMCIA probe maps the candidate window
        // VA via a section descriptor with domain=15 ("No access" in
        // DACR=0x00055555), then accesses it inside a setjmp +
        // AddExceptionHandler frame. The kernel's own DataAbortHandler
        // must reach the user exception handler to longjmp past the
        // probe — forward this DFSC to the kernel rather than DIAG'ing.
        // Verified against ARM ARM B4.1.52 + Table B3-23 (short-
        // descriptor FSR encoding: 0b01001 = first-level domain fault).
        // (iter-60: DFSC=0x05 deliberately excluded — see file-level
        // comment for rationale. The two slots formerly reserved as
        // NOPs are reused here; existing beq / b-SLOW offsets unchanged.)
        write_rom_code_word(rom_ptr, ft + 20, 0xE350_0009); // cmp r0, #9
        write_rom_code_word(rom_ptr, ft + 21, beq(21, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 22, 0xE350_000D); // cmp r0, #13
        write_rom_code_word(rom_ptr, ft + 23, beq(23, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 24, 0xE350_0006); // cmp r0, #6
        write_rom_code_word(rom_ptr, ft + 25, beq(25, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 26, 0xE350_0003); // cmp r0, #3
        write_rom_code_word(rom_ptr, ft + 27, beq(27, 31)); // beq FAST_FWD
        // Slow-path fall-through: BOTH R0 and R1 were clobbered (R0
        // by the SAVE_PA literal load + DFSR read, R1 by SPSR_abt and
        // the c7-MCR check). Restore both so the slow DABT_TRAMP's
        // `mcr p15,0,r0/r1,c13,c0,{2,3}` sees the original pre-abt
        // values when it re-saves them to TPIDR.
        write_rom_code_word(rom_ptr, ft + 28, 0xEE1D_0F50); // mrc p15,0,r0,c13,c0,2 (restore R0)
        write_rom_code_word(rom_ptr, ft + 29, 0xEE1D_1F70); // mrc p15,0,r1,c13,c0,3 (restore R1)
        write_rom_code_word(rom_ptr, ft + 30, b_far(
            (DABT_FAST_TRAMP_OFFSET as u32).wrapping_add(30 * 4),
            DABT_TRAMP_OFFSET as u32,
        ));                                                  // b SLOW_DABT_TRAMP
        // FAST_FWD:
        write_rom_code_word(rom_ptr, ft + 31, 0xEE1D_0F50); // mrc p15,0,r0,c13,c0,2 (restore R0)
        write_rom_code_word(rom_ptr, ft + 32, 0xEE1D_1F70); // mrc p15,0,r1,c13,c0,3 (restore R1)
        write_rom_code_word(rom_ptr, ft + 33, 0xE51F_F004); // ldr pc, [pc, #-4]
        write_rom_data_word(rom_ptr, ft + 34, DABT_FAST_TRAMP_DAH_TARGET);
        // C7_NOOP: cache-maintenance MCR is a no-op on a coherent
        // host. Restore the registers we stashed and ERET to
        // faulting_PC + 4 in the pre-abt mode (LR_abt = pc + 8 at
        // entry, so subs lr - 4 = faulting_PC + 4).
        write_rom_code_word(rom_ptr, ft + 35, 0xEE1D_0F50); // mrc p15,0,r0,c13,c0,2 (restore R0)
        write_rom_code_word(rom_ptr, ft + 36, 0xEE1D_1F70); // mrc p15,0,r1,c13,c0,3 (restore R1)
        write_rom_code_word(rom_ptr, ft + 37, 0xE25E_F004); // subs pc, lr, #4
        // Literal slots — all loaded via BE-8 LDR.
        write_rom_data_word(rom_ptr, ft + 38, DABT_SAVE_PA);
        write_rom_data_word(rom_ptr, ft + 39, 0x0FFF_0F10);
        write_rom_data_word(rom_ptr, ft + 40, 0x0E07_0F10);
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

unsafe fn patch_und_vector(rom: *mut u32) {
    // The trampoline's save-slot address is held in the literal at
    // offset 0x30. Pre-MMU we use the RAM *IPA* 0x0400_5F00 directly
    // (since VA == IPA with the MMU off, and 0x0400_5F00 is inside
    // our stage-2 RAM mapping). Once the guest enables its stage-1
    // MMU, VA 0x0400_xxxx aliases ROM (read-only) under the kernel's
    // L1[0x40] section, so `install_und_vector_swap_post_mmu()` swaps
    // the literal to the VA 0x0C00_4F00, which the kernel's
    // L1[0xC0] coarse → L2[0x04] small page maps back to RAM.

    // UND vector at IPA 0x04 → branch to FPA bypass stub. The stub
    // routes FPA-class UNDs straight to the kernel's FPE handler
    // (matching SA-110 hardware behaviour) and falls through to the
    // existing trampoline for everything else.
    //
    // ARM B (immediate): cond=AL=0xE, opcode=1010, imm24 = (target -
    // (PC+8)) / 4. PC at IPA 0x04 = 0x04, PC+8 = 0x0C.
    let imm24_to_bypass =
        ((FPA_BYPASS_STUB_OFFSET as u32 - 0x0C) / 4) & 0x00FF_FFFF;
    let branch_to_bypass = 0xEA00_0000 | imm24_to_bypass;

    // SAFETY: offsets below all sit in 0x00FF_FEC0..0x00FF_FF60,
    // well under ROM_SIZE (= 16 MiB = 0x0100_0000) and inside the
    // reserved window checked by `tracer::in_reserved_range`.
    unsafe {
        write_rom_code_word(rom, 1, branch_to_bypass);   // 0x04: b FPA_BYPASS_STUB_OFFSET

        // FPA-class UND bypass stub. See `FPA_BYPASS_STUB_OFFSET`
        // doc comment for the per-word commentary; reproduced here
        // alongside the encodings.
        //
        // Two-stage check: bits[27:24] in {0xC, 0xD, 0xE} (LDC/STC/CDP/MCR),
        // *then* bits[11:8] in {1, 2} (FPA cp_num). The first stage rules
        // out UDF (bits[27:24]=0x7), software breakpoints, tracer UDFs,
        // shadow-byte-access UDFs (all bits[27:24]=0x7), and other
        // non-coprocessor UND-causing insns. The second stage rules out
        // VFP/Advanced-SIMD (cp_num 10/11) — though those don't appear
        // in 717006 ROM, the check keeps the stub future-proof.
        let s = FPA_BYPASS_STUB_OFFSET / 4;
        write_rom_code_word(rom, s +  0, 0xEE0D_CF50);  // mcr p15,0,r12,c13,c0,2
        write_rom_code_word(rom, s +  1, 0xE51E_C004);  // ldr r12, [lr, #-4]
        write_rom_code_word(rom, s +  2, 0xE20C_C40F);  // and r12, r12, #0x0F000000
        write_rom_code_word(rom, s +  3, 0xE35C_040C);  // cmp r12, #0x0C000000
        write_rom_code_word(rom, s +  4, 0x135C_040D);  // cmpne r12, #0x0D000000
        write_rom_code_word(rom, s +  5, 0x135C_040E);  // cmpne r12, #0x0E000000
        write_rom_code_word(rom, s +  6, 0x1A00_0006);  // bne stub+0x38 (fall_through)
        write_rom_code_word(rom, s +  7, 0xE51E_C004);  // ldr r12, [lr, #-4]  (reload)
        write_rom_code_word(rom, s +  8, 0xE20C_CC0F);  // and r12, r12, #0xF00
        write_rom_code_word(rom, s +  9, 0xE35C_0C01);  // cmp r12, #0x100
        write_rom_code_word(rom, s + 10, 0x135C_0C02);  // cmpne r12, #0x200
        write_rom_code_word(rom, s + 11, 0x1A00_0001);  // bne stub+0x38 (fall_through)
        write_rom_code_word(rom, s + 12, 0xEE1D_CF50);  // mrc p15,0,r12,c13,c0,2  (FPA)
        // b FPE_JT (0x38d874). PC at this insn = stub+0x34.
        // PC+8 = stub+0x3C. imm24 = (target - PC+8) / 4.
        let pc_plus_8 = (FPA_BYPASS_STUB_OFFSET + 0x34 + 8) as i32;
        let target_fpe = 0x0038_D874_i32;
        let imm24_fpe =
            (((target_fpe - pc_plus_8) >> 2) as u32) & 0x00FF_FFFF;
        write_rom_code_word(rom, s + 13, 0xEA00_0000 | imm24_fpe); // b FPE_JT
        write_rom_code_word(rom, s + 14, 0xEE1D_CF50);  // mrc p15,0,r12,c13,c0,2  (fall_through)
        // b UND_TRAMP_OFFSET. PC+8 = stub+0x44 = 0x00FF_FF04. Target =
        // 0x00FF_FF00. offset = -4 bytes = -1 word; imm24 = 0xFFFFFF.
        let pc_plus_8b = (FPA_BYPASS_STUB_OFFSET + 0x3C + 8) as i32;
        let target_tramp = UND_TRAMP_OFFSET as i32;
        let imm24_tramp =
            (((target_tramp - pc_plus_8b) >> 2) as u32) & 0x00FF_FFFF;
        write_rom_code_word(rom, s + 15, 0xEA00_0000 | imm24_tramp); // b UND_TRAMP

        let base = UND_TRAMP_OFFSET / 4;
        write_rom_code_word(rom, base +  0, 0xEE0D_CF50);  // mcr p15,0,r12,c13,c0,2
        write_rom_code_word(rom, base +  1, 0xE59F_C050);  // ldr r12, [pc, #0x50]
        write_rom_code_word(rom, base +  2, 0xE58C_000C);  // str r0, [r12, #0x0C]
        write_rom_code_word(rom, base +  3, 0xE58C_1010);  // str r1, [r12, #0x10]
        write_rom_code_word(rom, base +  4, 0xE58C_E000);  // str lr, [r12]
        write_rom_code_word(rom, base +  5, 0xE14F_0000);  // mrs r0, SPSR
        write_rom_code_word(rom, base +  6, 0xE58C_0004);  // str r0, [r12, #4]
        write_rom_code_word(rom, base +  7, 0xE58C_2014);  // str r2, [r12, #0x14]
        write_rom_code_word(rom, base +  8, 0xE200_101F);  // and r1, r0, #0x1F
        write_rom_code_word(rom, base +  9, 0xE381_10C0);  // orr r1, r1, #0xC0
        write_rom_code_word(rom, base + 10, 0xE351_00D0);  // cmp r1, #0xD0
        write_rom_code_word(rom, base + 11, 0x03A0_10DF);  // moveq r1, #0xDF
        write_rom_code_word(rom, base + 12, 0xE129_F001);  // msr cpsr_c, r1
        write_rom_code_word(rom, base + 13, 0xE58C_D018);  // str sp, [r12, #0x18]
        write_rom_code_word(rom, base + 14, 0xE58C_E01C);  // str lr, [r12, #0x1C]
        write_rom_code_word(rom, base + 15, 0xE321_F0DB);  // msr cpsr_c, #0xdb (UND)
        write_rom_code_word(rom, base + 16, 0xE59C_2014);  // ldr r2, [r12, #0x14]
        write_rom_code_word(rom, base + 17, 0xE321_F0D3);  // msr cpsr_c, #0xd3 (SVC)
        write_rom_code_word(rom, base + 18, 0xE1A0_000E);  // mov r0, lr
        write_rom_code_word(rom, base + 19, 0xE58C_0008);  // str r0, [r12, #8]
        write_rom_code_word(rom, base + 20, 0xE321_F0DB);  // msr cpsr_c, #0xdb (UND)
        write_rom_code_word(rom, base + 21, HvcImm::Und.insn());  // hvc #0x10
        write_rom_code_word(rom, base + 22, 0xEAFF_FFFE);  // b . (trap)
        // Literal slot — loaded by the `ldr r12, [pc, #0x50]` at base+1
        // under BE-8, so write as data.
        write_rom_data_word(rom, base + 23, crate::trap::HYP_TRAMP_SCRATCH_BASE);

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
        //
        //   +0x00: e59fe000  ldr lr, [pc, #0]    ; lr = *(stub + 8)
        //   +0x04: e1b0f00e  movs pc, lr         ; CPSR = SPSR_und, PC = lr
        //   +0x08: <target PC literal, updated per ERET>
        //
        // Iter-28 attempted to extend this stub to also reload banked
        // SPSR_und from UND_SAVE_SPSR_IPA via `MSR SPSR_cxsf, lr` (so
        // flag-emulating probes' SPSR updates would propagate). The
        // attempt failed: with the MSR in place QEMU raspi3b broke the
        // boot with cascading data aborts at the next kernel store
        // (0x186b4). Loads + movs without the MSR are stable; only
        // the MSR triggers the regression. Suspected QEMU raspi3b
        // banked-SPSR-write quirk (consistent with docs/QEMU_BUGS.md
        // Bug #1's family of `banked_spsr[]` indexing issues; possibly
        // distinct because Bug #1 is about EL2 → AArch32 propagation,
        // whereas this is AArch32-internal MSR SPSR from UND mode).
        // Reverted to the 3-word stub. Approach (b) — emulate the bne
        // at 0x257088 directly via ELR_EL2 in an HVC handler — is the
        // path forward; it sidesteps SPSR entirely.
        let stub = UND_RETURN_STUB_OFFSET / 4;
        write_rom_code_word(rom, stub + 0, 0xE59F_E000); // ldr lr, [pc, #0]
        write_rom_code_word(rom, stub + 1, 0xE1B0_F00E); // movs pc, lr
        // Literal slot — loaded via the `ldr lr, [pc, #0]` at stub+0
        // under BE-8 each ERET; the runtime updater also lives in
        // `trap.rs`. Placeholder is data.
        write_rom_data_word(rom, stub + 2, 0xDEAD_C0DE);
    }

    // No per-range I-cache publish here: the single
    // `icache_publish_range` sweep over the whole ROM aperture at the
    // end of `load_newton_rom` runs strictly after this function and
    // covers the UND vector word at IPA 0x04, the trampoline body, the
    // FPA bypass stub, UND_TRAMP, and UND_RETURN_STUB with the same
    // DC CVAU; DSB; IC IVAU; DSB; ISB sequence over a wider range.
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
        // Same code/data discipline as patch_cp15_encodings: only
        // rewrite words the classifier marks as code. The exact-word
        // match below makes a false positive unlikely, but a data word
        // equal to 0xEE00_EA10 would still be silently corrupted
        // without this gate.
        if !rom_word_is_code(j) {
            continue;
        }
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
        // All these are instruction rewrites in REx code, so go
        // through write_rom_code_word so BE-8 sees the right encoding.
        let lead = unsafe { rom.add(mov_idx).read() };
        let new_lead = (lead & !0x0000_F000) | 0x0000_C000;
        unsafe { write_rom_code_word(rom, mov_idx, new_lead); }

        if let Some(ai) = add_idx {
            let add = unsafe { rom.add(ai).read() };
            let new_add = (add & !0x000F_F000) | 0x000C_C000;
            unsafe { write_rom_code_word(rom, ai, new_add); }
        }

        let new_mcr = MCR_P10_R12;
        unsafe { write_rom_code_word(rom, j, new_mcr); }
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
    // UND trampoline and the DABT diagnostic trampoline.
    //
    // With HYP_TRAMP_SCRATCH_BASE relocated into the SCRATCH_POOL IPA
    // window, the same value works both pre-MMU (stage-1 off → IPA →
    // stage-2 → host SCRATCH_POOL) and post-MMU (kernel L1[0x60] →
    // IPA → stage-2). The swap therefore writes the same literal as
    // the pre-MMU install path; kept as a callable no-op to preserve
    // the install/uninstall contract for future changes that might
    // re-introduce a swap.
    //
    // The literals are LDR-loaded data — under BE-8 they must be
    // byte-swapped on host so a CPSR.E=1 LDR returns the intended value.
    unsafe {
        let rom = rom_host_pa() as *mut u32;
        let base = UND_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, base + 23, crate::trap::HYP_TRAMP_SCRATCH_BASE);
        let db = DABT_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, db + 14, DABT_SAVE_PA);
    }
}

/// Revert the trampoline's save-slot literal back to the pre-MMU value.
/// Called when the guest turns its stage-1 MMU off — typically the
/// SWIBoot→ROMBoot soft-reset path. (Now a no-op given
/// HYP_TRAMP_SCRATCH_BASE works pre + post-MMU; retained for symmetry
/// with the post-MMU swap.)
pub unsafe fn install_und_vector_swap_pre_mmu() {
    // SAFETY: same as the post-MMU swap above. Literal slots are data
    // under BE-8.
    unsafe {
        let rom = rom_host_pa() as *mut u32;
        let base = UND_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, base + 23, crate::trap::HYP_TRAMP_SCRATCH_BASE);
        let db = DABT_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, db + 14, DABT_SAVE_PA);
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
    let mut first_pcs: [u32; 4] = [0; 4];
    for i in 0..word_count {
        // Only rewrite words the classifier marks as code. A *data*
        // word (stored BE, read back byteswapped) that happens to match
        // the ~15-fixed-bit MCR/MRC shape would otherwise be silently
        // corrupted through `write_rom_code_word`. The current ROM+REx
        // pair has no false hits, but every Einstein.rex rebuild
        // re-rolls those dice.
        if !rom_word_is_code(i) {
            continue;
        }
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
        // SAFETY: same index, in-range. Code rewrite — under BE-8 we
        // need the BE-numerical encoding stored as native u32.
        unsafe { write_rom_code_word(rom, i, new); }
        if count < first_pcs.len() {
            first_pcs[count] = (i * 4) as u32;
        }
        count += 1;
    }
    if count > 0 {
        let shown = count.min(first_pcs.len());
        kprintln!(
            "guest_mem: patch_cp15_encodings: {} code words rewritten; first PCs: {:#x?}",
            count, &first_pcs[..shown],
        );
    }
    count
}
