//! Phase 5b real-hardware USB-host probe for the Pi Zero 2 W.
//!
//! Standalone `[[bin]]` — depends on nothing from the hypervisor
//! crate except for `boot.s` and the inline PL011 driver below.
//! Brings up PL011 + the DWC2 OTG controller at `0x3F98_0000`,
//! reads `GSNPSID` to confirm the MMIO window is alive, prints
//! status, then WFE-loops.
//!
//! Once the DWC2 driver in the main crate (`src/usb/host/dwc2/`) is
//! brought up, this binary will fold in the enumeration walk so we
//! can read device + configuration + interface descriptors of
//! whatever's plugged in. For now it's the equivalent of `pi-probe`
//! — first-light "is the controller alive" check.
//!
//! Build:
//!
//! ```sh
//! cargo build --release --bin usb-probe \
//!   --no-default-features --features "pi-bare-metal usb-probe"
//! ```

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::ptr::{read_volatile, write_volatile};

global_asm!(include_str!("boot.s"));

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

const IBRD_VAL: u32 = 26;
const FBRD_VAL: u32 = 3;

fn init_pl011() {
    // SAFETY: MMIO at the documented BCM2710 PL011 base. Called once
    // at startup before any other code touches the UART.
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
    // SAFETY: MMIO; volatile, no aliasing.
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

fn write_hex_u32(v: u32) {
    for i in (0..8).rev() {
        let nib = ((v >> (i * 4)) & 0xF) as u8;
        write_byte(if nib < 10 { b'0' + nib } else { b'a' + nib - 10 });
    }
}

// DWC2 base + GSNPSID offset; expect 0x4F54_xxxx.
const DWC2_BASE: usize = 0x3F98_0000;
const GSNPSID_OFFSET: usize = 0x040;

fn read_gsnpsid() -> u32 {
    let p = (DWC2_BASE + GSNPSID_OFFSET) as *const u32;
    // SAFETY: MMIO read at the documented BCM2710 USB block. Side-
    // effect free.
    unsafe { read_volatile(p) }
}

#[no_mangle]
pub extern "C" fn kmain() -> ! {
    init_pl011();
    write_str("\r\n=== newton usb-probe ===\r\n");

    let id = read_gsnpsid();
    write_str("DWC2 GSNPSID = 0x");
    write_hex_u32(id);
    if (id >> 16) == 0x4F54 {
        write_str("   (OTG core present)\r\n");
    } else {
        write_str("   (UNEXPECTED — check peripheral power / MMIO map)\r\n");
    }

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
