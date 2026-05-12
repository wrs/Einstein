//! Pi VC framebuffer host_io backend.
//!
//! Renders Newton's 320 × 480 2 bpp grayscale framebuffer into the
//! HDMI panel via the allocator built in `src/display/`. Nearest-
//! neighbor 1.5× scale → 480 × 720 painted onto the configured
//! panel (e.g. 1280 × 720 → centred horizontally with black bars
//! on either side).
//!
//! The scale factor is chosen because:
//! - 720 / 480 = 1.5 exactly, so vertical fills the panel cleanly
//!   at 720p output.
//! - 320 × 1.5 = 480 horizontally, leaving 1280 - 480 = 800 px
//!   total for symmetric black bars (400 px each side).
//! - Aspect preserved — Newton's 2:3 portrait stays portrait.
//!
//! Each `push_blit` is a partial repaint: we touch only the panel
//! pixels covered by `ev.dst_*` (scaled), then `dc_civac_range`
//! over just those rows. The full-repaint blit issued by
//! `host_io::on_resume` covers the whole Newton FB so the whole
//! centred region gets refreshed.
//!
//! Pen input isn't wired up yet — `pump_input` is a no-op.

use core::sync::atomic::{AtomicBool, Ordering};

use super::BlitEvent;
use crate::display::fb::{self, FbInfo};
use crate::kprintln;
use crate::peripherals::screen::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// 2 bpp → 32 bpp lookup. Newton convention: 0 = white, 3 = black,
/// intermediate values are linear grays. Channel packing matches
/// `fb_set_pixel_order(1)` (RGB): byte 0 = R, byte 1 = G, byte 2 = B,
/// byte 3 = pad. Stored as u32 in native little-endian.
const PALETTE: [u32; 4] = [
    u32::from_le_bytes([0xFF, 0xFF, 0xFF, 0x00]), // 0: white
    u32::from_le_bytes([0xAA, 0xAA, 0xAA, 0x00]), // 1: light gray
    u32::from_le_bytes([0x55, 0x55, 0x55, 0x00]), // 2: dark gray
    u32::from_le_bytes([0x00, 0x00, 0x00, 0x00]), // 3: black
];

/// Scale Newton coordinates → panel coordinates. 3/2 with truncating
/// division — produces a 1 or 2-pixel block per Newton pixel, the
/// pattern alternating in a way that averages to the correct
/// position.
#[inline]
const fn scale(n: usize) -> usize {
    (n * 3) / 2
}

static INIT_DONE: AtomicBool = AtomicBool::new(false);
/// FbInfo captured from `display::fb::alloc_native`. `static mut` is
/// safe because we're single-core EL2 and `INIT_DONE` gates access.
static mut FB: Option<FbInfo> = None;
/// Horizontal offset (in panel pixels) where Newton's scaled FB
/// starts. Vertical offset is 0 (we always start at row 0).
static mut PANEL_OFFSET_X: usize = 0;

#[allow(static_mut_refs)]
fn fb() -> Option<&'static FbInfo> {
    if !INIT_DONE.load(Ordering::Relaxed) {
        return None;
    }
    // SAFETY: INIT_DONE is set only by `init` on core 0 before any
    // push_blit runs; subsequent reads see a fully written FbInfo.
    // Single-core EL2.
    unsafe { FB.as_ref() }
}

#[allow(static_mut_refs)]
fn panel_offset_x() -> usize {
    // SAFETY: same as fb().
    unsafe { PANEL_OFFSET_X }
}

pub fn init() {
    let info = match fb::alloc_native() {
        Ok(i) => i,
        Err(e) => {
            kprintln!("host_io_pi_fb: FB init FAILED: {:?}", e);
            return;
        }
    };
    // Clear to black (Newton background once it's drawing will be
    // white, but on a cold boot we want a defined initial state
    // rather than whatever firmware leftover happens to be there).
    fb::fill_solid(&info, PALETTE[3]);

    // Center the scaled Newton FB on the panel.
    let scaled_newton_w = scale(SCREEN_WIDTH as usize);
    let panel_w = info.width as usize;
    let offset_x = panel_w.saturating_sub(scaled_newton_w) / 2;

    // SAFETY: single-core EL2, called once from kmain before any
    // other code touches FB / PANEL_OFFSET_X.
    unsafe {
        #[allow(static_mut_refs)]
        {
            FB = Some(info);
            PANEL_OFFSET_X = offset_x;
        }
    }
    INIT_DONE.store(true, Ordering::Relaxed);
    kprintln!(
        "host_io_pi_fb: ready ({}x{} @ pa=0x{:x}, newton {}x{} -> {}x{} centred at x={})",
        info.width,
        info.height,
        info.pa,
        SCREEN_WIDTH,
        SCREEN_HEIGHT,
        scaled_newton_w,
        scale(SCREEN_HEIGHT as usize),
        offset_x,
    );
}

