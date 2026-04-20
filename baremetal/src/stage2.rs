//! Stage-2 MMU: IPA → host PA translation for the guest.
//!
//! For M1.5b the stage-2 layout is intentionally tiny:
//!
//!   IPA 0x0000_0000 .. 0x4000_0000 (1 GiB): identity-map as Normal WB,
//!     except for one 4 KiB page that we deliberately mark "no access".
//!     Any guest load/store to that page generates a stage-2 data abort
//!     and traps to the EL2 vector table (offset 0x600 when the guest
//!     is in AArch32).
//!
//! Descriptor format: VMSAv8-64 stage-2, 4 KiB granule.
//!   L1 table: 512 × 1 GiB entries.
//!     [0]       -> table descriptor pointing at L2
//!     [1..=511] -> 0 (stage-2 fault, translates to a data abort)
//!   L2 table: 512 × 2 MiB entries.
//!     Normal-case block descriptor, or a table descriptor pointing to an
//!     L3 table if we need page-level granularity (we do for the trap page).
//!   L3 table: 512 × 4 KiB pages.
//!     Used only for the 2 MiB region containing our trap page. One entry
//!     (the trap page) is invalid; the rest are identity-mapped pages.
//!
//! Stage-2 attribute/permission encoding differs from stage-1 — there's no
//! AP bit; instead S2AP and MemAttr live in the lower block attributes, and
//! access rights come from HCR_EL2.CD/ID and S2AP directly.

use core::ptr::addr_of_mut;

use crate::kprintln;

// VMSAv8-64 stage-2 descriptor bits
const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1;
const DESC_BLOCK: u64 = 0 << 1; // explicit clarity
const DESC_PAGE: u64 = 1 << 1;  // at L3 a page descriptor

const S2_MEMATTR_NORMAL_WB: u64 = 0b1111 << 2;  // Normal WB cacheable, inner+outer
const S2_MEMATTR_DEVICE_NGNRE: u64 = 0b0001 << 2;
const S2_AP_READ: u64 = 0b01 << 6;
const S2_AP_WRITE: u64 = 0b10 << 6;
const S2_AP_RW: u64 = S2_AP_READ | S2_AP_WRITE;
const S2_SH_INNER: u64 = 0b11 << 8;
const S2_SH_NONE: u64 = 0b00 << 8;
const S2_AF: u64 = 1 << 10;

const BLOCK_NORMAL_RW: u64 = DESC_VALID | DESC_BLOCK
    | S2_MEMATTR_NORMAL_WB | S2_AP_RW | S2_SH_INNER | S2_AF;
const PAGE_NORMAL_RW: u64 = DESC_VALID | DESC_PAGE
    | S2_MEMATTR_NORMAL_WB | S2_AP_RW | S2_SH_INNER | S2_AF;

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

static mut S2_L1: PageTable = PageTable([0; 512]);
static mut S2_L2: PageTable = PageTable([0; 512]);
// One L3 used to punch a 4 KiB hole inside an otherwise-blocked 2 MiB region.
static mut S2_L3_TRAP: PageTable = PageTable([0; 512]);

const TWO_MIB: u64 = 0x20_0000;
const FOUR_KIB: u64 = 0x1000;

/// IPA address of the 4 KiB page we deliberately leave unmapped at stage-2
/// so the toy guest's load generates a data abort.
pub const TRAP_IPA: u64 = 0x0010_0000; // inside the first 2 MiB, low enough
                                       // for a 32-bit immediate in the guest.

/// VTCR_EL2 (stage-2 translation control) for Cortex-A53:
///   T0SZ = 32 (40-bit IPA but we clip to 32 for guest)
///   actually PS=010 (40-bit) and T0SZ=32 gives 4 GiB IPA space starting level 1.
///   SL0 = 01 (start at level 1)
///   IRGN0=ORGN0 = 0b01 WB WA cacheable
///   SH0 = 0b11 inner shareable
///   TG0 = 0b00 4 KiB granule
///   PS  = 0b010 40-bit
///   VS  = 0 (8-bit VMID, matches ID_AA64MMFR1.VMIDBits=0)
const VTCR_EL2_VAL: u64 = (32 << 0)      // T0SZ
    | (0b01 << 6)                        // SL0 = start at level 1
    | (0b01 << 8)                        // IRGN0
    | (0b01 << 10)                       // ORGN0
    | (0b11 << 12)                       // SH0
    | (0b00 << 14)                       // TG0 = 4 KiB
    | (0b010 << 16);                     // PS = 40-bit

