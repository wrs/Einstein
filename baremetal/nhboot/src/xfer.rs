//! The serial upload protocol — nhboot's receiving side. The host
//! side is `scripts/pi-upload.py`; the two mirror each other's
//! constants and message layouts, so change both together.
//!
//! ```text
//!  handshake (text, 115200)
//!    host   →  \x01NHUP <baud>\n        repeated every 100 ms
//!    nhboot →  NHUP-OK <baud>\n         then both sides switch baud
//!
//!  framed messages (binary, little-endian, one tag byte first)
//!    nhboot →  T  u32 n, n×{u32 adler32, u32 crc32}, u32 crc32(entries)
//!                 (one entry per full 4 KiB block of the old payload)
//!    host   →  D  u32 offset, u32 len, u32 crc32, len bytes
//!    host   →  C  u32 new_offset, u32 old_offset, u32 len, u32 crc32(those 12 bytes)
//!    host   →  K  u32 payload_len, u32 payload_crc
//!    nhboot →  A  u32 echo                   (offset for D/C, len for K)
//!    nhboot →  N  u32 echo, u8 reason
//!  then, after the K's ACK: text lines (the SD write's progress),
//!  DONE\n, and the baud goes back to 115200.
//! ```
//!
//! The new container is assembled at [`image::NEW_BASE`] while the
//! firmware-loaded one at [`image::IMAGE_ADDR`] stays intact as the
//! source for `C` (COPY) messages. Only the header area of the new
//! container is zeroed up front: the host sends or copies every
//! payload byte, and the COMMIT CRC covers the whole payload, so
//! anything not written is caught rather than assumed.

use crate::crc::{adler32, crc32};
use crate::image::{self, HDR_SIZE, MAX_PAYLOAD, NEW_BASE};
use crate::time::{elapsed_us, now_us};
use crate::{persist, println, uart};

/// How long a valid image waits for a host before booting.
const HANDSHAKE_WINDOW_US: u64 = 1_000_000;
/// "Waiting" note interval when there is no bootable image.
const WAITING_NOTE_US: u64 = 5_000_000;
/// Silence inside a framed message that abandons it (NAK reason 3).
const MSG_BYTE_TIMEOUT_US: u64 = 2_000_000;
/// After an unknown tag, skip input until the line is this quiet.
const RESYNC_SILENCE_US: u64 = 100_000;
/// After NHUP-OK, wait for this much silence before sending TABLE: the
/// host repeats its hello every 100 ms, so one more line can already
/// be in flight (and arrives mangled once we have switched baud).
const HANDSHAKE_SETTLE_US: u64 = 150_000;
/// Baud range the handshake accepts. 3 M is clk/16 on the 48 MHz PL011
/// reference clock and the FTDI cable's ceiling.
const MIN_BAUD: u32 = 115_200;
const MAX_BAUD: u32 = 3_000_000;
/// Largest DATA message the host may send.
const MAX_DATA_LEN: u32 = 65_536;
/// TABLE block size: the old payload is fingerprinted in blocks of
/// this many bytes (the partial tail is not listed). The host mirrors
/// it (`TABLE_BLOCK` in pi-upload.py) — it sizes the windows it
/// slides over the new image, so the two must agree.
const TABLE_BLOCK: usize = 4096;

const TAG_TABLE: u8 = b'T';
const TAG_DATA: u8 = b'D';
const TAG_COPY: u8 = b'C';
const TAG_COMMIT: u8 = b'K';
const TAG_ACK: u8 = b'A';
const TAG_NAK: u8 = b'N';

const NAK_BAD_CRC: u8 = 1;
const NAK_BAD_RANGE: u8 = 2;
const NAK_RX_TIMEOUT: u8 = 3;
const NAK_NO_OLD_IMAGE: u8 = 4;
const NAK_UNKNOWN_TAG: u8 = 5;

