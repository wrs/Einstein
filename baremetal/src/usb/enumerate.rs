//! USB bus enumeration: bring a freshly-reset device through the
//! standard control-stage sequence and end up with a configured
//! device whose interfaces + endpoints we've cached.
//!
//! Sequence (USB 2.0 §9.1.2):
//!
//!   1. GET_DESCRIPTOR(Device, 8) at address 0, EP0 wMaxPacketSize=8.
//!      Extract real `bMaxPacketSize0`.
//!   2. SET_ADDRESS(1).
//!   3. GET_DESCRIPTOR(Device, 18) at address 1, real MPS0.
//!   4. GET_DESCRIPTOR(Configuration, 9) at address 1 → read
//!      `wTotalLength`.
//!   5. GET_DESCRIPTOR(Configuration, wTotalLength) → walk
//!      interface + endpoint records.
//!   6. SET_CONFIGURATION(bConfigurationValue).
//!
//! The MTouch panel has exactly one configuration, two interfaces,
//! and two interrupt endpoints we care about (interface 0's
//! IN+OUT pair); we surface enough of that in `UsbDevice` for the
//! dispatcher to match by VID/PID and the driver to find its IN ep.

pub use super::descriptor::EndpointDescriptor;
use super::descriptor::{
    walk_config, ConfigDescriptor, ConfigItem, DeviceDescriptor,
};
use super::host::{ControlData, UsbHostController};
use super::{
    SetupPacket, UsbError, UsbResult, DESC_CONFIGURATION, DESC_DEVICE, REQ_DIR_IN, REQ_DIR_OUT,
    REQ_GET_DESCRIPTOR, REQ_RECIP_DEVICE, REQ_SET_ADDRESS, REQ_SET_CONFIGURATION,
    REQ_TYPE_STANDARD,
};

/// Cached state of an attached, fully-enumerated USB device. Holds
/// only what the dispatcher and the MTouch driver need; we don't
/// keep the full descriptor blob around once parsing is done.
pub struct UsbDevice {
    pub address: u8,
    pub device: DeviceDescriptor,
    pub endpoints: [Option<EndpointEntry>; MAX_ENDPOINTS],
}

#[derive(Copy, Clone, Debug)]
pub struct EndpointEntry {
    pub interface_number: u8,
    pub ep: EndpointDescriptor,
}

pub const MAX_ENDPOINTS: usize = 8;

impl UsbDevice {
    pub fn vendor_id(&self) -> u16 {
        self.device.vendor_id
    }
    pub fn product_id(&self) -> u16 {
        self.device.product_id
    }

    /// First IN endpoint on the given interface, if any.
    pub fn first_in_endpoint(&self, interface: u8) -> Option<EndpointDescriptor> {
        for entry in self.endpoints.iter().flatten() {
            if entry.interface_number == interface && entry.ep.is_in() {
                return Some(entry.ep);
            }
        }
        None
    }
}

/// Walk a freshly-reset device through the standard enumeration
/// sequence and return its configured `UsbDevice`.
pub fn enumerate<H: UsbHostController>(host: &mut H) -> UsbResult<UsbDevice> {
    // Stage 1: short device descriptor at address 0 to learn EP0 MPS.
    let mut short = [0u8; 8];
    control_in(
        host,
        0,
        8,
        REQ_GET_DESCRIPTOR,
        u16::from(DESC_DEVICE) << 8,
        0,
        &mut short,
    )?;
    let ep0_mps = short[7].max(8);

    // Stage 2: SET_ADDRESS(1).
    let setup = SetupPacket::new(
        REQ_DIR_OUT | REQ_TYPE_STANDARD | REQ_RECIP_DEVICE,
        REQ_SET_ADDRESS,
        1,
        0,
        0,
    );
    host.control_transfer(0, ep0_mps, &setup, ControlData::None)?;
    // USB 2.0 §9.2.6.3 tDSETADDR: device has up to 50 ms to commit
    // the new address after the STATUS stage. Without this delay
    // the next SETUP at addr=1 hits XACT_ERR — the device is still
    // listening at addr=0. Matches Circle's `SetAddress` post-delay.
    crate::cpu::delay_ms(50);

    // Stage 3: full device descriptor at the new address.
    let mut full = [0u8; 18];
    control_in(
        host,
        1,
        ep0_mps,
        REQ_GET_DESCRIPTOR,
        u16::from(DESC_DEVICE) << 8,
        0,
        &mut full,
    )?;
    let device = DeviceDescriptor::parse(&full).ok_or(UsbError::Other)?;

    // Stage 4: config descriptor header only (9 bytes).
    let mut cfg_head = [0u8; 9];
    control_in(
        host,
        1,
        ep0_mps,
        REQ_GET_DESCRIPTOR,
        u16::from(DESC_CONFIGURATION) << 8,
        0,
        &mut cfg_head,
    )?;
    let config = ConfigDescriptor::parse(&cfg_head).ok_or(UsbError::Other)?;

    // Stage 5: full configuration tree. The MTouch panel reports
    // total_length ~ 100 bytes; we cap at 256 to be safe without
    // alloc.
    let mut cfg_buf = [0u8; 256];
    let want = (config.total_length as usize).min(cfg_buf.len());
    let got = control_in(
        host,
        1,
        ep0_mps,
        REQ_GET_DESCRIPTOR,
        u16::from(DESC_CONFIGURATION) << 8,
        0,
        &mut cfg_buf[..want],
    )?;

    let mut endpoints: [Option<EndpointEntry>; MAX_ENDPOINTS] = Default::default();
    let mut current_iface: u8 = 0;
    let mut ep_idx = 0usize;
    walk_config(&cfg_buf[..got], |item| match item {
        ConfigItem::Interface { interface_number } => {
            current_iface = interface_number;
        }
        ConfigItem::Endpoint(ep) => {
            if ep_idx < MAX_ENDPOINTS {
                endpoints[ep_idx] = Some(EndpointEntry {
                    interface_number: current_iface,
                    ep,
                });
                ep_idx += 1;
            }
        }
    });

    // Stage 6: SET_CONFIGURATION.
    let setup = SetupPacket::new(
        REQ_DIR_OUT | REQ_TYPE_STANDARD | REQ_RECIP_DEVICE,
        REQ_SET_CONFIGURATION,
        u16::from(config.configuration_value),
        0,
        0,
    );
    host.control_transfer(1, ep0_mps, &setup, ControlData::None)?;
    // Devices need time to commit the configuration (start the
    // EP machinery, enable interfaces). 50 ms matches Circle.
    crate::cpu::delay_ms(50);

    Ok(UsbDevice {
        address: 1,
        device,
        endpoints,
    })
}

/// Helper: standard GET_DESCRIPTOR-shape IN control transfer.
fn control_in<H: UsbHostController>(
    host: &mut H,
    addr: u8,
    ep0_mps: u8,
    request: u8,
    value: u16,
    index: u16,
    buf: &mut [u8],
) -> UsbResult<usize> {
    let setup = SetupPacket::new(
        REQ_DIR_IN | REQ_TYPE_STANDARD | REQ_RECIP_DEVICE,
        request,
        value,
        index,
        buf.len() as u16,
    );
    host.control_transfer(addr, ep0_mps, &setup, ControlData::In(buf))
}
