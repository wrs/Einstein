//! EL2 stage-1 MMU setup for identity-mapping the low N GiB of the host's
//! physical address space.
//!
//! The MMIO and DRAM windows differ between hosts; `init` takes them as
//! parameters (main.rs passes `host::platform`'s constants — arch
//! imports nothing above itself):
//!
//!   raspi3b  — DRAM 0x0..0x3F00_0000, MMIO 0x3F00_0000..0x4000_0000,
//!              BCM2836 local peripheral 0x4000_0000..0x8000_0000.
//!   fvp-base — MMIO 0x0800_0000..0x4000_0000 (covers UART0, GICv3, …),
//!              DRAM 0x8000_0000..0xC000_0000 (image lives here).
//!
//! Layout we install (generic):
//!
//!   L1 table: 512 × 1 GiB entries.
//!     [0]    table descriptor → L2 (covers low 1 GiB of MMIO / RAM)
//!     [1]    optional 1 GiB Device block (set on raspi3b only)
//!     [2]    optional 1 GiB Normal WB block (set on fvp-base for DRAM)
//!     others invalid
//!
//!   L2 table: 512 × 2 MiB block entries.
//!     identity-map PA == VA, Device-nGnRE inside the platform's
//!     `device_mmio` window, Normal WB elsewhere.
//!
//! Attribute encoding via MAIR_EL2:
//!   index 0: Normal inner+outer WB write-allocate cacheable (0xFF)
//!   index 1: Device-nGnRE (0x04)
//!
//! Everything MMU-related lives here so the call site (`init`) is a single
//! sequence: build tables, program sysregs, flush, enable.

use core::ptr::addr_of_mut;

use crate::kprintln;

// --------------- Descriptor layout (VMSAv8-64 short form) ---------------

const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1;          // L0/L1 table descriptor
const DESC_BLOCK: u64 = 0 << 1;          // L1/L2 block descriptor

const LOWER_AF: u64 = 1 << 10;           // access flag
const LOWER_SH_INNER: u64 = 0b11 << 8;   // inner shareable
const LOWER_SH_NONE: u64 = 0b00 << 8;    // non-shareable (for device)
const LOWER_AP_RW_EL2: u64 = 0b00 << 6;  // rw from EL2, no EL0 access
const LOWER_ATTR_IDX_NORMAL: u64 = 0 << 2;
const LOWER_ATTR_IDX_DEVICE: u64 = 1 << 2;

const BLOCK_NORMAL: u64 = DESC_VALID | DESC_BLOCK
    | LOWER_AF | LOWER_SH_INNER | LOWER_AP_RW_EL2 | LOWER_ATTR_IDX_NORMAL;
const BLOCK_DEVICE: u64 = DESC_VALID | DESC_BLOCK
    | LOWER_AF | LOWER_SH_NONE | LOWER_AP_RW_EL2 | LOWER_ATTR_IDX_DEVICE;

// ------------------------------ tables ----------------------------------

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

static mut L1: PageTable = PageTable([0; 512]);
static mut L2: PageTable = PageTable([0; 512]);

const TWO_MIB: u64 = 0x20_0000;
const ONE_GIB: u64 = 0x4000_0000;

// --------------------------- MAIR / TCR values --------------------------

// MAIR_EL2 attribute bytes:
//   attr0 = 0xFF (Normal inner+outer WB write-allocate, non-transient)
//   attr1 = 0x04 (Device-nGnRE)
const MAIR_EL2_VAL: u64 = 0x0000_0000_0000_04FF;

// TCR_EL2 (AArch64, VMSAv8-64):
//   T0SZ = 32 (4 GiB VA)
//   IRGN0 = 0b01 WB write-allocate
//   ORGN0 = 0b01 WB write-allocate
//   SH0   = 0b11 inner shareable
//   TG0   = 0b00 4 KiB granule
//   PS    = 0b010 40-bit physical address (matches MMFR0.PARange=2)
//   TBI   = 0 (use all 64 bits of VA)
//   RES1  = bit 31, bit 23
const TCR_EL2_VAL: u64 = (32 << 0)
    | (0b01 << 8)      // IRGN0
    | (0b01 << 10)     // ORGN0
    | (0b11 << 12)     // SH0
    | (0b00 << 14)     // TG0 = 4 KiB
    | (0b010 << 16)    // PS = 40-bit
    | (1 << 23)        // RES1
    | (1 << 31);       // RES1

