//! Minimal USB host stack.
//!
//! Scope (permanent — see `docs/REAL_HW_BRINGUP.md` Phase 5):
//!
//! - **Single full-speed device** on the OTG port. Pi Zero 2 W's
//!   micro-USB OTG wires straight to the BCM2710 DWC2 controller;
//!   audio exits via HDMI in Phase 6, so we never need a hub or a
//!   second USB device.
//! - **Control + interrupt transfers only.** No bulk, no
//!   isochronous, no split transactions, no device mode. The HID
//!   class we care about runs on EP0 (control) + interrupt-IN.
//! - **Polled.** Driven from the timer-IRQ tail in `trap.rs`. The
//!   touchscreen reports at ~16 ms cadence, which lines up
//!   naturally with our existing CNTHP heartbeat.
//!
//! Layering:
//!
//! ```text
//!   trait UsbHostController   ← src/host/usb/host/mod.rs
//!     impl Dwc2               ← src/host/usb/host/dwc2/
//!   enumerate / hid helpers   ← src/host/usb/{enumerate, class}/
//!     MTouch driver           ← src/host/input/mtouch.rs
//! ```
//!
//! Only builds that select the `input-mtouch` backend compile the
//! stack — `src/main.rs` gates `mod usb` on `cfg(nh_input_mtouch)`
//! (see `src/host/input/mod.rs` for the axis), so QEMU / FVP / bench
//! builds without a touchscreen pay nothing.

pub mod descriptor;
pub mod enumerate;
pub mod host;
pub mod class {
    pub mod hid;
}
// The MTouch driver lives in `src/host/input/mtouch.rs` — it doubles as
// the `PenSource` backend, so it makes more sense as part of the
// `input` tree than a `usb::device` module.

/// USB standard request `bmRequestType` direction bit.
pub const REQ_DIR_OUT: u8 = 0x00;
pub const REQ_DIR_IN: u8 = 0x80;

/// USB standard request `bmRequestType` type field.
pub const REQ_TYPE_STANDARD: u8 = 0x00;
pub const REQ_TYPE_CLASS: u8 = 0x20;

/// USB standard request `bmRequestType` recipient field.
pub const REQ_RECIP_DEVICE: u8 = 0x00;
pub const REQ_RECIP_INTERFACE: u8 = 0x01;

/// USB standard request codes — USB 2.0 §9.4. Only the requests the
/// enumeration sequence actually issues.
pub const REQ_SET_ADDRESS: u8 = 0x05;
pub const REQ_GET_DESCRIPTOR: u8 = 0x06;
pub const REQ_SET_CONFIGURATION: u8 = 0x09;

/// Standard descriptor `bDescriptorType` values — USB 2.0 Table 9-5.
pub const DESC_DEVICE: u8 = 1;
pub const DESC_CONFIGURATION: u8 = 2;
pub const DESC_INTERFACE: u8 = 4;
pub const DESC_ENDPOINT: u8 = 5;

/// USB device speed — full-speed only on this hardware.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
}

/// Setup-stage packet for a control transfer. 8 bytes on the wire,
/// USB 2.0 §9.3.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug)]
pub struct SetupPacket {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub w_value: u16,
    pub w_index: u16,
    pub w_length: u16,
}

const _: () = {
    assert!(core::mem::size_of::<SetupPacket>() == 8);
};

impl SetupPacket {
    pub const fn new(
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
    ) -> Self {
        Self {
            bm_request_type,
            b_request,
            w_value,
            w_index,
            w_length,
        }
    }
}

/// Errors that bubble out of host-controller calls.
#[derive(Copy, Clone, Debug)]
pub enum UsbError {
    /// Setup / IN / OUT transfer timed out.
    Timeout,
    /// Device returned a STALL handshake (endpoint or control stage).
    Stall,
    /// CRC, bit-stuffing, or babble error on the wire.
    TransactionError,
    /// Controller is in an unexpected state (port not connected,
    /// driver hasn't been initialised, etc.).
    NotReady,
    /// The enumerated device is not the one this driver binds to
    /// (VID/PID mismatch). Distinct from `NotReady` so a bring-up log
    /// can tell "wrong USB device plugged in" from "controller fault".
    WrongDevice,
    /// Generic catch-all for cases we haven't named yet.
    Other,
}

pub type UsbResult<T> = core::result::Result<T, UsbError>;
