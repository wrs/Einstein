//! SD-card-backed flash persistence.
//!
//! Stores GUEST_FLASH in `/NEWTON.BIN` on the FAT32 boot partition the
//! Pi firmware already mounts to load `config.txt` + `kernel8.img`.
//! Uses the same `embedded_sdmmc` stack the `sd-probe` validated.
//!
//! ## Lifecycle
//!
//! - `init()` (called from `kmain` before `try_load`): brings up
//!   SDHOST, builds a `VolumeManager`, stashes it in static state.
//! - `try_load()`: opens `NEWTON.BIN` read-only, verifies size, reads
//!   into GUEST_FLASH. Sets `FILE_VALID = true` on success.
//! - `mark_dirty(off, len)`: sets bits in a 128-bit bitmap covering
//!   the 128 × 64 KiB blocks of the 8 MiB GUEST_FLASH backing.
//! - `maybe_save()`:
//!   - If `FILE_VALID == false`: open `NEWTON.BIN` ReadWriteCreateOrTruncate,
//!     write the full 8 MiB, set `FILE_VALID = true`, and resolve the
//!     per-cluster LBA map so later saves take the background DMA path.
//!   - Otherwise: open ReadWriteAppend, for each dirty block
//!     `seek_from_start(off)` then `write(&block[64 KiB])`. Same
//!     dirty-tracking + incremental-save pattern as the semihost
//!     backend.
//!
//! Mirrors the semihost backend's semantics so dirty tracking +
//! fingerprint + load behaviour are interchangeable from the
//! caller's POV.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use embedded_sdmmc::{Mode, RawFile, VolumeIdx, VolumeManager};

use super::FlashStore;
use crate::sd::block_device::NullTime;
use crate::sd::sdhost::SdHost;
use crate::{kprint, kprintln, peripherals};

/// Read the generic-timer physical count. Inlined here rather than
/// pulling a helper from snapshot.rs so this module stays self-
/// contained.
#[inline]
fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: MRS of a RO sysreg has no side effects.
    unsafe {
        asm!("mrs {}, cntpct_el0", out(reg) v,
             options(nomem, nostack, preserves_flags));
    }
    v
}

#[inline]
fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: as above.
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) v,
             options(nomem, nostack, preserves_flags));
    }
    v
}

/// Wall-clock milliseconds between two cntpct readings.
fn elapsed_ms(start: u64, end: u64) -> u64 {
    let freq = cntfrq().max(1);
    end.wrapping_sub(start).saturating_mul(1000) / freq
}

/// File name at the root of the FAT32 boot partition. Short (8.3)
/// name so we don't depend on long-filename support on the read
/// side.
const FLASH_FILE: &str = "NEWTON.BIN";

/// Block granularity for dirty tracking + I/O. Each set bit covers
/// `BLOCK_SIZE` bytes of GUEST_FLASH. Matches the semihost backend.
const BLOCK_SIZE: usize = 64 * 1024;
const NUM_BLOCKS: usize = peripherals::flash::SIZE / BLOCK_SIZE; // 128
const NUM_BITMAP_WORDS: usize = NUM_BLOCKS / 32; // 4

/// Progress reporting for full-save / load. Print a '.' for every
/// `PROGRESS_DOT` bytes transferred, writing in `PROGRESS_CHUNK`
/// pieces. With an 8 MiB store and 256 KiB dots, that's 32 dots
/// per full save — coarse enough to fit on a serial line, fine
/// enough to confirm the boot isn't actually hung. The HDMI-audio
/// build uses smaller chunks so progress dots / splash bar update
/// incrementally as the multi-MiB transfer streams.
const PROGRESS_CHUNK: usize = if cfg!(nh_audio_pi_hdmi) {
    16 * 1024
} else {
    64 * 1024
};
const PROGRESS_DOT: usize = 256 * 1024;

