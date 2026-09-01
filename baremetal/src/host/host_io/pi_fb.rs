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
//! Two paint paths, chosen at boot by
//! `display::fb::alloc_guest_surface` (which the splash calls with
//! this module's geometry constants):
//!
//! - **VC-scaled (primary).** The framebuffer is a *small* surface
//!   whose height maps Newton 1:1 (e.g. 866×487 for a 1920×1080
//!   mode) and the firmware/HVS scales it to the unchanged HDMI mode
//!   on scan-out. `push_blit` then does no resampling at all: each
//!   2 bpp GUEST_FB byte expands through a LUT into four panel
//!   pixels — one u32 of gray-ramp palette indices on the 8 bpp
//!   paletted surface (the default; see `display::fb`), or four XRGB
//!   u32s on the 32 bpp fallback — and only the damaged column range
//!   per row is cache-cleaned (`dc cvac`, clean-only). Newton sits
//!   centered horizontally, letterboxed black.
//! - **CPU bilinear (runtime fallback + `pi-fb-force-cpu-scale`).**
//!   The pre-VC-path behavior: panel-native surface, Newton scaled
//!   with software bilinear to an aspect-preserving rectangle
//!   (~709×1064 on a 1080p panel, ~22–33 ms per full-screen paint).
//!   Engaged when the firmware refuses the small-physical /
//!   large-mode split — see `alloc_guest_surface`'s probe.
//!
//! Portrait rotation ([`ROTATION`], `pi-fb-rot90` feature, default
//! off) rides the VC path: the surface is allocated transposed, the
//! paint loop is byte-identical (Newton rows stay 1:1 row-major),
//! and the firmware rotates on scan-out (`display_hdmi_rotate=1` in
//! config.txt — the feature asserts what config.txt does; no
//! runtime readback exists). The CPU-bilinear fallback stays
//! landscape-only. See docs/REAL_HW_BRINGUP.md "Portrait rotation".
//!
//! Painting is decoupled from the guest's blit calls: `push_blit`
//! unions the dst Newton rect into a pending dirty rect and paints
//! at most once per [`PAINT_INTERVAL_MS`] (~60 Hz). An isolated blit
//! (pen ink, a clock tick) paints synchronously — the interval since
//! the last paint has long passed — while an animation's blit burst
//! accumulates and is flushed from the trap-return tail
//! ([`super::HostIo::pump_input`] runs there in every pi build
//! variant, with or without `serial-pen-inject`). Deferral has no
//! correctness cost: both paint paths sample from
//! `guest_mem::fb_host_pa()` — by the time `push_blit` runs,
//! `peripherals::screen::blit` has already written the new 2 bpp
//! pixels into `GUEST_FB` — so a later paint of the unioned rect
//! always shows the current content.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::BlitEvent;
use crate::host::display::fb::FbInfo;
use crate::kprintln;

/// Newton's screen dimensions as reported to `peripherals::screen`
/// through [`super::HostIo::panel_geometry`] (pulled by `main.rs` at
/// boot) and used by both paint paths. Default: MP2100-native
/// 320×480. The `pi-fb-hires` feature instead derives them from the
/// firmware panel readback at splash time ([`choose_newton_geometry`]
/// — half the logical scan-out shape, so the VC path keeps an exact
/// ×2 HVS scale; 540×960 on a rotated 1080×1920 panel).
///
/// The OS reflows to whatever GetScreenInfo reports —
/// hardware-verified at 540×960 (full UI layout, touch, store) —
/// but a few ROM code paths compute positions/bounds from the
/// native 320×480 (boot logo placement, animation erase, Dates'
/// view height), which is why hires is a DEFERRED opt-in experiment
/// and the default stays pinned. Quirk inventory + resume plan:
/// docs/REAL_HW_BRINGUP.md "Hires Newton geometry".
static NEWTON_W_RT: AtomicU32 = AtomicU32::new(320);
static NEWTON_H_RT: AtomicU32 = AtomicU32::new(480);

