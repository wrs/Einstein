#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::arch::global_asm;

mod cpu;
mod panic;
pub mod uart;

global_asm!(include_str!("boot.s"));

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point called from `boot.s` on core 0 after stack and bss are ready.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    uart::init();
    print_banner();
    kprintln!("Halted on core 0. Cores 1-3 parked in WFE.");
    kprintln!("Connect gdb via `target remote :1234` when running with `-s -S`.");
    cpu::halt();
}

fn print_banner() {
    kprintln!();
    kprintln!("===============================================");
    kprintln!(" Newton Hypervisor v{}  (baremetal, M0)", VERSION);
    kprintln!(" Target: Cortex-A53 / BCM2837 (Pi 3B, Zero 2 W)");
    kprintln!("===============================================");
    kprintln!("Current EL: {}", cpu::current_el());
    kprintln!("Core ID:    {}", cpu::core_id());
}
