//! MMIO dispatch for trapped guest accesses to Newton peripheral space.
//!
//! Every access that lands here comes from a stage-2 fault — the IPA
//! is outside our mapped ROM / RAM / flash / framebuffer regions.
//! We route each IPA to the owning peripheral module where we can,
//! and fall through to budget-limited logging + a sensible default
//! for the registers that don't yet have a home.
//!
//! Routing order (first match wins):
//!
//!   1. peripherals::vic     — interrupt controller + tick clock
//!                             (0x0F18_xxxx).
//!   2. peripherals::dma     — DMA bank 1 / 2 + chip-wide registers
//!                             (0x0F08_0000..0x0F09_9000).
//!   3. peripherals::pcmcia  — "no card" for slot 0 and slot 1
//!                             (0x30000000..0x50000000).
//!   4. A handful of still-inline stubs for registers the Newton ROM
//!      reads at boot time (RAM size, chipset revision, power/GPIO
//!      bits). These stay here until a broader "platform" or memctl
//!      module absorbs them.
//!   5. Unknown IPAs: log once per 4 KiB page; reads return 0; writes
//!      are dropped.
//!
//! When you find yourself guessing what a register should return,
//! build a probe run and check Einstein's behaviour first — see
//! `probe/FINDINGS.md`.

use crate::{kprintln, peripherals::{dma, pcmcia, vic}};
use core::sync::atomic::{AtomicUsize, Ordering};

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
        a if vic::owns(a) => vic::read(a),
        a if dma::owns(a) => dma::read(a),
        a if pcmcia::owns(a) => pcmcia::read(a),

        HW_RAM_SIZE_1 => 0x4040_0040,
        HW_RAM_SIZE_2 => 0,

        // Chipset revision ID register the kernel probes early.
        // Typical observed value per TMemoryConsts notes.
        0x0F24_2400 => 0x01F9_4573,

        // Bank control / memory speed registers — return 0.
        0x0F00_1000 => 0,
        0x0F24_1000 => 0,

        // GPIO input data (PCMCIA door lock etc.) — Einstein returns all 1s.
        0x0F18_D400 => 0xFFFF_FFFF,

        // Power status / miscellaneous — "all OK" = high.
        0x0F18_4C00 => 0xFFFF_FFFF,

        a if (HW_BASE..HW_END).contains(&a) => {
            log_unknown("hw read", a, sas);
            0
        }

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
    if dma::owns(ipa) {
        dma::write(ipa, value);
        return;
    }
    if pcmcia::owns(ipa) {
        pcmcia::write(ipa, value);
        return;
    }
    if (HW_BASE..HW_END).contains(&ipa) {
        log_unknown("hw write (ignored)", ipa, sas);
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
