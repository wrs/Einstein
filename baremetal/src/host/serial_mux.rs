//! Guest external-serial ↔ host console-wire multiplexer
//! (`cfg(nh_serial_mux)`: the `serial-mux` feature on a build whose
//! guest serial wire is the PL011).
//!
//! On the Pi the single PL011 is the only UART that reaches the GPIO
//! header (both on-chip UARTs multiplex onto GPIO 14/15 — see
//! `docs/REAL_HW_BRINGUP.md` "Guest serial over the console wire"),
//! and it already carries the kernel log. This module lets the
//! guest's `extr` port share that wire by framing the guest bytes,
//! leaving the log as plain text:
//!
//! ```text
//!   host ← Pi   log text …  FF 01 <len> <len payload bytes>  log text …
//!   host → Pi   FF 01 <len> <payload>        guest-bound bytes
//!               anything else                the control channel
//! ```
//!
//! * `0xFF` is the start-of-frame byte. It never occurs in the log:
//!   `kprintln!` output is UTF-8 (`0xFF` is not a valid UTF-8 byte),
//!   and the raw `write_byte` callers (tarmac markers, guest-test
//!   print) emit ASCII. The log direction therefore needs no
//!   escaping; the payload needs none either because it is
//!   length-prefixed.
//! * Channel `0x01` is the guest's external serial port. Other
//!   channel numbers are reserved; a frame on one is consumed and
//!   dropped (counted) so the stream stays in sync.
//! * `len` is 1..=255. A zero-length frame is accepted and ignored.
//! * Unframed bytes from the host are the *control channel*: with
//!   `serial-pen-inject` they feed the `~p<x>,<y>` tap parser in
//!   `host::serial_pen` (which passes non-command bytes on to the
//!   guest, exactly as it does on a raw wire); without it they are
//!   discarded (counted). The host tool frames everything guest-bound,
//!   so an unframed byte is either an operator command or line noise.
//!   (A stray `0xFF` from noise is read as a frame start and swallows
//!   up to 257 bytes; MNP's own CRC then rejects the damaged frame and
//!   the link layer retransmits.)
//!
//! **Downstream (guest → host).** `tx` stages guest TX bytes; `flush_tx`
//! (from the trap tail, and when the stage fills) emits one frame
//! through the console's own output path — the DMA TX ring on real
//! hardware — as a single all-or-nothing enqueue, so a frame is never
//! cut by the ring's drop-newest policy and log lines are never split
//! mid-frame. The guest produces its bytes inside a trap (a DMA
//! drain or a PIO write) and that trap's tail flushes them, so the
//! staging adds no latency.
//!
//! **Upstream (host → guest).** The PL011 RX FIFO is 16 bytes — 1.4 ms
//! at 115200 baud — while the trap tail can be a 16 ms heartbeat apart
//! on an idle guest, so on real hardware the FIFO is drained by the
//! PL011 RX / receive-timeout interrupt into a raw byte ring
//! ([`on_rx_irq`], reached from `platform::dispatch_uart_rx` on the
//! EL2 IRQ entry). The ring is single-producer / single-consumer: the
//! producer runs with IRQs masked (the ISR, or the trap-tail pump's
//! own masked drain, which doubles as the only producer where there
//! is no RX interrupt — QEMU / FVP), the consumer is the trap-tail
//! decoder. The decoder sorts bytes into the guest ring (framed
//! payload, served to `peripherals::dma::poll_rx` through
//! [`rx_guest`]) and the control ring ([`rx_unframed`]). A full guest
//! ring stops the decoder — the bytes wait in the raw ring — so the
//! only lossy point is the raw ring itself, and every loss (raw-ring
//! overflow, FIFO overrun, dropped TX frame, unknown channel,
//! discarded control byte) is counted and reported on the log.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::host::console;
use crate::kprintln;

/// Start-of-frame byte (both directions).
pub const SOF: u8 = 0xFF;
/// Channel number of the guest's external serial port.
pub const CH_EXTR: u8 = 0x01;
/// Longest frame payload (the length field is one byte).
const MAX_PAYLOAD: usize = 255;
const FRAME_HDR: usize = 3;

/// Raw wire bytes between the RX producer and the decoder. Sized for
/// a full MNP window of 256-byte frames arriving while the decoder
/// is held off by a full guest ring.
const RAW_LEN: usize = 4096;
/// Decoded guest-bound bytes waiting for the guest's RX DMA.
const GUEST_LEN: usize = 4096;
/// Control-channel bytes waiting for the `serial-pen-inject` parser.
const UNFRAMED_LEN: usize = 256;