/// Drain RX for the handshake. With a bootable image (`image_ok`)
/// this gives up after [`HANDSHAKE_WINDOW_US`] and returns `None`;
/// without one it waits indefinitely. Returns the baud a good `NHUP`
/// line asked for; the reply and the switch happen in [`receive`],
/// after the TABLE has been prepared, because the host expects the
/// `T` tag to be the first thing after the switch.
pub fn handshake_window(image_ok: bool) -> Option<u32> {
    let start = now_us();
    let mut last_note = start;
    let mut line = [0u8; 32];
    let mut n = 0usize;
    // The host starts spamming before we exist, so the first bytes
    // can be a partial line; \x01 is the resync point.
    let mut synced = false;
    loop {
        if let Some(b) = uart::getc_nonblock() {
            match b {
                0x01 => {
                    synced = true;
                    n = 0;
                }
                b'\n' | b'\r' if synced => {
                    synced = false;
                    match parse_handshake(&line[..n]) {
                        Some(baud) if (MIN_BAUD..=MAX_BAUD).contains(&baud) => {
                            return Some(baud);
                        }
                        Some(_) => println!("NHUP-ERR baud"),
                        None => {}
                    }
                }
                _ if synced => {
                    if n < line.len() {
                        line[n] = b;
                        n += 1;
                    } else {
                        synced = false; // overlong: not ours
                    }
                }
                _ => {}
            }
            continue;
        }
        if image_ok {
            if elapsed_us(start) >= HANDSHAKE_WINDOW_US {
                return None;
            }
        } else if elapsed_us(last_note) >= WAITING_NOTE_US {
            println!("nhboot: no bootable image; waiting for upload");
            last_note = now_us();
        }
    }
}

/// `NHUP <decimal baud>` → the baud.
fn parse_handshake(line: &[u8]) -> Option<u32> {
    let digits = line.strip_prefix(b"NHUP ")?;
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    let mut v: u32 = 0;
    for &d in digits {
        if !d.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (d - b'0') as u32;
    }
    Some(v)
}

