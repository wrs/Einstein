//! Stage-2 MMU: guest-physical → host-physical translation.
//!
//! We back the Newton guest physical layout out of our own `guest_mem`
//! regions and leave every other IPA unmapped so stage-2 faults trap to EL2:
//!
//!   Guest IPA                       Host PA                  Perms
//!   0x0000_0000..0x0100_0000 ROM    guest_mem::rom_host_pa() R/-
//!   0x0400_0000..0x0440_0000 RAM    guest_mem::ram_host_pa() RW
//!   everything else                                          stage-2 fault
//!
//! Stage-2 table layout at 4 KiB granule, T0SZ=32, SL0=1 (start at level 1):
//!   L1: 512 × 1 GiB; [0] → L2, rest invalid.
//!   L2: 512 × 2 MiB block descriptors; each entry is either a block
//!       mapping to host PA or invalid (fault).

use core::ptr::addr_of_mut;

use crate::{guest_mem, kprintln, peripherals};

// VMSAv8-64 stage-2 descriptor bits
const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1;
const DESC_BLOCK: u64 = 0 << 1;
// At L3 the descriptor-type field is `11` (valid + page). Same bit
// positions as a table descriptor at L1/L2 — architecture disambiguates
// by level rather than by bits.
const DESC_PAGE: u64 = 1 << 1;

const S2_MEMATTR_NORMAL_WB: u64 = 0b1111 << 2;
const S2_AP_READ: u64 = 0b01 << 6;
const S2_AP_WRITE: u64 = 0b10 << 6;
const S2_AP_RW: u64 = S2_AP_READ | S2_AP_WRITE;
const S2_AP_RO: u64 = S2_AP_READ;
const S2_SH_INNER: u64 = 0b11 << 8;
const S2_AF: u64 = 1 << 10;

const BLOCK_COMMON: u64 = DESC_VALID | DESC_BLOCK
    | S2_MEMATTR_NORMAL_WB | S2_SH_INNER | S2_AF;
const BLOCK_NORMAL_RO: u64 = BLOCK_COMMON | S2_AP_RO;
const BLOCK_NORMAL_RW: u64 = BLOCK_COMMON | S2_AP_RW;

// L3 page descriptor — same attribute bits as a block descriptor, just
// different type field and naturally smaller (4 KiB).
const PAGE_COMMON: u64 = DESC_VALID | DESC_PAGE
    | S2_MEMATTR_NORMAL_WB | S2_SH_INNER | S2_AF;
const PAGE_NORMAL_RO: u64 = PAGE_COMMON | S2_AP_RO;
const PAGE_NORMAL_RW: u64 = PAGE_COMMON | S2_AP_RW;

/// Stage-2 XN bit (bit 54) — single-bit on ARMv8.0-A (Cortex-A53).
/// Setting this raises an instruction abort on any fetch to this
/// IPA from the guest.
const S2_XN: u64 = 1 << 54;

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

static mut S2_L1: PageTable = PageTable([0; 512]);
static mut S2_L2: PageTable = PageTable([0; 512]);

// L3 table refining the single 2 MiB L2 slot that covers the Newton
// peripheral window at IPA 0x0F00_0000..0x0F20_0000. Used so we can
// install one 4 KiB non-trapping page (the tick register) without
// exposing the whole peripheral range as RAM — other 4 KiB entries
// in this L3 stay invalid and continue to stage-2-fault into mmio::.
static mut S2_L3_HW_TICKS: PageTable = PageTable([0; 512]);

// L3 table refining the 2 MiB L2 slot that covers the shadow-stub
// scratch carve-out at IPA 0x0180_0000..0x01A0_0000 (kernel VA
// 0x0180_0000 mapped via stage-1 L1[0x18]). The first 16 4 KiB pages
// are populated by `install_scratch_pool` and back the
// `shadow_stub::SCRATCH_POOL` host buffer. The remaining 496 entries
// stay invalid and stage-2-fault if anything outside the populated
// 64 KiB ever gets accessed (defensive — the ScratchVA stubs only
// touch their own slot).
static mut S2_L3_SCRATCH: PageTable = PageTable([0; 512]);

