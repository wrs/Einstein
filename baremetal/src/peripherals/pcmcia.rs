//! PCMCIA socket stubs.
//!
//! Einstein exposes four PCMCIA controller windows starting at
//! 0x3000_0000, 0x4000_0000, 0x5000_0000, and 0x6000_0000 (1 GiB
//! apart). Typical Newton hardware only wires up slots 0 and 1, which
//! is all we care about until a real card emulation lands.
//!
//! For the bring-up path we only need the kernel's card-present probe
//! to conclude "no card": reads return all-ones, writes silently
//! drop. See `docs/peripherals.md` §Things we deliberately don't model
//! yet for the rationale and `Emulator/PCMCIA/TPCMCIAController.cpp`
//! for the eventual reference implementation.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::kprintln;

/// Slot 0 window: 256 MiB at 0x3000_0000..0x4000_0000.
/// Einstein `TMemoryConsts::kPCMCIA0Base`.
const SLOT0_BASE: u64 = 0x3000_0000;
const SLOT0_END: u64 = 0x4000_0000;

/// Slot 1 window: 256 MiB at 0x4000_0000..0x5000_0000.
/// Einstein `TMemoryConsts::kPCMCIA1Base`.
const SLOT1_BASE: u64 = 0x4000_0000;
const SLOT1_END: u64 = 0x5000_0000;

static LOG_BUDGET: AtomicUsize = AtomicUsize::new(0);
const LOG_MAX: usize = 16;

pub fn owns(ipa: u64) -> bool {
    (SLOT0_BASE..SLOT0_END).contains(&ipa) || (SLOT1_BASE..SLOT1_END).contains(&ipa)
}

pub fn read(ipa: u64) -> u32 {
    log("pcmcia read", ipa, 0);
    // "No card" on both slots — Newton kernel interprets all-ones as
    // the empty socket response.
    0xFFFF_FFFF
}

pub fn write(ipa: u64, value: u32) {
    log("pcmcia write", ipa, value);
}

fn log(what: &str, ipa: u64, value: u32) {
    let n = LOG_BUDGET.fetch_add(1, Ordering::Relaxed);
    if n < LOG_MAX {
        kprintln!("{} IPA={:#010x} val={:#010x}", what, ipa, value);
    }
}