/// Run the framed protocol until a COMMIT verifies. `old` is the
/// firmware-loaded container (base, payload length) if it validated,
/// the COPY source. Returns the committed payload length; the new
/// container at [`NEW_BASE`] carries a freshly written header, and the
/// console is back at 115200.
pub fn receive(old: Option<(usize, u32)>, baud: u32) -> u32 {
    // Clear the header so a stale container can't validate by
    // accident; the payload area is covered by the COMMIT CRC.
    // SAFETY: NEW_BASE..+HDR_SIZE is nhboot's own staging RAM.
    unsafe { core::slice::from_raw_parts_mut(NEW_BASE as *mut u8, HDR_SIZE).fill(0) };

    // Fingerprint the old image while the console is still a console
    // (the timing line is the last free-form text before the framed
    // stream), then answer the hello and switch.
    let n = build_table(old);
    println!("NHUP-OK {}", baud);
    uart::set_baud(baud);
    drain_until_quiet(HANDSHAKE_SETTLE_US);
    send_table(n);

    // Nothing is printed from here until the COMMIT is acknowledged:
    // the console *is* the protocol link, and the host parses every
    // byte after the TABLE as a reply. Progress is the host's job.
    loop {
        let tag = read_byte_blocking();
        match tag {
            TAG_DATA => {
                let mut hdr = [0u8; 12];
                if read_exact(&mut hdr).is_err() {
                    nak(0, NAK_RX_TIMEOUT);
                    continue;
                }
                let offset = u32_at(&hdr, 0);
                let len = u32_at(&hdr, 4);
                let crc = u32_at(&hdr, 8);
                if len == 0 || len > MAX_DATA_LEN || !in_payload(offset, len) {
                    nak(offset, NAK_BAD_RANGE);
                    resync();
                    continue;
                }
                let dst = new_payload_slice(offset, len);
                if read_exact(dst).is_err() {
                    nak(offset, NAK_RX_TIMEOUT);
                    continue;
                }
                if crc32(dst) != crc {
                    nak(offset, NAK_BAD_CRC);
                    continue;
                }
                ack(offset);
            }
            TAG_COPY => {
                let mut hdr = [0u8; 16];
                if read_exact(&mut hdr).is_err() {
                    nak(0, NAK_RX_TIMEOUT);
                    continue;
                }
                let new_off = u32_at(&hdr, 0);
                let old_off = u32_at(&hdr, 4);
                let len = u32_at(&hdr, 8);
                // A COPY carries no payload for the COMMIT CRC to
                // vouch for until the very end, so its header gets
                // its own CRC: a line error here is retried now, not
                // discovered as an unexplained COMMIT failure.
                if crc32(&hdr[..12]) != u32_at(&hdr, 12) {
                    nak(new_off, NAK_BAD_CRC);
                    continue;
                }
                let Some((old_base, old_len)) = old else {
                    nak(new_off, NAK_NO_OLD_IMAGE);
                    continue;
                };
                if len == 0
                    || !in_payload(new_off, len)
                    || old_off.checked_add(len).is_none_or(|end| end > old_len)
                {
                    nak(new_off, NAK_BAD_RANGE);
                    continue;
                }
                // SAFETY: bounds checked against the old payload; the
                // two containers are disjoint (image.rs asserts).
                let src: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        (old_base + HDR_SIZE + old_off as usize) as *const u8,
                        len as usize,
                    )
                };
                new_payload_slice(new_off, len).copy_from_slice(src);
                ack(new_off);
            }
            TAG_COMMIT => {
                let mut hdr = [0u8; 8];
                if read_exact(&mut hdr).is_err() {
                    nak(0, NAK_RX_TIMEOUT);
                    continue;
                }
                let len = u32_at(&hdr, 0);
                let crc = u32_at(&hdr, 4);
                if len == 0 || len as usize > MAX_PAYLOAD {
                    nak(len, NAK_BAD_RANGE);
                    continue;
                }
                let actual = crc32(new_payload_slice(0, len));
                if actual != crc {
                    nak(len, NAK_BAD_CRC);
                    continue;
                }
                // Zero the pad up to the next 4 KiB boundary: the
                // staging RAM is whatever was there before (stale
                // across a power cycle), and persist.rs compares whole
                // sectors, so an unzeroed pad would be written to the
                // card as a spurious differing sector.
                let padded = (len as usize).next_multiple_of(4096).min(MAX_PAYLOAD) as u32;
                if padded > len {
                    new_payload_slice(len, padded - len).fill(0);
                }
                image::write_header(NEW_BASE, len, crc);
                ack(len);
                // From here on the console is plain text again; the
                // host echoes it while it waits for DONE.
                println!("xfer: commit ok, {} bytes, crc32 {:08x}", len, crc);
                match persist::persist(NEW_BASE, len, old.map(|(b, _)| b)) {
                    Ok(st) => println!(
                        "persist: wrote {}/{} sectors{} in {} ms",
                        st.sectors_written,
                        st.sectors_total,
                        if st.created { " (file created)" } else { "" },
                        st.ms
                    ),
                    Err(e) => println!(
                        "persist: FAILED ({}) — image boots from RAM only this time",
                        e
                    ),
                }
                println!("DONE");
                uart::set_baud(uart::CONSOLE_BAUD);
                return len;
            }
            _ => {
                nak(tag as u32, NAK_UNKNOWN_TAG);
                resync();
            }
        }
    }
}

/// TABLE entries live in .bss: `{adler32, crc32}` per full
/// [`TABLE_BLOCK`] of the old payload, at most `MAX_PAYLOAD / TABLE_BLOCK`
/// of them (32 KiB — too much for the 16 KiB stack).
struct TableCell(core::cell::UnsafeCell<[[u32; 2]; MAX_PAYLOAD / TABLE_BLOCK]>);
// SAFETY: single core, no interrupts; only `build_table`/`send_table` touch it.
unsafe impl Sync for TableCell {}
static TABLE: TableCell = TableCell(core::cell::UnsafeCell::new([[0; 2]; MAX_PAYLOAD / TABLE_BLOCK]));