/// Bytes the RX producer takes off the FIFO per drain. The FIFO holds
/// 16; the bound only matters against a babbling wire.
const DRAIN_BOUND: usize = 64;

/// Minimum spacing of the loss-counter report lines.
const REPORT_INTERVAL_S: u64 = 5;

/// Guest-bound bytes are released to `peripherals::dma::poll_rx` only
/// once the wire has been idle this long, or once this many have
/// accumulated. Without the gate a byte reaches the guest per trap
/// tail — at 115200 baud that is one RX-DMA completion FIQ and one
/// `dma[ch0] RX` log line per byte, and the log alone (~100 chars per
/// received byte) starves the console wire the frames share until both
/// ends of the MNP link time out. Gating makes one host write (one MNP
/// frame) one completion, the same shape the semihost backend's 16 ms
/// cadence produces on QEMU. 1 ms is ~11 byte-times: longer than the
/// PL011's 8-byte FIFO trigger cadence inside a frame, shorter than any
/// inter-frame gap a host tool leaves; 256 B is `poll_rx`'s per-tick cap.
const RX_IDLE_US: u64 = 1_000;
const RX_RELEASE_BYTES: usize = 256;

// ---- raw RX ring (SPSC: IRQ-masked producer, trap-tail consumer) ----

struct RawRing(UnsafeCell<[u8; RAW_LEN]>);
// SAFETY: slots are written by the producer only between
// RAW_HEAD-reserved and RAW_HEAD-published, and read by the consumer
// only below RAW_HEAD (Acquire) — the classic SPSC ring discipline;
// single core, producer always IRQ-masked.
unsafe impl Sync for RawRing {}

static RAW: RawRing = RawRing(UnsafeCell::new([0; RAW_LEN]));
static RAW_HEAD: AtomicUsize = AtomicUsize::new(0);
static RAW_TAIL: AtomicUsize = AtomicUsize::new(0);

/// Loss counters. Producer-side ones are bumped with IRQs masked; all
/// are read (and zeroed) by the trap-tail reporter.
static RAW_DROPPED: AtomicU32 = AtomicU32::new(0);
static FIFO_OVERRUNS: AtomicU32 = AtomicU32::new(0);
/// CNTPCT at the last FIFO drain that moved bytes (producer-written).
static LAST_RX_CNTPCT: AtomicU64 = AtomicU64::new(0);

/// Producer: append one byte. Runs with IRQs masked.
fn raw_push(b: u8) -> bool {
    let head = RAW_HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % RAW_LEN;
    if next == RAW_TAIL.load(Ordering::Acquire) {
        return false;
    }
    // SAFETY: see RawRing — this slot is unpublished until the
    // Release store below.
    unsafe {
        (*RAW.0.get())[head] = b;
    }
    RAW_HEAD.store(next, Ordering::Release);
    true
}

/// Consumer: look at the oldest byte without taking it.
fn raw_peek() -> Option<u8> {
    let tail = RAW_TAIL.load(Ordering::Relaxed);
    if tail == RAW_HEAD.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: see RawRing — published slot, not yet released.
    Some(unsafe { (*RAW.0.get())[tail] })
}

/// Consumer: release the byte `raw_peek` returned.
fn raw_advance() {
    let tail = RAW_TAIL.load(Ordering::Relaxed);
    RAW_TAIL.store((tail + 1) % RAW_LEN, Ordering::Release);
}

/// Move whatever the PL011 RX FIFO holds into the raw ring. The one
/// producer entry point; every caller holds IRQs masked (the ISR by
/// construction, the pump explicitly).
fn drain_fifo() {
    if console::rx_overrun_take() {
        FIFO_OVERRUNS.fetch_add(1, Ordering::Relaxed);
    }
    let mut moved = false;
    for _ in 0..DRAIN_BOUND {
        let Some(b) = console::read_byte_nonblock() else {
            break;
        };
        moved = true;
        if !raw_push(b) {
            RAW_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    if moved {
        LAST_RX_CNTPCT.store(cntpct(), Ordering::Relaxed);
    }
}

// ---- decoder-side state (trap tail only) ---------------------------

struct Ring<const N: usize> {
    buf: [u8; N],
    head: usize,
    tail: usize,
}

impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self { buf: [0; N], head: 0, tail: 0 }
    }
    fn is_full(&self) -> bool {
        (self.head + 1) % N == self.tail
    }
    fn len(&self) -> usize {
        (self.head + N - self.tail) % N
    }
    fn push(&mut self, b: u8) -> bool {
        if self.is_full() {
            return false;
        }
        self.buf[self.head] = b;
        self.head = (self.head + 1) % N;
        true
    }
    fn pop(&mut self) -> Option<u8> {
        if self.tail == self.head {
            return None;
        }
        let b = self.buf[self.tail];
        self.tail = (self.tail + 1) % N;
        Some(b)
    }
}