// L3 tables refining the two 2 MiB L2 slots covering guest RAM
// (IPA 0x0400_0000..0x0440_0000). Each L3 slot is a 4 KiB page; the
// shadow-stub flips per-page permissions between `RW + XN` (initial,
// and re-armed after a write to a previously-executed code page) and
// `RO + ¬XN` (post-scan, the page is executable and frozen). The
// state machine lets us re-scan a code page when it's overwritten by
// Newton's demand-pager.
static mut S2_L3_RAM_0: PageTable = PageTable([0; 512]);
static mut S2_L3_RAM_1: PageTable = PageTable([0; 512]);

// Backing page for the Newton tick clock, mapped read-only at stage-2
// at IPA 0x0F181000. The guest reads K_HDWR_TICKS (0x0F181800) hot in
// busy-wait delay loops — ~75 % of all stage-2 faults before this page
// existed. With stage-2 pointing the whole 4 KiB page at this RAM
// backing, reads become cache-coherent loads from the guest's
// perspective, no trap. EL2 periodically writes the current
// `vic::ticks()` value into offset 0x800 from the CNTHP IRQ handler
// and from a few other forward-progress hooks; see `tick_page::update`.
//
// Offsets 0x000 (calendar) and 0x400 (alarm) also live in this page;
// they currently always read as 0 from `vic::read`, which matches the
// page's zero-initialised state, so the non-trapping fast path returns
// the right value for them too. Writes to anywhere in the page still
// stage-2-fault (RO), so vic::write is still reached for register
// writes the kernel does.
#[repr(C, align(4096))]
pub(crate) struct TickPage(pub [u8; 4096]);
pub(crate) static mut TICK_PAGE: TickPage = TickPage([0; 4096]);

// Offset of K_HDWR_TICKS within the 4 KiB page. The other registers
// that share this page (calendar at +0x000, alarm at +0x400) are read
// as zero by the guest today; the zero-initialised backing matches
// what `vic::read` returns for them, so we don't need to wire them up
// explicitly.
pub(crate) const TICK_OFFSET_CALENDAR: usize = 0x000;
pub(crate) const TICK_OFFSET_TICKS: usize = 0x800;

// Base IPA of the 4 KiB page holding the tick cluster (calendar / alarm
// / ticks). The L3 slot index is `(base / 4 KiB) % 512` — we pick
// whichever L3 is covering the enclosing 2 MiB block.
const TICK_PAGE_IPA: u64 = 0x0F18_1000;

const TWO_MIB: u64 = 0x0020_0000;

// IPA ranges the guest expects. Keep in sync with TMemoryConsts on the
// Einstein side.
pub const ROM_IPA_BASE: u64 = 0x0000_0000;
pub const ROM_IPA_SIZE: u64 = 0x0100_0000; // 16 MiB
// Flash is split in two disjoint windows on real Newton hardware:
// bank 0 at `kFlashBank1` (0x02000000..0x02400000) and bank 1 at
// `kFlashBank2` (0x10000000..0x10400000), each 4 MiB. Einstein keeps
// both banks back-to-back in a single 8 MiB backing; the mapping
// below surfaces each half at the right guest IPA.
pub const FLASH_BANK_IPA_SIZE: u64 = 0x0040_0000; // 4 MiB per bank
pub const FLASH_BANK0_IPA_BASE: u64 = 0x0200_0000;
pub const FLASH_BANK1_IPA_BASE: u64 = 0x1000_0000;
pub const RAM_IPA_BASE: u64 = 0x0400_0000;
pub const RAM_IPA_SIZE: u64 = 0x0040_0000; // 4 MiB
// There is intentionally no IPA 0x0C mirror. Einstein's `TMemoryConsts`
// and `TMMU.cpp:1186-1193` document the real Newton layout: `kRAMStart =
// 0x04000000` is the only RAM PA; VA `0x0C000000+` is purely a stage-1
// remap to discrete 4 KiB pages in PA `0x04xxxxxx`. A blanket mirror at
// IPA `0x0C` would alias every pre-MMU 0x0C access to a contiguous RAM
// window that stage-1 will then remap to a *different* PA, causing
// pre-MMU writes and post-MMU reads to land in different host cells.
// Framebuffer scratch: a dumpable RAM region where guest screen drivers can
// deposit pixels. Not yet wired to any Newton display emulation; the region
// exists so M5 can point `TScreenManager`-equivalent code at it.
pub const FB_IPA_BASE: u64 = 0x0E00_0000;
pub const FB_IPA_SIZE: u64 = 0x0020_0000; // 2 MiB

