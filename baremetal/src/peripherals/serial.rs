//! Newton serial-port MMIO model.
//!
//! Four TSerialChipVoyager instances live back-to-back at
//! `TMemoryConsts::kExternalSerialBase` onwards — one 64 KiB window
//! per port (external, infrared, built-in, modem). The register set
//! is documented in `Emulator/Serial/TBasicSerialPortManager.cpp:95`
//! (and `docs/peripherals.md`).
//!
//! For boot bring-up we need three things to Just Work so the ROM's
//! polling loops terminate:
//!   * Reading the status register (`+0x4400`) reports TX empty and
//!     RX empty — no bytes to consume, no FIFO to drain.
//!   * Reading the RX byte (`+0x7000`) returns 0, matching "no data".
//!   * Writes to the TX byte (`+0x6000`), control registers, and the
//!     interrupt-mask registers are consumed. We log TX bytes (to a
//!     budget) so diagnostic dumps show up on the console.
//!
//! Unknown offsets inside a window halt loudly — the trip-wire for
//! cases Phase A's scope missed, per baremetal/PLAN.md. Addresses
//! outside the four windows never reach this module (mmio.rs
//! dispatches elsewhere).

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::kprintln;

/// Base of the external-serial port (TMemoryConsts::kExternalSerialBase).
pub const EXTERNAL_BASE: u64 = 0x0F1C_0000;
/// End of the modem window, exclusive.
pub const SERIAL_END: u64 = 0x0F20_0000;

/// Size of one port's register window.
const PORT_STRIDE: u64 = 0x0001_0000;

/// Register offsets within a port window. From Einstein's
/// `TBasicSerialPortManager.cpp:108-177` and `TSerialChipVoyager`
/// inlining in the Newton 2.x ROM at `0x001D6B70` (TxBufEmpty) and
/// `0x001D7A5C` (AllSent).
mod reg {
    // Control / interrupt-enable block — writes consumed silently.
    pub const CTRL_4400_STATUS: u64 = 0x4400; // read-only, returns ready bits
    pub const CTRL_4800_RX_ERR: u64 = 0x4800; // read-only, returns 0

    // Data FIFO shims.
    pub const TX_BYTE: u64 = 0x6000;
    pub const RX_BYTE: u64 = 0x7000;

    // Bit positions in the +0x4400 status register.
    pub const STATUS_TX_EMPTY: u32 = 1 << 7;
    // bit 6: RX FIFO full, bit 5: RX byte available, bit 4: DCD, bit 3: CTS
    // — all left clear (idle line, no data waiting).

    /// Control / interrupt-enable register offsets the kernel touches
    /// during `BasicBusControlRegInit` and the per-port
    /// `TVoyagerSerialPort` setup. The Newton 2.x kernel
    /// initialises these by writing fixed bit patterns and may also
    /// read them back (either as part of init, or implicitly via the
    /// BE-8 byte-write splice path, which read-modify-writes the
    /// surrounding word). We treat reads as "register holds zero"
    /// (idle peripheral) and writes as no-ops; the hypervisor doesn't
    /// deliver serial interrupts yet so the actual bit state isn't
    /// observable past this layer.
    pub const CONTROL_IE_OFFSETS: &[u64] = &[
        0x0000, 0x0400, 0x0800, 0x0C00, 0x1000, 0x2000, 0x2400, 0x2800,
        0x3000, 0x3400, 0x3800, 0x3C00, 0x5000, 0x5400, 0x5800, 0x5C00,
        0x8000,
    ];
}

/// True iff `ipa` lands inside one of the four port windows.
pub fn owns(ipa: u64) -> bool {
    (EXTERNAL_BASE..SERIAL_END).contains(&ipa)
}

/// Identify the port (0..=3) and register offset (0..0xFFFF) for an
/// address inside the serial window. Caller must have already
/// verified `owns(ipa)`.
fn split(ipa: u64) -> (u8, u64) {
    let rel = ipa - EXTERNAL_BASE;
    let port = (rel / PORT_STRIDE) as u8;
    let off = rel % PORT_STRIDE;
    (port, off)
}

fn port_name(port: u8) -> &'static str {
    match port {
        0 => "extr",
        1 => "infr",
        2 => "tblt",
        3 => "mdem",
        _ => "?",
    }
}

pub fn read(ipa: u64) -> u32 {
    let (port, off) = split(ipa);
    match off {
        // Status: TX FIFO empty, RX FIFO empty, no handshake lines. A
        // polling "wait for TX ready" loop reading bit 7 here clears
        // immediately; "wait for RX byte" reading bit 5 never clears.
        reg::CTRL_4400_STATUS => reg::STATUS_TX_EMPTY,

        // RX error status — no errors ever.
        reg::CTRL_4800_RX_ERR => 0,

        // No pending byte — a read of the RX FIFO returns 0 and
        // leaves the (empty) RX FIFO empty.
        reg::RX_BYTE => 0,

        _ if reg::CONTROL_IE_OFFSETS.contains(&off) => 0,

        _ => halt_unknown(port, off, /*write=*/ false, 0),
    }
}

pub fn write(ipa: u64, value: u32) {
    let (port, off) = split(ipa);
    match off {
        reg::TX_BYTE => log_tx_byte(port, value as u8),

        _ if reg::CONTROL_IE_OFFSETS.contains(&off) => {}

        _ => halt_unknown(port, off, /*write=*/ true, value),
    }
}

// ---- diagnostics --------------------------------------------------

static TX_BUDGETS: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
const TX_LOG_MAX: usize = 64;

fn log_tx_byte(port: u8, byte: u8) {
    if port >= 4 {
        return;
    }
    let n = TX_BUDGETS[port as usize].fetch_add(1, Ordering::Relaxed);
    if n < TX_LOG_MAX {
        kprintln!("serial[{}]: TX {:#04x} ({})",
            port_name(port), byte,
            if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            },
        );
    }
}

fn halt_unknown(port: u8, off: u64, write: bool, value: u32) -> ! {
    kprintln!();
    kprintln!("*** serial[{}] UNKNOWN {} +{:#06x} val={:#010x} halted ***",
        port_name(port),
        if write { "W" } else { "R" },
        off, value);
    kprintln!(
        "  (extend peripherals/serial.rs to model this register — see"
    );
    kprintln!(
        "   Emulator/Serial/TVoyagerSerialPort.cpp for the register layout.)"
    );
    crate::cpu::halt();
}
