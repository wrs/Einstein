//! iter-79: minimal printf interpreter for the kernel's REP /
//! Print probes.
//!
//! The kernel funnels all debug-print output through
//! `Print__14POutTranslatorFPCce` (ROM 0x389eb8) — 150+ call
//! sites including `REPprintf`, `REPStackTrace`,
//! `REPExceptionNotify`, the `printf` jump-table entry, and ad-
//! hoc kernel diags. On a stock boot the installed translator is
//! Null (no debug link), so the output is invisible; the iter-79
//! probe captures the format string + args BEFORE the vtable
//! dispatch and renders to the kernel UART.
//!
//! Scope is intentionally narrow: enough to render the format
//! strings the kernel actually emits in 717006. Supports
//! `%[+-#0 ]?[0-9]*[lL]?[diuxXcsp%]`. Anything weird passes
//! through verbatim. Width is honored (zero- or space-pad);
//! precision is ignored for simplicity.
//!
//! Format-string + C-string reads come from byteswapped guest
//! ROM/RAM: the hypervisor's `load_rom` reverses bytes within
//! each 4-byte word so u32 reads are LE-numerically-correct, but
//! byte-level reads then see characters reversed inside each
//! word. The helper here reads u32s and converts back via
//! `to_be_bytes` so consumers see the original on-disk byte
//! sequence.

use crate::guest_mem;

/// Cap on rendered output bytes. Kernel diag lines comfortably
/// fit inside a few hundred bytes; a hard cap prevents a
/// runaway %s reading 4 MiB of garbage from blowing the UART.
const RENDER_CAP: usize = 512;

/// Cap on bytes copied for a single `%s` argument.
const STRING_CAP: usize = 256;

/// Source of variadic args. Matches the ARM EABI convention used
/// by `Print(POutTranslator*, fmt, ...)`: r2/r3 are the first
/// two args, then the stack (at the source-mode SP at probe
/// entry). Each `next()` advances by 4 bytes.
pub struct VaArgs {
    r2: u32,
    r3: u32,
    sp: u32,
    stack_off: u32,
    consumed: u32,
}

impl VaArgs {
    pub fn new(r2: u32, r3: u32, sp: u32) -> Self {
        Self { r2, r3, sp, stack_off: 0, consumed: 0 }
    }

    pub fn next(&mut self) -> u32 {
        let v = match self.consumed {
            0 => self.r2,
            1 => self.r3,
            _ => guest_mem::read_word_va(self.sp.wrapping_add(self.stack_off))
                .or_else(|| guest_mem::read_word_pa(self.sp.wrapping_add(self.stack_off)))
                .map(|w| {
                    self.stack_off = self.stack_off.wrapping_add(4);
                    w
                })
                .unwrap_or(0xDEADBEEF),
        };
        self.consumed = self.consumed.saturating_add(1);
        v
    }
}

/// Render `fmt` (an address in guest memory) with `args`,
/// printing the result through `kprintln!`. The `prefix` is
/// emitted before the formatted line so the operator can tell
/// which probe produced it.
pub fn render_and_log(prefix: &str, fmt_ptr: u32, mut args: VaArgs) {
    let mut buf = [0u8; RENDER_CAP];
    let mut written = 0usize;

    let mut fmt_cursor = fmt_ptr;
    let mut byte_idx_within_word: u8 = (fmt_ptr & 0x3) as u8;
    let mut word_buf: Option<[u8; 4]> = None;
    let mut word_addr = fmt_ptr & !0x3;

    'outer: loop {
        // Read the next byte of the format string.
        let b = match read_string_byte(&mut word_buf, &mut word_addr, &mut byte_idx_within_word, &mut fmt_cursor) {
            Some(b) => b,
            None => break,
        };
        if b == 0 {
            break;
        }
        if b != b'%' {
            push_byte(&mut buf, &mut written, b);
            if written == RENDER_CAP {
                break;
            }
            continue;
        }

        // Got a '%'. Parse flags / width / length / specifier.
        let mut flags = SpecFlags::default();
        let mut width: u32 = 0;
        let mut have_width = false;
        let mut long_count = 0u8;
        loop {
            let c = match read_string_byte(&mut word_buf, &mut word_addr, &mut byte_idx_within_word, &mut fmt_cursor) {
                Some(c) => c,
                None => break 'outer,
            };
            match c {
                b'-' => flags.left = true,
                b'+' => flags.plus = true,
                b' ' => flags.space = true,
                b'#' => flags.alt = true,
                b'0' if !have_width => flags.zero = true,
                b'1'..=b'9' => {
                    width = width * 10 + (c - b'0') as u32;
                    have_width = true;
                }
                b'0' => {
                    width = width * 10;
                    have_width = true;
                }
                b'l' | b'L' => long_count = long_count.saturating_add(1),
                b'*' => {
                    // Width comes from the next vararg (a signed int).
                    let w = args.next() as i32;
                    if w >= 0 {
                        width = w as u32;
                    }
                    have_width = true;
                }
                b'.' => {
                    // Skip a precision spec without honoring it.
                    loop {
                        let p = match read_string_byte(&mut word_buf, &mut word_addr, &mut byte_idx_within_word, &mut fmt_cursor) {
                            Some(p) => p,
                            None => break 'outer,
                        };
                        if !p.is_ascii_digit() && p != b'*' {
                            // p is the actual specifier; restart loop with this byte.
                            handle_spec(p, &flags, width, long_count, &mut args, &mut buf, &mut written);
                            break;
                        }
                    }
                    break;
                }
                _ => {
                    handle_spec(c, &flags, width, long_count, &mut args, &mut buf, &mut written);
                    break;
                }
            }
            if written == RENDER_CAP {
                break 'outer;
            }
        }
        if written == RENDER_CAP {
            break;
        }
    }

    append_to_line(prefix, &buf[..written]);
}

