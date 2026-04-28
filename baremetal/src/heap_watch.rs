//! Forensic write-watch for newt's RelocHeap header.
//!
//! Phase B's current stop is content corruption of the heap struct at
//! IPA `0x0ca6b010` (the legitimate RelocHeap created by NewHeap call
//! #3 — see `INVESTIGATION.md`). NewHeap leaves `heap[+0] = 0x0ca6b000`
//! (the heap base); by the time SearchFreeList walks the freelist,
//! `heap[+0] = 0x002dd804` (a ROM PC inside `__ct__18TStoreObjectWriter`).
//! Some store along the boot path overwrites it.
//!
//! This module installs a sampler that reads `heap[+0]` from common
//! trap entry points and logs every value transition with the current
//! `ELR_EL2` and a short caller-source label. It does NOT trap stores
//! directly — that would need a stage-2 RO carve-out — but the
//! transition log is fine-grained enough to bisect the writer to a
//! function range.
//!
//! Lazy initialisation: `prev` starts at 0 (sentinel "never sampled").
//! The first sample populates it; thereafter every transition logs.
//! We cap kprintln volume at `LIMIT` transitions; after that the prev
//! pointer keeps tracking but stays silent.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::guest_mem;
use crate::kprintln;

/// IPA of newt's RelocHeap header (NewHeap #3 returns r5+16 where
/// r5=0x0ca6b000). The full header is 128 bytes; we only sample the
/// first word — that's enough to detect the corruption transition
/// from `0x0ca6b000` → `0x002dd804` documented in `INVESTIGATION.md`.
const WATCH_VA: u32 = 0x0ca6_b010;

/// Last value seen at WATCH_VA. 0 means "never sampled" (the heap
/// region is zero-initialised by stage-2 default before NewHeap runs).
static PREV: AtomicU32 = AtomicU32::new(0);

/// `ELR_EL2` at the most recent successful sample — i.e. the trap PC
/// where heap[+0] was last observed to be `PREV`. When a transition
/// fires, this tells us the trap-to-trap window in which the
/// corrupting store happened.
static PREV_ELR: AtomicU64 = AtomicU64::new(0);

/// Number of transitions observed (logged-or-not). The kprintln gate
/// uses `<LIMIT` to keep the log tractable on a long boot.
static TRANSITIONS: AtomicU32 = AtomicU32::new(0);

/// Maximum number of transitions to log. After this we still update
/// PREV so the diagnostic stays accurate, but we go silent.
const LIMIT: u32 = 32;

/// Ring buffer of recent trap ELRs. On a transition we dump the
/// buffer so the developer can see the trap stream that led up to
/// the corruption — much narrower than the global 500-entry trap log
/// budget that's already exhausted by mid-boot.
///
/// Each slot packs the source kind into bit 63 (1 = irq, 0 = sync) so
/// the dump can disambiguate IRQ-during-loop noise from a sync trap
/// inside the trampoline body. ELRs in our config never use bit 63,
/// so there's no aliasing risk.
const RING_SIZE: usize = 32;
const RING_SRC_IRQ_BIT: u64 = 1u64 << 63;
static RING: [AtomicU64; RING_SIZE] = [const { AtomicU64::new(0) }; RING_SIZE];
static RING_HEAD: AtomicUsize = AtomicUsize::new(0);

/// Source-kind tag matching the encoding bit packed into ring slots.
#[derive(Copy, Clone)]
pub enum Source {
    Sync,
    Irq,
}

impl Source {
    fn label(self) -> &'static str {
        match self { Source::Sync => "sync", Source::Irq => "irq" }
    }
    fn bit(self) -> u64 {
        match self { Source::Sync => 0, Source::Irq => RING_SRC_IRQ_BIT }
    }
}

