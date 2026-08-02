//! Semihosting-files transport for the host-IO backend.
//!
//! Outbound: `/tmp/newton-host-io/out` (truncated and opened at boot
//! for write-binary). Inbound: `/tmp/newton-host-io/in` (opened at
//! boot for read-binary, polled at ~60 Hz by `pump_input`).
//!
//! The companion viewer at `tools/host-viewer/` is responsible for
//! reading `out` and appending `PenEvent` records to `in`. Run order:
//!
//!   # term 1
//!   mkdir -p /tmp/newton-host-io
//!   cargo run --release --no-default-features \
//!     --features 'platform-raspi3b host-io-semihost'
//!
//!   # term 2  (after term 1 prints "host_io: outbound /tmp/newton-host-io/out")
//!   cargo run -p host-viewer
//!
//! Each `push_blit` issues two SYS_WRITE calls (header then payload);
//! the EL2 vCPU stalls until QEMU/FVP returns from the semihost trap.
//! Acceptable here: the previous fb_dump path stalled in the same
//! way once a second, and a typical UI redraw of ~4 KiB completes in
//! under a millisecond.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::log_host_io;

const SYS_OPEN: u64 = 0x01;
const SYS_CLOSE: u64 = 0x02;
const SYS_WRITE: u64 = 0x05;
const SYS_READ: u64 = 0x06;
const SYS_SEEK: u64 = 0x0A;
const SYS_FLEN: u64 = 0x0C;
const SYS_SYSTEM: u64 = 0x12;

const MODE_READ_BINARY: u64 = 0x01; // "rb"
const MODE_WRITE_BINARY: u64 = 0x05; // "wb"

const OUT_PATH: &[u8] = b"/tmp/newton-host-io/out\0";
const IN_PATH: &[u8] = b"/tmp/newton-host-io/in\0";

const PUMP_INTERVAL_MS: u64 = 16;

struct State {
    out_fh: i64,
    in_fh: i64,
    in_pos: u64,
}

struct StateCell(UnsafeCell<State>);
// SAFETY: only accessed from the single-threaded EL2 trap handler.
unsafe impl Sync for StateCell {}

static STATE: StateCell = StateCell(UnsafeCell::new(State {
    out_fh: -1,
    in_fh: -1,
    in_pos: 0,
}));

static INITIALISED: AtomicBool = AtomicBool::new(false);
static NEXT_PUMP_CNTPCT: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    ensure_output_dir();
    // SAFETY: single-threaded init from kmain.
    let s = unsafe { &mut *STATE.0.get() };
    let out = sh_open(OUT_PATH, MODE_WRITE_BINARY);
    let inh = sh_open(IN_PATH, MODE_READ_BINARY);
    if out < 0 {
        log_host_io!("host_io: SYS_OPEN {:?} (wb) failed; outbound disabled",
            core::str::from_utf8(&OUT_PATH[..OUT_PATH.len() - 1]).unwrap_or("?"));
    } else {
        log_host_io!("host_io: outbound /tmp/newton-host-io/out fh={}", out);
    }
    if inh < 0 {
        log_host_io!("host_io: SYS_OPEN {:?} (rb) failed; inbound disabled (touch the file with the host viewer first)",
            core::str::from_utf8(&IN_PATH[..IN_PATH.len() - 1]).unwrap_or("?"));
    } else {
        log_host_io!("host_io: inbound  /tmp/newton-host-io/in  fh={}", inh);
    }
    s.out_fh = out;
    s.in_fh = inh;
    // Start reading from end-of-file. /tmp/newton-host-io/in is a
    // FIFO-ish append log shared across sessions: the host viewer
    // appends pen events to it, but the file isn't cleared between
    // hypervisor runs. Starting at 0 would replay stale events from
    // the previous session — which the kernel processes as if they
    // were current taps. The host-viewer also truncates this file on
    // its own startup (tools/host-viewer/src/main.rs); `pump_input`
    // handles that case separately by detecting `len < in_pos`.
    s.in_pos = if inh >= 0 {
        let n = sh_flen(inh);
        if n >= 0 { n as u64 } else { 0 }
    } else {
        0
    };
    INITIALISED.store(true, Ordering::Release);
}