enum Decode {
    /// Between frames: the next byte is a SOF or a control byte.
    Sof,
    /// After SOF: the next byte is the channel number.
    Ch,
    /// After the channel: the next byte is the payload length.
    Len { ch: u8 },
    /// Inside a payload, `left` bytes to go.
    Payload { ch: u8, left: u8 },
}

struct State {
    decode: Decode,
    guest: Ring<GUEST_LEN>,
    unframed: Ring<UNFRAMED_LEN>,
    /// One frame under construction: header at [0..3), payload after.
    tx: [u8; FRAME_HDR + MAX_PAYLOAD],
    tx_len: usize,
    // Consumer-side loss counters (since the last report).
    tx_frames_dropped: u32,
    unknown_ch_bytes: u32,
    unframed_dropped: u32,
    /// CNTPCT of the last report line, 0 = never.
    last_report: u64,
}

struct StateCell(UnsafeCell<State>);
// SAFETY: single-core EL2; every entry point below runs on the
// trap-handling path (seam calls from MMIO handlers, the trap-tail
// pump), never from the IRQ producer.
unsafe impl Sync for StateCell {}

static STATE: StateCell = StateCell(UnsafeCell::new(State {
    decode: Decode::Sof,
    guest: Ring::new(),
    unframed: Ring::new(),
    tx: [0; FRAME_HDR + MAX_PAYLOAD],
    tx_len: 0,
    tx_frames_dropped: 0,
    unknown_ch_bytes: 0,
    unframed_dropped: 0,
    last_report: 0,
}));

fn with_state<R, F: FnOnce(&mut State) -> R>(f: F) -> R {
    // SAFETY: see StateCell.
    let s = unsafe { &mut *STATE.0.get() };
    f(s)
}

/// Pull the raw ring through the frame decoder into the guest and
/// control rings. Stops (leaving bytes in the raw ring) when the
/// destination ring is full.
fn decode_pending(s: &mut State) {
    while let Some(b) = raw_peek() {
        match s.decode {
            Decode::Sof => {
                if b == SOF {
                    s.decode = Decode::Ch;
                } else if !s.unframed.push(b) {
                    s.unframed_dropped += 1;
                }
            }
            Decode::Ch => s.decode = Decode::Len { ch: b },
            Decode::Len { ch } => {
                s.decode = if b == 0 {
                    Decode::Sof
                } else {
                    Decode::Payload { ch, left: b }
                };
            }
            Decode::Payload { ch, left } => {
                if ch == CH_EXTR {
                    if !s.guest.push(b) {
                        // Back-pressure: leave the byte in the raw ring
                        // until the guest drains its ring.
                        return;
                    }
                } else {
                    s.unknown_ch_bytes += 1;
                }
                s.decode = if left == 1 {
                    Decode::Sof
                } else {
                    Decode::Payload { ch, left: left - 1 }
                };
            }
        }
        raw_advance();
    }
}

fn report_if_due(s: &mut State) {
    let now = cntpct();
    let due = s.last_report == 0
        || now.wrapping_sub(s.last_report) >= REPORT_INTERVAL_S * cntfrq();
    if !due {
        return;
    }
    let raw = RAW_DROPPED.swap(0, Ordering::Relaxed);
    let over = FIFO_OVERRUNS.swap(0, Ordering::Relaxed);
    let (txd, unk, unf) = (s.tx_frames_dropped, s.unknown_ch_bytes, s.unframed_dropped);
    if raw | over | txd | unk | unf == 0 {
        return;
    }
    s.last_report = now;
    s.tx_frames_dropped = 0;
    s.unknown_ch_bytes = 0;
    s.unframed_dropped = 0;
    kprintln!(
        "serial_mux: loss — raw-ring {} B, fifo overruns {}, tx frames {}, \
         unknown-channel {} B, control {} B",
        raw, over, txd, unk, unf,
    );
}

// ---- public surface ---------------------------------------------

