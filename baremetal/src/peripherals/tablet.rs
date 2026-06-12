//! Tablet driver — Rust port of Einstein's `TMainTabletDriver` native
//! primitive class. Maintains calibration matrix + sample rate as
//! module-static state; everything else is no-op stubs that mirror
//! Einstein's behaviour when no real digitizer is attached.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000005. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteTabletDriverNative`
//! (`Emulator/TNativePrimitives.cpp:1773-2030`).
//!
//! Note: Einstein's Get/SetTabletCalibration uses an unusual offset
//! pattern — it writes/reads `fUnknown_00`, `_04`, `_08`, then
//! `_0C` at offset `+0x10` (twice), and never touches +0x0C. That looks
//! like a bug in Einstein but mirroring it keeps round-trips lossless
//! when the kernel sets and reads back its own calibration.

use core::cell::UnsafeCell;
use crate::{kprintln, peripherals::guest_access, trap_context::TrapContext};
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct Tablet;

impl NativeDriver for Tablet {
    /// Tablet-driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0005;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

/// "Not implemented" error code Einstein returns for finger-input
/// state queries (TNativePrimitives.cpp:1947).
const ERR_NOT_IMPL: u32 = (-56008i32) as u32;

#[derive(Default)]
struct TabletState {
    cal_00: u32,
    cal_04: u32,
    cal_08: u32,
    cal_0c: u32,
    cal_10: u32,
    sample_rate: u32,
}

struct TabletCell(UnsafeCell<TabletState>);
// SAFETY: accessed only from the single EL2 trap handler on core 0.
unsafe impl Sync for TabletCell {}

static TABLET: TabletCell = TabletCell(UnsafeCell::new(TabletState {
    cal_00: 0,
    cal_04: 0,
    cal_08: 0,
    cal_0c: 0,
    cal_10: 0,
    sample_rate: 0,
}));

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *TABLET.0.get() };
    match subfn {
        // New — no r0 write per Einstein.
        0x01 => {}
        // Delete, WakeUp, ShutDown, TabletIdle — r0 = 0.
        0x02 | 0x04 | 0x05 | 0x06 => {
            ctx.x[0] = 0;
        }
        // Init — reset calibration + sample rate to Einstein's defaults
        // (TNativePrimitives.cpp:1798-1803). No r0 write per Einstein.
        0x03 => {
            s.cal_00 = 0xFFFFDFA5;
            s.cal_04 = 0x000015EC;
            s.cal_08 = 0x01F5F6B0;
            s.cal_0c = 0xFFEE8314;
            s.cal_10 = 0xC8E60000;
            s.sample_rate = 0x0000B400;
        }
        // GetSampleRate.
        0x07 => {
            ctx.x[0] = s.sample_rate as u64;
        }
        // SetSampleRate(rate=r1).
        0x08 => {
            s.sample_rate = ctx.x[1] as u32;
            ctx.x[0] = 0;
        }
        // GetTabletCalibration(out=r1) — see header comment for Einstein's
        // unusual offset pattern. No r0 write per Einstein.
        0x09 => {
            let base = ctx.x[1] as u32;
            for (off, val) in [
                (0x00u32, s.cal_00),
                (0x04, s.cal_04),
                (0x08, s.cal_08),
                (0x10, s.cal_0c),
                (0x10, s.cal_0c), // Einstein writes 0x10 twice.
            ] {
                guest_access::write_word_or_halt(
                    base.wrapping_add(off), val, "tablet.GetTabletCalibration", pc);
            }
        }
        // SetTabletCalibration(in=r1) — mirror Einstein: read +0x00/+0x04/
        // +0x08/+0x10/+0x10 (the second read clobbers cal_0c with the same
        // value). No r0 write per Einstein.
        0x0A => {
            let base = ctx.x[1] as u32;
            s.cal_00 = guest_access::read_word_or_halt(base.wrapping_add(0x00), "tablet.SetTabletCalibration", pc);
            s.cal_04 = guest_access::read_word_or_halt(base.wrapping_add(0x04), "tablet.SetTabletCalibration", pc);
            s.cal_08 = guest_access::read_word_or_halt(base.wrapping_add(0x08), "tablet.SetTabletCalibration", pc);
            s.cal_0c = guest_access::read_word_or_halt(base.wrapping_add(0x10), "tablet.SetTabletCalibration", pc);
            s.cal_0c = guest_access::read_word_or_halt(base.wrapping_add(0x10), "tablet.SetTabletCalibration", pc);
        }
        // SetDoingCalibration — r0 = 0.
        0x0B => {
            ctx.x[0] = 0;
        }
        // GetTabletResolution — write 0x03200000 to *r1 and *r2.
        // No r0 write per Einstein.
        0x0C => {
            for which in [ctx.x[1] as u32, ctx.x[2] as u32] {
                guest_access::write_word_or_halt(
                    which, 0x0320_0000, "tablet.GetTabletResolution", pc);
            }
        }
        // TabSetOrientation — r0 = 0.
        0x0D => {
            ctx.x[0] = 0;
        }
        // GetTabletState — r0 = 0 (no pen state).
        0x0E => {
            ctx.x[0] = 0;
        }
        // GetFingerInputState / SetFingerInputState — r0 = -56008.
        0x0F | 0x10 => {
            ctx.x[0] = ERR_NOT_IMPL as u64;
        }
        // RecalibrateTabletAfterRotate, TabletNeedsRecalibration,
        // StartBypassTablet, StopBypassTablet, ReturnTabletToConsciousness
        // — r0 = 0.
        0x11 | 0x12 | 0x13 | 0x14 | 0x15 => {
            ctx.x[0] = 0;
        }
        // NativeGetSample — drain one sample from the host-IO pen
        // queue. Per Einstein TNativePrimitives.cpp:2012-2015:
        //   r0 = 1 (got sample) → *r1 = packed sample word,
        //                         *r2 = sample time in Newton ticks.
        //   r0 = 0 (queue empty) — leave *r1 / *r2 alone.
        0x16 => {
            // Budget-limited entry log — first 8 calls only,
            // unconditionally (the budget self-throttles so this
            // doesn't flood). Tells us Newton's tablet ISR
            // responded to INT_TABLET at all. After the first 8
            // entries, opt into log_irqs for full visibility.
            {
                use core::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    kprintln!("tablet.NativeGetSample call #{} @PC={:#x}", n, pc);
                } else {
                    crate::log_irqs!(
                        "tablet.NativeGetSample call #{} @PC={:#x}",
                        n, pc
                    );
                }
            }
            match crate::host_io::pop_pen_sample() {
                Some((sample, ticks)) => {
                    let r1 = ctx.x[1] as u32;
                    let r2 = ctx.x[2] as u32;
                    guest_access::write_word_or_halt(r1, sample, "tablet.NativeGetSample.sample", pc);
                    guest_access::write_word_or_halt(r2, ticks, "tablet.NativeGetSample.ticks", pc);
                    ctx.x[0] = 1;
                    use core::sync::atomic::{AtomicUsize, Ordering};
                    static N: AtomicUsize = AtomicUsize::new(0);
                    let n = N.fetch_add(1, Ordering::Relaxed);
                    if n < 16 {
                        kprintln!(
                            "tablet: returned sample={:#010x} ticks={:#x}",
                            sample, ticks
                        );
                    } else {
                        crate::log_irqs!(
                            "tablet: returned sample={:#010x} ticks={:#x}",
                            sample, ticks
                        );
                    }
                }
                None => {
                    ctx.x[0] = 0;
                }
            }
        }
        _ => crate::diag_util::halt_unknown_subfn(
            "tablet", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}

