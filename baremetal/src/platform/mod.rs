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

pub use imp::*;
