//! Boot-time splash screen + progress bar for the Pi VC framebuffer.
//!
//! Painted onto the panel as soon as the mailbox-allocated framebuffer
//! is in hand — before flash init, before VIC bring-up, before the
//! guest gets ERET'd in. The user looks at a light-blue background,
//! the Newton Hypervisor logo at 1/3 panel height, and a black
//! 500×16 progress bar at 2/3 panel height. The bar fills white as
//! the guest takes traps (rough boot-progress proxy; 100% =
//! `TARGET_TRAPS`).
//!
//! The splash owns the [`FbInfo`] for the rest of the run; the
//! `host_io::pi_fb` backend picks it up via [`fb_info`] and reuses it
//! for guest blits. The first guest blit (via [`take_first_blit`])
//! sets the frozen flag, after which `update_progress` becomes a
//! no-op — Newton's UI is what the user sees from then on.
//!
//! Logo source: `assets/splash_logo.ppm` (P6, 8 bpc RGB). `build.rs`
//! converts it to a raw RGB blob in OUT_DIR; missing file => zero-size
//! placeholder and the logo step is skipped (background + bar only).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::host::display::fb::{self, FbInfo};
use crate::kprintln;

/// Soft sky blue. Byte order matches `pi_fb`'s PALETTE: byte 0 = R,
/// byte 1 = G, byte 2 = B, byte 3 = pad (RGB pixel-order from VC).
const BG: u32 = u32::from_le_bytes([0xA0, 0xC8, 0xE8, 0x00]);
const BAR_BG: u32 = u32::from_le_bytes([0x00, 0x00, 0x00, 0x00]);
const BAR_FG: u32 = u32::from_le_bytes([0xFF, 0xFF, 0xFF, 0x00]);

/// Logical bar width: progress arithmetic ([`update_progress`],
/// [`set_load_progress`], `BAR_FILLED`) runs in this domain. The
/// *painted* width is `BAR_W.min(width * 5/8)` — the same ~5/8
/// proportion this constant gives the 866-wide landscape surface —
/// so the bar keeps side margins on the narrow rot90 surface
/// (320 → 200 painted px) instead of clipping edge-to-edge.
const BAR_W: u32 = 500;
const BAR_H: u32 = 16;

/// Bar layout: 0..LOAD_BAR_W px = SD flash-load progress (first
/// 20% of the bar); LOAD_BAR_W..BAR_W px = trap-counter progress
/// (remaining 80%). The two segments share `BAR_FILLED` and advance
/// monotonically; in practice the load completes well before the
/// first guest trap (timer::init runs after flash_persist::try_load),
/// so there's no race between the two updaters.
const LOAD_BAR_W: u32 = BAR_W / 5;

/// Trap count that fills the trap-driven segment (the upper 80% of
/// the bar). Tunable; once the count reaches this value the bar stays
/// at 100%.
pub const TARGET_TRAPS: u64 = 250_000;

const LOGO_W: u32 = {
    // env! returns &str at compile time; parse manually because const-fn
    // string parsing isn't on stable yet.
    let s = env!("NH_SPLASH_LOGO_W").as_bytes();
    let mut v = 0u32;
    let mut i = 0;
    while i < s.len() {
        v = v * 10 + (s[i] - b'0') as u32;
        i += 1;
    }
    v
};
const LOGO_H: u32 = {
    let s = env!("NH_SPLASH_LOGO_H").as_bytes();
    let mut v = 0u32;
    let mut i = 0;
    while i < s.len() {
        v = v * 10 + (s[i] - b'0') as u32;
        i += 1;
    }
    v
};

/// Raw RGB blob produced by build.rs from `assets/splash_logo.ppm`.
/// Empty when no logo file is present.
static LOGO_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/splash_logo.bin"));

static INIT_DONE: AtomicBool = AtomicBool::new(false);
static FROZEN: AtomicBool = AtomicBool::new(false);
/// Number of bar pixels currently filled white. Tracks the last paint
/// so each `update_progress` call only repaints newly-revealed pixels.
static BAR_FILLED: AtomicU32 = AtomicU32::new(0);

/// SAFETY: single-core EL2, written only by `init` on core 0 before
/// `INIT_DONE` is set; subsequent readers see a fully-initialized struct.
static mut FB: Option<FbInfo> = None;
/// Panel coordinates of the bar's top-left corner.
static mut BAR_X: u32 = 0;
static mut BAR_Y: u32 = 0;
/// Painted bar width (`BAR_W` scaled to fit the surface).
static mut BAR_W_PAINTED: u32 = BAR_W;

