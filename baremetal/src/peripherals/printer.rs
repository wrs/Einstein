//! Printer driver — Rust port of Einstein's `TPrinterManager` native
//! primitive class. Newton OS calls this to drive page rendering;
//! Einstein wraps the entire dispatch in
//! `#if defined(TARGET_UI_FLTK) || TARGET_IOS` and only does anything
//! useful when a host print backend is configured. Without one
//! (the most common case, including all Cocoa builds), every NewtonErr
//! arm returns 0 and every void arm just no-ops.
//!
//! In the hypervisor we have no host print backend, so we mirror that
//! "stubbed-out" path. Unknown subfns halt loud as a trip-wire.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x00000C. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecutePrinterDriverNative`
//! (`Emulator/TNativePrimitives.cpp:3237-3375`).

use crate::arch::trap_context::TrapContext;
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct Printer;

impl NativeDriver for Printer {
    /// Printer-driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_000C;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // PDNew — void. No r0 write.
        0x01 => {}
        // PDDelete — void; Einstein calls mPrinterManager->Delete(), we
        // have no manager so just no-op.
        0x02 => {}
        // PDOpen / PDClose / PDOpenPage / PDClosePage — NewtonErr. With
        // no manager, ret stays at 0; r0 = 0.
        0x03 | 0x04 | 0x05 | 0x06 => {
            ctx.x[0] = 0;
        }
        // PDImageBand(self=r0, band=r1, rect=r2) — NewtonErr; r0 = 0.
        0x07 => {
            ctx.x[0] = 0;
        }
        // PDCancelJob(self=r0, asyncCancel=r1) — void. No r0 write.
        0x08 => {}
        // PDIsProblemResolved — NewtonErr. r0 = 0.
        0x09 => {
            ctx.x[0] = 0;
        }
        // PDGetPageInfo / PDGetBandPrefs — void.
        0x0A | 0x0B => {}
        // PDFaxEndPage(self=r0, pageCount=r1) — NewtonErr. r0 = 0.
        0x0C => {
            ctx.x[0] = 0;
        }
        _ => crate::diag::diag_util::halt_unknown_subfn(
            "printer", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}
