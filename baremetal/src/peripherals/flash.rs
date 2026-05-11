//! Newton internal-store flash — Rust port of Einstein's `TFlash`.
//!
//! Two 4 MiB banks held back-to-back in a single 8 MiB backing, but
//! surfaced to the guest at two disjoint IPAs matching the real
//! hardware map:
//!
//!   guest IPA 0x02000000..0x02400000 → bytes 0..0x400000 of backing (bank 0)
//!   guest IPA 0x10000000..0x10400000 → bytes 0x400000..0x800000   (bank 1)
//!
//! Stage-2 maps both windows RW with no trap path: the Newton kernel
//! manages the AMD-style programming state machine in software, so
//! plain CPU loads / stores to the mapped pages are all the guest
//! ever needs. Cross-reference `Emulator/TMemoryConsts.h` for
//! `kFlashBank1` (0x02000000) and `kFlashBank2` (0x10000000).
//!
//! On a fresh boot, `init()` seeds the Newton filesystem header
//! (duplicated at block 0 / offset 0 and block 1 / offset 0x10000 of
//! bank 0). Every other byte stays at the static-init value of 0x00,
//! matching Einstein's behaviour: Einstein's flash file is mmap'd with
//! O_CREAT (TFlash.cpp:67-70), which gives a zero-filled buffer for
//! never-written bytes. We honour the same invariant — the static
//! GUEST_FLASH array is zero-initialised. Real flash hardware would
//! read 0xFF for erased bytes, but the kernel was tested against
//! Einstein's zero-fill backing, so we match Einstein not real flash.
//! See `Emulator/TFlash.cpp:137-172` for ground truth.
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

static mut GUEST_FLASH: Flash = Flash([0x00; SIZE]);
// Note: start zero-filled to match Einstein's mmap O_CREAT backing
// (TFlash.cpp:66-70). The kernel was tested against Einstein, not
// against real 0xFF-erased flash — so 0x00 is the correct initial
// value. `init()` then seeds the DLDS/OSCD header.

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