/// Allocate the framebuffer, paint background + logo + empty bar, and
/// flush. Returns the [`FbInfo`] so callers (kmain) can wire it
/// downstream. Idempotent — second call returns the same handle.
pub fn init() -> Option<FbInfo> {
    if INIT_DONE.load(Ordering::Relaxed) {
        return fb_info().copied();
    }

    // The splash allocates the surface the *guest* will scan out of
    // for the rest of the run (pi_fb adopts it via `fb_info`), so it
    // allocates via `alloc_guest_surface` with pi_fb's geometry
    // policy: preferably a small VC-scaled surface (firmware/HVS
    // upscales to the panel mode; pi_fb paints 1:1), panel-native as
    // the runtime fallback. The splash's own layout is relative to
    // the returned FbInfo either way, so boot visuals work on both —
    // on the small surface they're simply HVS-upscaled.
    //
    // Give pi_fb the panel readback first — under `pi-fb-hires` it
    // derives the Newton geometry from it (no-op otherwise, and on a
    // failed readback the default geometry stands).
    if let Ok((rw, rh)) = crate::host::mailbox::fb_get_physical_size() {
        crate::host::host_io::pi_fb::choose_newton_geometry(rw, rh);
    }
    let info = match fb::alloc_guest_surface(
        crate::host::host_io::pi_fb::newton_w(),
        crate::host::host_io::pi_fb::newton_h(),
        crate::host::host_io::pi_fb::RESERVED_TOP_PX,
        // Under rot90 the splash needs no rotation of its own: it
        // paints row-major into the transposed surface through the
        // same transform chain as guest blits, so its layout shows
        // upright on the portrait-mounted panel (hardware-verified).
        // Only the element sizes adapt — see BAR_W.
        crate::host::host_io::pi_fb::ROTATION == crate::host::host_io::Rotation::Rot90,
    ) {
        Ok(i) => i,
        Err(e) => {
            kprintln!("splash: FB init FAILED: {:?}", e);
            return None;
        }
    };

    paint_background(&info);
    if LOGO_W != 0 && LOGO_H != 0 {
        paint_logo(&info);
    }
    let (bx, by, bw) = paint_empty_bar(&info);

    // Flush the whole framebuffer once so the VC scan sees the splash.
    crate::arch::cpu::dc_civac_range(info.pa, info.size as usize);

    // SAFETY: single-core EL2; first call from kmain before any reader.
    unsafe {
        #[allow(static_mut_refs)]
        {
            FB = Some(info);
            BAR_X = bx;
            BAR_Y = by;
            BAR_W_PAINTED = bw;
        }
    }
    INIT_DONE.store(true, Ordering::Relaxed);
    kprintln!(
        "splash: ready ({}x{}, bar @ {},{} {}x{}, logo {}x{})",
        info.width,
        info.height,
        bx,
        by,
        bw,
        BAR_H,
        LOGO_W,
        LOGO_H,
    );
    Some(info)
}

/// Return the framebuffer handle once `init` has run. Used by
/// `host_io::pi_fb` so it doesn't double-allocate.
#[allow(static_mut_refs)]
pub fn fb_info() -> Option<&'static FbInfo> {
    if !INIT_DONE.load(Ordering::Relaxed) {
        return None;
    }
    // SAFETY: see comment on `FB`.
    unsafe { FB.as_ref() }
}

/// Returns `true` exactly once: the first call after `init` has run
/// while the splash is not yet frozen. Sets the frozen flag as a
/// side-effect (so the bar doesn't repaint over Newton's UI). Used by
/// `host_io::pi_fb::push_blit` to detect the hand-off from splash to
/// guest UI and trigger a one-shot panel blank.
pub fn take_first_blit() -> bool {
    if !INIT_DONE.load(Ordering::Relaxed) {
        return false;
    }
    !FROZEN.swap(true, Ordering::Relaxed)
}

/// Update the white-fill width based on `traps`. Cheap to call on
/// every timer IRQ: paints at most `target - BAR_FILLED` pixels and
/// flushes only the bar rows. No-op once the splash is frozen.
/// Trap progress drives only the upper segment of the bar
/// (`LOAD_BAR_W..BAR_W`); the lower segment is owned by
/// [`set_load_progress`].
pub fn update_progress(traps: u64) {
    let pct_num = traps.min(TARGET_TRAPS);
    let trap_segment_w = BAR_W - LOAD_BAR_W;
    let trap_fill = ((pct_num * trap_segment_w as u64) / TARGET_TRAPS) as u32;
    advance_bar(LOAD_BAR_W + trap_fill);
}

/// Update the white-fill width based on the SD flash-load progress.
/// Drives only the lower 20% of the bar (`0..LOAD_BAR_W`). Called from
/// `flash_persist::sd::try_load` as bytes stream in.
pub fn set_load_progress(done: u64, total: u64) {
    if total == 0 {
        return;
    }
    let done = done.min(total);
    let fill = (done * LOAD_BAR_W as u64 / total) as u32;
    advance_bar(fill);
}

