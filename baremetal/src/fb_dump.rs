//! PNG dumper — host-visible screenshots of GUEST_FB.
//!
//! `screen::blit` calls [`mark_dirty`] each time it lands pixels; the
//! timer-IRQ path calls [`maybe_dump`] each tick. Once `DUMP_DELAY_MS`
//! of wall-clock time has elapsed since the most recent `mark_dirty`,
//! we encode the visible 320×480 1-bpp framebuffer as a PNG and ship
//! it to the host via the same Arm-semihosting primitives `snapshot`
//! uses, then clear the dirty flag. Multiple blits within one second
//! collapse into a single screenshot taken once the activity quiets.
//!
//! The encoder uses **stored** deflate blocks (BTYPE=00) so we don't
//! pull in a compressor. Output for a 320×480 1-bpp panel is fixed at
//! 19 748 bytes — small enough that we serialise into a 24 KiB static
//! scratch buffer in one pass and ship via a single SYS_WRITE.
//!
//! Newton 1-bpp uses 1 = black; PNG 1-bpp grayscale uses 0 = black, so
//! every framebuffer byte is bitwise-inverted on the way into IDAT.

use core::arch::asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::{guest_mem, kprintln};

const SCREEN_WIDTH: u32 = 320;
const SCREEN_HEIGHT: u32 = 480;
const ROW_BYTES: usize = (SCREEN_WIDTH as usize) / 8;
const FB_BYTES: usize = ROW_BYTES * SCREEN_HEIGHT as usize;

/// Wall-clock delay between the last blit and the screenshot fire.
/// Each `mark_dirty` re-arms the deadline, so a flurry of blits
/// produces one screenshot ~1 s after the burst settles.
const DUMP_DELAY_MS: u64 = 1_000;

