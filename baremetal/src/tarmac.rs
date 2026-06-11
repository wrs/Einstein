//! FVP TarmacTrace windowing markers (FVP-plugin-specific).
//!
//! FVP's TarmacTrace plugin produces enormous output — every retired
//! instruction plus CP15/events/etc. A full-boot trace is tens of GiB.
//! To keep traces focused on a specific stall we pair the plugin's
//! `bp.pl011_uart0.toggle_mti` UART-token mechanism with two markers
//! the hypervisor emits straight to the mini-UART:
//!
//!   `<<TRM_START>>`  — TarmacTrace begins capturing
//!   `<<TRM_STOP>>`   — TarmacTrace stops
//!
//! The FVP wrapper (`scripts/fvp --tarmac-window=<file>`) configures the
//! UART toggle unit with `start_substr` / `stop_substr` on these exact
//! tokens and disables tracing at boot.
//!
//! Hook points for a windowed trace:
//!   - START: either set `START_AT_TRAP` to a sync-trap count (the
//!     `maybe_emit_start` path, called once per sync trap from
//!     `trap_sync_lower_aarch32`), or call `emit_start()` from the
//!     specific EL2 event you want the window to open on.
//!   - STOP: `emit_stop()` is already wired into the halt paths
//!     (`handle_und`'s unrecognised-UND halt and
//!     `halt_bootloader_canary`); add further `emit_stop()` calls at
//!     any point the window should close.
//!
//! With `START_AT_TRAP = 0` and no explicit `emit_start()` call, no
//! window opens and the markers never fire — the shipped default.
//!
//! Compiled only on the FVP-base platform, where the TarmacTrace
//! plugin exists; on QEMU/real hardware the markers would land in a
//! log no plugin is reading, so the module is gated out entirely.

use crate::uart;

/// If non-zero, emit `<<TRM_START>>` once when the sync-trap counter
/// reaches this value. See module docs.
const START_AT_TRAP: u64 = 0;

const START_MARKER: &str = "<<TRM_START>>";
const STOP_MARKER: &str = "<<TRM_STOP>>";

static mut STARTED: bool = false;
static mut STOPPED: bool = false;

/// Called from `trap_sync_lower_aarch32` once per sync trap. Emits the
/// start marker when `trap_counter` first reaches `START_AT_TRAP`.
pub fn maybe_emit_start(trap_counter: u64) {
    if START_AT_TRAP == 0 {
        return;
    }
    // SAFETY: single-threaded EL2.
    if unsafe { STARTED } {
        return;
    }
    if trap_counter >= START_AT_TRAP {
        unsafe { STARTED = true; }
        emit_marker(START_MARKER);
    }
}

/// Emit the stop marker. Re-emittable after a subsequent `emit_start`.
pub fn emit_stop() {
    // SAFETY: single-threaded EL2.
    if unsafe { STOPPED } {
        return;
    }
    unsafe {
        STOPPED = true;
        STARTED = false;
    }
    emit_marker(STOP_MARKER);
}

/// Emit the start marker on demand. Re-emittable after a subsequent
/// `emit_stop`, so multiple windows in one boot are possible. Use this
/// when the interesting window is triggered by a specific EL2 event
/// rather than a trap count (e.g., "SCTLR.A=1 just became live and I
/// want to trace from here").
#[allow(dead_code)]
pub fn emit_start() {
    // SAFETY: single-threaded EL2.
    if unsafe { STARTED } {
        return;
    }
    unsafe {
        STARTED = true;
        STOPPED = false;
    }
    emit_marker(START_MARKER);
}

fn emit_marker(s: &str) {
    // Write directly to UART as a single line. `kprintln!` would work
    // too, but we want to be sure the marker lands on its own line with
    // a deterministic newline (the UART toggle unit matches per-line,
    // terminating on any control character).
    for &b in s.as_bytes() {
        uart::write_byte(b);
    }
    uart::write_byte(b'\n');
}
