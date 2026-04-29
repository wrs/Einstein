//! alrt-task TAlertEventHandler CList header writer-capture probe.
//!
//! Per `INVESTIGATION.md` "alrt-task DABT", the wedge at FAR=0xe336000c
//! traces back to a corrupted CList header at VA=0x0cca37c4 (= alrt
//! globals + 0x8c). The IdleProc probe captured the corrupted state:
//! count=32, esize=1, ebase=0x003121fc — values that look like an
//! APCS stack frame imprint from `MoveFreeBlock → bl SetFreeChain`.
//!
//! This module installs a stage-2 RO+XN trap on the 4-KiB page
//! containing VA=0x0cca37c4 (= page-aligned VA=0x0cca3000) so we can
//! capture every kernel write to that PA with `(PC, offset, value,
//! src_mode)`. The PA isn't known at boot time (the kernel allocates
//! the page later), so we arm *lazily*: the Prim Remember probe calls
//! `maybe_arm_for_va` on every install; the first call whose VA
//! page-aligns to TARGET_VA pins the resolved PA and arms RO+XN.
//!
//! Same shape as `g1_capture.rs`:
//! - `is_armed_pa(pa)` — fast hot-path test in `handle_data_abort`.
//! - `note_perm_fault(elr, ipa, value, srt)` — record one write,
//!   set rearm-pending so the next trap entry re-imposes RO+XN.
//! - `maybe_rearm()` — called once per trap from
//!   `trap_irq` / data-abort entry to restore RO+XN.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::kprintln;

/// 4-KiB page-aligned VA we want to watch. `0x0cca3000` contains the
/// alrt task's TAlertEventHandler at `0x0cca37a8` and the corrupted
/// CList header at `0x0cca37c4`.
const TARGET_VA: u32 = 0x0cca_3000;

/// Known-stable PA backing TARGET_VA across boots, per the prior
/// PLAN.md alias table:
///   `PA=0x0402e000  VA=0x0cc9b000 ↔ VA=0x0cca3000`
/// Used as a boot-time arm fallback so we catch writes that happen
/// BEFORE the first Prim Remember install of TARGET_VA. The dynamic
/// arm path (`maybe_arm_for_va`) re-confirms this PA at install time;
/// a mismatch is logged.
const KNOWN_TARGET_PA: u32 = 0x0402_e000;

/// PA backing TARGET_VA, captured at first Prim Remember install or
/// at stage2::init time (whichever fires first). `0` means
/// "not yet armed".
static TARGET_PA: AtomicU32 = AtomicU32::new(0);

/// Set after `note_perm_fault` so the next trap-entry rearm restores
/// RO+XN. Without this we'd miss every write after the first.
static REARM_PENDING: AtomicBool = AtomicBool::new(false);

/// Total trap count (regardless of whether the write hit the CList
/// window) and out-of-window count. Diagnostic — tells us at boot
/// end whether the trap was firing at all.
static TOTAL_TRAPS: AtomicU32 = AtomicU32::new(0);
static OUT_OF_WINDOW: AtomicU32 = AtomicU32::new(0);

/// Print the diagnostic counters. Called by the Reboot canary.
pub fn dump_counters() {
    kprintln!(
        "alrt-capture summary: armed_pa={:#010x} traps={} out_of_window={} budget_remaining={}",
        TARGET_PA.load(Ordering::Relaxed),
        TOTAL_TRAPS.load(Ordering::Relaxed),
        OUT_OF_WINDOW.load(Ordering::Relaxed),
        BUDGET.load(Ordering::Relaxed),
    );
}

/// Per-page log budget. The page lives in the kernel's task-globals /
/// stack region, so it sees a lot of legitimate writes too. With
/// boot-time arming we capture everything from cold boot through the
/// wedge, so the budget needs to be much higher than the dynamic-arm
/// case. Most writes are stack push/pop traffic — we filter to the
/// CList-header window (offsets 0x7c0..0x7e0) at log time to keep
/// the volume manageable.
const LOG_BUDGET: u32 = 4096;
static BUDGET: AtomicU32 = AtomicU32::new(LOG_BUDGET);

/// Offset window in the page where the alrt CList header lives.
/// `inner+0x8c` resolves to `clist=0x0cca37c4` ⇒ offset within the
/// 4-KiB page = `0x37c4 - 0x3000 = 0x7c4`. Plus a CList header is
/// ~0x20 bytes, plus a few entries, so capture writes in
/// `[0x7c0, 0x800)`. Writes outside this window are silently
/// auto-flipped to RW (logged at TRACE-equivalent level only).
const CLIST_OFFSET_LO: u32 = 0x7c0;
const CLIST_OFFSET_HI: u32 = 0x800;

