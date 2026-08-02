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

/// Allocate a framebuffer at the panel's native size, 32 bpp, RGB
/// pixel order. Returns metadata for later blits.
///
/// Fallback when the panel doesn't report a size (e.g. HDMI not
/// negotiated, headless boot): use 1024×768 so we still produce a
/// visible image if a monitor is later attached during the run.
///
/// **Modeset-reset dance.** The firmware's initial HDMI modeset
/// (driven by config.txt / EDID at boot) leaves a thin white bar
/// across the top of the picture and intermittent link flicker on
/// the Pi Zero 2 W + 1024×600 panel we ship against. Raspbian shows
/// the same symptoms until KMS later does its own modeset, which
/// clears them. We replicate that: allocate the framebuffer once
/// (the rough firmware modeset), release it, then allocate again —
/// the second allocation provokes a fresh modeset that comes out
/// clean. Cheap (two extra mailbox round-trips); no-op on platforms
/// where the firmware modeset is already good.
pub fn alloc_native() -> Result<FbInfo, FbError> {
    let (panel_w, panel_h) = mailbox::fb_get_physical_size()?;
    let (w, h) = if panel_w == 0 || panel_h == 0 {
        kprintln!(
            "display: panel reported size=0; falling back to 1024x768"
        );
        (1024, 768)
    } else {
        (panel_w, panel_h)
    };

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
