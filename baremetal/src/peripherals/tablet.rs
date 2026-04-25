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
use crate::{cpu, guest_mem, kprintln, trap::TrapContext};

/// Tablet-driver class ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_0005;

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

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
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
                if !write_guest_word(base.wrapping_add(off), val) {
                    halt_io("GetTabletCalibration", base.wrapping_add(off), pc);
                }
            }
        }
        // SetTabletCalibration(in=r1) — mirror Einstein: read +0x00/+0x04/
        // +0x08/+0x10/+0x10 (the second read clobbers cal_0c with the same
        // value). No r0 write per Einstein.
        0x0A => {
            let base = ctx.x[1] as u32;
            s.cal_00 = read_guest_word_or_halt(base.wrapping_add(0x00), pc);
            s.cal_04 = read_guest_word_or_halt(base.wrapping_add(0x04), pc);
            s.cal_08 = read_guest_word_or_halt(base.wrapping_add(0x08), pc);
            s.cal_0c = read_guest_word_or_halt(base.wrapping_add(0x10), pc);
            s.cal_0c = read_guest_word_or_halt(base.wrapping_add(0x10), pc);
        }
        // SetDoingCalibration — r0 = 0.
        0x0B => {
            ctx.x[0] = 0;
        }
        // GetTabletResolution — write 0x03200000 to *r1 and *r2.
        // No r0 write per Einstein.
        0x0C => {
            for which in [ctx.x[1] as u32, ctx.x[2] as u32] {
                if !write_guest_word(which, 0x0320_0000) {
                    halt_io("GetTabletResolution", which, pc);
                }
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
        // NativeGetSample — r0 = 0 (no sample available; we don't write
        // to *r1 / *r2 in that case).
        0x16 => {
            ctx.x[0] = 0;
        }
        _ => {
            kprintln!(
                "*** tablet: unknown subfn {:#x} @PC={:#x} r1={:#x} r2={:#x} r3={:#x}",
                subfn, pc, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            kprintln!(
                "    (extend peripherals/tablet.rs::handle to add this subfn)"
            );
            cpu::halt();
        }
    }
}

fn write_guest_word(addr: u32, value: u32) -> bool {
    if guest_mem::write_word_va(addr, value) {
        return true;
    }
    guest_mem::write_word_pa(addr, value)
}

fn read_guest_word_or_halt(addr: u32, pc: u32) -> u32 {
    if let Some(v) = guest_mem::read_word_va(addr) {
        return v;
    }
    if let Some(v) = guest_mem::read_word_pa(addr) {
        return v;
    }
    kprintln!("*** tablet: cannot read at {:#x} @PC={:#x}", addr, pc);
    cpu::halt();
}

fn halt_io(what: &str, addr: u32, pc: u32) -> ! {
    kprintln!("*** tablet.{}: cannot write at {:#x} @PC={:#x}", what, addr, pc);
    cpu::halt();
}
