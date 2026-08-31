//! Newton main-display driver — screen-class native primitives.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x04. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteScreenDriverNative`
//! (Emulator/TNativePrimitives.cpp:1564):
//!   0x01  TMainDisplayDriver::Delete        — r0 = 0
//!   0x03  TMainDisplayDriver::GetScreenInfo — fills struct at r1
//!   0x04  TMainDisplayDriver::PowerInit     — r0 = 0
//!   0x05  TMainDisplayDriver::PowerOn       — r0 = 0
//!   0x06  TMainDisplayDriver::PowerOff      — r0 = 0
//!   0x07  TMainDisplayDriver::Blit          — copy PixelMap into FB
//!   0x08  TMainDisplayDriver::GetFeature    — returns per-feature id
//!   0x09  TMainDisplayDriver::SetFeature    — r0 = 0
//!   0x0A  TMainDisplayDriver::AutoAdjustFeatures — r0 = 0
//!
//! Unknown subfunctions halt loudly — the trip-wire for cases the
//! early-boot ROM fires that we haven't modelled yet.
//!
//! The blit is the one piece with real state: it reads the PixelMap
//! descriptor, source rect, and destination rect from guest memory,
//! then copies the visible bitmap bytes into `GUEST_FB`. We treat
//! guest VAs as PAs (identity) on the assumption that the call
//! happens with the Newton kernel's MMU mapping RAM identity-first;
//! that holds for the guest-tests and for Einstein's own captures.
//! Post-MMU ROM boots that set up a non-identity map will need an
//! `AT S12E1R` translation step here.
//!
//! The MP2x00 main display panel is 320x480, 2 bpp packed (4 pixels
//! per byte, MSB-first; pixel 0 in bits 7..6, pixel 1 in bits 5..4,
//! …). Pixel values map to grays: 00 = white, 11 = black, with two
//! intermediate levels. We carry the pixels verbatim into GUEST_FB —
//! no inversion — and forward each blit through the [`BlitSink`]
//! installed by `main.rs` (the active `host::host_io` backend) to a
//! live host viewer for display.

use crate::{arch::cpu, hv::guest_mem, kprintln, peripherals::guest_access, arch::trap_context::TrapContext};
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct Screen;

impl NativeDriver for Screen {
    /// Screen-class driver ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0004;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        0x01 => {
            // Delete — no-op, success.
            ctx.x[0] = 0;
        }
        0x03 => {
            get_screen_info(ctx, pc);
        }
        0x04 | 0x05 | 0x06 | 0x0A => {
            // PowerInit / PowerOn / PowerOff / AutoAdjustFeatures —
            // no device to poke; return success.
            ctx.x[0] = 0;
        }
        0x07 => {
            blit(ctx, pc);
        }
        0x08 => {
            get_feature(ctx);
        }
        0x09 => {
            // SetFeature: contrast/backlight/orientation. We don't
            // have a display backend that distinguishes these, so just
            // accept the write (return 0).
            ctx.x[0] = 0;
        }
        _ => crate::diag::diag_util::halt_unknown_subfn(
            "screen", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}

/// `TMainDisplayDriver::GetFeature(feature_id)` — Einstein's table at
/// `Emulator/TNativePrimitives.cpp:1662`. We don't model contrast /
/// backlight / orientation runtime knobs, so we return the same
/// constants Einstein returns for a default un-configured ScreenManager:
/// contrast/backlight/orientation default to 0, "display present" = 1,
/// feature 5 = 0xA, anything else = 0xFFFFFFFF.
fn get_feature(ctx: &mut TrapContext) {
    let feature = ctx.x[1] as u32;
    let value: u32 = match feature {
        0 => 0,           // contrast (default off)
        1 => 1,           // display present
        2 => 0,           // backlight (default off)
        3 => 0,
        4 => 0,           // orientation (default upright)
        5 => 0xA,
        _ => 0xFFFF_FFFF, // unknown feature
    };
    ctx.x[0] = value as u64;
}

