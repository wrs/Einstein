//! Real-hardware bring-up probe for the BCM2835 SDHOST driver.
//!
//! Called from `kmain` under `#[cfg(feature = "sd-probe")]`. Runs
//! after `uart::init` (so we have a console) and after `mmu::init`
//! (so the peripheral window is properly Device-nGnRE and our
//! cache-maintenance ops do what we expect on the mailbox buffer).
//!
//! The probe is destructive of the rest of boot: it halts via
//! `cpu::halt()` once finished, regardless of outcome.
//!
//! Test plan:
//!
//! 1. Initialise SDHOST, enumerate the card.
//! 2. Hand the `SdHost` to `embedded_sdmmc::VolumeManager`.
//! 3. Open volume 0 (the FAT32 boot partition we built with
//!    `scripts/build-sd.sh`).
//! 4. Open the root directory.
//! 5. Open `config.txt` and dump its contents.
//!
//! Success means we can read files we put on the card from a host
//! tool — i.e. the whole storage stack (SDHOST + FAT32) works
//! end-to-end. From there, `flash-persist-sd` and `snapshot-sd` are
//! mechanical.

use embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};

use crate::cpu;
use crate::{kprint, kprintln};

use super::block_device::NullTime;
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

    // Quick sanity-check read of sector 0 before handing off to the
    // FAT layer. Same output shape as before, so if FAT mount
    // fails we can still see whether the underlying read works.
    let mut sector0 = [0u8; 512];
    if let Err(e) = host.read_block(0, &mut sector0) {
        kprintln!("read sector 0 FAILED: {:?}", e);
        cpu::halt();
    }
    let sig = u16::from_le_bytes([sector0[510], sector0[511]]);
    kprintln!("sector 0 boot sig = 0x{:04x}", sig);
    for i in 0..4 {
        let off = 446 + i * 16;
        let bootable = sector0[off];
        let ptype = sector0[off + 4];
        let lba = u32::from_le_bytes([
            sector0[off + 8],
            sector0[off + 9],
            sector0[off + 10],
            sector0[off + 11],
        ]);
        let size = u32::from_le_bytes([
            sector0[off + 12],
            sector0[off + 13],
            sector0[off + 14],
            sector0[off + 15],
        ]);
        kprintln!(
            "  part {}: bootable=0x{:02x} type=0x{:02x} lba={} size={}",
            i, bootable, ptype, lba, size
        );
    }

    // Milestone 2: prove the DMA → SDHOST write path in isolation.
    // Sector 1 lives in the MBR gap (partitions start at LBA 2048), so
    // it's safe to scribble; we save and restore it regardless. The
    // write goes via DMA, the read-back via the proven PIO path, so a
    // match confirms the DMA path end-to-end (DREQ 13, FIFO addressing,
    // command/data sequencing).
    {
        const TEST_LBA: u32 = 1;
        kprintln!("dma-write: testing DMA block-write at LBA {}", TEST_LBA);
        let mut orig = [0u8; 512];
        match host.read_block(TEST_LBA, &mut orig) {
            Ok(()) => {
                let mut pattern = [0u8; 512];
                for (i, b) in pattern.iter_mut().enumerate() {
                    *b = (i as u8) ^ 0xA5;
                }
                match host.write_block_dma(TEST_LBA, &pattern) {
                    Ok(()) => {
                        let mut back = [0u8; 512];
                        match host.read_block(TEST_LBA, &mut back) {
                            Ok(()) => match back.iter().zip(pattern.iter()).position(|(a, b)| a != b)
                            {
                                None => kprintln!("dma-write: PASS — 512 bytes match"),
                                Some(i) => kprintln!(
                                    "dma-write: MISMATCH at byte {} (got 0x{:02x}, want 0x{:02x})",
                                    i, back[i], pattern[i]
                                ),
                            },
                            Err(e) => kprintln!("dma-write: read-back FAILED: {:?}", e),
                        }
                    }
                    Err(e) => kprintln!("dma-write: write_block_dma FAILED: {:?}", e),
                }
                // Restore the original contents via the PIO path.
                if let Err(e) = host.write_block(TEST_LBA, &orig) {
                    kprintln!("dma-write: WARNING restore of LBA {} FAILED: {:?}", TEST_LBA, e);
                }
            }
            Err(e) => kprintln!("dma-write: save of LBA {} FAILED, skipping test: {:?}", TEST_LBA, e),
        }
    }

    kprintln!("fat: handing off to embedded-sdmmc...");
    let vmgr = VolumeManager::new(host, NullTime);

    let volume = match vmgr.open_volume(VolumeIdx(0)) {
        Ok(v) => {
            kprintln!("fat: open_volume(0) ok");
            v
        }
        Err(e) => {
            kprintln!("fat: open_volume(0) FAILED: {:?}", e);
            cpu::halt();
        }
    };

    let root = match volume.open_root_dir() {
        Ok(d) => {
            kprintln!("fat: open_root_dir ok");
            d
        }
        Err(e) => {
            kprintln!("fat: open_root_dir FAILED: {:?}", e);
            cpu::halt();
        }
    };

    let file = match root.open_file_in_dir("CONFIG.TXT", Mode::ReadOnly) {
        Ok(f) => {
            kprintln!("fat: open_file_in_dir(CONFIG.TXT) ok");
            f
        }
        Err(e) => {
            kprintln!("fat: open_file_in_dir(CONFIG.TXT) FAILED: {:?}", e);
            cpu::halt();
        }
    };

    kprintln!("====== config.txt ======");
    let mut buf = [0u8; 128];
    let mut total = 0usize;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                for &b in &buf[..n] {
                    // Echo printable ASCII + CR/LF directly; replace
                    // anything else with '.' so a binary file
                    // doesn't break the terminal.
                    if b == b'\r' || b == b'\n' || (0x20..0x7F).contains(&b) {
                        kprint!("{}", b as char);
                    } else {
                        kprint!(".");
                    }
                }
            }
            Err(e) => {
                kprintln!("\r\nfat: read FAILED: {:?}", e);
                cpu::halt();
            }
        }
    }
    kprintln!("\r\n====== ({} bytes) ======", total);
    // Close the read-only handle so the volume manager's file-slot
    // count goes back to zero before we open another file.
    drop(file);

    // Write probe: round-trip a known payload through the FAT layer.
    // Validates the write path before flash-persist-sd commits to it.
    write_probe(&root);

    kprintln!("halt");
    cpu::halt();
}

