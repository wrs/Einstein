//! Serial chip driver — Rust port of Einstein's `TSerialChipEinstein`
//! native primitive class. Models a "no host port attached" state for
//! all four Voyager serial ports (extr / infr / tblt / mdem) — every
//! TSerialHostPort method short-circuits to r0 = 0 in Einstein when
//! the port is null (TNativePrimitives.cpp:2134), so we mirror that
//! here without modeling a real host port at all.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000006. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteSerialDriverNative`
//! (`Emulator/TNativePrimitives.cpp:2036-2467`).
//!
//! Subfns < 0x30 are deprecated Voyager-chip ops — Einstein returns
//! -10000 ("kSerErrTimeout"-ish) for those; we match.
//!
//! 0x35 PutByte routes the byte (r1) through `kprintln!` with a budgeted
//! log so guest serial output is visible without flooding the UART.

use crate::{kprintln, trap_context::TrapContext};
use crate::peripherals::native_primitives::NativeDriver;

/// Marker for the [`NativeDriver`] dispatch in
/// `peripherals/native_primitives.rs`.
pub struct SerialDriver;

impl NativeDriver for SerialDriver {
    /// Serial-chip-driver class ID in the native-primitive encoding.
    const DRIVER_ID: u32 = 0x00_0006;
    fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
        handle(ctx, subfn, pc)
    }
}

/// `kSerErrTimeout` / generic Voyager-deprecated error per Einstein
/// (TNativePrimitives.cpp:2045).
const ERR_VOYAGER_DEPRECATED: u32 = (-10000i32) as u32;

fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    // < 0x30 is the deprecated Voyager-chip dispatch path.
    if subfn < 0x30 {
        ctx.x[0] = ERR_VOYAGER_DEPRECATED as u64;
        return;
    }

    match subfn {
        // Process / InitByOption — Einstein parses an option struct
        // ('eloc' / 'sers') and writes back chip-side fields. Without a
        // real serial host we just return 0 (init succeeds with no
        // observable side effect). The chip-side struct fields the
        // Newton kernel will read back stay at whatever the kernel
        // initialised them to.
        0x4C | 0x4D => {
            ctx.x[0] = 0;
        }

        // PutByte(byte=r1) — Einstein forwards to TSerialHostPort::PutByte.
        // We log the byte through kprintln (budgeted) so the guest's
        // serial output is visible during tests.
        0x35 => {
            let byte = (ctx.x[1] as u32) & 0xFF;
            log_putbyte(byte as u8);
            ctx.x[0] = 0;
        }

        // All other TSerialChipEinstein primitives — Einstein forwards
        // each to a TSerialHostPort method, and the early
        // "if (port == NULL) return r=0;" short-circuits everything to
        // r0 = 0 when no port is bound. We have no port, ever, so all
        // of these collapse to r0 = 0.
        //
        //   0x33 InstallChipHandler         0x34 RemoveChipHandler
        //   0x36 ResetTxBEmpty              0x37 GetByte
        //   0x38 TxBufEmpty                 0x39 RxBufFull
        //   0x3A GetRxErrorStatus           0x3B GetSerialStatus
        //   0x3C ResetSerialStatus          0x3D SetSerialOutputs
        //   0x3E ClearSerialOutputs         0x3F GetSerialOutputs
        //   0x40 PowerOff                   0x41 PowerOn
        //   0x42 PowerIsOn                  0x43 SetInterruptEnable
        //   0x44 Reset                      0x45 SetBreak
        //   0x46 SetSpeed                   0x47 SetIOParms
        //   0x48 Reconfigure                0x49 Init
        //   0x4A CardRemoved                0x4B GetFeatures
        //   0x4E SetSerialMode              0x4F SysEventNotify
        //   0x50 SetTxDTransceiverEnable    0x51 GetByteAndStatus
        //   0x52 SetIntSourceEnable         0x53 AllSent
        //   0x54 ConfigureForOutput         0x55 InitTxDMA
        //   0x56 InitRxDMA                  0x57 TxDMAControl
        //   0x58 RxDMAControl               0x59 SetSDLCAddress
        //   0x5A ReEnableReceiver           0x5B LinkIsFree
        //   0x5C SendControlPacket          0x5D WaitForPacket
        //   0x5E WaitForAllSent
        0x33 | 0x34 | 0x36 | 0x37 | 0x38 | 0x39
        | 0x3A | 0x3B | 0x3C | 0x3D | 0x3E | 0x3F
        | 0x40 | 0x41 | 0x42 | 0x43 | 0x44 | 0x45 | 0x46 | 0x47 | 0x48 | 0x49 | 0x4A | 0x4B
        | 0x4E | 0x4F
        | 0x50 | 0x51 | 0x52 | 0x53 | 0x54 | 0x55 | 0x56 | 0x57 | 0x58 | 0x59 | 0x5A | 0x5B | 0x5C | 0x5D | 0x5E => {
            ctx.x[0] = 0;
        }

        _ => crate::diag_util::halt_unknown_subfn(
            "serial_driver", subfn, pc,
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
        ),
    }
}

/// Print bytes the guest writes through serial PutByte. Budget-limited
/// so a chatty guest doesn't drown the UART log; after BUDGET hits we
/// drop bytes silently.
fn log_putbyte(byte: u8) {
    static BUDGET: crate::diag_util::LogBudget = crate::diag_util::LogBudget::new(256);
    if BUDGET.allow() {
        if byte.is_ascii() && (byte == b'\n' || byte == b'\r' || (byte >= 0x20 && byte < 0x7F)) {
            kprintln!("serial.PutByte: {:?}", byte as char);
        } else {
            kprintln!("serial.PutByte: {:#04x}", byte);
        }
    }
}
