//! Null audio backend — no host audio output. Compiled in when
//! `audio-null` is the active feature (the default), so QEMU/FVP
//! builds compile cleanly and the Newton kernel's sound code is
//! exercised end-to-end without any host plumbing.
//!
//! There is no host audio device, but the Newton kernel still drives
//! the full output state machine and *waits* for a sound-DMA
//! completion interrupt after each scheduled buffer. This backend
//! supplies that completion, mirroring Einstein's `TNullSoundManager`
//! (`Emulator/Sound/TNullSoundManager.cpp`):
//!
//!   - `set_interrupt_mask(in, out)` stores `mOutputIntMask` — the VIC
//!     bit(s) the completion raises.
//!   - `start_output()` sets `mOutputIsRunning = true` and raises the
//!     output interrupt.
//!   - `schedule_output(_, size)` with `size == 0` stops the run;
//!     otherwise, while running, raises the output interrupt.
//!   - `stop_output()` clears the run flag.
//!   - `output_is_running()` returns `false` (Einstein's quirk: the
//!     kernel queries this only to decide whether to keep pumping, and
//!     the null manager always answers "no").
//!
//! Einstein raises the completion synchronously inside the same call.
//! We instead *arm* the completion and let it fire from the audio
//! tick (`audio::tick`, driven by the timer IRQ in `trap_irq`) once
//! the buffer's playback duration has elapsed — Newton produces BE
//! S16 mono @ 22.05 kHz, so a `byte_count`-byte buffer represents
//! `byte_count / 2 / 22050` seconds of audio. Pacing to real buffer
//! duration keeps the kernel's notion of elapsed playback time
//! roughly honest (a synchronous raise would let the chime "play" in
//! zero wall time), while still completing promptly — sub-second for
//! the boot chime's buffers — instead of via the old parked-PC wedge
//! probe's ~1 s latency.

use crate::peripherals::vic;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Newton output sample rate (BE S16 mono). See `audio::mod`'s
/// driver contract.
const NEWTON_RATE_HZ: u64 = 22_050;

/// VIC interrupt bit(s) raised on buffer completion, set by subfn
/// 0x1F (`set_interrupt_mask`). Zero until the kernel installs it.
static OUTPUT_INT_MASK: AtomicU32 = AtomicU32::new(0);

/// `mOutputIsRunning` — `start_output` sets it, `stop_output` and a
/// zero-size `schedule_output` clear it.
static OUTPUT_RUNNING: AtomicBool = AtomicBool::new(false);

/// Number of completion edges owed to the kernel. Einstein raises one
/// edge per `ScheduleOutputBuffer` call, so back-to-back schedules
/// (the double-buffer fill pattern: schedule buf0, schedule buf1, wait)
/// owe two edges — a single deadline slot would coalesce them and lose
/// a completion the kernel waits on.
static PENDING_EDGES: AtomicU32 = AtomicU32::new(0);

/// CNTPCT_EL0 deadline at which the next owed completion fires. Only
/// meaningful while `PENDING_EDGES > 0`. Set by `start_output` /
/// `schedule_output`, consumed and re-armed by `tick`.
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(0);

/// Duration (in CNTPCT ticks) of the most recently scheduled buffer,
/// used to pace the second of two outstanding edges when `tick` fires
/// the first. Double-buffered playback uses equal-sized buffers, so
/// reusing the last duration is exact in practice; if sizes ever
/// differ the edge is still delivered, just paced by the later
/// buffer's length.
static LAST_DURATION_TICKS: AtomicU64 = AtomicU64::new(0);

fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: CNTPCT_EL0 is a side-effect-free counter read.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: CNTFRQ_EL0 read, side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

/// Arm one completion edge `duration_samples`/22050 seconds after the
/// previously armed edge (or after now, if none is outstanding). A
/// duration of zero arms for the next tick (a buffer with no samples
/// still owes the kernel one completion edge). Each call owes exactly
/// one edge, matching Einstein's raise-per-ScheduleOutputBuffer.
fn arm_completion(duration_samples: u64) {
    let freq = cntfrq();
    let delay_ticks = (duration_samples.saturating_mul(freq)) / NEWTON_RATE_HZ;
    LAST_DURATION_TICKS.store(delay_ticks, Ordering::Relaxed);
    if PENDING_EDGES.fetch_add(1, Ordering::Relaxed) == 0 {
        NEXT_DEADLINE.store(cntpct().wrapping_add(delay_ticks), Ordering::Relaxed);
    }
    // else: the edge queues behind the outstanding one; `tick` re-arms
    // NEXT_DEADLINE from LAST_DURATION_TICKS when it fires the front.
}

/// Drop all owed edges (stop / zero-size schedule).
fn disarm_completions() {
    PENDING_EDGES.store(0, Ordering::Relaxed);
}

pub fn init() {}

pub fn set_interrupt_mask(_input_mask: u32, output_mask: u32) {
    OUTPUT_INT_MASK.store(output_mask, Ordering::Relaxed);
}

pub fn set_output_buffers(_buf1_addr: u32, _buf2_addr: u32) {}

/// Subfn 0x07. `byte_count == 0` ends the output run (matching
/// `TNullSoundManager::ScheduleOutputBuffer`); otherwise, while
/// running, arm a completion paced to the buffer's playback duration.
pub fn schedule_output(_which: u32, byte_count: u32) {
    if byte_count == 0 {
        OUTPUT_RUNNING.store(false, Ordering::Relaxed);
        disarm_completions();
        return;
    }
    if OUTPUT_RUNNING.load(Ordering::Relaxed) {
        // BE S16 mono: 2 bytes per sample.
        arm_completion((byte_count / 2) as u64);
    }
}

/// Subfn 0x0D. `TNullSoundManager::StartOutput` sets running and
/// raises the output interrupt immediately; we arm the completion for
/// the next tick so the kernel sees a prompt first edge.
pub fn start_output() {
    OUTPUT_RUNNING.store(true, Ordering::Relaxed);
    arm_completion(0);
}

/// Subfn 0x0F.
pub fn stop_output() {
    OUTPUT_RUNNING.store(false, Ordering::Relaxed);
    disarm_completions();
}

/// Subfn 0x13. Einstein's `TNullSoundManager::OutputIsRunning` always
/// returns `false`.
pub fn output_is_running() -> bool {
    false
}

pub fn output_volume_set(_volume: u32) {}

pub fn output_volume_get() -> u32 {
    0
}

/// Audio tick, driven from the timer IRQ via `audio::tick`. When the
/// front owed completion's deadline has elapsed, raise the stored
/// output interrupt mask through the VIC — the same bit(s) the kernel
/// waits on after scheduling a buffer — and re-arm for the next owed
/// edge, if any. This is the null backend's sole completion path; it
/// replaces the former wedge probe in `trap.rs`.
pub fn tick() {
    if PENDING_EDGES.load(Ordering::Relaxed) == 0 {
        return;
    }
    let now = cntpct();
    if now < NEXT_DEADLINE.load(Ordering::Relaxed) {
        return;
    }
    let remaining = PENDING_EDGES.fetch_sub(1, Ordering::Relaxed) - 1;
    if remaining > 0 {
        // The next owed buffer starts playing when this one finishes.
        let dur = LAST_DURATION_TICKS.load(Ordering::Relaxed);
        NEXT_DEADLINE.store(now.wrapping_add(dur), Ordering::Relaxed);
    }
    let mask = OUTPUT_INT_MASK.load(Ordering::Relaxed);
    if mask != 0 {
        vic::raise(mask);
    }
}
