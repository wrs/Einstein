//! Device dispatch: given an enumerated [`UsbDevice`], pick the
//! right driver and call its `attach`.
//!
//! There's only one device driver (MTouch) and there will probably
//! ever only be one or two. A static-dispatch enum is fine — no
//! `Box<dyn>` needed.

use super::enumerate::UsbDevice;
use super::host::UsbHostController;
use super::UsbResult;

/// Driver-matching predicate + entry point. Matching is by
/// `(vendor_id, product_id)` for now; future panels with different
/// VIDs can either add another arm or claim by class/subclass.
pub trait UsbDeviceDriver {
    fn matches(&self, dev: &UsbDevice) -> bool;
    /// Issue any device-specific setup (activation handshake,
    /// SET_IDLE, etc.). Called once after `enumerate` succeeds.
    fn attach<H: UsbHostController>(
        &mut self,
        host: &mut H,
        dev: &UsbDevice,
    ) -> UsbResult<()>;
}