/// Boot-time arm with the known-stable PA. Call from `stage2::init`
/// before the first guest ERET. Captures every write to PA from cold
/// boot, including pre-Prim writes that the dynamic arm misses.
///
/// SAFETY: same as `g1_capture::arm` — toggles stage-2 page
/// permissions; caller must hold the no-concurrent-writers
/// invariant. Single-threaded EL2 at init time satisfies this.
pub unsafe fn arm_at_boot() {
    let prev = TARGET_PA.compare_exchange(
        0, KNOWN_TARGET_PA, Ordering::AcqRel, Ordering::Relaxed,
    );
    if prev.is_err() {
        return;
    }
    let before = crate::stage2::ram_page_l3_entry(KNOWN_TARGET_PA);
    // SAFETY: helper performs its own TLB maintenance.
    unsafe { crate::stage2::set_ram_page_ro_xn(KNOWN_TARGET_PA); }
    let after = crate::stage2::ram_page_l3_entry(KNOWN_TARGET_PA);
    kprintln!(
        "alrt-capture: BOOT armed RO+XN on PA={:#010x} L3 before={:#x} after={:#x}",
        KNOWN_TARGET_PA, before.unwrap_or(0), after.unwrap_or(0),
    );
    // Dump initial RAM contents at the CList-header window. If these
    // bytes already match the corrupted state observed at IdleProc
    // (count=32, esize=1, ebase=0x003121fc) at this point, the
    // "corruption" is actually our hypervisor's RAM-init pattern —
    // we'd be looking for a hypervisor-side bug, not a guest one.
    kprintln!("alrt-capture: RAM at PA={:#010x}+0x7c0..0x800 at boot:",
        KNOWN_TARGET_PA);
    for i in 0..16u32 {
        let pa = KNOWN_TARGET_PA + 0x7c0 + i * 4;
        let v = crate::guest_mem::read_word_pa(pa).unwrap_or(0xDEAD_BEEF);
        kprintln!("    +{:#x}: {:#010x}", 0x7c0 + i*4, v);
    }
}

/// Called from the Prim Remember probe on every kernel install. If
/// the install's VA page-aligns to TARGET_VA and we haven't armed
/// yet, capture the resolved PA and impose RO+XN at stage-2. With
/// `arm_at_boot` already running this is now a sanity check that
/// the kernel really maps TARGET_VA → KNOWN_TARGET_PA (logs a
/// warning if it doesn't).
pub fn maybe_arm_for_va(va: u32, pa: u32) {
    if (va & !0xFFF) != TARGET_VA {
        return;
    }
    let pa_aligned = pa & !0xFFF;
    let prev = TARGET_PA.compare_exchange(
        0, pa_aligned, Ordering::AcqRel, Ordering::Relaxed,
    );
    if prev.is_err() {
        // Already armed. If the kernel re-mapped TARGET_VA to a
        // different PA, log it once — that itself would explain a
        // whole class of corruption.
        let cur = TARGET_PA.load(Ordering::Relaxed);
        if cur != pa_aligned {
            kprintln!(
                "alrt-capture: WARNING TARGET_VA={:#010x} re-mapped: was PA={:#010x}, now PA={:#010x}",
                TARGET_VA, cur, pa_aligned,
            );
        }
        return;
    }
    // Successful first arm. SAFETY: helper performs its own TLB
    // maintenance.
    unsafe { crate::stage2::set_ram_page_ro_xn(pa_aligned); }
    kprintln!(
        "alrt-capture: armed RO+XN on PA={:#010x} (covers VA={:#010x}, contains alrt CList at VA=0x0cca37c4)",
        pa_aligned, TARGET_VA,
    );
}

/// True iff `pa` (4-KiB-aligned) is the armed alrt CList page.
pub fn is_armed_pa(pa: u32) -> bool {
    let armed = TARGET_PA.load(Ordering::Relaxed);
    armed != 0 && (pa & !0xFFF) == armed
}

/// Record one captured write. `value` is `Some(u32)` only when the
/// faulting store's ISS encoded the source register (ISV=1); for
/// AArch32 STM / byte / half stores ISV=0 and we log `<isv0>`.
///
/// We always rearm + decrement budget, but only kprintln writes
/// hitting the CList-header window (offset 0x7c0..0x800). Writes
/// outside the window also need the rearm path because the kernel
/// performs many legitimate stores in this page (stack push/pop,
/// task-globals updates, etc.) — without rearming after every
/// trap, the page would stay RW after the first irrelevant write
/// and we'd miss the corruption when it does land in the window.
pub fn note_perm_fault(elr: u32, ipa: u32, value: Option<u32>, srt: u32, src_cpsr: u32) {
    if !is_armed_pa(ipa & !0xFFF) {
        return;
    }
    TOTAL_TRAPS.fetch_add(1, Ordering::Relaxed);
    REARM_PENDING.store(true, Ordering::Release);
    let off = ipa & 0xFFF;
    let in_window = off >= CLIST_OFFSET_LO && off < CLIST_OFFSET_HI;
    if !in_window {
        OUT_OF_WINDOW.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let prev = BUDGET.fetch_sub(1, Ordering::Relaxed);
    if prev == 0 {
        BUDGET.store(0, Ordering::Relaxed);
        return;
    }
    let mode = src_cpsr & 0x1F;
    match value {
        Some(v) => kprintln!(
            "alrt-capture[+{:#x}]: elr={:#010x} value={:#010x} srt={} src_mode={:#x}",
            off, elr, v, srt, mode,
        ),
        None => kprintln!(
            "alrt-capture[+{:#x}]: elr={:#010x} value=<isv0> srt={} src_mode={:#x}",
            off, elr, srt, mode,
        ),
    }
}

/// Re-arm if a prior trap captured a write. Called from trap entry
/// (mirrors the pattern used by `g1_capture::maybe_rearm`).
pub fn maybe_rearm() {
    let armed = TARGET_PA.load(Ordering::Relaxed);
    if armed == 0 {
        return;
    }
    if REARM_PENDING.swap(false, Ordering::Acquire) {
        // SAFETY: helper performs its own TLB maintenance.
        unsafe { crate::stage2::set_ram_page_ro_xn(armed); }
    }
}