static DIRTY: AtomicBool = AtomicBool::new(false);
static DEADLINE_TICKS: AtomicU64 = AtomicU64::new(0);
static DUMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// Arm the dump trigger. Called by `screen::blit` after pixels land.
pub fn mark_dirty() {
    #[cfg(nh_guest_test)]
    {
        return;
    }
    #[cfg(not(nh_guest_test))]
    {
        let now = cntpct();
        let interval = (DUMP_DELAY_MS * cntfrq()) / 1_000;
        DEADLINE_TICKS.store(now.wrapping_add(interval), Ordering::Relaxed);
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Test helper — encode the current FB and write it to slot
/// `/tmp/newton-fb-99999.png` immediately, bypassing the dirty / timer
/// gate. Used during bring-up to verify the encoder and semihost path
/// without needing a real blit. Remove the call site once a real blit
/// has been observed producing a screenshot.
#[allow(dead_code)]
pub fn force_dump_now() {
    #[cfg(not(nh_guest_test))]
    {
        match write_png(99_999) {
            Ok(n) => kprintln!("fb_dump: force_dump_now wrote {} bytes", n),
            Err(e) => kprintln!("fb_dump: force_dump_now failed: {}", e),
        }
    }
}

/// Poll hook for the timer-IRQ path. Fires at most once per blit-burst.
pub fn maybe_dump() {
    #[cfg(nh_guest_test)]
    {
        return;
    }
    #[cfg(not(nh_guest_test))]
    {
        if !DIRTY.load(Ordering::Relaxed) {
            return;
        }
        if cntpct() < DEADLINE_TICKS.load(Ordering::Relaxed) {
            return;
        }
        DIRTY.store(false, Ordering::Relaxed);
        let seq = DUMP_SEQ.fetch_add(1, Ordering::Relaxed);
        match write_png(seq) {
            Ok(n) => kprintln!("fb_dump: seq={} wrote {} bytes", seq, n),
            Err(e) => kprintln!("fb_dump: seq={} failed: {}", seq, e),
        }
    }
}

#[cfg(not(nh_guest_test))]
fn write_png(seq: u32) -> Result<usize, &'static str> {
    ensure_output_dir();

    // 24 KiB scratch — fixed-output PNG is 19 748 bytes for our panel.
    static mut PNG_BUF: [u8; 24 * 1024] = [0; 24 * 1024];
    // SAFETY: single-threaded EL2; only this function touches PNG_BUF
    // and `maybe_dump` is the sole caller (gated by an atomic flag).
    let buf = unsafe { &mut *addr_of_mut!(PNG_BUF) };
    // SAFETY: `fb_host_pa` returns the base of the static GUEST_FB
    // backing store; we read the visible region only.
    let fb = unsafe {
        core::slice::from_raw_parts(guest_mem::fb_host_pa() as *const u8, FB_BYTES)
    };
    let n = encode_png(buf, fb);

    let mut path_buf = [0u8; 48];
    let path_len = format_path(&mut path_buf, seq);
    let path = &path_buf[..path_len];

    let fh = sh_open(path).ok_or("SYS_OPEN failed")?;
    let res = sh_write(&fh, &buf[..n]);
    sh_close(fh);
    res?;
    Ok(n)
}

/// Make sure `/tmp/newton-fb` exists. Called once before the first
/// PNG write — semihosting `SYS_OPEN` won't create the parent dir, so
/// we shell out to the host via `SYS_SYSTEM`. Subsequent calls are
/// no-ops.
#[cfg(not(nh_guest_test))]
fn ensure_output_dir() {
    use core::sync::atomic::AtomicBool;
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let cmd = b"mkdir -p /tmp/newton-fb\0";
    let args: [u64; 2] = [cmd.as_ptr() as u64, (cmd.len() - 1) as u64];
    // SYS_SYSTEM (op 0x12) ignores the return value's exact shape; we
    // accept any outcome and let the subsequent SYS_OPEN report the
    // real error if the dir still isn't there.
    let _ = unsafe { semihost(SYS_SYSTEM, args.as_ptr()) };
}

#[cfg(not(nh_guest_test))]
fn format_path(buf: &mut [u8; 48], seq: u32) -> usize {
    // "/tmp/newton-fb/NNNNN.png\0" — NUL-terminated for SYS_OPEN; the
    // returned length includes the NUL (semihost open passes len-1).
    let prefix = b"/tmp/newton-fb/";
    let suffix = b".png\0";
    let mut i = 0;
    for &b in prefix {
        buf[i] = b;
        i += 1;
    }
    let mut tmp = seq;
    let mut digits = [0u8; 5];
    for j in (0..5).rev() {
        digits[j] = b'0' + (tmp % 10) as u8;
        tmp /= 10;
    }
    for &d in &digits {
        buf[i] = d;
        i += 1;
    }
    for &b in suffix {
        buf[i] = b;
        i += 1;
    }
    i
}

// ---- PNG encoding ------------------------------------------------

/// Total bytes of filtered scanline payload that goes into the deflate
/// stored block — one filter byte plus `ROW_BYTES` per row, for
/// `SCREEN_HEIGHT` rows.
const PAYLOAD_LEN: usize = (1 + ROW_BYTES) * SCREEN_HEIGHT as usize;

/// Encode `fb` (must be exactly `FB_BYTES` long, row-major 1-bpp packed
/// in MSB-first order, Newton convention 1=black) as a 320×480 1-bpp
/// grayscale PNG into `out`. Returns the number of bytes written.
fn encode_png(out: &mut [u8], fb: &[u8]) -> usize {
    debug_assert_eq!(fb.len(), FB_BYTES);
    let mut pos = 0;

    // PNG signature.
    let sig = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    out[pos..pos + 8].copy_from_slice(&sig);
    pos += 8;

    // IHDR.
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&SCREEN_WIDTH.to_be_bytes());
    ihdr[4..8].copy_from_slice(&SCREEN_HEIGHT.to_be_bytes());
    ihdr[8] = 1; // bit depth
    ihdr[9] = 0; // color type: grayscale
    ihdr[10] = 0; // compression: deflate
    ihdr[11] = 0; // filter method: 0
    ihdr[12] = 0; // interlace: none
    pos += write_chunk_at(out, pos, *b"IHDR", &ihdr);

    // IDAT — built in place because the payload is large.
    pos += write_idat_at(out, pos, fb);

    // IEND.
    pos += write_chunk_at(out, pos, *b"IEND", &[]);

    pos
}

fn write_chunk_at(out: &mut [u8], at: usize, ty: [u8; 4], data: &[u8]) -> usize {
    let mut p = at;
    out[p..p + 4].copy_from_slice(&(data.len() as u32).to_be_bytes());
    p += 4;
    let type_start = p;
    out[p..p + 4].copy_from_slice(&ty);
    p += 4;
    out[p..p + data.len()].copy_from_slice(data);
    p += data.len();
    let crc = crc32(&out[type_start..p]);
    out[p..p + 4].copy_from_slice(&crc.to_be_bytes());
    p += 4;
    p - at
}

