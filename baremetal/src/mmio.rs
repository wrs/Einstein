//! Stub MMIO dispatch for trapped guest accesses to Newton peripheral space.
//!
//! Every access that lands here comes from a stage-2 fault — IPA is outside
//! our mapped ROM/RAM regions. For first-light boot we recognise the Newton
//! memory-consts ranges and return sensible-enough values to let the guest
//! keep executing:
//!
//!   0x0200_0000..0x0240_0000  Flash bank 1 — reads 0xFFFFFFFF (erased)
//!   0x0F00_0000..0x0F40_0000  Hardware registers — returns 0 for unknown
//!   elsewhere                 log once, return 0
//!
//! Real peripheral emulation (`TInterruptManager`, `TDMAManager`,
//! `TFlash`, ...) lands in M3 / M4. For now we just want the guest to
//! continue past its initial probe.

use crate::{kprintln, vic};
use core::sync::atomic::{AtomicUsize, Ordering};

const FLASH1_BASE: u64 = 0x0200_0000;
const FLASH1_END: u64 = 0x0240_0000;

const HW_BASE: u64 = 0x0F00_0000;
const HW_END: u64 = 0x0F40_0000;

// Specific register reads the Newton kernel does very early.
//   TMemoryConsts::kHdWr_04RAMSize = 0x0F00_1800  — encodes installed RAM
//   TMemoryConsts::kHdWr_08RAMSize = 0x0F00_1C00  — secondary bank size
const HW_RAM_SIZE_1: u64 = 0x0F00_1800;
const HW_RAM_SIZE_2: u64 = 0x0F00_1C00;

/// Per-region bucket: IPA rounded to 4 KiB, decide once whether to log.
const MAX_UNIQUE_BUCKETS: usize = 256;
static BUCKET_KEYS: [AtomicUsize; MAX_UNIQUE_BUCKETS] =
    [const { AtomicUsize::new(0) }; MAX_UNIQUE_BUCKETS];
static BUCKET_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn read(ipa: u64, sas: u8) -> u32 {
    let value = match ipa {
        HW_RAM_SIZE_1 => 0x4040_0040,
        HW_RAM_SIZE_2 => 0,

        a if vic::owns(a) => vic::read(a),

        a if (HW_BASE..HW_END).contains(&a) => {
            log_unknown("hw read", a, sas);
            0
        }

        // Flash is now stage-2-mapped and never reaches this dispatcher,
        // but keep the fallback for any IPA below flash end in case we
        // ever hit an unaligned / out-of-range access.
        a if (FLASH1_BASE..FLASH1_END).contains(&a) => 0xFFFF_FFFF,

        a => {
            log_unknown("unmapped read", a, sas);
            0
        }
    };

    mask_for_size(value, sas)
}

pub fn write(ipa: u64, sas: u8, value: u32) {
    if vic::owns(ipa) {
        vic::write(ipa, value);
        return;
    }
    if (HW_BASE..HW_END).contains(&ipa) {
        log_unknown("hw write (ignored)", ipa, sas);
        let _ = value;
        return;
    }
    // Flash writes come through stage-2 directly (the region is mapped RW).
    if (FLASH1_BASE..FLASH1_END).contains(&ipa) {
        log_unknown("flash write via trap (ignored)", ipa, sas);
        let _ = value;
        return;
    }
    log_unknown("unmapped write (ignored)", ipa, sas);
    let _ = value;
}

fn mask_for_size(value: u32, sas: u8) -> u32 {
    match sas {
        0 => value & 0xFF,
        1 => value & 0xFFFF,
        _ => value,
    }
}

fn log_unknown(what: &str, ipa: u64, sas: u8) {
    // Dedup by 4 KiB page so a spin-loop on one address only logs once.
    let key = (ipa >> 12) as usize | 1; // 1-based so 0 means "empty slot"
    for i in 0..MAX_UNIQUE_BUCKETS {
        let cur = BUCKET_KEYS[i].load(Ordering::Relaxed);
        if cur == key {
            return; // already logged this page
        }
        if cur == 0 {
            // Try to claim the slot.
            if BUCKET_KEYS[i]
                .compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let n = BUCKET_COUNT.fetch_add(1, Ordering::Relaxed);
                let width = match sas {
                    0 => "B ", 1 => "H ", 2 => "W ", _ => "D ",
                };
                kprintln!(
                    "mmio[uniq {:3}] {}{} IPA={:#010x}",
                    n, width, what, ipa
                );
                return;
            }
        }
    }
    // Table full — silently drop further new pages.
}
