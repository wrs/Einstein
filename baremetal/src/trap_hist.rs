//! Trap-frequency histograms for idle/wedge diagnostics.
//!
//! Three rolling counters reset after every periodic dump
//! (`trap_irq` calls `dump_and_reset()` every ~2 s of wall time):
//!
//! - **EC histogram** (64 atomic slots) — `ESR_EL2.EC` of every
//!   guest sync trap. Tells you whether the rate is dominated by
//!   data aborts, HVCs, CP15 trap-on-VM, etc.
//! - **HVC immediate histogram** (256 atomic slots + overflow) —
//!   sub-bucketing of `EC=0x12`. Pins down which probe / UND-tramp /
//!   align-fault path is hot.
//! - **DABT top-K** (Misra-Gries, 16 slots each for guest PC and IPA) —
//!   bounded approximate top of (a) which guest PCs are generating data
//!   aborts, and (b) which IPAs are being hit. Counts are lower bounds
//!   on the true frequency; the slack is at most `total / 16`.
//!
//! Because each dump resets the counters, the window reflects roughly
//! the previous 2 s — a snapshot of "what's hot right now" rather than
//! a since-boot cumulative tally, which makes idle composition easy
//! to read.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::hvc_imm::HvcImm;
use crate::kprintln;

const EC_BUCKETS: usize = 64;
const HVC_BUCKETS: usize = 256;
const TOPK: usize = 16;
const PRINT_TOP: usize = 8;

static EC_HIST: [AtomicU64; EC_BUCKETS] = {
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; EC_BUCKETS]
};
static HVC_IMM_HIST: [AtomicU64; HVC_BUCKETS] = {
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; HVC_BUCKETS]
};
static HVC_HIGH_HIST: AtomicU64 = AtomicU64::new(0);

/// Record a sync trap by its `ESR_EL2.EC`. Called once per guest sync
/// trap at the top of the dispatcher.
pub fn record_sync(ec: u32) {
    EC_HIST[(ec & 0x3f) as usize].fetch_add(1, Ordering::Relaxed);
}

/// Record an `HVC #imm` by its 16-bit immediate.
pub fn record_hvc(imm: u32) {
    if imm < HVC_BUCKETS as u32 {
        HVC_IMM_HIST[imm as usize].fetch_add(1, Ordering::Relaxed);
    } else {
        HVC_HIGH_HIST.fetch_add(1, Ordering::Relaxed);
    }
}

/// Single-threaded Misra-Gries top-K tracker. Trap dispatch is core-0
/// only, so plain `static mut` with a single critical section is fine.
struct TopK {
    keys: [u32; TOPK],
    counts: [u64; TOPK],
}

impl TopK {
    const fn new() -> Self {
        Self { keys: [0; TOPK], counts: [0; TOPK] }
    }

    fn record(&mut self, key: u32) {
        for i in 0..TOPK {
            if self.counts[i] > 0 && self.keys[i] == key {
                self.counts[i] = self.counts[i].saturating_add(1);
                return;
            }
        }
        for i in 0..TOPK {
            if self.counts[i] == 0 {
                self.keys[i] = key;
                self.counts[i] = 1;
                return;
            }
        }
        for c in &mut self.counts {
            *c = c.saturating_sub(1);
        }
    }

    fn snapshot_sorted(&self) -> [(u32, u64); TOPK] {
        let mut out = [(0u32, 0u64); TOPK];
        for i in 0..TOPK {
            out[i] = (self.keys[i], self.counts[i]);
        }
        for k in 0..TOPK {
            let mut best = k;
            for j in (k + 1)..TOPK {
                if out[j].1 > out[best].1 {
                    best = j;
                }
            }
            out.swap(k, best);
        }
        out
    }

    fn reset(&mut self) {
        for i in 0..TOPK {
            self.keys[i] = 0;
            self.counts[i] = 0;
        }
    }
}

static mut DABT_PC: TopK = TopK::new();
static mut DABT_IPA: TopK = TopK::new();

/// Record a data abort by `(guest PC, IPA)`. Called from
/// `handle_data_abort` after ELR/IPA have been read.
pub fn record_dabt(elr_pc: u32, ipa: u32) {
    // SAFETY: trap dispatch is single-threaded on core 0; the only
    // references to these statics are taken here and in
    // `dump_and_reset`, which can't overlap.
    unsafe {
        (*addr_of_mut!(DABT_PC)).record(elr_pc);
        (*addr_of_mut!(DABT_IPA)).record(ipa);
    }
}

