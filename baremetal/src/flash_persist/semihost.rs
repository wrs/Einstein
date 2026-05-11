//! Semihosting-backed flash persistence.
//!
//! Reads / writes `$HOME/.newton/flash.bin` (path resolved at build
//! time, see `build.rs::emit_flash_path`). Dirty tracking is at 64 KiB
//! granularity: a 128-bit bitmap (`[AtomicU32; 4]`) covering the
//! 128 64 KiB blocks of the 8 MiB backing. The autosave path swaps
//! the bitmap to zero and writes only set blocks via SYS_SEEK +
//! SYS_WRITE, so a typical session's per-save cost is a handful of
//! 64 KiB writes rather than the full 8 MiB.
//!
//! First-time create (file absent or wrong size): the next save
//! truncates the file and writes the full 8 MiB regardless of the
//! bitmap. After that, all saves are incremental.
//!
//! Torn-write semantics: each block is one SYS_WRITE call, so blocks
//! are individually atomic. A SIGKILL mid-save may leave some blocks
//! updated and others not — the same model as a real-hardware power
//! loss mid-erase, which the Newton kernel already tolerates.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::FlashStore;
use crate::{kprintln, peripherals};

// Semihosting op IDs (ARM Semihosting for AArch32/64, section 5.3).
const SYS_OPEN: u64 = 0x01;
const SYS_CLOSE: u64 = 0x02;
const SYS_WRITE: u64 = 0x05;
const SYS_READ: u64 = 0x06;
const SYS_SEEK: u64 = 0x0A;
const SYS_FLEN: u64 = 0x0C;
const SYS_SYSTEM: u64 = 0x12;

// SYS_OPEN mode flags (C fopen-style).
const MODE_READ_BINARY: u64 = 0x01; // "rb"
const MODE_READ_WRITE_BINARY: u64 = 0x03; // "r+b"
const MODE_WRITE_BINARY: u64 = 0x05; // "wb"

/// `$HOME/.newton/flash.bin\0` — path resolved by `build.rs`, NUL
/// appended here (cargo rejects literal NULs in rustc-env values).
/// The trailing NUL is required by semihosting SYS_OPEN; the length
/// parameter we pass excludes it.
const FLASH_PATH: &[u8] = concat!(env!("NEWTON_FLASH_PATH"), "\0").as_bytes();
const FLASH_DIR: &str = env!("NEWTON_FLASH_DIR");

/// Block granularity for dirty tracking + I/O. Each set bit in DIRTY
/// covers BLOCK_SIZE bytes of GUEST_FLASH.
const BLOCK_SIZE: usize = 64 * 1024;
const NUM_BLOCKS: usize = peripherals::flash::SIZE / BLOCK_SIZE; // 128

