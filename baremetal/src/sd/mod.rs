//! SD-card storage stack for the Pi Zero 2 W.
//!
//! Layering (bottom up):
//!
//! - [`sdhost`] — bare-metal driver for the BCM2835 SDHOST
//!   controller. Polled mode, no IRQ, no DMA. 512-byte single-block
//!   read/write.
//! - `mbr` (planned) — parse the MBR partition table, expose
//!   partition 1 (the FAT32 boot partition we already have) as a
//!   partition-relative block device.
//! - `block_device` (planned) — adapt the SDHOST + MBR layers to
//!   `embedded_sdmmc::BlockDevice`.
//! - Consumers: `flash_persist::sd` (Phase 2 of the real-hardware
//!   bring-up) and eventually `snapshot::sd` (Phase 3).
//!
//! See `docs/REAL_HW_BRINGUP.md` for the phase plan and the rationale
//! behind picking `embedded-sdmmc` over `fatfs`.

#[cfg(feature = "sd-probe")]
pub mod probe;
pub mod regs;
pub mod sdhost;
