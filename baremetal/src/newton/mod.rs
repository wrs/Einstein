//! Newton-OS-specific logic: the `GuestOs` hook impl, the ROM loader,
//! ROM patches, probes, trampolines, the inline-patch and
//! unaligned-access emulation.

pub mod guest_trampolines;
pub mod inline_patch;
pub mod loader;
pub mod os;
pub mod probes;
pub mod rom_patches;
pub mod rom_ver;
pub mod unaligned;
pub mod unaligned_inline;

pub use os::NewtonOs;
