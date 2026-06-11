//! Byte-order-aware accessors for guest memory.
//!
//! This module is the single bottleneck through which the hypervisor
//! reads and writes guest memory at the architectural level (i.e. as
//! the guest CPU itself would see it via LDR/STR). Phase 1 of the
//! BE-8 migration (`PLAN_BE8_MIGRATION.md`) introduces these helpers
//! with **identity behavior**: every call delegates to the matching
//! `guest_mem::*` accessor unchanged. Phase 2c flips the byte-order
//! policy in this one file when the guest moves from "BE-32 word-
//! invariant via load-time word swap" to "BE-8 (CPSR.E=1) data
//! accesses".
//!
//! API contract — what the helpers *return / store*:
//!
//! - `guest_read_u32_*` / `guest_write_u32_*`: the value is the
//!   Newton-side numerical value, i.e. interpreted big-endian
//!   regardless of how the bytes are physically laid out in host
//!   memory. A caller that wants "the u32 the BE-32 source code
//!   reads through `LDR Rd, [Rm]`" gets exactly that.
//! - `guest_read_u8_va` / `guest_read_u16_va`: the byte (halfword)
//!   at the given guest *logical* address. Logical byte 0 of an
//!   aligned u32 is the most-significant byte. Today this maps to
//!   `host[va ^ 3]` (XOR-3 byte-lane transform under BE-32
//!   word-invariant); after Phase 2 the CPU itself does the
//!   byte-lane transform on every store, so logical byte 0 will
//!   live at `host[va]` directly.
//! - `guest_read_bytes_va`: copies a contiguous range in Newton-side
//!   logical-byte order into `out`, so the buffer matches what the
//!   BE-32 source code would see byte-by-byte.

use crate::guest_mem;

// In normal (BE-8) builds, the guest stores values with bytes in BE
// order. To recover the Newton-side numerical value from a host-LE
// view, we byte-swap on read and on write. Guest-test mode runs the
// guest in LE; the helpers act as identity wrappers so existing
// tests keep working.
//
// Exception: ROM **code** words are stored as LE byte order on host
// (the CPU's instruction fetch is always LE on Cortex-A53). When EL2
// reads a code word for emulation (e.g. handle_und decoding the
// faulting instruction), we want the instruction encoding back, NOT
// the byteswap. The classifier's `reach.bitmap` (consulted via
// `guest_mem::rom_word_is_code`) discriminates code from data per
// 32-bit ROM word; data words and everything outside the ROM
// aperture (RAM, framebuffer) are swapped on read/write.

#[cfg(not(nh_guest_test))]
#[inline]
fn pa_is_rom_code(pa: u32) -> bool {
    // Tracer trampoline pool: written by `tracer::init` as native-LE
    // instruction words (slot[0]/slot[1]/slot[2]) interleaved with
    // byte-swapped data literals (slot[3]/slot[4]). The classifier's
    // reach.bitmap doesn't cover this address range — and can't, since
    // the slots are populated at runtime — so without this short-circuit
    // a `handle_und` decoding the trampoline's `hvc #TRACE_TAG` from
    // USR mode would byteswap it and miss the dispatch arm.
    #[cfg(feature = "trace")]
    if pa >= crate::tracer::TRAMPOLINE_IPA && pa < crate::tracer::TRAMPOLINE_END {
        return true;
    }
    // Patch-stub arena: every wrapper installed by `rom_patches` (the
    // ResolveFault wrapper, NewStack pad, LockHeapRange wrappers, …)
    // is plain ARM-encoded LE instruction words — same situation as the
    // tracer pool. The classifier's bitmap doesn't reach here either.
    // Without this short-circuit, a USR-mode HVC inside a wrapper (e.g.
    // the ResolveFault-wrapper exit probe) gets its insn byteswapped
    // before handle_und compares against `HvcImm::*.insn()`, and the
    // dispatch arm fails to match.
    if pa >= crate::rom_patches::PATCH_STUB_ARENA_BASE
        && pa < crate::rom_patches::PATCH_STUB_ARENA_END {
        return true;
    }
    let pa = pa as usize;
    pa + 4 <= guest_mem::ROM_SIZE && guest_mem::rom_word_is_code(pa / 4)
}

#[cfg(not(nh_guest_test))]
#[inline]
fn swap_for_pa(pa: u32, raw: u32) -> u32 {
    if pa_is_rom_code(pa) { raw } else { raw.swap_bytes() }
}

#[cfg(nh_guest_test)]
#[inline]
fn swap_for_pa(_pa: u32, raw: u32) -> u32 { raw }

// Used only by the u16 read helpers, which are consumed solely by the
// `audio-pi-hdmi` backend — dead in the default/FVP builds.
#[cfg(not(nh_guest_test))]
#[inline]
#[allow(dead_code)]
fn swap16(v: u16) -> u16 { v.swap_bytes() }

