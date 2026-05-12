//! VideoCore mailbox property-channel client.
//!
//! Used by the SDHOST driver to query / set the SoC core clock rate
//! (which is the SDHOST's input clock — SDCDIV is the only further
//! divider). Phase 4 (display) will reuse the same module to allocate
//! a framebuffer; that's why this lives at the crate root rather than
//! inside `src/sd/`.
//!
//! ## Hardware reference
//!
//! BCM2710 mailbox 0/1 at `0x3F00_B880`. Two mailboxes:
//! - mailbox 0 is VC → ARM (read).
//! - mailbox 1 is ARM → VC (write).
//!
//! Register layout (offsets from `0x3F00_B880`):
//!
//! ```text
//!   0x00  READ    R    pop a 32-bit message from mailbox 0
//!   0x10  POLL    R    peek mailbox 0 without popping
//!   0x14  SENDER  R    last sender on mailbox 0
//!   0x18  STATUS  R    bit 31 = mailbox 1 full, bit 30 = mailbox 0 empty
//!   0x1C  CONFIG  R/W  enable/disable interrupts (we don't use them)
//!   0x20  WRITE   W    push a 32-bit message onto mailbox 1
//! ```
//!
//! Each 32-bit message packs `(value << 4) | channel` — bottom 4 bits
//! identify the destination channel, top 28 bits carry data (typically
//! a buffer pointer right-shifted by 4, since the buffer must be
//! 16-byte aligned).
//!
//! ## Property channel protocol (channel 8)
//!
//! The buffer is laid out as:
//!
//! ```text
//!   u32 buffer_size_in_bytes
//!   u32 request_code (0 on request; high bit set on response success)
//!   ... one or more tags ...
//!   u32 end_tag (0)
//!   [pad to 16-byte aligned size]
//! ```
//!
//! Each tag is:
//!
//! ```text
//!   u32 tag_id
//!   u32 value_buffer_size_bytes
//!   u32 request_code (0 on request)
//!   u8[value_buffer_size_bytes] value
//!   [pad to next 4-byte aligned position]
//! ```
//!
//! ## Bus address translation
//!
//! The VC accesses DRAM through its own MMU. The ARM CPU passes a
//! **bus address**, not a physical address, in the mailbox message.
//! On BCM2710 with `arm_64bit=1`:
//!
//! - `bus = pa | 0xC000_0000` — L2-coherent, uncached. We use this.
//! - `bus = pa | 0x4000_0000` — L2-cached. Would require us to also
//!   manage VC L2 maintenance, which is messy from EL2.
//!
//! ## Cache coherency
//!
//! Our buffer is on the stack — DRAM, mapped Normal Write-Back at
//! EL2 stage-1. The VC reads through the uncached alias, so we must
//! clean our cache lines to the PoC before the doorbell write and
//! invalidate after the response, or we'll exchange stale bytes in
//! both directions. [`crate::cpu::dc_civac_range`] does both in one
//! pass; we call it before and after.

#![allow(dead_code)] // SDHOST uses one tag today; Phase 4 will add more.

use core::ptr::{read_volatile, write_volatile};

use crate::cpu::dc_civac_range;

const MAILBOX_BASE: usize = 0x3F00_B880;
const MBOX_READ: *mut u32 = (MAILBOX_BASE + 0x00) as *mut u32;
const MBOX_STATUS: *mut u32 = (MAILBOX_BASE + 0x18) as *mut u32;
const MBOX_WRITE: *mut u32 = (MAILBOX_BASE + 0x20) as *mut u32;

const STATUS_FULL: u32 = 1 << 31; // mailbox 1 full — cannot write.
const STATUS_EMPTY: u32 = 1 << 30; // mailbox 0 empty — cannot read.

const CHANNEL_PROPERTY: u32 = 8;

const REQUEST_CODE: u32 = 0;
const RESPONSE_SUCCESS: u32 = 0x8000_0000;
const RESPONSE_ERROR: u32 = 0x8000_0001;

/// Bus-address tag bit. ANDing this in turns a u32 PA into the
/// VC-bus uncached alias.
const BUS_UNCACHED: u32 = 0xC000_0000;

/// Property-tag IDs we currently use. Add as needed; the full
/// catalogue lives at
/// <https://github.com/raspberrypi/firmware/wiki/Mailbox-property-interface>.
pub const TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;
pub const TAG_SET_CLOCK_RATE: u32 = 0x0003_8002;
pub const TAG_GET_CLOCK_RATE_MEASURED: u32 = 0x0003_0047;