// ---- line buffering ------------------------------------------------------
//
// The kernel's TInterpreter trace fragments arrive as separate Print
// calls — one per token (function name, '(', each arg, ')'). Printing
// each fragment on its own UART line is unreadable. We accumulate
// fragments in a static byte buffer keyed by `prefix`; when a `\n`
// appears (or `flush_line` is called explicitly), we emit the
// accumulated line via `kprintln!` and reset.

const LINE_CAP: usize = 256;

struct LineBuf {
    bytes: [u8; LINE_CAP],
    n: usize,
}

static mut LINE: LineBuf = LineBuf { bytes: [0; LINE_CAP], n: 0 };

/// Append a chunk of rendered output. Splits at `\n`: complete lines
/// flush via `kprintln!` (with `prefix` prepended); a trailing
/// fragment without a newline is buffered for the next call.
pub fn append_to_line(prefix: &str, chunk: &[u8]) {
    // SAFETY: single-threaded EL2.
    unsafe {
        for &b in chunk {
            if b == b'\n' || b == b'\r' {
                if LINE.n > 0 {
                    let s = core::str::from_utf8(&LINE.bytes[..LINE.n])
                        .unwrap_or("<non-utf8>");
                    crate::kprintln!("{}{}", prefix, s);
                    LINE.n = 0;
                }
                continue;
            }
            if LINE.n < LINE_CAP {
                LINE.bytes[LINE.n] = b;
                LINE.n += 1;
            } else {
                // Buffer full — force-flush at cap so we don't drop bytes
                // silently.
                let s = core::str::from_utf8(&LINE.bytes[..LINE.n])
                    .unwrap_or("<non-utf8>");
                crate::kprintln!("{}{} [buf-full]", prefix, s);
                LINE.n = 0;
                LINE.bytes[0] = b;
                LINE.n = 1;
            }
        }
    }
}

/// Flush any partial line. Called from the Flush thunk hook so an
/// explicit kernel-side `flush()` doesn't strand a half-built line.
pub fn flush_line(prefix: &str) {
    // SAFETY: single-threaded EL2.
    unsafe {
        if LINE.n > 0 {
            let s = core::str::from_utf8(&LINE.bytes[..LINE.n])
                .unwrap_or("<non-utf8>");
            crate::kprintln!("{}{}", prefix, s);
            LINE.n = 0;
        }
    }
}

/// Append a single character (from the abstract `Putc` thunk).
pub fn putc(prefix: &str, c: u8) {
    append_to_line(prefix, &[c]);
}

#[derive(Default, Clone, Copy)]
struct SpecFlags {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
}

