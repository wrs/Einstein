//! ROM-version skeleton for the 710031 ROM (MP2000, German) — the
//! seam proof for the per-version contract. Only the Tier-1 constants
//! carry values; every probe / patch / diagnostics group is `None` or
//! empty, which selects the graceful-degradation paths everywhere:
//! no ROM patches installed, DABTs stay on the slow HVC-only path,
//! FPA UNDs halt loudly, task/heap diagnostics print "unavailable".
//!
//! ALL VALUES BELOW ARE UNVERIFIED PLACEHOLDERS. The image-size /
//! REx-placement / ROM-tail numbers copy the 717006 layout (plausible
//! for any 8 MiB MP2x00 ROM, and the tail placement is a hypervisor
//! budget, not a ROM fact — it only needs the REx tail to end below
//! `dabt_fast_tramp`). Booting this version additionally needs
//! `roms/710031/{newton.rom, Einstein.rex}` plus a regenerated
//! classifier bitmap; without them build.rs stages zero-length
//! placeholders and the loader halts at boot.

use super::common_2x;
use super::types::*;
use crate::hv::hooks::UndDiagHints;

pub const NAME: &str = "710031 (MP2000 D) [UNVERIFIED skeleton]";

/// UNVERIFIED: assumed 8 MiB like every MP2x00 main ROM.
pub const ROM_IMAGE_SIZE: usize = 0x0080_0000;

/// UNVERIFIED: assumed identical aperture split to 717006.
pub const ROM_CODE_END: u32 = 0x0100_0000;

/// UNVERIFIED: assumed one embedded REx like 717006.
pub const REX: RexInfo = RexInfo {
    pa_offset: 0x0080_0000,
    num_embedded_rexes: 1,
};

/// Hypervisor placement budget — copied from 717006; revisit once the
/// 710031 REx tail extent is known.
pub const ROM_TAIL: RomTailLayout = RomTailLayout {
    dabt_fast_tramp:       0x008F_FF00,
    tracer_pool_base:      0x0090_0000,
    tracer_pool_end:       0x00E0_0000,
    stub_pool_base:        0x00E0_0000,
    stub_pool_end:         0x00FF_FF00,
    patch_stub_arena_base: 0x00FF_FD80,
    patch_stub_arena_end:  0x00FF_FEC0,
    fpa_bypass_stub:       0x00FF_FEC0,
    und_tramp:             0x00FF_FF00,
    dabt_tramp:            0x00FF_FFA8,
    und_return_stub:       0x00FF_FFE4,
    stubs_end:             0x0100_0000,
};

// No code addresses are known for this ROM yet: no patches, no probe
// sites, no kernel globals, no fault-handler fast paths.
pub const PATCHES: &[RomPatch] = &[];
pub const NS_TRACE_PATCH: Option<RomPatch> = None;
pub const BOOT: Option<BootSites> = None;
pub const LOUD_HALT: Option<LoudHaltSites> = None;
pub const HAMMER: Option<HammerSites> = None;
pub const UNHANDLED: Option<UnhandledExceptionSites> = None;
pub const REMEMBER_SWIRET: Option<ProbeSite> = None;
pub const DAH_MRS_SPSR: Option<ProbeSite> = None;
pub const DEBUG_UND_SLOTS: Option<DebugUndSlots> = None;
pub const REAL_CLOCK_SECONDS: Option<RealClockSite> = None;
pub const FTIME_IN_SECONDS: Option<InjectionSite> = None;
pub const FDATE_FROM_SECONDS: Option<InjectionSite> = None;
pub const INSN_AS_DATA_LDRS: &[InsnAsDataLdr] = &[];
pub const FPE_LDRS: Option<FpeLdrSites> = None;
pub const STORE_PROBES: Option<StoreProbeSites> = None;
pub const NOTIFY_PROBES: Option<NotifySites> = None;
pub const DATA_ABORT_HANDLER_VA: Option<u32> = None;
pub const FPE_JT_VA: Option<u32> = None;
pub const KERNEL_GLOBALS: Option<KernelGlobals> = None;
pub const WEDGE_DIAG: Option<UndDiagHints> = None;
pub const BP_REARM_QUIET_IPA: Option<u32> = None;

// NewtonOS-2.x-shared defaults.
pub use common_2x::{KERNEL_TTBR0_BASE, SAFE_INTERVAL_DELTA_SECONDS};
