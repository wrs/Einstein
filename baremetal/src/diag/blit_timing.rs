//! Per-blit wall-clock accumulators for the guest→panel video path.
//!
//! Two independently attributable layers (see PLAN.md item 10 and the
//! video-path plan): `screen::blit` (the backend-independent
//! *emulation* cost — page walks, byte copies, payload assembly) and
//! the active host-io backend's `push_blit` (the *paint* cost —
//! scaling, panel writes, cache maintenance). Each layer records its
//! own [`BlitTimer`]; every [`REPORT_EVERY`] recorded blits the timer
//! prints one `dprintln!` window summary (count / total µs / max µs)
//! and resets, so an optimisation pass on either layer shows up as a
//! change in exactly one line.
//!
//! Time source is `host::console::now_us()` — the same CNTPCT_EL0 /
//! CNTFRQ_EL0 pair `diag::stall` scales by. Single-core EL2 callers
//! make the plain relaxed atomics race-free in practice (same
//! argument as `stall.rs`).

use core::sync::atomic::{AtomicU64, Ordering};

/// Blits per report window. Small enough that a one-shot animation
/// produces at least one line — measured on hardware, an Extras
/// drawer open/close is a handful of large blits and a full cold
/// boot stays under 64 — while steady-state clock-tick redraws
/// (~1/min) still take minutes to fill a window.
const REPORT_EVERY: u64 = 16;

/// One layer's accumulator. Window-scoped: `record_since` resets all
/// three counters after printing the report line.
pub struct BlitTimer {
    label: &'static str,
    count: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
}

impl BlitTimer {
    const fn new(label: &'static str) -> Self {
        Self {
            label,
            count: AtomicU64::new(0),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    /// Record one blit whose start time (from [`begin`]) was `t0_us`.
    pub fn record_since(&self, t0_us: u64) {
        let dur = crate::host::console::now_us().wrapping_sub(t0_us);
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total_us.fetch_add(dur, Ordering::Relaxed) + dur;
        if dur > self.max_us.load(Ordering::Relaxed) {
            self.max_us.store(dur, Ordering::Relaxed);
        }
        if n >= REPORT_EVERY {
            let max = self.max_us.swap(0, Ordering::Relaxed);
            self.count.store(0, Ordering::Relaxed);
            self.total_us.store(0, Ordering::Relaxed);
            crate::dprintln!(
                "blit_timing {}: n={} total={}us avg={}us max={}us",
                self.label,
                n,
                total,
                total / n,
                max,
            );
        }
    }
}

/// `peripherals::screen::blit` emulation cost — function entry up to
/// (excluding) the push into the host-io sink.
pub static EMULATE: BlitTimer = BlitTimer::new("screen.blit");
/// Active host-io backend `push_blit` paint cost.
pub static PAINT: BlitTimer = BlitTimer::new("push_blit");

/// Start-of-measurement timestamp in µs. Pair with
/// [`BlitTimer::record_since`].
#[inline(always)]
pub fn begin() -> u64 {
    crate::host::console::now_us()
}
