//! USB descriptor decode helpers.
//!
//! Only the fields we actually consume are exposed — we never need a
//! generic descriptor parser. USB 2.0 §9.5 / §9.6.

use super::{DESC_ENDPOINT, DESC_HID, DESC_INTERFACE};

/// Parsed device descriptor (USB 2.0 §9.6.1, 18 bytes).
#[derive(Copy, Clone, Debug, Default)]
pub struct DeviceDescriptor {
    pub usb_release: u16,    // bcdUSB
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_release: u16, // bcdDevice
    pub num_configurations: u8,
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
            usb_release: u16::from_le_bytes([buf[2], buf[3]]),
            device_class: buf[4],
            device_subclass: buf[5],
            device_protocol: buf[6],
            max_packet_size0: buf[7],
            vendor_id: u16::from_le_bytes([buf[8], buf[9]]),
            product_id: u16::from_le_bytes([buf[10], buf[11]]),
            device_release: u16::from_le_bytes([buf[12], buf[13]]),
            num_configurations: buf[17],
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
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub attributes: u8,
    pub max_power_2ma: u8,
}

impl ConfigDescriptor {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 9 || buf[0] != 9 || buf[1] != 2 {
            return None;
        }
        Some(Self {
            total_length: u16::from_le_bytes([buf[2], buf[3]]),
            num_interfaces: buf[4],
            configuration_value: buf[5],
            attributes: buf[7],
            max_power_2ma: buf[8],
        })
    }
}

/// Parsed interface descriptor (USB 2.0 §9.6.5, 9 bytes).
#[derive(Copy, Clone, Debug, Default)]
pub struct InterfaceDescriptor {
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub num_endpoints: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// Parsed endpoint descriptor (USB 2.0 §9.6.6, 7 bytes).
#[derive(Copy, Clone, Debug, Default)]
pub struct EndpointDescriptor {
    pub address: u8,       // bEndpointAddress (bit 7 = IN/OUT)
    pub attributes: u8,    // bmAttributes (bits 1:0 = transfer type)
    pub max_packet_size: u16,
    pub interval_ms: u8,   // bInterval
}

impl EndpointDescriptor {
    pub fn is_in(self) -> bool {
        (self.address & 0x80) != 0
    }
    pub fn ep_num(self) -> u8 {
        self.address & 0x0F
    }
    pub fn transfer_type(self) -> super::EndpointType {
        match self.attributes & 0x03 {
            0 => super::EndpointType::Control,
            1 => super::EndpointType::Isochronous,
            2 => super::EndpointType::Bulk,
            3 => super::EndpointType::Interrupt,
            _ => unreachable!(),
        }
    }
}

/// One descriptor inside a configuration response.
#[derive(Copy, Clone, Debug)]
pub enum ConfigItem<'a> {
    Interface(InterfaceDescriptor),
    Endpoint(EndpointDescriptor),
    Hid {
        bcd_hid: u16,
        report_descriptor_length: u16,
    },
    /// Vendor-specific or class-specific descriptor we don't decode.
    Other { kind: u8, body: &'a [u8] },
}

/// Iterate over the descriptor list returned by
/// GET_DESCRIPTOR(Configuration, full). The first 9 bytes are the
/// configuration header itself (caller has already parsed via
/// `ConfigDescriptor::parse`); we walk the trailing tagged-length
/// records.
///
/// `visit` receives each descriptor in wire order. Returns the
/// number of descriptors visited.
pub fn walk_config<F: FnMut(ConfigItem<'_>)>(buf: &[u8], mut visit: F) -> usize {
    if buf.len() < 9 {
        return 0;
    }
    let total = ConfigDescriptor::parse(buf)
        .map(|c| c.total_length as usize)
        .unwrap_or(buf.len());
    let end = total.min(buf.len());
    let mut p = 9usize;
    let mut count = 0;
    while p + 2 <= end {
        let len = buf[p] as usize;
        let kind = buf[p + 1];
        if len < 2 || p + len > end {
            return count;
        }
        let body = &buf[p..p + len];
        let item = match (kind, len) {
            (DESC_INTERFACE, 9) => ConfigItem::Interface(InterfaceDescriptor {
                interface_number: body[2],
                alternate_setting: body[3],
                num_endpoints: body[4],
                class: body[5],
                subclass: body[6],
                protocol: body[7],
            }),
            (DESC_ENDPOINT, l) if l >= 7 => ConfigItem::Endpoint(EndpointDescriptor {
                address: body[2],
                attributes: body[3],
                max_packet_size: u16::from_le_bytes([body[4], body[5]]),
                interval_ms: body[6],
            }),
            (DESC_HID, l) if l >= 9 => {
                // HID class descriptor (HID 1.11 §6.2.1). Bytes:
                //   [2..4]  bcdHID
                //   [4]     bCountryCode
                //   [5]     bNumDescriptors
                //   [6]     bDescriptorType (typically 0x22 = Report)
                //   [7..9]  wDescriptorLength of that Report descriptor
                ConfigItem::Hid {
                    bcd_hid: u16::from_le_bytes([body[2], body[3]]),
                    report_descriptor_length: u16::from_le_bytes([body[7], body[8]]),
                }
            }
            _ => ConfigItem::Other { kind, body: &body[2..] },
        };
        visit(item);
        count += 1;
        p += len;
    }
    count
}