/// Geometry advertised to the guest on GetScreenInfo. Runtime so a
/// real-hardware build can derive Newton's screen size from the
/// negotiated HDMI panel (see `host_io::pi_fb::init`). Default
/// matches Einstein's MP2100 reply (320x480 / 2 bpp,
/// `TScreenManager::kBitsPerPixel`) which keeps QEMU / FVP /
/// guest-test builds behaving as before.
///
/// Width must be a multiple of 4 — the 2 bpp packing puts 4 pixels
/// in each byte and FB_ROW_BYTES must be an integer.
///
/// The OS-layer accepts any geometry: Einstein's `TScreenManager`
/// takes runtime `inPortraitWidth`/`inPortraitHeight`
/// (`Emulator/Screen/TScreenManager.h:122`), and the iOS app picks
/// `screenBounds / 2` for "Fit to Screen" at
/// `app/iEinstein/.../iEinsteinViewController.mm:362-367`.
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
static SCREEN_W: AtomicU32 = AtomicU32::new(320);
static SCREEN_H: AtomicU32 = AtomicU32::new(480);

pub const SCREEN_BPP: u32 = 2;

pub fn screen_width() -> u32 {
    SCREEN_W.load(Ordering::Relaxed)
}
pub fn screen_height() -> u32 {
    SCREEN_H.load(Ordering::Relaxed)
}
/// Bytes per packed scanline of the on-screen framebuffer.
/// width × 2 bpp / 8 = width / 4.
pub fn fb_row_bytes() -> u32 {
    screen_width() / 4
}
/// Set the Newton screen size that gets reported to the guest via
/// `GetScreenInfo`. Must be called before the guest first issues
/// the GetScreenInfo native primitive — in practice before the
/// guest's ERET in `kmain`. Width is clamped to a multiple of 4.
///
/// Called from `main.rs` boot wiring with the geometry the active
/// host-IO backend reports (`host_io::panel_geometry()`); backends
/// without a mandate (QEMU / FVP / guest-test) report `None` and the
/// 320×480 default stays in effect.
pub fn set_screen_size(w: u32, h: u32) {
    let w = w & !3;
    SCREEN_W.store(w, Ordering::Relaxed);
    SCREEN_H.store(h, Ordering::Relaxed);
}

/// Host blit sink: forwards one finished blit (parameters + packed
/// 2 bpp payload) to whatever the host displays on. Rects are
/// `(left, top, right, bottom)` in Newton pixels. Installed once from
/// `main.rs` with the active `host::host_io` backend's adapter; the
/// default drops blits (headless / uninstalled).
pub type BlitSink = fn(
    mode: u8,
    bpp: u8,
    src: (u16, u16, u16, u16),
    dst: (u16, u16, u16, u16),
    row_bytes: u16,
    payload: &[u8],
);

fn blit_sink_drop(
    _mode: u8,
    _bpp: u8,
    _src: (u16, u16, u16, u16),
    _dst: (u16, u16, u16, u16),
    _row_bytes: u16,
    _payload: &[u8],
) {
}

struct BlitSinkCell(core::cell::UnsafeCell<BlitSink>);
// SAFETY: written once by `install_blit_sink` from kmain on core 0
// before the guest runs; read-only afterwards from the single EL2
// trap handler.
unsafe impl Sync for BlitSinkCell {}

static BLIT_SINK: BlitSinkCell = BlitSinkCell(core::cell::UnsafeCell::new(blit_sink_drop));

/// Whether the installed sink consumes the packed payload slice.
/// Backends that repaint from GUEST_FB directly (pi_fb) or drop blits
/// (null) report false through `HostIo::wants_payload`; `blit` then
/// skips payload assembly and hands the sink an empty slice (the
/// blit-parameter metadata still flows).
static WANTS_PAYLOAD: AtomicBool = AtomicBool::new(true);

/// Install the host blit sink. Called once from `main.rs` boot wiring;
/// `wants_payload` carries the active backend's
/// `HostIo::wants_payload` answer down to `blit`.
pub fn install_blit_sink(sink: BlitSink, wants_payload: bool) {
    // SAFETY: single-core EL2, called before any blit can run.
    unsafe {
        *BLIT_SINK.0.get() = sink;
    }
    WANTS_PAYLOAD.store(wants_payload, Ordering::Relaxed);
}