pub fn on_resume() {
    // The host_io::on_resume layer pushes a full-repaint blit ahead
    // of calling backend on_resume; we don't need to do anything
    // here. (No host-side state of our own to reset.)
}

pub fn push_blit(ev: &BlitEvent, payload: &[u8]) {
    let Some(fb) = fb() else {
        return;
    };

    let src_w = ev.src_right.saturating_sub(ev.src_left) as usize;
    let src_h = ev.src_bottom.saturating_sub(ev.src_top) as usize;
    let dst_left = ev.dst_left as usize;
    let dst_top = ev.dst_top as usize;
    let dst_w = ev.dst_right.saturating_sub(ev.dst_left) as usize;
    let dst_h = ev.dst_bottom.saturating_sub(ev.dst_top) as usize;
    let row_bytes = ev.row_bytes as usize;

    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return;
    }

    let pixels_per_row = (fb.pitch / 4) as usize;
    let ptr = fb.pa as *mut u32;
    let offset_x = panel_offset_x();

    // Nearest-neighbor sample from src into dst when sizes differ.
    // Same-size blits (the common case for srcCopy paint) take the
    // fast path of sx == dx, sy == dy.
    let same_size = src_w == dst_w && src_h == dst_h;

    for dy in 0..dst_h {
        let sy = if same_size {
            dy
        } else {
            (dy * src_h) / dst_h.max(1)
        };
        let row_base = sy * row_bytes;
        let row = match payload.get(row_base..row_base + row_bytes) {
            Some(r) => r,
            None => return, // malformed payload; bail
        };
        let newton_y = dst_top + dy;
        let panel_y0 = scale(newton_y);
        let panel_y1 = scale(newton_y + 1);

        for dx in 0..dst_w {
            let sx = if same_size {
                dx
            } else {
                (dx * src_w) / dst_w.max(1)
            };
            // 2 bpp MSB-first: pixel n in bits (6 - 2*(n%4))..(8 - 2*(n%4))
            // of byte n/4.
            let byte_idx = sx / 4;
            let shift = 6 - 2 * ((sx % 4) as u32);
            let byte = match row.get(byte_idx) {
                Some(&b) => b,
                None => continue,
            };
            let pixel = ((byte >> shift) & 0x3) as usize;
            let color = PALETTE[pixel];

            let newton_x = dst_left + dx;
            let panel_x0 = offset_x + scale(newton_x);
            let panel_x1 = offset_x + scale(newton_x + 1);

            for py in panel_y0..panel_y1 {
                for px in panel_x0..panel_x1 {
                    // SAFETY: panel coordinates lie within the
                    // allocated framebuffer (Newton 320x480 scales to
                    // 480x720; for any panel >= 480x720 the writes
                    // are in-bounds). pixels_per_row is fb.pitch/4.
                    unsafe {
                        ptr.add(py * pixels_per_row + px).write_volatile(color);
                    }
                }
            }
        }
    }

    // Flush the rows we touched to RAM so the VC scan picks them up.
    // Range: from the top of the scaled dst rect to the bottom (one
    // extra row to cover the case where scale(top+h) rounds up by 1).
    let flush_y0 = scale(dst_top);
    let flush_y1 = scale(dst_top + dst_h).min(fb.height as usize);
    let row_bytes_panel = pixels_per_row * 4;
    let flush_pa = fb.pa.wrapping_add((flush_y0 * row_bytes_panel) as u64);
    let flush_len = (flush_y1 - flush_y0) * row_bytes_panel;
    crate::cpu::dc_civac_range(flush_pa, flush_len);
}

pub fn pump_input() {
    // No input source yet. Pen / power-switch wiring is a follow-up
    // (UART tunnel first, USB HID later — per the Phase 5 plan in
    // docs/REAL_HW_BRINGUP.md).
}
