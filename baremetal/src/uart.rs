//! Console output paths.
//!
//! `kprintln!` / `dprintln!` route through the semihosting host stdout
//! (Arm Semihosting `SYS_OPEN(":tt")` + `SYS_WRITE`, HLT `#0xF000`).
//! This frees the PL011 MMIO for the guest's external serial port
//! ("extr") wireup — see `peripherals/serial.rs`.
//!
//! The PL011 itself is still brought up and exposed through
//! `write_byte` for callers that must hit the real wire:
//!
//!   * `tarmac::emit_marker` — the FVP TarmacTrace plugin's UART-token
//!     window-gating watches PL011 byte stream for `<<TRM_START>>` /
//!     `<<TRM_STOP>>`. Semihosting bytes aren't visible to the
//!     plugin.
//!   * `GuestTestPrintByte` HVC (guest-test self-checks).
//!
//! PL011 address/clock come from `crate::platform`: raspi3b uses
//! 0x3F20_1000 @ 48 MHz, FVP_Base_RevC uses 0x1C09_0000 @ 14.7456 MHz.
//! 8N1 @ 115200, TX+RX, FIFO on. Both `cargo run` (QEMU
//! `-serial mon:stdio`) and `scripts/fvp` (`bp.pl011_uart0.out_file=-`)
//! deliver PL011 bytes to the host process stdio, the same destination
//! semihosting writes land on, so a single `> /tmp/run` capture sees
//! both streams interleaved.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};
#[cfg(not(feature = "no-semihost"))]
use core::sync::atomic::Ordering;

use crate::platform::{UART_BASE, UART_CLOCK_HZ};

// ---- PL011 (raw wire) --------------------------------------------------

const UART_DR: *mut u32 = (UART_BASE + 0x00) as *mut u32;
const UART_FR: *mut u32 = (UART_BASE + 0x18) as *mut u32;
const UART_IBRD: *mut u32 = (UART_BASE + 0x24) as *mut u32;
const UART_FBRD: *mut u32 = (UART_BASE + 0x28) as *mut u32;
const UART_LCRH: *mut u32 = (UART_BASE + 0x2C) as *mut u32;
const UART_CR: *mut u32 = (UART_BASE + 0x30) as *mut u32;
const UART_IMSC: *mut u32 = (UART_BASE + 0x38) as *mut u32;
const UART_ICR: *mut u32 = (UART_BASE + 0x44) as *mut u32;

const FR_RXFE: u32 = 1 << 4; // Receive FIFO empty.
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

/// Bring up PL011 for 115200 8N1, TX+RX, FIFO on, and open the
/// semihosting host stdout handle. Called exactly once from
/// [`crate::kmain`] on core 0 before any other code that produces
/// console output.
///
/// With `no-semihost` enabled (real-silicon builds), the semihosting
/// stdout open is skipped and `kprintln!` routes through the PL011 wire
/// instead — see `write_str` below.
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
    #[cfg(not(feature = "no-semihost"))]
    sh::open_stdout();
}

/// Write a single byte to the PL011, busy-waiting until the TX FIFO has
/// room. Reserved for callers that must produce bytes on the actual
/// wire — `tarmac.rs::emit_marker` and the `GuestTestPrintByte` HVC.
/// Console output (`kprintln!`/`dprintln!`) goes through semihosting
/// via `Writer` instead; routing it here would defeat the purpose of
/// freeing PL011 for the guest's serial chip.
pub fn write_byte(b: u8) {
    // SAFETY: MMIO at a fixed, documented address. Volatile access, no aliasing.
    unsafe {
        while read_volatile(UART_FR) & FR_TXFF != 0 {}
        write_volatile(UART_DR, b as u32);
    }
}

/// Non-blocking host-PL011 RX. Returns `Some(byte)` if the receive
/// FIFO has data, `None` otherwise. Used by `peripherals::dma` to
/// stream incoming bytes into the guest's external-serial DMA buffer.
///
/// FR.RXFE bit position confirmed against Linux's
/// `include/linux/amba/serial.h` (UART01x_FR_RXFE = `BIT(4)`),
/// matching the PrimeCell PL011 TRM (ARM DDI 0183G §3.3.3).
pub fn read_byte_nonblock() -> Option<u8> {
    // SAFETY: MMIO at a fixed, documented address. Volatile access, no aliasing.
    unsafe {
        if read_volatile(UART_FR) & FR_RXFE != 0 {
            None
        } else {
            // DR low 8 bits = data; upper bits are error flags we ignore
            // for the host-console use case.
            Some((read_volatile(UART_DR) & 0xFF) as u8)
        }
    }
}