pub fn newton_w() -> u32 {
    NEWTON_W_RT.load(Ordering::Relaxed)
}
pub fn newton_h() -> u32 {
    NEWTON_H_RT.load(Ordering::Relaxed)
}

/// Under `pi-fb-hires`, pick the Newton geometry from the firmware's
/// physical-size readback (already transposed when a rot90 scan-out
/// is active, i.e. always the logical scan-out shape): half each
/// axis, width aligned down to the 2 bpp 4-pixel packing, halving
/// again while the result exceeds the screen model's scratch ceiling
/// (`peripherals::screen::MAX_SCREEN_W/H`). Must run before the
/// splash allocates the surface — `display::splash::init` calls it
/// with the readback it makes for exactly this purpose. Without the
/// feature (or on a zero readback) the 320×480 default stands.
pub fn choose_newton_geometry(rep_w: u32, rep_h: u32) {
    if !cfg!(feature = "pi-fb-hires") || rep_w == 0 || rep_h == 0 {
        return;
    }
    // Ceiling on the derived geometry. Must not exceed
    // `peripherals::screen::MAX_SCREEN_W/H` (the screen model's blit
    // scratch — it halts the boot on a larger mandate); kept as
    // local constants because host code must not import peripherals
    // (scripts/check-layering.sh).
    const HIRES_MAX_W: u32 = 1280;
    const HIRES_MAX_H: u32 = 960;
    let mut div = 2;
    while rep_w / div > HIRES_MAX_W || rep_h / div > HIRES_MAX_H {
        div *= 2;
    }
    let w = (rep_w / div) & !3;
    let h = rep_h / div;
    NEWTON_W_RT.store(w, Ordering::Relaxed);
    NEWTON_H_RT.store(h, Ordering::Relaxed);
    kprintln!("host_io_pi_fb: hires newton geometry {}x{} (panel readback {}x{})", w, h, rep_w, rep_h);
}
/// 2 bpp grayscale — the MP2x00 panel depth `peripherals::screen`
/// models.
const NEWTON_BPP: u32 = 2;

/// Scan-out rotation this build asserts the firmware applies —
/// see [`super::Rotation`] for why it's a build assertion
/// (`pi-fb-rot90` feature) rather than a runtime probe. `Rot0` by
/// default. `Rot90` reshapes the VC-scaled surface transposed
/// (`display::fb::alloc_guest_surface`), realigns the painted
/// region here, and flips the touch map (`input::calibrate`) — the
/// paint loop itself is unchanged (Newton rows stay 1:1 row-major;
/// the firmware rotates on scan-out). The CPU-bilinear fallback is
/// landscape-only: with `Rot90` selected it logs loudly at init and
/// paints unrotated. UNVERIFIED ON HARDWARE — see
/// docs/REAL_HW_BRINGUP.md "Portrait rotation".
pub const ROTATION: super::Rotation = if cfg!(feature = "pi-fb-rot90") {
    super::Rotation::Rot90
} else {
    super::Rotation::Rot0
};

pub struct PiFbBackend;

