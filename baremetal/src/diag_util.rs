//! Diagnostic-logging utilities shared across the trap / emulation paths.
//!
//! `LogBudget` (and the two-tier `TwoTierLog`) use atomics and are safe
//! to touch from any context. `SeenSet`/`TopK` are `static mut` and rely
//! on the single-core-EL2 invariant below.
//!
//! # Single-core-EL2 safety (read once, applies to every type here)
//!
//! These structures are stored in `static mut` and mutated without any
//! lock or atomic. That is sound **only** because every trap, IRQ, and
//! emulation path in this hypervisor runs on core 0 with interrupts
//! masked for the duration of the handler: there is never a second
//! context concurrently touching a `static mut SeenSet`/`TopK`. The one
//! window where EL2 code runs with IRQs unmasked is `pause_system`'s WFI
//! loop (see `cpu::with_irqs_unmasked`), and none of these diagnostic
//! statics are touched from the slim ISR that runs there. Keep that
//! invariant: if a future change makes any of these reachable from the
//! slim-ISR path, they must move to atomics.

use core::sync::atomic::{AtomicUsize, Ordering};

/// First-N-unique tracker: a fixed-capacity set that answers "is this
/// the first time I've seen `key`?" Used to dedup one-shot diagnostic
/// log lines (per-PC, per-(FAR,mode,…) tuple, per-CP15-op key, …) so a
/// tight guest loop doesn't flood the console.
///
/// Capacity `N` is fixed; once full, every subsequent unseen key reports
/// "not first" (i.e. is silently dropped) — matching the hand-rolled
/// blocks this replaces. `T` is the key type (a `u32` PC, a packed op
/// key, or a small tuple of `u32`s).
pub struct SeenSet<T: Copy + PartialEq, const N: usize> {
    keys: [T; N],
    len: usize,
}

impl<T: Copy + PartialEq, const N: usize> SeenSet<T, N> {
    pub const fn new(fill: T) -> Self {
        Self { keys: [fill; N], len: 0 }
    }

    /// Returns `true` exactly once for each distinct `key` (until the set
    /// fills). Records `key` on the first call. After capacity is
    /// reached, an unseen key returns `false` and is not recorded.
    pub fn first_time(&mut self, key: T) -> bool {
        if self.contains(key) {
            return false;
        }
        self.insert(key)
    }

    /// Non-mutating membership test. Used where the check and the insert
    /// are separate operations (e.g. a known-rejected cache: probe on
    /// entry, insert only on a fresh rejection).
    pub fn contains(&self, key: T) -> bool {
        for i in 0..self.len {
            if self.keys[i] == key {
                return true;
            }
        }
        false
    }

    /// Record `key`. Returns `true` if it was inserted, `false` if the
    /// set is already full (the key is dropped). Does not de-dup — call
    /// `contains` first if you need that.
    pub fn insert(&mut self, key: T) -> bool {
        if self.len < N {
            self.keys[self.len] = key;
            self.len += 1;
            true
        } else {
            false
        }
    }
}

/// Misra-Gries top-K frequency tracker over `u32` keys with `u64`
/// counts. Replaces the duplicated `TopK` (trap_hist) and `RejTopK`
/// (unaligned_inline). `record` is O(N); `snapshot_sorted` returns the
/// tracked (key, count) pairs in descending count order; `reset` clears
/// the window.
pub struct TopK<const N: usize> {
    keys: [u32; N],
    counts: [u64; N],
}

impl<const N: usize> TopK<N> {
    pub const fn new() -> Self {
        Self { keys: [0; N], counts: [0; N] }
    }

    pub fn record(&mut self, key: u32) {
        for i in 0..N {
            if self.counts[i] > 0 && self.keys[i] == key {
                self.counts[i] = self.counts[i].saturating_add(1);
                return;
            }
        }
        for i in 0..N {
            if self.counts[i] == 0 {
                self.keys[i] = key;
                self.counts[i] = 1;
                return;
            }
        }
        for c in &mut self.counts {
            *c = c.saturating_sub(1);
        }
    }

