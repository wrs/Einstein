//! Polled PL011 driver — the bootloader's only I/O besides the SD card.
//!
//! Register map and bring-up sequence mirror the hypervisor's
//! `src/host/console.rs::init` (PL011 at 0x3F20_1000 on the Zero 2 W,
//! routed to GPIO 14/15 by `dtoverlay=disable-bt`; 48 MHz reference
//! clock from the firmware). Unlike the hypervisor there is no DMA
//! path: the bootloader has nothing else to do while it talks.
//!
//! Bit positions are from the PrimeCell PL011 TRM (ARM DDI 0183G
//! §3.3): UARTFR BUSY = bit 3, RXFE = bit 4, TXFF = bit 5.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const UART_BASE: usize = 0x3F20_1000;
const UART_CLOCK_HZ: u32 = 48_000_000;

const UART_DR: *mut u32 = UART_BASE as *mut u32;
const UART_FR: *mut u32 = (UART_BASE + 0x18) as *mut u32;
const UART_IBRD: *mut u32 = (UART_BASE + 0x24) as *mut u32;
const UART_FBRD: *mut u32 = (UART_BASE + 0x28) as *mut u32;
const UART_LCRH: *mut u32 = (UART_BASE + 0x2C) as *mut u32;
const UART_CR: *mut u32 = (UART_BASE + 0x30) as *mut u32;
const UART_IMSC: *mut u32 = (UART_BASE + 0x38) as *mut u32;
const UART_ICR: *mut u32 = (UART_BASE + 0x44) as *mut u32;

const FR_BUSY: u32 = 1 << 3;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const LCRH_FEN: u32 = 1 << 4;
const LCRH_WLEN_8: u32 = 0b11 << 5;
const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

/// The baud the firmware, the hypervisor and the host's console all
/// agree on. The upload protocol switches away from it only for the
/// duration of a transfer.
pub const CONSOLE_BAUD: u32 = 115_200;

/// PL011 baud divisors: integer part `clk / (16·baud)` and the
/// fractional part in 1/64ths (UARTIBRD / UARTFBRD, TRM §3.3.6-7).
/// Same truncating formula as the hypervisor's `console.rs` — the
/// two must agree, since the hypervisor's console output continues
/// on the link this driver set up.
pub const fn divisors(baud: u32) -> (u32, u32) {
    let ibrd = UART_CLOCK_HZ / (16 * baud);
    let scaled = (UART_CLOCK_HZ as u64 * 64) / (16 * baud as u64);
    (ibrd, (scaled - ibrd as u64 * 64) as u32)
}

// Divisor sanity, checked at compile time: the console baud (26 + 2/64
// = 26.03 vs the ideal 26.04, well inside the PL011's tolerance) and
// the two exact transfer rates the 48 MHz clock supports (3 M = clk/16,
// 1.5 M = clk/32). A wrong FBRD formula shows up here, not on the wire.
const _: () = {
    let (i, f) = divisors(115_200);
    assert!(i == 26 && f == 2);
    let (i, f) = divisors(1_500_000);
    assert!(i == 2 && f == 0);
    let (i, f) = divisors(3_000_000);
    assert!(i == 1 && f == 0);
};

/// Bring up the PL011 at `baud`, 8N1, FIFOs on, interrupts masked.
pub fn init(baud: u32) {
    let (ibrd, fbrd) = divisors(baud);
    // SAFETY: MMIO at the documented PL011 base; single core, no
    // concurrent users.
    unsafe {
        write_volatile(UART_CR, 0);
        write_volatile(UART_ICR, 0x7FF);
        write_volatile(UART_IBRD, ibrd);
        write_volatile(UART_FBRD, fbrd);
        // LCRH must be written after the divisors: the PL011 latches
        // IBRD/FBRD on the LCRH write (TRM §3.3.7).
        write_volatile(UART_LCRH, LCRH_FEN | LCRH_WLEN_8);
        write_volatile(UART_IMSC, 0);
        write_volatile(UART_CR, CR_UARTEN | CR_TXE | CR_RXE);
    }
}

/// Wait until the transmit FIFO and shift register are empty. Must
/// precede a baud change or the last bytes go out at the new rate.
pub fn flush() {
    // SAFETY: MMIO read.
    unsafe {
        while read_volatile(UART_FR) & FR_BUSY != 0 {}
    }
}

/// Re-program the baud. Drains TX first; the receive FIFO is left
/// alone (the caller decides what to do with bytes that arrived at
/// the old rate).
pub fn set_baud(baud: u32) {
    flush();
    init(baud);
}

/// Blocking single-byte transmit.
pub fn putc(b: u8) {
    // SAFETY: MMIO at the documented PL011 base.
    unsafe {
        while read_volatile(UART_FR) & FR_TXFF != 0 {}
        write_volatile(UART_DR, b as u32);
    }
}

/// Non-blocking receive: `None` when the RX FIFO is empty. The upper
/// DR bits carry framing/parity/overrun flags; they are dropped here
/// and the protocol's CRCs catch the damage instead.
pub fn getc_nonblock() -> Option<u8> {
    // SAFETY: MMIO at the documented PL011 base.
    unsafe {
        if read_volatile(UART_FR) & FR_RXFE != 0 {
            None
        } else {
            Some((read_volatile(UART_DR) & 0xFF) as u8)
        }
    }
}

pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                putc(b'\r');
            }
            putc(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::uart::Writer, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::uart::Writer, $($arg)*);
    }};
}