/// Per-page stage-1 translation cache for the blit source walk. The
/// source address advances contiguously along a row, so a translation
/// stays valid until the VA crosses a 4 KiB page boundary; every
/// short-descriptor mapping size (section, large page, small page)
/// passes VA bits 11..0 through, so translating the page base once
/// covers the whole page. Keeps `translate_va`'s MMU-off identity
/// fallback (None → the VA is the PA).
struct PageTranslate {
    va_page: u32,
    pa_page: u32,
}

impl PageTranslate {
    fn new() -> Self {
        // u32::MAX is not 4 KiB-aligned, so the first lookup misses.
        Self { va_page: u32::MAX, pa_page: 0 }
    }

    fn pa_for(&mut self, va: u32) -> u32 {
        let page = va & !0xFFF;
        if page != self.va_page {
            self.pa_page = guest_mem::translate_va(page).unwrap_or(page);
            self.va_page = page;
        }
        self.pa_page | (va & 0xFFF)
    }
}

/// Maximum Newton screen pixels the blit scratch (and any other
/// fixed-size buffers downstream) needs to handle. Covers Newton
/// surfaces up to 1280×960 (over an 1920×1080 panel at scale=2 →
/// 960×540, or a 4K panel at scale=4 → 960×540). At 2 bpp that's
/// 300 KiB. `set_screen_size` will halt the boot if asked for a
/// larger surface.
pub const MAX_SCREEN_W: u32 = 1280;
pub const MAX_SCREEN_H: u32 = 960;

fn get_screen_info(ctx: &mut TrapContext, pc: u32) {
    let info_addr = ctx.x[1] as u32;
    // Layout per TNativePrimitives.cpp:1590-1598.
    let fields = [
        (0x00, screen_height()),
        (0x04, screen_width()),
        (0x08, SCREEN_BPP),
        (0x0C, 0x0000_0037), // unknown (Einstein verbatim)
        (0x10, 0x0064_0064), // resolution 100x100
        (0x14, 0x0000_0020), // unknown
        (0x18, 0x0000_0020), // unknown
    ];
    // r1 is a user VA — Tmux task @PC=0x801b84 (REx-side) calls
    // GetScreenInfo with a stack VA like 0x0cc77e70 that the guest
    // kernel has stage-1-mapped to an IPA in 0x040x_xxxx. Translate
    // through the live stage-1 walk; fall back to identity when the
    // MMU is off (guest-test runtime path).
    for (off, val) in fields {
        guest_access::write_word_or_halt(info_addr + off, val, "screen.GetScreenInfo", pc);
    }
    ctx.x[0] = 0;
}

