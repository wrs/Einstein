//! Constants shared by every NewtonOS 2.x ROM build (as opposed to the
//! per-version code addresses in `r717006/` etc.). Version modules
//! re-export these; a hypothetical 1.x version module would supply its
//! own values instead.

/// The 2.x kernel builds its stage-1 L1 table at the base of guest
/// RAM; the hypervisor's table-normalisation walkers gate on the
/// guest's TTBR0 actually pointing there (guest tests pick other
/// bases).
pub const KERNEL_TTBR0_BASE: u32 = 0x0400_0000;

/// Einstein's `safeIntervalDeltaSeconds` (TJITGenericROMPatch.cpp:144)
/// — seconds between 1993-01-01 and 2008-01-01, the Y2010 fix constant
/// consumed by the FTimeInSeconds / FDateFromSeconds injection stubs.
pub const SAFE_INTERVAL_DELTA_SECONDS: u32 = 473_299_200;