// ----------------------------------------------------------------------
// Heap-header sanity check.
//
// `NewHeap` at ROM 0x00310e24 initialises a Newton heap header with
// these load-bearing invariants (offsets relative to the header VA):
//
//   heap[+0x00]  = heap - 16        (base of the heap's block area)
//   heap[+0x08]  = 0x736b6961       ("skia" little-endian magic, from
//                                    the ROM literal at 0x00310f34)
//   heap[+0x0C]  = heap             (self-pointer, set at 0x00310ec4)
//   heap[+0x10]  = heap             (self-pointer, set at 0x00310ed0)
//   heap[+0x40]  = 0x40             (constant 64, set at 0x00310f10)
//
// All five hold on a freshly-initialised heap. The wedge corruption
// (heap[+0]=0x002dd804, heap[+0x10]=0, etc.) breaks four of them on
// first observation, so a multi-field check catches it as reliably
// as a single-field watch and gives a clean "halt on first sign of
// corruption" tripwire.

/// Result of one sanity probe. Returns None when all invariants hold.
///
/// Only the two invariants the kernel never legitimately mutates are
/// checked. `heap[+0xC]` and `heap[+0x10]` are also self-pointers at
/// init but the kernel re-uses them as "next heap" / "free-list
/// owner" links during normal operation, so they false-positive
/// against working code.
pub fn check_heap_sanity(heap_va: u32) -> Option<(&'static str, u32, u32)> {
    let read = |off: u32| guest_mem::read_word_va(heap_va.wrapping_add(off));
    let base = read(0)?;
    let want_base = heap_va.wrapping_sub(16);
    if base != want_base {
        return Some(("heap[+0x00] != heap-16 (base)", base, want_base));
    }
    let magic = read(8)?;
    if magic != 0x736b_6961 {
        return Some(("heap[+0x08] != 'skia' magic", magic, 0x736b_6961));
    }
    None
}

// ----------------------------------------------------------------------
// Stage-2 RO carve-out at the RelocHeap header's PA.
//
// Once we know the PA backing VA 0x0ca6b000 (one stage-1 walk at the
// first SetCurrentHeap call with r0 = 0x0ca6b010), we mark that 4 KiB
// page RO at stage-2. Any guest store to it takes a stage-2
// permission fault that lands in `handle_data_abort`, where we log
// the writer's ELR + IPA + value, flip the page to RW so the CPU's
// retry succeeds, and arm a re-RO at the very next trap. The next
// guest store re-faults, gives us another writer-PC log line, and so
// on. The kernel sees no behavioural change — every write completes
// natively, just one trap each.
//
// 64-line log cap keeps a long boot tractable while still showing
// the first dozen writers, which is more than enough to localise the
// corrupting store.

/// 4 KiB-aligned PA (= IPA in stage-2) currently armed RO. 0 = none.
static CARVED_PA: AtomicU32 = AtomicU32::new(0);
static REARM_PENDING: AtomicBool = AtomicBool::new(false);
static PERM_FAULT_HITS: AtomicU32 = AtomicU32::new(0);
const PERM_FAULT_LOG_LIMIT: u32 = 256;

