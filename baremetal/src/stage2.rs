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

use crate::{guest_mem, kprintln};

// VMSAv8-64 stage-2 descriptor bits
const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1;
const DESC_BLOCK: u64 = 0 << 1;

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

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

static mut S2_L1: PageTable = PageTable([0; 512]);
static mut S2_L2: PageTable = PageTable([0; 512]);

const TWO_MIB: u64 = 0x0020_0000;

// IPA ranges the guest expects. Keep in sync with TMemoryConsts on the
// Einstein side.
pub const ROM_IPA_BASE: u64 = 0x0000_0000;
pub const ROM_IPA_SIZE: u64 = 0x0100_0000; // 16 MiB
pub const RAM_IPA_BASE: u64 = 0x0400_0000;
pub const RAM_IPA_SIZE: u64 = 0x0040_0000; // 4 MiB
// Kernel expects RAM at VA 0x0C000000 after stage-1 MMU is on. Until our
// CP15 shim cleanly enables guest stage-1, mirror the RAM at IPA 0x0C000000
// so guest stage-1-off accesses to that region work against the same bytes.
pub const RAM_MIRROR_IPA_BASE: u64 = 0x0C00_0000;

const VTCR_EL2_VAL: u64 = (32 << 0)
    | (0b01 << 6)          // SL0 = start at level 1
    | (0b01 << 8)          // IRGN0 = WB cacheable
    | (0b01 << 10)         // ORGN0 = WB cacheable
    | (0b11 << 12)         // SH0 = inner shareable
    | (0b00 << 14)         // TG0 = 4 KiB
    | (0b010 << 16);       // PS = 40-bit

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

    // RAM: 4 MiB read-write at guest PA 0x0400_0000.
    let ram_pa = guest_mem::ram_host_pa();
    // SAFETY: as above.
    unsafe {
        set_l2_blocks(
            RAM_IPA_BASE,
            ram_pa,
            RAM_IPA_SIZE / TWO_MIB,
            BLOCK_NORMAL_RW,
        );
        // Mirror of the same 4 MiB at IPA 0x0C00_0000 so the guest's
        // VA=PA accesses to the kernel RAM window work before its
        // own stage-1 MMU comes up. Backing is the SAME bytes.
        set_l2_blocks(
            RAM_MIRROR_IPA_BASE,
            ram_pa,
            RAM_IPA_SIZE / TWO_MIB,
            BLOCK_NORMAL_RW,
        );
    }

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
        "stage2: RAM @ IPA {:#x}..{:#x} -> host PA {:#x} (RW)",
        RAM_IPA_BASE, RAM_IPA_BASE + RAM_IPA_SIZE, ram_pa
    );
    kprintln!(
        "stage2: RAM mirror @ IPA {:#x}..{:#x} -> SAME host PA (RW)",
        RAM_MIRROR_IPA_BASE, RAM_MIRROR_IPA_BASE + RAM_IPA_SIZE
    );
    kprintln!("stage2: all other IPAs fault to EL2");
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
