//! Framebuffer allocation + access via the VC mailbox.
//!
//! Setup is straightforward: query the panel's native size, ask VC
//! to give us a framebuffer of that geometry, then talk to the
//! returned base address. We use 32 bpp (XRGB / RGB888-with-pad)
//! because that's what `host_io`'s blit path will eventually want
//! and it avoids a bpp-conversion step per pixel.
//!
//! VC returns a *bus* address. On the BCM2710 with `arm_64bit=1`
//! the VC L2 cache is disabled, so:
//!
//! - bus `pa | 0x4000_0000` → L2-cached alias (pass-through with L2
//!   off; equivalent to PA).
//! - bus `pa | 0xC000_0000` → uncached coherent alias.
//!
//! Firmware typically hands back the L2-cached form. We strip the
//! upper two alias bits to get the PA, which lands in our identity-
//! mapped DRAM region — no MMU plumbing required for the probe.
//! Writes go through Normal-WB; we call [`crate::arch::cpu::dc_civac_range`]
//! after a bulk fill so the VC sees coherent bytes on its next refresh.

#![allow(dead_code)] // Reachable when fb-probe or a host_io-pi-fb backend lands.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::{kprintln, host::mailbox};

/// Information about an allocated framebuffer.
#[derive(Debug, Clone, Copy)]
pub struct FbInfo {
    /// Physical address (CPU view) of the framebuffer's first byte.
    /// Identity-mapped — write through `pa as *mut u32`.
    pub pa: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per scanline. ≥ `width * 4` (firmware may pad rows).
    pub pitch: u32,
    /// Bits per pixel. We always request 32.
    pub bpp: u32,
    /// Total allocation size in bytes.
    pub size: u32,
}

#[derive(Debug)]
pub enum FbError {
    Mailbox(mailbox::MailboxError),
    /// Firmware honoured the request but returned 0 for a field that
    /// can't be 0 (typically size or base address).
    EmptyAllocation,
}

impl From<mailbox::MailboxError> for FbError {
    fn from(e: mailbox::MailboxError) -> Self {
        FbError::Mailbox(e)
    }
}

/// Which scan-out surface [`alloc_guest_surface`] ended up with.
/// Consumed by `host_io::pi_fb::init` to pick its paint path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    /// Small surface at guest-content scale; the firmware/HVS
    /// upscales it to the panel mode on scan-out. The paint path is
    /// 1:1, no CPU resampling.
    VcScaled,
    /// Surface at the panel's native mode size; the CPU scales guest
    /// content into it (the pre-Phase-3 behavior, kept as the
    /// runtime fallback).
    PanelNative,
}

/// Set by [`alloc_guest_surface`] before splash/pi_fb read it.
/// `false` = PanelNative (also the state when only [`alloc_native`]
/// ran, e.g. fb-probe).
static VC_SCALED_SURFACE: AtomicBool = AtomicBool::new(false);

/// What kind of surface the guest scan-out framebuffer is. Valid
/// after [`alloc_guest_surface`] returned.
pub fn guest_surface_kind() -> SurfaceKind {
    if VC_SCALED_SURFACE.load(Ordering::Relaxed) {
        SurfaceKind::VcScaled
    } else {
        SurfaceKind::PanelNative
    }
}

/// Allocate a framebuffer at the panel's native size, 32 bpp, RGB
/// pixel order. Returns metadata for later blits.
///
/// Fallback when the panel doesn't report a size (e.g. HDMI not
/// negotiated, headless boot): use 1024×768 so we still produce a
/// visible image if a monitor is later attached during the run.
pub fn alloc_native() -> Result<FbInfo, FbError> {
    let (panel_w, panel_h) = mailbox::fb_get_physical_size()?;
    let (w, h) = native_size_or_default(panel_w, panel_h);
    alloc_with_reset(w, h)
}

fn native_size_or_default(panel_w: u32, panel_h: u32) -> (u32, u32) {
    if panel_w == 0 || panel_h == 0 {
        kprintln!("display: panel reported size=0; falling back to 1024x768");
        (1024, 768)
    } else {
        (panel_w, panel_h)
    }
}