const VTCR_EL2_VAL: u64 = (32 << 0)
    | (0b01 << 6)          // SL0 = start at level 1
    | (0b01 << 8)          // IRGN0 = WB cacheable
    | (0b01 << 10)         // ORGN0 = WB cacheable
    | (0b11 << 12)         // SH0 = inner shareable
    | (0b00 << 14)         // TG0 = 4 KiB
    | (0b010 << 16)        // PS = 40-bit
    | (1u64 << 31);        // RES1 (DDI 0487 VTCR_EL2 description)

/// Return a mutable pointer to the L3 entry covering the 4 KiB RAM page
/// at `ipa`. None if `ipa` is outside the 4 MiB RAM aperture.
fn ram_l3_entry_ptr(ipa: u32) -> Option<*mut u64> {
    let ipa64 = ipa as u64;
    if ipa64 < RAM_IPA_BASE || ipa64 >= RAM_IPA_BASE + RAM_IPA_SIZE {
        return None;
    }
    let off = ipa64 - RAM_IPA_BASE;
    let table_ix = (off / TWO_MIB) as usize;         // 0 or 1
    let slot_ix = ((off % TWO_MIB) / 0x1000) as usize; // 0..512
    let base = match table_ix {
        0 => addr_of_mut!(S2_L3_RAM_0) as *mut u64,
        1 => addr_of_mut!(S2_L3_RAM_1) as *mut u64,
        _ => return None,
    };
    // SAFETY: slot_ix < 512.
    Some(unsafe { base.add(slot_ix) })
}

fn invalidate_ipa_s2(ipa: u32) {
    // IPA shift for TLBI IPAS2E1IS is 12 (the instruction takes bits [47:12]).
    let arg = (ipa as u64) >> 12;
    // SAFETY: stage-2 TLB maintenance.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "tlbi ipas2e1is, {0}",
            "dsb ish",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            in(reg) arg,
            options(nostack, preserves_flags),
        );
    }
}

/// Read back the stage-2 L3 entry covering the 4 KiB RAM page at
/// `ipa`. None when `ipa` is outside the RAM aperture. Diagnostic
/// only — used by `heap_watch` to verify a permission flip actually
/// landed in the table.
pub fn ram_page_l3_entry(ipa: u32) -> Option<u64> {
    let page = ipa & !0xFFF;
    let entry_ptr = ram_l3_entry_ptr(page)?;
    // SAFETY: pointer bounded to one of two 512-entry L3 tables.
    Some(unsafe { entry_ptr.read() })
}

/// Flip the stage-2 L3 entry for the 4 KiB RAM page at `ipa` to
/// `RO + executable`. Called by the shadow-stub after scan+patch on
/// an instruction-abort from guest code in RAM: subsequent fetches
/// succeed, and writes fault so the hypervisor can re-arm RW+XN and
/// re-scan the page on the next execute.
///
/// No-op when `ipa` is outside the RAM aperture.
pub unsafe fn set_ram_page_ro_x(ipa: u32) {
    let page = ipa & !0xFFF;
    let Some(entry_ptr) = ram_l3_entry_ptr(page) else { return; };
    let host_pa = guest_mem::ram_host_pa() + (page as u64 - RAM_IPA_BASE);
    let new = host_pa | PAGE_NORMAL_RO;
    // SAFETY: entry_ptr bounded to one of two 512-entry L3 tables.
    unsafe { entry_ptr.write(new); }
    invalidate_ipa_s2(page);
}