impl super::HostIo for PiFbBackend {
    fn init(&self) {
        init()
    }
    fn on_resume(&self) {
        // Repaint the panel from the restored GUEST_FB. Force the
        // paint-interval window open first so the repaint paints
        // synchronously instead of waiting out a pre-snapshot
        // deadline (CNTPCT keeps running across a restore).
        NEXT_PAINT_CNTPCT.store(0, Ordering::Relaxed);
        super::push_full_repaint(newton_w(), newton_h(), NEWTON_BPP);
    }
    fn push_blit(&self, ev: &super::BlitEvent, payload: &[u8]) {
        push_blit(ev, payload)
    }
    fn wants_payload(&self) -> bool {
        // `push_blit` samples pixels from GUEST_FB, never from the
        // payload — let `screen::blit` skip assembling it.
        false
    }
    fn pump_input(&self) {
        // No input source on this backend directly (pen input is
        // `input::mtouch`); the trap-tail cadence instead drives the
        // deferred dirty-rect flush.
        flush_deferred();
    }
    fn panel_geometry(&self) -> Option<(u32, u32)> {
        // Decided by splash-time geometry choice (default pin or
        // `pi-fb-hires` derivation), well before `main.rs` pulls
        // this — report it unconditionally.
        Some((newton_w(), newton_h()))
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
            // The scan-out rotation is a firmware property this build
            // asserts, independent of which paint path won — even the
            // (landscape-only) fallback scans out through the same
            // firmware transform, so calibration must invert it
            // either way.
            rotation: ROTATION,
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
///
/// Both paint paths honour it: the CPU-bilinear path shrinks its
/// effective panel height by this much (below); the VC-scaled path
/// bakes the same shrink into the surface geometry
/// (`display::fb::alloc_guest_surface` inflates the surface height
/// so Newton's 480 rows scale to `panel_h - FIRMWARE_TOP_BAR_PX`
/// visible rows — identical on-screen geometry either way). On the
/// HDMI-digitizer sink no bar is drawn and the fudge is a benign
/// 16-row bottom margin (capture-verified).
pub const FIRMWARE_TOP_BAR_PX: u32 = 16;

/// Reserved-top allowance for the scan-out surface geometry
/// (`display::fb::alloc_guest_surface`): [`FIRMWARE_TOP_BAR_PX`] in
/// landscape, 0 under rot90. Under a 90° CW rotation surface column
/// 0 scans out at the panel *top*, so the far-edge spare columns the
/// allowance would buy land at the panel bottom — it cannot dodge a
/// top bar there and only shrinks Newton. Capture-verified on the
/// digitizer sink: without it Newton spans all 1080 panel rows.
pub const RESERVED_TOP_PX: u32 = if cfg!(feature = "pi-fb-rot90") {
    0
} else {
    FIRMWARE_TOP_BAR_PX
};

/// 8-bit grayscale for each of the four 2 bpp Newton pixel values.
/// 0 = white, 3 = black, intermediates are linear grays. Used by
/// `newton_gray` as the input to bilinear blending and by
/// [`EXPAND_LUT`] for the 1:1 path.
const GRAY_TABLE: [u32; 4] = [255, 170, 85, 0];

/// 1:1 expansion LUT for the VC-scaled path on a 32 bpp surface: one
/// packed 2 bpp source byte (4 Newton pixels, MSB-first — pixel 0 in
/// bits 7..6, matching `newton_gray`'s shift) → four XRGB u32 panel
/// pixels, copied into the row as a single 16-byte run. 4 KiB, lives
/// in .rodata.
static EXPAND_LUT: [[u32; 4]; 256] = build_expand_lut();

const fn build_expand_lut() -> [[u32; 4]; 256] {
    let mut lut = [[0u32; 4]; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut i = 0usize;
        while i < 4 {
            let v = (b >> (6 - 2 * i)) & 0x3;
            let g = GRAY_TABLE[v] as u8;
            lut[b][i] = u32::from_le_bytes([g, g, g, 0]);
            i += 1;
        }
        b += 1;
    }
    lut
}

/// 1:1 expansion LUT for the 8 bpp paletted surface (the default —
/// see `display::fb::alloc_guest_at`): one packed 2 bpp source byte →
/// four gray-ramp palette indices, written as a single u32. A quarter
/// of [`EXPAND_LUT`]'s write bandwidth. 1 KiB, lives in .rodata.
static EXPAND_LUT8: [u32; 256] = build_expand_lut8();

const fn build_expand_lut8() -> [u32; 256] {
    let mut lut = [0u32; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut bytes = [0u8; 4];
        let mut i = 0usize;
        while i < 4 {
            let v = (b >> (6 - 2 * i)) & 0x3;
            bytes[i] = crate::host::display::fb::gray_ramp_index(GRAY_TABLE[v]);
            i += 1;
        }
        lut[b] = u32::from_le_bytes(bytes);
        b += 1;
    }
    lut
}

static INIT_DONE: AtomicBool = AtomicBool::new(false);
/// `true` when the surface is the small VC-scaled one and `push_blit`
/// paints 1:1; `false` selects the CPU-bilinear fallback. Mirrors
/// `display::fb::guest_surface_kind()` at init time.
static VC_SCALED: AtomicBool = AtomicBool::new(false);
/// FbInfo captured from `display::splash`. `static mut` is safe
/// because we're single-core EL2 and `INIT_DONE` gates access.
static mut FB: Option<FbInfo> = None;
/// Painted region inside the scan-out surface, in surface pixels
/// (VC-scaled: exactly NEWTON_W×NEWTON_H; panel-native: the
/// aspect-preserving bilinear target). Aspect 320:480 either way.
static PAINTED_W: AtomicU32 = AtomicU32::new(0);
static PAINTED_H: AtomicU32 = AtomicU32::new(0);
/// Top-left of the painted region inside the surface.
static OFFSET_X: AtomicU32 = AtomicU32::new(0);
static OFFSET_Y: AtomicU32 = AtomicU32::new(0);
/// Inverse scale in Q16.16: `newton_pixel_q16 = painted_pixel * inv`.
/// Stored per-axis even though we preserve aspect (so the two values
/// are equal in practice — kept separate to keep the math local).
static INV_SCALE_X_Q16: AtomicU32 = AtomicU32::new(0);
static INV_SCALE_Y_Q16: AtomicU32 = AtomicU32::new(0);

/// Pending dirty rect in Newton pixels, awaiting a paint. Empty iff
/// `DIRTY_RIGHT <= DIRTY_LEFT` (the reset state: left/top at MAX,
/// right/bottom at 0, so `fetch_min`/`fetch_max` union correctly
/// from empty). Written by `push_blit`, consumed by `flush_pending`.
/// Single-core EL2, and the slim same-EL ISR runs no host-io pumps
/// (`hv::trap::irq_from_el2`'s contract), so a paint's
/// `with_irqs_unmasked` window cannot race these.
static DIRTY_LEFT: AtomicU32 = AtomicU32::new(u32::MAX);
static DIRTY_TOP: AtomicU32 = AtomicU32::new(u32::MAX);
static DIRTY_RIGHT: AtomicU32 = AtomicU32::new(0);
static DIRTY_BOTTOM: AtomicU32 = AtomicU32::new(0);
/// Earliest CNTPCT at which the next paint may run; 0 = paint now.
/// Same throttle shape as `semihost`'s `NEXT_PUMP_CNTPCT`.
static NEXT_PAINT_CNTPCT: AtomicU64 = AtomicU64::new(0);
/// Minimum wall time between paints (~60 Hz).
const PAINT_INTERVAL_MS: u64 = 16;

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
            kprintln!("host_io_pi_fb: splash didn't run; no FB available");
            return;
        }
    };

    // VC-scaled surface: Newton lands 1:1; the HVS scales (and, under
    // Rot90, rotates) the whole surface to the panel mode.
    if crate::host::display::fb::guest_surface_kind()
        == crate::host::display::fb::SurfaceKind::VcScaled
    {
        let (offset_x, offset_y) = match ROTATION {
            // Landscape: centered horizontally, top-aligned — the
            // surface height carries the FIRMWARE_TOP_BAR_PX
            // allowance (see `alloc_guest_surface`), so the spare
            // rows sit at the bottom, where the bar-shifted scan-out
            // clips.
            super::Rotation::Rot0 => (info.width.saturating_sub(newton_w()) / 2, 0),
            // Rot90: the bar allowance lives on the surface x axis
            // (columns scan out as panel rows, column 0 at the panel
            // top under the asserted 90° CW rotation) — left-align so
            // the spare columns sit at the clipped panel-bottom edge,
            // and center along y (rows scan out as panel columns).
            super::Rotation::Rot90 => (0, info.height.saturating_sub(newton_h()) / 2),
        };
        // SAFETY: single-core EL2, called once from kmain before any
        // other code touches these statics.
        unsafe {
            #[allow(static_mut_refs)]
            {
                FB = Some(info);
            }
        }
        PAINTED_W.store(newton_w(), Ordering::Relaxed);
        PAINTED_H.store(newton_h(), Ordering::Relaxed);
        OFFSET_X.store(offset_x, Ordering::Relaxed);
        OFFSET_Y.store(offset_y, Ordering::Relaxed);
        // 1:1 — inverse scale is exactly one Newton pixel per painted
        // pixel. Unused by the 1:1 paint loop, but keeps
        // painted_region consumers' math uniform.
        INV_SCALE_X_Q16.store(1 << 16, Ordering::Relaxed);
        INV_SCALE_Y_Q16.store(1 << 16, Ordering::Relaxed);
        VC_SCALED.store(true, Ordering::Relaxed);
        INIT_DONE.store(true, Ordering::Relaxed);
        // Boot-once geometry line — deliberately `kprintln!`, see the
        // fallback arm's comment.
        kprintln!(
            "host_io_pi_fb: ready ({}x{} {} bpp @ pa=0x{:x}, vc-scaled 1:1{}, newton {}x{} @ {},{})",
            info.width,
            info.height,
            info.bpp,
            info.pa,
            if ROTATION == super::Rotation::Rot90 {
                " rot90"
            } else {
                ""
            },
            newton_w(),
            newton_h(),
            offset_x,
            offset_y,
        );
        return;
    }

    // The CPU-bilinear fallback cannot rotate (a rotating software
    // blit writes down panel columns — the cache-miss-per-store
    // pattern `display::fb::fill_h_gradient`'s doc records as a
    // ~0.5 s full-screen fill — so it was deliberately not built).
    // The firmware still rotates scan-out per config.txt, so the
    // panel image will be sideways and anisotropically squashed
    // until the VC-scaled path works again. Degraded diagnostic
    // state: log loudly, paint landscape.
    if ROTATION == super::Rotation::Rot90 {
        kprintln!(
            "host_io_pi_fb: WARNING: pi-fb-rot90 selected but the VC-scaled \
             surface fell back to panel-native; the CPU-bilinear fallback is \
             landscape-only — painting UNROTATED under a rotated scan-out"
        );
    }

    // Panel-native surface (runtime fallback / pi-fb-force-cpu-scale):
    // CPU bilinear scaling, pre-Phase-3 behavior.
    //
    // The firmware reserves `FIRMWARE_TOP_BAR_PX` rows at the top
    // of the scan-out region; FB row 0 lands at panel row
    // `FIRMWARE_TOP_BAR_PX`, so the visible portion of the FB is
    // only `panel_h - FIRMWARE_TOP_BAR_PX` rows tall. Treat that as
    // the effective panel height for the aspect-preserving fit.
    let effective_panel_h = info.height.saturating_sub(FIRMWARE_TOP_BAR_PX);

    let (nw, nh) = (newton_w(), newton_h());
    let painted_w_if_height_limited = effective_panel_h * nw / nh;
    let painted_h_if_width_limited = info.width * nh / nw;
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
    let inv_x = (nw << 16) / painted_w.max(1);
    let inv_y = (nh << 16) / painted_h.max(1);

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
    // Boot-once geometry line, deliberately `kprintln!` (not
    // `log_host_io!`): the deployed `pi-bare-metal-input` build omits
    // `log_host_io`, and this line is the only place the negotiated
    // panel mode and painted-region geometry reach a hardware boot
    // capture. One-shot, so the recurring-log doctrine doesn't apply.
    kprintln!(
        "host_io_pi_fb: ready ({}x{} {} bpp @ pa=0x{:x}, newton {}x{} bilinear → painted {}x{} @ {},{}, scale Q16 x={} y={})",
        info.width,
        info.height,
        info.bpp,
        info.pa,
        nw,
        nh,
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

    // Clamp the Newton dst rect to the chosen geometry — both paint
    // paths index GUEST_FB and the surface with it.
    let (nw, nh) = (newton_w(), newton_h());
    let dst_left = (ev.dst_left as u32).min(nw);
    let dst_top = (ev.dst_top as u32).min(nh);
    let dst_right = (ev.dst_right as u32).min(nw);
    let dst_bottom = (ev.dst_bottom as u32).min(nh);
    if dst_right <= dst_left || dst_bottom <= dst_top {
        return;
    }

    // Coalesce: union into the pending dirty rect. GUEST_FB already
    // holds the new pixels, so deferring just means a later flush
    // paints a superset.
    DIRTY_LEFT.fetch_min(dst_left, Ordering::Relaxed);
    DIRTY_TOP.fetch_min(dst_top, Ordering::Relaxed);
    DIRTY_RIGHT.fetch_max(dst_right, Ordering::Relaxed);
    DIRTY_BOTTOM.fetch_max(dst_bottom, Ordering::Relaxed);

    // Paint policy: paint synchronously when [`PAINT_INTERVAL_MS`]
    // has already elapsed since the last paint — an isolated blit
    // (pen ink, a clock tick) pays no added latency. Inside the
    // interval the rect only accumulates; `pump_input` flushes it
    // from the trap-return tail once the interval expires, so a
    // burst's final blit is painted at most one trap late.
    let now = cntpct();
    let next = NEXT_PAINT_CNTPCT.load(Ordering::Relaxed);
    if next == 0 || now >= next {
        flush_pending(fb, now);
    }
}

