//! ROM-symbol lookup, always available (independent of the `trace`
//! feature gate around `mod tracer`).
//!
//! `build.rs` reads `scripts/classify-out/code-symbols.txt` (the
//! curated code-only address list) and `../_Data_/symbols.txt` (the
//! mangled-name source) and emits three files into `OUT_DIR`:
//!
//!   - `fn_addrs.bin`     — packed u32 LE, sorted by entry address.
//!   - `fn_name_offs.bin` — packed u32 LE, parallel to fn_addrs;
//!                          each entry is the byte offset into
//!                          `fn_names.bin` of the corresponding
//!                          NUL-terminated function name.
//!   - `fn_names.bin`     — concatenated NUL-terminated names.
//!
//! The tracer (when its feature is on) and the task-dump stack
//! walker (always on) both want PC → name lookups. This module is
//! the shared backing.
//!
//! ## Why the symbol blob ships in every image (diag-M6)
//!
//! These three `include_bytes!` constants are linked into rodata of
//! *every* build, including real-hardware `pi-bare-metal-*` images
//! where there is no host debugger and the only failure channel is a
//! UART halt dump. That is deliberate: when the hypervisor loud-halts
//! on hardware, `task_dump`'s stack walker turns raw return addresses
//! into `name+0xNN` frames via `fn_name_for_pc`, which is the
//! difference between an actionable backtrace and a column of bare
//! hex. The bytes are worth it.
//!
//! Measured cost, `--no-default-features --features pi-bare-metal-input`
//! (build.rs's `symbol table` line + the on-disk OUT_DIR blobs):
//!   - 18925 function entries
//!   - fn_addrs.bin     =  75_700 bytes (4 B/entry, sorted u32 addrs)
//!   - fn_name_offs.bin =  75_700 bytes (4 B/entry, parallel offsets)
//!   - fn_names.bin     = 609_289 bytes (concatenated NUL-term names)
//!   - total            = 760_689 bytes (~743 KiB of rodata)
//! The image already embeds the full 8 MiB Newton ROM, so this is a
//! ~9% rodata add for the backtrace capability — a trade we keep.

const FN_ADDRS_RAW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_addrs.bin"));
const FN_NAME_OFFS_RAW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_name_offs.bin"));
const NAME_POOL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_names.bin"));

pub const FN_COUNT: usize = FN_ADDRS_RAW.len() / 4;

fn read_u32_le(slice: &[u8], i: usize) -> u32 {
    let o = i * 4;
    u32::from_le_bytes([slice[o], slice[o + 1], slice[o + 2], slice[o + 3]])
}

pub fn fn_addr(i: usize) -> u32 { read_u32_le(FN_ADDRS_RAW, i) }
pub fn fn_name_off(i: usize) -> usize { read_u32_le(FN_NAME_OFFS_RAW, i) as usize }

pub fn fn_name(i: usize) -> &'static str {
    let start = fn_name_off(i);
    let mut end = start;
    while end < NAME_POOL.len() && NAME_POOL[end] != 0 {
        end += 1;
    }
    core::str::from_utf8(&NAME_POOL[start..end]).unwrap_or("<non-utf8>")
}

/// Look up the function whose entry address is the largest one ≤ `pc`.
/// Returns `(entry_addr, name)` on hit, `None` if `pc` is below the
/// first function or if the symbol table is empty.
///
/// The classifier-vetted symbol list is sorted by address at build
/// time, so this is a straight binary search. The caller decides
/// what's "near enough": `pc - entry_addr` larger than a few hundred
/// bytes usually means we fell into a region with no nearby code
/// symbol (jump-table thunk, REx data area, etc.) and the name is
/// misleading. Stack-trace output renders that as `name+0xNN` so
/// the offset is visible.
pub fn fn_name_for_pc(pc: u32) -> Option<(u32, &'static str)> {
    if FN_COUNT == 0 { return None; }
    let mut lo = 0usize;
    let mut hi = FN_COUNT;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if fn_addr(mid) <= pc { lo = mid + 1; } else { hi = mid; }
    }
    if lo == 0 { return None; }
    let i = lo - 1;
    Some((fn_addr(i), fn_name(i)))
}