/// Flip the stage-2 L3 entry for the 4 KiB RAM page at `ipa` to
/// `RO + execute-never`. Same trapping behaviour as `set_ram_page_ro_x`
/// for writes (write-permission fault to EL2) but unlike the ro_x
/// variant, instruction fetches also fault. Used by the Group-1
/// kernel-globals self-map capture probe — those PA pages back the
/// guest L1/L2 page-tables and should never be executed, so XN is
/// the correct hardening.
pub unsafe fn set_ram_page_ro_xn(ipa: u32) {
    let page = ipa & !0xFFF;
    let Some(entry_ptr) = ram_l3_entry_ptr(page) else { return; };
    let host_pa = guest_mem::ram_host_pa() + (page as u64 - RAM_IPA_BASE);
    let new = host_pa | PAGE_NORMAL_RO | S2_XN;
    // SAFETY: entry_ptr bounded to one of two 512-entry L3 tables.
    unsafe { entry_ptr.write(new); }
    invalidate_ipa_s2(page);
}

/// Flip the stage-2 L3 entry for the 4 KiB RAM page at `ipa` to
/// `RW + execute-never`. Called by the data-abort handler when the
/// guest writes into a page that was previously frozen as `RO + X`
/// (i.e. Newton's demand-pager is overwriting a code page). The
/// next fetch takes another XN trap so we re-scan the fresh bytes.
///
/// No-op when `ipa` is outside the RAM aperture.
pub unsafe fn set_ram_page_rw_xn(ipa: u32) {
    let page = ipa & !0xFFF;
    let Some(entry_ptr) = ram_l3_entry_ptr(page) else { return; };
    let host_pa = guest_mem::ram_host_pa() + (page as u64 - RAM_IPA_BASE);
    let new = host_pa | PAGE_NORMAL_RW | S2_XN;
    // SAFETY: entry_ptr bounded to one of two 512-entry L3 tables.
    unsafe { entry_ptr.write(new); }
    invalidate_ipa_s2(page);
}

/// Write a contiguous range of stage-2 L2 block descriptors that identity
/// (or non-identity) map `count` × 2 MiB blocks starting at IPA
/// `ipa_base`, all backed by host PA starting at `host_pa_base`, with
/// the given attribute word.
unsafe fn set_l2_blocks(ipa_base: u64, host_pa_base: u64, count: u64, attrs: u64) {
    assert!(ipa_base % TWO_MIB == 0);
    assert!(host_pa_base % TWO_MIB == 0);
    let l2_ptr = addr_of_mut!(S2_L2) as *mut u64;
    for i in 0..count {
        let ipa = ipa_base + i * TWO_MIB;
        let pa = host_pa_base + i * TWO_MIB;
        let index = (ipa / TWO_MIB) as usize;
        // SAFETY: indices kept below 512 by caller's use of this helper.
        unsafe { l2_ptr.add(index).write(pa | attrs); }
    }
}

