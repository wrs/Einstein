//! PL011 UART driver for console output.
//!
//! Address and reference clock come from `crate::platform` — the same
//! PL011 IP block sits at different PAs on raspi3b (0x3F20_1000, 48 MHz)
//! and FVP_Base_RevC (0x1C09_0000, 14.7456 MHz). We perform a minimal init
//! (disable → clear pending → configure 8N1 → enable TX) because neither the
//! QEMU raspi3b model nor the FVP model nor real firmware guarantees the
//! UART is in a usable state on entry.
//!
//! Baud divisor for 115200 8N1 is computed from the platform's UART
//! reference clock at build time.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

use crate::platform::{UART_BASE, UART_CLOCK_HZ};

const UART_DR: *mut u32 = (UART_BASE + 0x00) as *mut u32;
const UART_FR: *mut u32 = (UART_BASE + 0x18) as *mut u32;
const UART_IBRD: *mut u32 = (UART_BASE + 0x24) as *mut u32;
const UART_FBRD: *mut u32 = (UART_BASE + 0x28) as *mut u32;
const UART_LCRH: *mut u32 = (UART_BASE + 0x2C) as *mut u32;
const UART_CR: *mut u32 = (UART_BASE + 0x30) as *mut u32;
const UART_IMSC: *mut u32 = (UART_BASE + 0x38) as *mut u32;
const UART_ICR: *mut u32 = (UART_BASE + 0x44) as *mut u32;

const FR_TXFF: u32 = 1 << 5; // Transmit FIFO full.
const LCRH_FEN: u32 = 1 << 4; // Enable TX/RX FIFOs.
const LCRH_WLEN_8: u32 = 0b11 << 5; // 8-bit word length.
const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

const BAUD: u32 = 115_200;
const IBRD_VAL: u32 = UART_CLOCK_HZ / (16 * BAUD);
// Fractional part of clock / (16 * baud) in 1/64-ths.
const FBRD_VAL: u32 = {
    let scaled = (UART_CLOCK_HZ as u64 * 64) / (16 * BAUD as u64);
    (scaled - (IBRD_VAL as u64) * 64) as u32
};

/// Initialise the PL011 for 115200 8N1, TX+RX, FIFO on.
///
/// Called exactly once from [`crate::kmain`] on core 0 before any other code
/// that might write to the UART.
pub fn init() {
    // SAFETY: MMIO at fixed, documented addresses; called once at startup
    // before other cores are running any hypervisor code.
    unsafe {
        write_volatile(UART_CR, 0); // Disable entirely while we reconfigure.
        write_volatile(UART_ICR, 0x7FF); // Clear all pending interrupts.
        write_volatile(UART_IBRD, IBRD_VAL);
        write_volatile(UART_FBRD, FBRD_VAL);
        write_volatile(UART_LCRH, LCRH_FEN | LCRH_WLEN_8);
        write_volatile(UART_IMSC, 0); // Mask all interrupts for now.
        write_volatile(UART_CR, CR_UARTEN | CR_TXE | CR_RXE);
    }
}

/// Write a single byte, busy-waiting until the TX FIFO has room.
pub fn write_byte(b: u8) {
    // SAFETY: MMIO at a fixed, documented address. Volatile access, no aliasing.
    unsafe {
        while read_volatile(UART_FR) & FR_TXFF != 0 {}
        write_volatile(UART_DR, b as u32);
    }
}

pub fn write_str(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

/// Writer implementing [`core::fmt::Write`] so callers can `write!` formatted
/// output.
pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Convenience macros for formatted output. Use like `kprintln!("val={:#x}", x);`.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::uart::Writer, $($arg)*);
    }};
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n"); };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::uart::Writer, $($arg)*);
    }};
}

/// Debug-log variant of `kprintln!` for recurring diagnostic messages
/// that dominate the console during phase-B bring-up (e.g., per-trap
/// ELR logs, stage-1 walk summaries, SCTLR writes). Expands to the
/// regular `kprintln!` by default and to a no-op when the `quiet`
/// feature is enabled.
#[cfg(not(feature = "quiet"))]
#[macro_export]
macro_rules! dprintln {
    () => { $crate::kprintln!(); };
    ($($arg:tt)*) => { $crate::kprintln!($($arg)*); };
}

#[cfg(feature = "quiet")]
#[macro_export]
macro_rules! dprintln {
    () => {};
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}
