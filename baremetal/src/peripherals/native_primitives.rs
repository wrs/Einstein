//! Newton "native primitives" — MCR-p10 call gateway.
//!
//! Newton OS uses `MCR p10, 0, Rd, ...` as a system call into host-
//! emulated drivers (flash, platform, sound, battery, screen, tablet,
//! serial, etc.). Einstein decodes the same mechanism in
//! `Emulator/TARMProcessor.cpp:340-376` (NativeCoprocRegisterTransfer)
//! and dispatches to `TNativePrimitives::ExecuteNative` which switches
//! on the `inInstruction >> 8` driver ID.
//!
//! The hypervisor enables CPTR_EL2.TFP to trap these to EL2 with
//! EC=0x07; `trap.rs::handle_fp_simd` decodes the instruction, reads
//! the CPU register the MCR names, and calls `execute` below with the
//! "native call code" the guest packed into that register.
//!
//! The handler is *real*: known codes are implemented (starting with
//! a "no-op / null primitive" stub used by the guest test), and every
//! unknown code halts loudly with a full context dump so the next
//! ROM boot that hits one points exactly at the missing table entry.

use crate::{cpu, kprintln, peripherals::{
    battery, flash_driver, host_call, in_translator, network, out_translator,
    platform, printer, screen, serial_driver, sound, tablet,
}, trap::TrapContext};

/// Dispatch a native-primitive call.
///
/// `native_insn` is the value the guest loaded into the MCR's Rd
/// register. `pc` is the guest PC of the MCR instruction itself
/// (for diagnostic output). The high bit of `native_insn` marks
/// Einstein's TVirtualizedCalls patching mechanism — we don't
/// use that path and halt on it.
pub fn execute(ctx: &mut TrapContext, native_insn: u32, pc: u32) {
    if (native_insn & 0x8000_0000) != 0 {
        kprintln!(
            "*** native primitive: virtualized-call path not wired up (bit 31 set in {:#010x} @PC={:#x}), halting",
            native_insn, pc
        );
        cpu::halt();
    }

    let driver = (native_insn >> 8) & 0x00FF_FFFF;
    let subfn = native_insn & 0xFF;

    match (driver, subfn) {
        // Null primitive — the guest test's "is the dispatch path
        // even connected?" probe. Sets r0 = 0 (success) and returns.
        // No equivalent in Einstein's ExecuteNative proper; this is
        // an Einstein-hypervisor-only slot reserved for testing.
        (0x00_0000, 0x00) => {
            ctx.x[0] = 0;
        }

        // Flash-class: driver=0 -> TEinsteinFlashDriver subfn dispatch.
        // Subfn 0x00 is reserved for the null-primitive test above;
        // anything else routes into the flash driver.
        (d, s) if d == flash_driver::DRIVER_ID => {
            flash_driver::handle(ctx, s, pc);
        }

        // Platform-class: driver=1 -> TMainPlatformDriver subfn dispatch.
        // See peripherals/platform.rs.
        (d, s) if d == platform::DRIVER_ID => {
            platform::handle(ctx, s, pc);
        }

        // Sound-class: driver=2 -> PMainSoundDriver subfn dispatch.
        // See peripherals/sound.rs.
        (d, s) if d == sound::DRIVER_ID => {
            sound::handle(ctx, s, pc);
        }

        // Battery-class: driver=3 -> PMainBatteryDriver subfn dispatch.
        // See peripherals/battery.rs.
        (d, s) if d == battery::DRIVER_ID => {
            battery::handle(ctx, s, pc);
        }

        // Screen-class: driver=4 -> TMainDisplayDriver method per
        // subfn. See peripherals/screen.rs.
        (d, s) if d == screen::DRIVER_ID => {
            screen::handle(ctx, s, pc);
        }

        // Tablet-class: driver=5 -> TMainTabletDriver subfn dispatch.
        // See peripherals/tablet.rs.
        (d, s) if d == tablet::DRIVER_ID => {
            tablet::handle(ctx, s, pc);
        }

        // Serial-chip-class: driver=6 -> TSerialChipEinstein subfn dispatch.
        // See peripherals/serial_driver.rs.
        (d, s) if d == serial_driver::DRIVER_ID => {
            serial_driver::handle(ctx, s, pc);
        }

        // In-translator: driver=7. See peripherals/in_translator.rs.
        (d, s) if d == in_translator::DRIVER_ID => {
            in_translator::handle(ctx, s, pc);
        }

        // Out-translator: driver=8. See peripherals/out_translator.rs.
        (d, s) if d == out_translator::DRIVER_ID => {
            out_translator::handle(ctx, s, pc);
        }

        // Host-call: driver=9 -> TEinsteinNativeCalls subfn dispatch.
        // See peripherals/host_call.rs.
        (d, s) if d == host_call::DRIVER_ID => {
            host_call::handle(ctx, s, pc);
        }

        // Network-manager: driver=0xA. See peripherals/network.rs.
        (d, s) if d == network::DRIVER_ID => {
            network::handle(ctx, s, pc);
        }

        // Printer: driver=0xC. See peripherals/printer.rs.
        (d, s) if d == printer::DRIVER_ID => {
            printer::handle(ctx, s, pc);
        }

        _ => {
            kprintln!(
                "*** unknown native primitive {:#010x} (driver={:#x} subfn={:#x}) @PC={:#x}",
                native_insn, driver, subfn, pc
            );
            kprintln!(
                "    r0={:#x} r1={:#x} r2={:#x} r3={:#x}",
                ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            kprintln!(
                "    (extend peripherals/native_primitives.rs::execute to handle it)"
            );
            cpu::halt();
        }
    }
}