static DIRTY: [AtomicU32; NUM_BITMAP_WORDS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// True once we know `NEWTON.BIN` exists at the right size and
/// matches GUEST_FLASH in shape. Saves use seek+write when true;
/// otherwise the next save truncates + rewrites the full backing.
static FILE_VALID: AtomicBool = AtomicBool::new(false);

/// True once `init` has constructed the `VolumeManager`. Calls
/// before that return early; we don't want a hidden SDHOST init
/// triggered by a snapshot autosave on the very first guest IRQ.
static INIT_DONE: AtomicBool = AtomicBool::new(false);

type Vm = VolumeManager<SdHost, NullTime, 4, 4, 1>;

/// Single global VolumeManager. Single-core EL2; no concurrency,
/// no locking. `static mut` access is gated behind `INIT_DONE` and
/// the `unsafe` block in `vm()`.
static mut VOL_MGR: Option<Vm> = None;

/// Max clusters in NEWTON.BIN's per-cluster LBA map. Sized for the
/// smallest FAT32 cluster we'd accept (4 KiB); a card with smaller
/// clusters resolves to `None` and falls back to the FAT save. A
/// 128 GB card uses 32–64 KiB clusters → 128–256 entries, well under.
const MAX_FLASH_CLUSTERS: usize = peripherals::flash::SIZE / 4096;

/// NEWTON.BIN's per-cluster start-LBA map, set by `resolve_extent_map`
/// (from `try_load`, or right after the first full save creates the
/// file) once its first cluster is verified. The background DMA save
/// (milestone 4) writes each dirty cluster raw to its LBA here —
/// fragmentation-immune, since a cluster is intrinsically contiguous.
/// Guarded by `FLASH_NUM_CLUSTERS` (0 = unresolved → FAT save path).
struct ClusterLbaMap(UnsafeCell<[u32; MAX_FLASH_CLUSTERS]>);
// SAFETY: single-core EL2; written only by `resolve_extent_map`, whose
// FLASH_NUM_CLUSTERS release-store gates all readers.
unsafe impl Sync for ClusterLbaMap {}
static FLASH_CLUSTER_LBAS: ClusterLbaMap =
    ClusterLbaMap(UnsafeCell::new([0; MAX_FLASH_CLUSTERS]));
static FLASH_NUM_CLUSTERS: AtomicUsize = AtomicUsize::new(0);
static FLASH_BLOCKS_PER_CLUSTER: AtomicU32 = AtomicU32::new(0);

// ---- Background DMA save state machine (milestone 4b) ---------------
//
// The autosave tick (`maybe_save`) starts a save by snapshotting the
// dirty bitmap, computing the set of dirty clusters, and kicking off
// the first cluster's DMA write — then returns to the guest. Each
// SD-TX channel completion IRQ (`on_dma_completion`) finishes the
// cluster just written (CMD12 + busy, under an IRQ-unmasked window so
// audio stays fed) and starts the next, until the set drains.
//
// All of this runs in IRQ context on a single core and is never
// re-entered: a save is only started while `SAVE_ACTIVE == false`, and
// the only place that re-enters the SD controller — the completion
// handler's unmasked `finish` — takes nested IRQs through the slim
// `irq_from_el2` path, which does not start saves. So plain atomics +
// an `UnsafeCell` bitmap are sufficient; no locking.

/// True while a cluster's DMA write is in flight (state `Writing`),
/// from `start_sectors_dma` until its completion IRQ finishes it.
static SAVE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The cluster index currently being DMA-written (valid iff
/// `SAVE_ACTIVE`). Read by the completion handler to finish it.
static SAVE_CLUSTER: AtomicUsize = AtomicUsize::new(0);

/// Number of completion IRQs needed to drain the in-flight save — i.e.
/// remaining + in-flight clusters. Diagnostic only.
static SAVE_REMAINING: AtomicUsize = AtomicUsize::new(0);

const CL_BITMAP_WORDS: usize = MAX_FLASH_CLUSTERS / 32; // 64

/// Bitmap of clusters still to write in the in-flight save (the
/// currently-`Writing` cluster's bit is already cleared). Built by
/// `start_dma_save`, consumed by `advance_save`.
struct ClusterBitmap(UnsafeCell<[u32; CL_BITMAP_WORDS]>);
// SAFETY: touched only by the save machine, which runs single-core in
// IRQ context and is never re-entered (see the module note above).
unsafe impl Sync for ClusterBitmap {}
static SAVE_PENDING_CL: ClusterBitmap = ClusterBitmap(UnsafeCell::new([0; CL_BITMAP_WORDS]));

/// The dirty-block snapshot backing the in-flight save, kept so a
/// mid-save failure can re-mark exactly those blocks dirty for retry.
static SAVE_SNAPSHOT: [AtomicU32; NUM_BITMAP_WORDS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

#[allow(static_mut_refs)]
fn vm() -> Option<&'static Vm> {
    if !INIT_DONE.load(Ordering::Relaxed) {
        return None;
    }
    // SAFETY: INIT_DONE is set only by `init()` on core 0 before any
    // other code touches VOL_MGR; subsequent reads see a fully
    // constructed Vm. Single-core EL2.
    unsafe { VOL_MGR.as_ref() }
}

pub struct SdBackend;

pub static BACKEND: SdBackend = SdBackend;

impl FlashStore for SdBackend {
    fn try_load(&self) {
        let Some(mgr) = vm() else {
            kprintln!("flash_persist_sd: init not called yet, skipping try_load");
            return;
        };
        let volume = match mgr.open_volume(VolumeIdx(0)) {
            Ok(v) => v,
            Err(e) => {
                kprintln!("flash_persist_sd: open_volume FAILED: {:?}", e);
                return;
            }
        };
        let root = match volume.open_root_dir() {
            Ok(d) => d,
            Err(e) => {
                kprintln!("flash_persist_sd: open_root_dir FAILED: {:?}", e);
                return;
            }
        };
        let file = match root.open_file_in_dir(FLASH_FILE, Mode::ReadOnly) {
            Ok(f) => f,
            Err(_e) => {
                kprintln!(
                    "flash_persist_sd: no persistent flash at {} (will create on first save)",
                    FLASH_FILE
                );
                return;
            }
        };
        let len = file.length();
        if len as usize != peripherals::flash::SIZE {
            kprintln!(
                "flash_persist_sd: {} is {} bytes, want {} — ignoring, will rewrite on next save",
                FLASH_FILE,
                len,
                peripherals::flash::SIZE
            );
            return;
        }
        kprintln!(
            "flash_persist_sd: loading {} bytes from {}",
            len, FLASH_FILE
        );
        let t0 = cntpct();
        // SAFETY: GUEST_FLASH backing is a static mut byte array; single-
        // threaded on core 0 during boot, before stage-2 exposes flash
        // to the guest. `len` matches SIZE, checked above.
        let buf = unsafe {
            core::slice::from_raw_parts_mut(
                peripherals::flash::host_pa() as *mut u8,
                peripherals::flash::SIZE,
            )
        };
        kprint!("flash_persist_sd: load [");
        let mut off = 0usize;
        let mut next_dot = PROGRESS_DOT;
        while off < buf.len() {
            // Cap each read at PROGRESS_CHUNK so progress (dots +
            // splash bar) updates incrementally. The SD driver
            // happily fulfills a single multi-MiB read in one call,
            // which would make the bar/dots jump from empty to full
            // in one tick.
            let end = (off + PROGRESS_CHUNK).min(buf.len());
            match file.read(&mut buf[off..end]) {
                Ok(0) => break,
                Ok(n) => {
                    off += n;
                    while off >= next_dot && next_dot <= buf.len() {
                        kprint!(".");
                        next_dot += PROGRESS_DOT;
                    }
                    // Drive the lower 20% of the boot-splash bar from
                    // SD-load progress. Gated on pi_fb; no-op on other
                    // backends. Safe to call before
                    // `display::splash::init` (becomes a no-op).
                    #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
                    crate::display::splash::set_load_progress(
                        off as u64,
                        buf.len() as u64,
                    );
                }
                Err(e) => {
                    kprintln!("] FAILED at off={}: {:?}", off, e);
                    return;
                }
            }
        }
        kprintln!("]");
        if off != peripherals::flash::SIZE {
            kprintln!(
                "flash_persist_sd: short read ({} of {}); cold-booting flash state",
                off,
                peripherals::flash::SIZE,
            );
            return;
        }
        FILE_VALID.store(true, Ordering::Relaxed);
        let ms = elapsed_ms(t0, cntpct());
        kprintln!(
            "flash_persist_sd: loaded {} bytes in {} ms ({} KB/s)",
            peripherals::flash::SIZE,
            ms,
            (peripherals::flash::SIZE as u64 * 1000 / 1024) / ms.max(1),
        );

        // The image is now loaded, so GUEST_FLASH[0..512] equals the
        // file's first sector — resolve the per-cluster LBA map for
        // the background DMA save (milestones 3/4).
        resolve_extent_map(mgr, file.to_raw_file());
    }

    fn mark_dirty(&self, off: usize, len: usize) {
        if len == 0 {
            return;
        }
        let first = off / BLOCK_SIZE;
        let last = (off + len - 1) / BLOCK_SIZE;
        if first >= NUM_BLOCKS {
            return;
        }
        let last = last.min(NUM_BLOCKS - 1);
        let mut w = first / 32;
        while w * 32 <= last && w < NUM_BITMAP_WORDS {
            let word_first = w * 32;
            let word_last = word_first + 31;
            let lo = first.max(word_first) - word_first;
            let hi = last.min(word_last) - word_first;
            let span = (hi - lo + 1) as u32;
            let m = if span == 32 { u32::MAX } else { (1u32 << span) - 1 };
            let mask = m << lo;
            DIRTY[w].fetch_or(mask, Ordering::Relaxed);
            w += 1;
        }
    }

    fn maybe_save(&self) {
        let Some(mgr) = vm() else {
            return;
        };
        let snapshot: [u32; NUM_BITMAP_WORDS] = [
            DIRTY[0].swap(0, Ordering::Relaxed),
            DIRTY[1].swap(0, Ordering::Relaxed),
            DIRTY[2].swap(0, Ordering::Relaxed),
            DIRTY[3].swap(0, Ordering::Relaxed),
        ];
        let any_dirty = snapshot.iter().any(|w| *w != 0);
        let valid = FILE_VALID.load(Ordering::Relaxed);
        if !any_dirty && valid {
            return;
        }

        // Background DMA save path (milestone 4b). Eligible once the
        // file exists at full size (`valid`) and its per-cluster LBA map
        // resolved (`FLASH_NUM_CLUSTERS != 0`). Starts the write and
        // returns immediately; the SD-TX completion IRQ drains it while
        // the guest keeps running. Falls through to the synchronous FAT
        // path below when not eligible (first save / unresolved map).
        if valid && try_start_dma_save(&snapshot) {
            return;
        }

        let volume = match mgr.open_volume(VolumeIdx(0)) {
            Ok(v) => v,
            Err(e) => {
                kprintln!("flash_persist_sd: open_volume FAILED: {:?}", e);
                remark_dirty(&snapshot);
                return;
            }
        };
        let root = match volume.open_root_dir() {
            Ok(d) => d,
            Err(e) => {
                kprintln!("flash_persist_sd: open_root_dir FAILED: {:?}", e);
                remark_dirty(&snapshot);
                return;
            }
        };

        if !valid {
            // Full write: file doesn't exist or wrong size. Announce
            // up-front because the write blocks EL2 (and therefore
            // the guest) for its duration — no other sign-of-life
            // otherwise.
            kprintln!(
                "flash_persist_sd: starting full save ({} bytes)",
                peripherals::flash::SIZE
            );
            let t0 = cntpct();
            let file =
                match root.open_file_in_dir(FLASH_FILE, Mode::ReadWriteCreateOrTruncate) {
                    Ok(f) => f,
                    Err(e) => {
                        kprintln!(
                            "flash_persist_sd: open(create) {} FAILED: {:?}",
                            FLASH_FILE,
                            e
                        );
                        remark_dirty(&snapshot);
                        return;
                    }
                };
            // SAFETY: GUEST_FLASH is a static mut byte array; the write
            // takes a &[u8] for the duration of these calls only.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    peripherals::flash::host_pa() as *const u8,
                    peripherals::flash::SIZE,
                )
            };
            // Chunk the write so we can emit progress dots; one
            // file.write call of 8 MiB is otherwise a silent
            // multi-minute black box.
            kprint!("flash_persist_sd: full save [");
            let mut off = 0;
            let mut next_dot = PROGRESS_DOT;
            while off < bytes.len() {
                let end = (off + PROGRESS_CHUNK).min(bytes.len());
                if let Err(e) = file.write(&bytes[off..end]) {
                    kprintln!("] FAILED at off={}: {:?}", off, e);
                    remark_dirty(&snapshot);
                    return;
                }
                off = end;
                while off >= next_dot && next_dot <= bytes.len() {
                    kprint!(".");
                    next_dot += PROGRESS_DOT;
                }
            }
            kprintln!("]");
            if let Err(e) = file.flush() {
                kprintln!("flash_persist_sd: flush after full write FAILED: {:?}", e);
                remark_dirty(&snapshot);
                return;
            }
            FILE_VALID.store(true, Ordering::Relaxed);
            let ms = elapsed_ms(t0, cntpct());
            kprintln!(
                "flash_persist_sd: full save done ({} bytes in {} ms, {} KB/s)",
                bytes.len(),
                ms,
                (bytes.len() as u64 * 1000 / 1024) / ms.max(1),
            );
            // The file now exists at full size and GUEST_FLASH[0..512]
            // equals its first sector — resolve the per-cluster LBA
            // map so this session's incremental saves take the
            // background DMA path instead of freezing the guest in
            // the synchronous FAT path below.
            resolve_extent_map(mgr, file.to_raw_file());
            return;
        }

        // Incremental save. Walk set bits and seek+write each block.
        let file = match root.open_file_in_dir(FLASH_FILE, Mode::ReadWriteAppend) {
            Ok(f) => f,
            Err(e) => {
                kprintln!("flash_persist_sd: open(rw) {} FAILED: {:?}", FLASH_FILE, e);
                FILE_VALID.store(false, Ordering::Relaxed);
                remark_dirty(&snapshot);
                return;
            }
        };
        let total_dirty: u32 = snapshot.iter().map(|w| w.count_ones()).sum();
        let t0 = cntpct();
        if total_dirty > 0 {
            kprint!("flash_persist_sd: incremental save {} blk [", total_dirty);
        }
        let mut blocks_written = 0usize;
        for blk in 0..NUM_BLOCKS {
            let word = blk / 32;
            let bit = blk % 32;
            if snapshot[word] & (1 << bit) == 0 {
                continue;
            }
            let off = blk * BLOCK_SIZE;
            if let Err(e) = file.seek_from_start(off as u32) {
                kprintln!(
                    "] seek to off={} FAILED: {:?}",
                    off, e
                );
                FILE_VALID.store(false, Ordering::Relaxed);
                remark_dirty(&snapshot);
                return;
            }
            // SAFETY: GUEST_FLASH static mut byte array, single-threaded.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    peripherals::flash::host_pa().wrapping_add(off as u64) as *const u8,
                    BLOCK_SIZE,
                )
            };
            if let Err(e) = file.write(bytes) {
                kprintln!(
                    "] write at off={} FAILED: {:?}",
                    off, e
                );
                FILE_VALID.store(false, Ordering::Relaxed);
                remark_dirty(&snapshot);
                return;
            }
            blocks_written += 1;
            kprint!(".");
        }
        if total_dirty > 0 {
            let ms = elapsed_ms(t0, cntpct());
            let bytes = (blocks_written as u64) * BLOCK_SIZE as u64;
            kprintln!(
                "] done ({} KB in {} ms, {} KB/s)",
                bytes / 1024,
                ms,
                (bytes * 1000 / 1024) / ms.max(1),
            );
        }
        if let Err(e) = file.flush() {
            kprintln!("flash_persist_sd: flush after incremental save FAILED: {:?}", e);
            // Don't clear FILE_VALID — the writes went through; next
            // save will just re-flush.
            return;
        }
    }

    fn fingerprint(&self) -> u32 {
        // SAFETY: GUEST_FLASH static mut byte array; single-threaded
        // reads during snapshot save / load.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                peripherals::flash::host_pa() as *const u8,
                peripherals::flash::SIZE,
            )
        };
        let mut h: u32 = 0x811c_9dc5;
        for &b in bytes {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }
}

