//! Newton internal-store flash — Rust port of Einstein's `TFlash`.
//!
//! Two 4 MiB banks back-to-back, mapped to guest IPAs 0x02000000 (bank 0)
//! and 0x02400000 (bank 1). Stage-2 maps the backing RW with no trap
//! path: the Newton kernel manages the AMD-style programming state
//! machine in software, so plain CPU loads / stores to the mapped
//! pages are all the guest ever needs.
//!
//! On a fresh boot, `init()` seeds the Newton filesystem header
//! (duplicated at block 0 / offset 0 and block 1 / offset 0x10000 of
//! bank 0). Every other byte stays zero, matching Einstein's behaviour
//! against an mmap-backed flash file: software "pretends" erased flash
//! reads as 0xFF, but the actual bytes on the backing are 0x00 until
//! a programmed write lands. See `docs/peripherals.md` §Flash and
//! `Emulator/TFlash.cpp:137-172` for ground truth.
//!
//! Einstein stores each 32-bit word big-endian in its mmap'd file and
//! byteswaps on Read/Write so the kernel sees native-order values.
//! We skip the indirection: the A53 is little-endian, the backing is
//! a native byte array, and a u32 written with the Rust `write` below
//! reads back as the same u32 from a guest `LDR`. The kernel sees the
//! same logical word as it would through Einstein.

use core::ptr::addr_of_mut;

/// Size of each bank (4 MiB). `kFlashBank1Size` / `kFlashBank2Size` in
/// `Emulator/TFlash.h`.
pub const BANK_SIZE: usize = 0x0040_0000;

/// Total backing size: two banks, back-to-back.
pub const SIZE: usize = BANK_SIZE * 2;

// 2 MiB alignment matches the stage-2 block-descriptor mapping
// strategy in `stage2.rs`.
#[repr(C, align(0x200000))]
struct Flash([u8; SIZE]);

static mut GUEST_FLASH: Flash = Flash([0; SIZE]);

/// Host physical base of the flash backing store.
pub fn host_pa() -> u64 {
    addr_of_mut!(GUEST_FLASH) as u64
}

/// Seed the Newton "DLDS" / "OSCD" header at the block-0 and block-1
/// offsets within bank 0. Mirrors the first-boot branch of
/// `TFlash::TFlash` (`Emulator/TFlash.cpp:137-168`). Bank 1 is left
/// zeroed, same as Einstein.
pub fn init() {
    seed_block(0x00000000);
    seed_block(0x00010000);
}

fn seed_block(base: u32) {
    // Offsets and constants from `Emulator/TFlash.cpp:145-168`. The
    // u32 values are byte-for-byte what a guest `LDR` at the same IPA
    // sees through Einstein's TFlash::Read (which does the BE->host
    // swap internally).
    write_u32(base + 0x00, 0x444C4453); // "DLDS"
    write_u32(base + 0x04, 0x4F534344); // "OSCD"
    write_u32(base + 0x08, 0x0000010C); // block size / offset to block 1
    write_u32(base + 0x24, 0x00003916); // calibration
    write_u32(base + 0x34, 0x0000465A); // calibration
    write_u32(base + 0x3C, 0x00008000); // calibration
    write_u32(base + 0x40, 0x00000000); // manufacture date
    write_u32(base + 0x50, 0x444C4453); // "DLDS" duplicate
    write_u32(base + 0x54, 0xD7ECCC66); // checksum
    // Einstein emits 0xFFFFFFFC at block 0's 0x58 and 0xFFFFFFF0 at
    // block 1's 0x58. Semantics aren't documented in the original
    // source; preserve them literally.
    let some_number = if base == 0 { 0xFFFFFFFC } else { 0xFFFFFFF0 };
    write_u32(base + 0x58, some_number);
    write_u32(base + 0x8C, 0xFFFFFFFF); // calibration-valid flag
}

fn write_u32(byte_offset: u32, value: u32) {
    assert!((byte_offset as usize) + 4 <= SIZE);
    assert!(byte_offset % 4 == 0);
    // SAFETY: bounds- and alignment-checked above; called single-threaded
    // from kmain on core 0 before the guest is running, so no aliasing.
    unsafe {
        let base = addr_of_mut!(GUEST_FLASH) as *mut u8;
        (base.add(byte_offset as usize) as *mut u32).write(value);
    }
}
