//! Real-hardware bring-up probe for the BCM2835 SDHOST driver.
//!
//! Called from `kmain` under `#[cfg(feature = "sd-probe")]`. Runs
//! after `uart::init` (so we have a console) and after `mmu::init`
//! (so the peripheral window is properly Device-nGnRE and our
//! cache-maintenance ops do what we expect on the mailbox buffer).
//!
//! The probe is destructive of the rest of boot: it deliberately
//! halts via `cpu::halt()` once finished, regardless of outcome.
//! For a first SDHOST signal we want a clean snapshot of "did the
//! controller respond?" with no later boot state to confuse the
//! reading.
//!
//! Output shape (success case) on a card with a standard MBR:
//!
//! ```text
//! === SDHOST probe ===
//! init... ok (SDHC, RCA=0x59b40000)
//! read sector 0...
//!   boot sig = 0xaa55 (OK)
//!   first 16: eb 3c 90 4d 53 44 4f 53 35 2e 30 00 02 08 20 00
//!   part 0:   bootable=0x00 type=0x0c lba=8192 size=...
//! halt
//! ```

use crate::cpu;
use crate::{kprint, kprintln};

use super::sdhost::{CardCapacity, SdHost};

pub fn run() -> ! {
    kprintln!("\r\n=== SDHOST probe ===");

    let host = match SdHost::init() {
        Ok(h) => {
            let cap = match h.capacity() {
                CardCapacity::HighCapacity => "SDHC",
                CardCapacity::StandardCapacity => "SDSC",
            };
            kprintln!("init... ok ({}, RCA=0x{:08x})", cap, h.rca());
            h
        }
        Err(e) => {
            kprintln!("init... FAILED: {:?}", e);
            cpu::halt();
        }
    };

    kprintln!("read sector 0...");
    let mut buf = [0u8; 512];
    if let Err(e) = host.read_block(0, &mut buf) {
        kprintln!("  FAILED: {:?}", e);
        cpu::halt();
    }

    let sig = u16::from_le_bytes([buf[510], buf[511]]);
    let sig_ok = if sig == 0xAA55 { "OK" } else { "BAD" };
    kprintln!("  boot sig = 0x{:04x} ({})", sig, sig_ok);

    kprint!("  first 16:");
    for b in &buf[..16] {
        kprint!(" {:02x}", b);
    }
    kprintln!();

    if sig == 0xAA55 {
        // Decode the four MBR partition entries.
        for i in 0..4 {
            let off = 446 + i * 16;
            let bootable = buf[off];
            let ptype = buf[off + 4];
            let lba = u32::from_le_bytes([
                buf[off + 8],
                buf[off + 9],
                buf[off + 10],
                buf[off + 11],
            ]);
            let size = u32::from_le_bytes([
                buf[off + 12],
                buf[off + 13],
                buf[off + 14],
                buf[off + 15],
            ]);
            kprintln!(
                "  part {}: bootable=0x{:02x} type=0x{:02x} lba={} size={}",
                i, bootable, ptype, lba, size
            );
        }
    }

    kprintln!("halt");
    cpu::halt();
}
