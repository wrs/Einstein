//! USB descriptor decode helpers.
//!
//! Only the fields we actually consume are exposed — we never need a
//! generic descriptor parser. USB 2.0 §9.5 / §9.6.

use super::{DESC_ENDPOINT, DESC_INTERFACE};

/// Parsed device descriptor (USB 2.0 §9.6.1, 18 bytes). Only the
/// fields the enumeration walk and the VID/PID dispatcher consume.
#[derive(Copy, Clone, Debug, Default)]
pub struct DeviceDescriptor {
    pub max_packet_size0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
}

impl DeviceDescriptor {
    /// Parse the first 18 bytes of a GET_DESCRIPTOR(Device) reply.
    /// Returns None if the buffer is too short or the tag bytes don't
    /// match (bLength = 18, bDescriptorType = 1).
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 18 || buf[0] != 18 || buf[1] != 1 {
            return None;
        }
        Some(Self {
            max_packet_size0: buf[7],
            vendor_id: u16::from_le_bytes([buf[8], buf[9]]),
            product_id: u16::from_le_bytes([buf[10], buf[11]]),
        })
    }
}

/// Parsed configuration descriptor header (USB 2.0 §9.6.3, 9 bytes).
/// The tail of the configuration response is a sequence of
/// interface + endpoint + class-specific descriptors that
/// [`walk_config`] iterates over.
#[derive(Copy, Clone, Debug, Default)]
pub struct ConfigDescriptor {
    pub total_length: u16,
    pub configuration_value: u8,
}

impl ConfigDescriptor {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 9 || buf[0] != 9 || buf[1] != 2 {
            return None;
        }
        Some(Self {
            total_length: u16::from_le_bytes([buf[2], buf[3]]),
            configuration_value: buf[5],
        })
    }
}

/// Parsed endpoint descriptor (USB 2.0 §9.6.6, 7 bytes). Only the
/// address (for IN/OUT + endpoint number) and packet size survive
/// parsing — the MTouch driver needs nothing else.
#[derive(Copy, Clone, Debug, Default)]
pub struct EndpointDescriptor {
    pub address: u8, // bEndpointAddress (bit 7 = IN/OUT)
    pub max_packet_size: u16,
}

impl EndpointDescriptor {
    pub fn is_in(self) -> bool {
        (self.address & 0x80) != 0
    }
}

/// One descriptor inside a configuration response that the
/// enumeration walk cares about. Class-specific and vendor records
/// (HID descriptors etc.) are skipped by [`walk_config`].
#[derive(Copy, Clone, Debug)]
pub enum ConfigItem {
    Interface { interface_number: u8 },
    Endpoint(EndpointDescriptor),
}

/// Iterate over the descriptor list returned by
/// GET_DESCRIPTOR(Configuration, full). The first 9 bytes are the
/// configuration header itself (caller has already parsed via
/// `ConfigDescriptor::parse`); we walk the trailing tagged-length
/// records and surface the interface / endpoint ones.
///
/// `visit` receives each surfaced descriptor in wire order.
pub fn walk_config<F: FnMut(ConfigItem)>(buf: &[u8], mut visit: F) {
    if buf.len() < 9 {
        return;
    }
    let total = ConfigDescriptor::parse(buf)
        .map(|c| c.total_length as usize)
        .unwrap_or(buf.len());
    let end = total.min(buf.len());
    let mut p = 9usize;
    while p + 2 <= end {
        let len = buf[p] as usize;
        let kind = buf[p + 1];
        if len < 2 || p + len > end {
            return;
        }
        let body = &buf[p..p + len];
        match (kind, len) {
            (DESC_INTERFACE, 9) => visit(ConfigItem::Interface {
                interface_number: body[2],
            }),
            (DESC_ENDPOINT, l) if l >= 7 => visit(ConfigItem::Endpoint(EndpointDescriptor {
                address: body[2],
                max_packet_size: u16::from_le_bytes([body[4], body[5]]),
            })),
            // HID class descriptors, strings, vendor records: skipped.
            _ => {}
        }
        p += len;
    }
}