fn handle_spec(
    c: u8,
    flags: &SpecFlags,
    width: u32,
    _long: u8,
    args: &mut VaArgs,
    buf: &mut [u8],
    written: &mut usize,
) {
    let mut tmp = [0u8; 32];
    let mut tmp_n = 0usize;
    match c {
        b'%' => {
            push_byte(buf, written, b'%');
            return;
        }
        b'd' | b'i' => {
            let v = args.next() as i32;
            tmp_n = fmt_signed_dec(&mut tmp, v);
        }
        b'u' => {
            let v = args.next();
            tmp_n = fmt_unsigned_dec(&mut tmp, v);
        }
        b'x' | b'p' => {
            let v = args.next();
            if flags.alt || c == b'p' {
                tmp[tmp_n] = b'0';
                tmp[tmp_n + 1] = b'x';
                tmp_n += 2;
            }
            tmp_n += fmt_hex(&mut tmp[tmp_n..], v, false);
        }
        b'X' => {
            let v = args.next();
            if flags.alt {
                tmp[tmp_n] = b'0';
                tmp[tmp_n + 1] = b'X';
                tmp_n += 2;
            }
            tmp_n += fmt_hex(&mut tmp[tmp_n..], v, true);
        }
        b'c' => {
            let v = args.next() as u8;
            tmp[0] = if v.is_ascii() && v >= 0x20 { v } else { b'?' };
            tmp_n = 1;
        }
        b's' => {
            let p = args.next();
            push_string_arg(buf, written, p, width as usize, flags.left);
            return;
        }
        _ => {
            // Unknown — emit "%c" verbatim so the operator sees something.
            tmp[0] = b'%';
            tmp[1] = c;
            tmp_n = 2;
        }
    }

    let pad = (width as usize).saturating_sub(tmp_n);
    if !flags.left {
        let fill = if flags.zero && (c == b'd' || c == b'i' || c == b'u' || c == b'x' || c == b'X' || c == b'p') {
            b'0'
        } else {
            b' '
        };
        for _ in 0..pad {
            push_byte(buf, written, fill);
            if *written == buf.len() {
                return;
            }
        }
    }
    for &b in &tmp[..tmp_n] {
        push_byte(buf, written, b);
        if *written == buf.len() {
            return;
        }
    }
    if flags.left {
        for _ in 0..pad {
            push_byte(buf, written, b' ');
            if *written == buf.len() {
                return;
            }
        }
    }
}

fn push_byte(buf: &mut [u8], written: &mut usize, b: u8) {
    if *written < buf.len() {
        buf[*written] = b;
        *written += 1;
    }
}

fn push_string_arg(buf: &mut [u8], written: &mut usize, addr: u32, width: usize, left: bool) {
    let mut sbuf = [0u8; STRING_CAP];
    let mut n = 0usize;
    let mut byte_idx: u8 = (addr & 0x3) as u8;
    let mut word_buf: Option<[u8; 4]> = None;
    let mut word_addr = addr & !0x3;
    let mut cur = addr;
    while n < STRING_CAP {
        let b = match read_string_byte(&mut word_buf, &mut word_addr, &mut byte_idx, &mut cur) {
            Some(b) => b,
            None => break,
        };
        if b == 0 {
            break;
        }
        sbuf[n] = b;
        n += 1;
    }
    let pad = width.saturating_sub(n);
    if !left {
        for _ in 0..pad {
            push_byte(buf, written, b' ');
            if *written == buf.len() { return; }
        }
    }
    for &b in &sbuf[..n] {
        push_byte(buf, written, b);
        if *written == buf.len() { return; }
    }
    if left {
        for _ in 0..pad {
            push_byte(buf, written, b' ');
            if *written == buf.len() { return; }
        }
    }
}

fn fmt_signed_dec(out: &mut [u8], v: i32) -> usize {
    if v >= 0 {
        return fmt_unsigned_dec(out, v as u32);
    }
    out[0] = b'-';
    1 + fmt_unsigned_dec(&mut out[1..], v.unsigned_abs())
}

fn fmt_unsigned_dec(out: &mut [u8], mut v: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut n = 0usize;
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    while v != 0 && n < digits.len() {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for i in 0..n {
        out[i] = digits[n - 1 - i];
    }
    n
}

fn fmt_hex(out: &mut [u8], mut v: u32, upper: bool) -> usize {
    let mut digits = [0u8; 8];
    let mut n = 0usize;
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    while v != 0 && n < digits.len() {
        let d = (v & 0xF) as u8;
        digits[n] = if d < 10 {
            b'0' + d
        } else if upper {
            b'A' + d - 10
        } else {
            b'a' + d - 10
        };
        v >>= 4;
        n += 1;
    }
    for i in 0..n {
        out[i] = digits[n - 1 - i];
    }
    n
}

/// Read one byte of a guest C string, honoring the per-word
/// byteswap that `load_rom` applied. We read u32 words, convert
/// to big-endian bytes (which restores the original on-disk
/// order), and stream out byte-by-byte.
fn read_string_byte(
    word_buf: &mut Option<[u8; 4]>,
    word_addr: &mut u32,
    byte_idx: &mut u8,
    cursor: &mut u32,
) -> Option<u8> {
    if word_buf.is_none() {
        let w = guest_mem::read_word_va(*word_addr)
            .or_else(|| guest_mem::read_word_pa(*word_addr))?;
        *word_buf = Some(w.to_be_bytes());
    }
    let bytes = word_buf.expect("word_buf is Some");
    let b = bytes[*byte_idx as usize];
    *byte_idx += 1;
    *cursor = cursor.wrapping_add(1);
    if *byte_idx == 4 {
        *byte_idx = 0;
        *word_addr = word_addr.wrapping_add(4);
        *word_buf = None;
    }
    Some(b)
}
