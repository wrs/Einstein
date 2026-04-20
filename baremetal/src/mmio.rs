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

use crate::kprintln;
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

/// Upper bound on chatty log lines before we go silent (stops runaway
/// output when the guest spins on an MMIO poll).
const MMIO_LOG_BUDGET: usize = 128;
static MMIO_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn read(ipa: u64, sas: u8) -> u32 {
    let value = match ipa {
        // Empty / erased flash: 0xFF bytes everywhere.
        a if (FLASH1_BASE..FLASH1_END).contains(&a) => 0xFFFF_FFFF,

        // RAM size register: MP2100 has 4 MiB. TMemory.cpp:868 computes
        //   (pages << 24) | (pages << 16) | pages
        // with pages = (RAMSize >> 16) & 0xFF = 0x40. Result = 0x40404040.
        HW_RAM_SIZE_1 => 0x4040_0040,

        // Secondary bank size: no secondary bank.
        HW_RAM_SIZE_2 => 0,

        // Anything else in the HW window: zero. Log the first few
        // unknown addresses for diagnostic purposes.
        a if (HW_BASE..HW_END).contains(&a) => {
            log_unknown("hw read", a, sas);
            0
        }

        // Fully off-map address: log and zero.
        a => {
            log_unknown("unmapped read", a, sas);
            0
        }
    };

    mask_for_size(value, sas)
}

pub fn write(ipa: u64, sas: u8, value: u32) {
    if (FLASH1_BASE..FLASH1_END).contains(&ipa) {
        log_unknown("flash write (ignored)", ipa, sas);
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
    let n = MMIO_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < MMIO_LOG_BUDGET {
        let width = match sas {
            0 => "B ",
            1 => "H ",
            2 => "W ",
            _ => "D ",
        };
        kprintln!("mmio[{:3}] {}{} IPA={:#010x}", n, width, what, ipa);
    } else if n == MMIO_LOG_BUDGET {
        kprintln!("mmio log budget exhausted — silencing further output");
    }
}
