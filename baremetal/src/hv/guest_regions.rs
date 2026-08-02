//! Single manifest of guest-visible, memory-backed regions.
//!
//! Three subsystems each need to know the Newton guest's physical
//! memory map: `stage2::init` (what to map and with which permissions),
//! `guest_mem::host_addr_for` (IPA → host-pointer dispatch for EL2-side
//! reads/writes), and `snapshot::{save,load}` (which regions to
//! serialize). Before this manifest those three places hand-maintained
//! the list independently, which is exactly how SCRATCH_POOL ended up
//! guest-visible at stage-2 but absent from the snapshot (review finding
//! mem-H2). This table is the single source of truth; the boot-time
//! `cross_check()` makes "mapped in stage-2 but missing from
//! host_addr_for / snapshot" a loud halt rather than a silent omission.
//!
//! What is NOT in this table, and why:
//!   * The tick page (a 4 KiB RO mapping inside the MMIO window) and the
//!     MMIO holes are not memory-backed RAM/ROM regions — they are
//!     peripheral mechanisms with their own trap-based register model.
//!   * Flash's RO mapping plus write-absorb is a genuinely different
//!     mechanism (writes are dropped in `trap`, content is mutated only
//!     via the flash native primitives). The two flash banks ARE listed
//!     here (they are memory-backed and stage-2-mapped), but their host
//!     backing comes from `peripherals::flash`, not `guest_mem`, and
//!     they are snapshotted via the separate `flash_persist` file rather
//!     than the snapshot regions — hence `snapshot: No`.
//!   * The hypervisor-written trampoline / patch-stub code regions are
//!     sub-ranges of the ROM aperture; they are tracked by
//!     `guest_mem::is_hypervisor_code_region`, not here.

use crate::{hv::guest_mem, peripherals, newton::shadow_stub};

/// Stage-2 access permission for a region's mapping.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Stage2Perm {
    /// Read-only normal memory (ROM, flash banks).
    ReadOnly,
    /// Read-write normal memory backed by a flat L2 block mapping (FB).
    ReadWrite,
    /// Read-write normal memory refined to 4 KiB L3 pages. RAM uses the
    /// per-page RW+XN ↔ RO+X state machine; SCRATCH_POOL maps only its
    /// populated pages. The mapping mechanism lives in `stage2`; the
    /// manifest only records that this region is L3-paged, not blocked.
    ReadWritePaged,
}

/// Which host-side backing store a region resolves to. The host base
/// address is the address of a `static mut`, so it can only be obtained
/// at runtime; the manifest stays `const` by carrying this tag and
/// resolving it in [`Region::host_pa`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HostBacking {
    /// `guest_mem::GUEST_ROM`.
    Rom,
    /// `guest_mem::GUEST_RAM`.
    Ram,
    /// `guest_mem::GUEST_FB`.
    Framebuffer,
    /// `shadow_stub::SCRATCH_POOL`.
    ScratchPool,
    /// `peripherals::flash::GUEST_FLASH`, with the given byte offset into
    /// the 8 MiB backing (bank 0 at 0, bank 1 at 4 MiB).
    Flash { offset: u64 },
}

/// One memory-backed guest-physical region.
#[derive(Copy, Clone)]
pub struct Region {
    /// Human-readable name (boot logs, cross-check halts).
    pub name: &'static str,
    /// Guest IPA base.
    pub ipa: u64,
    /// Region size in bytes.
    pub size: u64,
    /// Which host static backs this region.
    pub backing: HostBacking,
    /// Stage-2 permission / mapping shape.
    pub perm: Stage2Perm,
    /// True when the region's bytes are part of a snapshot. Order in the
    /// snapshot file follows the order of `snapshot_regions()`.
    pub snapshot: bool,
    /// True when this region is reachable through
    /// `guest_mem::host_addr_for` (the EL2 IPA → host-pointer layer).
    /// Flash is mapped at stage-2 but its host backing is owned by
    /// `peripherals::flash`, so it is not reachable via host_addr_for.
    pub host_addr_for: bool,
}

impl Region {
    /// Resolve the host physical base of this region's backing store.
    pub fn host_pa(&self) -> u64 {
        match self.backing {
            HostBacking::Rom => guest_mem::rom_host_pa(),
            HostBacking::Ram => guest_mem::ram_host_pa(),
            HostBacking::Framebuffer => guest_mem::fb_host_pa(),
            HostBacking::ScratchPool => shadow_stub::scratch_pool_host_pa(),
            HostBacking::Flash { offset } => peripherals::flash::host_pa() + offset,
        }
    }

    /// True when guest PA `pa` (size `sz`) lies entirely within this
    /// region's IPA window.
    pub fn contains(&self, pa: u64, sz: u64) -> bool {
        pa >= self.ipa && pa.checked_add(sz).is_some_and(|end| end <= self.ipa + self.size)
    }
}