/// Walk the guest stage-1 (rooted at the architectural TTBR0 = guest
/// PA 0x0400_0000) for `va` and log the L1 / optional L2 entries
/// decoded into "section vs coarse" + AP[2:0] + Domain. Exists so the
/// post-rebind silence on the heap carve-out can be diagnosed:
/// stage-1 RO at user mode would route writes through stage-1 DABT
/// before stage-2 sees them, bypassing the carve-out.
///
/// Short-descriptor format (ARMv7-A B3.5.1):
/// - L1 section bits[1:0]=10. Domain = bits[8:5]. AP[2,1,0] in
///   bits[15,11:10]; AP[2]=APX, AP[1:0]=AP. nG = bit 17.
/// - L1 coarse bits[1:0]=01. Domain = bits[8:5]. PA = bits[31:10].
/// - L2 small page bits[1:0]=10. AP[2,1,0] in bits[9,5:4].
/// - L2 large page bits[1:0]=01. AP[2,1,0] in bits[9,5:4].
fn log_stage1_walk(va: u32) {
    let l1_idx = (va >> 20) as usize;
    let l1_pa = 0x0400_0000u32 + (l1_idx as u32) * 4;
    let l1 = match guest_mem::read_word_pa(l1_pa) {
        Some(v) => v,
        None => {
            kprintln!("    stage-1 walk VA={:#010x}: L1 read at PA={:#x} failed", va, l1_pa);
            return;
        }
    };
    let l1_kind = l1 & 3;
    match l1_kind {
        2 => {
            let domain = (l1 >> 5) & 0xF;
            let ap2 = (l1 >> 15) & 1;
            let ap10 = (l1 >> 10) & 0x3;
            let ng = (l1 >> 17) & 1;
            kprintln!(
                "    stage-1 walk VA={:#010x}: L1[{:#x}]={:#010x} (section, domain={}, AP=[{}{:02b}], nG={}, PA={:#010x})",
                va, l1_idx, l1, domain, ap2, ap10, ng, l1 & 0xFFF0_0000,
            );
        }
        1 => {
            let domain = (l1 >> 5) & 0xF;
            let l2_pa_base = l1 & 0xFFFF_FC00;
            let l2_idx = (va >> 12) & 0xFF;
            let l2_addr = l2_pa_base + l2_idx * 4;
            let l2 = match guest_mem::read_word_pa(l2_addr) {
                Some(v) => v,
                None => {
                    kprintln!(
                        "    stage-1 walk VA={:#010x}: L1[{:#x}]={:#010x} (coarse, domain={}, L2 PA={:#x}, L2 read failed)",
                        va, l1_idx, l1, domain, l2_addr,
                    );
                    return;
                }
            };
            let l2_kind = l2 & 3;
            let ap2 = (l2 >> 9) & 1;
            let ap10 = (l2 >> 4) & 0x3;
            let xn = (l2 >> 0) & 1;
            let kind_str = match l2_kind {
                1 => "large",
                2 | 3 => "small",
                _ => "fault",
            };
            let pa_field = match l2_kind {
                1 => l2 & 0xFFFF_0000,
                2 | 3 => l2 & 0xFFFF_F000,
                _ => 0,
            };
            kprintln!(
                "    stage-1 walk VA={:#010x}: L1[{:#x}]={:#010x} (coarse, domain={}); L2[{:#x}]={:#010x} ({} page, AP=[{}{:02b}], XN={}, PA={:#010x})",
                va, l1_idx, l1, domain, l2_idx, l2, kind_str, ap2, ap10, xn, pa_field,
            );
        }
        _ => {
            kprintln!(
                "    stage-1 walk VA={:#010x}: L1[{:#x}]={:#010x} (kind={}, fault)",
                va, l1_idx, l1, l1_kind,
            );
        }
    }
}

/// Translate VA 0x0ca6b000 to its current guest PA via the kernel's
/// stage-1 tables, then install a stage-2 RO carve-out on the
/// containing 4 KiB page. Idempotent on subsequent calls — we only
/// arm once. Returns Some(pa) on the install call, None if already
/// armed or stage-1 translation failed.
pub fn arm_carve_out_at_heap_va(va: u32) -> Option<u32> {
    if CARVED_PA.load(Ordering::Relaxed) != 0 {
        return None; // already armed
    }
    let pa = guest_mem::translate_va(va)?;
    let page = pa & !0xFFF;
    CARVED_PA.store(page, Ordering::Release);
    // SAFETY: stage-2 helper handles its own TLB maintenance.
    unsafe { crate::stage2::set_ram_page_ro_x(page); }
    kprintln!(
        "heap-watch: armed stage-2 RO carve-out — VA={:#010x} → PA={:#010x} (page={:#010x})",
        va, pa, page
    );
    Some(page)
}

/// True if `ipa` falls in the 4 KiB page currently armed RO.
pub fn is_carved_out_ipa(ipa: u32) -> bool {
    let armed = CARVED_PA.load(Ordering::Relaxed);
    armed != 0 && (ipa & !0xFFF) == armed
}

/// 4 KiB-aligned PA currently armed RO, or 0 if none. Used by the
/// trap handler's debug arm to log all DABTs on the watched page,
/// regardless of fault class.
pub fn carved_pa() -> u32 {
    CARVED_PA.load(Ordering::Relaxed)
}