/// TMainDisplayDriver::Blit(PixelMap*, Rect* src, Rect* dst, long mode).
///
/// Registers on entry:
///   r1 = PixelMap pointer
///   r2 = src SRect pointer    (four u16: top, left, bottom, right)
///   r3 = dst SRect pointer    (same layout)
///   [SP + 4] = mode (blit mode, ignored here)
///
/// PixelMap layout (struct NewtonPixmap in TNativePrimitives.cpp:68;
/// confirmed against TScreenManager::Blit @ Screen/TScreenManager.cpp):
///   +0x00  addy           — bitmap data pointer (guest VA)
///   +0x04  rowBytes_pkd   — packed; actual rowBytes = (word >> 16)
///   +0x08  pixmapTopLeft  — pixmap's coordinate-space origin
///                            (top in high 16, left in low 16);
///                            src rect is given in this same space
///                            and must be biased back to byte offsets
///                            inside `addy`
///   +0x10  flags
///   +0x14  table
///
/// We copy the row band [src.top, src.bottom) of the pixmap into
/// GUEST_FB. `rowBytes` already encodes the pixel-to-byte packing
/// (4 px/byte on a 2 bpp panel), so a byte-wise copy is correct
/// when `src.left` lands on a multiple of 4 px; sub-byte source
/// rects (text glyphs, narrow icons) and mode-1 merges fall to a
/// slow path that merges a destination byte at a time.
fn blit(ctx: &mut TrapContext, pc: u32) {
    // Emulation-cost accumulator (`nh_diag`): entry up to the host-io
    // push, so the paint cost (timed separately in
    // `host_io::push_guest_blit`) stays attributable on its own.
    let t_emu = crate::diag::blit_timing::begin();
    let pixmap_va = ctx.x[1] as u32;
    let src_rect_va = ctx.x[2] as u32;
    let dst_rect_va = ctx.x[3] as u32;

    let addy = guest_access::read_word_or_halt(pixmap_va, "pixmap.addy", pc);
    // rowBytes is in the HIGH 16 bits of the word at +0x04 (per
    // TScreenManager::Blit `srcRowBytes >> 16`). Iter-53 wedge:
    // reading the full word gave row_bytes = 0x00280000 — a 2.5 MB
    // stride that walked addy+(src_top*row_bytes) into unmapped
    // memory at 0xc64d000 within a few rows.
    let row_bytes_pkd = guest_access::read_word_or_halt(pixmap_va + 4, "pixmap.rowBytes_pkd", pc);
    let row_bytes = row_bytes_pkd >> 16;
    // pixmap origin: src/dst rects are in this coord space; subtract
    // to get byte offsets relative to `addy`.
    let pixmap_top_left = guest_access::read_word_or_halt(pixmap_va + 8, "pixmap.topLeft", pc);
    let pixmap_top = (pixmap_top_left >> 16) as u16;
    let pixmap_left = (pixmap_top_left & 0xFFFF) as u16;

    let (src_top, src_left, src_bottom, src_right) =
        read_rect(src_rect_va, "srcRect", pc);
    let (dst_top, dst_left, dst_bottom, dst_right) =
        read_rect(dst_rect_va, "dstRect", pc);

    if src_bottom < src_top || src_right < src_left {
        kprintln!(
            "*** screen.blit: degenerate srcRect ({},{},{},{}) @PC={:#x}",
            src_top, src_left, src_bottom, src_right, pc
        );
        cpu::halt();
    }

    // Bias src rect into pixmap-relative coordinates. Halt loud on
    // an out-of-bounds source — that's a kernel-data inconsistency
    // we want to see, not silently zero-blit.
    if src_top < pixmap_top || src_left < pixmap_left {
        kprintln!(
            "*** screen.blit: src ({},{}) outside pixmap origin ({},{}) @PC={:#x}",
            src_top, src_left, pixmap_top, pixmap_left, pc
        );
        cpu::halt();
    }
    let pixmap_src_top = src_top - pixmap_top;
    let pixmap_src_left = src_left - pixmap_left;

    let height = (src_bottom - src_top) as u32;

    // Up-front parameter dump so a partial blit halt downstream can
    // be correlated with the pixmap geometry that drove it.
    log_blit_enter(pc, pixmap_va, addy, row_bytes,
        pixmap_top, pixmap_left,
        src_top, src_left, src_bottom, src_right,
        dst_top, dst_left, dst_bottom, dst_right);

    // 2 bpp packing: each byte holds 4 pixels (pixel 0 in bits 7..6,
    // pixel 1 in 5..4, pixel 2 in 3..2, pixel 3 in 1..0).
    //
    // The guest is BE-8, so pixmap byte N lives at host PA `p + N` with
    // no lane transform — `guest_read_u8_pa` is a plain byte read. The
    // FB is hypervisor-managed linear-LE, host byte N being pixel byte N
    // in display order, so FB writes need no transform either.
    let src_width_pixels = (src_right - src_left) as u32;
    let fb_row_bytes = fb_row_bytes();

    // Blit mode (Einstein `Emulator/Screen/TScreenManager.cpp:280`):
    //   0 = srcCopy (default).
    //   1 = "darken only" / pen overlay — final = max(src, dst) per
    //       pixel under our "0=white .. 3=black" convention. Used to
    //       draw ink over existing content without erasing surrounding
    //       pixels — treating a mode=1 glyph blit as srcCopy would
    //       clear the rect around the ink.
    // Anything else falls back to srcCopy with a log.
    let mode = ctx_blit_mode(ctx, pc);
    if mode != 0 && mode != 1 {
        kprintln!("screen.blit: unrecognised mode {} @PC={:#x} — treating as srcCopy", mode, pc);
    }

    // 4 pixels per byte → alignment unit is 4. The fast path does
    // byte-granular GUEST_FB writes, which requires BOTH source and
    // destination to land on a 4-pixel boundary; otherwise we'd
    // corrupt pixels left of `dst_left` (or right of dst_left+width-1).
    // Mode 1 also forces the slow path because its max() merge needs
    // the current dst pixels.
    let byte_aligned =
        mode == 0
        && (pixmap_src_left & 0x3) == 0
        && (src_width_pixels & 0x3) == 0
        && (dst_left & 0x3) == 0;

    // Scratch buffer used to assemble the contiguous 2-bpp payload that
    // gets forwarded to the host viewer at the end. Sized for the
    // worst case (full-screen redraw at the upper bound the runtime
    // screen size is allowed to reach). Single-threaded EL2 access,
    // no contention. Assembled only when the active sink consumes it
    // (`HostIo::wants_payload` — pi_fb repaints from GUEST_FB and null
    // drops blits, so both skip the whole write stream); the sink then
    // gets an empty payload slice with the metadata intact.
    const SCRATCH_LEN: usize = (MAX_SCREEN_W * MAX_SCREEN_H / 4) as usize;
    struct ScratchCell(core::cell::UnsafeCell<[u8; SCRATCH_LEN]>);
    // SAFETY: single-threaded EL2 trap handler.
    unsafe impl Sync for ScratchCell {}
    static SCRATCH: ScratchCell = ScratchCell(core::cell::UnsafeCell::new([0; SCRATCH_LEN]));
    // SAFETY: see ScratchCell.
    let scratch = unsafe { &mut *SCRATCH.0.get() };
    let wants_payload = WANTS_PAYLOAD.load(Ordering::Relaxed);

    let payload_row_bytes = (src_width_pixels * SCREEN_BPP).div_ceil(8) as usize;
    let payload_len = payload_row_bytes * height as usize;
    // Geometry tripwire — kept independent of `wants_payload` so a
    // blit that would overflow the scratch halts identically on every
    // backend.
    if payload_len > scratch.len() {
        kprintln!(
            "*** screen.blit: payload {} bytes exceeds scratch {}",
            payload_len, scratch.len()
        );
        cpu::halt();
    }

    if !byte_aligned {
        // Sub-byte rect (Newton UI passes 1-pixel-aligned glyph blits)
        // or a mode-1 ink merge. Works a destination byte at a time:
        // read the dst byte once, merge up to 4 2-bpp pixels with
        // shift/mask, write once. The per-pixel combining rule is
        // mode 1 = max(src, dst) under the 0=white..3=black
        // convention; everything else is srcCopy.
        if wants_payload {
            // Zero so the edge bytes' out-of-range pixel slots start
            // from a known state under the masked merges below.
            for b in &mut scratch[..payload_len] { *b = 0; }
        }
        let mut xlate = PageTranslate::new();
        if src_width_pixels > 0 {
            // Absolute dst pixel range covered by the blit (inclusive).
            let dst_first = dst_left as u32;
            let dst_last = dst_first + src_width_pixels - 1;
            for row in 0..height {
                let src_row_off = (pixmap_src_top as u32 + row) * row_bytes;
                let fb_row_base = (dst_top as u32 + row) * fb_row_bytes;
                let pay_row_off = row as usize * payload_row_bytes;
                // Consecutive pixels share a source byte — refetch
                // only when the byte offset moves.
                let mut src_off_cached = u32::MAX;
                let mut src_byte = 0u8;
                for dst_byte_idx in (dst_first / 4)..=(dst_last / 4) {
                    let fb_ipa = guest_mem::FB_IPA_BASE
                        .wrapping_add(fb_row_base + dst_byte_idx);
                    // The dst byte feeds the mode-1 max() merge and
                    // carries an edge byte's out-of-rect pixels — an
                    // out-of-range dst rect must halt (mirroring the
                    // src read below), not merge against a fabricated 0.
                    let mut fb_byte = match guest_mem::read_byte_pa(fb_ipa) {
                        Some(b) => b,
                        None => {
                            kprintln!(
                                "*** screen.blit: dst FB IPA {:#x} outside mapped regions",
                                fb_ipa
                            );
                            cpu::halt();
                        }
                    };
                    let pix_first = (dst_byte_idx * 4).max(dst_first);
                    let pix_last = (dst_byte_idx * 4 + 3).min(dst_last);
                    for dst_pix in pix_first..=pix_last {
                        let col_pix = dst_pix - dst_first;
                        let abs_src_pix = pixmap_src_left as u32 + col_pix;
                        let src_off = src_row_off + abs_src_pix / 4;
                        if src_off != src_off_cached {
                            let src_va = addy + src_off;
                            let src_pa = xlate.pa_for(src_va);
                            src_byte = match crate::hv::guest_endian::guest_read_u8_pa(src_pa) {
                                Some(b) => b,
                                None => {
                                    kprintln!(
                                        "*** screen.blit: src VA {:#x} → PA {:#x} outside mapped regions",
                                        src_va, src_pa
                                    );
                                    cpu::halt();
                                }
                            };
                            src_off_cached = src_off;
                        }
                        let src_shift = 6 - 2 * (abs_src_pix & 3) as u8;
                        let src_2bit = (src_byte >> src_shift) & 0x3;
                        let dst_shift = 6 - 2 * (dst_pix & 3) as u8;
                        let cur_dst_2bit = (fb_byte >> dst_shift) & 0x3;

                        // Combine per blit mode.
                        let final_2bit = match mode {
                            1 => src_2bit.max(cur_dst_2bit),
                            _ => src_2bit,
                        };
                        fb_byte = (fb_byte & !(0x3 << dst_shift))
                            | (final_2bit << dst_shift);

                        // Merge into payload scratch (replaces, not
                        // ORs — so mode-1 "no change" pixels carry the
                        // existing dst value through to the host
                        // viewer).
                        if wants_payload {
                            let pay_off = pay_row_off + (col_pix / 4) as usize;
                            let pay_shift = 6 - 2 * (col_pix & 3) as u8;
                            scratch[pay_off] = (scratch[pay_off] & !(0x3 << pay_shift))
                                | (final_2bit << pay_shift);
                        }
                    }
                    // Write the merged byte into GUEST_FB.
                    if !guest_mem::write_byte_pa(fb_ipa, fb_byte) {
                        kprintln!(
                            "*** screen.blit: FB IPA {:#x} outside framebuffer",
                            fb_ipa
                        );
                        cpu::halt();
                    }
                }
            }
        }
        log_blit(pc, addy, row_bytes, height,
            src_top, src_left, src_bottom, src_right,
            dst_top, dst_left, dst_bottom, dst_right,
            src_width_pixels * height);
        crate::diag::blit_timing::EMULATE.record_since(t_emu);
        push_blit_event(
            mode,
            src_top, src_left, src_bottom, src_right,
            dst_top, dst_left, dst_bottom, dst_right,
            payload_row_bytes as u16,
            if wants_payload { &scratch[..payload_len] } else { &[] },
        );
        ctx.x[0] = 0;
        return;
    }

    // Byte-aligned fast path: one stage-1 translation per source page
    // (the row address is a guest VA when stage-1 is on; when it's off
    // — the guest-test runtime — `PageTranslate` keeps the identity
    // fallback) and one region lookup per contiguous span, copied in
    // bulk.
    let src_col0_byte = pixmap_src_left as u32 / 4;
    let src_width_bytes = src_width_pixels / 4;

    let mut xlate = PageTranslate::new();
    let mut copied = 0u32;
    for row in 0..height {
        let src_row_va = addy + (pixmap_src_top as u32 + row) * row_bytes + src_col0_byte;
        let fb_row_ipa = guest_mem::FB_IPA_BASE.wrapping_add(
            (dst_top as u32 + row) * fb_row_bytes + ((dst_left as u32) / 4),
        );
        let pay_row_off = row as usize * payload_row_bytes;
        // The GUEST_FB destination row is contiguous and never crosses
        // a region boundary; a failed resolve falls through to the
        // per-byte writes below, whose halt names the exact byte.
        let dst_host =
            guest_mem::host_slice_for(fb_row_ipa, src_width_bytes as usize, /*for_write=*/ true);
        // Walk the row in spans bounded by 4 KiB source pages — the
        // VA→PA translation is constant within a page.
        let mut done = 0u32;
        while done < src_width_bytes {
            let seg_va = src_row_va.wrapping_add(done);
            let seg_len = (0x1000 - (seg_va & 0xFFF)).min(src_width_bytes - done);
            let seg_pa = xlate.pa_for(seg_va);
            let src_host =
                guest_mem::host_slice_for(seg_pa, seg_len as usize, /*for_write=*/ false);
            let bulk = match (src_host, dst_host) {
                (Some(s), Some(d)) => {
                    let d = d + done as usize;
                    // A source span overlapping the destination row
                    // (an FB→FB self-blit) keeps the per-byte
                    // ascending copy order.
                    let overlap =
                        s < d + seg_len as usize && d < s + seg_len as usize;
                    if overlap { None } else { Some((s, d)) }
                }
                _ => None,
            };
            match bulk {
                Some((s, d)) => {
                    // SAFETY: both spans are bounds-checked by
                    // host_slice_for against their backing regions and
                    // proven non-overlapping above.
                    let ok = unsafe {
                        crate::hv::guest_endian::guest_copy_from_pa(
                            s as *const u8, seg_pa, d as *mut u8, seg_len as usize,
                        )
                    };
                    if !ok {
                        kprintln!(
                            "*** screen.blit: src VA {:#x} → PA {:#x} outside mapped regions",
                            seg_va, seg_pa
                        );
                        cpu::halt();
                    }
                    if wants_payload {
                        // The payload mirrors GUEST_FB byte-for-byte on
                        // the aligned path — copy from the freshly
                        // written destination span.
                        // SAFETY: pay_row_off + done + seg_len ≤
                        // payload_len ≤ SCRATCH_LEN (guarded above);
                        // scratch and GUEST_FB are distinct statics.
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                d as *const u8,
                                scratch.as_mut_ptr().add(pay_row_off + done as usize),
                                seg_len as usize,
                            );
                        }
                    }
                }
                None => {
                    // Span outside a single region (or self-overlapping)
                    // — per-byte copy, halting on the exact failing byte.
                    for i in 0..seg_len {
                        let src_va = seg_va.wrapping_add(i);
                        let src_pa = seg_pa.wrapping_add(i);
                        let byte = match crate::hv::guest_endian::guest_read_u8_pa(src_pa) {
                            Some(b) => b,
                            None => {
                                kprintln!(
                                    "*** screen.blit: src VA {:#x} → PA {:#x} outside mapped regions",
                                    src_va, src_pa
                                );
                                cpu::halt();
                            }
                        };
                        if wants_payload {
                            scratch[pay_row_off + (done + i) as usize] = byte;
                        }
                        let fb_ipa = fb_row_ipa.wrapping_add(done + i);
                        if !guest_mem::write_byte_pa(fb_ipa, byte) {
                            kprintln!(
                                "*** screen.blit: FB IPA {:#x} outside framebuffer",
                                fb_ipa
                            );
                            cpu::halt();
                        }
                    }
                }
            }
            done += seg_len;
        }
        copied += src_width_bytes;
    }

    log_blit(pc, addy, row_bytes, height,
        src_top, src_left, src_bottom, src_right,
        dst_top, dst_left, dst_bottom, dst_right,
        copied);

    crate::diag::blit_timing::EMULATE.record_since(t_emu);
    push_blit_event(
        mode,
        src_top, src_left, src_bottom, src_right,
        dst_top, dst_left, dst_bottom, dst_right,
        payload_row_bytes as u16,
        if wants_payload { &scratch[..payload_len] } else { &[] },
    );

    ctx.x[0] = 0;
}

