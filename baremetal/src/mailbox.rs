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

/// 16-byte-aligned buffer wrapping a single property request.
///
/// We use a fixed 32-word buffer; the largest message we currently
/// build (get-then-set clock with full headers + pad) fits in 8
/// words. The buffer is on the stack so each call gets a fresh,
/// uncontended copy — there's no global state to lock and no
/// re-entrancy hazard.
#[repr(C, align(16))]
struct Buffer {
    words: [u32; 32],
}

impl Buffer {
    const fn new() -> Self {
        Self { words: [0; 32] }
    }
}

/// Send a one-tag property request and return the first response
/// word. `arg_words` carries the request payload; the response
/// overwrites it in place. The caller passes the *number* of u32s in
/// the value buffer (not bytes) for both lanes.
fn send_one_tag(tag_id: u32, arg_words: &mut [u32]) -> Result<(), MailboxError> {
    // Layout: hdr[0..2] + tag_hdr[0..3] + payload[..] + end_tag.
    let payload_bytes: u32 = (arg_words.len() as u32) * 4;
    let total_words: usize = 2 + 3 + arg_words.len() + 1;
    assert!(total_words <= 32, "mailbox buffer too small for this tag");
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
        // Wait for room in mailbox 1.
        for _ in 0..10_000_000 {
            if read_volatile(MBOX_STATUS) & STATUS_FULL == 0 {
                break;
            }
        }
        if read_volatile(MBOX_STATUS) & STATUS_FULL != 0 {
            return Err(MailboxError::Timeout);
        }

        write_volatile(MBOX_WRITE, bus_addr | CHANNEL_PROPERTY);

        // Wait for response on mailbox 0, on the right channel.
        for _ in 0..10_000_000 {
            if read_volatile(MBOX_STATUS) & STATUS_EMPTY != 0 {
                continue;
            }
            let m = read_volatile(MBOX_READ);
            if m & 0xF == CHANNEL_PROPERTY {
                break;
            }
            // Wrong channel — keep draining.
        }
    }

    // Invalidate so we re-read what the VC wrote.
    dc_civac_range(buf_ptr, (total_bytes_padded as usize).max(16));

    if buf.words[1] != RESPONSE_SUCCESS {
        return Err(MailboxError::FirmwareError);
    }
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