/// Allocate at `(w, h)` with the **modeset-reset dance**. The
/// firmware's initial HDMI modeset (driven by config.txt / EDID at
/// boot) leaves a thin white bar across the top of the picture and
/// intermittent link flicker on the Pi Zero 2 W + 1024×600 panel we
/// ship against. Raspbian shows the same symptoms until KMS later
/// does its own modeset, which clears them. We replicate that:
/// allocate the framebuffer once (the rough firmware modeset),
/// release it, then allocate again — the second allocation provokes
/// a fresh modeset that comes out clean. Cheap (two extra mailbox
/// round-trips); no-op on platforms where the firmware modeset is
/// already good.
fn alloc_with_reset(w: u32, h: u32) -> Result<FbInfo, FbError> {
    // First pass: forces the firmware's initial modeset. We
    // immediately discard the result — the FB it backs is the one
    // that exhibits the white-bar / flicker symptoms.
    let _ = alloc(w, h)?;
    if let Err(e) = mailbox::fb_release() {
        kprintln!("display: fb_release after first alloc failed: {:?}", e);
    }
    // Second pass: the fresh modeset. Use the returned FbInfo.
    alloc(w, h)
}

/// Best-effort HDMI pixel-clock readback (Hz; 0 = unreadable). Same
/// source `audio::pi_hdmi` uses. The pixel clock is a property of
/// the HDMI *mode*, not of the framebuffer, so a change across a
/// framebuffer allocation is evidence the firmware re-modeset.
fn pixel_clock_hz() -> u32 {
    if let Ok(hz) = mailbox::get_clock_rate_measured(mailbox::CLOCK_ID_PIXEL) {
        if hz != 0 {
            return hz;
        }
    }
    mailbox::get_clock_rate(mailbox::CLOCK_ID_PIXEL).unwrap_or(0)
}

fn div_round(n: u32, d: u32) -> u32 {
    (n + d / 2) / d
}

/// Allocate the guest scan-out surface.
///
/// Primary attempt: a **small VC-scaled surface** whose physical size
/// keeps the panel's aspect but whose height maps guest content 1:1 —
/// the firmware/HVS then scales it up to the (unchanged) HDMI mode on
/// scan-out, and the CPU never resamples a pixel. Geometry: the
/// visible guest content should span `panel_h - reserved_top_px`
/// panel rows (the same `FIRMWARE_TOP_BAR_PX` fudge the CPU scaler
/// applies — see `host_io::pi_fb`), so
///
/// ```text
/// fb_h = content_h * panel_h / (panel_h - reserved_top_px)   (≈ content_h)
/// fb_w = panel_w * fb_h / panel_h                            (panel aspect)
/// ```
///
/// For a 1920×1080 mode and 320×480 content with 16 reserved rows:
/// 866×487, content at x=(866-320)/2, scaled ×2.218 → visually
/// ~709×1064 — the exact geometry the CPU bilinear path produced.
///
/// **Runtime probe + fallback.** After allocating, verify the
/// firmware honoured the small physical size (returned w/h/pitch
/// match) *and* kept the HDMI mode (pixel-clock readback unchanged —
/// re-modesetting to a ~487-line mode would change it, and would
/// also break `audio_pi_hdmi`'s CTS). Any surprise logs loudly and
/// falls back to the panel-native surface + CPU scaling, so a
/// firmware quirk degrades to the slow path instead of a blank or
/// distorted panel. `pi-fb-force-cpu-scale` skips the attempt for
/// hardware A/B testing.
pub fn alloc_guest_surface(
    content_w: u32,
    content_h: u32,
    reserved_top_px: u32,
) -> Result<FbInfo, FbError> {
    let (panel_w, panel_h) = mailbox::fb_get_physical_size()?;
    VC_SCALED_SURFACE.store(false, Ordering::Relaxed);

    if cfg!(feature = "pi-fb-force-cpu-scale") {
        kprintln!("display: pi-fb-force-cpu-scale set; using panel-native surface");
        let (w, h) = native_size_or_default(panel_w, panel_h);
        return alloc_with_reset(w, h);
    }

    let eff_h = panel_h.saturating_sub(reserved_top_px);
    if panel_w == 0 || panel_h == 0 || eff_h < content_h {
        kprintln!(
            "display: panel {}x{} unusable for a VC-scaled {}x{} surface; using panel-native",
            panel_w, panel_h, content_w, content_h
        );
        let (w, h) = native_size_or_default(panel_w, panel_h);
        return alloc_with_reset(w, h);
    }

    let fb_h = div_round(content_h * panel_h, eff_h);
    let fb_w = div_round(panel_w * fb_h, panel_h);
    if fb_w < content_w || fb_w > panel_w || fb_h > panel_h {
        kprintln!(
            "display: VC-scaled geometry {}x{} out of range for panel {}x{}; using panel-native",
            fb_w, fb_h, panel_w, panel_h
        );
        return alloc_with_reset(panel_w, panel_h);
    }

    let pc_before = pixel_clock_hz();
    let attempt = alloc_with_reset(fb_w, fb_h);
    match attempt {
        Ok(info) => {
            let pc_after = pixel_clock_hz();
            if let Some(why) = vc_surface_surprise(&info, fb_w, fb_h, pc_before, pc_after) {
                kprintln!(
                    "display: firmware refused the VC-scaled surface ({}); \
                     falling back to panel-native {}x{} + CPU scaling",
                    why, panel_w, panel_h
                );
                if let Err(e) = mailbox::fb_release() {
                    kprintln!("display: fb_release after refused surface failed: {:?}", e);
                }
                return alloc_with_reset(panel_w, panel_h);
            }
            VC_SCALED_SURFACE.store(true, Ordering::Relaxed);
            kprintln!(
                "display: VC-scaled surface {}x{} (panel mode {}x{}, pixel clock {} Hz held)",
                info.width, info.height, panel_w, panel_h, pc_after
            );
            Ok(info)
        }
        Err(e) => {
            kprintln!(
                "display: VC-scaled {}x{} allocation FAILED ({:?}); \
                 falling back to panel-native {}x{} + CPU scaling",
                fb_w, fb_h, e, panel_w, panel_h
            );
            if let Err(e) = mailbox::fb_release() {
                kprintln!("display: fb_release after failed surface alloc failed: {:?}", e);
            }
            alloc_with_reset(panel_w, panel_h)
        }
    }
}

