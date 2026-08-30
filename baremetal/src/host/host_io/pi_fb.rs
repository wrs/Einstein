//! Pi VC framebuffer host_io backend.
//!
//! Renders Newton's 2 bpp grayscale framebuffer onto the HDMI panel
//! allocated by `src/host/display/`. Newton's screen geometry is locked
//! to the MP2100 native 320×480 portrait — the OS-layer accepts
//! other sizes but the *ROM* has portrait-320-wide constants baked
//! into animation-erase code and various view-bounds tables, so
//! anything wider leaves the trash-can animation, view-shrink
//! transitions, etc. with debris past the OS-believed left half of
//! the screen.
//!
//! Newton 320×480 is scaled with **software bilinear** to an
//! aspect-preserving rectangle that fills one axis of the panel
//! (typically the height — the panel is wider than 2:3). Bilinear
//! sampling gives us smooth non-integer scaling without going
//! through the panel's own scaler, which produces visibly-bad
//! resampling on cheap HDMI monitors. Compared to the HVS scaler:
//! cheaper to bring up (no DispmanX from bare metal), comparable
//! quality at our pixel counts; the cost is ~30 ms for a full-
//! screen repaint at 512×768, which only happens on resume.
//!
//! On each `push_blit` we recompute only the panel rect affected
//! by the dst Newton rect (forward-mapped through the scale), then
//! bilinear-sample from `guest_mem::fb_host_pa()` — by the time
//! `push_blit` runs, `peripherals::screen::blit` has already
//! written the new 2 bpp pixels into `GUEST_FB`, so reading from
//! there picks up the latest content (and naturally blends with
//! the existing surround at the edges of a partial blit).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::BlitEvent;
use crate::host::display::fb::FbInfo;
use crate::log_host_io;

/// Newton's screen dimensions, pinned to MP2100 native 320×480 to
/// avoid ROM landscape-mode quirks. The OS-layer would accept other
/// sizes; the ROM does not. Reported to `peripherals::screen` through
/// [`super::HostIo::panel_geometry`] (pulled by `main.rs` at boot).
const NEWTON_W: u32 = 320;
const NEWTON_H: u32 = 480;
/// 2 bpp grayscale — the MP2x00 panel depth `peripherals::screen`
/// models.
const NEWTON_BPP: u32 = 2;

pub struct PiFbBackend;

impl super::HostIo for PiFbBackend {
    fn init(&self) {
        init()
    }
    fn on_resume(&self) {
        // Repaint the panel from the restored GUEST_FB. No host-side
        // state of our own to reset beyond that.
        super::push_full_repaint(NEWTON_W, NEWTON_H, NEWTON_BPP);
    }
    fn push_blit(&self, ev: &super::BlitEvent, payload: &[u8]) {
        push_blit(ev, payload)
    }
    fn pump_input(&self) {
        // No input source on this backend directly; see `input::mtouch`.
    }
    fn panel_geometry(&self) -> Option<(u32, u32)> {
        // The pin is a compile-time property of this backend, not an
        // outcome of panel bring-up — report it unconditionally.
        Some((NEWTON_W, NEWTON_H))
    }
    #[cfg(nh_input_mtouch)]
    fn painted_region(&self) -> Option<super::PaintedRegion> {
        let f = fb()?;
        Some(super::PaintedRegion {
            panel_w: f.width,
            panel_h: f.height,
            offset_x: OFFSET_X.load(Ordering::Relaxed),
            offset_y: OFFSET_Y.load(Ordering::Relaxed),
            painted_w: PAINTED_W.load(Ordering::Relaxed),
            painted_h: PAINTED_H.load(Ordering::Relaxed),
        })
    }
}

pub static BACKEND: PiFbBackend = PiFbBackend;

/// Panel rows the Pi firmware reserves at the top of the scan-out
/// region for its own loader UI — observed as a persistent thin
/// white bar across the top of the picture. Survives both the
/// cut-down (`start_cd.elf`) and full (`start.elf`) firmware, and
/// `disable_splash=1`. Raspbian clears the same band only when KMS
/// loads and reconfigures the CRTC end-to-end (DispmanX + direct
/// VC4 register access); we can't do that from this layer without
/// bringing up VCHIQ.
///
/// Workaround: pretend the panel is `info.height - FIRMWARE_TOP_BAR_PX`
/// rows tall when picking the painted region. The painted region
/// then fits below the bar with no off-screen clipping. The bar
/// stays visible, but the Newton image is fully on-screen and
/// vertically centered inside the visible area.
///
/// Tune up if the Newton image still clips at the bottom, down if
/// a black gap appears between the bar and the image.
const FIRMWARE_TOP_BAR_PX: u32 = 16;

/// 8-bit grayscale for each of the four 2 bpp Newton pixel values.
/// 0 = white, 3 = black, intermediates are linear grays. Used by
/// `newton_gray` as the input to bilinear blending.
const GRAY_TABLE: [u32; 4] = [255, 170, 85, 0];