/// Snapshot every counter, print the top entries, and zero everything
/// so the next dump shows a fresh window.
pub fn dump_and_reset() {
    // ---- EC histogram --------------------------------------------------
    let mut ec = [0u64; EC_BUCKETS];
    let mut total: u64 = 0;
    for i in 0..EC_BUCKETS {
        ec[i] = EC_HIST[i].swap(0, Ordering::Relaxed);
        total += ec[i];
    }

    // ---- HVC immediate histogram --------------------------------------
    let mut hvc = [0u64; HVC_BUCKETS];
    let mut hvc_total: u64 = 0;
    for i in 0..HVC_BUCKETS {
        hvc[i] = HVC_IMM_HIST[i].swap(0, Ordering::Relaxed);
        hvc_total += hvc[i];
    }
    let hvc_high = HVC_HIGH_HIST.swap(0, Ordering::Relaxed);

    // ---- DABT top-K ----------------------------------------------------
    // SAFETY: single-threaded; the only other access site is
    // `record_dabt`, which can't overlap (no re-entry across the dump).
    let dabt_pc = unsafe {
        let p = addr_of_mut!(DABT_PC);
        let s = (*p).snapshot_sorted();
        (*p).reset();
        s
    };
    // SAFETY: same as above.
    let dabt_ipa = unsafe {
        let p = addr_of_mut!(DABT_IPA);
        let s = (*p).snapshot_sorted();
        (*p).reset();
        s
    };

    if total == 0 && hvc_total == 0 && hvc_high == 0 && dabt_pc[0].1 == 0 {
        // Nothing observed since last dump — skip to avoid log spam.
        return;
    }

    kprintln!("trap-hist: total={} sync traps in window", total);

    // EC entries sorted desc, nonzero only.
    let mut ec_idx: [usize; EC_BUCKETS] = [0; EC_BUCKETS];
    for i in 0..EC_BUCKETS { ec_idx[i] = i; }
    for k in 0..EC_BUCKETS {
        let mut best = k;
        for j in (k + 1)..EC_BUCKETS {
            if ec[ec_idx[j]] > ec[ec_idx[best]] {
                best = j;
            }
        }
        ec_idx.swap(k, best);
    }
    for k in 0..EC_BUCKETS {
        let i = ec_idx[k];
        if ec[i] == 0 { break; }
        kprintln!(
            "  EC={:#04x} {}: {}",
            i,
            crate::trap::describe_ec(i as u32),
            ec[i]
        );
    }

    // HVC top, sorted desc.
    if hvc_total + hvc_high > 0 {
        let mut hvc_idx: [usize; HVC_BUCKETS] = [0; HVC_BUCKETS];
        for i in 0..HVC_BUCKETS { hvc_idx[i] = i; }
        let printable = PRINT_TOP.min(HVC_BUCKETS);
        for k in 0..printable {
            let mut best = k;
            for j in (k + 1)..HVC_BUCKETS {
                if hvc[hvc_idx[j]] > hvc[hvc_idx[best]] {
                    best = j;
                }
            }
            hvc_idx.swap(k, best);
        }
        kprintln!("  hvc-imm top:");
        for k in 0..printable {
            let i = hvc_idx[k];
            if hvc[i] == 0 { break; }
            kprintln!(
                "    imm={:#04x} {}: {}",
                i,
                hvc_imm_name(i as u32),
                hvc[i]
            );
        }
        if hvc_high > 0 {
            kprintln!("    imm>=0x100: {}", hvc_high);
        }
    }

    // DABT PC top.
    if dabt_pc[0].1 > 0 {
        kprintln!("  dabt-pc top (Misra-Gries, counts are lower bounds):");
        for k in 0..PRINT_TOP.min(TOPK) {
            let (pc, c) = dabt_pc[k];
            if c == 0 { break; }
            kprintln!("    PC={:#010x}: >={}", pc, c);
        }
    }

    // DABT IPA top.
    if dabt_ipa[0].1 > 0 {
        kprintln!("  dabt-ipa top:");
        for k in 0..PRINT_TOP.min(TOPK) {
            let (ipa, c) = dabt_ipa[k];
            if c == 0 { break; }
            kprintln!(
                "    IPA={:#010x} ({}): >={}",
                ipa,
                describe_ipa(ipa),
                c
            );
        }
    }

    crate::unaligned_inline::log_stats();
}