// ---- semihosting host stdout ------------------------------------------
//
// This whole block is compiled out under `no-semihost` (real-silicon
// builds). `write_str` then routes through `write_byte` over PL011.

#[cfg(not(feature = "no-semihost"))]
mod sh {
    use core::sync::atomic::{AtomicI64, Ordering};

    const SYS_OPEN: u64 = 0x01;
    const SYS_WRITE: u64 = 0x05;
    const SYS_WRITEC: u64 = 0x03;

    /// Arm Semihosting SYS_OPEN mode 4 = "w" (write, text).
    const MODE_WRITE_TEXT: u64 = 0x04;

    /// `:tt` opens the host's stdout (per Arm Semihosting §5.3.1.2).
    static STDOUT_PATH: &[u8] = b":tt\0";

    /// File handle returned by `SYS_OPEN(":tt", "w")`. `-1` sentinel
    /// means "not opened yet" — `write_str` falls back to per-byte
    /// SYS_WRITEC in that case (covers any kprintln issued before
    /// `uart::init()`).
    pub(super) static STDOUT_FH: AtomicI64 = AtomicI64::new(-1);

    /// Open `:tt` once, stash the handle for the rest of the run.
    pub(super) fn open_stdout() {
        let args: [u64; 3] = [
            STDOUT_PATH.as_ptr() as u64,
            MODE_WRITE_TEXT,
            (STDOUT_PATH.len() - 1) as u64,
        ];
        let h = unsafe { semihost(SYS_OPEN, args.as_ptr()) };
        if h >= 0 {
            STDOUT_FH.store(h, Ordering::Release);
        }
    }

    /// Execute one semihosting call. `op` is the SYS_* subfunction ID;
    /// `arg` points at the argument block (layout per op, see Arm
    /// Semihosting §5.3). Returns the value placed in x0 by the host.
    ///
    /// SAFETY: HLT #0xF000 is the AArch64 semihosting trap. QEMU's and
    /// the FVP AEM model's handlers intercept it and return to EL2
    /// without disturbing register state beyond x0.
    unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
        let result: u64;
        unsafe {
            core::arch::asm!(
                "hlt #0xF000",
                inout("x0") op => result,
                in("x1") arg as u64,
                options(nostack, preserves_flags),
            );
        }
        result as i64
    }

    /// Push a byte buffer to the host stdout via SYS_WRITE. Short
    /// writes are silently ignored — we'd have nowhere to surface the
    /// error anyway, and dropping a few bytes of console output is a
    /// better failure mode than recursing back into the same write
    /// path.
    pub(super) fn write_bytes(fh: i64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let args: [u64; 3] = [fh as u64, data.as_ptr() as u64, data.len() as u64];
        let _ = unsafe { semihost(SYS_WRITE, args.as_ptr()) };
    }

    /// Per-byte SYS_WRITEC fallback for kprintlns issued before
    /// `init()` completes (i.e., before STDOUT_FH is set). One HLT per
    /// character — slow but bounded to the handful of `print_banner`
    /// / `print_caps` lines that run before `uart::init`.
    pub(super) fn writec(b: u8) {
        let byte: u8 = b;
        let ptr = &byte as *const u8 as u64;
        let args = [ptr];
        let _ = unsafe { semihost(SYS_WRITEC, args.as_ptr()) };
    }
}

/// Write a string to the console.
///
/// Default build: routes through Arm Semihosting `SYS_WRITE` to `:tt`,
/// keeping PL011 free for the guest's external-serial chip emulation.
///
/// `no-semihost` build (real Pi silicon): routes through `write_byte`
/// over PL011 directly. The guest's external-serial chip is not yet
/// hooked up on real hw, so the wire is ours. Bytes emitted before
/// `init()` are silently dropped (UARTEN=0).
pub fn write_str(s: &str) {
    #[cfg(feature = "no-semihost")]
    {
        for &b in s.as_bytes() {
            write_byte(b);
        }
        return;
    }
    #[cfg(not(feature = "no-semihost"))]
    {
        let fh = sh::STDOUT_FH.load(Ordering::Acquire);
        if fh >= 0 {
            sh::write_bytes(fh, s.as_bytes());
        } else {
            for &b in s.as_bytes() {
                sh::writec(b);
            }
        }
    }
}

/// Writer implementing [`core::fmt::Write`] so callers can `write!` formatted
/// output. Routes through semihosting (`SYS_WRITE` to `:tt`).
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