/// Bring the RX side up. On real hardware this enables the PL011 RX
/// and receive-timeout interrupts and routes them to the EL2 IRQ
/// entry (`platform::dispatch_uart_rx`); elsewhere the trap-tail pump
/// is the only FIFO drain. Called once from `main.rs` before IRQs
/// are unmasked.
pub fn init() {
    // The overrun flag is sticky since reset and the image upload that
    // preceded us ran the wire at a different rate: start clean.
    console::rx_overrun_take();
    #[cfg(nh_real_hw)]
    {
        console::enable_rx_irq();
        crate::host::platform::enable_bcm2835_irq(crate::host::platform::IRQ_UART);
    }
    kprintln!(
        "serial_mux: guest extr port framed on the console wire (SOF {:#04x}, ch {}); rx {}",
        SOF,
        CH_EXTR,
        if cfg!(nh_real_hw) { "PL011 irq + trap tail" } else { "trap tail poll" },
    );
}

/// Guest TX seam: stage one byte; a full stage flushes as a frame.
pub fn tx(b: u8) {
    with_state(|s| {
        if s.tx_len == MAX_PAYLOAD {
            flush_stage(s);
        }
        s.tx[FRAME_HDR + s.tx_len] = b;
        s.tx_len += 1;
    });
}

/// Emit the staged bytes (if any) as one frame.
fn flush_stage(s: &mut State) {
    if s.tx_len == 0 {
        return;
    }
    s.tx[0] = SOF;
    s.tx[1] = CH_EXTR;
    s.tx[2] = s.tx_len as u8;
    if !console::write_bytes_all_or_nothing(&s.tx[..FRAME_HDR + s.tx_len]) {
        s.tx_frames_dropped += 1;
    }
    s.tx_len = 0;
}

/// Guest RX seam: the next guest-bound byte, if any — held back while
/// the wire is still busy (see [`RX_IDLE_US`]).
pub fn rx_guest() -> Option<u8> {
    with_state(|s| {
        decode_pending(s);
        if s.guest.len() < RX_RELEASE_BYTES {
            let last = LAST_RX_CNTPCT.load(Ordering::Relaxed);
            let idle_ticks = cntpct().wrapping_sub(last);
            if idle_ticks < RX_IDLE_US * cntfrq() / 1_000_000 {
                return None;
            }
        }
        s.guest.pop()
    })
}

/// Next control-channel (unframed) byte, if any. Consumed by
/// `host::serial_pen`'s escape parser; without that feature the
/// channel has no reader and the ring's overflow counter is the only
/// trace of it.
#[cfg_attr(not(feature = "serial-pen-inject"), allow(dead_code))]
pub fn rx_unframed() -> Option<u8> {
    with_state(|s| {
        decode_pending(s);
        s.unframed.pop()
    })
}

/// Trap-tail pump: flush staged TX, top the raw ring up from the FIFO
/// (with IRQs masked — the same producer discipline as the ISR), run
/// the decoder, and report losses.
pub fn pump() {
    with_state(|s| {
        flush_stage(s);
        let daif = mask_irqs();
        drain_fifo();
        unmask_irqs(daif);
        decode_pending(s);
        report_if_due(s);
    });
}

/// Trap-tail composite installed as `HostPumpOps::host_io_pump_input`:
/// the mux pump, then (with `serial-pen-inject`) the injector's pump,
/// then the regular host-io input pump — the same ordering rationale
/// as `serial_pen::pump_and_host_io_input`.
pub fn pump_and_host_io_input() {
    pump();
    #[cfg(feature = "serial-pen-inject")]
    crate::host::serial_pen::pump();
    crate::host::host_io::pump_input();
}

/// PL011 RX / receive-timeout interrupt: drain the FIFO into the raw
/// ring and clear the interrupt. Slim-ISR safe — touches only the
/// producer side of the raw ring and the PL011 (see
/// `arch::slim_isr`).
#[cfg(nh_real_hw)]
pub fn on_rx_irq(_cap: crate::arch::slim_isr::IrqCap) {
    drain_fifo();
    console::clear_rx_irq();
}

fn mask_irqs() -> u64 {
    let daif: u64;
    // SAFETY: sysreg read + write to DAIF, side-effect on IRQ mask.
    unsafe {
        core::arch::asm!(
            "mrs {}, daif",
            "msr daifset, #3",
            out(reg) daif,
            options(nostack, preserves_flags),
        );
    }
    daif
}

fn unmask_irqs(daif: u64) {
    // SAFETY: sysreg write to DAIF, restoring caller-saved state.
    unsafe {
        core::arch::asm!("msr daif, {}", in(reg) daif, options(nostack, preserves_flags));
    }
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