/// Map an HVC immediate to its `HvcImm` variant name, or `"?"` if
/// unknown (e.g. a guest-test imm in a non-test build or a stale slot).
fn hvc_imm_name(imm: u32) -> &'static str {
    match imm {
        v if v == HvcImm::GuestTestPrintByte as u32 => "GuestTestPrintByte",
        v if v == HvcImm::GuestTestPrintHex as u32 => "GuestTestPrintHex",
        v if v == HvcImm::GuestTestPass as u32 => "GuestTestPass",
        v if v == HvcImm::GuestTestFail as u32 => "GuestTestFail",
        v if v == HvcImm::GuestMark as u32 => "GuestMark",
        v if v == HvcImm::GpioTrigger as u32 => "GpioTrigger",
        v if v == HvcImm::Und as u32 => "Und",
        v if v == HvcImm::Align as u32 => "Align",
        v if v == HvcImm::Snapshot as u32 => "Snapshot",
        v if v == HvcImm::DebugStr as u32 => "DebugStr",
        v if v == HvcImm::Debugger as u32 => "Debugger",
        v if v == HvcImm::GuestInjectPen as u32 => "GuestInjectPen",
        v if v == HvcImm::Diag as u32 => "Diag",
        v if v == HvcImm::LoudHalt as u32 => "LoudHalt",
        v if v == HvcImm::BootOs as u32 => "BootOs",
        v if v == HvcImm::RememberSwiret as u32 => "RememberSwiret",
        v if v == HvcImm::DahMrsSpsr as u32 => "DahMrsSpsr",
        v if v == HvcImm::Trace as u32 => "Trace",
        v if v == HvcImm::DahFmeEntry as u32 => "DahFmeEntry",
        v if v == HvcImm::DahOrChain as u32 => "DahOrChain",
        v if v == HvcImm::DahFmeRet as u32 => "DahFmeRet",
        v if v == HvcImm::UnhandledException as u32 => "UnhandledException",
        v if v == HvcImm::UnhandledNumException as u32 => "UnhandledNumException",
        v if v == HvcImm::TaskDump as u32 => "TaskDump",
        v if v == HvcImm::DumpObjectById as u32 => "DumpObjectById",
        v if v == HvcImm::HammerPrint as u32 => "HammerPrint",
        v if v == HvcImm::HammerPutc as u32 => "HammerPutc",
        v if v == HvcImm::HammerFlush as u32 => "HammerFlush",
        v if v == HvcImm::HammerStackTrace as u32 => "HammerStackTrace",
        v if v == HvcImm::HammerExceptionNotify as u32 => "HammerExceptionNotify",
        v if v == HvcImm::FpeEntryProbe as u32 => "FpeEntryProbe",
        v if v == HvcImm::SplashProbe as u32 => "SplashProbe",
        v if v == HvcImm::ResolveFaultRet as u32 => "ResolveFaultRet",
        v if v == HvcImm::ResolveFaultEntry as u32 => "ResolveFaultEntry",
        v if v == HvcImm::StorePermObjEntry as u32 => "StorePermObjEntry",
        v if v == HvcImm::LoadPermObjRet as u32 => "LoadPermObjRet",
        v if v == HvcImm::DoCallEntry as u32 => "DoCallEntry",
        _ => "?",
    }
}

/// Coarse peripheral / memory-region label for an IPA. Hot Voyager
/// MMIO registers from `peripherals/vic.rs` get their own name; the
/// rest collapse to a region.
fn describe_ipa(ipa: u32) -> &'static str {
    match ipa & !0x3FF {
        0x0F18_1000 => "Calendar",
        0x0F18_1400 => "Alarm",
        0x0F18_1800 => "Ticks",
        0x0F18_2000 => "Match0",
        0x0F18_2400 => "Match1",
        0x0F18_2800 => "Match2",
        0x0F18_2C00 => "Match3",
        0x0F18_3000 => "IntPresent",
        0x0F18_3400 => "IntCtrl",
        0x0F18_3800 => "IntClear",
        0x0F18_3C00 => "FIQMask",
        0x0F18_4000 => "IntED1",
        0x0F18_4400 => "IntED2",
        0x0F18_4800 => "IntED3",
        0x0F18_C000 => "GPIORaised",
        0x0F18_C400 => "GPIOCtrl",
        0x0F18_C800 => "GPIOClear",
        _ => region(ipa),
    }
}

fn region(ipa: u32) -> &'static str {
    match ipa {
        0x0000_0000..=0x00FF_FFFF => "ROM",
        0x0200_0000..=0x02FF_FFFF => "Flash0",
        0x1000_0000..=0x10FF_FFFF => "Flash1",
        0x0400_0000..=0x04FF_FFFF => "RAM",
        0x0F00_0000..=0x0F0F_FFFF => "Platform",
        0x0F11_0000..=0x0F11_FFFF => "Voyager",
        0x0F18_0000..=0x0F1F_FFFF => "MMIO",
        _ => "other",
    }
}