/// `Some(reason)` when the allocation is not the honoured VC-scaled
/// surface we asked for. Pixel-clock check tolerates 0.5% jitter and
/// is skipped when either readback failed (geometry alone decides).
fn vc_surface_surprise(
    info: &FbInfo,
    req_w: u32,
    req_h: u32,
    pc_before: u32,
    pc_after: u32,
) -> Option<&'static str> {
    if info.width != req_w || info.height != req_h {
        return Some("returned geometry differs from request");
    }
    if info.pitch < req_w * 4 || info.pitch > req_w * 4 + 4096 {
        return Some("returned pitch not sane for 32 bpp");
    }
    if (info.size as u64) < info.pitch as u64 * req_h as u64 {
        return Some("returned size smaller than pitch*height");
    }
    if pc_before != 0
        && pc_after != 0
        && pc_before.abs_diff(pc_after) as u64 * 200 > pc_before as u64
    {
        return Some("HDMI pixel clock moved (firmware re-modeset)");
    }
    None
}

/// Allocate a framebuffer at the given dimensions, 32 bpp RGB.
///
/// All setup tags + the allocation go through `fb_setup_and_allocate`
/// in a single mailbox message. Splitting them across messages
/// silently fails — the firmware processes each request atomically
/// and the second message doesn't inherit the first's geometry, so
/// allocation lands at firmware defaults (typically size=512,
/// pitch=32 — a useless degenerate framebuffer).
pub fn alloc(w: u32, h: u32) -> Result<FbInfo, FbError> {
    // 32 bpp, RGB pixel order (1), 4 KiB alignment.
    let a = mailbox::fb_setup_and_allocate(w, h, 32, 1, 4096)?;
    if a.bus_addr == 0 || a.size == 0 {
        return Err(FbError::EmptyAllocation);
    }

    // Strip the VC bus-alias bits to get the ARM PA. On BCM2710
    // with VC L2 disabled, bits 30:31 carry the cached / uncached
    // alias selection and don't participate in the address.
    let pa = (a.bus_addr & 0x3FFF_FFFF) as u64;

    Ok(FbInfo {
        pa,
        width: a.width,
        height: a.height,
        pitch: a.pitch,
        bpp: a.depth,
        size: a.size,
    })
}

