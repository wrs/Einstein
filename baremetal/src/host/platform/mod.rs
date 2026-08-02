//! Host-platform constants and hooks.
//!
//! Everything that differs between the QEMU raspi3b / Pi 3B host and the
//! ARM FVP_Base_RevC-2xAEMvA host lives behind this module:
//!
//!   * `UART_BASE`, `UART_CLOCK_HZ` — PL011 address and reference clock.
//!   * `DEVICE_MMIO_RANGE` — the IPA window the MMU must mark Device-nGnRE.
//!   * `NEWTON_TICK_HZ` — the guest-visible Newton tick rate we report
//!     (scaled on QEMU to compensate for its slow CNTPCT, real on FVP).
//!   * `NAME` — banner string.
//!   * `install_cnthp_irq_routing()` — host-specific glue to make the
//!     EL2 physical timer PPI reach the CPU's IRQ line (BCM2836 local
//!     peripheral on raspi3b, GICv3 on FVP).
//!
//! Exactly one of the `platform-*` cargo features is on at a time;
//! build.rs enforces this and also picks the matching linker script.

#[cfg(feature = "platform-raspi3b")]
#[path = "raspi3b.rs"]
mod imp;

#[cfg(feature = "platform-fvp-base")]
#[path = "fvp_base.rs"]
mod imp;

#[cfg(feature = "platform-fvp-base")]
pub mod gicv3;

pub use imp::*;

/// Outcome of the per-IRQ USB interrupt-IN fast path
/// (`poll_usb_fast_path`). Lets the EL2 IRQ entry decide whether the
/// heavy guest-IRQ body can be skipped without the dispatcher knowing
/// anything about the BCM2835 pending registers.
///
/// `UsbOnly` / `UsbCoPending` are constructed only by the `raspi3b.rs`
/// `nh_real_hw` path; off real hardware `poll_usb_fast_path` always
/// returns `NotUsb`, so they read as dead there.
#[allow(dead_code)]
pub enum UsbFastPath {
    /// USB source 9 was not pending — take the normal IRQ path.
    NotUsb,
    /// USB was the *sole* pending source; the heavy body can be
    /// skipped. `enqueued` is true if a pen sample was harvested.
    UsbOnly { enqueued: bool },
    /// USB was pending but other sources (DMA channels / CNTHP) are
    /// co-pending — harvest done, but still take the normal path.
    UsbCoPending,
}
