//! Serial debug pen injector (`serial-pen-inject` feature only).
//!
//! Closes the see-and-measure loop for real-hardware video work: the
//! HDMI digitizer lets the host *observe* the panel, and this module
//! lets it *drive* the UI without the physical touchscreen — a tap can
//! be injected from a script over the same USB-TTL console cable
//! `scripts/pi-upload.py` uploads through
//! (`scripts/capture-timing.py --tap` is the host-side sender).
//!
//! Mechanics: `main.rs` wires [`read_byte_nonblock`] in place of
//! `host::console::read_byte_nonblock` as the guest external-serial RX
//! seam, and wires [`pump_and_host_io_input`] as the trap-tail host-io
//! input pump. Every host-console RX byte funnels through one escape
//! state machine (with the `serial-mux` feature, every *unframed*
//! byte — the multiplexer hands framed guest traffic straight to the
//! guest and this parser only sees the control channel, see
//! `host::serial_mux`):
//!
//!   * `~p<x>,<y>\n` — tap at Newton screen coords (x, y) held for
//!     [`DEFAULT_HOLD_MS`].
//!   * `~p<x>,<y>,<ms>\n` — tap held for `<ms>` (clamped to
//!     [`MAX_HOLD_MS`]).
//!   * `~~` — a literal `~` forwarded to the guest.
//!
//! Recognised sequences are consumed — the guest never sees their
//! bytes. Anything else after `~` is forwarded verbatim (the `~` plus
//! the byte), and a malformed/overlong command is dropped with a
//! `kprintln!` note, so line noise degrades to a lost escape sequence
//! rather than a wedged parser. Coordinates are **Newton screen
//! pixels** (320×480 on the pinned pi_fb geometry) — the same space
//! `input::calibrate::panel_to_newton` produces and
//! `host_io::pack_pen_sample` expects.
//!
//! A tap enqueues exactly what a stationary physical mtouch tap
//! produces: `PEN_DOWN_SAMPLE_MARKER` + one packed sample on the down
//! edge (plus the power-switch wake when the guest is parked in
//! PowerOff, mirroring `input::drain_into_queue`), then
//! `PEN_UP_SAMPLE_MARKER` once the hold time elapses. The pen-up is
//! driven from [`pump`], which runs on the trap-return tail, so no new
//! interrupt path exists.
//!
//! Why the pump exists at all: the RX seam is only polled while the
//! guest keeps its external-serial RX DMA armed
//! (`peripherals::dma::poll_rx` returns early otherwise), which a
//! booted Newton doesn't guarantee. [`pump`] therefore drains the host
//! wire itself on a CNTPCT throttle (the `NEXT_PUMP_CNTPCT` idiom from
//! `host_io::semihost`), parking pass-through bytes in a small ring
//! that [`read_byte_nonblock`] serves before touching the wire — byte
//! order to the guest is preserved.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::host::host_io::{
    pack_pen_sample, queue, PEN_DOWN_SAMPLE_MARKER, PEN_UP_SAMPLE_MARKER,
};
use crate::kprintln;

/// Pen-down → pen-up hold when the command carries no duration.
/// Long enough that the guest's tablet sampling sees the down state
/// on at least a few 16 ms polls.
const DEFAULT_HOLD_MS: u32 = 80;
/// Upper bound on a requested hold, so a garbled duration can't wedge
/// the pen down for minutes.
const MAX_HOLD_MS: u32 = 5_000;
/// Same fixed pressure as `input::drain_into_queue`.
const PRESSURE: u16 = 4;
/// Wire-drain throttle for [`pump`].
const PUMP_INTERVAL_MS: u64 = 8;

/// Escape / command parser state.
enum Parse {
    /// Forwarding bytes to the guest.
    Idle,
    /// Saw `~`, waiting for the command byte.
    Escape,
    /// Collecting a `p` command line into `buf[..len]`.
    Cmd { buf: [u8; 16], len: usize },
}

struct State {
    parse: Parse,
    /// Pass-through bytes drained off the wire by [`pump`] (or pushed
    /// back by the parser) that the guest hasn't consumed yet.
    /// Head==tail is empty; capacity PASS_LEN-1. Drop-newest on
    /// overflow — this is a human-typed debug channel, not a data
    /// link.
    pass: [u8; PASS_LEN],
    pass_head: usize,
    pass_tail: usize,
    /// CNTPCT-derived µs deadline for the pending pen-up; 0 = no tap
    /// in flight.
    pen_up_at_us: u64,
}

const PASS_LEN: usize = 64;

