//! Null pen-input backend.
//!
//! Compiled in when the resolved input backend is `null` (the default
//! when no `input-*` Cargo feature is selected). `init` and `pump`
//! are no-ops so the timer-IRQ tail pays nothing when there's no real
//! source of pen events.
//!
//! QEMU and FVP builds get their pen events through
//! `host_io-semihost`, which writes directly to
//! `host_io::queue::enqueue_pen_sample`. The semihost path is
//! independent of this module — keeping the null backend silent
//! avoids two parallel pen-event streams competing for the same
//! `INT_TABLET`.

pub fn init() {}
pub fn pump() {}
