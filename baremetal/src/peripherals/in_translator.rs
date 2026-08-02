//! In-translator driver — Rust port of Einstein's `PEinsteinInTranslator`
//! native primitive class.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000007. Einstein's
//! `TNativePrimitives::ExecuteInTranslatorNative`
//! (`Emulator/TNativePrimitives.cpp:2475-2489`) has no specific case
//! arms — it returns 0 for every subfn. We enumerate the valid PInTranslator
//! protocol method indices (from `Drivers/PInTranslator.h` /
//! `PEinsteinInTranslator.impl.h`) so a future ROM call to an unmodelled
//! opcode points exactly at the missing entry, then return r0 = 0 for
//! the ones the protocol defines — matching Einstein's "no real protocol
//! modelled" stance when no host translator backend is configured.

use crate::arch::trap_context::TrapContext;
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct InTranslator;

impl NativeDriver for InTranslator {
    /// In-translator driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0007;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // PInTranslator protocol method indices: New, Delete, Init,
        // Idle, FrameAvailable, ProduceFrame.
        0x00 | 0x01 | 0x02 | 0x03 | 0x04 | 0x05 => {
            ctx.x[0] = 0;
        }
        _ => crate::diag::diag_util::halt_unknown_subfn(
            "in_translator", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}
