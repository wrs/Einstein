//! Einstein-equivalent ROM patches applied at load time.
//!
//! Phase A baseline: the 717006 ROM needs a handful of patches to
//! behave sensibly under any emulator / hypervisor. Einstein ships
//! these in `Emulator/JIT/Generic/TJITGenericROMPatch.cpp` and applies
//! them during `TROMImage::CreateImage`. Skipping them during our own
//! ROM load is what left the boot going sideways — most of these are
//! "disable a function that would otherwise hang" or "set a kernel-
//! globals flag that selects a boot path the rest of Einstein is
//! built around".
//!
//! We only port the simple *word-write* patches here (TJITGenericPatch
//! in Einstein's tree). The JIT-specific native-call patches
//! (TJITGenericPatchNativeCall — `DebugStr`, `Debugger`,
//! `RealClockSeconds`, `FTimeInSeconds`, etc.) write a SWI-style marker
//! that only Einstein's JIT knows how to intercept; on real hardware
//! they would execute as actual SWIs and take the ROM's own SWI path.
//! The virtualized-call patches (`__rt_sdiv`, `__rt_udiv`, `symcmp`)
//! write a 5-word sequence whose marker bit-31 is caught by Einstein's
//! `TNativePrimitives::ExecuteNative`; the same mechanism is cheap to
//! add to `peripherals::native_primitives` but is deferred — the ROM
//! runs these routines natively on an A53 just fine, the virtualized
//! version is a performance tweak, not a correctness requirement.
//!
//! What the simple patches change (all at main-ROM offsets, applied
//! AFTER byteswap so we write in guest-CPU view):
//!
//! - `0x0000_13F4` ← 1               — `gDebugger` on: ROM takes the
//!   debugger-enabled codepath (selects the driver path we need).
//! - `0x0000_13FC` ← 0x0000_8202     — `gNewtConfig`:
//!   kEnableListener | kDefaultStdioOn | kEnableStdout.
//! - `0x0008_A20C` ← MOV PC, LR      — `Ignore setting time` (the
//!   real ROM would call RTC hardware we don't model).
//! - `0x000D_B0D8`/`0x000D_B0DC`     — BeaconDetect no-op
//!   (MOV R0,#0 ; MOV PC,LR). Einstein disables the geoport beacon
//!   detect loop; on our hypervisor the same loop would spin forever
//!   on a peripheral we don't model.
//! - `0x0014_12F8` ← B +0x24          — Avoid screen calibration.
//! - `0x0030_F088`, `0x0042_0750`, `0x0042_0798`, `0x004D_CA14` —
//!   "Year 2010" time-base constants. Newer time base minutes /
//!   seconds so NewtonOS time arithmetic stays inside the valid range.
//!
//! See `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp` for the
//! full annotated list and the Einstein-side rationale for each.

use crate::kprintln;

/// A single word-write patch against the main ROM (IPA 0..0x00800000).
#[derive(Copy, Clone)]
struct RomPatch {
    offset: u32,
    value:  u32,
    name:   &'static str,
}

/// Patches for the 717006 ROM (MP2100 US) — mirrors the `inAddr0`
/// column from every `TJITGenericPatch` in
/// `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp`, restricted
/// to entries that the 717006 ROM id selects (not `kROMPatchVoid`).
///
/// Values are precisely what Einstein writes:
///   - `newTimeBaseMinutes` = 218_799_360 = 0x0D09_5000
///   - `newTimeBaseSeconds` = 3_281_990_400 = 0xC3A5_1800
///   - `gNewtConfig` combines `kEnableListener (0x2)`,
///     `kDefaultStdioOn (0x200)`, `kEnableStdout (0x8000)`.
const PATCHES_717006: &[RomPatch] = &[
    RomPatch { offset: 0x0000_13F4, value: 0x0000_0001, name: "gDebugger patch" },
    RomPatch { offset: 0x0000_13FC, value: 0x0000_8202, name: "gNewtConfig patch" },
    RomPatch { offset: 0x0008_A20C, value: 0xE1A0_F00E, name: "Ignore setting time" },
    RomPatch { offset: 0x000D_B0D8, value: 0xE3A0_0000, name: "BeaconDetect (1/2)" },
    RomPatch { offset: 0x000D_B0DC, value: 0xE1A0_F00E, name: "BeaconDetect (2/2)" },
    RomPatch { offset: 0x0014_12F8, value: 0xEA00_0009, name: "Avoid screen calibration" },
    RomPatch { offset: 0x0030_F088, value: 0xC3A5_1800, name: "Time base (4/4)" },
    RomPatch { offset: 0x0042_0750, value: 0x0D09_5000, name: "Time base (1/4)" },
    RomPatch { offset: 0x0042_0798, value: 0x0D09_5000, name: "Time base (2/4)" },
    RomPatch { offset: 0x004D_CA14, value: 0x0D09_5000, name: "Time base (3/4)" },
];

/// Apply Einstein's word-write patches to the byteswapped main ROM
/// backing. Caller must own `rom_ptr`; the patches live entirely in the
/// main-ROM half (offsets < 0x0080_0000), so overlap with Einstein.rex
/// loaded at 0x0080_0000 is not a concern.
///
/// SAFETY: `rom_ptr` must point to at least `0x0080_0000` bytes of
/// writable backing, and all patch offsets are checked to be in range
/// and word-aligned before the write.
pub unsafe fn apply_717006_patches(rom_ptr: *mut u32) {
    let mut applied = 0usize;
    for p in PATCHES_717006 {
        debug_assert!(p.offset & 3 == 0, "patch offset must be word-aligned");
        debug_assert!((p.offset as usize) < 0x0080_0000, "patch offset must be in main ROM");
        let word_idx = (p.offset / 4) as usize;
        // SAFETY: bounds-checked against the 8 MiB main-ROM region.
        unsafe {
            let prev = rom_ptr.add(word_idx).read();
            rom_ptr.add(word_idx).write(p.value);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({})",
                p.offset, prev, p.value, p.name,
            );
        }
        applied += 1;
    }
    kprintln!("rom_patch: applied {} simple patches", applied);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Basic invariants: all patches fit in the main ROM and are word-aligned.
    #[test]
    fn patches_are_word_aligned_and_in_main_rom() {
        for p in PATCHES_717006 {
            assert_eq!(p.offset & 3, 0, "offset {:#x} not word-aligned", p.offset);
            assert!(p.offset < 0x0080_0000, "offset {:#x} outside main ROM", p.offset);
        }
    }

    // No duplicates.
    #[test]
    fn patch_offsets_are_unique() {
        let mut offs: std::vec::Vec<u32> =
            PATCHES_717006.iter().map(|p| p.offset).collect();
        offs.sort();
        for w in offs.windows(2) {
            assert_ne!(w[0], w[1], "duplicate patch offset {:#x}", w[0]);
        }
    }
}
