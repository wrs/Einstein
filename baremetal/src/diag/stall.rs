//! EL2 IRQs-masked stretch watermark — attribution for audio-visible
//! EL2 stalls.
//!
//! Serial captures show MAI period-IRQ dispatch gaps of 40–71 ms
//! (vs the 23 ms period): some single stretch of EL2 execution runs
//! that long with IRQs masked. Physical IRQs are only deliverable
//! while the guest executes or inside a `cpu::with_irqs_unmasked`
//! window, so the longest masked stretch IS the worst-case IRQ
//! dispatch latency — and naming the handler that owned it names the
//! stall.
//!
//! Mechanics: every EL2 trap handler entry opens a "stretch" stamped
//! with an attribution tag (sync EC + guest PC, or the IRQ path);
//! handler exit closes it and keeps the longest one seen.
//! `cpu::with_irqs_unmasked` closes the current stretch when it
//! unmasks and opens a fresh `wtail` stretch when it re-masks, so a
//! window's open time (where IRQs are serviced fine, e.g. the
//! hundreds-of-ms flash save) is correctly NOT counted. Nested slim
//! ISRs inside a window measure themselves through the same two entry
//! points.
//!
//! Consumer: `pi_hdmi::on_mai_dma_done` calls [`take_max_us`] each
//! period IRQ, so the recorded maximum always covers "since the
//! previous period-IRQ dispatch", and prints it on the `late period`
//! line. Single-core EL2 and IRQs-masked callers make the plain
//! atomics race-free in practice.

use core::sync::atomic::{AtomicU64, Ordering};

/// Attribution kinds for a stretch, packed into the tag.
pub const KIND_SYNC: u8 = 1;
pub const KIND_IRQ: u8 = 2;
/// Remainder of a handler after a `with_irqs_unmasked` window closed.
pub const KIND_WINDOW_TAIL: u8 = 3;

/// CNTPCT at the start of the current masked stretch; 0 = none open
/// (the guest is running, or a `with_irqs_unmasked` window is open).
static STRETCH_START: AtomicU64 = AtomicU64::new(0);
/// Tag of the current stretch (see `pack_tag`).
static CUR_TAG: AtomicU64 = AtomicU64::new(0);
/// Longest stretch since the last `take_max_us`, in CNTPCT ticks.
static MAX_TICKS: AtomicU64 = AtomicU64::new(0);
/// Tag of that longest stretch.
static MAX_TAG: AtomicU64 = AtomicU64::new(0);

fn pack_tag(kind: u8, ec: u32, pc: u32) -> u64 {
    ((kind as u64) << 40) | (((ec & 0x3f) as u64) << 32) | pc as u64
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

/// Open a masked stretch. Overwrites any stretch already open — the
/// only way that happens is a nested handler entry, whose time the
/// outer stretch would double-count anyway.
pub fn stretch_begin(kind: u8, ec: u32, pc: u32) {
    CUR_TAG.store(pack_tag(kind, ec, pc), Ordering::Relaxed);
    STRETCH_START.store(cntpct().max(1), Ordering::Relaxed);
}

/// Close the current stretch (if one is open) and keep it if it is
/// the longest since the last `take_max_us`.
pub fn stretch_end() {
    let start = STRETCH_START.swap(0, Ordering::Relaxed);
    if start == 0 {
        return;
    }
    let dur = cntpct().wrapping_sub(start);
    if dur > MAX_TICKS.load(Ordering::Relaxed) {
        MAX_TICKS.store(dur, Ordering::Relaxed);
        MAX_TAG.store(CUR_TAG.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// RAII guard for a trap handler's stretch: covers every return path
/// (the IRQ fast path returns early).
pub struct StretchGuard(());

impl Drop for StretchGuard {
    fn drop(&mut self) {
        stretch_end();
    }
}

/// Open a stretch for a trap handler; the guard closes it on any exit.
#[must_use]
pub fn trap_stretch(kind: u8, ec: u32, pc: u32) -> StretchGuard {
    stretch_begin(kind, ec, pc);
    StretchGuard(())
}

/// `cpu::with_irqs_unmasked` is about to unmask: IRQ latency is no
/// longer bounded by this handler, so close its stretch.
pub fn window_open() {
    stretch_end();
}

/// `cpu::with_irqs_unmasked` re-masked: the remainder of the
/// enclosing handler is a fresh masked stretch.
pub fn window_close() {
    stretch_begin(KIND_WINDOW_TAIL, 0, 0);
}

/// Take-and-reset the longest recorded stretch:
/// `(microseconds, kind, ec, pc)`, or `None` if nothing was recorded
/// since the last call.
pub fn take_max_us() -> Option<(u64, u8, u32, u32)> {
    let ticks = MAX_TICKS.swap(0, Ordering::Relaxed);
    if ticks == 0 {
        return None;
    }
    let tag = MAX_TAG.load(Ordering::Relaxed);
    let freq: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
            options(nomem, nostack, preserves_flags));
    }
    let us = ticks.saturating_mul(1_000_000) / freq.max(1);
    Some((
        us,
        ((tag >> 40) & 0xff) as u8,
        ((tag >> 32) & 0x3f) as u32,
        tag as u32,
    ))
}

/// Short label for a stretch kind, for log lines.
pub fn kind_label(kind: u8) -> &'static str {
    match kind {
        KIND_SYNC => "sync",
        KIND_IRQ => "irq",
        KIND_WINDOW_TAIL => "wtail",
        _ => "?",
    }
}
