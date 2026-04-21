//! PL011 UART driver for console output.
//!
//! Targets the BCM2837 PL011 at `0x3F201000`. We perform a minimal init
//! (disable → clear pending → configure 8N1 → enable TX) because neither QEMU
//! nor bare firmware guarantees the UART is in a usable state on entry.
//!
//! The baud divisor assumes the PL011 reference clock is 48 MHz (the default
//! on Pi firmware when `init_uart_clock` is unset, and the value QEMU uses).
//! For 115200 baud: div = 48e6 / (16 * 115200) = 26.042
//!   IBRD = 26, FBRD = round(0.042 * 64) = 3.
//! GPIO 14/15 alt0 selection is also left to firmware / a later init pass;
//! QEMU accepts writes regardless.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

// BCM2837 peripheral base (Pi 3B and Zero 2 W — same SoC).
const MMIO_BASE: usize = 0x3F00_0000;
const PL011_BASE: usize = MMIO_BASE + 0x0020_1000;

const UART_DR: *mut u32 = (PL011_BASE + 0x00) as *mut u32;
const UART_FR: *mut u32 = (PL011_BASE + 0x18) as *mut u32;
const UART_IBRD: *mut u32 = (PL011_BASE + 0x24) as *mut u32;
const UART_FBRD: *mut u32 = (PL011_BASE + 0x28) as *mut u32;
const UART_LCRH: *mut u32 = (PL011_BASE + 0x2C) as *mut u32;
const UART_CR: *mut u32 = (PL011_BASE + 0x30) as *mut u32;
const UART_IMSC: *mut u32 = (PL011_BASE + 0x38) as *mut u32;
const UART_ICR: *mut u32 = (PL011_BASE + 0x44) as *mut u32;

const FR_TXFF: u32 = 1 << 5; // Transmit FIFO full.
const LCRH_FEN: u32 = 1 << 4; // Enable TX/RX FIFOs.
const LCRH_WLEN_8: u32 = 0b11 << 5; // 8-bit word length.
const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

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
        write_volatile(UART_IBRD, 26);
        write_volatile(UART_FBRD, 3);
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