/// Paint the pending dirty rect, if any, and start a new
/// [`PAINT_INTERVAL_MS`] window. `now` is the caller's CNTPCT read.
///
/// This is where `diag::blit_timing::PAINT` records — once per
/// *actual* paint, whether immediate or trap-tail-deferred, so a
/// window line keeps measuring paint work (one record may cover
/// several coalesced blits). `host_io::push_guest_blit` skips its
/// generic PAINT wrapper for this backend.
fn flush_pending(fb: &FbInfo, now: u64) {
    let dst_left = DIRTY_LEFT.swap(u32::MAX, Ordering::Relaxed);
    let dst_top = DIRTY_TOP.swap(u32::MAX, Ordering::Relaxed);
    let dst_right = DIRTY_RIGHT.swap(0, Ordering::Relaxed);
    let dst_bottom = DIRTY_BOTTOM.swap(0, Ordering::Relaxed);
    if dst_right <= dst_left || dst_bottom <= dst_top {
        return;
    }
    let interval = (PAINT_INTERVAL_MS * cntfrq()) / 1_000;
    NEXT_PAINT_CNTPCT.store(now.wrapping_add(interval), Ordering::Relaxed);

    // A full-screen CPU-bilinear paint measures 22–33 ms (the EL2
    // stall watermark attributed the audio "late period" stalls to
    // exactly this handler) — far past the audio pump's tolerance, so
    // paint with IRQs unmasked, the same shape as the flash save: the
    // slim EL2 ISR keeps CNTHP and the MAI DMA refills serviced while
    // we loop. The 1:1 path is ~10× cheaper but the wrapper costs
    // nothing, so both paths keep it. Nothing here touches
    // slim-ISR-owned state (panel FB writes, guest FB reads, pi_fb
    // scaling and dirty-rect atomics), and the guest is not running
    // while EL2 paints, so nothing re-enters.
    let t_paint = crate::diag::blit_timing::begin();
    crate::arch::cpu::with_irqs_unmasked(|| {
        if VC_SCALED.load(Ordering::Relaxed) {
            paint_1to1(fb, dst_left, dst_top, dst_right, dst_bottom);
        } else {
            paint_bilinear(fb, dst_left, dst_top, dst_right, dst_bottom);
        }
    });
    crate::diag::blit_timing::PAINT.record_since(t_paint);
}