const WRITE_PROBE_NAME: &str = "EL2HELLO.TXT";
const WRITE_PROBE_PAYLOAD: &[u8] =
    b"hello from EL2 SDHOST probe (newton-hypervisor)\r\n";

fn write_probe(root: &embedded_sdmmc::Directory<SdHost, NullTime, 4, 4, 1>) {
    kprintln!("====== write probe ({}) ======", WRITE_PROBE_NAME);
    {
        // Open create-or-truncate so each run starts from a known
        // empty file. ReadWriteCreateOrTruncate creates the file if
        // it doesn't exist, otherwise empties it.
        let file = match root
            .open_file_in_dir(WRITE_PROBE_NAME, Mode::ReadWriteCreateOrTruncate)
        {
            Ok(f) => {
                kprintln!("fat: open(write) ok");
                f
            }
            Err(e) => {
                kprintln!("fat: open(write) FAILED: {:?}", e);
                cpu::halt();
            }
        };
        if let Err(e) = file.write(WRITE_PROBE_PAYLOAD) {
            kprintln!("fat: write FAILED: {:?}", e);
            cpu::halt();
        }
        kprintln!("fat: wrote {} bytes", WRITE_PROBE_PAYLOAD.len());
        if let Err(e) = file.flush() {
            kprintln!("fat: flush FAILED: {:?}", e);
            cpu::halt();
        }
        kprintln!("fat: flush ok");
        // file dropped at end of scope -> directory entry committed.
    }

    // Reopen read-only and verify byte-for-byte.
    let file = match root.open_file_in_dir(WRITE_PROBE_NAME, Mode::ReadOnly) {
        Ok(f) => f,
        Err(e) => {
            kprintln!("fat: reopen(read) FAILED: {:?}", e);
            cpu::halt();
        }
    };
    let mut readback = [0u8; 64];
    let n = match file.read(&mut readback) {
        Ok(n) => n,
        Err(e) => {
            kprintln!("fat: readback FAILED: {:?}", e);
            cpu::halt();
        }
    };
    if n != WRITE_PROBE_PAYLOAD.len() || &readback[..n] != WRITE_PROBE_PAYLOAD {
        kprintln!(
            "fat: readback MISMATCH (got {} bytes, want {})",
            n,
            WRITE_PROBE_PAYLOAD.len(),
        );
        kprint!("  got:  ");
        for &b in &readback[..n] {
            kprint!("{:02x} ", b);
        }
        kprintln!();
        cpu::halt();
    }
    kprintln!("fat: readback ok ({} bytes match)", n);
}