/// Fingerprint the old payload into [`TABLE`]; returns the entry
/// count (0 without a valid old image). Each entry is the
/// `{adler32, crc32}` of one full block, so the host can find where
/// the new image repeats it at *any* byte offset (the 8 MiB ROM blob
/// shifts whenever the code before it grows) and replace those
/// stretches with COPY messages. Uncached reads make the pass a few
/// hundred ms on the A53; the timing is printed for the record.
fn build_table(old: Option<(usize, u32)>) -> usize {
    let Some((base, len)) = old else { return 0 };
    let n = len as usize / TABLE_BLOCK;
    let t0 = now_us();
    // SAFETY: see `TableCell`.
    let table = unsafe { &mut *TABLE.0.get() };
    for (j, entry) in table.iter_mut().enumerate().take(n) {
        // SAFETY: `j < len / TABLE_BLOCK`, inside the validated old payload.
        let block: &[u8] = unsafe {
            core::slice::from_raw_parts(
                (base + HDR_SIZE + j * TABLE_BLOCK) as *const u8,
                TABLE_BLOCK,
            )
        };
        *entry = [adler32(block), crc32(block)];
    }
    println!("xfer: table n={} in {} ms", n, elapsed_us(t0) / 1000);
    n
}

/// Send the `T` message: `u32 n`, the `n` entries, then the CRC-32 of
/// the `8·n` entry bytes (the `n` field excluded; with no old image
/// that is the CRC of nothing).
fn send_table(n: usize) {
    // SAFETY: see `TableCell`; `build_table` ran first.
    let table = unsafe { &*TABLE.0.get() };
    uart::putc(TAG_TABLE);
    put_u32(n as u32);
    let mut table_crc = 0xFFFF_FFFFu32;
    for entry in table.iter().take(n) {
        for v in entry {
            let bytes = v.to_le_bytes();
            table_crc = crate::crc::crc32_update(table_crc, &bytes);
            for b in bytes {
                uart::putc(b);
            }
        }
    }
    put_u32(table_crc ^ 0xFFFF_FFFF);
}

fn ack(echo: u32) {
    uart::putc(TAG_ACK);
    put_u32(echo);
}

fn nak(echo: u32, reason: u8) {
    uart::putc(TAG_NAK);
    put_u32(echo);
    uart::putc(reason);
}

fn put_u32(v: u32) {
    for b in v.to_le_bytes() {
        uart::putc(b);
    }
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn in_payload(offset: u32, len: u32) -> bool {
    offset
        .checked_add(len)
        .is_some_and(|end| end as usize <= MAX_PAYLOAD)
}

/// `len` bytes of the new payload starting at `offset` (caller has
/// bounds-checked with `in_payload`).
fn new_payload_slice(offset: u32, len: u32) -> &'static mut [u8] {
    // SAFETY: inside the NEW container's payload area, which only this
    // module touches; the returned slices never overlap a live one.
    unsafe {
        core::slice::from_raw_parts_mut(
            (NEW_BASE + HDR_SIZE + offset as usize) as *mut u8,
            len as usize,
        )
    }
}

/// Wait for a tag byte with no timeout — between messages the host
/// may pause for as long as it likes (it is computing the delta, or
/// the user is reading a log).
fn read_byte_blocking() -> u8 {
    loop {
        if let Some(b) = uart::getc_nonblock() {
            return b;
        }
    }
}

/// Fill `dst` from the link. Inside a message the line must not go
/// quiet for [`MSG_BYTE_TIMEOUT_US`]. The receive loop is kept tight:
/// the timer is read only when the FIFO is found empty, once per idle
/// gap, so at 3 Mbaud (a byte every 3.3 µs into a 16-deep FIFO) the
/// poll keeps up.
fn read_exact(dst: &mut [u8]) -> Result<(), ()> {
    let mut i = 0;
    let mut gap_start: Option<u64> = None;
    while i < dst.len() {
        if let Some(b) = uart::getc_nonblock() {
            dst[i] = b;
            i += 1;
            gap_start = None;
        } else {
            match gap_start {
                None => gap_start = Some(now_us()),
                Some(t) if elapsed_us(t) > MSG_BYTE_TIMEOUT_US => return Err(()),
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// After garbage: discard input until the line has been silent for
/// [`RESYNC_SILENCE_US`], so a stream of unknown bytes produces one
/// NAK rather than one per byte.
fn resync() {
    drain_until_quiet(RESYNC_SILENCE_US);
}

/// Discard input until nothing has arrived for `silence_us`.
fn drain_until_quiet(silence_us: u64) {
    let mut quiet_since = now_us();
    loop {
        if uart::getc_nonblock().is_some() {
            quiet_since = now_us();
        } else if elapsed_us(quiet_since) >= silence_us {
            return;
        }
    }
}
