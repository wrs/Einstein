//! Single manifest of the guest-visible memory layout.
//!
//! Three subsystems each need to know the Newton guest's physical
//! memory map: `stage2::init` (what to map and with which permissions),
//! `guest_mem::host_addr_for` (IPA → host-pointer dispatch for EL2-side
//! reads/writes), and `snapshot::{save,load}` (which regions to
//! serialize). Hand-maintaining that list in three places is how a
//! region ends up guest-visible at stage-2 but absent from the
//! snapshot — the failure the SCRATCH_POOL region hit.
//! This table is the single source of truth; the boot-time
//! [`cross_check`] makes "mapped in stage-2 but missing from
//! host_addr_for / snapshot" a loud halt rather than a silent omission.
//!
//! Beyond the memory-backed [`REGIONS`], the manifest also names:
//!   * [`MMIO_WINDOWS`] — the trap-handled IPA windows and their
//!     policies. The `mmio` router walks this table first-match-wins
//!     and dispatches on each window's [`MmioPolicy`].
//!     Individually-modelled register *addresses* (ROM serial chip,
//!     BankCtrl, RAM-size, …) stay in the module that models them
//!     (`peripherals::asic` for the miscellany) — only windows/ranges
//!     live here.
//!   * [`TICK_PAGE_IPA`] — the one 4 KiB non-trapping page inside the
//!     hardware window (backed by `stage2::TICK_PAGE`).
//!   * The scratch-pool carve-out constants (`SCRATCH_POOL_*`).
//!   * `HYP_CODE_RANGES` — runtime-registered guest-IPA ranges the
//!     hypervisor fills with native-LE code (tracer pool, patch-stub
//!     arena, trampolines), queried via [`is_hyp_code`].
//!
//! Layering: this module imports nothing above `arch` (plus same-layer
//! `guest_mem` for the backing-array sizes). The host base address of
//! each region's backing store is a runtime value owned by an upper
//! layer (`guest_mem` statics, `inline_patch::SCRATCH_POOL`,
//! `peripherals::flash`), so it is resolved through the
//! [`register_backing`] table that `main.rs` wires at boot instead of
//! through direct imports.
//!
//! Flash is split in two disjoint windows on real Newton hardware:
//! bank 0 at `kFlashBank1` (0x02000000) and bank 1 at `kFlashBank2`
//! (0x10000000), each 4 MiB. Einstein keeps both banks back-to-back in
//! a single 8 MiB backing; the manifest surfaces each half at the right
//! guest IPA via `backing_offset`. The banks are stage-2-mapped RO and
//! writes are absorbed in `trap` (content is mutated only via the flash
//! native primitives), and they are persisted via the separate
//! `flash_persist` file rather than the snapshot — hence
//! `snapshot: false, host_addr_for: false`.
//!
//! There is intentionally no IPA 0x0C RAM mirror. Einstein's
//! `TMemoryConsts` and `TMMU.cpp:1186-1193` document the real Newton
//! layout: `kRAMStart = 0x04000000` is the only RAM PA; VA `0x0C000000+`
//! is purely a stage-1 remap to discrete 4 KiB pages in PA
//! `0x04xxxxxx`. A blanket mirror at IPA `0x0C` would alias every
//! pre-MMU 0x0C access to a contiguous RAM window that stage-1 will
//! then remap to a *different* PA, causing pre-MMU writes and post-MMU
//! reads to land in different host cells.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::hv::guest_mem;
use crate::kprintln;

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

/// Which registered host backing store a region resolves to. The host
/// base address is the address of a `static mut` owned by an upper
/// layer, so it can only be obtained at runtime; the manifest stays
/// `const` by carrying this tag and resolving it in [`Region::host_pa`]
/// through the [`register_backing`] table.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegionTag {
    /// `guest_mem::GUEST_ROM`.
    Rom,
    /// `guest_mem::GUEST_RAM`.
    Ram,
    /// `guest_mem::GUEST_FB`.
    Framebuffer,
    /// `inline_patch::SCRATCH_POOL`.
    ScratchPool,
    /// `peripherals::flash::GUEST_FLASH` (8 MiB, both banks).
    Flash,
}
const NUM_REGION_TAGS: usize = 5;