/// Framebuffer property tags. See the firmware-wiki link above.
pub const TAG_FB_ALLOCATE: u32 = 0x0004_0001;
pub const TAG_FB_GET_PHYSICAL_W_H: u32 = 0x0004_0003;
pub const TAG_FB_SET_PHYSICAL_W_H: u32 = 0x0004_8003;
pub const TAG_FB_SET_VIRTUAL_W_H: u32 = 0x0004_8004;
pub const TAG_FB_SET_DEPTH: u32 = 0x0004_8005;
pub const TAG_FB_SET_PIXEL_ORDER: u32 = 0x0004_8006;
pub const TAG_FB_GET_PITCH: u32 = 0x0004_0008;
pub const TAG_FB_SET_VIRTUAL_OFFSET: u32 = 0x0004_8009;

/// Power-state tag — request `(device, state)`, response same
/// shape. State bit 0 = on(1)/off(0), bit 1 = block until the
/// power state has been reached. On response bit 1 means "no
/// such device" (NOT "wait" any more — sense flips). See Circle's
/// `bcmpropertytags.h`.
pub const TAG_SET_POWER_STATE: u32 = 0x0002_8001;
pub const DEVICE_ID_USB_HCD: u32 = 3;
pub const POWER_STATE_OFF: u32 = 0;
pub const POWER_STATE_ON: u32 = 1 << 0;
pub const POWER_STATE_WAIT: u32 = 1 << 1;
pub const POWER_STATE_NO_DEVICE: u32 = 1 << 1; // response only

/// Clock-ID constants for `TAG_*_CLOCK_RATE`.
pub const CLOCK_ID_EMMC: u32 = 1;
pub const CLOCK_ID_UART: u32 = 2;
pub const CLOCK_ID_ARM: u32 = 3;
pub const CLOCK_ID_CORE: u32 = 4;

#[derive(Debug, Clone, Copy)]
pub enum MailboxError {
    /// Firmware did not respond within our polling window.
    Timeout,
    /// Firmware acked the message but the response code wasn't
    /// `0x8000_0000`.
    FirmwareError,
    /// A tag came back with the high bit of `value_buffer_size`
    /// unset — i.e. firmware didn't recognise / didn't fill it.
    TagNotHandled,
}

/// 16-byte-aligned buffer wrapping one property request.
///
/// We use a fixed 64-word buffer. The largest single-tag message
/// (get-or-set clock) needs ~8 words; the multi-tag FB setup
/// request needs ~35. The buffer is on the stack so each call gets
/// a fresh, uncontended copy — no global state to lock, no
/// re-entrancy hazard.
#[repr(C, align(16))]
struct Buffer {
    words: [u32; 64],
}

impl Buffer {
    const fn new() -> Self {
        Self { words: [0; 64] }
    }
}

/// Common doorbell path: flush the buffer to the PoC, post the
/// bus address to the mailbox, wait for the channel-8 reply, and
/// invalidate so the buffer holds the firmware's response on
/// return. Caller has already filled the buffer and set
/// `total_bytes_padded` in `words[0]`.
fn mailbox_call(
    buf: &mut Buffer,
    total_bytes_padded: u32,
) -> Result<(), MailboxError> {
    let buf_ptr = buf.words.as_ptr() as u64;
    let pa: u32 = buf_ptr as u32;
    debug_assert!(pa & 0xF == 0, "mailbox buffer must be 16-byte aligned");
    let bus_addr = pa | BUS_UNCACHED;

    // Clean our writes to the PoC so the VC sees them.
    dc_civac_range(buf_ptr, (total_bytes_padded as usize).max(16));

    // SAFETY: MMIO at the documented mailbox addresses on the
    // BCM2710 peripheral window, identity-mapped Device-nGnRE by
    // `mmu::init`. Single-core EL2; no concurrency.
    unsafe {
        for _ in 0..10_000_000 {
            if read_volatile(MBOX_STATUS) & STATUS_FULL == 0 {
                break;
            }
        }
        if read_volatile(MBOX_STATUS) & STATUS_FULL != 0 {
            return Err(MailboxError::Timeout);
        }

        write_volatile(MBOX_WRITE, bus_addr | CHANNEL_PROPERTY);

        for _ in 0..10_000_000 {
            if read_volatile(MBOX_STATUS) & STATUS_EMPTY != 0 {
                continue;
            }
            let m = read_volatile(MBOX_READ);
            if m & 0xF == CHANNEL_PROPERTY {
                break;
            }
        }
    }

    dc_civac_range(buf_ptr, (total_bytes_padded as usize).max(16));

    if buf.words[1] != RESPONSE_SUCCESS {
        return Err(MailboxError::FirmwareError);
    }
    Ok(())
}

