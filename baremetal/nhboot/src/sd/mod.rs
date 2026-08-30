//! SD-card stack for the bootloader: the BCM2835 SDHOST driver (PIO
//! copy, see `sdhost.rs`), the `embedded_sdmmc::BlockDevice` shim and
//! the register constants. The latter two are the hypervisor's own
//! files, included by path — they have no dependencies beyond
//! `super::sdhost` and the vendored FAT crate, so one copy serves
//! both binaries.

#[path = "../../../src/host/sd/block_device.rs"]
pub mod block_device;
#[path = "../../../src/host/sd/regs.rs"]
pub mod regs;
pub mod sdhost;