pub fn on_resume() {
    // Drop any pen events that arrived between snapshot save and
    // resume — they're timing-stale. The shared input queue is
    // already cleared by the caller.
    // SAFETY: see STATE.
    let s = unsafe { &mut *STATE.0.get() };
    if s.in_fh >= 0 {
        let len = sh_flen(s.in_fh);
        if len >= 0 {
            s.in_pos = len as u64;
        }
    }
}

pub fn push_blit(ev: &super::BlitEvent, payload: &[u8]) {
    if !INITIALISED.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: STATE accessed under EL2 single-threaded invariant.
    let s = unsafe { &*STATE.0.get() };
    if s.out_fh < 0 {
        return;
    }
    // BlitEvent is 24 bytes, repr(C, packed) — write the header bytes
    // verbatim and then the payload. Two SYS_WRITEs because the
    // semihost ABI takes (handle, ptr, len) and we have two slices.
    let header_ptr = ev as *const _ as *const u8;
    let header_len = core::mem::size_of::<super::BlitEvent>();
    // SAFETY: the header bytes live for the duration of the call.
    let header = unsafe { core::slice::from_raw_parts(header_ptr, header_len) };
    let _ = sh_write(s.out_fh, header);
    if !payload.is_empty() {
        let _ = sh_write(s.out_fh, payload);
    }
}

pub fn pump_input() {
    if !INITIALISED.load(Ordering::Acquire) {
        return;
    }
    let now = cntpct();
    let next = NEXT_PUMP_CNTPCT.load(Ordering::Relaxed);
    if next != 0 && now < next {
        return;
    }
    let interval = (PUMP_INTERVAL_MS * cntfrq()) / 1_000;
    NEXT_PUMP_CNTPCT.store(now.wrapping_add(interval), Ordering::Relaxed);

    // SAFETY: STATE accessed under EL2 single-threaded invariant.
    let s = unsafe { &mut *STATE.0.get() };
    if s.in_fh < 0 {
        return;
    }
    let len = sh_flen(s.in_fh);
    if len < 0 {
        return;
    }
    let len = len as u64;
    // The host viewer truncates `/tmp/newton-host-io/in` on its own
    // startup, so the file can get shorter than our last-read
    // position. Reset and read from the new beginning when that
    // happens — otherwise we'd silently drop everything until the
    // file grows back past the stale offset, which presents as "pen
    // input takes a while to start working."
    if len < s.in_pos {
        s.in_pos = 0;
    }
    if len <= s.in_pos {
        return;
    }
    let want = (len - s.in_pos).min(BUF_LEN as u64) as usize;
    if sh_seek(s.in_fh, s.in_pos as i64) < 0 {
        return;
    }
    let buf = scratch_buf();
    let n = sh_read(s.in_fh, &mut buf[..want]);
    // semihosting SYS_READ returns "bytes not read"; on success n == 0
    // means we got the full buffer. We accept a partial read.
    let got = want.saturating_sub(n);
    if got == 0 {
        return;
    }
    s.in_pos = s.in_pos.wrapping_add(got as u64);

    // Each PenEvent is 8 bytes. Decode whole records; ignore a
    // trailing partial. Track pen-down state so we can inject the
    // 0x000D / 0x000E edge markers around x/y samples per
    // TScreenManager::PenDown / PenUp.
    static DOWN: AtomicBool = AtomicBool::new(false);
    let ev_size = core::mem::size_of::<super::PenEvent>();
    let n_evs = got / ev_size;
    log_host_io!("host_io: pump_input drained {} byte(s), {} pen event(s)", got, n_evs);
    for i in 0..n_evs {
        let off = i * ev_size;
        // SAFETY: ev_size bytes at off..off+ev_size; PenEvent is repr(C,packed).
        let ev: super::PenEvent = unsafe {
            core::ptr::read_unaligned(buf[off..].as_ptr() as *const super::PenEvent)
        };
        let kind = ev.kind;
        let x = ev.x;
        let y = ev.y;
        let pressure = ev.pressure;
        // Only log pen-down / pen-up edges; move events are very noisy
        // during a drag, and POWER_SWITCH has its own dedicated log
        // line.
        if kind != super::PEN_MOVE && kind != super::POWER_SWITCH {
            log_host_io!(
                "host_io: pen kind={} x={} y={} p={}  vic.ictrl={:#010x} vic.ipres={:#010x}",
                kind, x, y, pressure,
                crate::peripherals::vic::int_ctrl_raw(),
                crate::peripherals::vic::int_present_raw(),
            );
        }
        // ticks = 0 — Einstein's InsertSample tolerates this and the
        // kernel substitutes GetTimer() when zero.
        match kind {
            super::PEN_DOWN => {
                if !DOWN.swap(true, Ordering::AcqRel) {
                    super::queue::enqueue_pen_sample(super::PEN_DOWN_SAMPLE_MARKER, 0);
                }
                super::queue::enqueue_pen_sample(super::pack_pen_sample(x, y, pressure), 0);
            }
            super::PEN_MOVE => {
                if DOWN.load(Ordering::Acquire) {
                    super::queue::enqueue_pen_sample(super::pack_pen_sample(x, y, pressure), 0);
                }
            }
            super::PEN_UP => {
                if DOWN.swap(false, Ordering::AcqRel) {
                    super::queue::enqueue_pen_sample(super::PEN_UP_SAMPLE_MARKER, 0);
                }
            }
            super::POWER_SWITCH => {
                log_host_io!("host_io: power-switch press");
                crate::peripherals::vic::raise_power_switch();
            }
            _ => {}
        }
    }
}