/// Log a stage-2 perm fault on the carve-out and request the page be
/// re-armed RO at the next trap. The caller should flip the page to
/// RW (so the guest's retry succeeds) and return without advancing
/// ELR; the existing shadow-stub `set_ram_page_rw_xn` path does this.
///
/// Only writes hitting the heap header (first 0x80 bytes of the
/// 4 KiB page, where the corruption lives) are logged. The rest of
/// the page is the heap's freelist / data area and produces a sea
/// of noise. Writes with `value=0x002dd804` (the wedge signature)
/// are always logged regardless of where they land.
pub fn note_perm_fault_on_carve_out(
    elr: u32, ipa: u32, value: Option<u32>, isv1: bool, srt: u32,
) {
    let n = PERM_FAULT_HITS.fetch_add(1, Ordering::Relaxed);
    REARM_PENDING.store(true, Ordering::Release);
    if n >= PERM_FAULT_LOG_LIMIT {
        return;
    }
    let is_wedge_value = matches!(
        value,
        Some(0x002d_d804) | Some(0x001a_48f0) | Some(0x002d_fa20) | Some(0x002d_d7c4)
    );
    let armed = CARVED_PA.load(Ordering::Relaxed);
    let heap_off = ipa.wrapping_sub(armed.wrapping_add(0x10));
    match value {
        Some(v) => kprintln!(
            "heap-watch perm-fault[{:>4}]: ipa={:#010x} (heap[{:+#x}]) elr={:#010x} value={:#010x} srt={} (isv1={}){}",
            n, ipa, heap_off as i32, elr, v, srt, isv1,
            if is_wedge_value { "  *** WEDGE VALUE ***" } else { "" },
        ),
        None => kprintln!(
            "heap-watch perm-fault[{:>4}]: ipa={:#010x} (heap[{:+#x}]) elr={:#010x} value=<isv0 — undecoded> (isv1={})",
            n, ipa, heap_off as i32, elr, isv1,
        ),
    }
}