/// Send a one-tag property request and return the first response
/// word. `arg_words` carries the request payload; the response
/// overwrites it in place. The caller passes the *number* of u32s in
/// the value buffer (not bytes) for both lanes.
fn send_one_tag(tag_id: u32, arg_words: &mut [u32]) -> Result<(), MailboxError> {
    // Layout: hdr[0..2] + tag_hdr[0..3] + payload[..] + end_tag.
    let payload_bytes: u32 = (arg_words.len() as u32) * 4;
    let total_words: usize = 2 + 3 + arg_words.len() + 1;
    assert!(total_words <= 64, "mailbox buffer too small for this tag");
    let total_bytes = (total_words as u32) * 4;
    let total_bytes_padded = (total_bytes + 15) & !15;

    let mut buf = Buffer::new();
    buf.words[0] = total_bytes_padded;
    buf.words[1] = REQUEST_CODE;
    buf.words[2] = tag_id;
    buf.words[3] = payload_bytes;
    buf.words[4] = REQUEST_CODE;
    for (i, &w) in arg_words.iter().enumerate() {
        buf.words[5 + i] = w;
    }
    buf.words[5 + arg_words.len()] = 0; // end tag.

    mailbox_call(&mut buf, total_bytes_padded)?;

    // Per the property-interface spec, the per-tag response indicator
    // lives in the tag's third header word (request_code, which the
    // firmware turns into response_code by setting bit 31). That's
    // buf.words[4] in our layout — buf.words[3] is the value-buffer
    // size, unchanged by the firmware.
    if buf.words[4] & 0x8000_0000 == 0 {
        return Err(MailboxError::TagNotHandled);
    }
    for (i, slot) in arg_words.iter_mut().enumerate() {
        *slot = buf.words[5 + i];
    }
    Ok(())
}

/// Set the power state of a SoC device. Returns the state the
/// firmware reports after applying. With `POWER_STATE_WAIT` set,
/// the firmware blocks until the rail has stabilised; without it
/// the call returns immediately and the caller is expected to
/// delay. The DWC2 USB HCD wants the rail stable before any
/// register access, so always pass WAIT.
pub fn set_power_state(device_id: u32, state: u32) -> Result<u32, MailboxError> {
    let mut payload = [device_id, state];
    send_one_tag(TAG_SET_POWER_STATE, &mut payload)?;
    Ok(payload[1])
}

/// Query the current rate of a clock ID. Returns Hz.
pub fn get_clock_rate(clock_id: u32) -> Result<u32, MailboxError> {
    let mut payload = [clock_id, 0];
    send_one_tag(TAG_GET_CLOCK_RATE, &mut payload)?;
    Ok(payload[1])
}

/// Ask firmware to set the rate of a clock ID. Returns the actual
/// rate the firmware programmed (may differ from `hz`).
pub fn set_clock_rate(clock_id: u32, hz: u32) -> Result<u32, MailboxError> {
    let mut payload = [clock_id, hz, 0 /* skip_setting_turbo */];
    send_one_tag(TAG_SET_CLOCK_RATE, &mut payload)?;
    Ok(payload[1])
}

// ---- Framebuffer helpers -------------------------------------------
//
// Each helper is a single property-tag call. The conventional Pi
// idiom batches the FB setup tags into one request (to save five
// mailbox round-trips), but in polled mode round-trips are cheap
// and one-tag-per-call is much easier to debug — if any step
// fails, the call that returned the error is the one that failed.

/// Query the panel's currently configured physical width × height
/// (the mode HDMI is delivering). Returns `(width, height)` in
/// pixels.
pub fn fb_get_physical_size() -> Result<(u32, u32), MailboxError> {
    let mut p = [0u32, 0u32];
    send_one_tag(TAG_FB_GET_PHYSICAL_W_H, &mut p)?;
    Ok((p[0], p[1]))
}

