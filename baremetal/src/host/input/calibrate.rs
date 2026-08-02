//! Map TSTP MTouch USB touchscreen panel coords → Newton screen coords.
//!
//! The MTouch always reports in its internal 1024×600 logical
//! coordinate space regardless of the HDMI mode (see
//! `docs/MTOUCH.md` §Coordinate space). The touch surface is
//! physically coincident with the HDMI display surface, so touch
//! (0..1024, 0..600) maps linearly across the panel's display area
//! at whatever resolution we drive HDMI. Inside that area the
//! painted Newton region is described by the active host-IO
//! backend's `host_io::painted_region()` report — with a backend
//! that owns no physical panel (null, semihost) the report is
//! `None` and every touch is dropped, so `input-mtouch` no longer
//! requires the `host-io-pi-fb` backend at build time.

use crate::host::host_io;

const TOUCH_W: u32 = 1024;
const TOUCH_H: u32 = 600;

/// Map a raw touch sample to a Newton screen pixel. Returns `None`
/// when the backend reports no painted panel, or when the touch falls
/// outside the painted Newton region (i.e. in a black margin pixel
/// left over from `panel - newton*scale` rounding).
pub fn panel_to_newton(touch_x: u16, touch_y: u16) -> Option<(u16, u16)> {
    let region = host_io::painted_region()?;
    if region.painted_w == 0 || region.painted_h == 0 {
        return None;
    }
    let (newton_w, newton_h) = host_io::panel_geometry()?;
    if newton_w == 0 || newton_h == 0 {
        return None;
    }

    // Touch logical → panel pixel (linear; touch surface is
    // physically coincident with the panel display area).
    let panel_x = (touch_x as u32) * region.panel_w / TOUCH_W;
    let panel_y = (touch_y as u32) * region.panel_h / TOUCH_H;
    let px = panel_x as usize;
    let py = panel_y as usize;
    let offset_x = region.offset_x as usize;
    let offset_y = region.offset_y as usize;
    if px < offset_x || py < offset_y {
        return None;
    }
    let in_x = (px - offset_x) as u32;
    let in_y = (py - offset_y) as u32;
    if in_x >= region.painted_w || in_y >= region.painted_h {
        return None;
    }

    // Painted pixel → Newton pixel, inverting the bilinear scale
    // (linear map; the painted region is `newton × painted/newton`
    // in each axis).
    let nx = (in_x * newton_w / region.painted_w).min(newton_w - 1);
    let ny = (in_y * newton_h / region.painted_h).min(newton_h - 1);
    Some((nx as u16, ny as u16))
}
