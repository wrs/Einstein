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
        _ => {
            kprintln!(
                "*** screen: unknown subfn {:#x} @PC={:#x} r1={:#x} r2={:#x} r3={:#x}",
                subfn, pc, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            cpu::halt();
        }
    }
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
    for (off, val) in fields {
        if !guest_mem::write_word_pa(info_addr + off, val) {
            kprintln!(
                "*** screen.GetScreenInfo: cannot write {:#x} @PC={:#x}",
                info_addr + off, pc
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
/// PixelMap layout (struct NewtonPixmap in TNativePrimitives.cpp:68):
///   +0x00  addy      — bitmap data pointer (guest VA)
///   +0x04  rowBytes  — bytes per source row
///   +0x08  bounds    — SRect {top, left, bottom, right}
///   +0x10  flags
///   +0x14  table
///
/// We copy the row band [src.top, src.bottom) of the pixmap into
/// GUEST_FB laid out identically for the destination rect. For 1-bpp
/// Newton panels, rowBytes already encodes the pixel-to-byte packing,
/// so a byte-wise copy is correct.
fn blit(ctx: &mut TrapContext, pc: u32) {
    let pixmap_va = ctx.x[1] as u32;
    let src_rect_va = ctx.x[2] as u32;
    let dst_rect_va = ctx.x[3] as u32;

    let addy = read_word_or_halt(pixmap_va, "pixmap.addy", pc);
    let row_bytes = read_word_or_halt(pixmap_va + 4, "pixmap.rowBytes", pc);

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

    let height = (src_bottom - src_top) as u32;

    // The destination rect tells us where in the FB the band lands.
    // We treat the FB as a linear bitmap whose row stride matches the
    // source pixmap — same packing, same byte-per-pixel — so the
    // copy is a straight (src -> fb) memmove per row.
    let fb_stride = row_bytes;
    let dst_row0_offset = (dst_top as u32) * fb_stride;

    let mut copied = 0u32;
    for row in 0..height {
        for col in 0..row_bytes {
            let src_va = addy + (src_top as u32 + row) * row_bytes + col;
            let byte = match guest_mem::read_byte_pa(src_va) {
                Some(b) => b,
                None => {
                    kprintln!(
                        "*** screen.blit: src VA {:#x} outside mapped regions",
                        src_va
                    );
                    cpu::halt();
                }
            };
            let fb_off = dst_row0_offset + row * fb_stride + col;
            let fb_ipa = guest_mem::FB_IPA_BASE.wrapping_add(fb_off);
            if !guest_mem::write_byte_pa(fb_ipa, byte) {
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
    match guest_mem::read_word_pa(va) {
        Some(v) => v,
        None => {
            kprintln!(
                "*** screen.blit: cannot read {} at VA {:#x} @PC={:#x}",
                what, va, pc
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