/// Trap-tail flush: paint the pending dirty rect once the paint
/// interval has expired. Runs from `pump_input` on every sync-trap
/// exit and guest-IRQ tail; even an otherwise idle guest gets the
/// ~16 ms CNTHP heartbeat, which bounds how late a deferred paint
/// can land.
fn flush_deferred() {
    // Cheap emptiness gate — keeps the per-trap cost at two loads.
    if DIRTY_RIGHT.load(Ordering::Relaxed) <= DIRTY_LEFT.load(Ordering::Relaxed) {
        return;
    }
    let now = cntpct();
    let next = NEXT_PAINT_CNTPCT.load(Ordering::Relaxed);
    if next != 0 && now < next {
        return;
    }
    let Some(fb) = fb() else {
        return;
    };
    flush_pending(fb, now);
}

/// VC-scaled path: expand the damaged GUEST_FB bytes 1:1 onto the
/// small surface — no resampling. One [`EXPAND_LUT`] lookup + one
/// 16-byte copy per 2 bpp source byte; per row, clean only the
/// damaged column range (`dc cvac`, clean-only — the surface lines
/// stay cache-resident for the next frame; the VC reads DRAM, never
/// writes, so there is nothing to invalidate).
///
/// Painting whole source bytes may redraw up to 3 pixels beyond each
/// horizontal edge of the dst rect; they repaint with their current
/// GUEST_FB value, so this is idempotent.
fn paint_1to1(fb: &FbInfo, dst_left: u32, dst_top: u32, dst_right: u32, dst_bottom: u32) {
    let offset_x = OFFSET_X.load(Ordering::Relaxed) as usize;
    let offset_y = OFFSET_Y.load(Ordering::Relaxed) as usize;

    let guest_fb = crate::hv::guest_mem::fb_host_pa() as *const u8;
    let stride = (newton_w() / 4) as usize;

    let xb0 = (dst_left / 4) as usize;
    let xb1 = (dst_right as usize).div_ceil(4).min(stride);
    let n_bytes = xb1 - xb0;
    let row_px0 = offset_x + xb0 * 4;

    if fb.bpp == 8 {
        // Paletted surface: one u32 of four palette-index bytes per
        // source byte. The dst address is byte-granular (offset_x
        // need not be 4-aligned), so write_unaligned — AArch64
        // Normal-WB memory takes unaligned stores natively.
        let pitch = fb.pitch as usize;
        let panel_ptr = fb.pa as *mut u8;
        for y in dst_top as usize..dst_bottom as usize {
            // SAFETY: src as in the 32 bpp arm below. Dst: init
            // guarantees offset_x + newton_w() ≤ fb.width ≤ pitch and
            // offset_y + newton_h() ≤ fb.height, so every write lands
            // inside [fb.pa, fb.pa+size).
            unsafe {
                let src = guest_fb.add(y * stride + xb0);
                let dst = panel_ptr.add((y + offset_y) * pitch + row_px0);
                for i in 0..n_bytes {
                    let b = *src.add(i);
                    (dst.add(i * 4) as *mut u32).write_unaligned(EXPAND_LUT8[b as usize]);
                }
            }
            let row_pa = fb.pa.wrapping_add(((y + offset_y) * pitch + row_px0) as u64);
            crate::arch::cpu::dc_cvac_range(row_pa, n_bytes * 4);
        }
        return;
    }

    let pitch_words = (fb.pitch / 4) as usize;
    let panel_ptr = fb.pa as *mut u32;

    for y in dst_top as usize..dst_bottom as usize {
        // SAFETY: y < newton_h(), xb0..xb1 ≤ stride — inside the
        // GUEST_FB backing (≥ newton_h()*stride bytes). Dst: init
        // guarantees offset_x + newton_w() ≤ fb.width and offset_y
        // + newton_h() ≤ fb.height for the VC-scaled surface in both
        // rotations (`alloc_guest_surface` sizes it that way), so
        // every write lands inside [fb.pa, fb.pa+size).
        unsafe {
            let src = guest_fb.add(y * stride + xb0);
            let dst = panel_ptr.add((y + offset_y) * pitch_words + row_px0);
            for i in 0..n_bytes {
                let b = *src.add(i);
                core::ptr::copy_nonoverlapping(
                    EXPAND_LUT[b as usize].as_ptr(),
                    dst.add(i * 4),
                    4,
                );
            }
        }
        let row_pa = fb
            .pa
            .wrapping_add(((y + offset_y) * pitch_words + row_px0) as u64 * 4);
        crate::arch::cpu::dc_cvac_range(row_pa, n_bytes * 16);
    }
}