/// Common paint path: extend `BAR_FILLED` toward `target` (clamped to
/// `BAR_W`), painting and flushing only the newly-revealed pixels.
/// Monotonic — calls with `target <= BAR_FILLED` are no-ops, so the
/// two progress sources can share state without coordinating.
fn advance_bar(target: u32) {
    if !INIT_DONE.load(Ordering::Relaxed) || FROZEN.load(Ordering::Relaxed) {
        return;
    }
    let fb = match fb_info() {
        Some(f) => f,
        None => return,
    };
    let target = target.min(BAR_W);
    let current = BAR_FILLED.load(Ordering::Relaxed);
    if target <= current {
        return;
    }

    // SAFETY: single-core EL2; BAR_X/BAR_Y/BAR_W_PAINTED immutable
    // post-init.
    let (bx, by, bw) = unsafe { (BAR_X, BAR_Y, BAR_W_PAINTED) };
    // Progress runs in the logical 0..BAR_W domain; scale to painted
    // pixels only here.
    let cur_px = current * bw / BAR_W;
    let tgt_px = target * bw / BAR_W;
    if tgt_px > cur_px {
        fill_rect(fb, bx + cur_px, by, tgt_px - cur_px, BAR_H, BAR_FG);
    }

    let row_bytes = fb.pitch as u64;
    let flush_pa = fb.pa + by as u64 * row_bytes;
    let flush_len = BAR_H as usize * fb.pitch as usize;
    crate::arch::cpu::dc_civac_range(flush_pa, flush_len);

    BAR_FILLED.store(target, Ordering::Relaxed);
}

fn paint_background(fb: &FbInfo) {
    fill_rect(fb, 0, 0, fb.width, fb.height, BG);
}

fn paint_logo(fb: &FbInfo) {
    // Position: horizontally centered, vertically centered on the
    // 1/3-down line.
    let logo_x = (fb.width as i32 - LOGO_W as i32) / 2;
    let logo_y = (fb.height as i32) / 3 - (LOGO_H as i32) / 2;
    if logo_x < 0 || logo_y < 0 {
        kprintln!(
            "splash: logo {}x{} doesn't fit panel {}x{}; skipping",
            LOGO_W, LOGO_H, fb.width, fb.height
        );
        return;
    }
    blit_logo(fb, logo_x as u32, logo_y as u32);
}

fn paint_empty_bar(fb: &FbInfo) -> (u32, u32, u32) {
    let bw = BAR_W.min(fb.width * 5 / 8);
    let bx = (fb.width - bw) / 2;
    let by = ((fb.height as i32) * 2 / 3 - (BAR_H as i32) / 2).max(0) as u32;
    fill_rect(fb, bx, by, bw, BAR_H, BAR_BG);
    (bx, by, bw)
}

/// Fill an axis-aligned rectangle with `color`. Clips to FB bounds.
/// No cache maintenance — caller flushes.
fn fill_rect(fb: &FbInfo, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let pixels_per_row = (fb.pitch / 4) as usize;
    let x_end = (x + w).min(fb.width) as usize;
    let y_end = (y + h).min(fb.height) as usize;
    let x = x as usize;
    let y = y as usize;
    if x >= x_end || y >= y_end {
        return;
    }
    let ptr = fb.pa as *mut u32;
    for py in y..y_end {
        let row_base = py * pixels_per_row;
        for px in x..x_end {
            // SAFETY: x_end ≤ fb.width and y_end ≤ fb.height by the
            // clamps above; ptr at fb.pa is fb.size bytes valid.
            unsafe {
                ptr.add(row_base + px).write_volatile(color);
            }
        }
    }
}

/// Blit the raw RGB logo blob onto the FB at `(x0, y0)`. Source layout:
/// row-major, LOGO_W*3 bytes per row, byte 0 = R, byte 1 = G, byte 2 = B.
fn blit_logo(fb: &FbInfo, x0: u32, y0: u32) {
    let pixels_per_row = (fb.pitch / 4) as usize;
    let ptr = fb.pa as *mut u32;
    let row_bytes = LOGO_W as usize * 3;
    let x_end = (x0 + LOGO_W).min(fb.width) as usize;
    let y_end = (y0 + LOGO_H).min(fb.height) as usize;
    for py in y0 as usize..y_end {
        let src_row = (py - y0 as usize) * row_bytes;
        let dst_row = py * pixels_per_row;
        for px in x0 as usize..x_end {
            let src_off = src_row + (px - x0 as usize) * 3;
            let r = LOGO_BYTES[src_off];
            let g = LOGO_BYTES[src_off + 1];
            let b = LOGO_BYTES[src_off + 2];
            let color = u32::from_le_bytes([r, g, b, 0]);
            // SAFETY: x_end / y_end clamped to fb bounds above.
            unsafe {
                ptr.add(dst_row + px).write_volatile(color);
            }
        }
    }
}