/// Set the framebuffer's *physical* (displayed) dimensions in
/// pixels. Should match the panel for crisp output. Returns the
/// dimensions firmware actually configured.
pub fn fb_set_physical_size(w: u32, h: u32) -> Result<(u32, u32), MailboxError> {
    let mut p = [w, h];
    send_one_tag(TAG_FB_SET_PHYSICAL_W_H, &mut p)?;
    Ok((p[0], p[1]))
}

/// Set the framebuffer's *virtual* (back-buffer) dimensions. Usually
/// equal to the physical size unless you want pan/scroll. Returns
/// the dimensions firmware actually configured.
pub fn fb_set_virtual_size(w: u32, h: u32) -> Result<(u32, u32), MailboxError> {
    let mut p = [w, h];
    send_one_tag(TAG_FB_SET_VIRTUAL_W_H, &mut p)?;
    Ok((p[0], p[1]))
}

/// Set pixel depth in bits/pixel (16 or 32 typical). Returns the
/// depth firmware actually configured.
pub fn fb_set_depth(bits: u32) -> Result<u32, MailboxError> {
    let mut p = [bits];
    send_one_tag(TAG_FB_SET_DEPTH, &mut p)?;
    Ok(p[0])
}

/// Set the byte order of each pixel. 0 = BGR, 1 = RGB.
pub fn fb_set_pixel_order(order: u32) -> Result<u32, MailboxError> {
    let mut p = [order];
    send_one_tag(TAG_FB_SET_PIXEL_ORDER, &mut p)?;
    Ok(p[0])
}

/// Set the virtual-offset (pan) of the visible region in the
/// virtual framebuffer. Returns the offset firmware actually
/// applied.
pub fn fb_set_virtual_offset(x: u32, y: u32) -> Result<(u32, u32), MailboxError> {
    let mut p = [x, y];
    send_one_tag(TAG_FB_SET_VIRTUAL_OFFSET, &mut p)?;
    Ok((p[0], p[1]))
}

/// Allocate the framebuffer. `alignment` is requested in bytes;
/// the firmware honours alignments at least up to 4 KiB. Returns
/// `(bus_addr, size_bytes)` — `bus_addr` is the VC-bus form and
/// usually has the L2-cached alias bit (`0x40000000`) set. Mask
/// off the upper alias bits (`& 0x3FFF_FFFF` on a Pi 3+ with VC L2
/// disabled) to get the ARM physical address.
pub fn fb_allocate(alignment: u32) -> Result<(u32, u32), MailboxError> {
    let mut p = [alignment, 0];
    send_one_tag(TAG_FB_ALLOCATE, &mut p)?;
    Ok((p[0], p[1]))
}

/// Query the row stride (bytes per scanline) of the currently-
/// allocated framebuffer. Always ≥ `width * bpp / 8`; the firmware
/// may pad each row out for alignment.
pub fn fb_get_pitch() -> Result<u32, MailboxError> {
    let mut p = [0u32];
    send_one_tag(TAG_FB_GET_PITCH, &mut p)?;
    Ok(p[0])
}

/// Result of [`fb_setup_and_allocate`]. Field values are what
/// firmware actually configured (may differ from what we asked for).
#[derive(Debug, Clone, Copy)]
pub struct FbAlloc {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub pixel_order: u32,
    /// VC-bus address of the framebuffer base. Strip the upper
    /// alias bits to get the ARM physical address.
    pub bus_addr: u32,
    /// Total allocation size in bytes.
    pub size: u32,
    /// Row stride in bytes.
    pub pitch: u32,
}