/// Source the blit mode from the guest stack slot [SP+4] per the
/// native-primitive ABI. Halts loudly on a read failure (the same
/// convention as the rest of the blit emulation) rather than silently
/// degrading a mode-1 ink overlay into a srcCopy rect-clear.
fn ctx_blit_mode(ctx: &TrapContext, pc: u32) -> u8 {
    // Einstein reads `GetRegister(13)` — the *current-mode* banked R13.
    // ctx.x[13] is SP_usr regardless of the trapping mode, so reading it
    // directly is the historical wrong-slot bug (see flash_driver.rs and
    // docs/QEMU_BUGS.md). Resolve the banked SP for the trapping mode via
    // SPSR_EL2 + Table D1-79; the mode word lives at [SP+4] (the caller
    // pushed the 4th arg there).
    let spsr: u64;
    // SAFETY: reading a sysreg has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, spsr_el2",
            out(reg) spsr,
            options(nomem, nostack, preserves_flags),
        );
    }
    let sp = crate::arch::banked::sp_for_mode(ctx, spsr as u32);
    guest_access::read_word_or_halt(sp.wrapping_add(4), "blit mode word [SP+4]", pc) as u8
}

#[allow(clippy::too_many_arguments)]
fn push_blit_event(
    mode: u8,
    src_top: u16, src_left: u16, src_bottom: u16, src_right: u16,
    dst_top: u16, dst_left: u16, dst_bottom: u16, dst_right: u16,
    row_bytes: u16, payload: &[u8],
) {
    // SAFETY: see BlitSinkCell.
    let sink = unsafe { *BLIT_SINK.0.get() };
    sink(
        mode,
        SCREEN_BPP as u8,
        (src_left, src_top, src_right, src_bottom),
        (dst_left, dst_top, dst_right, dst_bottom),
        row_bytes,
        payload,
    );
}

