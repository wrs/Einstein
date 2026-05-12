//! Map touchscreen panel coordinates → Newton screen coordinates.
//!
//! Phase 4 paints Newton's 320×480 portrait FB scaled 1.5× to
//! 480×720 centred on a 1280×720 HDMI panel (400 px black bars on
//! each side). The TSTP MTouch panel reports raw touch coordinates
//! in its native 1024×600 logical space, which maps physically onto
//! the full HDMI image (the panel's touch surface and display
//! surface are coincident — see `docs/MTOUCH.md` §Coordinate space).
//!
//! Inverse transform:
//!
//! ```text
//!   panel pixel ratio:  panel_w_px = 1280, panel_h_px = 720
//!   touch logical:      tx_max = 1024, ty_max = 600
//!   newton scaled:      newt_w_px = 480, newt_h_px = 720
//!                       newt_offset_x_px = (1280 - 480) / 2 = 400
//!   touch units per panel px (X) = 1024 / 1280 = 0.8
//!   touch units per panel px (Y) =  600 /  720 ≈ 0.833...
//!   left-band  : tx < newt_offset_x_px * 0.8       (=  320 touch)
//!   right-band : tx > (newt_offset_x_px+newt_w_px) * 0.8
//!                                                  (=  704 touch)
//!   in-region  : 320 <= tx <= 704
//!   newton_x   = ((tx - 320) * 320) / 384        // 0..319
//!   newton_y   = (ty * 480) / 600                // 0..479
//! ```
//!
//! All arithmetic stays in `u32` to avoid sign-handling subtleties.

pub const PANEL_W_PX: u32 = 1280;
pub const PANEL_H_PX: u32 = 720;
pub const TOUCH_W: u32 = 1024;
pub const TOUCH_H: u32 = 600;
pub const NEWTON_W_PX: u32 = 480; // 320 * 3 / 2
pub const NEWTON_H_PX: u32 = 720; // 480 * 3 / 2
pub const NEWTON_OFFSET_X_PX: u32 = (PANEL_W_PX - NEWTON_W_PX) / 2; // 400

/// Left edge of the Newton-painted region, expressed in panel
/// touch units (0..1024).
pub const LEFT_BAND_END: u32 = NEWTON_OFFSET_X_PX * TOUCH_W / PANEL_W_PX; // 320
/// Right edge of the Newton-painted region, in touch units.
pub const RIGHT_BAND_START: u32 =
    (NEWTON_OFFSET_X_PX + NEWTON_W_PX) * TOUCH_W / PANEL_W_PX; // 704

pub const NEWTON_SCREEN_W: u16 = 320;
pub const NEWTON_SCREEN_H: u16 = 480;

/// Map a raw panel touch (x in 0..1024, y in 0..600) to Newton
/// screen coordinates (x in 0..319, y in 0..479). Returns `None`
/// when the touch falls in the left or right black letterbox band
/// outside the Newton image.
pub const fn panel_to_newton(touch_x: u16, touch_y: u16) -> Option<(u16, u16)> {
    let tx = touch_x as u32;
    let ty = touch_y as u32;
    if tx < LEFT_BAND_END {
        return None;
    }
    if tx >= RIGHT_BAND_START {
        return None;
    }
    // Width inside the Newton region, in touch units.
    let in_x = tx - LEFT_BAND_END;
    let in_x_max = RIGHT_BAND_START - LEFT_BAND_END;
    let nx = (in_x * NEWTON_SCREEN_W as u32) / in_x_max;
    let ny = (ty * NEWTON_SCREEN_H as u32) / TOUCH_H;
    let nx = if nx >= NEWTON_SCREEN_W as u32 {
        NEWTON_SCREEN_W as u32 - 1
    } else {
        nx
    };
    let ny = if ny >= NEWTON_SCREEN_H as u32 {
        NEWTON_SCREEN_H as u32 - 1
    } else {
        ny
    };
    Some((nx as u16, ny as u16))
}

// Compile-time spot checks against four representative touches.
const _: () = {
    // Far-left letterbox (~0 panel px) — drops.
    assert!(panel_to_newton(0, 300).is_none());
    // Just inside left edge of Newton region — should yield x≈0.
    let (x_l, _) = match panel_to_newton(LEFT_BAND_END as u16, 0) {
        Some(p) => p,
        None => panic!("left edge must map"),
    };
    assert!(x_l == 0);
    // Centre touch.
    let cx = (LEFT_BAND_END + RIGHT_BAND_START) / 2;
    let (x_c, y_c) = match panel_to_newton(cx as u16, (TOUCH_H / 2) as u16) {
        Some(p) => p,
        None => panic!("centre must map"),
    };
    assert!(x_c >= NEWTON_SCREEN_W / 2 - 2 && x_c <= NEWTON_SCREEN_W / 2 + 2);
    assert!(y_c >= NEWTON_SCREEN_H / 2 - 2 && y_c <= NEWTON_SCREEN_H / 2 + 2);
    // Far-right letterbox — drops.
    assert!(panel_to_newton((TOUCH_W - 1) as u16, 300).is_none());
};
