//! Map TSTP MTouch USB touchscreen panel coords → Newton screen coords.
//!
//! The MTouch always reports in its internal 1024×600 logical
//! coordinate space regardless of the HDMI mode (see
//! `docs/MTOUCH.md` §Coordinate space). The touch surface is
//! physically coincident with the HDMI display surface, so touch
//! (0..1024, 0..600) maps linearly across the panel's display area
//! at whatever resolution we drive HDMI. Inside that area the
//! painted Newton region starts at `host_io::pi_fb::painted_offset`
//! and each Newton pixel covers `paint_scale × paint_scale` panel
//! pixels.

use crate::host_io::pi_fb;
use crate::peripherals::screen;

const TOUCH_W: u32 = 1024;
const TOUCH_H: u32 = 600;

/// Map a raw touch sample to a Newton screen pixel. Returns `None`
/// when the touch falls outside the painted Newton region (i.e. in
/// a black margin pixel left over from `panel - newton*scale`
/// rounding).
pub fn panel_to_newton(touch_x: u16, touch_y: u16) -> Option<(u16, u16)> {
    let (panel_w, panel_h) = pi_fb::panel_size()?;
    let (offset_x, offset_y) = pi_fb::painted_offset();
    let (painted_w, painted_h) = pi_fb::painted_size();
    if painted_w == 0 || painted_h == 0 {
        return None;
    }
    let newton_w = screen::screen_width();
    let newton_h = screen::screen_height();
    if newton_w == 0 || newton_h == 0 {
        return None;
    }

    // Touch logical → panel pixel (linear; touch surface is
    // physically coincident with the panel display area).
    let panel_x = (touch_x as u32) * panel_w / TOUCH_W;
    let panel_y = (touch_y as u32) * panel_h / TOUCH_H;
    let px = panel_x as usize;
    let py = panel_y as usize;
    if px < offset_x || py < offset_y {
        return None;
    }
    let in_x = (px - offset_x) as u32;
    let in_y = (py - offset_y) as u32;
    if in_x >= painted_w || in_y >= painted_h {
        return None;
    }

    // Painted pixel → Newton pixel, inverting the bilinear scale
    // (linear map; the painted region is `newton × painted/newton`
    // in each axis).
    let nx = (in_x * newton_w / painted_w).min(newton_w - 1);
    let ny = (in_y * newton_h / painted_h).min(newton_h - 1);
    Some((nx as u16, ny as u16))
}
