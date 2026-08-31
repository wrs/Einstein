//! Null backend — no display sink, no input source. Compiled in when
//! `host-io-null` is the active feature, which is the default. Guest
//! tests and CI runs stay in this mode.

use super::HostIo;

pub struct NullBackend;

impl HostIo for NullBackend {
    fn init(&self) {}
    fn on_resume(&self) {}
    fn push_blit(&self, _ev: &super::BlitEvent, _payload: &[u8]) {}
    fn wants_payload(&self) -> bool {
        // Blits are dropped whole — no point assembling a payload.
        false
    }
    fn pump_input(&self) {}
}

pub static BACKEND: NullBackend = NullBackend;
