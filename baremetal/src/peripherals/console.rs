//! Guest-serial ↔ host-console seam.
//!
//! The guest's external serial port (`extr`, port 0) moves bytes in
//! two ways — PIO TX through `peripherals::serial` and ring-buffer DMA
//! through `peripherals::dma` (ch 1 TX / ch 0 RX). Both ultimately
//! talk to whatever the host considers "the serial wire". This module
//! is the fn-pointer seam between the two layers: the guest models
//! call [`tx`] / [`rx`] and never import `host::*`; `main.rs` installs
//! the concrete host endpoints (`host::console::write_byte` /
//! `read_byte_nonblock`) at boot via [`install`].
//!
//! Defaults are inert (TX drops the byte, RX reports "no data"), so an
//! uninstalled seam behaves like an unplugged serial cable rather than
//! a fault.

use core::cell::UnsafeCell;

/// Host endpoints for the guest serial wire.
pub struct GuestConsoleOps {
    /// Emit one guest TX byte on the host wire. Must not block beyond
    /// a bounded FIFO wait — callers run inside trap handlers.
    pub tx: fn(u8),
    /// Non-blocking host RX poll: `Some(byte)` if the host has data
    /// for the guest, `None` otherwise.
    pub rx: fn() -> Option<u8>,
}

fn tx_drop(_b: u8) {}
fn rx_none() -> Option<u8> {
    None
}

struct OpsCell(UnsafeCell<GuestConsoleOps>);
// SAFETY: written once by `install` from kmain on core 0 before the
// guest runs; read-only afterwards from the single EL2 trap handler.
unsafe impl Sync for OpsCell {}

static OPS: OpsCell = OpsCell(UnsafeCell::new(GuestConsoleOps {
    tx: tx_drop,
    rx: rx_none,
}));

/// Install the host endpoints. Called once from `main.rs` during boot
/// wiring, before the guest can touch the serial models.
pub fn install(ops: GuestConsoleOps) {
    // SAFETY: single-core EL2, called before any trap handler reads OPS.
    unsafe {
        *OPS.0.get() = ops;
    }
}

/// Forward one guest TX byte to the host wire.
pub fn tx(b: u8) {
    // SAFETY: see OpsCell.
    ((unsafe { &*OPS.0.get() }).tx)(b)
}

/// Poll the host wire for one RX byte (non-blocking).
pub fn rx() -> Option<u8> {
    // SAFETY: see OpsCell.
    ((unsafe { &*OPS.0.get() }).rx)()
}
