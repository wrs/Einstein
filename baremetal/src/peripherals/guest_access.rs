//! Shared guest-memory accessors for the native-primitive peripherals.
//!
//! Every native-prim peripheral that reads or writes a guest-side
//! structure (flash_driver, platform, battery, tablet, screen, network)
//! needs the same VA-first / PA-fallback access with a loud halt on
//! failure: Einstein completes these paths, so a failed guest read/write
//! is a hypervisor emulation bug — not a guest bug — and must stop the
//! boot with a context dump rather than silently corrupting state.
//!
//! VA-first / PA-fallback: in MMU-on mode (Newton boot) the guest hands
//! us VAs, resolved through the live stage-1 walk. In MMU-off mode
//! (guest tests, where `translate_va` returns `None`) the same address
//! is treated as a PA. `guest_endian::guest_read/write_u32_va` already
//! encode that fallback, and we additionally retry the raw PA form so a
//! caller that passes a PA directly still succeeds.

use crate::{cpu, guest_endian, guest_mem, kprintln};

/// Read a 32-bit guest word (VA-first, PA-fallback). Halts loudly with
/// `what` and `pc` in the message if neither access resolves.
pub fn read_word_or_halt(addr: u32, what: &str, pc: u32) -> u32 {
    if let Some(v) = guest_endian::guest_read_u32_va(addr) {
        return v;
    }
    if let Some(v) = guest_endian::guest_read_u32_pa(addr) {
        return v;
    }
    kprintln!("*** {}: cannot read word at {:#x} @PC={:#x}", what, addr, pc);
    cpu::halt();
}

/// Write a 32-bit guest word (VA-first, PA-fallback). Halts loudly with
/// `what` and `pc` in the message if neither access resolves.
pub fn write_word_or_halt(addr: u32, value: u32, what: &str, pc: u32) {
    if guest_endian::guest_write_u32_va(addr, value) {
        return;
    }
    if guest_endian::guest_write_u32_pa(addr, value) {
        return;
    }
    kprintln!("*** {}: cannot write word at {:#x} @PC={:#x}", what, addr, pc);
    cpu::halt();
}

/// Read a single guest byte: translate the VA (identity if the MMU is
/// off) and read the PA. Halts loudly with `what` and `pc` if the read
/// faults.
pub fn read_byte_or_halt(addr: u32, what: &str, pc: u32) -> u8 {
    let pa = guest_mem::translate_va(addr).unwrap_or(addr);
    match guest_mem::read_byte_pa(pa) {
        Some(b) => b,
        None => {
            kprintln!("*** {}: cannot read byte at {:#x} @PC={:#x}", what, addr, pc);
            cpu::halt();
        }
    }
}

/// Write a single guest byte (VA-translate, identity if MMU off). Halts
/// loudly with `what` and `pc` if the write faults.
pub fn write_byte_or_halt(addr: u32, value: u8, what: &str, pc: u32) {
    let pa = guest_mem::translate_va(addr).unwrap_or(addr);
    if !guest_mem::write_byte_pa(pa, value) {
        kprintln!("*** {}: cannot write byte at {:#x} @PC={:#x}", what, addr, pc);
        cpu::halt();
    }
}