/// Re-mark a snapshot's bits dirty after a failed save so the next
/// save retries the same blocks.
fn remark_dirty(snapshot: &[u32; NUM_BITMAP_WORDS]) {
    for (i, w) in snapshot.iter().enumerate() {
        DIRTY[i].fetch_or(*w, Ordering::Relaxed);
    }
}

// ---- Background DMA save state machine ------------------------------

#[inline]
fn cl_test(bm: &[u32; CL_BITMAP_WORDS], i: usize) -> bool {
    bm[i / 32] & (1 << (i % 32)) != 0
}

#[inline]
fn cl_set(bm: &mut [u32; CL_BITMAP_WORDS], i: usize) {
    bm[i / 32] |= 1 << (i % 32);
}

#[inline]
fn cl_clear(bm: &mut [u32; CL_BITMAP_WORDS], i: usize) {
    bm[i / 32] &= !(1 << (i % 32));
}

/// First set cluster bit at index >= `from`, or `None` if the bitmap is
/// drained from there on.
fn cl_next(bm: &[u32; CL_BITMAP_WORDS], from: usize) -> Option<usize> {
    let mut i = from;
    while i < MAX_FLASH_CLUSTERS {
        // Skip whole zero words for speed (2048 bits = 64 words).
        if i % 32 == 0 && bm[i / 32] == 0 {
            i += 32;
            continue;
        }
        if cl_test(bm, i) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Resolve `NEWTON.BIN`'s per-cluster LBA map for the background DMA
/// save and verify it: read cluster 0's first sector raw and compare
/// it against GUEST_FLASH[0..512]. The two are equal both after a load
/// and after a full save, so the same check serves both callers. On
/// any doubt (too many clusters / mismatch / error) FLASH_NUM_CLUSTERS
/// stays 0 and saves stay on the proven FAT writes. Per-cluster, so
/// file fragmentation is fine. Consumes (closes) `raw`.
fn resolve_extent_map(mgr: &Vm, raw: RawFile) {
    // SAFETY: single-core EL2; FLASH_CLUSTER_LBAS is written only here,
    // and its readers (the DMA save path) are gated on the
    // FLASH_NUM_CLUSTERS release-store below. No save is in flight
    // while we run: try_load runs at boot before any save, and the
    // full-save caller runs synchronously with SAVE_ACTIVE == false.
    let lbas = unsafe { &mut *FLASH_CLUSTER_LBAS.0.get() };
    match mgr.file_cluster_lbas(raw, lbas) {
        Ok(Some((n, bpc))) => {
            let lba0 = lbas[0];
            let mut sec = [0u8; 512];
            let rd = mgr.device(|d| d.read_block(lba0, &mut sec));
            // SAFETY: GUEST_FLASH backing; single-core EL2.
            let head = unsafe {
                core::slice::from_raw_parts(peripherals::flash::host_pa() as *const u8, 512)
            };
            match rd {
                Ok(()) if &sec[..] == head => {
                    FLASH_BLOCKS_PER_CLUSTER.store(bpc, Ordering::Relaxed);
                    FLASH_NUM_CLUSTERS.store(n, Ordering::Release);
                    kprintln!(
                        "flash_persist_sd: extent {} clusters, {} blocks/cluster, lba[0]={} — verified (DMA save eligible)",
                        n, bpc, lba0
                    );
                }
                Ok(()) => kprintln!(
                    "flash_persist_sd: extent lba[0]={} MISMATCH vs in-memory image — DMA save disabled",
                    lba0
                ),
                Err(e) => kprintln!(
                    "flash_persist_sd: extent lba[0]={} raw read FAILED: {:?} — DMA save disabled",
                    lba0, e
                ),
            }
        }
        Ok(None) => kprintln!(
            "flash_persist_sd: {} too many clusters or empty — DMA save disabled (FAT save path)",
            FLASH_FILE
        ),
        Err(e) => kprintln!("flash_persist_sd: extent resolve FAILED: {:?}", e),
    }
    let _ = mgr.close_file(raw);
}

/// Try to start a background DMA save of the dirty clusters covered by
/// `snapshot`. Returns true if the DMA path handled this tick — a save
/// was started, one is already in flight, or there was nothing to do —
/// and false if the caller should fall back to the synchronous FAT save
/// (per-cluster map unresolved).
///
/// Caller guarantees `FILE_VALID` (the file exists at full size); the
/// raw cluster writes don't touch FAT metadata, so the dir entry and
/// allocation chain stay stable.
fn try_start_dma_save(snapshot: &[u32; NUM_BITMAP_WORDS]) -> bool {
    let n = FLASH_NUM_CLUSTERS.load(Ordering::Acquire);
    let bpc = FLASH_BLOCKS_PER_CLUSTER.load(Ordering::Relaxed) as usize;
    if n == 0 || bpc == 0 {
        return false; // map unresolved → caller uses the FAT path
    }
    if SAVE_ACTIVE.load(Ordering::Relaxed) {
        // A save from a previous tick is still draining. Re-mark this
        // tick's dirty blocks so the next pass picks them up, and let
        // the in-flight save run.
        remark_dirty(snapshot);
        return true;
    }

    // Build the dirty-cluster set from the dirty 64 KiB blocks. A block
    // maps to the clusters its byte range overlaps; a 64 KiB block can
    // span several smaller clusters or sit inside a larger one.
    let cluster_bytes = bpc * 512;
    // SAFETY: single-core IRQ context, save machine not re-entered.
    let pend = unsafe { &mut *SAVE_PENDING_CL.0.get() };
    pend.iter_mut().for_each(|w| *w = 0);
    let mut count = 0usize;
    for blk in 0..NUM_BLOCKS {
        if snapshot[blk / 32] & (1 << (blk % 32)) == 0 {
            continue;
        }
        let bstart = blk * BLOCK_SIZE;
        let c0 = bstart / cluster_bytes;
        let c1 = (bstart + BLOCK_SIZE - 1) / cluster_bytes;
        for ci in c0..=c1 {
            if ci < n && !cl_test(pend, ci) {
                cl_set(pend, ci);
                count += 1;
            }
        }
    }
    if count == 0 {
        return true; // nothing to write
    }

    // Stash the snapshot so a mid-save failure re-marks exactly these
    // blocks dirty for the next pass.
    for (i, w) in snapshot.iter().enumerate() {
        SAVE_SNAPSHOT[i].store(*w, Ordering::Relaxed);
    }
    SAVE_REMAINING.store(count, Ordering::Relaxed);
    kprintln!(
        "flash_persist_sd: DMA save start ({} cluster(s), {} KiB each)",
        count,
        cluster_bytes / 1024
    );
    advance_save(0);
    true
}

/// Start the next pending cluster's DMA write, scanning from cluster
/// index `from`, or close out the save when the set is drained. Sets
/// `SAVE_ACTIVE` on a successful start; aborts (re-mark + FAT fallback)
/// on a start failure.
fn advance_save(from: usize) {
    let Some(mgr) = vm() else {
        abort_save();
        return;
    };
    // SAFETY: single-core IRQ context, save machine not re-entered.
    let pend = unsafe { &mut *SAVE_PENDING_CL.0.get() };
    let Some(ci) = cl_next(pend, from) else {
        finish_save();
        return;
    };
    cl_clear(pend, ci);

    let bpc = FLASH_BLOCKS_PER_CLUSTER.load(Ordering::Relaxed) as usize;
    let cluster_bytes = bpc * 512;
    let off = ci * cluster_bytes;
    let len = cluster_bytes.min(peripherals::flash::SIZE - off);
    // SAFETY: GUEST_FLASH backing is a static byte array. The DMA reads
    // this range while the guest may keep writing it — acceptable: any
    // block dirtied during the save stays set in DIRTY (we swapped the
    // snapshot out) and is re-captured by the next pass.
    let src = unsafe {
        core::slice::from_raw_parts(
            peripherals::flash::host_pa().wrapping_add(off as u64) as *const u8,
            len,
        )
    };
    // SAFETY: FLASH_CLUSTER_LBAS written by resolve_extent_map before
    // FLASH_NUM_CLUSTERS released the DMA save path; never written again.
    let lba = unsafe { (*FLASH_CLUSTER_LBAS.0.get())[ci] };

    match mgr.device(|d| d.start_sectors_dma(lba, src)) {
        Ok(()) => {
            SAVE_CLUSTER.store(ci, Ordering::Relaxed);
            SAVE_ACTIVE.store(true, Ordering::Relaxed);
        }
        Err(e) => {
            kprintln!(
                "flash_persist_sd: DMA save start cluster {} (lba {}) FAILED: {:?} — \
                 disabling DMA save, FAT fallback",
                ci, lba, e
            );
            abort_save();
        }
    }
}

/// SD-TX completion IRQ handler: finish the cluster just DMA-written
/// (settle FSM + CMD12 busy-wait) and advance to the next. Runs in IRQ
/// context; the CMD12 busy-wait is wrapped in an IRQ-unmasked window so
/// it doesn't starve the audio MAI feed / CNTHP rearm.
pub fn on_dma_completion() {
    if !SAVE_ACTIVE.load(Ordering::Relaxed) {
        // Stray completion (e.g. the polled bring-up path armed the
        // channel with INTEN, or a late IRQ after a finished save).
        return;
    }
    let ci = SAVE_CLUSTER.load(Ordering::Relaxed);
    let Some(mgr) = vm() else {
        abort_save();
        return;
    };
    let r = crate::cpu::with_irqs_unmasked(|| mgr.device(|d| d.finish_sectors_dma()));
    if let Err(e) = r {
        kprintln!(
            "flash_persist_sd: DMA save cluster {} finish FAILED: {:?} — \
             disabling DMA save, FAT fallback",
            ci, e
        );
        abort_save();
        return;
    }
    let left = SAVE_REMAINING.load(Ordering::Relaxed).saturating_sub(1);
    SAVE_REMAINING.store(left, Ordering::Relaxed);
    // SAVE_ACTIVE stays true through advance_save (which either starts
    // the next cluster or clears it in finish_save) so a concurrent
    // autosave tick won't start a second save.
    advance_save(ci + 1);
}

/// All clusters drained — the background save is complete.
fn finish_save() {
    SAVE_REMAINING.store(0, Ordering::Relaxed);
    SAVE_ACTIVE.store(false, Ordering::Relaxed);
    kprintln!("flash_persist_sd: DMA save complete");
}

/// Abort the in-flight save on a hardware error: tear down the channel,
/// re-mark the snapshot's blocks dirty for retry, and disable the DMA
/// save path so the next tick falls back to the proven FAT writes
/// rather than retrying a latched-error channel forever.
fn abort_save() {
    crate::host_dma::sd_tx_abort();
    let snap = [
        SAVE_SNAPSHOT[0].load(Ordering::Relaxed),
        SAVE_SNAPSHOT[1].load(Ordering::Relaxed),
        SAVE_SNAPSHOT[2].load(Ordering::Relaxed),
        SAVE_SNAPSHOT[3].load(Ordering::Relaxed),
    ];
    remark_dirty(&snap);
    FLASH_NUM_CLUSTERS.store(0, Ordering::Release);
    SAVE_REMAINING.store(0, Ordering::Relaxed);
    SAVE_ACTIVE.store(false, Ordering::Relaxed);
}

/// One-time SDHOST bring-up + VolumeManager construction. Called
/// from `kmain` before `flash_persist::try_load`. Subsequent
/// FlashStore calls become no-ops if this didn't succeed.
pub fn init() {
    if INIT_DONE.load(Ordering::Relaxed) {
        return;
    }
    let host = match SdHost::init() {
        Ok(h) => h,
        Err(e) => {
            kprintln!("flash_persist_sd: SDHOST init FAILED: {:?}", e);
            return;
        }
    };
    // SAFETY: single-core EL2, called once from kmain before any
    // other code touches VOL_MGR.
    unsafe {
        #[allow(static_mut_refs)]
        {
            VOL_MGR = Some(VolumeManager::new(host, NullTime));
        }
    }
    INIT_DONE.store(true, Ordering::Relaxed);
    kprintln!("flash_persist_sd: SDHOST ready");
}