static INIT_DONE: AtomicBool = AtomicBool::new(false);
/// FbInfo captured from `display::splash`. `static mut` is safe
/// because we're single-core EL2 and `INIT_DONE` gates access.
static mut FB: Option<FbInfo> = None;
/// Painted region inside the panel, in panel pixels. Aspect 320:480.
static PAINTED_W: AtomicU32 = AtomicU32::new(0);
static PAINTED_H: AtomicU32 = AtomicU32::new(0);
/// Top-left of the painted region inside the panel.
static OFFSET_X: AtomicU32 = AtomicU32::new(0);
static OFFSET_Y: AtomicU32 = AtomicU32::new(0);
/// Inverse scale in Q16.16: `newton_pixel_q16 = painted_pixel * inv`.
/// Stored per-axis even though we preserve aspect (so the two values
/// are equal in practice — kept separate to keep the math local).
static INV_SCALE_X_Q16: AtomicU32 = AtomicU32::new(0);
static INV_SCALE_Y_Q16: AtomicU32 = AtomicU32::new(0);

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

fn init() {
    // The boot splash (`display::splash::init`, called earlier from
    // kmain) already allocated the framebuffer, painted the
    // background + logo + progress bar, and flushed. We adopt its
    // FbInfo so we don't double-allocate and so the splash stays
    // visible until the guest's first blit freezes the splash
    // (`splash::take_first_blit`).
    let info = match crate::host::display::splash::fb_info() {
        Some(i) => *i,
        None => {
            log_host_io!("host_io_pi_fb: splash didn't run; no FB available");
            return;
        }
    };

    // The firmware reserves `FIRMWARE_TOP_BAR_PX` rows at the top
    // of the scan-out region; FB row 0 lands at panel row
    // `FIRMWARE_TOP_BAR_PX`, so the visible portion of the FB is
    // only `panel_h - FIRMWARE_TOP_BAR_PX` rows tall. Treat that as
    // the effective panel height for the aspect-preserving fit.
    let effective_panel_h = info.height.saturating_sub(FIRMWARE_TOP_BAR_PX);

    let painted_w_if_height_limited = effective_panel_h * NEWTON_W / NEWTON_H;
    let painted_h_if_width_limited = info.width * NEWTON_H / NEWTON_W;
    let (painted_w, painted_h) = if painted_w_if_height_limited <= info.width {
        (painted_w_if_height_limited, effective_panel_h)
    } else {
        (info.width, painted_h_if_width_limited)
    };
    let offset_x = (info.width - painted_w) / 2;
    // offset_y is relative to the FB; FB row 0 already lands at
    // panel row `FIRMWARE_TOP_BAR_PX`, so we center within the
    // visible window by treating `effective_panel_h` as the canvas.
    let offset_y = (effective_panel_h - painted_h) / 2;

    // Inverse scale: newton-pixel-q16 per painted pixel.
    let inv_x = (NEWTON_W << 16) / painted_w.max(1);
    let inv_y = (NEWTON_H << 16) / painted_h.max(1);

    // SAFETY: single-core EL2, called once from kmain before any
    // other code touches these statics.
    unsafe {
        #[allow(static_mut_refs)]
        {
            FB = Some(info);
        }
    }
    PAINTED_W.store(painted_w, Ordering::Relaxed);
    PAINTED_H.store(painted_h, Ordering::Relaxed);
    OFFSET_X.store(offset_x, Ordering::Relaxed);
    OFFSET_Y.store(offset_y, Ordering::Relaxed);
    INV_SCALE_X_Q16.store(inv_x, Ordering::Relaxed);
    INV_SCALE_Y_Q16.store(inv_y, Ordering::Relaxed);
    INIT_DONE.store(true, Ordering::Relaxed);
    log_host_io!(
        "host_io_pi_fb: ready ({}x{} @ pa=0x{:x}, newton {}x{} bilinear → painted {}x{} @ {},{}, scale Q16 x={} y={})",
        info.width,
        info.height,
        info.pa,
        NEWTON_W,
        NEWTON_H,
        painted_w,
        painted_h,
        offset_x,
        offset_y,
        inv_x,
        inv_y,
    );
}

