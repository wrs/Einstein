//! nhboot — the Pi Zero 2 W bootloader that stands between the
//! firmware and the Newton hypervisor so a new hypervisor image can be
//! delivered over the serial console instead of by moving the SD card.
//!
//! Boot flow (see `docs/REAL_HW_BRINGUP.md`, "Serial image upload"):
//!
//! 1. The firmware loads this binary (`kernel8.img`) at 0x80000 and
//!    `HYPERV.IMG` at [`image::IMAGE_ADDR`] (`initramfs` line in
//!    config.txt), then enters `_start` at EL2 (boot.s).
//! 2. boot.s relocates us to 0x10000000, out of the payload's way.
//! 3. We validate the container and jump to the hypervisor at
//!    0x80000 — or, if the host asks during the handshake window,
//!    receive a new image first (xfer.rs, later phases).
//!
//! The MMU stays off throughout: all RAM is Normal Non-cacheable and
//! MMIO is Device, which is exactly what polled drivers and a one-shot
//! memcpy want. Nothing here uses atomics (LDXR/STXR would be
//! CONSTRAINED UNPREDICTABLE on Non-cacheable memory).

#![no_std]
#![no_main]

mod crc;
mod image;
mod panic;
mod uart;

use image::ImageState;

core::arch::global_asm!(include_str!("boot.s"));

/// Value boot.s stores at `__stack_guard`, "STKGUARD" — same as the
/// hypervisor's `cpu::STACK_GUARD_MAGIC`.
const STACK_GUARD_MAGIC: u64 = 0x5354_4B47_5541_5244;

extern "C" {
    static __stack_guard: u64;
    /// boot.s entry point; its address is the link base (linker.ld).
    static _start: u8;
}

fn stack_guard_intact() -> bool {
    // SAFETY: linker-provided symbol; boot.s wrote it before main.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(__stack_guard)) == STACK_GUARD_MAGIC }
}

fn park() -> ! {
    loop {
        // SAFETY: `wfe` has no operands and no memory effects.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) }
    }
}

fn current_el() -> u64 {
    let el: u64;
    // SAFETY: sysreg read, no side effects. CurrentEL.EL is bits [3:2]
    // (ARM ARM D23.2.32).
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    (el >> 2) & 0b11
}

/// Entered from boot.s at the link address with the stack and bss set
/// up. `dtb` is the firmware's x0; `entered_at` is where the firmware
/// actually loaded us (diagnostic — it should always be 0x80000).
#[no_mangle]
pub extern "C" fn main(dtb: u64, entered_at: u64) -> ! {
    uart::init(uart::CONSOLE_BAUD);
    println!();
    println!(
        "nhboot v1 el={} dtb={:#x} entered_at={:#x} linked_at={:#x}",
        current_el(),
        dtb,
        entered_at,
        core::ptr::addr_of!(_start) as usize,
    );

    // SAFETY: fixed RAM address the firmware filled (or not — then
    // it's whatever was there, and the header check says so).
    let head: &[u8] = unsafe { core::slice::from_raw_parts(image::IMAGE_ADDR as *const u8, 16) };
    print!("image @{:#x}:", image::IMAGE_ADDR);
    for b in head {
        print!(" {:02x}", b);
    }
    println!();

    match image::inspect(image::IMAGE_ADDR) {
        ImageState::Valid { len, crc } => {
            println!("image: valid, {} bytes, crc32 {:08x}; booting @{:#x}", len, crc, image::LOAD_ADDR);
            uart::flush();
            // SAFETY: just validated.
            unsafe { image::boot(image::IMAGE_ADDR, len, dtb) }
        }
        ImageState::BadPayloadCrc { expected, actual } => {
            println!("image: BadPayloadCrc (header says {:08x}, payload is {:08x})", expected, actual);
            println!("nhboot: no bootable image — upload protocol not implemented yet (phase 2)");
            park()
        }
        other => {
            println!("image: {:?}", other);
            println!("nhboot: no bootable image — upload protocol not implemented yet (phase 2)");
            park()
        }
    }
}