// IPA / size constants for each region. Cross-checked against the
// per-subsystem constants below so a future divergence is a compile
// error rather than a silent drift.
const ROM_IPA: u64 = 0x0000_0000;
const ROM_SZ: u64 = guest_mem::ROM_SIZE as u64; // 16 MiB
const FLASH_BANK0_IPA: u64 = 0x0200_0000;
const FLASH_BANK1_IPA: u64 = 0x1000_0000;
const FLASH_BANK_SZ: u64 = 0x0040_0000; // 4 MiB per bank
const RAM_IPA: u64 = guest_mem::RAM_IPA_BASE as u64; // 0x0400_0000
const RAM_SZ: u64 = guest_mem::RAM_SIZE as u64; // 4 MiB
const SCRATCH_IPA: u64 = shadow_stub::SCRATCH_POOL_IPA as u64; // 0x0600_0000
const SCRATCH_SZ: u64 = shadow_stub::SCRATCH_POOL_SIZE as u64; // 384 KiB
const FB_IPA: u64 = guest_mem::FB_IPA_BASE as u64; // 0x0E00_0000
const FB_SZ: u64 = guest_mem::FB_SIZE as u64; // 2 MiB

/// The manifest. Order matters: the snapshot file serializes the
/// `snapshot: true` entries in the order they appear here. To preserve
/// snapshot VERSION 7's on-disk layout this list keeps RAM, FB,
/// SCRATCH_POOL in that relative order among the snapshotted regions.
pub const REGIONS: &[Region] = &[
    Region {
        name: "ROM",
        ipa: ROM_IPA,
        size: ROM_SZ,
        backing: HostBacking::Rom,
        perm: Stage2Perm::ReadOnly,
        snapshot: false,
        host_addr_for: true,
    },
    Region {
        name: "flash bank 0",
        ipa: FLASH_BANK0_IPA,
        size: FLASH_BANK_SZ,
        backing: HostBacking::Flash { offset: 0 },
        perm: Stage2Perm::ReadOnly,
        snapshot: false,
        host_addr_for: false,
    },
    Region {
        name: "flash bank 1",
        ipa: FLASH_BANK1_IPA,
        size: FLASH_BANK_SZ,
        backing: HostBacking::Flash { offset: FLASH_BANK_SZ },
        perm: Stage2Perm::ReadOnly,
        snapshot: false,
        host_addr_for: false,
    },
    Region {
        name: "RAM",
        ipa: RAM_IPA,
        size: RAM_SZ,
        backing: HostBacking::Ram,
        perm: Stage2Perm::ReadWritePaged,
        snapshot: true,
        host_addr_for: true,
    },
    Region {
        name: "framebuffer",
        ipa: FB_IPA,
        size: FB_SZ,
        backing: HostBacking::Framebuffer,
        perm: Stage2Perm::ReadWrite,
        snapshot: true,
        host_addr_for: true,
    },
    Region {
        name: "scratch pool",
        ipa: SCRATCH_IPA,
        size: SCRATCH_SZ,
        backing: HostBacking::ScratchPool,
        perm: Stage2Perm::ReadWritePaged,
        snapshot: true,
        host_addr_for: true,
    },
];

/// Look up the region containing `[pa, pa+sz)`. Returns `None` when no
/// single region fully contains the access.
pub fn region_for(pa: u64, sz: u64) -> Option<&'static Region> {
    REGIONS.iter().find(|r| r.contains(pa, sz))
}

/// Iterate the snapshotted regions in serialized order (RAM, FB,
/// SCRATCH_POOL). The snapshot save/load loops drive off this so the
/// region set and order have a single definition.
pub fn snapshot_regions() -> impl Iterator<Item = &'static Region> {
    REGIONS.iter().filter(|r| r.snapshot)
}

// ---- Compile-time invariants -------------------------------------------

// Every region must be 4 KiB aligned in both IPA and size (the smallest
// stage-2 granule). Block-mapped regions additionally need 2 MiB
// alignment, checked in stage2's own asserts.
const _: () = {
    let mut i = 0;
    while i < REGIONS.len() {
        let r = REGIONS[i];
        assert!(r.ipa % 0x1000 == 0, "region IPA must be 4 KiB aligned");
        assert!(r.size % 0x1000 == 0, "region size must be 4 KiB aligned");
        i += 1;
    }
};

// Region IPA windows must not overlap (sorted-free check: O(n^2) but n
// is tiny and this runs at compile time).
const _: () = {
    let mut i = 0;
    while i < REGIONS.len() {
        let mut j = i + 1;
        while j < REGIONS.len() {
            let a = REGIONS[i];
            let b = REGIONS[j];
            let disjoint = a.ipa + a.size <= b.ipa || b.ipa + b.size <= a.ipa;
            assert!(disjoint, "guest regions overlap");
            j += 1;
        }
        i += 1;
    }
};
