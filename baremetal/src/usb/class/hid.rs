//! HID class control requests.
//!
//! HID 1.11 §7. We only need the one operation the MTouch driver
//! issues: `GET_REPORT(Feature, id)` — its activation handshake.
//! It's a free function over `UsbHostController` — there's one HID
//! class on the only device we'll ever drive, so a class trait would
//! be overengineering.

use super::super::host::{ControlData, UsbHostController};
use super::super::{
    SetupPacket, UsbResult, REQ_DIR_IN, REQ_RECIP_INTERFACE, REQ_TYPE_CLASS,
};

/// HID class-specific request code — HID 1.11 §7.2.
pub const HID_REQ_GET_REPORT: u8 = 0x01;

/// Report type byte that rides in wValue's high byte.
pub const HID_REPORT_FEATURE: u8 = 0x03;

/// `GET_REPORT(type, id, length)` — HID 1.11 §7.2.1. This is the
/// MTouch activation handshake: `get_report(Feature, 3, 2)` returns
/// `0x0a 0x00` ("Contact Count Max = 10") and unblocks the
/// interrupt stream.
pub fn get_report<H: UsbHostController>(
    host: &mut H,
    addr: u8,
    ep0_mps: u8,
    interface: u8,
    report_type: u8,
    report_id: u8,
    buf: &mut [u8],
) -> UsbResult<usize> {
    let value = (u16::from(report_type) << 8) | u16::from(report_id);
    let setup = SetupPacket::new(
        REQ_DIR_IN | REQ_TYPE_CLASS | REQ_RECIP_INTERFACE,
        HID_REQ_GET_REPORT,
        value,
        u16::from(interface),
        buf.len() as u16,
    );
    host.control_transfer(addr, ep0_mps, &setup, ControlData::In(buf))
}
