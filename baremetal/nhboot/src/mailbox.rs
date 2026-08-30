//! VideoCore mailbox property-channel client — the one query the
//! bootloader needs: the SoC core clock rate, which is the SDHOST's
//! input clock (`sd::sdhost::clock_setup`). Register map, message
//! packing and the tag protocol are those of the hypervisor's
//! `src/host/mailbox.rs`, trimmed to `get_clock_rate`.
//!
//! BCM2710 mailbox 0/1 at `0x3F00_B880`: mailbox 0 is VC → ARM
//! (READ at +0x00), mailbox 1 is ARM → VC (WRITE at +0x20), STATUS at
//! +0x18 (bit 31 = mailbox 1 full, bit 30 = mailbox 0 empty). A
//! message packs `(buffer_address << 0) | channel` in the low 4 bits,
//! so the buffer must be 16-byte aligned. The VC reads the buffer
//! through its own bus alias: `pa | 0xC000_0000` is the L2-uncached
//! view.
//!
//! The hypervisor cleans and invalidates the buffer around the call
//! (`dc civac`) because its RAM is cacheable. Here the MMU is off, so
//! every access is Non-cacheable and there is nothing to maintain.

use core::ptr::{read_volatile, write_volatile};

const MAILBOX_BASE: usize = 0x3F00_B880;
const MBOX_READ: *mut u32 = MAILBOX_BASE as *mut u32;
const MBOX_STATUS: *mut u32 = (MAILBOX_BASE + 0x18) as *mut u32;
const MBOX_WRITE: *mut u32 = (MAILBOX_BASE + 0x20) as *mut u32;

const STATUS_FULL: u32 = 1 << 31; // mailbox 1 full — cannot write.
const STATUS_EMPTY: u32 = 1 << 30; // mailbox 0 empty — cannot read.

const CHANNEL_PROPERTY: u32 = 8;

const REQUEST_CODE: u32 = 0;
const RESPONSE_SUCCESS: u32 = 0x8000_0000;

/// Bus-address tag bit: the VC-bus uncached alias of a u32 PA.
const BUS_UNCACHED: u32 = 0xC000_0000;

/// Property tag: get clock rate (`[clock_id, 0]` → `[clock_id, hz]`).
const TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;

/// Clock ID of the SoC core clock, which feeds SDHOST.
pub const CLOCK_ID_CORE: u32 = 4;

#[derive(Debug, Clone, Copy)]
pub enum MailboxError {
    /// Firmware did not respond within our polling window.
    Timeout,
    /// Firmware acked the message but the response code wasn't
    /// `0x8000_0000`.
    FirmwareError,
    /// A tag came back with the high bit of its request code unset —
    /// i.e. firmware didn't recognise / didn't fill it.
    TagNotHandled,
}

/// One property request. 16 words is plenty for a single clock tag
/// (2 header + 3 tag header + 2 payload + 1 end tag). Lives on the
/// stack; the protocol's only alignment demand is 16 bytes.
#[repr(C, align(16))]
struct Buffer {
    words: [u32; 16],
}

/// Post `buf` on the property channel and wait for the reply. The
/// caller has filled the buffer and set `words[0]` to the padded byte
/// length.
fn mailbox_call(buf: &mut Buffer) -> Result<(), MailboxError> {
    let pa = buf.words.as_ptr() as usize as u32;
    debug_assert!(pa & 0xF == 0, "mailbox buffer must be 16-byte aligned");
    let bus_addr = pa | BUS_UNCACHED;

    // SAFETY: MMIO at the documented mailbox addresses; single core,
    // no concurrent users.
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

        let mut got_reply = false;
        for _ in 0..10_000_000 {
            if read_volatile(MBOX_STATUS) & STATUS_EMPTY != 0 {
                continue;
            }
            let m = read_volatile(MBOX_READ);
            if m & 0xF == CHANNEL_PROPERTY {
                got_reply = true;
                break;
            }
        }
        if !got_reply {
            return Err(MailboxError::Timeout);
        }
    }

    // The VC wrote its reply into our (uncached) buffer; re-read it
    // volatile so the compiler doesn't reuse the request words.
    // SAFETY: the buffer is live for the whole call.
    let code = unsafe { read_volatile(core::ptr::addr_of!(buf.words[1])) };
    if code != RESPONSE_SUCCESS {
        return Err(MailboxError::FirmwareError);
    }
    Ok(())
}

/// Send a one-tag request; the response overwrites `arg_words`.
fn send_one_tag(tag_id: u32, arg_words: &mut [u32]) -> Result<(), MailboxError> {
    // Layout: hdr[0..2] + tag_hdr[0..3] + payload[..] + end_tag.
    let payload_bytes: u32 = (arg_words.len() as u32) * 4;
    let total_words: usize = 2 + 3 + arg_words.len() + 1;
    assert!(total_words <= 16, "mailbox buffer too small for this tag");
    let total_bytes = (total_words as u32) * 4;
    let total_bytes_padded = (total_bytes + 15) & !15;

    let mut buf = Buffer { words: [0; 16] };
    buf.words[0] = total_bytes_padded;
    buf.words[1] = REQUEST_CODE;
    buf.words[2] = tag_id;
    buf.words[3] = payload_bytes;
    buf.words[4] = REQUEST_CODE;
    for (i, &w) in arg_words.iter().enumerate() {
        buf.words[5 + i] = w;
    }
    buf.words[5 + arg_words.len()] = 0; // end tag.

    mailbox_call(&mut buf)?;

    // The per-tag response indicator is bit 31 of the tag's request
    // code word (words[4]); words[3] is the value-buffer size.
    // SAFETY: as in `mailbox_call` — volatile re-reads of VC-written
    // words.
    unsafe {
        if read_volatile(core::ptr::addr_of!(buf.words[4])) & 0x8000_0000 == 0 {
            return Err(MailboxError::TagNotHandled);
        }
        for (i, slot) in arg_words.iter_mut().enumerate() {
            *slot = read_volatile(core::ptr::addr_of!(buf.words[5 + i]));
        }
    }
    Ok(())
}

/// Current rate of a clock ID, in Hz.
pub fn get_clock_rate(clock_id: u32) -> Result<u32, MailboxError> {
    let mut payload = [clock_id, 0];
    send_one_tag(TAG_GET_CLOCK_RATE, &mut payload)?;
    Ok(payload[1])
}
