//! Pen-input seam.
//!
//! Defines [`PenSource`] (a trait so a future second panel slots in
//! without rewriting the input path) and a small [`PenEvent`] enum
//! shared by all backends. [`pump`] runs from the timer-IRQ tail; it
//! drains the active backend in a non-blocking loop, tracks the
//! pen-down edge so [`crate::host_io::queue::enqueue_pen_sample`] sees
//! the same `0x000D` / `0x000E` markers the `host_io-semihost`
//! backend already inserts, and packs (x, y) into Einstein's
//! `TScreenManager` sample format.
//!
//! Backend selection: opt-in via the `input-*` Cargo features and
//! [`crate::build::resolve_input_backend`]; the resolver emits
//! `cfg(nh_input_<chosen>)`. With no feature enabled the fallback is
//! `null`, so a bare QEMU/FVP build inherits its pen events through
//! `host_io-semihost` (which writes directly to the queue and never
//! touches this module).
//!
//! Note the layering: pen events flow USB driver → [`PenSource`] →
//! [`pump`] → [`crate::host_io::queue`] → guest IRQ. The host-io
//! `pi_fb` backend does not implement `PenSource`; it just calls
//! [`pump`] from its own `pump_input` so all real-hw display-+input
//! builds share one input path regardless of how the host-io
//! backend is configured.

#[cfg(nh_input_null)]
mod null;
#[cfg(nh_input_mtouch)]
pub mod mtouch;

/// Logical pen event from a backend. Coordinates are in the
/// **Newton coordinate space** (0..319 horizontally, 0..479
/// vertically) — backends are responsible for any
/// panel-to-Newton transform (see `input::calibrate` for the
/// `pi-bare-metal-input` build) before producing one of these.
#[derive(Copy, Clone, Debug)]
pub enum PenEvent {
    Down { x: u16, y: u16 },
    Move { x: u16, y: u16 },
    Up,
}

/// Pluggable source of pen events. Implementations are owned by the
/// active backend module and accessed exclusively from `pump` on the
/// trap-return tail (single-threaded EL2), so no internal locking is
/// required.
pub trait PenSource {
    /// Return the next pending event, or `None` if the source has
    /// nothing buffered. Must be non-blocking — `pump` runs from a
    /// trap exit with the guest stalled.
    fn poll(&mut self) -> Option<PenEvent>;
}

/// Called once from `kmain` after `host_io::init`. Lets the backend
/// open whatever transport it needs (USB controller bring-up, etc.).
pub fn init() {
    #[cfg(nh_input_null)]
    null::init();
    #[cfg(nh_input_mtouch)]
    mtouch::init();
}

/// Drain the active backend and forward events to the guest. Runs
/// on the trap-return tail (`trap.rs`).
pub fn pump() {
    #[cfg(nh_input_null)]
    null::pump();
    #[cfg(nh_input_mtouch)]
    mtouch::pump();
}

/// Helper used by every concrete backend's pump implementation:
/// pulls events from a `PenSource` until it's empty, tracks the
/// pen-down edge, and writes the Einstein-format sample pairs onto
/// the host_io queue. Pressure is fixed at 4 to match the
/// `host_io-semihost` host-viewer path byte-for-byte (Einstein's
/// `TScreenManager::PenDown` default pressure is also 4); the
/// kernel only consults the low 4 bits but specific values can
/// matter to downstream pen-event handlers.
#[cfg(any(nh_input_mtouch))]
pub(crate) fn drain_into_queue<P: PenSource>(src: &mut P) {
    use core::sync::atomic::{AtomicBool, Ordering};
    use crate::host_io::{pack_pen_sample, queue, PEN_DOWN_SAMPLE_MARKER, PEN_UP_SAMPLE_MARKER};
    static DOWN: AtomicBool = AtomicBool::new(false);
    const PRESSURE: u16 = 4;
    while let Some(ev) = src.poll() {
        match ev {
            PenEvent::Down { x, y } => {
                if !DOWN.swap(true, Ordering::AcqRel) {
                    // Einstein's "first tap acts as the power button"
                    // hack (AndroidGlue.cpp:205-216): the Pi Zero 2 W
                    // build has no physical power button, so when the
                    // guest is parked in subfn 0x0E PowerOffSystem
                    // (deep-sleep WFI), synthesise a power-switch press
                    // on the pen-down edge. raise_power_switch sets
                    // WAKE_REQUEST, which `pause_system`'s WFI loop
                    // polls between heartbeats. The pen-down sample
                    // itself is still enqueued so the same tap registers
                    // in Newton's tablet driver once power-on completes.
                    if crate::peripherals::vic::is_powered_off() {
                        crate::peripherals::vic::raise_power_switch();
                    }
                    queue::enqueue_pen_sample(PEN_DOWN_SAMPLE_MARKER, 0);
                }
                queue::enqueue_pen_sample(pack_pen_sample(x, y, PRESSURE), 0);
            }
            PenEvent::Move { x, y } => {
                if DOWN.load(Ordering::Acquire) {
                    queue::enqueue_pen_sample(pack_pen_sample(x, y, PRESSURE), 0);
                }
            }
            PenEvent::Up => {
                if DOWN.swap(false, Ordering::AcqRel) {
                    queue::enqueue_pen_sample(PEN_UP_SAMPLE_MARKER, 0);
                }
            }
        }
    }
}

#[cfg(nh_input_mtouch)]
pub mod calibrate;