/// Configure framebuffer geometry and allocate in **one** mailbox
/// message.
///
/// This matters: the Pi firmware property mailbox treats each
/// request as an atomic transaction. State set in one
/// `fb_set_physical_size`/`fb_set_depth`/etc. call does *not*
/// persist into a subsequent `fb_allocate` call. Allocating after
/// separate-message setup leaves us with the firmware's defaults
/// (typically a 0-sized framebuffer at a nonsense pitch), which
/// makes the VC scan random DRAM as pixels. So everything goes in
/// one message.
///
/// Tag order matches Circle / Linux conventions:
/// SET_PHYSICAL → SET_VIRTUAL → SET_VIRTUAL_OFFSET → SET_DEPTH →
/// SET_PIXEL_ORDER → ALLOCATE_BUFFER → GET_PITCH.
pub fn fb_setup_and_allocate(
    width: u32,
    height: u32,
    depth: u32,
    pixel_order: u32,
    alignment: u32,
) -> Result<FbAlloc, MailboxError> {
    // Buffer layout, by word index — comments give the field name.
    // 0:  total_bytes (filled at the end)
    // 1:  request code (= 0)
    // 2:  SET_PHYSICAL_W_H tag
    // 3:  value-size (8 bytes)
    // 4:  req/resp code
    // 5:  width
    // 6:  height
    // 7:  SET_VIRTUAL_W_H tag
    // 8:  value-size (8)
    // 9:  req/resp code
    // 10: width
    // 11: height
    // 12: SET_VIRTUAL_OFFSET tag
    // 13: value-size (8)
    // 14: req/resp code
    // 15: x = 0
    // 16: y = 0
    // 17: SET_DEPTH tag
    // 18: value-size (4)
    // 19: req/resp code
    // 20: depth bits
    // 21: SET_PIXEL_ORDER tag
    // 22: value-size (4)
    // 23: req/resp code
    // 24: pixel_order
    // 25: ALLOCATE_BUFFER tag
    // 26: value-size (8)
    // 27: req/resp code
    // 28: alignment (→ bus_addr on response)
    // 29: 0 (→ size on response)
    // 30: GET_PITCH tag
    // 31: value-size (4)
    // 32: req/resp code
    // 33: 0 (→ pitch on response)
    // 34: end tag = 0
    const N_WORDS: usize = 35;
    let total_bytes = (N_WORDS as u32) * 4;
    let total_bytes_padded = (total_bytes + 15) & !15;

    let mut buf = Buffer::new();
    let w = &mut buf.words;
    w[0] = total_bytes_padded;
    w[1] = REQUEST_CODE;

    // Tag 1: SET_PHYSICAL_W_H
    w[2] = TAG_FB_SET_PHYSICAL_W_H;
    w[3] = 8;
    w[4] = REQUEST_CODE;
    w[5] = width;
    w[6] = height;

    // Tag 2: SET_VIRTUAL_W_H
    w[7] = TAG_FB_SET_VIRTUAL_W_H;
    w[8] = 8;
    w[9] = REQUEST_CODE;
    w[10] = width;
    w[11] = height;

    // Tag 3: SET_VIRTUAL_OFFSET
    w[12] = TAG_FB_SET_VIRTUAL_OFFSET;
    w[13] = 8;
    w[14] = REQUEST_CODE;
    w[15] = 0;
    w[16] = 0;

    // Tag 4: SET_DEPTH
    w[17] = TAG_FB_SET_DEPTH;
    w[18] = 4;
    w[19] = REQUEST_CODE;
    w[20] = depth;

    // Tag 5: SET_PIXEL_ORDER
    w[21] = TAG_FB_SET_PIXEL_ORDER;
    w[22] = 4;
    w[23] = REQUEST_CODE;
    w[24] = pixel_order;

    // Tag 6: ALLOCATE_BUFFER. value-size is max(req=4, resp=8) = 8.
    w[25] = TAG_FB_ALLOCATE;
    w[26] = 8;
    w[27] = REQUEST_CODE;
    w[28] = alignment;
    w[29] = 0;

    // Tag 7: GET_PITCH
    w[30] = TAG_FB_GET_PITCH;
    w[31] = 4;
    w[32] = REQUEST_CODE;
    w[33] = 0;

    // End
    w[34] = 0;

    mailbox_call(&mut buf, total_bytes_padded)?;

    // Per-tag response check: bit 31 of the request/response code
    // (third word of each tag header) is set when firmware processed
    // it. Index = tag_start + 2 (the request/response code slot).
    let tag_resp_indices = [4, 9, 14, 19, 23, 27, 32];
    for &i in &tag_resp_indices {
        if buf.words[i] & 0x8000_0000 == 0 {
            return Err(MailboxError::TagNotHandled);
        }
    }

    Ok(FbAlloc {
        width: buf.words[5],
        height: buf.words[6],
        depth: buf.words[20],
        pixel_order: buf.words[24],
        bus_addr: buf.words[28],
        size: buf.words[29],
        pitch: buf.words[33],
    })
}