/// Build stage-2 tables reflecting the Newton memory map, program VTCR_EL2
/// and VTTBR_EL2. Must be called after `guest_mem::load_rom` so the backing
/// stores are ready, and before stage2::enable().
pub unsafe fn init() {
    // All L2 entries start invalid (fault on access).
    let l2_ptr = addr_of_mut!(S2_L2) as *mut u64;
    for i in 0..512usize {
        // SAFETY: 0 ≤ i < 512, table holds 512 entries.
        unsafe { l2_ptr.add(i).write(0); }
    }

    // ROM: 16 MiB read-only at guest PA 0.
    let rom_pa = guest_mem::rom_host_pa();
    // SAFETY: helper writes `count` entries starting at a known index.
    unsafe {
        set_l2_blocks(
            ROM_IPA_BASE,
            rom_pa,
            ROM_IPA_SIZE / TWO_MIB,
            BLOCK_NORMAL_RO,
        );
    }

    // Flash bank 0/1: 4 MiB read-only at guest PA 0x0200_0000 / 0x1000_0000.
    // Einstein's `TMemory::WriteP` silently ignores all direct CPU writes
    // to flash bank addresses (`Emulator/TMemory.cpp:1777` returns
    // without storing); flash is mutated only via the
    // `TEinsteinFlashDriver` native primitives (WriteToFlash16/32Bits,
    // EraseFlash) which call into our `peripherals::flash_driver`,
    // touching the host backing directly without going through stage-2.
    //
    // Mapping the banks RO at stage-2 trips a write-permission fault
    // for any direct CPU store from the guest (e.g. AMD-style
    // command-sequence writes the kernel's flash chip code emits).
    // `trap::handle_data_abort` recognises flash-bank IPAs and silently
    // drops the write (matching Einstein), so the backing keeps the
    // values seeded by `flash::init` / programmed by the native
    // primitives.
    let flash_pa = peripherals::flash::host_pa();
    // SAFETY: helper bounds-checks; flash_pa is 2-MiB aligned.
    unsafe {
        set_l2_blocks(
            FLASH_BANK0_IPA_BASE,
            flash_pa,
            FLASH_BANK_IPA_SIZE / TWO_MIB,
            BLOCK_NORMAL_RO,
        );
        set_l2_blocks(
            FLASH_BANK1_IPA_BASE,
            flash_pa + FLASH_BANK_IPA_SIZE,
            FLASH_BANK_IPA_SIZE / TWO_MIB,
            BLOCK_NORMAL_RO,
        );
    }

    // RAM: 4 MiB at guest PA 0x0400_0000. Refined to 4 KiB L3 pages;
    // each page starts `RW + XN` and flips to `RO + executable` on
    // first fetch, after the shadow-stub scan+patch pass. A subsequent
    // write (Newton's demand-pager overwriting a code page) takes a
    // stage-2 RO permission fault; the handler re-arms the page as
    // `RW + XN`, the write retries, and the next fetch re-scans the
    // fresh bytes. See `set_ram_page_{ro_x,rw_xn}`.
    // SAFETY: installs two L3 tables and points L2[32], L2[33] at them.
    unsafe { install_ram_l3(); }

    // Framebuffer: dumpable RAM for future screen-manager code.
    let fb_pa = guest_mem::fb_host_pa();
    // SAFETY: as above.
    unsafe {
        set_l2_blocks(
            FB_IPA_BASE,
            fb_pa,
            FB_IPA_SIZE / TWO_MIB,
            BLOCK_NORMAL_RW,
        );
    }

    // Refine one 2 MiB L2 slot into 4 KiB pages so we can plant the
    // non-trapping tick register inside the otherwise-MMIO peripheral
    // window. See TICK_PAGE / tick_page::update for the rationale.
    // SAFETY: see the called helper's contract.
    unsafe { install_tick_page(); }

    // Carve out a 64 KiB RW window at IPA 0x0180_0000 to back
    // shadow-stub ScratchVA-variant inline stubs. Stage-2 maps it to
    // `shadow_stub::SCRATCH_POOL`; stage-1 (kernel L1[0x18]) is
    // populated separately by `guest_mem::install_scratch_pool_l1_section`
    // on the first M=0→M=1 transition.
    // SAFETY: helper installs L3 entries and points L2[0xC] at the L3.
    unsafe { install_scratch_pool(); }

    // Under the UDF-trap shadow-byte-access path there are no in-guest
    // stub pools. Byte/halfword-access sites are replaced in place with
    // `UDF #imm16` markers; the UND raises into EL2 and the emulator in
    // shadow_stub::handle_sba_udf performs the access in Rust. No
    // additional stage-2 mappings are required.

    // L1[0] → L2. L1[1..] stay invalid (any IPA ≥ 1 GiB faults).
    let l1_ptr = addr_of_mut!(S2_L1) as *mut u64;
    let l2_phys = addr_of_mut!(S2_L2) as u64;
    // SAFETY: single index write.
    unsafe { l1_ptr.write(l2_phys | DESC_VALID | DESC_TABLE); }

    // Publish the tables and flush any stale translations.
    // SAFETY: MMU maintenance instructions.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "tlbi alle1",
            "tlbi vmalls12e1",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
        core::arch::asm!(
            "msr vtcr_el2, {vtcr}",
            "msr vttbr_el2, {vttbr}",
            "isb",
            vtcr = in(reg) VTCR_EL2_VAL,
            vttbr = in(reg) l1_ptr as u64,
            options(nostack, preserves_flags),
        );
    }

    kprintln!(
        "stage2: ROM @ IPA {:#x}..{:#x} -> host PA {:#x} (RO)",
        ROM_IPA_BASE, ROM_IPA_BASE + ROM_IPA_SIZE, rom_pa
    );
    kprintln!(
        "stage2: RAM @ IPA {:#x}..{:#x} -> host PA {:#x} (per-page RW+XN initially)",
        RAM_IPA_BASE, RAM_IPA_BASE + RAM_IPA_SIZE, guest_mem::ram_host_pa()
    );
    kprintln!(
        "stage2: flash bank 0 @ IPA {:#x}..{:#x} -> host PA {:#x} (RO, {} MiB)",
        FLASH_BANK0_IPA_BASE, FLASH_BANK0_IPA_BASE + FLASH_BANK_IPA_SIZE,
        flash_pa, FLASH_BANK_IPA_SIZE / (1024 * 1024)
    );
    kprintln!(
        "stage2: flash bank 1 @ IPA {:#x}..{:#x} -> host PA {:#x} (RO, {} MiB)",
        FLASH_BANK1_IPA_BASE, FLASH_BANK1_IPA_BASE + FLASH_BANK_IPA_SIZE,
        flash_pa + FLASH_BANK_IPA_SIZE, FLASH_BANK_IPA_SIZE / (1024 * 1024)
    );
    kprintln!(
        "stage2: framebuffer @ IPA {:#x}..{:#x} -> host PA {:#x} (RW, {} MiB)",
        FB_IPA_BASE, FB_IPA_BASE + FB_IPA_SIZE, fb_pa,
        FB_IPA_SIZE / (1024 * 1024)
    );
    kprintln!("stage2: all other IPAs fault to EL2");
}

