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

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

/// Sample heap[+0]. If it changed since the last sample, log the
/// transition with the supplied source label and the EL2-saved
/// faulting address. Cheap enough to call from every trap entry; the
/// guest_mem walk is a few stage-1 page-table reads.
pub fn sample(elr_el2: u64, source: Source) {
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
        kprintln!(
            "heap-watch[{}] {}: heap[{:#010x}] {:#010x} -> {:#010x}  (elr={:#x}, prev-trap-elr={:#x})",
            n, source.label(), WATCH_VA, prev, value, elr_el2, prev_elr,
        );
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
}
