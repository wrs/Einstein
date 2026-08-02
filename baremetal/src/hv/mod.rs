//! Generic hypervisor core: stage-2 translation, guest entry/exit,
//! trap dispatch, MMIO routing, snapshots, guest memory map.

pub mod guest;
pub mod guest_endian;
pub mod guest_mem;
pub mod guest_regions;
pub mod hvc_imm;
pub mod mmio;
pub mod snapshot;
pub mod stage2;
pub mod timer;
pub mod trap;