/// Fill the entire framebuffer with a single 32-bit pixel value.
/// Packing: byte 0 = R, byte 1 = G, byte 2 = B, byte 3 = X
/// (firmware was asked for RGB pixel order). `0x00FF_0000` is red.
///
/// Walks one u32 per pixel via raw volatile writes — no cache
/// maintenance per-write; one `dc_civac_range` over the full FB at
/// the end ensures the VC's next refresh sees our bytes.
pub fn fill_solid(fb: &FbInfo, pixel: u32) {
    // SAFETY: framebuffer PA is identity-mapped Normal-WB by
    // mmu::init for the 0..1 GiB DRAM block; the firmware allocated
    // [pa, pa+size) for our use, no other code touches it.
    let ptr = fb.pa as *mut u32;
    let pixels_per_row = (fb.pitch / 4) as usize;
    for y in 0..fb.height as usize {
        for x in 0..fb.width as usize {
            // SAFETY: in-bounds by construction; ptr is aligned to
            // u32 (pitch is bytes-per-row, always multiple of 4 for
            // a 32 bpp surface).
            unsafe {
                ptr.add(y * pixels_per_row + x).write_volatile(pixel);
            }
        }
    }
    crate::arch::cpu::dc_civac_range(fb.pa, fb.size as usize);
}

/// Fill the top `n` rows of the framebuffer with a single pixel
/// value. Used as an overlay-vs-paint diagnostic: paint a known
/// distinctive colour, see whether the disputed bar covers it.
pub fn fill_top_rows(fb: &FbInfo, n: u32, pixel: u32) {
    let ptr = fb.pa as *mut u32;
    let pixels_per_row = (fb.pitch / 4) as usize;
    let rows = n.min(fb.height) as usize;
    for y in 0..rows {
        for x in 0..fb.width as usize {
            // SAFETY: in-bounds by construction (rows ≤ fb.height,
            // x < fb.width, ptr at fb.pa is fb.size bytes valid).
            unsafe {
                ptr.add(y * pixels_per_row + x).write_volatile(pixel);
            }
        }
    }
    let row_bytes = pixels_per_row * 4;
    crate::arch::cpu::dc_civac_range(fb.pa, rows * row_bytes);
}

/// Fill with a horizontal gradient — left = `left`, right = `right`.
/// Interpolation is byte-wise per channel, no gamma correction.
///
/// Row-major iteration. Each row is sequential memory, so each
/// cache line fill covers 16 pixels — ~16x fewer misses than a
/// column-major loop. Earlier column-major version produced a
/// visible ~0.5 s left-to-right paint sweep at boot because every
/// store touched a fresh cache line (pitch = 5120 bytes ≫ line
/// size 64).
///
/// Gradient math is recomputed per pixel rather than precomputed
/// to a per-column lookup; recomputation is ~30 cycles, vs ~8 KiB
/// of stack we'd otherwise consume (boot stack is 16 KiB total).
/// Total fill at 1280×720 is well under 50 ms either way.
pub fn fill_h_gradient(fb: &FbInfo, left: u32, right: u32) {
    let ptr = fb.pa as *mut u32;
    let pixels_per_row = (fb.pitch / 4) as usize;
    let w = fb.width as usize;
    let l = left.to_le_bytes();
    let r = right.to_le_bytes();
    let t_den = (w as u32).saturating_sub(1).max(1);

    let mix = |a: u8, b: u8, t_num: u32| -> u8 {
        ((a as u32 * (t_den - t_num) + b as u32 * t_num) / t_den) as u8
    };

    for y in 0..fb.height as usize {
        let row_base = y * pixels_per_row;
        for x in 0..w {
            let t_num = x as u32;
            let px = u32::from_le_bytes([
                mix(l[0], r[0], t_num),
                mix(l[1], r[1], t_num),
                mix(l[2], r[2], t_num),
                mix(l[3], r[3], t_num),
            ]);
            // SAFETY: see fill_solid.
            unsafe {
                ptr.add(row_base + x).write_volatile(px);
            }
        }
    }
    crate::arch::cpu::dc_civac_range(fb.pa, fb.size as usize);
}