// 128 bits = 4 × u32. Per-word AtomicU32 lets `mark_dirty` use
// fetch_or with no global lock; saves use swap(0) for the
// clear-before-write race-free pattern.
const NUM_BITMAP_WORDS: usize = NUM_BLOCKS / 32;
static DIRTY: [AtomicU32; NUM_BITMAP_WORDS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// True once we know `FLASH_PATH` is an 8 MiB file on disk. Saves use
/// in-place (r+b) writes when true; otherwise they truncate-and-write
/// the full backing (wb).
static FILE_VALID: AtomicBool = AtomicBool::new(false);

/// One-shot guard for `mkdir -p $FLASH_DIR` so we don't fork a shell
/// on every save.
static MKDIR_DONE: AtomicBool = AtomicBool::new(false);

pub struct SemihostBackend;

impl FlashStore for SemihostBackend {
    fn try_load(&self) {
        // SAFETY: HLT #0xF000 semihosting traps don't disturb EL2 state.
        let fh = sh_open(FLASH_PATH, MODE_READ_BINARY);
        if fh < 0 {
            kprintln!(
                "flash_persist: no persistent flash at {} (will create on first save)",
                path_str()
            );
            return;
        }
        let len = sh_flen(fh);
        if len != peripherals::flash::SIZE as i64 {
            kprintln!(
                "flash_persist: {} is {} bytes, want {} — ignoring, will rewrite on next save",
                path_str(),
                len,
                peripherals::flash::SIZE
            );
            sh_close(fh);
            return;
        }
        // SAFETY: GUEST_FLASH backing is a static mut byte array; we're
        // single-threaded on core 0 during boot, before stage-2 exposes
        // the flash to the guest. `len` matches SIZE, checked above.
        let buf = unsafe {
            core::slice::from_raw_parts_mut(
                peripherals::flash::host_pa() as *mut u8,
                peripherals::flash::SIZE,
            )
        };
        let ok = sh_read(fh, buf) == 0;
        sh_close(fh);
        if !ok {
            kprintln!(
                "flash_persist: short read from {}; cold-booting flash state",
                path_str()
            );
            // Reset the buffer to the just-seeded state? No — we don't
            // have the seeded copy any more. Easier path: leave the
            // partial read in place; the kernel will treat the bad
            // bytes as a torn-write and either repair (block-1 copy)
            // or re-init. FILE_VALID stays false so the next save
            // rewrites the whole file.
            return;
        }
        FILE_VALID.store(true, Ordering::Relaxed);
        kprintln!(
            "flash_persist: loaded {} bytes from {}",
            peripherals::flash::SIZE,
            path_str()
        );
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
        // Set each bit in [first, last]. Coalesce by bitmap word so a
        // multi-block erase typically does one fetch_or per word.
        let mut w = first / 32;
        while w * 32 <= last && w < NUM_BITMAP_WORDS {
            let word_first = w * 32;
            let word_last = word_first + 31;
            let lo = first.max(word_first) - word_first;
            let hi = last.min(word_last) - word_first;
            let mask: u32 = if hi == 31 {
                u32::MAX.wrapping_shr(lo as u32).wrapping_shl(lo as u32)
            } else {
                // bits [lo..=hi]
                let span = (hi - lo + 1) as u32;
                let m = if span == 32 { u32::MAX } else { (1u32 << span) - 1 };
                m << lo
            };
            DIRTY[w].fetch_or(mask, Ordering::Relaxed);
            w += 1;
        }
    }

    fn maybe_save(&self) {
        // Snapshot the bitmap (and clear) so concurrent writes from
        // program_word during the save just re-mark dirty and get
        // picked up next time. EL2 is single-core so this is mostly
        // theoretical, but the ordering keeps it correct either way.
        let snapshot: [u32; NUM_BITMAP_WORDS] = [
            DIRTY[0].swap(0, Ordering::Relaxed),
            DIRTY[1].swap(0, Ordering::Relaxed),
            DIRTY[2].swap(0, Ordering::Relaxed),
            DIRTY[3].swap(0, Ordering::Relaxed),
        ];
        let any_dirty = snapshot.iter().any(|w| *w != 0);
        let valid = FILE_VALID.load(Ordering::Relaxed);

        // Fast paths: nothing to do.
        if !any_dirty && valid {
            return;
        }

        ensure_dir();

        if !valid {
            // Full write-and-truncate. Covers two cases: first-ever
            // boot (file doesn't exist) and a previously-bad file
            // (wrong size, short-read, etc.).
            let fh = sh_open(FLASH_PATH, MODE_WRITE_BINARY);
            if fh < 0 {
                kprintln!(
                    "flash_persist: SYS_OPEN wb {} failed (rc={})",
                    path_str(),
                    fh
                );
                // Re-mark everything dirty so the next save retries.
                for w in &DIRTY {
                    w.fetch_or(u32::MAX, Ordering::Relaxed);
                }
                return;
            }
            // SAFETY: GUEST_FLASH is a static mut byte array; we read
            // it for the duration of the semihosting write call.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    peripherals::flash::host_pa() as *const u8,
                    peripherals::flash::SIZE,
                )
            };
            let unwritten = sh_write(fh, bytes);
            sh_close(fh);
            if unwritten != 0 {
                kprintln!(
                    "flash_persist: SYS_WRITE short by {} bytes (full save); will retry",
                    unwritten
                );
                for w in &DIRTY {
                    w.fetch_or(u32::MAX, Ordering::Relaxed);
                }
                return;
            }
            FILE_VALID.store(true, Ordering::Relaxed);
            kprintln!(
                "flash_persist: wrote full {} bytes to {}",
                peripherals::flash::SIZE,
                path_str()
            );
            return;
        }

        // Incremental save: file is known-good 8 MiB, we just need to
        // write the dirty blocks in place via r+b + SYS_SEEK.
        let fh = sh_open(FLASH_PATH, MODE_READ_WRITE_BINARY);
        if fh < 0 {
            kprintln!(
                "flash_persist: SYS_OPEN r+b {} failed (rc={}); marking file invalid",
                path_str(),
                fh
            );
            FILE_VALID.store(false, Ordering::Relaxed);
            // Re-mark blocks dirty so the next save retries.
            for (i, w) in snapshot.iter().enumerate() {
                DIRTY[i].fetch_or(*w, Ordering::Relaxed);
            }
            return;
        }

        let mut written_blocks: u32 = 0;
        let mut had_error = false;
        for word_idx in 0..NUM_BITMAP_WORDS {
            let mut bits = snapshot[word_idx];
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let block = word_idx * 32 + bit;
                let off = block * BLOCK_SIZE;
                if sh_seek(fh, off as i64) != 0 {
                    kprintln!(
                        "flash_persist: SYS_SEEK to {:#x} failed; marking file invalid",
                        off
                    );
                    had_error = true;
                    break;
                }
                // SAFETY: read-only view of GUEST_FLASH for the
                // duration of the write call; bounded by SIZE.
                let block_bytes = unsafe {
                    core::slice::from_raw_parts(
                        (peripherals::flash::host_pa() as *const u8).add(off),
                        BLOCK_SIZE,
                    )
                };
                let unwritten = sh_write(fh, block_bytes);
                if unwritten != 0 {
                    kprintln!(
                        "flash_persist: SYS_WRITE short ({} unwritten) at block {}; marking file invalid",
                        unwritten, block
                    );
                    had_error = true;
                    break;
                }
                written_blocks += 1;
            }
            if had_error {
                break;
            }
        }
        sh_close(fh);

        if had_error {
            FILE_VALID.store(false, Ordering::Relaxed);
            // Re-mark every block from this save so the next pass
            // retries (including the ones we did succeed on — cheap
            // insurance, the next save is a full 8 MiB anyway).
            for (i, w) in snapshot.iter().enumerate() {
                DIRTY[i].fetch_or(*w, Ordering::Relaxed);
            }
            return;
        }

        if written_blocks > 0 {
            kprintln!(
                "flash_persist: wrote {} dirty 64 KiB block(s) to {}",
                written_blocks,
                path_str()
            );
        }
    }

    fn fingerprint(&self) -> u32 {
        // FNV-1a-32 over all 8 MiB. Matches the helper in snapshot.rs's
        // rom_fingerprint but spans the full backing.
        // SAFETY: static mut byte array; single-threaded EL2.
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

pub static BACKEND: SemihostBackend = SemihostBackend;

// ---- semihosting primitives --------------------------------------

#[inline]
unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
    let result: u64;
    // SAFETY: HLT #0xF000 is the AArch64 semihosting trap; QEMU/FVP
    // intercepts and returns through x0.
    unsafe {
        asm!(
            "hlt #0xF000",
            inout("x0") op => result,
            in("x1") arg as u64,
            options(nostack, preserves_flags),
        );
    }
    result as i64
}