/// Compute the 10-entry ROM-REx checksum table the kernel's
/// `TReservedBlockAccessor` uses for its post-read validation pass
/// (`trace 1628 operator==(TROMREXCheckSums...)`), and seed it into
/// flash at `0x64..0x8C` of both block 0 and block 1.
///
/// Each `ComputeSegmentChecksums` entry is two u32s (`highBits`,
/// `lowBits`) where `highBits += word >> 16` and
/// `lowBits += word & 0xFFFF` for every u32 in the segment. Mirrors
/// `TROMImage::DoComputeChecksums` (`Emulator/ROM/TROMImage.cpp:164-229`).
///
/// Without this, the kernel sees zeros where checksums should be,
/// declares the flash header stale, and runs
/// `UpdateBlock0FromBlock1` → block-1 → block-0 restore. That restore
/// round-trips through the 16-bit `DoWrite` stride expansion, which
/// makes the kernel's subsequent `CompareFlashAndMemRebootIfDifferent`
/// fail (read-back is sparse; source buffer is dense). The kernel
/// then calls `PowerOffAndReboot`.
///
/// `rom_le_words` is the byteswapped-to-LE view of the full 16 MiB
/// ROM+REx aperture that the guest sees (`guest_mem::rom_host_pa()`
/// as `*const u32`). We compute checksums over it as LE-host u32
/// values — the same ones the kernel will see via LE LDR at runtime.
pub fn seed_rom_rex_checksums(rom_le_words: *const u32, rom_len_bytes: usize) {
    // Read the base-ROM size from ROM+0x3C (as Einstein does). This
    // is a u32 word; the ROM is already byteswapped to LE, so the
    // value matches what the guest would read.
    // SAFETY: caller asserts rom_le_words covers rom_len_bytes bytes.
    let base_size = unsafe { rom_le_words.add(0x3C / 4).read() };
    if base_size == 0 || (base_size as usize) > rom_len_bytes {
        return;
    }

    // Find embedded REx(es) — scan from base_size forwards for
    // "RExB" / "lock" magic. The ROM was byteswapped BE→LE on load, so
    // an ASCII string stored in BE order reads as a u32 with the
    // bytes in BE order = `from_be_bytes`. Einstein caps at 4 REx
    // slots.
    const MAGIC_REXB: u32 = u32::from_be_bytes(*b"RExB");
    const MAGIC_LOCK: u32 = u32::from_be_bytes(*b"lock");
    let mut rex_bases = [0u32; 4];
    let mut rex_sizes = [0u32; 4];
    let mut nb_rexes = 0usize;
    let mut cursor = base_size;
    while nb_rexes < 4 && (cursor as usize) < rom_len_bytes.saturating_sub(0x20) {
        let m0 = unsafe { rom_le_words.add((cursor / 4) as usize).read() };
        let m1 = unsafe { rom_le_words.add((cursor / 4) as usize + 1).read() };
        if m0 != MAGIC_REXB || m1 != MAGIC_LOCK {
            break;
        }
        let sz = unsafe { rom_le_words.add(((cursor + 0x18) / 4) as usize).read() };
        if sz < 0x20 || (cursor + sz) as usize > rom_len_bytes {
            break;
        }
        rex_bases[nb_rexes] = cursor;
        rex_sizes[nb_rexes] = sz;
        nb_rexes += 1;
        cursor += sz;
    }
    // Also scan at the external-REx anchor 0x00800000.
    if nb_rexes < 4 {
        let anchor = 0x0080_0000u32;
        if (anchor as usize) < rom_len_bytes {
            let m0 = unsafe { rom_le_words.add((anchor / 4) as usize).read() };
            let m1 = unsafe { rom_le_words.add((anchor / 4) as usize + 1).read() };
            if m0 == MAGIC_REXB && m1 == MAGIC_LOCK {
                let sz = unsafe { rom_le_words.add(((anchor + 0x18) / 4) as usize).read() };
                if sz >= 0x20 && (anchor + sz) as usize <= rom_len_bytes {
                    rex_bases[nb_rexes] = anchor;
                    rex_sizes[nb_rexes] = sz;
                    nb_rexes += 1;
                }
            }
        }
    }

    let mut checksums = [0u32; 10];
    compute_segment_checksum(rom_le_words, 0, base_size, &mut checksums[0..2]);
    for i in 0..4 {
        let (base, size) = (rex_bases[i], rex_sizes[i]);
        let slot = &mut checksums[(2 * i) + 2..(2 * i) + 4];
        if size == 0 {
            slot[0] = 0xFFFF_FFFF;
            slot[1] = 0xFFFF_FFFF;
        } else {
            compute_segment_checksum(rom_le_words, base, size, slot);
        }
    }

    // Write the 10 u32 checksums at flash[0x64 .. 0x8C] of BLOCK 0 only.
    // Einstein's loop at TFlash.cpp:108-134 reads `Read(0x64 + 4*i, 0)`
    // (note `bank=0`, but more importantly `Write(..., 0)` to block 0
    // only) — block 1 is left zeroed at 0x64..0x8C. Mirror that.
    crate::dprintln!(
        "flash: ROM/REx checksums seeded (base_size={:#x}, nb_rexes={})",
        base_size, nb_rexes
    );
    for (i, csum) in checksums.iter().enumerate() {
        write_u32(0x64 + (i as u32) * 4, *csum);
    }
}

fn compute_segment_checksum(
    rom_le_words: *const u32,
    base: u32,
    size: u32,
    out: &mut [u32],
) {
    let mut high: u32 = 0;
    let mut low: u32 = 0;
    let start_word = (base / 4) as usize;
    let word_count = (size / 4) as usize;
    for i in 0..word_count {
        // SAFETY: caller bounds-checked that base + size <= rom_len_bytes.
        let value = unsafe { rom_le_words.add(start_word + i).read() };
        low = low.wrapping_add(value & 0x0000_FFFF);
        high = high.wrapping_add(value >> 16);
    }
    out[0] = high;
    out[1] = low;
}

fn seed_block(base: u32) {
    // Offsets and constants from `Emulator/TFlash.cpp:145-168`. The
    // u32 values are byte-for-byte what a guest `LDR` at the same IPA
    // sees through Einstein's TFlash::Read (which does the BE->host
    // swap internally). The unseeded gap bytes (0x0C..0x23 etc.) are
    // already 0 because the static GUEST_FLASH is zero-initialised
    // (matches Einstein's mmap O_CREAT backing).
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
    // The kernel reads flash via stage-2-mapped LDR with CPSR.E=1
    // (BE-8), so on-disk bytes must be the BE encoding of `value`.
    // A native LE u32 store of `value.swap_bytes()` lays down the
    // right bytes. Guest-test mode runs LE — identity store.
    #[cfg(not(nh_guest_test))]
    let stored = value.swap_bytes();
    #[cfg(nh_guest_test)]
    let stored = value;
    // SAFETY: bounds- and alignment-checked above; called single-threaded
    // from kmain on core 0 before the guest is running, so no aliasing.
    unsafe {
        let base = addr_of_mut!(GUEST_FLASH) as *mut u8;
        (base.add(byte_offset as usize) as *mut u32).write(stored);
    }
}