impl State {
    const fn new() -> Self {
        Self {
            parse: Parse::Idle,
            pass: [0; PASS_LEN],
            pass_head: 0,
            pass_tail: 0,
            pen_up_at_us: 0,
        }
    }

    fn pass_push(&mut self, b: u8) {
        let next = (self.pass_head + 1) % PASS_LEN;
        if next == self.pass_tail {
            return; // full — drop
        }
        self.pass[self.pass_head] = b;
        self.pass_head = next;
    }

    fn pass_pop(&mut self) -> Option<u8> {
        if self.pass_tail == self.pass_head {
            return None;
        }
        let b = self.pass[self.pass_tail];
        self.pass_tail = (self.pass_tail + 1) % PASS_LEN;
        Some(b)
    }
}

struct StateCell(core::cell::UnsafeCell<State>);
// SAFETY: single-core EL2; both entry points ([`read_byte_nonblock`]
// from the guest DMA poll, [`pump`] from the trap-return tail) run on
// the same single-threaded trap-handling path.
unsafe impl Sync for StateCell {}

static STATE: StateCell = StateCell(core::cell::UnsafeCell::new(State::new()));

/// Throttle for [`pump`] — next CNTPCT tick at which the wire is
/// drained again (0 = immediately). Same shape as the semihost
/// backend's `NEXT_PUMP_CNTPCT`.
static NEXT_PUMP_CNTPCT: AtomicU64 = AtomicU64::new(0);

fn with_state<R, F: FnOnce(&mut State) -> R>(f: F) -> R {
    // SAFETY: see StateCell.
    let s = unsafe { &mut *STATE.0.get() };
    f(s)
}

/// Guest-facing RX seam replacement for
/// `host::console::read_byte_nonblock`: serves parser-forwarded bytes
/// first, then pulls fresh wire bytes through the escape state
/// machine. Installed by `main.rs` when `serial-pen-inject` is on.
pub fn read_byte_nonblock() -> Option<u8> {
    // Under the multiplexer the guest's own bytes arrive framed and
    // bypass the parser entirely; the parser only sees the unframed
    // control channel.
    #[cfg(nh_serial_mux)]
    if let Some(b) = crate::host::serial_mux::rx_guest() {
        return Some(b);
    }
    with_state(|s| {
        loop {
            if let Some(b) = s.pass_pop() {
                return Some(b);
            }
            let b = wire_rx()?;
            feed(s, b);
            // feed() may have consumed the byte (escape traffic) or
            // parked it in the pass ring; loop to pick up either
            // outcome or the next wire byte.
        }
    })
}

/// Trap-tail pump: fires any due pen-up, then drains the host wire
/// through the parser on a [`PUMP_INTERVAL_MS`] throttle so escape
/// commands work even while the guest's serial RX DMA is not armed.
/// Wrapped together with `host_io::pump_input` by
/// [`pump_and_host_io_input`].
pub fn pump() {
    let now_us = crate::host::console::now_us();
    with_state(|s| {
        if s.pen_up_at_us != 0 && now_us >= s.pen_up_at_us {
            s.pen_up_at_us = 0;
            queue::enqueue_pen_sample(PEN_UP_SAMPLE_MARKER, 0);
        }
    });

    let now = cntpct();
    let next = NEXT_PUMP_CNTPCT.load(Ordering::Relaxed);
    if next != 0 && now < next {
        return;
    }
    let interval = (PUMP_INTERVAL_MS * cntfrq()) / 1_000;
    NEXT_PUMP_CNTPCT.store(now.wrapping_add(interval), Ordering::Relaxed);

    with_state(|s| {
        // Bounded drain so the trap tail stays cheap even against a
        // babbling wire.
        for _ in 0..256 {
            let Some(b) = wire_rx() else {
                break;
            };
            feed(s, b);
        }
    });
}

/// The parser's byte source: the raw console wire, or — under the
/// serial multiplexer — its unframed control channel (framed guest
/// traffic never passes through the escape parser, so a `~` inside
/// an MNP frame cannot be mistaken for a command).
fn wire_rx() -> Option<u8> {
    #[cfg(nh_serial_mux)]
    {
        crate::host::serial_mux::rx_unframed()
    }
    #[cfg(not(nh_serial_mux))]
    {
        crate::host::console::read_byte_nonblock()
    }
}

/// Trap-tail composite installed as `HostPumpOps::host_io_pump_input`
/// when the feature is on: injector pump first (so a freshly enqueued
/// pen sample's `INT_TABLET` reaches `update_virq` on this same trap
/// exit), then the regular host-io input pump.
#[cfg(not(nh_serial_mux))]
pub fn pump_and_host_io_input() {
    pump();
    crate::host::host_io::pump_input();
}

