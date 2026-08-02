//! BE-8 sub-word lane math for MMIO accesses.
//!
//! Pure functions, no state. The guest runs BE-8: byte 0 of an aligned
//! word is the MSB, so a guest LDRB/STRB at lane 0 addresses
//! bits[31:24] of the 32-bit register word. The MMIO router
//! (`hv::mmio`) uses these helpers to normalize sub-word accesses onto
//! word-granular peripheral models: writes splice the sub-word value
//! into the surrounding register word, reads extract the addressed
//! lane, and splice/extract share the lane-shift functions so a
//! write-then-read of a single byte round-trips (periph-H2/periph-M1).
//!
//! Guest-test builds (`nh_guest_test`) run the guest LE under the
//! legacy inline-patch path, where inline-stub byte/halfword accesses
//! are pre-XOR'd by 3/2; [`unxor_sub_word`] undoes that instead, and
//! the lane splice/extract path is compiled out.

/// Mask `value` down to the access width (`sas`: 0 = byte,
/// 1 = halfword, 2+ = word).
pub const fn mask_for_size(value: u32, sas: u8) -> u32 {
    match sas {
        0 => value & 0xFF,
        1 => value & 0xFFFF,
        _ => value,
    }
}

/// BE-8 byte-lane shift for `ipa`: lane 0 (IPA mod 4 == 0) is
/// bits[31:24] (MSB-side under BE-8, since the guest sees byte 0 of an
/// aligned word as the MSB), lane 3 is bits[7:0].
#[cfg(not(nh_guest_test))]
pub const fn byte_lane_shift(ipa: u64) -> u32 {
    let lane = (ipa & 3) as u32;
    24 - 8 * lane // lane 0 → 24 (bits[31:24] = MSB)
}

/// BE-8 halfword-lane shift for `ipa`: halfword 0 (IPA aligned mod 4
/// == 0) is bits[31:16]; halfword 1 is bits[15:0].
#[cfg(not(nh_guest_test))]
pub const fn halfword_lane_shift(ipa: u64) -> u32 {
    let lane = ((ipa >> 1) & 1) as u32;
    if lane == 0 {
        16
    } else {
        0
    }
}

/// Splice a guest BE-8 byte write into the existing word `prev`.
#[cfg(not(nh_guest_test))]
pub const fn splice_byte(prev: u32, ipa: u64, byte: u32) -> u32 {
    let shift = byte_lane_shift(ipa);
    let mask = !(0xFFu32 << shift);
    (prev & mask) | ((byte & 0xFF) << shift)
}

/// Splice a guest BE-8 halfword write into the existing word `prev`.
#[cfg(not(nh_guest_test))]
pub const fn splice_halfword(prev: u32, ipa: u64, half: u32) -> u32 {
    let shift = halfword_lane_shift(ipa);
    let mask = !(0xFFFFu32 << shift);
    (prev & mask) | ((half & 0xFFFF) << shift)
}

/// Extract the BE-8 sub-word lane addressed by `ipa` from the aligned
/// register word `word` (periph-H2). The inverse of the write splice:
/// a byte read at lane 0 returns bits[31:24], the same lane a byte
/// write at lane 0 targets, so write-then-read of a single byte
/// round-trips. `sas` is 0 (byte) or 1 (halfword); a word read never
/// reaches here.
#[cfg(not(nh_guest_test))]
pub const fn extract_sub_word(word: u32, ipa: u64, sas: u8) -> u32 {
    match sas {
        0 => (word >> byte_lane_shift(ipa)) & 0xFF,
        _ => (word >> halfword_lane_shift(ipa)) & 0xFFFF,
    }
}

/// Un-XOR the BE-32 byte / halfword XOR that the inline-stub emitter
/// applies before an MMIO-range access. Only used in guest-test mode
/// (the legacy inline-patch path). Above XOR_LIMIT (PCMCIA etc.),
/// inline stubs skip the XOR and we shouldn't un-XOR.
#[cfg(nh_guest_test)]
pub const fn unxor_sub_word(ipa: u64, sas: u8) -> u64 {
    const XOR_LIMIT: u64 = 0x1000_0000;
    if ipa >= XOR_LIMIT {
        return ipa;
    }
    match sas {
        0 => ipa ^ 3,
        1 => ipa ^ 2,
        _ => ipa,
    }
}

// =======================================================================
// Compile-time lane-math checks
// =======================================================================
//
// The crate has no host test harness (no_std, cross-compiled), so the
// lane math is verified by const evaluation, same pattern as
// `arch::aarch32_emit::_check_encoders`.

const fn _check_mask() {
    assert!(mask_for_size(0xDEAD_BEEF, 0) == 0xEF);
    assert!(mask_for_size(0xDEAD_BEEF, 1) == 0xBEEF);
    assert!(mask_for_size(0xDEAD_BEEF, 2) == 0xDEAD_BEEF);
}
const _: () = _check_mask();

#[cfg(not(nh_guest_test))]
const fn _check_lanes() {
    // Byte lanes: lane 0 is the MSB under BE-8.
    assert!(byte_lane_shift(0x0F24_3000) == 24);
    assert!(byte_lane_shift(0x0F24_3001) == 16);
    assert!(byte_lane_shift(0x0F24_3002) == 8);
    assert!(byte_lane_shift(0x0F24_3003) == 0);
    // Halfword lanes: halfword 0 is bits[31:16].
    assert!(halfword_lane_shift(0x0F24_3000) == 16);
    assert!(halfword_lane_shift(0x0F24_3002) == 0);
    // Byte splice targets exactly the addressed lane.
    assert!(splice_byte(0x1122_3344, 0x1000, 0xAA) == 0xAA22_3344);
    assert!(splice_byte(0x1122_3344, 0x1001, 0xAA) == 0x11AA_3344);
    assert!(splice_byte(0x1122_3344, 0x1002, 0xAA) == 0x1122_AA44);
    assert!(splice_byte(0x1122_3344, 0x1003, 0xAA) == 0x1122_33AA);
    // Halfword splice.
    assert!(splice_halfword(0x1122_3344, 0x1000, 0xAABB) == 0xAABB_3344);
    assert!(splice_halfword(0x1122_3344, 0x1002, 0xAABB) == 0x1122_AABB);
    // Extract is the inverse of splice: write-then-read round-trips.
    assert!(extract_sub_word(splice_byte(0, 0x1001, 0x5C), 0x1001, 0) == 0x5C);
    assert!(extract_sub_word(splice_halfword(0, 0x1002, 0xBEEF), 0x1002, 1) == 0xBEEF);
    assert!(extract_sub_word(0x1122_3344, 0x1000, 0) == 0x11);
    assert!(extract_sub_word(0x1122_3344, 0x1000, 1) == 0x1122);
}
#[cfg(not(nh_guest_test))]
const _: () = _check_lanes();

#[cfg(nh_guest_test)]
const fn _check_unxor() {
    // Below XOR_LIMIT: byte accesses un-XOR by 3, halfwords by 2.
    assert!(unxor_sub_word(0x0F00_1803, 0) == 0x0F00_1800);
    assert!(unxor_sub_word(0x0F00_1802, 1) == 0x0F00_1800);
    assert!(unxor_sub_word(0x0F00_1800, 2) == 0x0F00_1800);
    // At/above XOR_LIMIT (PCMCIA etc.): pass-through.
    assert!(unxor_sub_word(0x3000_0001, 0) == 0x3000_0001);
    assert!(unxor_sub_word(0x3000_0002, 1) == 0x3000_0002);
}
#[cfg(nh_guest_test)]
const _: () = _check_unxor();