/// Read a 32-bit word from a guest VA and return it as a Newton-side
/// numerical value.
pub fn guest_read_u32_va(va: u32) -> Option<u32> {
    let pa = guest_mem::translate_va(va).unwrap_or(va);
    guest_mem::read_word_pa(pa).map(|w| swap_for_pa(pa, w))
}

/// Read a 32-bit word from a guest PA. See `guest_read_u32_va`.
pub fn guest_read_u32_pa(pa: u32) -> Option<u32> {
    guest_mem::read_word_pa(pa).map(|w| swap_for_pa(pa, w))
}

/// Write a 32-bit Newton-side numerical value to a guest VA.
pub fn guest_write_u32_va(va: u32, value: u32) -> bool {
    let pa = guest_mem::translate_va(va).unwrap_or(va);
    guest_mem::write_word_pa(pa, swap_for_pa(pa, value))
}

/// Write a 32-bit Newton-side numerical value to a guest PA.
pub fn guest_write_u32_pa(pa: u32, value: u32) -> bool {
    guest_mem::write_word_pa(pa, swap_for_pa(pa, value))
}

/// Read a single byte from a guest PA at the given Newton-side logical
/// byte address. Under BE-8 with CPSR.E=1 the CPU stored the byte at
/// the natural offset, so host[pa] is exactly the logical byte. Guest-
/// test mode keeps the legacy XOR-3 byte-lane transform.
#[cfg(not(nh_guest_test))]
pub fn guest_read_u8_pa(pa: u32) -> Option<u8> {
    guest_mem::read_byte_pa(pa)
}

#[cfg(nh_guest_test)]
pub fn guest_read_u8_pa(pa: u32) -> Option<u8> {
    guest_mem::read_byte_pa(pa ^ 3)
}

/// Read a single byte from a guest VA at the given Newton-side logical
/// byte address. Walks stage-1 to find the PA, then delegates.
pub fn guest_read_u8_va(va: u32) -> Option<u8> {
    let pa = guest_mem::translate_va(va).unwrap_or(va);
    guest_read_u8_pa(pa)
}

/// Read a halfword from a guest PA at the given Newton-side logical
/// halfword address. Consumed only by the `audio-pi-hdmi` backend.
#[cfg(not(nh_guest_test))]
#[allow(dead_code)]
pub fn guest_read_u16_pa(pa: u32) -> Option<u16> {
    guest_mem::read_halfword_pa(pa).map(swap16)
}

#[cfg(nh_guest_test)]
#[allow(dead_code)]
pub fn guest_read_u16_pa(pa: u32) -> Option<u16> {
    guest_mem::read_halfword_pa(pa ^ 2)
}

/// Read a halfword from a guest VA at the given Newton-side logical
/// halfword address. Consumed only by the `audio-pi-hdmi` backend.
#[allow(dead_code)]
pub fn guest_read_u16_va(va: u32) -> Option<u16> {
    let pa = guest_mem::translate_va(va).unwrap_or(va);
    guest_read_u16_pa(pa)
}

/// Write a single byte to a guest PA at the given Newton-side logical
/// byte address.
#[cfg(not(nh_guest_test))]
pub fn guest_write_u8_pa(pa: u32, value: u8) -> bool {
    guest_mem::write_byte_pa(pa, value)
}

#[cfg(nh_guest_test)]
pub fn guest_write_u8_pa(pa: u32, value: u8) -> bool {
    guest_mem::write_byte_pa(pa ^ 3, value)
}

/// Read a contiguous range of guest bytes in Newton-side logical-byte
/// order into `out`. Stops short on the first failed VA→PA translation;
/// returns the number of bytes actually written, or `None` if the very
/// first word fails.
///
/// The range is read word-by-word and each word is reformatted via
/// `to_be_bytes()` so the buffer mirrors the original on-disk byte
/// order. A caller that wants to print a kernel string verbatim (or
/// hash a binary blob) gets the bytes in their natural sequence.
///
/// Consumed only by the `log_store` Ref pretty-printer today.
#[allow(dead_code)]
pub fn guest_read_bytes_va(addr: u32, out: &mut [u8]) -> Option<usize> {
    let mut written = 0;
    let mut cursor = addr;
    while written + 4 <= out.len() {
        let w = guest_read_u32_va(cursor).or_else(|| guest_read_u32_pa(cursor));
        let w = match w {
            Some(w) => w,
            None => break,
        };
        out[written..written + 4].copy_from_slice(&w.to_be_bytes());
        written += 4;
        cursor = cursor.wrapping_add(4);
    }
    if written == 0 { None } else { Some(written) }
}
