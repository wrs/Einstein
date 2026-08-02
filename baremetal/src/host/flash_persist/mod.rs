//! Persistent backing for the guest's internal-store flash.
//!
//! The flash bytes themselves live in `peripherals::flash::GUEST_FLASH`
//! (8 MiB, two banks). This module is the I/O layer that mirrors those
//! bytes to and from host storage so user data — soup entries,
//! settings, installed packages — survives cold boots, snapshot
//! invalidation, and ROM-fingerprint mismatches.
//!
//! ## Why a separate file from snapshots?
//!
//! `src/hv/snapshot.rs` saves full guest state (CPU + RAM + FB) but its
//! file is invalidated by ROM patches (fingerprint mismatch), VERSION
//! bumps, and `trace` toggles. Flash, in contrast, is user data — it
//! should outlive those events. Snapshots store a flash *fingerprint*
//! (not the bytes) so a snapshot resume can detect divergence between
//! the saved CPU/RAM state and the current persistent flash and fall
//! back to a cold boot.
//!
//! ## Backend selection
//!
//! `build.rs::resolve_flash_persist_backend` picks the active backend
//! from the `flash-persist-*` Cargo features (with "semihost" as the
//! no-features fallback) and emits one of `cfg(nh_flash_persist_*)`.
//! In `nh_guest_test` mode the resolver forces "null" so tests start
//! from a clean GUEST_FLASH.

#[cfg(nh_flash_persist_semihost)]
mod semihost;
#[cfg(nh_flash_persist_null)]
mod null;
#[cfg(nh_flash_persist_sd)]
mod sd;

use core::sync::atomic::{AtomicU64, Ordering};

/// Size of the guest-flash backing this layer persists (two 4 MiB
/// banks). The backing itself belongs to `peripherals::flash`;
/// [`set_backing`] cross-checks the registered store against this
/// constant so the two layers can't silently diverge.
pub const FLASH_SIZE: usize = 8 * 1024 * 1024;

/// Host physical base of the guest-flash backing store, registered by
/// `main.rs` boot wiring (`peripherals::flash::host_pa()`). 0 =
/// unregistered; [`backing_base`] halts loudly on use before wiring.
static BACKING_BASE: AtomicU64 = AtomicU64::new(0);

/// Register the guest-flash backing store. Called once from `main.rs`
/// before [`init`] / [`try_load`] run.
pub fn set_backing(base: u64, len: usize) {
    if len != FLASH_SIZE {
        crate::kprintln!(
            "*** flash_persist: registered backing is {} bytes, expected {} ***",
            len,
            FLASH_SIZE
        );
        crate::arch::cpu::halt();
    }
    BACKING_BASE.store(base, Ordering::Release);
}

/// Host physical base of the registered backing. Halts loudly if
/// `main.rs` never registered one — a boot wiring bug.
#[cfg(any(nh_flash_persist_semihost, nh_flash_persist_sd))]
fn backing_base() -> u64 {
    let base = BACKING_BASE.load(Ordering::Acquire);
    if base == 0 {
        crate::kprintln!(
            "*** flash_persist: no backing registered — main.rs must set_backing() before use ***"
        );
        crate::arch::cpu::halt();
    }
    base
}

/// Backend interface. Single-threaded EL2 callers; impls do not need
/// to be re-entrant.
pub trait FlashStore: Sync {
    /// Called once at boot, between `peripherals::flash::init()` (which
    /// seeds the DLDS/OSCD headers) and the ROM-REx checksum seeding.
    /// If a persistent store exists, overwrites `GUEST_FLASH` with its
    /// contents. No-op if the store is absent or wrong-sized.
    fn try_load(&self);

    /// Marks the 64 KiB blocks covered by `[off, off+len)` dirty.
    /// Hooked from `flash::program_word` (len=4) and
    /// `flash::erase_block` (len=erase size, typically 128 KiB).
    fn mark_dirty(&self, off: usize, len: usize);

    /// Persists any dirty blocks to the host store. Called from the
    /// snapshot autosave path; respects the same wall-clock gate.
    fn maybe_save(&self);

    /// FNV-1a-32 over the current `GUEST_FLASH` bytes. Stored in
    /// snapshot headers so a resume can verify the on-disk flash
    /// matches the state the snapshot was captured against.
    fn fingerprint(&self) -> u32;
}

#[cfg(nh_flash_persist_semihost)]
use self::semihost::BACKEND;
#[cfg(nh_flash_persist_null)]
use self::null::BACKEND;
#[cfg(nh_flash_persist_sd)]
use self::sd::BACKEND;

/// Backend-specific bring-up. SD backend uses this to construct the
/// SDHOST driver + VolumeManager before `try_load` runs. Other
/// backends are no-ops.
pub fn init() {
    #[cfg(nh_flash_persist_sd)]
    sd::init();
}

pub fn try_load() {
    BACKEND.try_load();
}

pub fn mark_dirty(off: usize, len: usize) {
    BACKEND.mark_dirty(off, len);
}

pub fn maybe_save() {
    BACKEND.maybe_save();
}

pub fn fingerprint() -> u32 {
    BACKEND.fingerprint()
}

/// Forwarded from `host_dma::on_completion(SD_TX_CHANNEL)` — advances
/// the SD backend's background DMA save state machine on each SD-TX
/// channel completion IRQ. No-op for backends without an async DMA save
/// (only the SD backend on real-hardware Pi builds owns that channel).
#[cfg(nh_real_hw)]
pub fn on_sd_dma_done() {
    #[cfg(nh_flash_persist_sd)]
    sd::on_dma_completion();
}
