//! Fixed-size SPSC ring of pen samples.
//!
//! Each Einstein `InsertSample` writes two u32 entries: the packed
//! sample word followed by the timestamp in Newton ticks. We mirror
//! that — `enqueue_pen_sample` advances the producer cursor by two
//! slots, `pop` advances the consumer cursor by two slots.
//!
//! Pen-down / pen-up edge markers (`PEN_DOWN_SAMPLE_MARKER`,
//! `PEN_UP_SAMPLE_MARKER`) ride through the same ring as separate
//! pairs; the host backend is responsible for inserting them around
//! the x/y pairs.
//!
//! Single-producer / single-consumer: the producer is the backend's
//! `pump_input` (which runs on the trap-return tail, single-threaded
//! EL2), the consumer is `tablet::handle` subfn 0x16 (same context).
//! We still use atomic cursors so a future split (e.g. an IRQ-time
//! producer) is straightforward.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 512 u32 slots = 256 (sample, ticks) pairs. Matches Einstein's
/// `kTabletBufferSize`.
const QSIZE: usize = 512;

struct PenQueue {
    buf: UnsafeCell<[u32; QSIZE]>,
    p: AtomicUsize,
    c: AtomicUsize,
}
// SAFETY: SPSC with atomic cursors; single producer + single consumer.
unsafe impl Sync for PenQueue {}

static Q: PenQueue = PenQueue {
    buf: UnsafeCell::new([0; QSIZE]),
    p: AtomicUsize::new(0),
    c: AtomicUsize::new(0),
};

/// Push one (sample, ticks) pair onto the queue and raise the tablet
/// IRQ so the guest takes a virtual IRQ on the next ERET. On overflow
/// (more samples queued than `NativeGetSample` has drained) we drop
/// the new pair on the floor — losing a pen sample is preferable to
/// stalling the producer.
pub fn enqueue_pen_sample(sample: u32, ticks: u32) {
    let p = Q.p.load(Ordering::Acquire);
    let c = Q.c.load(Ordering::Acquire);
    // Need 2 free slots. Cursors increment monotonically; capacity is
    // `QSIZE` slots (`QSIZE/2` pairs).
    if p.wrapping_sub(c) >= QSIZE - 1 {
        return;
    }
    let buf = Q.buf.get();
    // SAFETY: SPSC; consumer reads only entries below `p`.
    unsafe {
        (*buf)[p % QSIZE] = sample;
        (*buf)[(p + 1) % QSIZE] = ticks;
    }
    Q.p.store(p.wrapping_add(2), Ordering::Release);
    crate::peripherals::vic::raise(crate::peripherals::vic::INT_TABLET);
}

/// Pop one (sample, ticks) pair. Returns None if the queue is empty.
pub fn pop() -> Option<(u32, u32)> {
    let p = Q.p.load(Ordering::Acquire);
    let c = Q.c.load(Ordering::Acquire);
    if p == c {
        return None;
    }
    let buf = Q.buf.get();
    // SAFETY: SPSC; producer never writes entries below `c`.
    let pair = unsafe {
        let sample = (*buf)[c % QSIZE];
        let ticks = (*buf)[(c + 1) % QSIZE];
        (sample, ticks)
    };
    Q.c.store(c.wrapping_add(2), Ordering::Release);
    Some(pair)
}

/// Clear all pending samples. Called from `host_io::on_resume` after
/// a snapshot restore — pre-snapshot pen events are timing-stale and
/// would surprise the just-restored guest.
pub fn reset() {
    let p = Q.p.load(Ordering::Acquire);
    Q.c.store(p, Ordering::Release);
}