fn push_blit(ev: &BlitEvent, _payload: &[u8]) {
    let Some(fb) = fb() else {
        return;
    };

    // First guest blit ends the splash. Freeze progress updates so
    // they stop scribbling on Newton's UI, then blank the panel to
    // black — this hides the splash logo and the bar fragments that
    // extend past the painted Newton region.
    if crate::host::display::splash::take_first_blit() {
        crate::host::display::fb::fill_solid(fb, 0x0000_0000);
    }

    let dst_left = ev.dst_left as u32;
    let dst_top = ev.dst_top as u32;
    let dst_right = ev.dst_right as u32;
    let dst_bottom = ev.dst_bottom as u32;
    if dst_right <= dst_left || dst_bottom <= dst_top {
        return;
    }

    let painted_w = PAINTED_W.load(Ordering::Relaxed);
    let painted_h = PAINTED_H.load(Ordering::Relaxed);
    let inv_x = INV_SCALE_X_Q16.load(Ordering::Relaxed);
    let inv_y = INV_SCALE_Y_Q16.load(Ordering::Relaxed);
    if painted_w == 0 || painted_h == 0 || inv_x == 0 || inv_y == 0 {
        return;
    }
    let offset_x = OFFSET_X.load(Ordering::Relaxed) as usize;
    let offset_y = OFFSET_Y.load(Ordering::Relaxed) as usize;

    // Forward-map Newton dst rect → painted-pixel rect. Inclusive
    // on the top-left (floor), exclusive on the bottom-right (ceil
    // via div_ceil) so we cover every panel pixel whose bilinear
    // footprint overlaps the dst rect.
    let p_left = dst_left * painted_w / NEWTON_W;
    let p_top = dst_top * painted_h / NEWTON_H;
    let p_right = (dst_right * painted_w).div_ceil(NEWTON_W).min(painted_w);
    let p_bottom = (dst_bottom * painted_h).div_ceil(NEWTON_H).min(painted_h);

    let guest_fb = crate::hv::guest_mem::fb_host_pa() as *const u8;
    let stride = (NEWTON_W / 4) as usize;
    let pitch_words = (fb.pitch / 4) as usize;
    let panel_ptr = fb.pa as *mut u32;

    // The bilinear upscale runs 4 guest-FB samples + one volatile
    // panel write per painted pixel, plus a row-range cache flush — a
    // full-screen Newton update measures 22–33 ms (the EL2 stall
    // watermark attributed the audio "late period" stalls to exactly
    // this handler). That is far past the audio pump's tolerance, so
    // paint with IRQs unmasked, the same shape as the flash save: the
    // slim EL2 ISR keeps CNTHP and the MAI DMA refills serviced while
    // we loop. Nothing here touches slim-ISR-owned state (panel FB
    // writes, guest FB reads, pi_fb scaling atomics), and the guest
    // is not running while EL2 paints, so nothing re-enters.
    crate::arch::cpu::with_irqs_unmasked(|| {
        for py in p_top..p_bottom {
            let ny_q = py * inv_y;
            let ny_i = (ny_q >> 16) as usize;
            let ny_f = (ny_q >> 8) & 0xFF;
            let panel_y = py as usize + offset_y;

            for px in p_left..p_right {
                let nx_q = px * inv_x;
                let nx_i = (nx_q >> 16) as usize;
                let nx_f = (nx_q >> 8) & 0xFF;
                let panel_x = px as usize + offset_x;

                // Sample 4 Newton neighbors. `newton_gray` clamps at the
                // far edge so we don't read past the framebuffer.
                let g00 = newton_gray(guest_fb, stride, nx_i, ny_i);
                let g01 = newton_gray(guest_fb, stride, nx_i + 1, ny_i);
                let g10 = newton_gray(guest_fb, stride, nx_i, ny_i + 1);
                let g11 = newton_gray(guest_fb, stride, nx_i + 1, ny_i + 1);

                // Bilinear blend in 8-bit grayscale. Weights are Q0.8
                // (so each multiply stays in u32; the final >> 16
                // collapses the two Q0.8 levels back to 8-bit).
                let top = g00 * (256 - nx_f) + g01 * nx_f;
                let bot = g10 * (256 - nx_f) + g11 * nx_f;
                let g = (top * (256 - ny_f) + bot * ny_f) >> 16;
                let g8 = g.min(255) as u8;
                let color = u32::from_le_bytes([g8, g8, g8, 0]);

                // SAFETY: panel_x < painted_w + offset_x ≤ fb.width,
                // panel_y < painted_h + offset_y ≤ fb.height (set in
                // `init`). pitch_words = fb.pitch / 4.
                unsafe {
                    panel_ptr
                        .add(panel_y * pitch_words + panel_x)
                        .write_volatile(color);
                }
            }
        }

        // Flush the rows we touched so the VC scan picks them up.
        let flush_y0 = p_top as usize + offset_y;
        let flush_y1 = ((p_bottom as usize) + offset_y).min(fb.height as usize);
        let row_bytes_panel = pitch_words * 4;
        let flush_pa = fb.pa.wrapping_add((flush_y0 * row_bytes_panel) as u64);
        let flush_len = (flush_y1 - flush_y0) * row_bytes_panel;
        crate::arch::cpu::dc_civac_range(flush_pa, flush_len);
    });
}

/// 8-bit grayscale value at Newton FB pixel (x, y). Clamps at the
/// far edges so the bilinear sampler doesn't fall off the buffer
/// when the (x+1, y+1) neighbor sits exactly at the boundary.
fn newton_gray(fb: *const u8, stride: usize, x: usize, y: usize) -> u32 {
    let x = x.min(NEWTON_W as usize - 1);
    let y = y.min(NEWTON_H as usize - 1);
    // SAFETY: x < NEWTON_W and y < NEWTON_H by the clamp; stride =
    // NEWTON_W/4. The GUEST_FB backing is at least NEWTON_H*stride
    // bytes (guest_mem::FRAMEBUFFER_SIZE = 2 MiB ≫ 320×480/4).
    let byte = unsafe { *fb.add(y * stride + x / 4) };
    let shift = 6 - 2 * ((x as u32) % 4);
    let v = ((byte >> shift) & 0x3) as usize;
    GRAY_TABLE[v]
}