/// Re-arm the carve-out RO if a perm-fault path requested it, and
/// follow the VA across stage-1 remaps so the carve-out stays
/// attached to the heap header even when the kernel rebinds the VA
/// to a fresh PA. The latter is essential: empirically the heap VA
/// 0x0ca6b000 hops from PA 0x0401f000 to PA 0x04032000 partway
/// through boot, and a fixed-PA carve-out misses every write after
/// the rebind.
///
/// Called from every trap entry. Cheap when the VA hasn't moved
/// (one stage-1 walk).
fn maybe_rearm() {
    let armed = CARVED_PA.load(Ordering::Relaxed);
    if armed == 0 {
        return;
    }

    // Detect VA → PA rebind. If the current PA differs from the
    // armed one, release the old page (flip RW so the kernel can
    // recycle it) and arm the new page RO. Logged once per rebind.
    let pa_now = match guest_mem::translate_va(WATCH_VA) {
        Some(p) => p & !0xFFF,
        None => 0,
    };
    if pa_now != 0 && pa_now != armed {
        // SAFETY: stage-2 helpers handle TLB maintenance.
        unsafe {
            crate::stage2::set_ram_page_rw_xn(armed);
            crate::stage2::set_ram_page_ro_x(pa_now);
            // Defensive: nuke every TLB entry, both stages, EL1+EL2.
            // The per-IPA tlbi inside set_ram_page_ro_x ought to be
            // sufficient, but the post-rebind silence persists, so
            // hammer the whole thing to rule out a cached stale RW
            // entry. This costs us a TLB miss flurry; acceptable for
            // diagnostic.
            core::arch::asm!(
                "dsb ish",
                "tlbi vmalls12e1is",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
        CARVED_PA.store(pa_now, Ordering::Release);
        let l3 = crate::stage2::ram_page_l3_entry(pa_now).unwrap_or(0xDEADBEEF_DEADBEEF);
        kprintln!(
            "heap-watch: VA {:#010x} rebound — old PA={:#010x} → new PA={:#010x} (L3={:#018x})",
            WATCH_VA, armed, pa_now, l3,
        );
        return;
    }

    if REARM_PENDING.swap(false, Ordering::Acquire) {
        // SAFETY: helper handles its own TLB maintenance.
        unsafe { crate::stage2::set_ram_page_ro_x(armed); }
    }
}

/// Sample heap[+0]. If it changed since the last sample, log the
/// transition with the supplied source label and the EL2-saved
/// faulting address. Cheap enough to call from every trap entry; the
/// guest_mem walk is a few stage-1 page-table reads.
/// PC ranges where the kernel legitimately mutates a heap header
/// transiently (NewHeap init, allocator inner loops, semaphore-glue
/// at SetCurrentHeap). The sanity check skips when ELR is inside one
/// of these so we don't false-positive during a partial update.
///
/// This is intentionally narrow — only the actual allocator code
/// ranges, not trap trampolines or stub pools (which are valid
/// observation points for "the kernel just left heap code, before
/// we re-enter it").
fn elr_inside_heap_code(elr: u32) -> bool {
    // NewHeap body, SetCurrentHeap, NewHandle/HLock/HUnlock, etc.
    if (0x0014_0000..0x0014_8000).contains(&elr) { return true; }
    // CompactHeap / SearchFreeList / JumpBlock / NewBlock / freelist ops.
    if (0x0031_0000..0x0032_0000).contains(&elr) { return true; }
    false
}

/// True iff a sanity-check failure has already been reported.
/// Latches so we halt cleanly on the first trip-wire and don't
/// flood the log if the same heap is read multiple times.
static SANITY_FIRED: AtomicU32 = AtomicU32::new(0);

pub fn sample(elr_el2: u64, source: Source) {
    // Defensive RO-state poll: if some other code path has flipped
    // the carved page to RW without our knowledge (e.g. shadow_stub
    // claiming the page as a code page after a fetch trap on it),
    // log it loudly. AP=0b01 is encoded in bits[7:6] = 0x40 of the
    // L3 entry. Anything other than that on the armed page is
    // suspect.
    {
        let armed = CARVED_PA.load(Ordering::Relaxed);
        if armed != 0 {
            if let Some(l3) = crate::stage2::ram_page_l3_entry(armed) {
                if (l3 & (3 << 6)) != (1 << 6) {
                    static REPORTED: AtomicU32 = AtomicU32::new(0);
                    let n = REPORTED.fetch_add(1, Ordering::Relaxed);
                    if n < 64 {
                        kprintln!(
                            "heap-watch: !!! armed PA {:#010x} is NOT RO at sample (L3={:#018x}) src={}",
                            armed, l3, source.label(),
                        );
                    }
                }
            }
        }
    }

    // Re-arm the stage-2 carve-out (if any) one trap after a perm
    // fault flipped its page to RW. Doing it here means the guest
    // retried the faulting store under RW, the store landed, and now
    // the next store will trigger another fault → another log line.
    maybe_rearm();

    // Always record this ELR + source-bit in the ring buffer, even
    // when the value hasn't changed.
    let slot = (elr_el2 & !RING_SRC_IRQ_BIT) | source.bit();
    let head = RING_HEAD.fetch_add(1, Ordering::Relaxed);
    RING[head % RING_SIZE].store(slot, Ordering::Relaxed);

    let value = match guest_mem::read_word_va(WATCH_VA) {
        Some(v) => v,
        None => return, // VA not mapped under current task — skip.
    };
    let prev = PREV.load(Ordering::Relaxed);
    if value == prev {
        // No transition; just record this trap's ELR so the next
        // change reports a tight window.
        PREV_ELR.store(elr_el2, Ordering::Relaxed);
        return;
    }
    let n = TRANSITIONS.fetch_add(1, Ordering::Relaxed);
    if n < LIMIT {
        let prev_elr = PREV_ELR.load(Ordering::Relaxed);
        let pa_now = guest_mem::translate_va(WATCH_VA).unwrap_or(0);
        let armed_pa = CARVED_PA.load(Ordering::Relaxed);
        let pa_match = if armed_pa == 0 {
            "(no carve-out armed)"
        } else if (pa_now & !0xFFF) == armed_pa {
            "(carve-out PA matches)"
        } else {
            "(*** PA REMAPPED — carve-out now stale ***)"
        };
        let l3 = if armed_pa != 0 {
            crate::stage2::ram_page_l3_entry(armed_pa).unwrap_or(0xDEAD_BEEF_DEAD_BEEF)
        } else { 0 };
        kprintln!(
            "heap-watch[{}] {}: heap[{:#010x}] {:#010x} -> {:#010x}  (elr={:#x}, prev-trap-elr={:#x}, pa_now={:#010x}, armed={:#010x} {} L3={:#018x})",
            n, source.label(), WATCH_VA, prev, value, elr_el2, prev_elr,
            pa_now, armed_pa, pa_match, l3,
        );
        log_stage1_walk(WATCH_VA);
        // Dump the ring buffer in chronological order so the operator
        // can see the trap stream that led to this transition. The
        // newest entry is at `head` (just stored above); the oldest is
        // `head + 1`. Walk forward by index.
        let next_head = head.wrapping_add(1);
        for i in 0..RING_SIZE {
            let idx = next_head.wrapping_add(i) % RING_SIZE;
            let raw = RING[idx].load(Ordering::Relaxed);
            if raw != 0 {
                let src = if (raw & RING_SRC_IRQ_BIT) != 0 { "irq " } else { "sync" };
                let e = raw & !RING_SRC_IRQ_BIT;
                kprintln!("    ring[{:>2}] {}: elr={:#x}", i, src, e);
            }
        }
    }
    PREV.store(value, Ordering::Relaxed);
    PREV_ELR.store(elr_el2, Ordering::Relaxed);

    // Multi-field heap-header sanity check. Skipped when ELR is
    // inside a heap-allocator function (the fields can be transiently
    // out-of-spec while NewHeap / NewHandle / SetCurrentHeap / etc.
    // update them). Skipped before NewHeap runs (PREV still 0). Halts
    // on the first trip-wire so the operator can see WHICH trap
    // observed the corruption — much tighter than waiting for
    // SearchFreeList to wedge.
    if SANITY_FIRED.load(Ordering::Relaxed) != 0 {
        return;
    }
    if elr_inside_heap_code(elr_el2 as u32) {
        return;
    }
    if PREV.load(Ordering::Relaxed) == 0 {
        return; // pre-NewHeap; the heap doesn't exist yet
    }
    if let Some((reason, got, want)) = check_heap_sanity(WATCH_VA) {
        SANITY_FIRED.store(1, Ordering::Relaxed);
        kprintln!(
            "*** heap-watch sanity FAIL: {} (got={:#010x} want={:#010x})",
            reason, got, want,
        );
        kprintln!(
            "    at trap source={} elr={:#x} heap-VA={:#010x}",
            source.label(), elr_el2, WATCH_VA,
        );
        let armed = CARVED_PA.load(Ordering::Relaxed);
        let pa_now = guest_mem::translate_va(WATCH_VA).unwrap_or(0);
        kprintln!(
            "    pa_now={:#010x} armed={:#010x}", pa_now, armed,
        );
        // Dump the first 0x40 bytes of the heap header for context.
        for off in (0..0x40u32).step_by(16) {
            let mut row = [0u32; 4];
            for i in 0..4u32 {
                row[i as usize] = guest_mem::read_word_va(
                    WATCH_VA.wrapping_add(off + i * 4)
                ).unwrap_or(0xDEADBEEF);
            }
            kprintln!(
                "      heap[+{:#04x}]  {:#010x} {:#010x} {:#010x} {:#010x}",
                off, row[0], row[1], row[2], row[3],
            );
        }
        // Dump the trap-stream ring buffer (newest at index 31) so
        // the operator can bisect the corrupting writer to between
        // two adjacent ring entries. The ring is updated at the top
        // of every sample call, so it includes this trap.
        let head = RING_HEAD.load(Ordering::Relaxed);
        let next_head = head.wrapping_add(1);
        for i in 0..RING_SIZE {
            let idx = next_head.wrapping_add(i) % RING_SIZE;
            let raw = RING[idx].load(Ordering::Relaxed);
            if raw != 0 {
                let src = if (raw & RING_SRC_IRQ_BIT) != 0 { "irq " } else { "sync" };
                let e = raw & !RING_SRC_IRQ_BIT;
                kprintln!("      ring[{:>2}] {}: elr={:#x}", i, src, e);
            }
        }
        // Log-only mode for this iteration: don't halt, so the boot
        // continues into the SBA-stub-pool wedge where the dabt-trip
        // handler can decode the stub's slot-14 back-branch and tell
        // us which ROM PC the stub emulates. Re-enable the halt once
        // the cascade source is identified.
        kprintln!(
            "    (sanity FAIL latched; continuing boot to expose downstream wedge)"
        );
    }
}