fn read_rect(rect_va: u32, what: &str, pc: u32) -> (u16, u16, u16, u16) {
    // Two packed u32s: first = (top << 16) | left, second = (bottom << 16) | right.
    let w0 = guest_access::read_word_or_halt(rect_va, what, pc);
    let w1 = guest_access::read_word_or_halt(rect_va + 4, what, pc);
    (
        (w0 >> 16) as u16,
        (w0 & 0xFFFF) as u16,
        (w1 >> 16) as u16,
        (w1 & 0xFFFF) as u16,
    )
}

fn log_blit(pc: u32, addy: u32, row_bytes: u32, height: u32,
    st: u16, sl: u16, sb: u16, sr: u16,
    dt: u16, dl: u16, db: u16, dr: u16,
    copied: u32,
) {
    static BUDGET: crate::diag::diag_util::LogBudget = crate::diag::diag_util::LogBudget::new(8);
    if BUDGET.allow() {
        kprintln!(
            "screen.blit @PC={:#x} addy={:#x} rowBytes={} h={} src=({},{},{},{}) dst=({},{},{},{}) copied={}",
            pc, addy, row_bytes, height,
            st, sl, sb, sr, dt, dl, db, dr, copied
        );
    }
}

fn log_blit_enter(pc: u32, pixmap_va: u32, addy: u32, row_bytes: u32,
    pmt: u16, pml: u16,
    st: u16, sl: u16, sb: u16, sr: u16,
    dt: u16, dl: u16, db: u16, dr: u16,
) {
    static BUDGET: crate::diag::diag_util::LogBudget = crate::diag::diag_util::LogBudget::new(8);
    if BUDGET.allow() {
        kprintln!(
            "screen.blit ENTER @PC={:#x} pixmap={:#x} addy={:#x} rowBytes={} pmTL=({},{}) src=({},{},{},{}) dst=({},{},{},{})",
            pc, pixmap_va, addy,
            row_bytes,
            pmt, pml,
            st, sl, sb, sr, dt, dl, db, dr,
        );
    }
}