/// Refine the 4 MiB RAM aperture into 4 KiB pages across two L3 tables.
/// Each page starts `RW + XN`; the shadow-stub flips pages to `RO + X`
/// after scan+patch, and the data-abort handler flips them back on
/// write.
unsafe fn install_ram_l3() {
    let ram_pa = guest_mem::ram_host_pa();
    let n_blocks = (RAM_IPA_SIZE / TWO_MIB) as usize;
    assert!(n_blocks <= 2, "RAM L3 tables assume ≤ 2 × 2 MiB; widen if RAM grows");

    for block in 0..n_blocks {
        let l3_base = match block {
            0 => addr_of_mut!(S2_L3_RAM_0) as *mut u64,
            1 => addr_of_mut!(S2_L3_RAM_1) as *mut u64,
            _ => unreachable!(),
        };
        let block_ipa_base = RAM_IPA_BASE + (block as u64) * TWO_MIB;
        let block_host_base = ram_pa + (block as u64) * TWO_MIB;
        for slot in 0..512usize {
            let host_pa = block_host_base + (slot as u64) * 0x1000;
            // Initial permissions: RW, execute-never.
            let entry = host_pa | PAGE_NORMAL_RW | S2_XN;
            // SAFETY: slot < 512.
            unsafe { l3_base.add(slot).write(entry); }
        }
        // Point the L2 slot at the L3 table.
        let l2_index = (block_ipa_base / TWO_MIB) as usize;
        let l2_ptr = addr_of_mut!(S2_L2) as *mut u64;
        let l3_phys = l3_base as u64;
        // SAFETY: l2_index < 512 (RAM IPAs are within the L2 table).
        unsafe { l2_ptr.add(l2_index).write(l3_phys | DESC_VALID | DESC_TABLE); }
    }
}