/// Backing resolvers, one per [`RegionTag`], stored as raw fn pointers
/// (`fn() -> u64` returning the host physical base of the backing).
/// 0 = unregistered. Wired by `main.rs` at boot before any region
/// resolution; [`cross_check`] halts if any region's slot is missing.
static BACKING_RESOLVERS: [AtomicUsize; NUM_REGION_TAGS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// Register the host-backing resolver for `tag`. Called from `main.rs`
/// during early boot (before `stage2::init`), once per tag.
pub fn register_backing(tag: RegionTag, resolver: fn() -> u64) {
    BACKING_RESOLVERS[tag as usize].store(resolver as usize, Ordering::Release);
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
    /// Which registered backing store this region resolves through.
    pub tag: RegionTag,
    /// Byte offset into the registered backing (flash bank 1 sits 4 MiB
    /// into the shared 8 MiB flash backing; every other region is 0).
    pub backing_offset: u64,
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
    /// Halts loudly if the region's tag was never registered — a boot
    /// wiring bug, caught by [`cross_check`] before the guest runs.
    pub fn host_pa(&self) -> u64 {
        let raw = BACKING_RESOLVERS[self.tag as usize].load(Ordering::Acquire);
        if raw == 0 {
            kprintln!(
                "*** layout: region {} has no registered backing ({:?}) — \
                 main.rs must register_backing() before use ***",
                self.name,
                self.tag
            );
            crate::arch::cpu::halt();
        }
        // SAFETY: the only writer is register_backing, which stores a
        // valid `fn() -> u64`; 0 is filtered above.
        let f: fn() -> u64 = unsafe { core::mem::transmute(raw) };
        f() + self.backing_offset
    }

    /// True when guest PA `pa` (size `sz`) lies entirely within this
    /// region's IPA window.
    pub fn contains(&self, pa: u64, sz: u64) -> bool {
        pa >= self.ipa
            && pa
                .checked_add(sz)
                .is_some_and(|end| end <= self.ipa + self.size)
    }
}

// IPA / size constants for each region. Sizes come from the backing
// arrays in `guest_mem` so a size change there propagates here (and
// the const asserts below re-check alignment).
const ROM_IPA: u64 = 0x0000_0000;
const ROM_SZ: u64 = guest_mem::ROM_SIZE as u64; // 16 MiB
const FLASH_BANK0_IPA: u64 = 0x0200_0000;
const FLASH_BANK1_IPA: u64 = 0x1000_0000;
const FLASH_BANK_SZ: u64 = 0x0040_0000; // 4 MiB per bank
const RAM_IPA: u64 = guest_mem::RAM_IPA_BASE as u64; // 0x0400_0000
const RAM_SZ: u64 = guest_mem::RAM_SIZE as u64; // 4 MiB
const FB_IPA: u64 = guest_mem::FB_IPA_BASE as u64; // 0x0E00_0000
const FB_SZ: u64 = guest_mem::FB_SIZE as u64; // 2 MiB

// =======================================================================
// Scratch pool carve-out
// =======================================================================
//
// 384 KiB carve-out at IPA == VA == 0x0600_0000. Identity-mapped so:
//   * Newton boot (kernel stage-1 on): kernel L1[VA>>20] = section
//     descriptor identity-mapping VA→IPA. Stage-2 maps IPA →
//     `inline_patch::SCRATCH_POOL`.
//   * Guest-test mode (kernel stage-1 off): stage-1 is bypassed; the
//     CPU emits VA as IPA directly. Stage-2 sees IPA == VA and maps
//     to SCRATCH_POOL.
//
// Identity mapping keeps per-slot literals usable from both regimes
// without two separate stage-2 mappings.
//
// L1[0x60] sits in a free gap of the kernel's L1 census (slots
// 0x52..0xBF are unused) and is also free in the existing stage-2
// layout (between RAM at 0x0440_0000 and the framebuffer at
// 0x0E00_0000).
pub const SCRATCH_POOL_VA: u32 = 0x0600_0000;
pub const SCRATCH_POOL_IPA: u32 = 0x0600_0000;
pub const SCRATCH_POOL_SIZE: usize = 384 * 1024; // 96 × 4 KiB pages

/// Einstein's `kHighROMEnd`: the MP2x00 ROM bus aperture is IPA
/// 0..16 MiB; writes below this are absorbed (mask ROM ignores them on
/// real hardware). This is a *hardware* aperture constant — the same
/// for any ROM image on MP2x00 silicon — which is why it lives in the
/// layout manifest and not in `newton::rom_ver`. The per-version
/// `rom_ver::ROM_CODE_END` is the semantically distinct "where this
/// ROM build's own code addresses end" bound (numerically equal for
/// 717006, but a version fact rather than an address-map fact).
const HIGH_ROM_END: u64 = 0x0100_0000;

/// Upper bound of the ROM write-absorb aperture (`kHighROMEnd`).
pub const fn high_rom_end() -> u64 {
    HIGH_ROM_END
}

/// The guest RAM IPA window.
pub const fn ram_range() -> core::ops::Range<u64> {
    RAM_IPA..RAM_IPA + RAM_SZ
}

/// The guest ROM aperture IPA window.
pub const fn rom_range() -> core::ops::Range<u64> {
    ROM_IPA..ROM_IPA + ROM_SZ
}

/// The manifest. Order matters: the snapshot file serializes the
/// `snapshot: true` entries in the order they appear here. To preserve
/// snapshot VERSION 7's on-disk layout this list keeps RAM, FB,
/// SCRATCH_POOL in that relative order among the snapshotted regions.
pub const REGIONS: &[Region] = &[
    Region {
        name: "ROM",
        ipa: ROM_IPA,
        size: ROM_SZ,
        tag: RegionTag::Rom,
        backing_offset: 0,
        perm: Stage2Perm::ReadOnly,
        snapshot: false,
        host_addr_for: true,
    },
    Region {
        name: "flash bank 0",
        ipa: FLASH_BANK0_IPA,
        size: FLASH_BANK_SZ,
        tag: RegionTag::Flash,
        backing_offset: 0,
        perm: Stage2Perm::ReadOnly,
        snapshot: false,
        host_addr_for: false,
    },
    Region {
        name: "flash bank 1",
        ipa: FLASH_BANK1_IPA,
        size: FLASH_BANK_SZ,
        tag: RegionTag::Flash,
        backing_offset: FLASH_BANK_SZ,
        perm: Stage2Perm::ReadOnly,
        snapshot: false,
        host_addr_for: false,
    },
    Region {
        name: "RAM",
        ipa: RAM_IPA,
        size: RAM_SZ,
        tag: RegionTag::Ram,
        backing_offset: 0,
        perm: Stage2Perm::ReadWritePaged,
        snapshot: true,
        host_addr_for: true,
    },
    Region {
        name: "framebuffer",
        ipa: FB_IPA,
        size: FB_SZ,
        tag: RegionTag::Framebuffer,
        backing_offset: 0,
        perm: Stage2Perm::ReadWrite,
        snapshot: true,
        host_addr_for: true,
    },
    Region {
        name: "scratch pool",
        ipa: SCRATCH_POOL_IPA as u64,
        size: SCRATCH_POOL_SIZE as u64,
        tag: RegionTag::ScratchPool,
        backing_offset: 0,
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

// =======================================================================
// MMIO windows
// =======================================================================

/// The peripheral models the MMIO router dispatches to. Closed enum:
/// a new model must add a variant, so Phase 4's window-driven dispatch
/// can't silently miss one.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PeriphId {
    Vic,
    Dma,
    Pcmcia,
    Serial,
    /// Memory-controller / bank-config / bus-strap miscellany
    /// (`peripherals::asic`).
    Asic,
}

/// What the router does with an access inside a window.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MmioPolicy {
    /// Routed to the named peripheral model.
    Peripheral(PeriphId),
    /// Reads return 0, writes are silently dropped (Einstein's
    /// "unknown bank" default for windows known to behave that way).
    ReadZeroDropWrite,
    /// Any access not claimed by a finer window or a modelled register
    /// halts loudly (Phase A trip-wire).
    HaltUnknown,
}

/// One trap-handled IPA window.
#[derive(Copy, Clone)]
pub struct MmioWindow {
    pub name: &'static str,
    pub base: u64,
    pub end: u64,
    pub policy: MmioPolicy,
}

impl MmioWindow {
    pub const fn contains(&self, ipa: u64) -> bool {
        ipa >= self.base && ipa < self.end
    }
}

/// Base IPA of the 4 KiB page holding the Newton tick cluster
/// (calendar at +0x000, alarm at +0x400, K_HDWR_TICKS at +0x800).
/// Stage-2 backs this page with a real RO mapping (`stage2::TICK_PAGE`)
/// so hot tick reads don't trap; writes still fault into the VIC model,
/// and `mmio::write`'s catch-net guards against spliced sub-word writes
/// landing in the page.
pub const TICK_PAGE_IPA: u64 = 0x0F18_1000;

/// The Newton hardware register window. Any unknown access inside it
/// means "add the register to a peripheral model"; outside it, "decide
/// whether to model or widen stage-2".
pub const HW_WINDOW: MmioWindow = MmioWindow {
    name: "Newton hardware window",
    base: 0x0F00_0000,
    end: 0x0F40_0000,
    policy: MmioPolicy::HaltUnknown,
};

// MP2x00 RAM-bank probe window. BootOS probes 0x04000000 (present,
// 4 MiB — we map it) and 0x08000000 (absent — the "we have 4 MiB not
// 8 MiB" path). The probe does a signature write/read at `base +
// 0x200000`; if the read doesn't match the signature, the bank is
// declared absent. We model the second bank as "no memory": writes
// are dropped deterministically, reads return 0. That gives the
// probe a clean "absent" signal without a silent ignored write.
pub const RAM_PROBE_ABSENT: MmioWindow = MmioWindow {
    name: "RAM probe (absent bank)",
    base: 0x0800_0000,
    end: 0x0900_0000,
    policy: MmioPolicy::ReadZeroDropWrite,
};

// "No extra ROM / REx / flash" probe window. The Newton kernel's
// TestForREx (rom 0x3137dc) and related probes scan fixed addresses
// past the mapped flash-bank-2 window (0x10400000 upward) looking
// for RExBlock magic at fixed offsets. We explicitly model these as
// absent so reads return 0 and the probe's magic-compare fails
// cleanly. PCMCIA (0x30000000+) is handled separately.
pub const NO_REX_PROBE: MmioWindow = MmioWindow {
    name: "REx/flash probe (absent)",
    base: 0x1040_0000,
    end: 0x2000_0000,
    policy: MmioPolicy::ReadZeroDropWrite,
};

// "Unknown bank #5" silent-zero window — the gap between Newton MP2x00's
// kFlashBank2End (0x1040_0000) and kPCMCIA0Base (0x3000_0000). Einstein's
// `TMemory::ReadP` (Emulator/TMemory.cpp:1026-1034) returns 0 silently
// for any read in this range and absorbs writes. The 717006 kernel hits
// this on a TInterpreter-side `MakeString__FPCc` whose to-Unicode
// translator descriptor's `+16` slot (the per-encoding lookup table
// base) is 0x2000_0110 — a bogus pointer the kernel computed from
// uninitialised / partially-installed encoding state. Einstein tolerates
// it via this silent-zero path (the convert function reads 0 → emits
// U+0000 → boot continues with garbled string output instead of a hard
// fault). Match that behaviour here so the trip-wire isn't load-bearing
// past the modelled-MMIO window. The deeper "why is the descriptor
// wrong" question is decoupled from this wedge: it's a NewtonScript-
// level bug Einstein masks the same way.
pub const UNKNOWN_BANK5: MmioWindow = MmioWindow {
    name: "unknown bank #5",
    base: 0x2000_0000,
    end: 0x3000_0000,
    policy: MmioPolicy::ReadZeroDropWrite,
};

// BIO interface register bank. `TBIOInterface::BIOReadRegister` /
// `BIOWriteCommand` / etc. at ROM `0x26b878..0x26ba10` compute the
// target register address as `0x0F05_0000 + (bank_index << 10)`, so
// the 32 registers live at `0x0F05_0000`, `0x0F05_0400`, …,
// `0x0F05_7C00`. The early-boot kernel iterates over several banks
// (14, 15, 16, 17, 18, 19, 20, …) during BIO init; Einstein's TMemory
// doesn't model these registers — the "unknown bank #3" fallback
// accepts writes silently and returns 0 for reads (TMemory.cpp:952-959).
// Rather than whack-a-mole each register as the iterator advances,
// accept the whole known-stride range in one explicit entry. The
// 0x400 stride check lives in `peripherals::asic::in_bio_bank`, which
// is why the policy is `Peripheral(Asic)` rather than
// `ReadZeroDropWrite`: this is still a closed whitelist — addresses
// off the stride inside the window continue to halt loudly.
pub const BIO_BANKS: MmioWindow = MmioWindow {
    name: "BIO register banks",
    base: 0x0F05_0000,
    end: 0x0F05_8000,
    policy: MmioPolicy::Peripheral(PeriphId::Asic),
};

/// The trap-handled IPA windows, walked first-match-wins by the
/// `hv::mmio` router — finer windows precede the `HW_WINDOW`
/// catch-all. Each `Peripheral` window routes to the model named by
/// its [`PeriphId`]; the model owns the register decode inside the
/// window and halts loudly on unmodelled addresses.
pub const MMIO_WINDOWS: &[MmioWindow] = &[
    MmioWindow {
        name: "VIC clocks",
        base: 0x0F11_0000,
        end: 0x0F11_1800,
        policy: MmioPolicy::Peripheral(PeriphId::Vic),
    },
    MmioWindow {
        name: "VIC/RTC/GPIO",
        base: 0x0F18_0000,
        end: 0x0F19_0000,
        policy: MmioPolicy::Peripheral(PeriphId::Vic),
    },
    MmioWindow {
        name: "DMA",
        base: 0x0F08_0000,
        end: 0x0F09_9000,
        policy: MmioPolicy::Peripheral(PeriphId::Dma),
    },
    MmioWindow {
        name: "serial",
        base: 0x0F1C_0000,
        end: 0x0F20_0000,
        policy: MmioPolicy::Peripheral(PeriphId::Serial),
    },
    MmioWindow {
        name: "PCMCIA",
        base: 0x3000_0000,
        end: 0x7000_0000,
        policy: MmioPolicy::Peripheral(PeriphId::Pcmcia),
    },
    // ASIC / memory-controller register clusters (`peripherals::asic`):
    // the bank-config area (incl. kHdWr_PlatformVers at 0x0F00_0008 and
    // the RAM-size registers), two mid-range config registers, the
    // external-abort / bank-control / chip-rev / ROM-serial cluster,
    // and the write-only bus / pin-strap block.
    MmioWindow {
        name: "ASIC bank config",
        base: 0x0F00_0000,
        end: 0x0F00_2400,
        policy: MmioPolicy::Peripheral(PeriphId::Asic),
    },
    MmioWindow {
        name: "ASIC mem config",
        base: 0x0F04_3000,
        end: 0x0F04_8400,
        policy: MmioPolicy::Peripheral(PeriphId::Asic),
    },
    MmioWindow {
        name: "ASIC abort/bank/chip-rev",
        base: 0x0F24_0000,
        end: 0x0F24_7400,
        policy: MmioPolicy::Peripheral(PeriphId::Asic),
    },
    MmioWindow {
        name: "ASIC bus straps",
        base: 0x0F28_0000,
        end: 0x0F28_4400,
        policy: MmioPolicy::Peripheral(PeriphId::Asic),
    },
    BIO_BANKS,
    RAM_PROBE_ABSENT,
    NO_REX_PROBE,
    UNKNOWN_BANK5,
    HW_WINDOW,
];

// =======================================================================
// Hypervisor-written code ranges
// =======================================================================

const MAX_HYP_CODE_RANGES: usize = 8;

#[derive(Copy, Clone)]
struct HypCodeRange {
    name: &'static str,
    start: u32,
    end: u32,
}

static mut HYP_CODE_RANGES: [HypCodeRange; MAX_HYP_CODE_RANGES] = [HypCodeRange {
    name: "",
    start: 0,
    end: 0,
}; MAX_HYP_CODE_RANGES];
static HYP_CODE_RANGE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Register a guest-IPA range the hypervisor populates at runtime with
/// native (little-endian) AArch32 instruction words rather than
/// guest-authored data. Called from the Newton install paths (tracer
/// pool, patch-stub arena, trampolines) during boot, before the guest
/// runs. The registered set feeds [`is_hyp_code`], which is shared by:
///   * `guest_endian::pa_is_rom_code` — these words must NOT be
///     byte-swapped on a guest read (they're already host-LE code), and
///   * `snapshot`'s autosave gate — a guest PC parked in one of these
///     regions is mid-trampoline and must not anchor an autosave (the
///     EL2-side code at that IPA is rebuilt every boot).
pub fn register_hyp_code_range(name: &'static str, start: u32, end: u32) {
    let n = HYP_CODE_RANGE_COUNT.load(Ordering::Relaxed);
    if n >= MAX_HYP_CODE_RANGES {
        kprintln!(
            "*** layout: hyp-code-range table full registering {} ({:#x}..{:#x}) — \
             raise MAX_HYP_CODE_RANGES ***",
            name,
            start,
            end
        );
        crate::arch::cpu::halt();
    }
    // SAFETY: single-core boot-time registration; the Release store on
    // the count publishes the entry before any reader can index it.
    unsafe {
        (*core::ptr::addr_of_mut!(HYP_CODE_RANGES))[n] = HypCodeRange { name, start, end };
    }
    HYP_CODE_RANGE_COUNT.store(n + 1, Ordering::Release);
}

/// True if `pa` lies in a registered hypervisor-written code range.
/// See [`register_hyp_code_range`] for the consumers and semantics.
pub fn is_hyp_code(pa: u32) -> bool {
    let n = HYP_CODE_RANGE_COUNT.load(Ordering::Acquire);
    // SAFETY: entries below the Acquire-loaded count are fully written
    // (Release-published by register_hyp_code_range).
    let ranges = unsafe { &(*core::ptr::addr_of!(HYP_CODE_RANGES)) };
    ranges[..n].iter().any(|r| pa >= r.start && pa < r.end)
}

// =======================================================================
// Boot-time cross-check
// =======================================================================

/// Layout-level boot-time consistency check, called from `stage2::init`
/// (after `main.rs` has registered the backings and the Newton loader
/// has registered its hyp-code ranges). Halts loudly on any violation
/// rather than letting a misconfigured manifest boot into
/// hard-to-diagnose corruption. Checks the invariants that involve the
/// runtime-registered tables (the purely-const invariants are enforced
/// by the `const _` blocks below):
///   * every region resolves to a registered, non-null backing,
///   * no MMIO window overlaps a memory-backed region,
///   * the tick page lies inside a `Peripheral` window, and
///   * every hyp-code range lies inside the ROM aperture.
pub fn cross_check() {
    for r in REGIONS {
        // host_pa() itself halts on an unregistered tag.
        if r.host_pa() == 0 {
            kprintln!("*** layout: region {} backing resolves to null ***", r.name);
            crate::arch::cpu::halt();
        }
    }
    for w in MMIO_WINDOWS {
        for r in REGIONS {
            if w.base < r.ipa + r.size && r.ipa < w.end {
                kprintln!(
                    "*** layout: MMIO window {} ({:#x}..{:#x}) overlaps region {} ({:#x}..{:#x}) ***",
                    w.name, w.base, w.end, r.name, r.ipa, r.ipa + r.size
                );
                crate::arch::cpu::halt();
            }
        }
    }
    let tick_in_periph = MMIO_WINDOWS.iter().any(|w| {
        matches!(w.policy, MmioPolicy::Peripheral(_))
            && w.contains(TICK_PAGE_IPA)
            && w.contains(TICK_PAGE_IPA + 0xFFF)
    });
    if !tick_in_periph {
        kprintln!(
            "*** layout: tick page {:#x} not inside any Peripheral MMIO window ***",
            TICK_PAGE_IPA
        );
        crate::arch::cpu::halt();
    }
    let n = HYP_CODE_RANGE_COUNT.load(Ordering::Acquire);
    // SAFETY: entries below the count are published (see is_hyp_code).
    let ranges = unsafe { &(*core::ptr::addr_of!(HYP_CODE_RANGES)) };
    for r in &ranges[..n] {
        if !(rom_range().contains(&(r.start as u64)) && r.end as u64 <= rom_range().end) {
            kprintln!(
                "*** layout: hyp code range {} ({:#x}..{:#x}) outside the ROM aperture ***",
                r.name,
                r.start,
                r.end
            );
            crate::arch::cpu::halt();
        }
    }
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

// The tick page must sit inside the hardware window, and the scratch
// pool identity-map assumption (VA == IPA) must hold.
const _: () = {
    assert!(
        HW_WINDOW.contains(TICK_PAGE_IPA),
        "tick page outside HW window"
    );
    assert!(
        SCRATCH_POOL_VA == SCRATCH_POOL_IPA,
        "scratch pool must be identity-mapped"
    );
};
