//! Ports of Einstein's Newton peripheral state machines into Rust.
//!
//! Each submodule owns the backing storage and observable behaviour of
//! one Newton peripheral. The module layout mirrors the Einstein class
//! names so cross-referencing `docs/peripherals.md` and
//! `Emulator/T*.cpp` stays straightforward.

pub mod asic;
pub mod battery;
pub mod dma;
pub mod flash;
pub mod flash_driver;
pub mod guest_access;
pub mod host_call;
pub mod in_translator;
pub mod native_primitives;
pub mod network;
pub mod out_translator;
pub mod platform;
pub mod pcmcia;
pub mod printer;
pub mod screen;
pub mod serial;
pub mod serial_driver;
pub mod sound;
pub mod tablet;
pub mod vic;