/// Wire the 4 KiB page containing K_HDWR_TICKS into stage-2 as a
/// normal-memory RO mapping backed by `TICK_PAGE`. Replaces the single
/// 2 MiB L2 slot covering the peripheral window with a table
/// descriptor pointing at `S2_L3_HW_TICKS`, then installs one valid
/// L3 entry for the tick page. Invalid L3 entries still fault into
/// handle_data_abort → mmio:: like before, so peripherals outside this
/// one page keep their trap-based register model.
unsafe fn install_tick_page() {
    // Seed the page with the "current" ticks() value so any read before
    // the first timer IRQ returns something non-zero-but-consistent.
    // Calendar / alarm offsets stay zero-initialised, which matches the
    // values `vic::read` returns for those registers today.
    tick_page::update();

    // L2 index for the 2 MiB block containing TICK_PAGE_IPA.
    let l2_index = (TICK_PAGE_IPA / TWO_MIB) as usize; // = 0x78 (120) for 0x0F000000
    let l3_ptr = addr_of_mut!(S2_L3_HW_TICKS) as *mut u64;
    let tick_pa = addr_of_mut!(TICK_PAGE) as u64;

    // Clear the L3 table (all invalid).
    for i in 0..512usize {
        // SAFETY: 0 ≤ i < 512.
        unsafe { l3_ptr.add(i).write(0); }
    }
    // L3 slot for the tick page within this L2-covered 2 MiB window.
    let l3_index =
        ((TICK_PAGE_IPA - (l2_index as u64) * TWO_MIB) / 0x1000) as usize;
    // SAFETY: 0 ≤ l3_index < 512.
    unsafe { l3_ptr.add(l3_index).write(tick_pa | PAGE_NORMAL_RO); }

    // Replace the L2 slot with a table descriptor pointing at the L3.
    let l2_ptr = addr_of_mut!(S2_L2) as *mut u64;
    let l3_phys = l3_ptr as u64;
    // SAFETY: l2_index < 512.
    unsafe { l2_ptr.add(l2_index).write(l3_phys | DESC_VALID | DESC_TABLE); }

    kprintln!(
        "stage2: tick page (calendar / alarm / ticks) @ IPA {:#x} -> host PA {:#x} (RO, 4 KiB)",
        TICK_PAGE_IPA, tick_pa
    );
}

/// Wire the 64 KiB shadow-stub scratch carve-out into stage-2 as RW
/// normal-cacheable memory backed by `shadow_stub::SCRATCH_POOL`. The
/// 2 MiB L2 block covering IPA 0x0180_0000..0x01A0_0000 is replaced
/// with a table descriptor pointing at `S2_L3_SCRATCH`; the first 16
/// L3 entries (4 KiB each) point at the host pool. Pages 16..512 stay
/// invalid so any access outside the 64 KiB window stage-2-faults.
unsafe fn install_scratch_pool() {
    let l2_index =
        (crate::shadow_stub::SCRATCH_POOL_IPA as u64 / TWO_MIB) as usize; // 0xC
    let l3_ptr = addr_of_mut!(S2_L3_SCRATCH) as *mut u64;
    let pool_pa = crate::shadow_stub::scratch_pool_host_pa();
    let pool_pages = crate::shadow_stub::SCRATCH_POOL_SIZE / 0x1000; // 16

    // Clear the L3 table (all invalid).
    for i in 0..512usize {
        // SAFETY: 0 ≤ i < 512.
        unsafe { l3_ptr.add(i).write(0); }
    }
    // Map the populated pages of the carve-out.
    let l3_base_ipa = (l2_index as u64) * TWO_MIB;
    let pool_ipa = crate::shadow_stub::SCRATCH_POOL_IPA as u64;
    let l3_index_base = ((pool_ipa - l3_base_ipa) / 0x1000) as usize;
    for i in 0..pool_pages {
        let entry = (pool_pa + (i as u64) * 0x1000) | PAGE_NORMAL_RW;
        // SAFETY: l3_index_base + i < 512 by construction (pool fits).
        unsafe { l3_ptr.add(l3_index_base + i).write(entry); }
    }

    // Replace the L2 slot with a table descriptor pointing at the L3.
    let l2_ptr = addr_of_mut!(S2_L2) as *mut u64;
    let l3_phys = l3_ptr as u64;
    // SAFETY: l2_index < 512.
    unsafe { l2_ptr.add(l2_index).write(l3_phys | DESC_VALID | DESC_TABLE); }

    kprintln!(
        "stage2: shadow-stub scratch pool @ IPA {:#x}..{:#x} -> host PA {:#x} (RW, {} KiB)",
        crate::shadow_stub::SCRATCH_POOL_IPA,
        crate::shadow_stub::SCRATCH_POOL_IPA
            + crate::shadow_stub::SCRATCH_POOL_SIZE as u32,
        pool_pa,
        crate::shadow_stub::SCRATCH_POOL_SIZE / 1024,
    );
}

