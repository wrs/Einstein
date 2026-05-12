//! Phase 0 hardware probe for the Pi Zero 2 W.
//!
//! Standalone `[[bin]]` — depends on nothing from the hypervisor crate.
//! Brings up PL011 directly (no semihosting), reads `CurrentEL`,
//! `MIDR_EL1`, `MPIDR_EL1`, prints them, WFE-loops.
//!
//! Goal: confirm in practice what we believe in theory (per
//! `docs/REAL_HW_BRINGUP.md`): the default Pi armstub hands `kernel8.img`
//! off at EL2h on a real Zero 2 W. Output appears on GPIO 14/15 (pins 8
//! and 10) at 115200 8N1, but only when `config.txt` carries
//! `dtoverlay=disable-bt` — otherwise the header pins carry the
//! mini-UART, not the PL011 this binary drives.
//!
//! Reuses `boot.s` via `global_asm!` so `_start` is identical to the
//! main hypervisor. boot.s parks anything that arrives at EL0/EL1
//! silently, so if no output appears the EL gate is the first thing
//! to check (along with serial wiring and the dtoverlay above).

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::ptr::{read_volatile, write_volatile};

global_asm!(include_str!("boot.s"));

// ---- PL011 (UART0) on BCM2710A1, peripheral base 0x3F00_0000 ----------

const UART_BASE: usize = 0x3F20_1000;
const UART_DR: *mut u32 = (UART_BASE + 0x00) as *mut u32;
const UART_FR: *mut u32 = (UART_BASE + 0x18) as *mut u32;
const UART_IBRD: *mut u32 = (UART_BASE + 0x24) as *mut u32;
const UART_FBRD: *mut u32 = (UART_BASE + 0x28) as *mut u32;
const UART_LCRH: *mut u32 = (UART_BASE + 0x2C) as *mut u32;
const UART_CR: *mut u32 = (UART_BASE + 0x30) as *mut u32;
const UART_IMSC: *mut u32 = (UART_BASE + 0x38) as *mut u32;
const UART_ICR: *mut u32 = (UART_BASE + 0x44) as *mut u32;

const FR_TXFF: u32 = 1 << 5;
const LCRH_FEN: u32 = 1 << 4;
const LCRH_WLEN_8: u32 = 0b11 << 5;
const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

// UARTCLK = 48 MHz (Pi firmware default), baud = 115200.
// IBRD = 48_000_000 / (16 * 115_200) = 26.041666...
// IBRD = 26, FBRD = round(0.041666... * 64) = 3.
const IBRD_VAL: u32 = 26;
const FBRD_VAL: u32 = 3;

fn init_pl011() {
    // SAFETY: MMIO at the documented BCM2710 PL011 base. Called once at
    // startup before any other code touches the UART.
    unsafe {
        write_volatile(UART_CR, 0);
        write_volatile(UART_ICR, 0x7FF);
        write_volatile(UART_IBRD, IBRD_VAL);
        write_volatile(UART_FBRD, FBRD_VAL);
        write_volatile(UART_LCRH, LCRH_FEN | LCRH_WLEN_8);
        write_volatile(UART_IMSC, 0);
        write_volatile(UART_CR, CR_UARTEN | CR_TXE | CR_RXE);
    }
}

fn write_byte(b: u8) {
    // SAFETY: MMIO at a fixed, documented address; volatile, no aliasing.
    unsafe {
        while read_volatile(UART_FR) & FR_TXFF != 0 {}
        write_volatile(UART_DR, b as u32);
    }
}

fn write_str(s: &str) {
    for &b in s.as_bytes() {
        write_byte(b);
    }
}

fn write_hex_u64(v: u64) {
    for i in (0..16).rev() {
        let nib = ((v >> (i * 4)) & 0xF) as u8;
        write_byte(if nib < 10 { b'0' + nib } else { b'a' + nib - 10 });
    }
}

fn write_dec_u32(mut v: u32) {
    if v == 0 {
        write_byte(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut n = 0usize;
    while v != 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        write_byte(buf[n]);
    }
}

#[inline(always)]
fn read_current_el() -> u32 {
    let v: u64;
    // SAFETY: CurrentEL is unconditionally readable at EL1+; boot.s only
    // reaches kmain via the EL2 path.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) v, options(nomem, nostack, preserves_flags)) };
    ((v >> 2) & 0x3) as u32
}

#[inline(always)]
fn read_midr_el1() -> u64 {
    let v: u64;
    // SAFETY: MIDR_EL1 is unconditionally readable.
    unsafe { asm!("mrs {}, MIDR_EL1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline(always)]
fn read_mpidr_el1() -> u64 {
    let v: u64;
    // SAFETY: MPIDR_EL1 is unconditionally readable.
    unsafe { asm!("mrs {}, MPIDR_EL1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Entry called from `boot.s` on core 0 after stack and bss are ready.
/// boot.s has already gated on `MPIDR_EL1` (only Aff2|Aff1|Aff0 == 0
/// reaches here) and branched on `CurrentEL` (we land in `.Lat_el2`
/// directly because the Pi armstub already eret'd to EL2h before
/// jumping here).
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    init_pl011();

    write_str("\r\n=== newton pi-probe ===\r\n");

    write_str("CurrentEL = ");
    write_dec_u32(read_current_el());
    write_str("\r\n");

    write_str("MIDR_EL1  = 0x");
    write_hex_u64(read_midr_el1());
    write_str("\r\n");

    write_str("MPIDR_EL1 = 0x");
    write_hex_u64(read_mpidr_el1());
    write_str("\r\n");

    write_str("ok, parking core 0 in WFE\r\n");

    loop {
        // SAFETY: WFE is unprivileged and always safe.
        unsafe { asm!("wfe", options(nomem, nostack, preserves_flags)) };
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        // SAFETY: WFE is unprivileged and always safe.
        unsafe { asm!("wfe", options(nomem, nostack, preserves_flags)) };
    }
}