/// Run one wire byte through the escape state machine. Pass-through
/// bytes land in the pass ring; escape-command bytes are consumed.
fn feed(s: &mut State, b: u8) {
    match s.parse {
        Parse::Idle => {
            if b == b'~' {
                s.parse = Parse::Escape;
            } else {
                s.pass_push(b);
            }
        }
        Parse::Escape => match b {
            b'p' => {
                s.parse = Parse::Cmd {
                    buf: [0; 16],
                    len: 0,
                };
            }
            b'~' => {
                // `~~` = literal tilde for the guest.
                s.pass_push(b'~');
                s.parse = Parse::Idle;
            }
            _ => {
                // Not a command: forward the swallowed `~` and the
                // byte unchanged.
                s.pass_push(b'~');
                s.pass_push(b);
                s.parse = Parse::Idle;
            }
        },
        Parse::Cmd { ref mut buf, ref mut len } => match b {
            b'\n' | b'\r' => {
                let cmd = &buf[..*len];
                match parse_tap(cmd) {
                    Some((x, y, hold_ms)) => tap_down(s, x, y, hold_ms),
                    None => {
                        kprintln!("serial_pen: dropped malformed ~p command");
                    }
                }
                s.parse = Parse::Idle;
            }
            b'0'..=b'9' | b',' => {
                if *len >= buf.len() {
                    kprintln!("serial_pen: dropped overlong ~p command");
                    s.parse = Parse::Idle;
                } else {
                    buf[*len] = b;
                    *len += 1;
                }
            }
            _ => {
                // Line noise inside a command: drop the sequence
                // (loudly) rather than forwarding garbage.
                kprintln!(
                    "serial_pen: dropped ~p command on unexpected byte {:#04x}",
                    b
                );
                s.parse = Parse::Idle;
            }
        },
    }
}

/// Parse `x,y` or `x,y,ms` from ASCII decimal fields. Returns
/// `(x, y, hold_ms)`; coordinates are bounded to the 11-bit packed
/// sample range up front so a garbled value is rejected instead of
/// silently masked.
fn parse_tap(cmd: &[u8]) -> Option<(u16, u16, u32)> {
    let mut fields = [0u32; 3];
    let mut n_fields = 0usize;
    let mut cur: u32 = 0;
    let mut have_digit = false;
    for &c in cmd {
        match c {
            b'0'..=b'9' => {
                cur = cur.checked_mul(10)?.checked_add((c - b'0') as u32)?;
                have_digit = true;
            }
            b',' => {
                if !have_digit || n_fields >= 2 {
                    return None;
                }
                fields[n_fields] = cur;
                n_fields += 1;
                cur = 0;
                have_digit = false;
            }
            _ => return None,
        }
    }
    if !have_digit || n_fields < 1 {
        return None;
    }
    fields[n_fields] = cur;
    n_fields += 1;
    if n_fields < 2 {
        return None;
    }
    let (x, y) = (fields[0], fields[1]);
    if x >= 0x800 || y >= 0x800 {
        return None;
    }
    let hold_ms = if n_fields == 3 {
        fields[2].min(MAX_HOLD_MS)
    } else {
        DEFAULT_HOLD_MS
    };
    Some((x as u16, y as u16, hold_ms))
}

/// Inject the pen-down edge and schedule the pen-up. Mirrors the
/// mtouch Down path in `input::drain_into_queue`: power-switch wake
/// when the guest is parked in PowerOff, then the down marker plus one
/// packed sample.
fn tap_down(s: &mut State, x: u16, y: u16, hold_ms: u32) {
    // A tap arriving while one is still held: finish the previous tap
    // first so the guest sees a clean down/up pairing.
    if s.pen_up_at_us != 0 {
        s.pen_up_at_us = 0;
        queue::enqueue_pen_sample(PEN_UP_SAMPLE_MARKER, 0);
    }
    if crate::peripherals::vic::is_powered_off() {
        crate::peripherals::vic::raise_power_switch();
    }
    queue::enqueue_pen_sample(PEN_DOWN_SAMPLE_MARKER, 0);
    queue::enqueue_pen_sample(pack_pen_sample(x, y, PRESSURE), 0);
    s.pen_up_at_us = crate::host::console::now_us() + hold_ms as u64 * 1_000;
    kprintln!(
        "serial_pen: tap at newton ({},{}) hold {} ms",
        x,
        y,
        hold_ms
    );
}

fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}
