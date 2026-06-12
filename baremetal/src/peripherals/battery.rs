//! Battery driver — Rust port of Einstein's `PMainBatteryDriver` native
//! primitive class. Newton OS doesn't depend on real ADC readings to
//! boot; Einstein returns a fixed synthetic battery struct and r0 = 0
//! for everything, and so do we.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000003. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteBatteryDriverNative`
//! (`Emulator/TNativePrimitives.cpp:1406-1558`).
//!
//! Status (0x07) and RawStatus (0x08) write a 13-word "PowerPlantStatus"
//! struct at the address in r2 (NOT r1 — Einstein reads from r2). The
//! values mirror Einstein's tables verbatim so the kernel's downstream
//! consumers see the same battery state we'd see in Einstein.

use crate::trap_context::TrapContext;
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct Battery;

impl NativeDriver for Battery {
    /// Battery-driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0003;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // New — no r0 write per Einstein.
        0x01 => {}
        // Delete, Init, WakeUp, ShutDown, Count — r0 = 0.
        // (Einstein returns 0 for Count too — see TNativePrimitives.cpp:1454.)
        0x02 | 0x03 | 0x04 | 0x05 | 0x06 => {
            ctx.x[0] = 0;
        }
        // Status — fill 13-word struct at *r2 with Einstein's "running on
        // battery" values, r0 = 0.
        0x07 => fill_status(ctx, pc, &STATUS_FIELDS),
        // RawStatus — same shape, raw-voltage variant.
        0x08 => fill_status(ctx, pc, &RAW_STATUS_FIELDS),
        // StartSleepCharge, SetType, ReadADCVoltage, ConvertVoltage —
        // r0 = 0 per Einstein.
        0x09 | 0x0A | 0x0B | 0x0C => {
            ctx.x[0] = 0;
        }
        _ => crate::diag_util::halt_unknown_subfn(
            "battery", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}

/// PowerPlantStatus values for subfn 0x07 Status. Field comments mirror
/// `TNativePrimitives.cpp:1474-1489`.
const STATUS_FIELDS: [(u32, u32); 13] = [
    (0x00, 0x00000003), // mBatteryType
    (0x04, 0x000587C0), // mVoltage1
    (0x08, 0x00000064), // mBatteryLevel
    (0x0C, 0x00000014), // mBatteryAlert
    (0x10, 0x00000000), // unknown
    (0x14, 0x006CF999), // mVoltage6
    (0x18, 0x00000000), // mAdapterPlugged
    (0x1C, 0x00003F36), // mVoltage7
    (0x20, 0x00000000), // unknown
    (0x24, 0xFFFFFFFF), // unknown
    (0x28, 0xFFFFFFFF), // mUnknownDIOPins33Related
    (0x2C, 0x001A2F28), // mVoltage4
    (0x30, 0x001A8D79), // mVoltage5
];

/// PowerPlantStatus values for subfn 0x08 RawStatus. Mirrors
/// `TNativePrimitives.cpp:1499-1512`.
const RAW_STATUS_FIELDS: [(u32, u32); 13] = [
    (0x00, 0x00000003),
    (0x04, 0x0C97D000),
    (0x08, 0x00000064),
    (0x0C, 0x00000014),
    (0x10, 0x00000000),
    (0x14, 0x00E19000),
    (0x18, 0x00000000),
    (0x1C, 0x005C0000),
    (0x20, 0x00000000),
    (0x24, 0xFFFFFFFF),
    (0x28, 0xFFFFFFFF),
    (0x2C, 0x086E2000),
    (0x30, 0x07D3B000),
];

fn fill_status(ctx: &mut TrapContext, pc: u32, fields: &[(u32, u32)]) {
    let base = ctx.x[2] as u32;
    for &(off, val) in fields {
        crate::peripherals::guest_access::write_word_or_halt(
            base.wrapping_add(off), val, "battery.fill_status", pc);
    }
    ctx.x[0] = 0;
}
