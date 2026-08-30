//! IEEE CRC-32 (the `zlib.crc32` / PNG / Ethernet polynomial), used
//! for the HYPERV.IMG header and every protocol message, so the host
//! script can compute the same values with the Python stdlib.
//!
//! Cortex-A53 implements the ARMv8 CRC32 extension, and the
//! `target-cpu=cortex-a53` flag in `.cargo/config.toml` enables it,
//! so the primary path is the `CRC32X/W/B` instructions (~1 byte per
//! cycle even on uncached reads — the bootloader runs with the MMU
//! off). The table fallback keeps the crate building on a target
//! without `crc`, e.g. host-side unit tests.

/// CRC-32 of `data`, init and final XOR 0xFFFF_FFFF.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// Continue a CRC from a *raw* running register value (already
/// inverted). `crc32(a ++ b) == crc32_update(crc32_update(!0, a), b) ^ !0`.
pub fn crc32_update(state: u32, data: &[u8]) -> u32 {
    if cfg!(target_feature = "crc") {
        // SAFETY: the `crc` target feature is enabled for this build
        // (checked just above), so the instructions exist.
        unsafe { crc32_hw(state, data) }
    } else {
        crc32_sw(state, data)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32_hw(mut crc: u32, data: &[u8]) -> u32 {
    use core::arch::aarch64::{__crc32b, __crc32d};
    // The intrinsics operate on the running (inverted) register value
    // directly; the init / final XOR live in `crc32`.
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        crc = __crc32d(crc, u64::from_le_bytes(c.try_into().unwrap()));
    }
    for &b in chunks.remainder() {
        crc = __crc32b(crc, b);
    }
    crc
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn crc32_hw(crc: u32, data: &[u8]) -> u32 {
    crc32_sw(crc, data)
}

/// Nibble-table software CRC (16 entries; the bootloader is not
/// throughput-bound on this path).
const fn crc32_sw(mut crc: u32, data: &[u8]) -> u32 {
    const TABLE: [u32; 16] = {
        let mut t = [0u32; 16];
        let mut i = 0;
        while i < 16 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 4 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    // `while` rather than `for`: this runs in const context for the
    // check-vector assertion below.
    let mut i = 0;
    while i < data.len() {
        let b = data[i] as u32;
        crc = TABLE[((crc ^ b) & 0xF) as usize] ^ (crc >> 4);
        crc = TABLE[((crc ^ (b >> 4)) & 0xF) as usize] ^ (crc >> 4);
        i += 1;
    }
    crc
}

// The check vector every CRC-32 implementation is tested against.
const _: () = assert!(crc32_sw(0xFFFF_FFFF, b"123456789") ^ 0xFFFF_FFFF == 0xCBF4_3926);
