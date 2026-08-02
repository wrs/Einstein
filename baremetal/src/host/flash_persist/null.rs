//! No-op flash-persistence backend.
//!
//! Used in `nh_guest_test` mode (tests want hermetic startup) and
//! when no semihosting / hardware backend is enabled. `fingerprint()`
//! returns a constant so snapshot headers can still carry the field
//! without divergence.

use super::FlashStore;

pub struct NullBackend;

impl FlashStore for NullBackend {
    fn try_load(&self) {}
    fn mark_dirty(&self, _off: usize, _len: usize) {}
    fn maybe_save(&self) {}
    fn fingerprint(&self) -> u32 {
        0
    }
}

pub static BACKEND: NullBackend = NullBackend;