/// Panel-native fallback path: software bilinear upscale, 4 GUEST_FB
/// samples + one volatile surface write per painted pixel.
fn paint_bilinear(fb: &FbInfo, dst_left: u32, dst_top: u32, dst_right: u32, dst_bottom: u32) {
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
    let (nw, nh) = (newton_w(), newton_h());
    let p_left = dst_left * painted_w / nw;
    let p_top = dst_top * painted_h / nh;
    let p_right = (dst_right * painted_w).div_ceil(nw).min(painted_w);
    let p_bottom = (dst_bottom * painted_h).div_ceil(nh).min(painted_h);

    let guest_fb = crate::hv::guest_mem::fb_host_pa() as *const u8;
    let stride = (nw / 4) as usize;
    let pitch_words = (fb.pitch / 4) as usize;
    let panel_ptr = fb.pa as *mut u32;
    let pal8 = fb.bpp == 8;

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
            let g00 = newton_gray(guest_fb, stride, nw as usize, nh as usize, nx_i, ny_i);
            let g01 = newton_gray(guest_fb, stride, nw as usize, nh as usize, nx_i + 1, ny_i);
            let g10 = newton_gray(guest_fb, stride, nw as usize, nh as usize, nx_i, ny_i + 1);
            let g11 = newton_gray(guest_fb, stride, nw as usize, nh as usize, nx_i + 1, ny_i + 1);

            // Bilinear blend in 8-bit grayscale. Weights are Q0.8
            // (so each multiply stays in u32; the final >> 16
            // collapses the two Q0.8 levels back to 8-bit).
            let top = g00 * (256 - nx_f) + g01 * nx_f;
            let bot = g10 * (256 - nx_f) + g11 * nx_f;
            let g = (top * (256 - ny_f) + bot * ny_f) >> 16;
            let g8 = g.min(255);

            // SAFETY: panel_x < painted_w + offset_x ≤ fb.width,
            // panel_y < painted_h + offset_y ≤ fb.height (set in
            // `init`). pitch_words = fb.pitch / 4; on the paletted
            // surface fb.width ≤ fb.pitch.
            if pal8 {
                unsafe {
                    (fb.pa as *mut u8)
                        .add(panel_y * fb.pitch as usize + panel_x)
                        .write_volatile(crate::host::display::fb::gray_ramp_index(g8));
                }
            } else {
                let g8 = g8 as u8;
                let color = u32::from_le_bytes([g8, g8, g8, 0]);
                unsafe {
                    panel_ptr
                        .add(panel_y * pitch_words + panel_x)
                        .write_volatile(color);
                }
            }
        }
    }

    // Flush the rows we touched so the VC scan picks them up.
    let flush_y0 = p_top as usize + offset_y;
    let flush_y1 = ((p_bottom as usize) + offset_y).min(fb.height as usize);
    let row_bytes_panel = fb.pitch as usize;
    let flush_pa = fb.pa.wrapping_add((flush_y0 * row_bytes_panel) as u64);
    let flush_len = (flush_y1 - flush_y0) * row_bytes_panel;
    crate::arch::cpu::dc_civac_range(flush_pa, flush_len);
}

/// 8-bit grayscale value at Newton FB pixel (x, y). Clamps at the
/// far edges (`nw`/`nh` = the Newton geometry) so the bilinear
/// sampler doesn't fall off the buffer when the (x+1, y+1) neighbor
/// sits exactly at the boundary.
fn newton_gray(fb: *const u8, stride: usize, nw: usize, nh: usize, x: usize, y: usize) -> u32 {
    let x = x.min(nw - 1);
    let y = y.min(nh - 1);
    // SAFETY: x < nw and y < nh by the clamp; stride = nw/4. The
    // GUEST_FB backing is at least nh*stride bytes
    // (guest_mem::FRAMEBUFFER_SIZE = 2 MiB ≫ any geometry within
    // screen::MAX_SCREEN_W/H at 2 bpp).
    let byte = unsafe { *fb.add(y * stride + x / 4) };
    let shift = 6 - 2 * ((x as u32) % 4);
    let v = ((byte >> shift) & 0x3) as usize;
    GRAY_TABLE[v]
}

fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: sysreg read, side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: sysreg read.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