fn sh_open(path: &[u8], mode: u64) -> i64 {
    let args: [u64; 3] = [path.as_ptr() as u64, mode, (path.len() - 1) as u64];
    unsafe { semihost(SYS_OPEN, args.as_ptr()) }
}

fn sh_close(fh: i64) {
    let args: [u64; 1] = [fh as u64];
    let _ = unsafe { semihost(SYS_CLOSE, args.as_ptr()) };
}

/// Returns 0 on full success, otherwise number of bytes left unread.
fn sh_read(fh: i64, buf: &mut [u8]) -> i64 {
    let args: [u64; 3] = [fh as u64, buf.as_mut_ptr() as u64, buf.len() as u64];
    unsafe { semihost(SYS_READ, args.as_ptr()) }
}

/// Returns 0 on full success, otherwise number of bytes left unwritten.
fn sh_write(fh: i64, data: &[u8]) -> i64 {
    let args: [u64; 3] = [fh as u64, data.as_ptr() as u64, data.len() as u64];
    unsafe { semihost(SYS_WRITE, args.as_ptr()) }
}

fn sh_seek(fh: i64, pos: i64) -> i64 {
    let args: [u64; 2] = [fh as u64, pos as u64];
    unsafe { semihost(SYS_SEEK, args.as_ptr()) }
}

fn sh_flen(fh: i64) -> i64 {
    let args: [u64; 1] = [fh as u64];
    unsafe { semihost(SYS_FLEN, args.as_ptr()) }
}

fn ensure_dir() {
    if MKDIR_DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    // `mkdir -p $FLASH_DIR\0` — the leading "mkdir -p " is constant, but
    // we need a NUL-terminated buffer for SYS_SYSTEM. Build it on the
    // stack; FLASH_DIR is a build-time literal so it fits.
    const PREFIX: &[u8] = b"mkdir -p ";
    let dir = FLASH_DIR.as_bytes();
    let mut buf = [0u8; 512];
    let total = PREFIX.len() + dir.len() + 1; // +1 for trailing NUL
    if total > buf.len() {
        kprintln!(
            "flash_persist: FLASH_DIR too long ({} bytes) — skipping mkdir; \
             you may need to create {} manually",
            dir.len(),
            FLASH_DIR
        );
        return;
    }
    buf[..PREFIX.len()].copy_from_slice(PREFIX);
    buf[PREFIX.len()..PREFIX.len() + dir.len()].copy_from_slice(dir);
    // buf[PREFIX.len() + dir.len()] is already 0 (trailing NUL).
    let args: [u64; 2] = [buf.as_ptr() as u64, (total - 1) as u64];
    let _ = unsafe { semihost(SYS_SYSTEM, args.as_ptr()) };
}

fn path_str() -> &'static str {
    // Strip trailing NUL for human-readable logging.
    let len = FLASH_PATH.len() - 1;
    core::str::from_utf8(&FLASH_PATH[..len]).unwrap_or("?")
}
