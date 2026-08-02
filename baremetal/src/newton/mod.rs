//! Newton-OS-specific logic: ROM patches, probes, trampolines, the
//! shadow stub and unaligned-access emulation.

pub mod guest_trampolines;
pub mod probes;
pub mod rom_patches;
pub mod shadow_stub;
pub mod unaligned;
pub mod unaligned_inline;
