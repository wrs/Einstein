//! Ports of Einstein's Newton peripheral state machines into Rust.
//!
//! Each submodule owns the backing storage and observable behaviour of
//! one Newton peripheral. The module layout mirrors the Einstein class
//! names so cross-referencing `docs/peripherals.md` and
//! `Emulator/T*.cpp` stays straightforward.

pub mod dma;
pub mod flash;
pub mod flash_driver;
pub mod native_primitives;
pub mod platform;
pub mod pcmcia;
pub mod screen;
pub mod serial;
pub mod sound;
pub mod vic;
