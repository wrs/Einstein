//! The HYPERV.IMG container: a fixed-size file on the boot partition
//! holding the hypervisor's raw `kernel8.img` bytes behind a small
//! header. The firmware loads the whole file to [`IMAGE_ADDR`] via
//! `initramfs HYPERV.IMG 0x02000000` in config.txt, so the bootloader
//! never reads it from the SD card itself; it only validates, copies
//! the payload to the hypervisor's link address and jumps.
//!
//! Layout (all little-endian; mirrored by `ImageFormat` in
//! `scripts/pi-upload.py` — change both together):
//!
//! ```text
//!   0x000  "NHIMG001"          magic
//!   0x008  u32 payload_len     bytes of payload after the header
//!   0x00C  u32 payload_crc     CRC-32 of the payload
//!   0x010  u32 hdr_crc         CRC-32 of bytes [0x000, 0x010)
//!   0x014  zero-fill to HDR_SIZE
//!   0x1000 payload             the hypervisor image, then zero pad
//!                              to FILE_SIZE
//! ```
//!
//! The file is a fixed 16 MiB so that a re-upload can rewrite
//! individual sectors in place (persist.rs) without ever changing the
//! FAT allocation.

use crate::crc::crc32;

/// Where the firmware places the file (config.txt `initramfs` line).
pub const IMAGE_ADDR: usize = 0x0200_0000;
/// Header bytes before the payload.
pub const HDR_SIZE: usize = 4096;
/// Size of HYPERV.IMG on the card, always.
pub const FILE_SIZE: usize = 16 * 1024 * 1024;
/// Largest payload the container can hold.
pub const MAX_PAYLOAD: usize = FILE_SIZE - HDR_SIZE;
/// Where the hypervisor links and expects to run (linker.ld.in,
/// platform-raspi3b).
pub const LOAD_ADDR: usize = 0x8_0000;

/// Staging area for an image arriving over the serial link (xfer.rs):
/// the "new" container is assembled here while the firmware-loaded
/// "old" one at [`IMAGE_ADDR`] stays intact as the COPY source.
pub const NEW_BASE: usize = 0x0300_0000;

const MAGIC: &[u8; 8] = b"NHIMG001";
const OFF_LEN: usize = 8;
const OFF_CRC: usize = 12;
const OFF_HDR_CRC: usize = 16;

// The payload is copied down to LOAD_ADDR; it must not reach back into
// its own source at IMAGE_ADDR.
const _: () = assert!(LOAD_ADDR + MAX_PAYLOAD <= IMAGE_ADDR);
// The two containers must not overlap each other or nhboot itself
// (linked at 0x1000_0000, linker.ld).
const _: () = assert!(NEW_BASE >= IMAGE_ADDR + FILE_SIZE);
const _: () = assert!(NEW_BASE + FILE_SIZE <= 0x1000_0000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    Valid { len: u32, crc: u32 },
    NoMagic,
    BadHeaderCrc,
    BadLength,
    BadPayloadCrc { expected: u32, actual: u32 },
}

fn read_u32(base: usize, off: usize) -> u32 {
    // SAFETY: `base` is RAM the firmware or the upload path filled;
    // the MMU is off so there are no attribute concerns, and reads
    // are volatile because nothing in this program wrote there.
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

/// Validate the container at `base` without touching anything else.
pub fn inspect(base: usize) -> ImageState {
    // SAFETY: as in `read_u32`.
    let hdr: &[u8] = unsafe { core::slice::from_raw_parts(base as *const u8, HDR_SIZE) };
    if &hdr[..MAGIC.len()] != MAGIC {
        return ImageState::NoMagic;
    }
    if crc32(&hdr[..OFF_HDR_CRC]) != read_u32(base, OFF_HDR_CRC) {
        return ImageState::BadHeaderCrc;
    }
    let len = read_u32(base, OFF_LEN);
    if len == 0 || len as usize > MAX_PAYLOAD {
        return ImageState::BadLength;
    }
    let expected = read_u32(base, OFF_CRC);
    // SAFETY: length was bounds-checked against the container.
    let payload: &[u8] =
        unsafe { core::slice::from_raw_parts((base + HDR_SIZE) as *const u8, len as usize) };
    let actual = crc32(payload);
    if actual != expected {
        return ImageState::BadPayloadCrc { expected, actual };
    }
    ImageState::Valid { len, crc: expected }
}

/// Write a container header at `base` for a payload of `len` bytes
/// with CRC `crc` — the same bytes `scripts/pi-upload.py --make-image`
/// produces. The rest of the header is zero-filled.
pub fn write_header(base: usize, len: u32, crc: u32) {
    // SAFETY: `base` is the start of one of the two container areas,
    // which nothing else in the bootloader aliases.
    let hdr: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, HDR_SIZE) };
    hdr.fill(0);
    hdr[..MAGIC.len()].copy_from_slice(MAGIC);
    hdr[OFF_LEN..OFF_LEN + 4].copy_from_slice(&len.to_le_bytes());
    hdr[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    let hdr_crc = crc32(&hdr[..OFF_HDR_CRC]);
    hdr[OFF_HDR_CRC..OFF_HDR_CRC + 4].copy_from_slice(&hdr_crc.to_le_bytes());
}

/// Copy the payload at `base` to [`LOAD_ADDR`] and enter it the way
/// the firmware would have: EL2, x0 = DTB pointer, x1..x3 = 0.
///
/// # Safety
/// `base` must hold a container that `inspect` reported `Valid` with
/// this `len`. Never returns; the bootloader's memory is dead after
/// the jump.
pub unsafe fn boot(base: usize, len: u32, dtb: u64) -> ! {
    let src = (base + HDR_SIZE) as *const u64;
    let dst = LOAD_ADDR as *mut u64;
    let words = (len as usize).div_ceil(8);
    // Word copy; the regions never overlap (asserted above), and the
    // pad after the payload is zero so over-reading up to 7 bytes is
    // harmless.
    for i in 0..words {
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
    }
    // Make the stores visible to instruction fetch: barrier, drop the
    // I-cache (it may hold lines from the previous occupant of
    // 0x80000 — the bootloader's own load image), barrier, isb.
    core::arch::asm!(
        "dsb sy",
        "ic iallu",
        "dsb sy",
        "isb",
        "mov x1, xzr",
        "mov x2, xzr",
        "mov x3, xzr",
        "br x4",
        // The entry address is pinned to x4: with a compiler-chosen
        // register the allocator could hand us x1..x3, which the
        // `mov`s above zero before the branch.
        in("x4") LOAD_ADDR as u64,
        in("x0") dtb,
        options(noreturn)
    )
}
