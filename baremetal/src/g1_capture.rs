//! Group-1 kernel-globals self-map capture probe.
//!
//! `verify-mmu` reports 3 stable Group-1 aliases throughout boot:
//!
//!   PA=0x04004000  VA=0x0c000000 ↔ VA=0x0c002000  (L1[0xc0],L2[0x0/0x2])
//!   PA=0x04005000  VA=0x0c003000 ↔ VA=0x0c004000
//!   PA=0x04006000  VA=0x0c007000 ↔ VA=0x0c008000
//!
//! These PAs back the guest's stage-1 L2 page-tables for kernel-VA
//! sections. The kernel installs entries within those L2 pages that
//! point back at the L2 page itself, exposing the L2 PT bytes through
//! kernel-VA windows so it can edit L2 entries via direct CPU stores.
//! Under ARMv4 subpage AP each self-map slot owned its own subpage;
//! under our flat AP=011 both slots become RW aliases to the same PA.
//!
//! These writes go through direct kernel L2 stores at TTBR0 setup
//! time and bypass the entire Remember/Prim layer, so the prior probe
//! iterations don't see them. This module installs a stage-2 RO+XN
//! mapping on the 3 PAs at boot. Each guest-side write traps to EL2
//! as a permission-fault; the handler logs `(elr, ipa, value, srt,
//! isv1)`, then the data-abort path's existing auto-flip-to-RW lets
//! the write complete natively. A re-arm hook fires on every trap
//! entry to re-impose RO so subsequent writes also fault and log.
//!
//! Goal: capture the (PC, offset, value) triples that produce the
//! 3 self-map aliases. Once captured, decide a fix layer in the
//! next iteration:
//!   (a) Einstein-port behaviour for the writer PCs;
//!   (b) ROM patch redirecting the self-map writes;
//!   (c) hypervisor-synthesised second mapping (write distinct PA
//!       values into the L2 entries so the guest sees the same byte
//!       values without underlying alias).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};


/// The three Group-1 PAs (kernel-globals self-mapping). Held as a
/// fixed array so `is_armed_pa` is a 3-comparison hot path.
const G1_PAS: [u32; 3] = [0x0400_4000, 0x0400_5000, 0x0400_6000];

/// Per-page rearm-pending flag. `note_perm_fault` sets the matching
/// flag so the next call to `maybe_rearm` re-imposes RO+XN on the
/// page that just took (and resolved) a write fault. Without this
/// the page would stay RW after the first capture and we'd miss
/// every subsequent self-map write.
static REARM_PENDING: [AtomicBool; 3] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

/// Whether `arm()` has run. The data-abort handler short-circuits
/// before the auto-flip-to-RW path when a fault hits one of the
/// armed PAs; if we haven't armed yet, a stray match against the
/// G1_PAS array shouldn't be treated as a "captured write".
static ARMED: AtomicBool = AtomicBool::new(false);

/// Per-page log budget (writes captured before we go silent on this
/// page). Boot writes thousands of L2 entries through these pages,
/// but only a small number of distinct writer PCs do the *self-map*
/// installations. Cap each page at 64 captures to keep the trace
/// volume manageable.
const LOG_BUDGET_PER_PAGE: u32 = 64;
static PAGE_BUDGET: [AtomicU32; 3] = [
    AtomicU32::new(LOG_BUDGET_PER_PAGE),
    AtomicU32::new(LOG_BUDGET_PER_PAGE),
    AtomicU32::new(LOG_BUDGET_PER_PAGE),
];

/// Mark the 3 Group-1 PAs RO+XN at stage-2. Call from
/// `stage2::init()` after the per-RAM-page L3 table has been built —
/// the helper assumes the descriptor for each target IPA already
/// exists and is currently RW.
///
/// SAFETY: Toggles stage-2 page permissions; caller must hold the
/// usual stage-2 invariants (no concurrent writers). We invoke this
/// from kmain before the first ERET to the guest, so single-threaded.
pub unsafe fn arm() {
    for pa in G1_PAS {
        // SAFETY: helper performs its own TLB invalidation.
        unsafe { crate::stage2::set_ram_page_ro_xn(pa); }
    }
    ARMED.store(true, Ordering::Release);
    crate::log_mmu!(
        "g1-capture: armed RO+XN on PA={:#010x}, {:#010x}, {:#010x}",
        G1_PAS[0], G1_PAS[1], G1_PAS[2],
    );
}

/// True iff the 4-KiB-aligned PA is one of the armed Group-1 pages
/// (and `arm()` has been called).
pub fn is_armed_pa(pa: u32) -> bool {
    if !ARMED.load(Ordering::Relaxed) {
        return false;
    }
    let page = pa & !0xFFF;
    G1_PAS.iter().any(|&p| p == page)
}

/// Index of `pa` in `G1_PAS`, or None.
fn page_idx(pa: u32) -> Option<usize> {
    let page = pa & !0xFFF;
    G1_PAS.iter().position(|&p| p == page)
}

/// Record one captured write to a Group-1 page. Called from
/// `handle_data_abort` after it has determined the fault is a stage-2
/// permission fault on RAM. Logs `(elr, ipa, offset, value, srt,
/// isv1)` and sets the matching page's rearm-pending flag so the
/// next `maybe_rearm()` call re-imposes RO+XN.
///
/// `value` is `Some(u32)` when ISV=1 (the architectural ISS encoded
/// the source register and we read `ctx.x[srt]`). For ISV=0 stores
/// (most AArch32 STMDA / STMDB / STM-with-rotation, plus byte/half
/// stores), the value isn't recoverable from the trap — log
/// `<isv0>` so post-processing knows to disambiguate from the PC.
pub fn note_perm_fault(elr: u32, ipa: u32, value: Option<u32>, srt: u32) {
    let Some(idx) = page_idx(ipa) else { return; };
    REARM_PENDING[idx].store(true, Ordering::Release);
    let prev = PAGE_BUDGET[idx].fetch_sub(1, Ordering::Relaxed);
    if prev == 0 {
        // Underflow guard: don't log forever. Restore to 0 (saturating).
        PAGE_BUDGET[idx].store(0, Ordering::Relaxed);
        return;
    }
    let off = ipa & 0xFFF;
    match value {
        Some(v) => crate::log_mmu!(
            "g1-capture[PA={:#010x} +{:#x}]: elr={:#010x} value={:#010x} srt={}",
            G1_PAS[idx], off, elr, v, srt,
        ),
        None => crate::log_mmu!(
            "g1-capture[PA={:#010x} +{:#x}]: elr={:#010x} value=<isv0> srt={}",
            G1_PAS[idx], off, elr, srt,
        ),
    }
}

/// Re-arm any Group-1 pages whose rearm-pending flag is set. Called
/// from trap entry — once per trap. The data-abort handler's existing
/// auto-flip-to-RW path runs before us (so the offending write
/// completes natively); we restore RO+XN here so the *next* write
/// to that page also faults.
pub fn maybe_rearm() {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    for (i, pa) in G1_PAS.iter().enumerate() {
        if REARM_PENDING[i].swap(false, Ordering::Acquire) {
            // SAFETY: helper performs its own TLB maintenance.
            unsafe { crate::stage2::set_ram_page_ro_xn(*pa); }
        }
    }
}