/// Writer-side helpers for `TICK_PAGE`. Invoked from the CNTHP IRQ
/// handler so the guest's non-trapping reads observe a monotonically
/// advancing tick value in lockstep with EL2 wall-clock heartbeats.
pub mod tick_page {
    use super::*;

    /// Sync-trap path: advance synthetic ticks, poll match crossings,
    /// and republish the non-trapping tick / calendar registers.
    /// Called from `trap_sync_lower_aarch32` after every guest sync
    /// trap. The `tick_advance` here is what makes the tick rate track
    /// guest progress rather than wall clock — see
    /// `vic::SYNTH_TICKS`.
    pub fn update_from_sync_trap() {
        crate::peripherals::vic::tick_advance();
        publish();
    }
    /// Heartbeat path: do NOT advance ticks ourselves (so the heartbeat
    /// can detect "no guest progress" by SYNTH_TICKS being unchanged).
    /// Just poll matches and republish. Forward-progress fast-forward
    /// is handled in `vic::heartbeat_forward_progress`, called from
    /// `timer::on_irq` before this.
    pub fn update_from_heartbeat() {
        publish();
    }
    /// Back-compat shim — older call sites that don't yet distinguish
    /// path. New code should use `update_from_sync_trap` /
    /// `update_from_heartbeat` directly.
    pub fn update() {
        update_from_sync_trap();
    }
    fn publish() {
        crate::peripherals::vic::poll_timer_matches();
        crate::peripherals::vic::poll_alarm();
        let ticks = crate::peripherals::vic::ticks();
        let calendar = crate::peripherals::vic::calendar_seconds();
        // SAFETY: TICK_PAGE is a statically allocated 4 KiB-aligned
        // buffer; writing u32s at fixed offsets is in-bounds.
        unsafe {
            let ptr = addr_of_mut!(TICK_PAGE) as *mut u8;
            let cal_addr = ptr.add(TICK_OFFSET_CALENDAR);
            let ticks_addr = ptr.add(TICK_OFFSET_TICKS);
            core::ptr::write_volatile(ticks_addr as *mut u32, ticks);
            core::ptr::write_volatile(cal_addr as *mut u32, calendar);
            // Clean the cache lines to the Point of Coherency. When
            // the guest boots with stage-1 MMU off (as every
            // guest-test does — see `guest-tests/common/test_runtime.S`),
            // its loads are treated as Device accesses that bypass
            // the cache; a plain `dsb ish` publishes the stores to
            // the inner-shareable domain but leaves them sitting in
            // the hypervisor's cache, invisible to a Device read.
            // `dc cvac` pushes the line to DRAM where the Device
            // read will see it. The two addresses sit in different
            // 64-byte lines (calendar at 0x000, ticks at 0x800) so
            // we clean both.
            core::arch::asm!(
                "dc cvac, {cal}",
                "dc cvac, {ticks}",
                "dsb ish",
                cal = in(reg) cal_addr,
                ticks = in(reg) ticks_addr,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Enable stage-2 translation via HCR_EL2.VM. Takes effect on the next ERET
/// to a lower EL. Call once after init().
pub unsafe fn enable() {
    let mut hcr: u64;
    // SAFETY: EL2 sysreg access.
    unsafe {
        core::arch::asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
    }
    hcr |= 1 << 0;
    // SAFETY: EL2 sysreg write + TLBI.
    unsafe {
        core::arch::asm!(
            "msr hcr_el2, {}",
            "tlbi vmalls12e1",
            "dsb ish",
            "isb",
            in(reg) hcr,
            options(nostack, preserves_flags),
        );
    }

    let vtcr: u64;
    let vttbr: u64;
    // SAFETY: EL2 sysreg reads.
    unsafe {
        core::arch::asm!("mrs {}, vtcr_el2",  out(reg) vtcr,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, vttbr_el2", out(reg) vttbr,
            options(nomem, nostack, preserves_flags));
    }
    kprintln!(
        "stage2: HCR_EL2 = {:#x}  VTCR_EL2 = {:#x}  VTTBR_EL2 = {:#x}",
        hcr, vtcr, vttbr
    );
}
