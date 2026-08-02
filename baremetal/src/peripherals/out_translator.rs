//! Out-translator driver — Rust port of Einstein's `PEinsteinOutTranslator`
//! native primitive class.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000008. Einstein's
//! `TNativePrimitives::ExecuteOutTranslatorNative`
//! (`Emulator/TNativePrimitives.cpp:2495-2509`) has no specific case
//! arms — it returns 0 for every subfn. We enumerate the valid POutTranslator
//! protocol method indices (from `Drivers/POutTranslator.h` /
//! `PEinsteinOutTranslator.impl.h`) so a future ROM call to an unmodelled
//! opcode points exactly at the missing entry, then return r0 = 0 for the
//! ones the protocol defines.

use crate::arch::trap_context::TrapContext;
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct OutTranslator;

impl NativeDriver for OutTranslator {
    /// Out-translator driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0008;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // POutTranslator protocol method indices: New, Delete, Init,
        // Idle, ConsumeFrame, Flush, Prompt, Print, Putc, EnterBreakLoop,
        // ExitBreakLoop, StackTrace, ExceptionNotify.
        0x00 | 0x01 | 0x02 | 0x03 | 0x04 | 0x05 | 0x06
        | 0x07 | 0x08 | 0x09 | 0x0A | 0x0B | 0x0C => {
            ctx.x[0] = 0;
        }
        _ => crate::diag::diag_util::halt_unknown_subfn(
            "out_translator", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}