    /// Snapshot the tracked entries in descending count order. Only
    /// compiled where a dump site consumes it.
    #[cfg(feature = "log_traps")]
    pub fn snapshot_sorted(&self) -> [(u32, u64); N] {
        let mut out = [(0u32, 0u64); N];
        for i in 0..N {
            out[i] = (self.keys[i], self.counts[i]);
        }
        for k in 0..N {
            let mut best = k;
            for j in (k + 1)..N {
                if out[j].1 > out[best].1 {
                    best = j;
                }
            }
            out.swap(k, best);
        }
        out
    }

    #[cfg(feature = "log_traps")]
    pub fn reset(&mut self) {
        for i in 0..N {
            self.keys[i] = 0;
            self.counts[i] = 0;
        }
    }
}

/// Atomic log budget: lets the first `max` calls log, then goes silent.
/// Replaces the hand-rolled `static AtomicU32/Usize` + `fetch_add` +
/// `if n < MAX` patterns scattered across the peripherals. Safe to share
/// from any context (it's purely atomic).
///
/// Use `allow()` for a flat first-`max` budget, or `allow_or_every(p)`
/// for "first `max`, then 1-in-`p`" so a long-running signal keeps a
/// faint heartbeat without flooding.
pub struct LogBudget {
    count: AtomicUsize,
    max: usize,
}

impl LogBudget {
    pub const fn new(max: usize) -> Self {
        Self { count: AtomicUsize::new(0), max }
    }

    /// Returns `true` for the first `max` calls, `false` after.
    pub fn allow(&self) -> bool {
        self.count.fetch_add(1, Ordering::Relaxed) < self.max
    }

    /// Returns `true` for the first `max` calls, then once every
    /// `every` calls. `every == 0` disables the periodic tail (flat
    /// budget, identical to `allow`).
    pub fn allow_or_every(&self, every: usize) -> bool {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        n < self.max || (every != 0 && n % every == 0)
    }
}

/// Two-tier log budget with the expected-stub vs unknown-input split
/// from periph-M4 built in: routine/expected traffic gets a tight
/// budget so it can't flood, while genuinely-unknown inputs get their
/// own (generous) budget so discovery never goes silent just because
/// the expected stream filled the shared quota.
pub struct TwoTierLog {
    expected: LogBudget,
    unknown: LogBudget,
}

impl TwoTierLog {
    pub const fn new(expected_max: usize, unknown_max: usize) -> Self {
        Self {
            expected: LogBudget::new(expected_max),
            unknown: LogBudget::new(unknown_max),
        }
    }

    /// Gate for routine/expected stub traffic (known registers,
    /// unmodelled-but-harmless offsets).
    pub fn expected(&self) -> bool {
        self.expected.allow()
    }

    /// Gate for genuinely-unknown / out-of-range inputs that the model
    /// doesn't recognise — kept on a separate, larger budget.
    pub fn unknown(&self) -> bool {
        self.unknown.allow()
    }
}

/// Loud halt for an unrecognised native-primitive sub-function.
///
/// Every native driver (`peripherals/native_primitives.rs` dispatches to
/// flash_driver, platform, sound, …) routes its `_ =>` arm here so the
/// "unknown subfn" trip-wire prints a uniform, fully-actionable context
/// dump: the driver/file name, the sub-function, the guest PC, the
/// argument registers, and the exact file to extend. `file` is the
/// driver's source file stem (e.g. `"battery"`), used both as the
/// message label and in the "extend peripherals/<file>.rs::handle" hint.
///
/// r0..r3 are printed unconditionally — the superset of what the
/// per-driver copies this replaces used to print, so no argument is ever
/// dropped from a halt (the prior copies variously printed r1..r3,
/// r0..r2, or r1..r2).
pub fn halt_unknown_subfn(
    file: &'static str,
    subfn: u32,
    pc: u32,
    r0: u32,
    r1: u32,
    r2: u32,
    r3: u32,
) -> ! {
    crate::kprintln!();
    crate::kprintln!(
        "*** {}: unknown subfn {:#x} @PC={:#x} r0={:#x} r1={:#x} r2={:#x} r3={:#x}",
        file, subfn, pc, r0, r1, r2, r3
    );
    crate::kprintln!(
        "    (extend peripherals/{}.rs::handle to add this subfn)",
        file
    );
    crate::cpu::halt();
}
