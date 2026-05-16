//! Null audio backend — no host audio output. Compiled in when
//! `audio-null` is the active feature (the default), so QEMU/FVP
//! builds compile cleanly and the Newton kernel's sound code is
//! exercised end-to-end without any host plumbing.
//!
//! Every entry is a no-op; values returned to the guest are the same
//! ones the existing `peripherals::sound::handle` stubs returned
//! before the audio seam was added (success + zero).

pub fn init() {}