/// Build stage-2 tables and program VTCR_EL2 / VTTBR_EL2. Must be called
/// before HCR_EL2.VM=1 is set (which is the caller's responsibility).
pub unsafe fn init() {
    // L3 table covers the 2 MiB window [TRAP_IPA & ~0x1FFFFF,
    // (TRAP_IPA & ~0x1FFFFF) + 2 MiB). Punch a hole at the trap page.
    let region_base = TRAP_IPA & !(TWO_MIB - 1);
    let hole_index = ((TRAP_IPA - region_base) / FOUR_KIB) as usize;
    let l3_ptr = addr_of_mut!(S2_L3_TRAP) as *mut u64;
    for i in 0..512usize {
        if i == hole_index {
            // SAFETY: writing to a fixed-size static table, i < 512.
            unsafe { l3_ptr.add(i).write(0); } // invalid → stage-2 fault
        } else {
            let pa = region_base + (i as u64) * FOUR_KIB;
            // SAFETY: writing to a fixed-size static table, i < 512.
            unsafe { l3_ptr.add(i).write(pa | PAGE_NORMAL_RW); }
        }
    }

    // L2 table: 2 MiB blocks identity everywhere, except the window that
    // contains the trap page, which becomes a table descriptor pointing at L3.
    let l2_ptr = addr_of_mut!(S2_L2) as *mut u64;
    let l3_phys = addr_of_mut!(S2_L3_TRAP) as u64;
    let trap_block_index = (region_base / TWO_MIB) as usize;
    for i in 0..512usize {
        let pa = (i as u64) * TWO_MIB;
        let desc = if i == trap_block_index {
            l3_phys | DESC_VALID | DESC_TABLE
        } else {
            pa | BLOCK_NORMAL_RW
        };
        // SAFETY: writing to a fixed-size static table, i < 512.
        unsafe { l2_ptr.add(i).write(desc); }
    }

    // L1[0] → L2. Everything above 1 GiB IPA is fault.
    let l1_ptr = addr_of_mut!(S2_L1) as *mut u64;
    let l2_phys = addr_of_mut!(S2_L2) as u64;
    // SAFETY: writing to a fixed-size static table, index 0.
    unsafe { l1_ptr.write(l2_phys | DESC_VALID | DESC_TABLE); }

    // Publish tables. Stage-2 walks observe the same cache/barrier rules as
    // stage-1; ish is sufficient for the PE we're on.
    // SAFETY: fixed-encoding system instructions with no side effects beyond
    // the cache/TLB maintenance we're explicitly asking for.
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
        "Stage-2: identity map 0..1 GiB; trap hole punched at IPA {:#x}",
        TRAP_IPA
    );
}

/// Enable stage-2 translation via HCR_EL2.VM. Takes effect on the next ERET
/// to a lower EL (or any VA resolution done there). Caller must already
/// have programmed HCR_EL2 bits relevant to their guest (RW etc.).
pub unsafe fn enable() {
    let mut hcr: u64;
    // SAFETY: reading HCR_EL2 at EL2.
    unsafe {
        core::arch::asm!("mrs {}, hcr_el2", out(reg) hcr,
            options(nomem, nostack, preserves_flags));
    }
    hcr |= 1 << 0; // VM: stage-2 enabled for guest accesses
    // SAFETY: writing HCR_EL2 at EL2 + re-invalidating stage-2 TLB so
    // any entries cached before VM=1 won't hide the new mapping.
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
    // SAFETY: read-only sysreg access.
    unsafe {
        core::arch::asm!("mrs {}, vtcr_el2",  out(reg) vtcr,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, vttbr_el2", out(reg) vttbr,
            options(nomem, nostack, preserves_flags));
    }
    kprintln!(
        "HCR_EL2 = {:#018x}  VTCR_EL2 = {:#018x}  VTTBR_EL2 = {:#018x}",
        hcr, vtcr, vttbr
    );
    kprintln!("Stage-2 active for the guest (HCR.VM = 1).");
}