// ---- scratch + semihosting helpers ----

const BUF_LEN: usize = 4096;

fn scratch_buf() -> &'static mut [u8; BUF_LEN] {
    struct BufCell(UnsafeCell<[u8; BUF_LEN]>);
    // SAFETY: single-threaded EL2.
    unsafe impl Sync for BufCell {}
    static BUF: BufCell = BufCell(UnsafeCell::new([0; BUF_LEN]));
    // SAFETY: only `pump_input` calls this and it's not re-entered.
    unsafe { &mut *BUF.0.get() }
}

fn ensure_output_dir() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    // mkdir + touch. SYS_OPEN in "rb" mode won't create a missing
    // file, so we make sure both `out` and `in` exist before our
    // opens fire — otherwise a first-run with the viewer not yet
    // started leaves the inbound channel disabled until the next
    // reboot.
    let cmd = b"mkdir -p /tmp/newton-host-io && touch /tmp/newton-host-io/in /tmp/newton-host-io/out\0";
    let args: [u64; 2] = [cmd.as_ptr() as u64, (cmd.len() - 1) as u64];
    let _ = unsafe { semihost(SYS_SYSTEM, args.as_ptr()) };
}

unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
    let result: u64;
    // SAFETY: HLT #0xF000 is the AArch64 semihosting trap.
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

fn sh_open(path: &[u8], mode: u64) -> i64 {
    let args: [u64; 3] = [
        path.as_ptr() as u64,
        mode,
        (path.len() - 1) as u64,
    ];
    unsafe { semihost(SYS_OPEN, args.as_ptr()) }
}

fn sh_write(fh: i64, data: &[u8]) -> i64 {
    let args: [u64; 3] = [fh as u64, data.as_ptr() as u64, data.len() as u64];
    unsafe { semihost(SYS_WRITE, args.as_ptr()) }
}

fn sh_read(fh: i64, buf: &mut [u8]) -> usize {
    let args: [u64; 3] = [fh as u64, buf.as_mut_ptr() as u64, buf.len() as u64];
    let r = unsafe { semihost(SYS_READ, args.as_ptr()) };
    if r < 0 { buf.len() } else { r as usize }
}

fn sh_seek(fh: i64, pos: i64) -> i64 {
    let args: [u64; 2] = [fh as u64, pos as u64];
    unsafe { semihost(SYS_SEEK, args.as_ptr()) }
}

fn sh_flen(fh: i64) -> i64 {
    let args: [u64; 1] = [fh as u64];
    unsafe { semihost(SYS_FLEN, args.as_ptr()) }
}

#[allow(dead_code)]
fn sh_close(fh: i64) {
    let args: [u64; 1] = [fh as u64];
    let _ = unsafe { semihost(SYS_CLOSE, args.as_ptr()) };
}

fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: sysreg read, side-effect free.
    unsafe { asm!("mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack, preserves_flags)); }
    v
}

fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: sysreg read.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)); }
    v
}