fn write_idat_at(out: &mut [u8], at: usize, fb: &[u8]) -> usize {
    // IDAT data layout:
    //   2 bytes  zlib header (0x78 0x01 — deflate, 32K window, no preset dict)
    //   1 byte   stored deflate block header (BFINAL=1, BTYPE=00)
    //   2 bytes  LEN  (little-endian, length of stored data)
    //   2 bytes  NLEN (one's complement of LEN)
    //   PAYLOAD  raw filtered scanlines
    //   4 bytes  Adler32 of the uncompressed payload (big-endian)
    const IDAT_DATA_LEN: usize = 2 + 1 + 4 + PAYLOAD_LEN + 4;

    let mut p = at;
    out[p..p + 4].copy_from_slice(&(IDAT_DATA_LEN as u32).to_be_bytes());
    p += 4;
    let type_start = p;
    out[p..p + 4].copy_from_slice(b"IDAT");
    p += 4;

    // zlib header.
    out[p] = 0x78;
    out[p + 1] = 0x01;
    p += 2;

    // Stored deflate block header + LEN/NLEN. The payload fits in one
    // 16-bit LEN since 19 680 < 65 535.
    out[p] = 0x01;
    p += 1;
    let len_le = (PAYLOAD_LEN as u16).to_le_bytes();
    let nlen_le = (!(PAYLOAD_LEN as u16)).to_le_bytes();
    out[p..p + 2].copy_from_slice(&len_le);
    p += 2;
    out[p..p + 2].copy_from_slice(&nlen_le);
    p += 2;

    // Filtered scanlines.
    let mut adler = Adler32::new();
    for row in 0..SCREEN_HEIGHT as usize {
        out[p] = 0; // filter type 0 (None)
        adler.update(&[0]);
        let src = &fb[row * ROW_BYTES..(row + 1) * ROW_BYTES];
        for j in 0..ROW_BYTES {
            // Newton 1-bpp: 1 = black, PNG 1-bpp grayscale: 0 = black.
            // Invert so PNG viewers reproduce the panel image.
            out[p + 1 + j] = !src[j];
        }
        adler.update(&out[p + 1..p + 1 + ROW_BYTES]);
        p += 1 + ROW_BYTES;
    }

    // Adler32 (BE).
    let a = adler.finish();
    out[p..p + 4].copy_from_slice(&a.to_be_bytes());
    p += 4;

    // CRC32 over (type + data).
    let crc = crc32(&out[type_start..p]);
    out[p..p + 4].copy_from_slice(&crc.to_be_bytes());
    p += 4;

    p - at
}

// ---- checksum primitives -----------------------------------------

struct Adler32 {
    a: u32,
    b: u32,
}

impl Adler32 {
    fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    fn update(&mut self, data: &[u8]) {
        // Per-byte modulo: simple, and our total input is ~20 KiB
        // running once a second at most.
        for &x in data {
            self.a = (self.a + x as u32) % 65521;
            self.b = (self.b + self.a) % 65521;
        }
    }

    fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

const CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

fn crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ---- semihosting primitives --------------------------------------

#[cfg(not(nh_guest_test))]
const SYS_OPEN: u64 = 0x01;
#[cfg(not(nh_guest_test))]
const SYS_CLOSE: u64 = 0x02;
#[cfg(not(nh_guest_test))]
const SYS_WRITE: u64 = 0x05;
#[cfg(not(nh_guest_test))]
const SYS_SYSTEM: u64 = 0x12;
#[cfg(not(nh_guest_test))]
const MODE_WRITE_BINARY: u64 = 0x05;

#[cfg(not(nh_guest_test))]
struct Handle(u64);

#[cfg(not(nh_guest_test))]
unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
    let result: u64;
    // SAFETY: HLT #0xF000 is the AArch64 semihosting trap; QEMU's
    // semihosting handler intercepts it and returns to EL2 without
    // disturbing register state beyond x0.
    unsafe {
        asm!(
            "hlt #0xF000",
            inout("x0") op => result,
            in("x1") arg as u64,
            options(nostack, preserves_flags),
        );
    }
    result as i64
}

#[cfg(not(nh_guest_test))]
fn sh_open(path: &[u8]) -> Option<Handle> {
    // path is NUL-terminated; SYS_OPEN takes the string length without
    // the NUL, matching the convention used in `snapshot::open`.
    let args: [u64; 3] = [
        path.as_ptr() as u64,
        MODE_WRITE_BINARY,
        (path.len() - 1) as u64,
    ];
    let h = unsafe { semihost(SYS_OPEN, args.as_ptr()) };
    if h < 0 {
        None
    } else {
        Some(Handle(h as u64))
    }
}

#[cfg(not(nh_guest_test))]
fn sh_write(h: &Handle, data: &[u8]) -> Result<(), &'static str> {
    let args: [u64; 3] = [h.0, data.as_ptr() as u64, data.len() as u64];
    let unwritten = unsafe { semihost(SYS_WRITE, args.as_ptr()) };
    if unwritten == 0 {
        Ok(())
    } else {
        Err("SYS_WRITE short write")
    }
}

#[cfg(not(nh_guest_test))]
fn sh_close(h: Handle) {
    let args: [u64; 1] = [h.0];
    let _ = unsafe { semihost(SYS_CLOSE, args.as_ptr()) };
}

// ---- generic timer reads -----------------------------------------

#[cfg(not(nh_guest_test))]
fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: MRS of a read-only sysreg has no side effects.
    unsafe {
        asm!("mrs {}, cntpct_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(not(nh_guest_test))]
fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: as above.
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}
