//! Host-call driver — Rust port of Einstein's `TEinsteinNativeCalls`
//! native primitive class. Newton OS uses this to invoke arbitrary
//! host C functions through libffi; Einstein only enables it on
//! 32-bit non-mac, non-Android, non-Win32 hosts (i.e. 32-bit Linux),
//! and otherwise the entire driver is a no-op (`#if !TARGET_OS_*`
//! gating in `TNativePrimitives.cpp:2518`).
//!
//! In the hypervisor we have no host runtime, so we mirror the
//! "stubbed-out" platform path: every recognised subfn returns r0 = 0
//! without doing anything. Unknown subfns halt loud as a trip-wire,
//! since Einstein's recognised set lists which entry points the
//! Newton kernel actually invokes.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000009. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteHostCallNative`
//! (`Emulator/TNativePrimitives.cpp:2515-2882`).

use crate::arch::trap_context::TrapContext;
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct HostCall;

impl NativeDriver for HostCall {
    /// Host-call driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0009;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // Init/Delete (Einstein doesn't write r0 for these on its
        // 32-bit Linux path; just no-op).
        0x01 | 0x02 => {}

        // OpenLib / CloseLib / PrepareFFIStructure / DisposeFFIStructure
        // / GetErrorMessage. With no host FFI we return r0 = 0 ("success
        // / OK") for all of them.
        0x03 | 0x04 | 0x05 | 0x06 | 0x07 => {
            ctx.x[0] = 0;
        }

        // SetArgValue_uint8 / sint8 / uint16 / sint16 / uint32 / sint32
        // / uint64 / sint64 / float / double / longdouble / string /
        // binary / pointer.
        0x10 | 0x11 | 0x12 | 0x13 | 0x14 | 0x15
        | 0x16 | 0x17 | 0x18 | 0x19 | 0x1A
        | 0x1B | 0x1C | 0x1D => {
            ctx.x[0] = 0;
        }

        // SetResultType / GetOutArgValue_string / GetOutArgValue_binary.
        0x20 | 0x21 | 0x22 => {
            ctx.x[0] = 0;
        }

        // Call_void / Call_int / Call_real / Call_string / Call_pointer.
        0x30 | 0x31 | 0x32 | 0x33 | 0x34 => {
            ctx.x[0] = 0;
        }

        // GetErrno.
        0x40 => {
            ctx.x[0] = 0;
        }

        _ => crate::diag::diag_util::halt_unknown_subfn(
            "host_call", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}