// SCTLR_EL2: enable MMU, data cache, instruction cache.
//   Bit 0:  M  = 1  (MMU enable)
//   Bit 2:  C  = 1  (D-cache enable)
//   Bit 12: I  = 1  (I-cache enable)
//   RES1 bits per ARM ARM for SCTLR_EL2 (v8.0): 4, 5, 11, 16, 18, 22, 23, 28, 29
const SCTLR_EL2_M: u64 = 1 << 0;
const SCTLR_EL2_C: u64 = 1 << 2;
const SCTLR_EL2_I: u64 = 1 << 12;
const SCTLR_EL2_RES1: u64 = (1 << 4) | (1 << 5) | (1 << 11)
    | (1 << 16) | (1 << 18) | (1 << 22) | (1 << 23) | (1 << 28) | (1 << 29);

// ------------------------------ init ------------------------------------

/// Build the L1 and L2 identity-map tables and enable the EL2 stage-1 MMU.
///
/// Must be called once from `kmain` on core 0 before any code that relies on
/// caches or virtual addressing. After this returns the MMU is on and the
/// low 1 GiB is identity-mapped: RAM as Normal WB, the platform's MMIO
/// window as Device-nGnRE.
///
/// `device_mmio` is the low-GiB PA window to map Device-nGnRE;
/// `device_mmio_1gib_block` / `dram_1gib_block` optionally install one
/// extra 1 GiB L1 block each (Device / Normal WB) at the named PA.
/// The caller (`main.rs`) supplies `host::platform`'s constants.
pub unsafe fn init(
    device_mmio: core::ops::Range<u64>,
    device_mmio_1gib_block: Option<u64>,
    dram_1gib_block: Option<u64>,
) {
    // Populate L2 (covers PAs 0..1 GiB): identity-map each 2 MiB block as
    // Device-nGnRE inside the platform's MMIO window, Normal WB elsewhere.
    let l2_ptr = addr_of_mut!(L2) as *mut u64;
    for i in 0..512_u64 {
        let pa = i * TWO_MIB;
        let attr = if device_mmio.contains(&pa) {
            BLOCK_DEVICE
        } else {
            BLOCK_NORMAL
        };
        // SAFETY: i < 512 and the array has 512 entries.
        unsafe { l2_ptr.add(i as usize).write(pa | attr); }
    }

    // L1[0] -> L2 as a table descriptor. The other L1 entries are filled in
    // per-platform: raspi3b adds a 1 GiB Device block at 0x4000_0000 for the
    // BCM2836 per-core peripheral; fvp-base adds a 1 GiB Normal WB block at
    // 0x8000_0000 covering the DRAM where the hypervisor image and guest ROM
    // both live. Anything else stays zero so a stray VA above the mapped
    // ranges takes a translation fault at EL2 instead of escaping silently.
    let l1_ptr = addr_of_mut!(L1) as *mut u64;
    let l2_addr = addr_of_mut!(L2) as u64;
    // SAFETY: L1 has 512 entries; we only touch indices 0 and one optional slot per platform.
    unsafe {
        l1_ptr.write(l2_addr | DESC_VALID | DESC_TABLE);
        if let Some(pa) = device_mmio_1gib_block {
            let idx = (pa / ONE_GIB) as usize;
            l1_ptr.add(idx).write(pa | BLOCK_DEVICE);
        }
        if let Some(pa) = dram_1gib_block {
            let idx = (pa / ONE_GIB) as usize;
            l1_ptr.add(idx).write(pa | BLOCK_NORMAL);
        }
    }

    // Publish the tables to the MMU walker before enabling. dsb ish is
    // enough for the stage-1 walker; ic iallu flushes any stale I-cache
    // lines the boot path may have picked up.
    // SAFETY: fixed-encoding system instructions with no memory side effects
    // other than the cache/barrier ordering we're explicitly asking for.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "tlbi alle2",
            "dsb ish",
            "ic iallu",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );

        // Program sysregs in the conventional order: MAIR, TCR, TTBR, then SCTLR.
        core::arch::asm!(
            "msr mair_el2, {mair}",
            "msr tcr_el2, {tcr}",
            "msr ttbr0_el2, {ttbr}",
            "isb",
            mair = in(reg) MAIR_EL2_VAL,
            tcr  = in(reg) TCR_EL2_VAL,
            ttbr = in(reg) l1_ptr as u64,
            options(nostack, preserves_flags),
        );

        // Enable MMU + caches. Read-modify-write SCTLR_EL2 so we don't
        // stomp on any reset-value RES1 bits the implementation sets.
        let mut sctlr: u64;
        core::arch::asm!(
            "mrs {}, sctlr_el2",
            out(reg) sctlr,
            options(nomem, nostack, preserves_flags),
        );
        sctlr |= SCTLR_EL2_M | SCTLR_EL2_C | SCTLR_EL2_I | SCTLR_EL2_RES1;
        core::arch::asm!(
            "msr sctlr_el2, {}",
            "isb",
            in(reg) sctlr,
            options(nostack, preserves_flags),
        );
    }

    kprintln!("MMU: EL2 stage-1 enabled (identity map 0..1 GiB, MMIO as Device)");
}
