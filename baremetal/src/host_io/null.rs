//! Null backend — no display sink, no input source. Compiled in when
//! `host-io-null` is the active feature, which is the default. Guest
//! tests and CI runs stay in this mode.

pub fn init() {}
pub fn on_resume() {}

pub fn push_blit(_ev: &super::BlitEvent, _payload: &[u8]) {}
pub fn pump_input() {}
