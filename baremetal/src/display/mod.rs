//! Display + framebuffer for the Pi Zero 2 W.
//!
//! Built on top of the VC mailbox client in `src/mailbox.rs`. The
//! framebuffer is allocated by firmware; we get back a base address
//! and a row pitch and write pixels into it. HDMI scaling produces
//! the actual output.
//!
//! See `docs/REAL_HW_BRINGUP.md` Phase 4 for the surrounding plan.

pub mod fb;
#[cfg(feature = "fb-probe")]
pub mod probe;
#[cfg(nh_host_io_pi_fb)]
pub mod splash;