/// Guest PA of flash bank 0 (Einstein `kFlashBank1`).
pub const BANK0_PA_BASE: u32 = 0x0200_0000;
/// Guest PA of flash bank 1 (Einstein `kFlashBank2`).
pub const BANK1_PA_BASE: u32 = 0x1000_0000;

/// Translate a guest flash PA to a byte offset in the backing store.
/// Returns None for addresses outside either bank's window.
pub fn pa_to_offset(pa: u32) -> Option<usize> {
    if pa >= BANK0_PA_BASE && pa < BANK0_PA_BASE + BANK_SIZE as u32 {
        Some((pa - BANK0_PA_BASE) as usize)
    } else if pa >= BANK1_PA_BASE && pa < BANK1_PA_BASE + BANK_SIZE as u32 {
        Some(BANK_SIZE + (pa - BANK1_PA_BASE) as usize)
    } else {
        None
    }
}

/// True when `pa` falls inside flash bank 0 or bank 1.
/// Used by `trap::handle_data_abort` to recognise stage-2 RO faults
/// that should be silently dropped (matching `TMemory::WriteP`).
pub fn is_flash_pa(pa: u64) -> bool {
    let pa32 = pa as u32;
    if pa > u32::MAX as u64 {
        return false;
    }
    (pa32 >= BANK0_PA_BASE && pa32 < BANK0_PA_BASE + BANK_SIZE as u32)
        || (pa32 >= BANK1_PA_BASE && pa32 < BANK1_PA_BASE + BANK_SIZE as u32)
}

/// Masked 32-bit program into flash, following Einstein's
/// `TFlash::Write` semantics (`Emulator/TFlash.cpp:192-208`): the
/// stored word becomes `(existing & ~mask) | word`. Returns false if
/// `pa` is outside the flash windows or not word-aligned.
pub fn program_word(pa: u32, word: u32, mask: u32) -> bool {
    if pa & 3 != 0 {
        return false;
    }
    let Some(off) = pa_to_offset(pa) else {
        return false;
    };
    if off + 4 > SIZE {
        return false;
    }
    // SAFETY: `off` bounded above; single-writer under the EL2 trap
    // handler.
    //
    // Under BE-8 the kernel reads the prev/new word via LDR with
    // CPSR.E=1 (BE byte order). Round-trip through swap_bytes so the
    // mask logic operates on the kernel-intended numerical value.
    unsafe {
        let base = addr_of_mut!(GUEST_FLASH) as *mut u8;
        let slot = base.add(off) as *mut u32;
        #[cfg(not(nh_guest_test))]
        let prev = core::ptr::read_volatile(slot).swap_bytes();
        #[cfg(nh_guest_test)]
        let prev = core::ptr::read_volatile(slot);
        let new = (prev & !mask) | word;
        #[cfg(not(nh_guest_test))]
        let stored = new.swap_bytes();
        #[cfg(nh_guest_test)]
        let stored = new;
        core::ptr::write_volatile(slot, stored);
    }
    crate::flash_persist::mark_dirty(off, 4);
    true
}

/// Erase a block by filling `size` bytes with `0xFF` starting at
/// `pa`. Matches `TFlash::Erase` (`Emulator/TFlash.cpp:214-235`);
/// whole block assumed to lie in one bank. Returns false on
/// out-of-range.
pub fn erase_block(pa: u32, size: u32) -> bool {
    let Some(off) = pa_to_offset(pa) else { return false };
    let end = off + size as usize;
    if end > SIZE {
        return false;
    }
    // SAFETY: bounds-checked.
    unsafe {
        let base = addr_of_mut!(GUEST_FLASH) as *mut u8;
        core::ptr::write_bytes(base.add(off), 0xFF, size as usize);
    }
    crate::flash_persist::mark_dirty(off, size as usize);
    true
}
