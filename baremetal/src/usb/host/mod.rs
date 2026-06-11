//! Host-controller trait.
//!
//! Minimum surface for a single full-speed device on a hub-less bus:
//! port reset, control transfer, interrupt-IN submit. No bulk, no
//! isochronous, no split transactions, no device mode. The MTouch
//! panel never uses anything more.
//!
//! There's only one impl (DWC2) and there's only ever going to be
//! one — the trait exists to keep host-controller concerns separate
//! from enumeration / class / device-driver code, not to leave room
//! for a second backend. We treat it as a layering boundary, not a
//! plugin point.

pub mod dwc2;

use super::{SetupPacket, Speed, UsbResult};

/// Polled USB host controller. All methods block in a poll loop —
/// the touchscreen reports at ~16 ms and we run from the timer-IRQ
/// tail (also ~16 ms), so we have a full heartbeat to complete a
/// transfer between two scheduled poll opportunities.
///
/// The DWC2 implementation does NOT need to dynamically allocate
/// transfer state; all bookkeeping lives in the driver-owned
/// `Dwc2` struct. Channels and FIFOs are reset between transfers.
pub trait UsbHostController {
    /// Bring the controller out of reset, initialise the core,
    /// transition to host mode. Idempotent.
    fn init(&mut self) -> UsbResult<()>;

    /// Power the port, wait for the device to attach and stabilise,
    /// then issue the USB reset signalling. Returns the device's
    /// reported speed. Must be called after `init` and any time the
    /// port is reattached.
    fn port_reset_and_speed(&mut self) -> UsbResult<Speed>;

    /// Run a control transfer through the active device's EP0.
    ///
    /// - `addr` is the USB device address (0 immediately after reset,
    ///   then whatever SET_ADDRESS assigned).
    /// - `max_packet_size0` is the device's EP0 wMaxPacketSize. The
    ///   first GET_DESCRIPTOR(Device, 8) before SET_ADDRESS uses
    ///   `8` as a safe lower bound.
    /// - `setup` is the 8-byte Setup stage packet.
    /// - `data` (for an IN data stage) is filled with the device's
    ///   reply; the number of bytes actually returned is the return
    ///   value. None of the requests we issue carries an OUT data
    ///   stage, so there is no OUT buffer variant.
    ///
    /// Returns `Ok(bytes_transferred)` on success.
    fn control_transfer(
        &mut self,
        addr: u8,
        max_packet_size0: u8,
        setup: &SetupPacket,
        data: ControlData<'_>,
    ) -> UsbResult<usize>;
}

/// Data buffer for a control transfer's optional data stage.
pub enum ControlData<'a> {
    /// No data stage.
    None,
    /// IN: buffer is filled by the device.
    In(&'a mut [u8]),
}
