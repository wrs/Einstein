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
//! `AT S12E1R` translation step here — Phase B when it fires.

use crate::{cpu, guest_mem, kprintln, trap::TrapContext};

/// Screen-class driver ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_0004;

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
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
        _ => {
            kprintln!(
                "*** screen: unknown subfn {:#x} @PC={:#x} r1={:#x} r2={:#x} r3={:#x}",
                subfn, pc, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            cpu::halt();
        }
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

/// Geometry advertised to the guest on GetScreenInfo. The values
/// aren't hot-path: the Newton's screen bring-up just uses them to
/// size its framebuffer bookkeeping. Matches Einstein's reply for
/// a 320x480 / 1 bpp MP2x00 panel (TScreenManager::kBitsPerPixel).
const SCREEN_WIDTH: u32 = 320;
const SCREEN_HEIGHT: u32 = 480;
const SCREEN_BPP: u32 = 1;

fn get_screen_info(ctx: &mut TrapContext, pc: u32) {
    let info_addr = ctx.x[1] as u32;
    // Layout per TNativePrimitives.cpp:1590-1598.
    let fields = [
        (0x00, SCREEN_HEIGHT),
        (0x04, SCREEN_WIDTH),
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
        let va = info_addr + off;
        let pa = guest_mem::translate_va(va).unwrap_or(va);
        if !crate::guest_endian::guest_write_u32_pa(pa, val) {
            kprintln!(
                "*** screen.GetScreenInfo: cannot write VA {:#x} (PA {:#x}) @PC={:#x}",
                va, pa, pc
            );
            cpu::halt();
        }
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
/// GUEST_FB. For 1-bpp Newton panels, rowBytes already encodes the
/// pixel-to-byte packing, so a byte-wise copy is correct (we don't
/// model bit-aligned masking like Einstein's `Blit_0` because the
/// Newton ROM aligns its src rects to byte boundaries on a 320-px
/// 1-bpp panel — `srcLeft` always lands on a multiple of 8).
fn blit(ctx: &mut TrapContext, pc: u32) {
    let pixmap_va = ctx.x[1] as u32;
    let src_rect_va = ctx.x[2] as u32;
    let dst_rect_va = ctx.x[3] as u32;

    let addy = read_word_or_halt(pixmap_va, "pixmap.addy", pc);
    // rowBytes is in the HIGH 16 bits of the word at +0x04 (per
    // TScreenManager::Blit `srcRowBytes >> 16`). Iter-53 wedge:
    // reading the full word gave row_bytes = 0x00280000 — a 2.5 MB
    // stride that walked addy+(src_top*row_bytes) into unmapped
    // memory at 0xc64d000 within a few rows.
    let row_bytes_pkd = read_word_or_halt(pixmap_va + 4, "pixmap.rowBytes_pkd", pc);
    let row_bytes = row_bytes_pkd >> 16;
    // pixmap origin: src/dst rects are in this coord space; subtract
    // to get byte offsets relative to `addy`.
    let pixmap_top_left = read_word_or_halt(pixmap_va + 8, "pixmap.topLeft", pc);
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

    // 1-bpp packing: each byte holds 8 pixels. Src starts at
    // addy + (pixmap_src_top * rowBytes) + (pixmap_src_left / 8).
    //
    // BE-32 word-invariant byte access: the Newton kernel writes
    // pixmap data as BE-32, so logical byte N at PA `p` lives at
    // host PA `p ^ 3` (within each 4-byte word). Mirror the convention
    // shadow_stub uses for in-guest LDRB (see `shadow_stub::XOR_LIMIT`
    // and `shadow_stub::dispatch_byte_read`). The FB itself is
    // hypervisor-managed linear-LE — host byte N is pixel byte N in
    // display order — so FB writes don't XOR.
    let src_width_pixels = (src_right - src_left) as u32;
    let fb_row_bytes = (SCREEN_WIDTH * SCREEN_BPP) / 8;

    let byte_aligned =
        (pixmap_src_left & 0x7) == 0 && (src_width_pixels & 0x7) == 0;

    if !byte_aligned {
        // Non-byte-aligned blit (Newton UI passes sub-byte rects for
        // text glyphs and small graphics). Per-pixel read/inverted-
        // write: read source bit, write into the dst byte's matching
        // bit position. Slow vs Einstein's word-mask Blit_0, but we
        // run this only on cold-boot UI rendering — correctness is
        // the priority here, not throughput.
        let mut copied = 0u32;
        for row in 0..height {
            let src_row_pa_off = (pixmap_src_top as u32 + row) * row_bytes;
            for col_pix in 0..src_width_pixels {
                let abs_src_pix = pixmap_src_left as u32 + col_pix;
                let src_va = addy + src_row_pa_off + abs_src_pix / 8;
                let src_pa = guest_mem::translate_va(src_va).unwrap_or(src_va);
                // Read the kernel's logical byte at this PA. The XOR-3
                // byte-lane transform is applied internally by
                // `guest_endian::guest_read_u8_pa` (see top-of-blit
                // comment).
                let byte = match crate::guest_endian::guest_read_u8_pa(src_pa) {
                    Some(b) => b,
                    None => {
                        kprintln!(
                            "*** screen.blit: src VA {:#x} → PA {:#x} outside mapped regions",
                            src_va, src_pa
                        );
                        cpu::halt();
                    }
                };
                // Newton 1-bpp bit ordering within the (now logical)
                // byte: pixel 0 is bit 7 (MSB). Extract this column's
                // bit, then INVERT (Newton's 1=pen-pressed → host FB
                // 1=white).
                let src_bit = (byte >> (7 - (abs_src_pix & 7))) & 1;
                let fb_bit = src_bit ^ 1;

                let dst_pix = dst_left as u32 + col_pix;
                let fb_off = (dst_top as u32 + row) * fb_row_bytes + dst_pix / 8;
                let fb_ipa = guest_mem::FB_IPA_BASE.wrapping_add(fb_off);
                let mut fb_byte = guest_mem::read_byte_pa(fb_ipa).unwrap_or(0);
                let bit_pos = 7 - (dst_pix & 7) as u8;
                let bit_mask = 1u8 << bit_pos;
                if fb_bit != 0 {
                    fb_byte |= bit_mask;
                } else {
                    fb_byte &= !bit_mask;
                }
                if !guest_mem::write_byte_pa(fb_ipa, fb_byte) {
                    kprintln!(
                        "*** screen.blit: FB IPA {:#x} outside framebuffer",
                        fb_ipa
                    );
                    cpu::halt();
                }
                copied += 1;
            }
        }
        log_blit(pc, addy, row_bytes, height,
            src_top, src_left, src_bottom, src_right,
            dst_top, dst_left, dst_bottom, dst_right,
            copied);
        crate::fb_dump::mark_dirty();
        ctx.x[0] = 0;
        return;
    }

    // Byte-aligned fast path.
    let src_col0_byte = (pixmap_src_left / 8) as u32;
    let src_width_bytes = src_width_pixels / 8;

    let mut copied = 0u32;
    for row in 0..height {
        let src_row = addy + (pixmap_src_top as u32 + row) * row_bytes + src_col0_byte;
        for col in 0..src_width_bytes {
            let src_va = src_row + col;
            // src_va is a guest VA when stage-1 is on (post-MMU
            // Newton boot); when stage-1 is off (guest-test runtime),
            // VA is treated as PA via the identity. translate_va
            // returns None in the MMU-off case; fall back to identity
            // so guest-tests' MMU-off paths still work.
            let src_pa = guest_mem::translate_va(src_va).unwrap_or(src_va);
            // Read the kernel's logical byte at this PA. The XOR-3
            // byte-lane transform is applied internally by
            // `guest_endian::guest_read_u8_pa` (see top-of-blit
            // comment).
            let byte = match crate::guest_endian::guest_read_u8_pa(src_pa) {
                Some(b) => b,
                None => {
                    kprintln!(
                        "*** screen.blit: src VA {:#x} → PA {:#x} outside mapped regions",
                        src_va, src_pa
                    );
                    cpu::halt();
                }
            };
            // Newton 1-bpp pixmaps are stored INVERTED (1 = white,
            // 0 = black) relative to the host framebuffer convention
            // we use; Einstein's Blit_0 flips with `~chunk` for
            // srcCopy mode. Mirror that here so the FB dumps look
            // right.
            let fb_byte = !byte;
            let fb_off = (dst_top as u32 + row) * fb_row_bytes
                + ((dst_left as u32) / 8) + col;
            let fb_ipa = guest_mem::FB_IPA_BASE.wrapping_add(fb_off);
            if !guest_mem::write_byte_pa(fb_ipa, fb_byte) {
                kprintln!(
                    "*** screen.blit: FB IPA {:#x} outside framebuffer",
                    fb_ipa
                );
                cpu::halt();
            }
            copied += 1;
        }
    }

    log_blit(pc, addy, row_bytes, height,
        src_top, src_left, src_bottom, src_right,
        dst_top, dst_left, dst_bottom, dst_right,
        copied);

    crate::fb_dump::mark_dirty();

    ctx.x[0] = 0;
}

fn read_word_or_halt(va: u32, what: &str, pc: u32) -> u32 {
    // VA-aware in MMU-on mode (Newton boot); identity in MMU-off mode
    // (guest tests) — `translate_va` returns None when SCTLR.M=0.
    let pa = guest_mem::translate_va(va).unwrap_or(va);
    match crate::guest_endian::guest_read_u32_pa(pa) {
        Some(v) => v,
        None => {
            kprintln!(
                "*** screen.blit: cannot read {} at VA {:#x} (PA {:#x}) @PC={:#x}",
                what, va, pa, pc
            );
            cpu::halt();
        }
    }
}

fn read_rect(rect_va: u32, what: &str, pc: u32) -> (u16, u16, u16, u16) {
    // Two packed u32s: first = (top << 16) | left, second = (bottom << 16) | right.
    let w0 = read_word_or_halt(rect_va, what, pc);
    let w1 = read_word_or_halt(rect_va + 4, what, pc);
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
    use core::sync::atomic::{AtomicUsize, Ordering};
    static BUDGET: AtomicUsize = AtomicUsize::new(0);
    const MAX: usize = 8;
    let n = BUDGET.fetch_add(1, Ordering::Relaxed);
    if n < MAX {
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
    use core::sync::atomic::{AtomicUsize, Ordering};
    static BUDGET: AtomicUsize = AtomicUsize::new(0);
    const MAX: usize = 8;
    let n = BUDGET.fetch_add(1, Ordering::Relaxed);
    if n < MAX {
        kprintln!(
            "screen.blit ENTER @PC={:#x} pixmap={:#x} addy={:#x} rowBytes={} pmTL=({},{}) src=({},{},{},{}) dst=({},{},{},{})",
            pc, pixmap_va, addy,
            row_bytes,
            pmt, pml,
            st, sl, sb, sr, dt, dl, db, dr,
        );
    }
}
