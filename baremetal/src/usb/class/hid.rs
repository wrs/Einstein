//! HID class control requests.
//!
//! HID 1.11 §7. We only need the operations the MTouch driver
//! issues: SET_IDLE, GET_REPORT(type, id), GET_DESCRIPTOR(Report).
//! Helpers are free functions over `UsbHostController` — there's
//! one HID class on the only device we'll ever drive, so a class
//! trait would be overengineering.

use super::super::host::{ControlData, UsbHostController};
use super::super::{
    SetupPacket, UsbResult, DESC_HID_REPORT, REQ_DIR_IN, REQ_DIR_OUT, REQ_GET_DESCRIPTOR,
    REQ_RECIP_INTERFACE, REQ_TYPE_CLASS, REQ_TYPE_STANDARD,
};

/// HID class-specific request codes — HID 1.11 §7.2.
pub const HID_REQ_GET_REPORT: u8 = 0x01;
pub const HID_REQ_GET_IDLE: u8 = 0x02;
pub const HID_REQ_GET_PROTOCOL: u8 = 0x03;
pub const HID_REQ_SET_REPORT: u8 = 0x09;
pub const HID_REQ_SET_IDLE: u8 = 0x0A;
pub const HID_REQ_SET_PROTOCOL: u8 = 0x0B;

/// Report type byte that rides in wValue's high byte.
pub const HID_REPORT_INPUT: u8 = 0x01;
pub const HID_REPORT_OUTPUT: u8 = 0x02;
pub const HID_REPORT_FEATURE: u8 = 0x03;

/// `SET_IDLE` — set idle rate to `duration` × 4 ms (HID 1.11 §7.2.4).
/// `duration=0` means "report only on change", which is what we want
/// for the touchscreen.
pub fn set_idle<H: UsbHostController>(
    host: &mut H,
    addr: u8,
    ep0_mps: u8,
    interface: u8,
    duration: u8,
    report_id: u8,
) -> UsbResult<()> {
    let value = (u16::from(duration) << 8) | u16::from(report_id);
    let setup = SetupPacket::new(
        REQ_DIR_OUT | REQ_TYPE_CLASS | REQ_RECIP_INTERFACE,
        HID_REQ_SET_IDLE,
        value,
        u16::from(interface),
        0,
    );
    host.control_transfer(addr, ep0_mps, &setup, ControlData::None)?;
    Ok(())
}

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

/// `GET_DESCRIPTOR(Report)` — fetch the device's HID report
/// descriptor. Sent as a standard GET_DESCRIPTOR with descriptor
/// type 0x22 (REPORT). HID 1.11 §7.1.1.
#[allow(dead_code)]
pub fn get_report_descriptor<H: UsbHostController>(
    host: &mut H,
    addr: u8,
    ep0_mps: u8,
    interface: u8,
    buf: &mut [u8],
) -> UsbResult<usize> {
    let setup = SetupPacket::new(
        REQ_DIR_IN | REQ_TYPE_STANDARD | REQ_RECIP_INTERFACE,
        REQ_GET_DESCRIPTOR,
        u16::from(DESC_HID_REPORT) << 8,
        u16::from(interface),
        buf.len() as u16,
    );
    host.control_transfer(addr, ep0_mps, &setup, ControlData::In(buf))
}
