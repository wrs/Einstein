//! SD-card storage stack for the Pi Zero 2 W.
//!
//! Layering (bottom up):
//!
//! - [`sdhost`] — bare-metal driver for the BCM2835 SDHOST
//!   controller. Polled mode, no IRQ, no DMA. 512-byte single-block
//!   read/write.
//! - [`block_device`] — adapts the SDHOST driver to
//!   `embedded_sdmmc::BlockDevice`. MBR and FAT parsing come from
//!   the vendored `embedded-sdmmc`, which reaches partition 1 (the
//!   FAT32 boot partition) through this impl.
//! - Consumers: `flash_persist::sd` (Phase 2 of the real-hardware
//!   bring-up) and eventually `snapshot::sd` (Phase 3).
//!
//! See `docs/REAL_HW_BRINGUP.md` for the phase plan and the rationale
//! behind picking `embedded-sdmmc` over `fatfs`.

pub mod block_device;
#[cfg(feature = "sd-probe")]
pub mod probe;
pub mod regs;
pub mod sdhost;
