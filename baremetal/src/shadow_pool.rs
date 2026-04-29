//! Hypervisor-managed pool of "shadow" 4 KiB pages.
//!
//! Used by the alias-redirect path: when the guest kernel installs a
//! second VA mapping for a physical page that's already mapped at a
//! different VA, the hypervisor allocates a shadow PA from this pool
//! and redirects the new VA to it. Each shadow page has its own
//! 4 KiB host-backed storage and its own stage-2 mapping, so writes
//! through one VA never collide with writes through another even
//! though the kernel believes both VAs share a PA.
//!
//! Layout:
//! - 64 KiB of host-backed RW storage in `SHADOW_POOL` (16 pages).
//! - Mapped at stage-2 as `IPA 0x0601_0000..0x0602_0000` (immediately
//!   after `shadow_stub::SCRATCH_POOL`, sharing its 2 MiB L2 block).
//! - Allocations are monotonic via `NEXT_SLOT`. Slots are not freed
//!   today — the kernel's `PrimForgetMapping` path will be wired
//!   later to release the corresponding shadow when the VA is
//!   forgotten; for now alias installs on the boot timeline are well
//!   below 16, so the pool doesn't fill up.
//!
//! NOTE: the policy that USES this pool (the `PrimRememberMapping`
//! redirect) is intentionally NOT in this module — it lives in
//! `trap.rs` next to the existing Prim probes. This module is just
//! the allocator + storage + stage-2 wiring.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::kprintln;

/// IPA where the shadow pool starts. Chosen so it sits in the same
/// 2 MiB L2 block as `shadow_stub::SCRATCH_POOL` (which covers
/// IPA 0x0600_0000..0x0620_0000), letting us reuse `S2_L3_SCRATCH`.
pub const SHADOW_POOL_IPA: u32 = 0x0601_0000;
/// 16 4 KiB pages = 64 KiB. Enough to cover the 12 known Group-2
/// aliases plus headroom.
pub const SHADOW_POOL_SIZE: usize = 16 * 4096;
pub const SHADOW_POOL_PAGES: usize = SHADOW_POOL_SIZE / 4096;

#[repr(C, align(4096))]
pub struct ShadowPool(pub [u8; SHADOW_POOL_SIZE]);

/// Backing storage. SAFETY: `addr_of_mut!` is the only way to take
/// the address; concrete reads/writes go through `host_addr_for` in
/// `guest_mem`, which bounds-checks the IPA.
pub static mut SHADOW_POOL: ShadowPool = ShadowPool([0; SHADOW_POOL_SIZE]);

/// Next free slot index. Incremented monotonically by `allocate`.
static NEXT_SLOT: AtomicU32 = AtomicU32::new(0);

/// Return the host pointer to the shadow pool's first byte. Used by
/// `guest_mem::host_addr_for`.
pub fn host_pa() -> u64 {
    addr_of_mut!(SHADOW_POOL) as u64
}

/// Allocate a new shadow page. Returns the IPA (4 KiB-aligned) that
/// the caller should use as the redirect target. Returns `None` when
/// the pool is exhausted.
pub fn allocate() -> Option<u32> {
    let slot = NEXT_SLOT.fetch_add(1, Ordering::AcqRel);
    if (slot as usize) >= SHADOW_POOL_PAGES {
        // Roll back — keep the counter from runaway growth.
        NEXT_SLOT.store(SHADOW_POOL_PAGES as u32, Ordering::Relaxed);
        return None;
    }
    Some(SHADOW_POOL_IPA + slot * 0x1000)
}

/// Number of shadow pages currently allocated. Diagnostic only.
#[allow(dead_code)]
pub fn allocated_count() -> u32 {
    NEXT_SLOT.load(Ordering::Relaxed).min(SHADOW_POOL_PAGES as u32)
}

/// Smoke test: allocate the first shadow page, write a sentinel, read
/// it back through `read_word_pa`, and log. Verifies that the stage-2
/// mapping + host_addr_for hookup is live before any policy code uses
/// the pool. Does NOT consume a permanent slot — the slot it
/// allocates is "wasted" (no further use), but the slot count is
/// only ~1; we have 16 slots total.
pub fn smoke_test() {
    let slot_ipa = match allocate() {
        Some(ipa) => ipa,
        None => {
            kprintln!("shadow_pool: smoke test skipped — pool exhausted");
            return;
        }
    };
    // Write a sentinel via the IPA helper (which uses host_addr_for).
    const SENTINEL: u32 = 0xCAFEF00D;
    let ok_w = crate::guest_mem::write_word_pa(slot_ipa, SENTINEL);
    let read_back = crate::guest_mem::read_word_pa(slot_ipa);
    let ok = ok_w && read_back == Some(SENTINEL);
    kprintln!(
        "shadow_pool smoke test: ipa={:#010x} write={} readback={:?} -> {}",
        slot_ipa,
        ok_w,
        read_back,
        if ok { "OK" } else { "FAIL" },
    );
}
