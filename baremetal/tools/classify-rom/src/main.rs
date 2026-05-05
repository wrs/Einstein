//! Host-side byte/halfword-access classifier for Newton 2.x ROM + Einstein.rex.
//!
//! Produces a single bitmap, one bit per 32-bit word across 16 MiB of guest
//! ROM space (0..0x01000000):
//!
//!   byte-access-static.bitmap — bit set iff the word is reachable as code
//!   AND decodes as an endianness-sensitive subword access (LDRB / STRB /
//!   LDRH / STRH / LDRSB / LDRSH / SWPB).
//!
//! This is the authoritative patch list for the endianness pre-patching
//! pass. By construction every bit set corresponds to an instruction that
//! baremetal/src/shadow_stub.rs::decode would accept.
//!
//! Invariant (when the oracle bitmap is present): every bit set in
//! byte-access.bitmap (oracle, from NewtonProbe) must be set in
//! byte-access-static.bitmap. A violation is either a walker reachability
//! gap or a decoder-alignment bug and is reported as a hard error.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ROM_SIZE_BYTES: usize = 16 * 1024 * 1024;
const ROM_WORD_COUNT: usize = ROM_SIZE_BYTES / 4;
const BITMAP_BYTES: usize = ROM_WORD_COUNT / 8;
const REX_PA_OFFSET: usize = 0x0080_0000;

struct Args {
    rom: PathBuf,
    rex: PathBuf,
    symbols: PathBuf,
    out_root: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut rom: Option<PathBuf> = None;
    let mut rex: Option<PathBuf> = None;
    let mut symbols: Option<PathBuf> = None;
    let mut out_root: Option<PathBuf> = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let take = |it: &mut std::iter::Skip<std::env::Args>, flag: &str| -> Result<PathBuf, String> {
            it.next().map(PathBuf::from)
                .ok_or_else(|| format!("{flag} requires a path argument"))
        };
        match a.as_str() {
            "--rom" => rom = Some(take(&mut it, "--rom")?),
            "--rex" => rex = Some(take(&mut it, "--rex")?),
            "--symbols" => symbols = Some(take(&mut it, "--symbols")?),
            "--out" => out_root = Some(take(&mut it, "--out")?),
            "-h" | "--help" => {
                eprintln!("classify-rom --rom <path> --rex <path> --symbols <path> --out <dir>");
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }

    Ok(Args {
        rom: rom.ok_or("--rom required")?,
        rex: rex.ok_or("--rex required")?,
        symbols: symbols.ok_or("--symbols required")?,
        out_root: out_root.ok_or("--out required")?,
    })
}

fn fnv1a_32(bytes: &[u8], seed: u32) -> u32 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Load ROM (8 MiB) + REX (up to 8 MiB) into a 16 MiB little-endian word view.
/// Each on-disk word is stored MSB-first; `from_be_bytes` yields that word
/// directly as a host u32 (the value the ARM guest reads at that PC in LE
/// mode). See baremetal/src/guest_mem.rs:509-519 for the reference byteswap.
fn load_rom_and_rex(rom_path: &Path, rex_path: &Path) -> Result<(Vec<u32>, u32), String> {
    let rom = fs::read(rom_path).map_err(|e| format!("read {}: {}", rom_path.display(), e))?;
    let rex = fs::read(rex_path).map_err(|e| format!("read {}: {}", rex_path.display(), e))?;

    if rom.len() > REX_PA_OFFSET {
        return Err(format!("rom {} bytes exceeds low 8 MiB window", rom.len()));
    }
    if rex.len() > ROM_SIZE_BYTES - REX_PA_OFFSET {
        return Err(format!("rex {} bytes exceeds high 8 MiB window", rex.len()));
    }

    let hash = fnv1a_32(&rex, fnv1a_32(&rom, 0x811C_9DC5));

    let mut words = vec![0u32; ROM_WORD_COUNT];
    for (i, w) in words.iter_mut().enumerate().take(rom.len() / 4) {
        let o = i * 4;
        *w = u32::from_be_bytes([rom[o], rom[o + 1], rom[o + 2], rom[o + 3]]);
    }
    let rex_word_base = REX_PA_OFFSET / 4;
    for i in 0..(rex.len() / 4) {
        let o = i * 4;
        words[rex_word_base + i] = u32::from_be_bytes([rex[o], rex[o + 1], rex[o + 2], rex[o + 3]]);
    }

    Ok((words, hash))
}

/// Counters returned by `load_symbol_roots` so `run()` can surface
/// per-filter drop counts in the summary report.
#[derive(Default, Debug)]
struct SymbolFilterStats {
    raw_lines: usize,
    parse_skipped: usize,
    misaligned: usize,
    out_of_rom: usize,
    linker_marker_dropped: usize,
    g_prefix_dropped: usize,
    k_prefix_dropped: usize,
    nv_cond_dropped: usize,
    not_prologue_dropped: usize,
    data_stop_dropped: usize,
    jt_va_resolved: usize,
    accepted: usize,
}

/// Parse `_Data_/symbols.txt` and return the code-root list.
///
/// Filters applied (in order):
///   1. Linker-marker names ($$, Image$, $Size, $Length, $Base, $Limit,
///      $End, $ZI) — section synthetic labels, never code.
///   2. `^g[A-Z]` / `^k[A-Z]` name prefixes — Newton convention for
///      global variables and constants. Seeding these as code roots
///      risks data getting marked code-reachable, and under BE-8 every
///      code-marked word is byteswapped at load — corrupting the
///      kernel's data reads.
///   3. Word-aligned address.
///   4. Address must resolve to an in-ROM PA — direct PAs (addr <
///      ROM_SIZE_BYTES) are accepted unchanged; JT-VA addresses (the
///      patch-table 0x01A00000..0x01C20000 range, gROMPublicJumpTable,
///      secondary-jt) are translated through `resolve_target_to_rom`
///      to their underlying thunk PA. Anything outside those ranges
///      is dropped.
///   5. NV-cond (cond=0xF, deprecated in ARMv7+) on the first word —
///      cheap sanity filter against obvious garbage seeds.
///
/// No prologue-shape gate: the walker is self-correcting on data
/// (NV-cond / Step::Stop terminate fast), and the name-prefix rules
/// already filter the dominant data-symbol classes upstream.
fn load_symbol_roots(
    path: &Path,
    words: &[u32],
) -> Result<(Vec<(u32, &'static str)>, SymbolFilterStats), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut out: Vec<(u32, &'static str)> = Vec::new();
    let mut stats = SymbolFilterStats::default();

    for line in text.lines() {
        stats.raw_lines += 1;
        let mut addr_s: Option<&str> = None;
        let mut name: Option<&str> = None;
        for (i, f) in line.split('\t').enumerate() {
            let t = f.trim();
            if t.is_empty() { continue; }
            if addr_s.is_none()
                && (t.starts_with("0x") || t.starts_with("0X"))
            {
                addr_s = Some(t);
                continue;
            }
            if addr_s.is_some() && name.is_none() {
                name = Some(t);
                break;
            }
            if addr_s.is_none() && i == 0 && t.parse::<u64>().is_ok() {
                continue;
            }
        }
        let (addr_s, name) = match (addr_s, name) {
            (Some(a), Some(n)) => (a, n),
            _ => { stats.parse_skipped += 1; continue; }
        };

        let hex = addr_s.strip_prefix("0x").or_else(|| addr_s.strip_prefix("0X"))
            .unwrap_or(addr_s);
        let addr = match u32::from_str_radix(hex, 16) {
            Ok(a) => a,
            Err(_) => { stats.parse_skipped += 1; continue; }
        };

        if addr & 3 != 0 { stats.misaligned += 1; continue; }

        if name.contains("$$")
            || name.starts_with("Image$")
            || name.ends_with("$Size")
            || name.ends_with("$Length")
            || name.ends_with("$Base")
            || name.ends_with("$Limit")
            || name.ends_with("$End")
            || name.ends_with("$ZI")
        {
            stats.linker_marker_dropped += 1;
            continue;
        }

        // `^g[A-Z0-9]` / `^k[A-Z0-9]`: Newton convention for global
        // variables (`gInitial...`, `g8MegContinuousTable...`) and
        // constants (`kEntriesPerSlot`, ...). Digit second-char matters:
        // `g8MegContinuousTableStartFor4MbK` would slip through an
        // uppercase-only test. Lowercase second-char (`getFoo`,
        // `gsm_add`, `glitch_to_*`) is a real C-function convention,
        // keep those.
        let bytes = name.as_bytes();
        if bytes.len() >= 2
            && (bytes[1].is_ascii_uppercase() || bytes[1].is_ascii_digit())
        {
            if bytes[0] == b'g' { stats.g_prefix_dropped += 1; continue; }
            if bytes[0] == b'k' { stats.k_prefix_dropped += 1; continue; }
        }

        // Resolve the symbol's address to an in-ROM PA. Direct PAs pass
        // through as-is; JT-VA addresses are translated to their thunk
        // PA. Anything that doesn't resolve is outside the address space
        // we can mark.
        let pa = match resolve_target_to_rom(words, addr) {
            Some(p) => p,
            None => { stats.out_of_rom += 1; continue; }
        };
        if (addr as usize) >= ROM_SIZE_BYTES {
            stats.jt_va_resolved += 1;
        }

        let w = words[(pa >> 2) as usize];
        if (w >> 28) == 0xF { stats.nv_cond_dropped += 1; continue; }

        // Drop symbol roots that fall into a known data range. Newton's
        // demangled-symbol set includes data labels like `PublicFiller`
        // whose first word coincidentally decodes as a known ARM
        // instruction shape (`STRB r0, [r0], -r0, lsl #8` here). Without
        // this gate the walker linearly walks ~90 KiB of bp-weight data,
        // pushing branch targets into NSRuntime / package data and
        // producing thousands of false-positive code marks.
        if in_data_stop_range(pa) { stats.data_stop_dropped += 1; continue; }

        // Prologue gate. Despite the appeal of "the walker is self-
        // correcting on data", the walker is *not* self-correcting on
        // data-symbol seeds: a name like `DataAreaTable` (whose first
        // word is `0x64617461` = 'data' ASCII, cond=VS) hits no
        // termination in `step()` and walks linearly through the
        // entire data region — turning ~80 bytes of table into
        // false-positive code marks (which under BE-8 then byteswap
        // the kernel's data reads). The prologue gate is the only
        // filter that catches cond != AL on the first word, where
        // most data symbols live. Real code with cond != AL on its
        // first instruction (rare predicated tail-calls) is the
        // tradeoff — none observed in the boot path so far.
        if !is_known_function_start(w) { stats.not_prologue_dropped += 1; continue; }

        if seen.insert(pa) {
            // Leak the name to get a `&'static str` we can carry in
            // SeedSource::Symbol without having to plumb a name table
            // through every collector. The list is bounded (~36k accepted
            // symbols) so the leak doesn't grow without bound.
            let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
            out.push((pa, leaked));
            stats.accepted += 1;
        }
    }
    out.sort_unstable_by_key(|&(pa, _)| pa);
    Ok((out, stats))
}

/// Parse the REx block header at guest PA 0x00800000 and harvest
/// code entry-point addresses that the Newton kernel hands to the
/// `RExScanner` path: the `fdrv` config entry carries an absolute PA
/// to the flash driver's class info, embedded `pkgl` packages each
/// bundle their own dispatch tables, etc. Returns every PA we can
/// reasonably seed as a potential code root.
///
/// Header layout (big-endian in the file; `words` below already holds
/// the byteswapped LE view):
///     +0x00 "RExBlock" magic (8 bytes)
///     +0x08 checksum
///     +0x0C header version (=1)
///     +0x10 manufacturer ('Eins')
///     +0x14 REx ID
///     +0x18 block size
///     +0x1C unknown
///     +0x20 nominal load PA (0x00800000)
///     +0x24 num-entries
///     +0x28 entries[N] of { tag: u32, offset: u32, size: u32 }
///
/// Offsets inside the entry table are REx-relative (i.e. add
/// `REX_PA_OFFSET` to get an absolute PA).
fn rex_header_roots(words: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let rex_base_w = REX_PA_OFFSET / 4;
    if rex_base_w + 10 >= ROM_WORD_COUNT { return out; }
    let w0 = words[rex_base_w];
    let w1 = words[rex_base_w + 1];
    // "RExBlock" = 0x52457842 'RExB' / 0x6c6f636b 'lock'
    if w0 != 0x5245_7842 || w1 != 0x6c6f_636b { return out; }

    let num_entries = words[rex_base_w + 9] as usize;
    // Clamp — if the field is garbage (mis-aligned REx, truncated
    // file) we don't want to walk off the end.
    let max_entries = 64usize;
    let n = num_entries.min(max_entries);
    for i in 0..n {
        let ent_w = rex_base_w + 10 + i * 3;
        if ent_w + 2 >= ROM_WORD_COUNT { break; }
        let tag = words[ent_w];
        let off = words[ent_w + 1];
        let size = words[ent_w + 2];
        let data_pa = (REX_PA_OFFSET as u32).wrapping_add(off);
        let data_end = data_pa.saturating_add(size);
        // DO NOT seed entry-data PAs directly: they always point at
        // configuration structures, not at instructions. Seeding
        // 0x00800054 (the FDRV class info) pulls the walker into
        // binary data where integer-offset values happen to decode
        // as byte/halfword-shape instructions.
        match tag {
            // 'fdrv' — 8-byte config entry. Layout: +0 version (0x01),
            // +4 absolute PA pointing at the class-info block.
            0x6664_7276 => {
                let data_idx = (data_pa as usize) >> 2;
                if data_idx + 1 < ROM_WORD_COUNT {
                    let pa = words[data_idx + 1];
                    if pa & 3 == 0 && (pa as usize) < ROM_SIZE_BYTES {
                        // Still data (the class info block), but
                        // immediately followed by pointer-shaped
                        // words — let FDRV handler below handle it.
                        // Don't seed the class-info PA itself as code.
                        let _ = pa;
                    }
                }
            }
            // 'FDRV' — class info structure. Scan every word-aligned
            // slot for values that look like code pointers (point at
            // a prologue-shaped target); each such pointer is a real
            // method root.
            0x4644_5256 => {
                let start = (data_pa as usize) >> 2;
                let end = ((data_end as usize) >> 2).min(ROM_WORD_COUNT);
                for w in start..end {
                    let p = words[w];
                    if p == 0 { continue; }
                    // FDRV class-info method pointers may be direct
                    // ROM PAs or patch-table VAs.
                    let final_tgt = match resolve_target_to_rom(words, p) {
                        Some(t) => t,
                        None => continue,
                    };
                    let tgt_idx = (final_tgt >> 2) as usize;
                    let tgt = words[tgt_idx];
                    if (tgt >> 28) == 0xF { continue; }
                    if !is_known_function_start(tgt) { continue; }
                    out.push(final_tgt);
                }
            }
            // 'pkgl' — embedded package list. Package internal layout
            // (NSRuntime header, ObjectPool, etc.) isn't parsed here;
            // any executable package init points get reached via
            // direct BL from the FDRV methods that invoke them.
            _ => {}
        }
    }
    out
}

/// Parse `_Data_/symbols.txt` for `<prefix>$$Base` / `<prefix>$$Limit`
/// pairs and return one `(base, limit)` range per matching `<prefix>`.
/// Restricted to prefixes ending in `vec` so we only catch ARM linker
/// vectors of function pointers (C$$ctorvec, C$$dtorvec, ...) — Image$
/// and Load$ section markers don't carry function pointers.
///
/// Each emitted range delimits an array of function pointers (one
/// 32-bit ROM PA per slot). The walker uses the range to seed every
/// pointer's target as a code root; without these the ctor / dtor
/// bodies are unreachable by any control-flow pass (the kernel's
/// runtime invokes them via the vector, not via static B/BL).
fn load_vector_ranges(path: &Path) -> Result<Vec<(String, u32, u32)>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut bases: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut limits: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for line in text.lines() {
        let mut parts = line.split('\t').map(str::trim).filter(|s| !s.is_empty());
        let addr_s = match parts.next() { Some(s) => s, None => continue };
        let name = match parts.next() { Some(s) => s, None => continue };
        let hex = addr_s.strip_prefix("0x").or_else(|| addr_s.strip_prefix("0X"))
            .unwrap_or(addr_s);
        let addr = match u32::from_str_radix(hex, 16) { Ok(a) => a, Err(_) => continue };
        if (addr as usize) >= ROM_SIZE_BYTES || addr & 3 != 0 { continue; }
        if let Some(prefix) = name.strip_suffix("$$Base") {
            if prefix.ends_with("vec") { bases.insert(prefix.to_string(), addr); }
        } else if let Some(prefix) = name.strip_suffix("$$Limit") {
            if prefix.ends_with("vec") { limits.insert(prefix.to_string(), addr); }
        }
    }
    let mut out: Vec<(String, u32, u32)> = Vec::new();
    for (prefix, base) in bases {
        if let Some(&limit) = limits.get(&prefix) {
            if limit > base { out.push((prefix, base, limit)); }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

/// Seed every function-pointer target inside the supplied vector
/// ranges. Each word in `[base, limit)` is read as a ROM PA; targets
/// gated by `is_known_function_start` are pushed as worklist roots.
/// Returns (vector_count, seeds_added) for summary reporting.
fn seed_vector_table_roots(
    words: &[u32],
    ranges: &[(String, u32, u32)],
    worklist: &mut Vec<(u32, SeedSource)>,
) -> (usize, usize) {
    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    for (_label, base, limit) in ranges {
        let mut pa = *base;
        while pa < *limit {
            let p = words[(pa >> 2) as usize];
            pa = pa.wrapping_add(4);
            if p & 3 != 0 || (p as usize) >= ROM_SIZE_BYTES || p == 0 { continue; }
            let tw = words[(p >> 2) as usize];
            if (tw >> 28) == 0xF { continue; }
            if !is_known_function_start(tw) { continue; }
            if seen.insert(p) {
                worklist.push((p, SeedSource::Vector));
                added += 1;
            }
        }
    }
    (ranges.len(), added)
}

/// Manual code-root exceptions for code that no static-analysis pass
/// can reach. Each entry must be a real instruction PC; the walker
/// seeds it as a worklist root and continues from there. Use sparingly
/// — every entry is a hand-fixed gap and a maintenance liability.
const MANUAL_CODE_ROOTS: &[u32] = &[
    // FP-library trap-dispatched error paths. The IEEE-754 special-case
    // handlers in `sqrt` (0x3823b0..0x3823d8) and `_ldfp`'s post-trap
    // continuation (0x382418..0x38242c) are reached only via the FPE
    // trap dispatch — no static B/BL or table reference exists.
    // Seeding the `_ldfp` continuation entry transitively pulls in all
    // four sqrt error paths (each falls through `b 0x3823dc` into
    // already-reached cleanup) and the three conditional branches
    // _ldfp uses to enter them.
    0x0038_2418,
];

/// Manual data-range exceptions: half-open `[start, end)` ranges that
/// the post-walk pass clears from `reach`, no matter what the walker
/// or any indirect-pass collector did. Use for tiny corrupt corners
/// where static analysis follows a clearly-broken or never-executed
/// branch into a data region — fixing the walker to refuse the path
/// would risk dropping legitimate coverage elsewhere.
const MANUAL_DATA_RANGES: &[(u32, u32)] = &[
    // DiagHook (PA 0x184d0, hand-written asm) contains `beq 0x1862c`
    // at PC 0x185a4 whose target lands inside DiagHook's own literal
    // pool — appears to be a bug; DiagHook only runs in diagnostic
    // mode (hardware-switch-asserted boot) so the path is dead on a
    // normal boot. The walker follows it statically, marks the
    // literal pool, then walks past into the trailing "STUB ROM" /
    // "diagRExBlock" header. `clear_literal_pool_targets_from_reach`
    // cleans the literal pool itself; this range covers the trailing
    // header words that aren't LDR-pc-rel targets.
    (0x0001_8668, 0x0001_8688),
];

/// "ROM soup": the post-code region of the 717006 ROM that contains
/// only data — neural-net weights, lex/recognition tables, NSRuntime
/// package metadata, REx-internal structures, etc. The last real
/// code symbol is `TCountXrAsm` at 0x003ad244 (followed by a small
/// asm trailer); the user-defined cutoff is 0x003afda8 to be safe.
/// The REx aperture starts at 0x00800000.
///
/// Any walk that reaches an address in this range is by definition
/// a classifier failure — either a symbol root that should be data,
/// or an indirect-pass collector following a misdecoded structure.
/// The walker logs every entry to stderr so the offending root /
/// trace stack can be tracked down.
const ROM_SOUP_RANGE: (u32, u32) = (0x003a_fda8, 0x0080_0000);

fn in_rom_soup(addr: u32) -> bool {
    addr >= ROM_SOUP_RANGE.0 && addr < ROM_SOUP_RANGE.1
}

/// Half-open `[start, end)` ranges of ROM that contain no code at all.
/// The walker terminates on entry (not just clears bits afterwards),
/// preventing the indirect cascades data-as-code walks produce: e.g.
/// PublicFiller (0x003948e4, byte-shape first word) seeded as a code
/// root walks linearly through the bp-weight tables, eventually
/// decodes a random data word as `bne 0x7a0dbc`, pushes a worklist
/// root in the NSRuntime data region, and so on.
///
/// These ranges mirror `classify-symbols.py`'s `DATA_RANGES` —
/// regions verified by inspection / bucket cross-check to contain
/// zero code symbols.
const DATA_STOP_RANGES: &[(u32, u32)] = &[
    // Recognition tables + modem constants + DES tables + IrDA
    // tables + charsets + yacc tables + C runtime globals +
    // QuickDraw pattern data. (`classify-symbols.py:DATA_RANGES[0]`)
    (0x0036_6f2c, 0x0038_2324),
    // Inline ASCII string "SVC mode in MonitorEntryGlue\0" embedded
    // at the tail of MonitorEntryGlue (0x00394318). The walker can
    // fall into it because the preceding DebuggerUND call doesn't
    // terminate the basic block cleanly.
    // (`classify-symbols.py:DATA_RANGES[2]`)
    (0x0039_433c, 0x0039_435c),
    // PublicFiller (0x003948e4) + bpWeight (0x003948f0) + ~90 KiB
    // back-propagation weight table + newtConnects data table at
    // 0x003aace4. Code resumes at TCountXrAsm (0x003ad244).
    // (`classify-symbols.py:DATA_RANGES[1]`)
    (0x0039_48e4, 0x003a_d244),
    // NSRuntime / package metadata: the bytes between the last
    // ROM-driver class info trampoline run (~0x007a4xxx) and the
    // first ClassInfo SRO struct at 0x007a4a30. The walker can
    // get here via misdecoded `bne 0x7a0dbc` / `bne 0x7a11fc`
    // type branches in the bp-weight data above. Bounded so the
    // legitimate ROM-driver class-info trampolines at 0x007a5xxx+
    // are still discovered.
    (0x007a_0dbc, 0x007a_1200),
];

fn in_data_stop_range(addr: u32) -> bool {
    DATA_STOP_RANGES.iter().any(|&(lo, hi)| addr >= lo && addr < hi)
}

/// True iff `w` looks like the first instruction of a function entry —
/// any AL-cond word (top nibble 0xE) decoding as a known ARM
/// instruction class. Used by indirect-target recovery (function-
/// pointer literals, vtable method slots, dispatch-table entries) to
/// distinguish real code addresses from data constants. Conditional
/// (cond != AL) first instructions exist (tail-called leaves) but
/// data-constant false positives dominate that case, so cond=AL is
/// the conservative cutoff. Mirrors `classify-symbols.py` —
/// `is_known_function_start` accepts top3 ∈ {000..101, 111}.
/// 110 (coproc LDC/STC) is rare as a first insn and excluded.
fn is_known_function_start(w: u32) -> bool {
    if (w >> 28) & 0xF != 0xE { return false; }
    let top3 = (w >> 25) & 0b111;
    if top3 <= 0b101 { return true; }
    if top3 == 0b111 { return true; }
    false
}

/// Byte/halfword-access decoder. MUST match the acceptance set of
/// baremetal/src/shadow_stub.rs::decode (lines 259-377). Divergence is
/// caught by the oracle ⊆ static invariant check at run end.
///
/// Returns true iff `insn` is an endianness-sensitive subword access that
/// the patcher would accept.
fn is_byte_access(insn: u32) -> bool {
    let cond = (insn >> 28) & 0xF;
    if cond == 0xF {
        return false;
    }

    // Form 1a: LDRB/STRB immediate (bits[27:25]=010, B=1).
    if (insn & 0x0E00_0000) == 0x0400_0000 && (insn & (1 << 22)) != 0 {
        return true;
    }
    // Form 1b: LDRB/STRB register, bit 4 == 0 (bits[27:25]=011, B=1).
    if (insn & 0x0E00_0010) == 0x0600_0000 && (insn & (1 << 22)) != 0 {
        return true;
    }

    // Form 2: extra load/store (halfword / signed byte / signed halfword).
    // Keyed on bits[27:25]=000, bit 7=1, bit 4=1, op=(bits[6:5])!=0.
    // Excludes LDRD (op=10, L=0) and STRD (op=11, L=0) — shadow_stub::decode
    // returns None for those.
    if (insn & 0x0E00_0090) == 0x0000_0090 {
        let op = (insn >> 5) & 0x3;
        let l = (insn >> 20) & 1 != 0;
        return match (op, l) {
            (0b01, _) => true,           // LDRH / STRH
            (0b10, true) => true,        // LDRSB
            (0b10, false) => false,      // LDRD
            (0b11, true) => true,        // LDRSH
            (0b11, false) => false,      // STRD
            _ => false,                  // op=00 falls to sync primitives
        };
    }

    // Form 3: SWPB. cond 0001 0100 Rn Rt 0000 1001 Rm.
    // shadow_stub::decode refuses Rt == Rm (UNPREDICTABLE); match that.
    if (insn & 0x0FF0_0FF0) == 0x0140_0090 {
        let rt = (insn >> 12) & 0xF;
        let rm = insn & 0xF;
        return rt != rm;
    }

    false
}

struct Bitmap {
    bits: Vec<u8>,
}

impl Bitmap {
    fn new() -> Self { Self { bits: vec![0u8; BITMAP_BYTES] } }

    fn from_bytes(b: Vec<u8>) -> Result<Self, String> {
        if b.len() != BITMAP_BYTES {
            return Err(format!("expected {} bytes, got {}", BITMAP_BYTES, b.len()));
        }
        Ok(Self { bits: b })
    }

    fn set_word(&mut self, addr: u32) {
        if (addr as usize) >= ROM_SIZE_BYTES || addr & 3 != 0 { return; }
        let idx = (addr >> 2) as usize;
        self.bits[idx >> 3] |= 1u8 << (idx & 7);
    }

    fn get_word(&self, addr: u32) -> bool {
        if (addr as usize) >= ROM_SIZE_BYTES || addr & 3 != 0 { return false; }
        let idx = (addr >> 2) as usize;
        (self.bits[idx >> 3] >> (idx & 7)) & 1 != 0
    }

    fn clear_word(&mut self, addr: u32) {
        if (addr as usize) >= ROM_SIZE_BYTES || addr & 3 != 0 { return; }
        let idx = (addr >> 2) as usize;
        self.bits[idx >> 3] &= !(1u8 << (idx & 7));
    }

    fn popcount(&self) -> u64 {
        self.bits.iter().map(|b| b.count_ones() as u64).sum()
    }
}

enum Step {
    Continue { branch: Option<u32> },
    Jump(u32),
    Stop,
}

/// Provenance tag carried alongside every worklist entry so the walker
/// can answer "why was this address ever marked code?" when a mystery
/// bit shows up. The walker maintains a small per-walk stack of these
/// (seed reason + Step::Jump transitions) and dumps it when `cur` lands
/// on any address listed in `WALK_TRACE_ADDRS`.
///
/// Increments (`cur += 4`) are NOT pushed onto the stack — they're
/// implicit in the surrounding bracketing entries, and adding them
/// would balloon the trace and obscure the real decisions.
#[derive(Clone, Copy)]
enum WalkReason {
    /// Initial seed (root from symbols.txt, vectors, ctorvec/dtorvec,
    /// REx header parser, MANUAL_CODE_ROOTS, or one of the indirect-
    /// pass collectors below).
    Seed(SeedSource),
    /// Step::Continue { branch: Some(t) } — Bcc / BL pushed `t` to the
    /// worklist; this entry got popped later. `from_pc` is the PC of
    /// the branch instruction.
    Branch { from_pc: u32 },
    /// Step::Jump — the current walk's `cur` jumped here from a B (no
    /// link, not in_table). Recorded on the current walk's stack, not
    /// on the worklist.
    Jump { from_pc: u32 },
}

impl std::fmt::Debug for WalkReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkReason::Seed(s) => write!(f, "Seed({s:?})"),
            WalkReason::Branch { from_pc } => write!(f, "Branch from 0x{from_pc:08x}"),
            WalkReason::Jump { from_pc } => write!(f, "Jump from 0x{from_pc:08x}"),
        }
    }
}

/// Where a worklist seed originated. Each indirect-pass collector
/// stamps its pushes with the matching variant + the most useful
/// originating PC so the trace points at the exact source line.
#[derive(Clone, Copy)]
enum SeedSource {
    /// Symbol-table seed. Carries the symbol name (`&'static str`,
    /// leaked at parse time from the symbols.txt line) so the trace
    /// is greppable: e.g. `Seed(Symbol "PublicFiller_1")` vs the
    /// previous useless `Seed(Symbol)`.
    Symbol(&'static str),
    Vector,
    Manual,
    RexHeader,
    /// LDR-pc-rel + STR-to-this pair installs a vtable; `ldr_pc` is
    /// the LDR instruction that loaded the vtable address.
    Vtable { ldr_pc: u32 },
    /// LDR-pc-rel literal looked like a function pointer; `ldr_pc` is
    /// the LDR. Loaded literal = popped PC of this entry.
    FnPtrLiteral { ldr_pc: u32 },
    /// 3+-entry B-AL run discovered at `entry_pa`.
    BRunEntry { entry_pa: u32 },
    /// `add Rd, PC, #imm` reaching this address; `adr_pc` is that ADR.
    PcRelAddr { adr_pc: u32 },
    /// TClassInfo trampoline at `tramp_pc` (`sub r0, pc, #68; mov pc,
    /// lr; mov r0, #imm; mov pc, lr`).
    ClassInfoTramp { tramp_pc: u32 },
    ClassInfoBranchSlot { tramp_pc: u32 },
    ClassInfoBTable { tramp_pc: u32 },
    ClassInfoEntryProc { tramp_pc: u32 },
    /// `LDR pc, [Rn, Rm, lsl #2]` indexed dispatch at `dispatch_pc`.
    IndexedDispatch { dispatch_pc: u32 },
    /// PC-relative jump-table dispatch at `dispatch_pc`.
    JtTableEntry { dispatch_pc: u32 },
    JtTableTarget { dispatch_pc: u32 },
    /// `mov r0, pc; mov pc, lr` micro-trampoline immediately after a
    /// terminal `add pc, ip, #N` (or similar PC-write through ip / lr).
    /// Used by Newton class-info dispatch to expose a "get class name
    /// string pointer" alt entry: caller BLs the alt-entry, the
    /// `mov r0, pc` returns address+8 = first byte of the trailing
    /// ASCII class-name string. Static analysis can't see the BL when
    /// it's installed via vtable; this collector seeds the alt entry
    /// directly so the two instructions are still byteswapped at load.
    AltEntryAfterIpJump { trampoline_pc: u32 },
}

impl std::fmt::Debug for SeedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedSource::Symbol(name) => write!(f, "Symbol {name:?}"),
            SeedSource::Vector => write!(f, "Vector"),
            SeedSource::Manual => write!(f, "Manual"),
            SeedSource::RexHeader => write!(f, "RexHeader"),
            SeedSource::Vtable { ldr_pc } => write!(f, "Vtable @ ldr 0x{ldr_pc:08x}"),
            SeedSource::FnPtrLiteral { ldr_pc } => write!(f, "FnPtrLiteral @ ldr 0x{ldr_pc:08x}"),
            SeedSource::BRunEntry { entry_pa } => write!(f, "BRunEntry @ 0x{entry_pa:08x}"),
            SeedSource::PcRelAddr { adr_pc } => write!(f, "PcRelAddr @ adr 0x{adr_pc:08x}"),
            SeedSource::ClassInfoTramp { tramp_pc } => write!(f, "ClassInfoTramp @ 0x{tramp_pc:08x}"),
            SeedSource::ClassInfoBranchSlot { tramp_pc } => write!(f, "ClassInfoBranchSlot @ tramp 0x{tramp_pc:08x}"),
            SeedSource::ClassInfoBTable { tramp_pc } => write!(f, "ClassInfoBTable @ tramp 0x{tramp_pc:08x}"),
            SeedSource::ClassInfoEntryProc { tramp_pc } => write!(f, "ClassInfoEntryProc @ tramp 0x{tramp_pc:08x}"),
            SeedSource::IndexedDispatch { dispatch_pc } => write!(f, "IndexedDispatch @ 0x{dispatch_pc:08x}"),
            SeedSource::JtTableEntry { dispatch_pc } => write!(f, "JtTableEntry @ dispatch 0x{dispatch_pc:08x}"),
            SeedSource::JtTableTarget { dispatch_pc } => write!(f, "JtTableTarget @ dispatch 0x{dispatch_pc:08x}"),
            SeedSource::AltEntryAfterIpJump { trampoline_pc } => write!(f, "AltEntryAfterIpJump @ tramp 0x{trampoline_pc:08x}"),
        }
    }
}

/// Addresses to trace through the walker. When `cur` is about to mark
/// any of these, the walker prints the current trace stack to stderr
/// and continues. Add an address here, recompile, run; remove when
/// the mystery is solved. Empty by default.
const WALK_TRACE_ADDRS: &[u32] = &[];

fn sign_extend(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

/// If `w` is a manual-BL LR setup (`MOV LR, PC` or `ADD LR, PC, #imm`),
/// return `Some(imm)` where `imm` is the offset that the following
/// PC-write will return to: walker should treat the eventual jump as
/// a call returning at `lr_set_pc + 8 + imm`. `None` for any other
/// instruction.
///
/// MOV LR, PC: LR = PC+8 of this insn → imm = 0.
/// ADD LR, PC, #imm12 (cond=AL, opcode ADD, S=0, Rn=15, Rd=14):
///     LR = PC+8 + imm12. Used in protocol-call thunks where the
///     compiler interleaves intermediate insns between the LR setup
///     and the indirect jump (e.g. `add lr, pc, #4; ldr ip, [r0, #8];
///     add pc, ip, #12`); the imm offset accounts for the
///     intermediate insns so LR still lands at jump_pc + 4.
fn lr_setup_imm(w: u32) -> Option<u32> {
    if w == 0xE1A0_E00F { return Some(0); }       // MOV LR, PC
    if w == 0xE28F_E000 { return Some(0); }       // ADD LR, PC, #0
    // ADD LR, PC, #imm: cond=AL(E) 001 opcode=ADD(0100) S=0 Rn=15 Rd=14.
    // Mask: 0xFFFF_F000 = E28F_E000; check imm12 in the low 12 bits.
    if (w & 0xFFFF_F000) == 0xE28F_E000 {
        let rot = ((w >> 8) & 0xF) * 2;
        let val8 = w & 0xFF;
        let imm = val8.rotate_right(rot);
        return Some(imm);
    }
    None
}

/// Translate a ROM patch-table VA (0x01A00000..0x01C20880) to the
/// underlying ROM PA. Per `docs/NEWTON_INTERNALS.md`:
///
///   17 buckets of 0x20000 bytes at 0x01A00000 + B*0x20000, B ∈ 0..16.
///   Each bucket's 32 VA 4 KB pages all alias the same 4 KB phys
///   page at `0x2000 + B*0x1000`. Within a slot, only 0x80 valid
///   bytes live at offset P*0x80 (where P = 0..31 is the slot
///   index = which 4 KB VA page within the bucket).
///
/// For static analysis we only need the VA→phys mapping: the
/// kernel's stage-1 maps every VA in the bucket's 0x20000-byte
/// range to the same 4 KB phys page at the bucket's phys base, with
/// `va_offset_in_page == phys_offset_in_page`.
fn jt_va_to_phys(va: u32) -> Option<u32> {
    if va < 0x01A0_0000 || va >= 0x01C2_0880 { return None; }
    let off = va - 0x01A0_0000;
    let bucket = off / 0x2_0000;
    if bucket > 16 { return None; }
    let off_in_page = off & 0xFFF;
    Some(0x2000 + bucket * 0x1000 + off_in_page)
}

/// Translate a VA in the *secondary* ROM jump-table window
/// (0x01E00000..0x01F00000) to a ROM PA via the L2 page table at
/// `SECONDARY_JT_L2_PA`. The 717006 ROM ships a pre-built short-
/// descriptor L2 at PA 0x7EC000 with 256 small-page descriptors;
/// at boot the kernel programs `L1[0x01E] = coarse(0x7EC000)` and
/// every branch through VA 0x01E0xxxx walks via this L2. The
/// dominant entries (224 of 256) all alias to the single thunk page
/// at PA 0x7EE000 — a small per-thunk table of `B kernel_va_target`
/// instructions reached from PA 0x7a5618+ in the ROM-init driver
/// glue. We read the L2 directly so a different ROM revision with
/// a different alias layout still resolves correctly.
///
/// Returns `None` for VAs outside the window or when the L2 entry
/// isn't a small-page descriptor (type bits != 2/3).
const SECONDARY_JT_VA_BASE: u32 = 0x01E0_0000;
const SECONDARY_JT_VA_END:  u32 = 0x01F0_0000;
const SECONDARY_JT_L2_PA:   u32 = 0x007E_C000;
fn secondary_jt_va_to_phys(words: &[u32], va: u32) -> Option<u32> {
    if va < SECONDARY_JT_VA_BASE || va >= SECONDARY_JT_VA_END { return None; }
    let l2_idx = (va >> 12) & 0xFF;
    let entry_pa = SECONDARY_JT_L2_PA + l2_idx * 4;
    if (entry_pa as usize) + 4 > ROM_SIZE_BYTES { return None; }
    let entry = words[(entry_pa >> 2) as usize];
    // ARMv4 short-descriptor L2: bits[1:0] = 10 (small page) or 11
    // (small-page-XN-extended). Either is acceptable here.
    if (entry & 0b10) != 0b10 { return None; }
    let page_pa = entry & 0xFFFF_F000;
    let pa = page_pa | (va & 0xFFF);
    if (pa as usize) + 4 > ROM_SIZE_BYTES { return None; }
    Some(pa)
}

/// Translate a VA in the gROMPublicJumpTable window (1 MiB at
/// 0x01800000) to a ROM PA via `gROMPublicJumpTablePageTable` (PA
/// 0x18000). The kernel installs this L2 at L1[0x18] inside
/// `BuildPatchTablePageTable` (PA 0x183230) so a branch through
/// VA 0x0180xxxx fetches a thunk word from PA 0x13000+ (= the
/// gROMPublicJumpTable backing) — same idiom as the secondary
/// jump-table.
///
/// The B encoded in each thunk targets a patch-table VA when
/// decoded against this section's runtime PC, which then resolves
/// via `resolve_jt_va` to a real ROM function. Callers chain the
/// resolution through `resolve_target_to_rom`.
const PUBLIC_JT_VA_BASE: u32 = 0x0180_0000;
const PUBLIC_JT_VA_END:  u32 = 0x0190_0000;
const PUBLIC_JT_L2_PA:   u32 = 0x0001_8000;
fn public_jt_va_to_phys(words: &[u32], va: u32) -> Option<u32> {
    if va < PUBLIC_JT_VA_BASE || va >= PUBLIC_JT_VA_END { return None; }
    let l2_idx = (va >> 12) & 0xFF;
    let entry_pa = PUBLIC_JT_L2_PA + l2_idx * 4;
    if (entry_pa as usize) + 4 > ROM_SIZE_BYTES { return None; }
    let entry = words[(entry_pa >> 2) as usize];
    if (entry & 0b10) != 0b10 { return None; }
    let page_pa = entry & 0xFFFF_F000;
    let pa = page_pa | (va & 0xFFF);
    if (pa as usize) + 4 > ROM_SIZE_BYTES { return None; }
    Some(pa)
}


/// Resolve a branch / function-pointer target to its in-ROM PA.
/// Returns the thunk PA for patch-table / public-jt / secondary-jt VAs
/// (so the walker visits the thunk word, marks it for byteswap, and
/// follows through via `Step::Jump`'s PA-decoded fall-through into
/// the eventual ROM target). Direct ROM PAs pass through unchanged.
/// Returns `None` for kernel-VA targets we don't recognise.
fn resolve_target_to_rom(words: &[u32], target: u32) -> Option<u32> {
    if target & 3 != 0 { return None; }
    if (target as usize) < ROM_SIZE_BYTES { return Some(target); }
    if let Some(thunk_pa) = jt_va_to_phys(target) {
        // Validate the slot decodes as a B/BL — patch-table slots can
        // be patched at boot to non-B forms.
        let w = words[(thunk_pa >> 2) as usize];
        let cond = (w >> 28) & 0xF;
        let kind = (w >> 25) & 0b111;
        if cond == 0xF || kind != 0b101 { return None; }
        return Some(thunk_pa);
    }
    if let Some(pa) = public_jt_va_to_phys(words, target) {
        return Some(pa);
    }
    secondary_jt_va_to_phys(words, target)
}

/// Result of chasing a (possibly aliased) target through the JT chain.
/// `thunks` lists every thunk PA visited during resolution — these
/// must be marked code at load time so BE-8 byteswap covers the bytes
/// the kernel branches through, but they should NOT be added to the
/// walker's worklist (PA decoding from a thunk would chase a wrong
/// target).
struct ChainResolution {
    final_pa: u32,
    thunks: Vec<u32>,
}

fn resolve_jt_chain(words: &[u32], target: u32) -> Option<ChainResolution> {
    let mut thunks: Vec<u32> = Vec::new();
    let mut cur = target;
    // Defensive cap: a chain longer than this is almost certainly a
    // cycle from misdecoded data.
    for _ in 0..8 {
        if cur & 3 != 0 { return None; }
        if (cur as usize) < ROM_SIZE_BYTES {
            return Some(ChainResolution { final_pa: cur, thunks });
        }
        // VA-keyed L2 lookup: returns the thunk PA for whichever JT
        // window `cur` belongs to.
        let thunk_pa = jt_va_to_phys(cur)
            .or_else(|| public_jt_va_to_phys(words, cur))
            .or_else(|| secondary_jt_va_to_phys(words, cur))?;
        if (thunk_pa as usize) + 4 > ROM_SIZE_BYTES { return None; }
        thunks.push(thunk_pa);
        // Decode the thunk's B against the runtime VA (`cur`), not the
        // thunk PA — the kernel branches via VA, so VA arithmetic gives
        // the real next-hop target.
        let w = words[(thunk_pa >> 2) as usize];
        let cond = (w >> 28) & 0xF;
        let kind = (w >> 25) & 0b111;
        if cond == 0xF || kind != 0b101 { return None; }
        let imm24 = w & 0x00FF_FFFF;
        let simm = sign_extend(imm24, 24) << 2;
        cur = cur.wrapping_add(8).wrapping_add(simm as u32);
    }
    None
}

/// Enumerate the one-instruction-per-entry table that immediately
/// follows a PC-relative dispatch (`<dpop> PC, PC, Rn[, shift]`).
/// Pushes each entry's PA as a worklist root, plus B/BL targets.
/// Bounded by:
///   - if `prev_w` is `CMP Rn, #imm`, exactly `imm + 1` entries (the
///     `addls pc, pc, Rn, lsl #2` idiom dispatches indices 0..=imm),
///   - otherwise the first entry that doesn't decode as a valid
///     one-instruction handler (B / BL / Bcond / MOV PC / LDR PC /
///     LDM-with-PC / BX),
///   - `MAX_ENTRIES = 256` defensive cap.
///
/// The dispatch's PC reads as `pc_of_dispatch + 8` per ARM convention,
/// so table entry 0 lives exactly there — no padding to skip. Each
/// entry is a single-instruction terminal control-flow op; mixed
/// `B handler / mov pc, lr / b default_handler` is normal in Newton
/// (e.g. CallAirusANoLock at 0x2d590).
fn enumerate_pc_rel_jump_table(
    words: &[u32],
    pc_of_dispatch: u32,
    prev_w: u32,
    fn_ranges: &[(u32, u32)],
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    parent_trace: &[WalkReason],
) -> usize {
    const MAX_ENTRIES: usize = 256;
    // Without a CMP bound, cap iteration at MAX_UNBOUNDED slots so
    // runaway walks can't pull adjacent data into the reach set.
    // 64 × 4 = 256 bytes covers the multi-insn switches observed in
    // the Newton ROM (e.g. 16 × 16-byte case bodies for the cond-
    // code emulator at 0x3add80).
    const MAX_UNBOUNDED: usize = 64;
    // If the preceding insn is `CMP Rn, #imm` (cond=AL, opcode CMP=0xA,
    // S=1, imm form), the dispatch handles indices 0..=imm so the table
    // has imm+1 entries. CMP encoding: 0xE35Riiii where R is Rn.
    let bounded_size: Option<usize> = {
        let cond = (prev_w >> 28) & 0xF;
        let opcode = (prev_w >> 21) & 0xF;
        let s_bit = (prev_w >> 20) & 1;
        let bit25 = (prev_w >> 25) & 1;
        if cond == 0xE && opcode == 0xA && s_bit == 1 && bit25 == 1 {
            // Decode imm12 modified-immediate.
            let rot = ((prev_w >> 8) & 0xF) * 2;
            let val8 = prev_w & 0xFF;
            let imm = val8.rotate_right(rot);
            Some((imm as usize) + 1)
        } else {
            None
        }
    };
    let limit = bounded_size
        .unwrap_or(MAX_UNBOUNDED)
        .min(MAX_ENTRIES);
    // Iter-72 fix: clamp seeding to the containing function's range.
    // For DynArrayLeaf (0x3ad4e4..0x3ad524), the dispatch at 0x3ad4e4
    // is followed by 14 case-body insns ending in `mov pc, lr`; data
    // starts at 0x3ad568. Without a CMP bound and without a function-
    // boundary clamp, the unbounded 64-slot seeding walked into the
    // SWIBoot handler-pointer table at 0x3ad568..0x3ad5f4, classifying
    // those data words as code and producing UDF-marker corruption at
    // 0x003ad580/0x003ad584/0x003ad58c (slots 0x486/0x487/0x488 in the
    // SBA site table). The cond-code emulator at 0x3add80 still works
    // because its containing function (SWIBoot, 0x3ad698..0x3ae158) is
    // large enough to fully contain its 64-slot table.
    let fn_end = find_fn_range(fn_ranges, pc_of_dispatch).map(|(_, e)| e);
    let mut tbl = pc_of_dispatch.wrapping_add(8);
    let mut count = 0usize;
    for _ in 0..limit {
        if (tbl as usize) + 4 > ROM_SIZE_BYTES { break; }
        if tbl & 3 != 0 { break; }
        if let Some(end) = fn_end {
            if tbl >= end { break; }
        }
        let w = words[(tbl >> 2) as usize];
        // Each 4-byte slot is one of:
        //   1. Branch (B/BL/Bcc): single-insn handler; seed target.
        //   2. Other PC-write (MOV PC, LR / LDR PC / etc.):
        //      single-insn early-return / register jump.
        //   3. Non-terminal: a switch with fallthrough — the slot
        //      is the first instruction of a multi-insn handler
        //      that falls through into its own continuation. Seed
        //      `tbl` as a worklist root and let the walker walk
        //      the body until its natural epilogue.
        let is_branch = ((w >> 25) & 0b111) == 0b101 && ((w >> 28) & 0xF) != 0xF;
        if is_branch {
            let imm24 = w & 0xFFFFFF;
            let simm = sign_extend(imm24, 24) << 2;
            let target = tbl.wrapping_add(8).wrapping_add(simm as u32);
            let mut child = parent_trace.to_vec();
            child.push(WalkReason::Seed(SeedSource::JtTableTarget { dispatch_pc: pc_of_dispatch }));
            worklist.push((target, child));
        }
        let mut child = parent_trace.to_vec();
        child.push(WalkReason::Seed(SeedSource::JtTableEntry { dispatch_pc: pc_of_dispatch }));
        worklist.push((tbl, child));
        count += 1;
        tbl = tbl.wrapping_add(4);
    }
    count
}

/// Returns true if `w` is a PC-relative jump-table dispatch:
/// `<dp_op> PC, PC, Rn[, shift]` (Rd = Rn = 15, register operand).
/// Newton's compiler emits these (e.g. `ADD PC, PC, R9, LSR #25` or
/// `EORLS PC, PC, R0, LSL #2`) for n-way switch dispatches; the n
/// table entries are unconditional `B`s starting at `pc + 8`.
fn is_pc_rel_pc_dispatch(w: u32) -> bool {
    // DP family (bits[27:26]=00).
    if (w >> 26) & 0b11 != 0b00 { return false; }
    // Register operand (bit 25 = 0).
    if (w >> 25) & 1 != 0 { return false; }
    let rd = (w >> 12) & 0xF;
    let rn = (w >> 16) & 0xF;
    if rd != 15 || rn != 15 { return false; }
    let opcode = (w >> 21) & 0xF;
    let s_bit = (w >> 20) & 1;
    // Reject TST/TEQ/CMP/CMN (opcodes 8..=B with S=0) — those are
    // compare-only forms that don't write Rd.
    if matches!(opcode, 0x8..=0xB) && s_bit == 0 { return false; }
    true
}

/// Returns true if `w` (when executed unconditionally) writes to PC. Used
/// to detect jump-table dispatch: after such an instruction, a run of
/// unconditional `B` entries is a jump table, not terminal jumps.
fn is_pc_write(w: u32) -> bool {
    // Conditional DP with Rd=15, e.g. `EORLS pc, pc, r0, LSL #2`.
    // Match any DP variant (bits[27:26]=00) writing Rd=15, regardless of cond.
    if (w >> 26) & 0b11 == 0b00 {
        let rd = (w >> 12) & 0xF;
        let opcode = (w >> 21) & 0xF;
        let s_bit = (w >> 20) & 1;
        let is_dp_standard = !matches!(opcode, 0x8..=0xB) || s_bit == 1;
        if is_dp_standard && rd == 15 { return true; }
    }
    // LDR with Rd=15 (any cond).
    if (w >> 26) & 0b11 == 0b01 {
        let l_bit = (w >> 20) & 1;
        let rd = (w >> 12) & 0xF;
        if l_bit == 1 && rd == 15 { return true; }
    }
    // BX Rn (any cond, A1 encoding).
    if (w & 0x0FFF_FFF0) == 0x012F_FF10 { return true; }
    // LDM with PC in reglist.
    if (w >> 25) & 0b111 == 0b100 {
        let l_bit = (w >> 20) & 1;
        let has_pc = (w >> 15) & 1 == 1;
        if l_bit == 1 && has_pc { return true; }
    }
    false
}

/// Decode one ARM word for the walker's control-flow purposes. Data-flow
/// classification (byte/halfword access) is a separate check via
/// `is_byte_access`. `manual_bl` says the walker has tracked a recent
/// `mov lr, pc` / `add lr, pc, #imm` whose target equals `pc + 4` —
/// any PC-write at `pc` is therefore a call returning at `pc + 4`,
/// not a terminal jump. `in_table` says the walker is currently
/// traversing a jump-table fall-through — the caller sets this after a
/// PC-writing instruction; while true, unconditional `B` is interpreted
/// as a table entry (push target, keep walking) rather than a terminal
/// jump.
fn step(w: u32, _pc: u32, manual_bl: bool, in_table: bool) -> Step {
    let cond = (w >> 28) & 0xF;
    let prev_sets_lr = manual_bl;
    let pc = _pc;

    // B / BL / Bcc.
    if (w >> 25) & 0b111 == 0b101 {
        let imm24 = w & 0x00FF_FFFF;
        let simm = sign_extend(imm24, 24) << 2;
        let target = pc.wrapping_add(8).wrapping_add(simm as u32);
        if cond == 0xE {
            let is_link = (w >> 24) & 1 != 0;
            if in_table && !is_link {
                return Step::Continue { branch: Some(target) };
            }
            return if is_link {
                Step::Continue { branch: Some(target) }
            } else {
                Step::Jump(target)
            };
        } else {
            return Step::Continue { branch: Some(target) };
        }
    }

    // LDR pc, [...] — terminal jump unless the previous insn set LR for return.
    if (w >> 26) & 0b11 == 0b01 {
        let l_bit = (w >> 20) & 1;
        let rd = (w >> 12) & 0xF;
        if cond == 0xE && l_bit == 1 && rd == 15 {
            return if prev_sets_lr {
                Step::Continue { branch: None }
            } else {
                Step::Stop
            };
        }
    }

    // BX Rn.
    if cond == 0xE && (w & 0x0FFF_FFF0) == 0x012F_FF10 {
        return if prev_sets_lr {
            Step::Continue { branch: None }
        } else {
            Step::Stop
        };
    }

    // MOV / any DP writing pc.
    if cond == 0xE && (w >> 26) & 0b11 == 0b00 {
        let rd = (w >> 12) & 0xF;
        let opcode = (w >> 21) & 0xF;
        let s_bit = (w >> 20) & 1;
        let is_dp_standard = match opcode {
            0x8..=0xB => s_bit == 1,
            _ => true,
        };
        if is_dp_standard && rd == 15 {
            return if prev_sets_lr {
                Step::Continue { branch: None }
            } else {
                Step::Stop
            };
        }
    }

    // LDM with PC in reglist. Prev-sets-LR cannot rescue this — LDM-with-pc
    // is a return from a stack frame, not a call. Exception: while
    // traversing a jump-table, this is a default-case return that sits
    // between the dispatch and the real table entries; keep walking so
    // the B-AL entries that follow get enqueued.
    if (w >> 25) & 0b111 == 0b100 {
        let l_bit = (w >> 20) & 1;
        let has_pc = (w >> 15) & 1 == 1;
        if cond == 0xE && l_bit == 1 && has_pc {
            return if in_table {
                Step::Continue { branch: None }
            } else {
                Step::Stop
            };
        }
    }

    // SWI/SVC: kernel call that returns to PC+4. Walker must fall
    // through — Newton's SWI-wrapper functions (e.g. SMemMsg…SWI at
    // 0x3ae458) do bookkeeping after the SWI before their own `mov
    // pc, lr` epilogue. Stopping at the SWI strands everything past
    // it, including the actual return.
    if cond == 0xE && (w >> 24) & 0xF == 0xF {
        return Step::Continue { branch: None };
    }

    // UDF (A1 encoding family).
    if cond == 0xE && (w & 0x0FF0_00F0) == 0x0700_00F0 {
        return Step::Stop;
    }

    // Newton "panic with inline message" pseudo-op. The trap doesn't
    // return; the bytes after it are an ASCII null-terminated message
    // padded to 4-byte alignment, NOT instructions. Without recognising
    // it the walker falls through into the message and marks string
    // words as code (string chars happen to decode as LDRB/STRB shape
    // with Rn=PC, then leak into the byte-access bitmap and force
    // shadow_stub to skip them at install time).
    if w == 0xE600_0510 {
        return Step::Stop;
    }

    Step::Continue { branch: None }
}

#[derive(Default, Debug)]
struct WalkStats {
    initial_roots: usize,
    words_walked: u64,
    nv_cond_skips: u64,
    data_range_stops: u64,
    indirect_passes: u32,
    indirect_roots_added: usize,
    rex_header_roots: usize,
    vtable_roots: usize,
    vtables_found: usize,
    fnptr_literal_roots: usize,
    b_run_roots: usize,
    pc_rel_addr_roots: usize,
    classinfo_roots: usize,
    alt_entry_roots: usize,
    indexed_dispatch_roots: usize,
    /// Words cleared from reach because they are the targets of
    /// `LDR Rt, [pc, #±imm12]` from inside reached code — i.e. literal
    /// pool entries. Under BE-8 these MUST be data: a guest LDR with
    /// `CPSR.E=1` reads bytes in BE-natural order, but if the loader
    /// byteswapped the word at load time (as it does for code), the
    /// LDR returns the wrong numerical value. Some entries are also
    /// "reached" through dual-purpose dead-code branches (e.g. the
    /// `beq 0x1862c` in DiagHook lands in its own literal pool); the
    /// guest never executes that path under our boot, so treating them
    /// as data is safe.
    literal_targets_cleared: u64,
    /// Count of distinct walks that crossed into ROM_SOUP_RANGE. Each
    /// is logged to stderr with its full trace stack at entry time.
    rom_soup_entries: u64,
}

/// Walk from roots, closing over indirect-targets by scanning unreached
/// word-aligned values for function-pointer-shaped data. Returns the
/// reachable-code bitmap. The caller then intersects with
/// `is_byte_access` to produce the final byte-access-static bitmap.
fn walk(
    words: &[u32],
    initial_roots: &[(u32, SeedSource)],
    initial_reach: Bitmap,
    fn_ranges: &[(u32, u32)],
) -> (Bitmap, WalkStats) {
    let mut stats = WalkStats::default();
    let mut reach = initial_reach;
    // Each worklist entry carries the FULL trace of decisions that
    // led to it being pushed: seed → [Jump → Jump → ...] → Branch.
    // Popping it restores that trace state, so any mystery `cur` mark
    // dumps the complete origin chain (seed plus every Jump and Branch
    // along the way). Increments are not on the stack — they're
    // implicit between adjacent stack frames.
    let mut worklist: Vec<(u32, Vec<WalkReason>)> = initial_roots
        .iter()
        .map(|&(a, src)| (a, vec![WalkReason::Seed(src)]))
        .collect();
    stats.initial_roots = worklist.len();

    let mut pass = 0u32;
    loop {
        pass += 1;
        while let Some((pc, popped_trace)) = worklist.pop() {
            let mut cur = pc;
            let mut prev_w: u32 = 0;
            let mut in_table = false;
            // Trace stack restored from the worklist entry: the full
            // origin chain (seed + all Jump/Branch transitions) that
            // got us to this pop. Step::Jump appends a Jump frame as
            // the walker hops across basic blocks; Step::Continue with
            // a branch pushes a NEW worklist entry carrying
            // `current_trace + Branch{from_pc}`.
            let mut trace: Vec<WalkReason> = popped_trace;
            // LR-target tracker for multi-insn manual-BL idioms.
            // When `lr_target == Some(cur + 4)` at a PC-writing
            // instruction, the call returns at cur+4 — even when
            // the LR setup happened several insns earlier. Cleared
            // when a non-PC, non-call instruction overwrites LR.
            let mut lr_target: Option<u32> = None;
            // Log only the first ROM-soup entry per popped walk: once
            // we've crossed in, every subsequent step that stays inside
            // shares the same root cause — logging each word would
            // bury the signal.
            let mut soup_logged = false;
            loop {
                if (cur as usize) >= ROM_SIZE_BYTES || cur & 3 != 0 { break; }
                if reach.get_word(cur) { break; }
                if in_data_stop_range(cur) {
                    stats.data_range_stops += 1;
                    break;
                }
                let w = words[(cur >> 2) as usize];
                if (w >> 28) == 0xF {
                    stats.nv_cond_skips += 1;
                    break;
                }
                if WALK_TRACE_ADDRS.contains(&cur) {
                    eprintln!("WALK_TRACE 0x{cur:08x} (pc popped 0x{pc:08x}, pass {pass}):");
                    for (i, r) in trace.iter().enumerate() {
                        eprintln!("  [{i}] {r:?}");
                    }
                }
                if !soup_logged && in_rom_soup(cur) {
                    soup_logged = true;
                    stats.rom_soup_entries += 1;
                    eprintln!(
                        "ROM-SOUP CLASSIFY FAIL 0x{cur:08x} insn=0x{w:08x} (pop pc 0x{pc:08x}, pass {pass}):",
                    );
                    for (i, r) in trace.iter().enumerate() {
                        eprintln!("  [{i}] {r:?}");
                    }
                }
                reach.set_word(cur);
                stats.words_walked += 1;
                // Refresh LR-target tracker. `mov lr, pc` /
                // `add lr, pc, #imm` set LR to a static address
                // we can compute against the current PC. Other
                // instructions that write LR (Rd=14) invalidate
                // the tracker.
                if let Some(imm) = lr_setup_imm(w) {
                    lr_target = Some(cur.wrapping_add(8).wrapping_add(imm));
                } else {
                    let writes_lr = ((w >> 28) & 0xF) == 0xE
                        && (w >> 12) & 0xF == 14
                        // DP / LDR with Rd = LR.
                        && ((w >> 26) & 0b11 == 0b00 || (w >> 26) & 0b11 == 0b01);
                    if writes_lr {
                        lr_target = None;
                    }
                }
                let manual_bl = lr_target == Some(cur.wrapping_add(4));
                let step_result = step(w, cur, manual_bl, in_table);

                // PC-relative jump-table dispatch (`<dpop> PC, PC, Rn[, shift]`):
                // the n table entries are unconditional `B`s starting at PC+8.
                // step() returns Stop on this insn (since it doesn't know the
                // target), so we have to enumerate the B-AL run here before
                // breaking. Without this, the walker misses every case body
                // reachable only through the dispatch.
                if is_pc_rel_pc_dispatch(w) {
                    enumerate_pc_rel_jump_table(
                        words, cur, prev_w, fn_ranges, &mut worklist, &trace,
                    );
                }

                // Update table state for the next iteration: entering a
                // table when the current insn writes PC (dispatch), staying
                // in a table while we keep seeing unconditional `B` entries,
                // leaving once any other word appears.
                //
                // Exception: a PC-write preceded by `mov lr, pc` (or
                // `add lr, pc, #0`) is Newton's manual-BL idiom — the PC
                // load is a function call, not a dispatch. Don't enter
                // in-table mode after it, otherwise the walker
                // misinterprets the call's epilogue (LDM-with-PC at the
                // function's true end) as a "default-case return"
                // between dispatch and table entries, walks past the
                // epilogue, and falls into the literal pool. That's how
                // a function-pointer literal at `function_end + 0` ends
                // up classified as a byte-access instruction
                // (iter-69: 0x35c49c → 0x01b494f4 corruption).
                let cond = (w >> 28) & 0xF;
                let is_b_al = ((w >> 25) & 0b111) == 0b101
                    && cond == 0xE
                    && ((w >> 24) & 1) == 0;
                in_table = if is_pc_write(w) && !manual_bl {
                    true
                } else if in_table && is_b_al {
                    true
                } else {
                    false
                };
                match step_result {
                    Step::Continue { branch: Some(t) } => {
                        if let Some(root) = resolve_target_to_rom(words, t) {
                            let mut child_trace = trace.clone();
                            child_trace.push(WalkReason::Branch { from_pc: cur });
                            worklist.push((root, child_trace));
                        }
                        prev_w = w;
                        cur = cur.wrapping_add(4);
                    }
                    Step::Continue { branch: None } => {
                        prev_w = w;
                        cur = cur.wrapping_add(4);
                    }
                    Step::Jump(t) => {
                        prev_w = 0;
                        in_table = false;
                        let from_pc = cur;
                        cur = match resolve_target_to_rom(words, t) {
                            Some(pa) => pa,
                            None => break,
                        };
                        trace.push(WalkReason::Jump { from_pc });
                    }
                    Step::Stop => break,
                }
            }
        }

        // Vtable-install-pattern pass: scan the newly-reached code for
        // the signature two-instruction sequence a Newton constructor
        // uses to install its vtable in *this — an LDR of a literal
        // immediately followed by a store of the loaded value to
        // [r0, #0]:
        //   `LDR Rn, [pc, #imm]`     -> 0xE59Fxxxx / 0xE51Fxxxx
        //   `STR Rn, [r0, #0]`       -> 0xE580xxxx (we require U=1,
        //                               P=1, W=0 which gives 0xE58Rxxxx
        //                               with Rn=0)
        //
        // When found, read the literal at (pc + 8 +- imm12) — that's
        // the vtable address. Then walk consecutive words at the
        // vtable address; each word that points at a prologue-shaped
        // instruction is a method entry to seed. Stop at the first
        // non-code-looking word.
        //
        // Bounded by: LDR must load into the SAME register that the
        // following STR stores; store must be to Rn=r0 (this),
        // P=1/U=1/W=0, imm12=0.
        let mut new_roots = 0usize;
        new_roots += collect_vtable_roots(
            words, &reach, &mut worklist, &mut stats,
        );
        new_roots += collect_fnptr_literal_roots(
            words, &reach, &mut worklist, &mut stats,
        );
        new_roots += collect_b_run_roots(
            words, &reach, &mut worklist, &mut stats,
        );
        new_roots += collect_pc_relative_addr_roots(
            words, &reach, fn_ranges, &mut worklist, &mut stats,
        );
        new_roots += collect_classinfo_roots(
            words, &reach, &mut worklist, &mut stats,
        );
        new_roots += collect_alt_entry_roots(
            words, &reach, &mut worklist, &mut stats,
        );
        new_roots += collect_indexed_dispatch_roots(
            words, &reach, fn_ranges, &mut worklist, &mut stats,
        );

        stats.indirect_passes = pass;
        stats.indirect_roots_added += new_roots;
        if new_roots == 0 { break; }
    }

    // Post-pass: subtract literal-pool words from `reach`. Any word
    // that is the target of an `LDR Rt, [pc, #±imm12]` from reached
    // code is a data literal — it stores a constant the kernel reads
    // via the LDR — and must be left in BE-natural byte order at load
    // time so a CPSR.E=1 LDR returns the kernel's intended numerical
    // value. The walker may also flag the same word as code if a
    // conditional branch happens to land in the literal pool (e.g.
    // DiagHook's `beq 0x1862c` falls into its own literal pool); under
    // BE-8 the bytes can't be both code (byteswapped) and data
    // (verbatim), and our boot never executes the dead-code branch,
    // so clear it.
    let cleared = clear_literal_pool_targets_from_reach(words, &mut reach);
    stats.literal_targets_cleared = cleared;

    (reach, stats)
}

/// Clear from `reach` every word that is the target of an
/// `LDR Rt, [pc, #±imm12]` (any cond except NV) issued from a word
/// currently in `reach`. Returns the number of bits cleared.
///
/// This is a static-only signal: a literal-pool word is reached as
/// CODE by the walker (e.g. via a conditional branch) but the kernel
/// also reads it as DATA via the LDR. Under BE-8, code and data
/// loads have different byte-order requirements; since the LDR-data
/// reading is the load-bearing path (constants used by the kernel),
/// we mark the word as data.
///
/// LDR Rt, [pc, #±imm12] encoding (immediate, pre-indexed, no writeback):
///   [31:28] cond (any except 0xF)
///   [27:25] = 010
///   [24]    = 1 (P, pre-indexed)
///   [23]    = U (1 = +imm, 0 = -imm)
///   [22]    = 0 (B, word access)
///   [21]    = 0 (W, no writeback)
///   [20]    = 1 (L, load)
///   [19:16] = 1111 (Rn = PC)
///   [15:12] = Rt (any)
///   [11:0]  = imm12
/// Top 16 bits (mask 0x0FFF, cond removed): 0x59F (U=1) or 0x51F (U=0).
fn clear_literal_pool_targets_from_reach(words: &[u32], reach: &mut Bitmap) -> u64 {
    let mut targets: Vec<u32> = Vec::new();
    for i in 0..ROM_WORD_COUNT {
        let addr = (i as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w = words[i];
        let cond = (w >> 28) & 0xF;
        if cond == 0xF { continue; }
        let masked = (w >> 16) & 0x0FFF;
        let imm_sign: i32 = match masked {
            0x59F => 1,
            0x51F => -1,
            _ => continue,
        };
        let imm12 = (w & 0xFFF) as i32;
        let lit_pc = (addr as i64) + 8 + (imm_sign as i64) * (imm12 as i64);
        if lit_pc < 0 || (lit_pc as usize) + 4 > ROM_SIZE_BYTES { continue; }
        if (lit_pc as u32) & 3 != 0 { continue; }
        targets.push(lit_pc as u32);
    }
    let mut cleared: u64 = 0;
    for t in targets {
        if reach.get_word(t) {
            reach.clear_word(t);
            cleared += 1;
        }
    }
    cleared
}

/// Scan reached code for the LDR-PC-rel + STR-to-this install pair,
/// chase the literal to a vtable, enumerate its method pointers, and
/// add each as a worklist root. Returns the number of method roots
/// pushed this call.
fn collect_vtable_roots(
    words: &[u32],
    reach: &Bitmap,
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    let mut added = 0usize;
    // Track vtables we've already enumerated so we don't walk the
    // same table from every constructor that installs it.
    let mut seen: HashSet<u32> = HashSet::new();

    for addr_idx in 0..ROM_WORD_COUNT.saturating_sub(1) {
        let addr = (addr_idx as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w0 = words[addr_idx];
        // LDR Rt, [pc, #+-imm12]: bits[27:20]=0101_0001 (L=1, U=?),
        // P=1, W=0, B=0, Rn=15. The two valid top halves are
        // 0xE59F (U=1) and 0xE51F (U=0).
        let top0 = w0 >> 16;
        let (u, imm_sign): (u32, i32) = match top0 {
            0xE59F => (1, 1),
            0xE51F => (0, -1),
            _ => continue,
        };
        let _ = u;
        let rt = (w0 >> 12) & 0xF;
        let imm12 = (w0 & 0xFFF) as i32;

        // The STR must be immediately at addr+4 storing Rt into
        // [Rn, #0] for any Rn — Newton's APCS-style constructors
        // typically move `this` into R4 early, then install the
        // vtable via `STR Rt, [R4, #0]`. We only require imm12=0
        // (offset-0 install for the primary vtable) and Rt to match
        // the preceding LDR's destination register.
        let w1 = words[addr_idx + 1];
        // STR Rt,[Rn,#0]: cond=AL(0xE) 010 P=1 U=1 B=0 W=0 L=0 Rn Rt imm12=0.
        // Fixed bits are [31:20] = 0xE58 and [11:0] = 0; Rn (19:16) and
        // Rt (15:12) are variable. Mask = 0xFFF0_0FFF, value = 0xE580_0000.
        if (w1 & 0xFFF0_0FFF) != 0xE580_0000 { continue; }
        let rt_w1 = (w1 >> 12) & 0xF;
        if rt_w1 != rt { continue; }

        // Literal address: pc_of_ldr + 8 + signed_offset.
        let lit_pc = (addr as i64) + 8 + (imm_sign as i64 * imm12 as i64);
        if lit_pc < 0 || (lit_pc as usize) + 4 > ROM_SIZE_BYTES { continue; }
        if (lit_pc as u32) & 3 != 0 { continue; }
        let vtable_addr = words[(lit_pc as usize) >> 2];
        if (vtable_addr as usize) >= ROM_SIZE_BYTES { continue; }
        if vtable_addr & 3 != 0 { continue; }
        if !seen.insert(vtable_addr) { continue; }

        // Enumerate consecutive method pointers at vtable_addr.
        // Stop at the first word that doesn't point at a prologue-
        // shaped target. Also bound the scan so a runaway (pointer-
        // shape noise) can't walk the whole ROM.
        const MAX_VTABLE_ENTRIES: usize = 256;
        let mut entries_added = 0usize;
        for j in 0..MAX_VTABLE_ENTRIES {
            let vptr_addr = vtable_addr.wrapping_add((j as u32) * 4);
            if (vptr_addr as usize) + 4 > ROM_SIZE_BYTES { break; }
            let p = words[(vptr_addr as usize) >> 2];
            if p == 0 { break; }
            // Resolve patch-table VAs to their underlying ROM PA.
            // Vtables in Newton overwhelmingly point at JT thunks
            // (so methods are patchable post-ship); a vtable scan
            // that requires direct ROM-PA targets misses every
            // method.
            let p = match resolve_target_to_rom(words, p) {
                Some(pa) => pa,
                None => break,
            };
            let tgt_idx = (p >> 2) as usize;
            let tgt_word = words[tgt_idx];
            if (tgt_word >> 28) == 0xF { break; }
            if !is_known_function_start(tgt_word) { break; }
            if !reach.get_word(p) {
                worklist.push((p, vec![WalkReason::Seed(SeedSource::Vtable { ldr_pc: addr })]));
                added += 1;
                entries_added += 1;
            }
        }
        if entries_added > 0 {
            stats.vtables_found += 1;
            stats.vtable_roots += entries_added;
        }
    }
    added
}

/// Scan reached code for `ADD Rd, PC, #imm12` / `SUB Rd, PC, #imm12`
/// (cond=AL, register form, Rd != PC, Rn=PC) and seed `pc + 8 ± imm12`
/// as a worklist root. Newton uses this to compute table-base
/// addresses for runtime-dispatched jump tables — e.g.
/// `add sl, pc, #232` followed later by `add pc, sl, r9, lsl #4`. The
/// table base isn't a B target, isn't in a vtable, and isn't preceded
/// by an LDR-pc-rel literal, so none of the existing harvesters catch
/// it. Without this, every case body of every SL-based dispatch is
/// unreachable.
fn collect_pc_relative_addr_roots(
    words: &[u32],
    reach: &Bitmap,
    fn_ranges: &[(u32, u32)],
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    for addr_idx in 0..ROM_WORD_COUNT {
        let addr = (addr_idx as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w = words[addr_idx];
        // ADD/SUB cond=AL imm: 0xE28FRddd (ADD) / 0xE24FRddd (SUB).
        let kind = w >> 16;
        let imm_sign: i32 = match kind {
            0xE28F => 1,
            0xE24F => -1,
            _ => continue,
        };
        let rd = (w >> 12) & 0xF;
        if rd == 15 { continue; }
        let rot = ((w >> 8) & 0xF) * 2;
        let val8 = w & 0xFF;
        let imm = val8.rotate_right(rot);
        let target = (addr.wrapping_add(8) as i64)
            .wrapping_add(imm_sign as i64 * imm as i64) as u32;
        if (target as usize) >= ROM_SIZE_BYTES { continue; }
        if target & 3 != 0 { continue; }
        // Two ways for the PC-rel computed address to be code:
        //
        //   (a) Dispatch-base setup: Rd is later used as the base
        //       register of a runtime PC-write dispatch
        //       (`<dpop>cond pc, Rd, Rn, lsl #imm`) inside the
        //       same function — e.g. BPNetEvaluate's `add sl, pc,
        //       #232` later feeding `add pc, sl, r9, lsl #4`.
        //   (b) Function-pointer construction: the target word
        //       itself is a function prologue. Newton emits this
        //       when handing a small in-line handler off as an
        //       argument — e.g. FPE init at 0x39264c does
        //       `sub r1, pc, #0x2c` to point r1 at a 2-insn stub
        //       (`mvn r0, #0; movs pc, lr`) that's never B-called
        //       and has no 32-bit pointer reference anywhere.
        //
        // ASCII-string targets (REPStackTrace's `add r1, pc, #0xa4`)
        // can't pass (b)'s prologue gate because ASCII top nibbles
        // are 0x2..0x7, not 0xE.
        let tw = words[(target >> 2) as usize];
        if (tw >> 28) == 0xF { continue; }
        let target_is_code = is_known_function_start(tw);
        let fn_range = match find_fn_range(fn_ranges, addr) {
            Some(r) => r,
            None => continue,
        };
        let is_dispatch_base = is_used_as_dispatch_base(words, reach, fn_range, rd, addr);
        if !target_is_code && !is_dispatch_base { continue; }
        if reach.get_word(target) { continue; }
        if !seen.insert(target) { continue; }
        worklist.push((target, vec![WalkReason::Seed(SeedSource::PcRelAddr { adr_pc: addr })]));
        added += 1;
    }
    stats.pc_rel_addr_roots += added;
    added
}

/// Find the (start, end) range of the function containing `addr`,
/// using the sorted `fn_ranges` list. Returns `None` if `addr` is
/// outside any known function.
fn find_fn_range(fn_ranges: &[(u32, u32)], addr: u32) -> Option<(u32, u32)> {
    let idx = fn_ranges.partition_point(|&(s, _)| s <= addr);
    if idx == 0 { return None; }
    let (s, e) = fn_ranges[idx - 1];
    if s <= addr && addr < e { Some((s, e)) } else { None }
}

/// Returns true if any word in `[fn_range.0, fn_range.1)` decodes as a
/// PC-write dispatch (`<dpop>cond pc, Rd_target, ...`) using `rd_target`
/// as the base register. Skips `add Rd, PC, ...` itself by requiring
/// Rn = `rd_target` (the dispatch reads from rd_target — only meaningful
/// if rd_target was set earlier).
///
/// MOV (opcode 0xD) and MVN (opcode 0xF) are excluded: they don't read
/// the Rn field, so its value is encoded as 0 by convention. Treating
/// `mov pc, lr` (every function epilogue) as a dispatch from R0 would
/// match every `add r0, pc, #imm` whose function ends with `mov pc, lr`,
/// pulling string-pointer arguments into the reach set.
fn is_used_as_dispatch_base(
    words: &[u32],
    reach: &Bitmap,
    fn_range: (u32, u32),
    rd_target: u32,
    skip_addr: u32,
) -> bool {
    let (s, e) = fn_range;
    let start_idx = (s >> 2) as usize;
    let end_idx = ((e >> 2) as usize).min(ROM_WORD_COUNT);
    for i in start_idx..end_idx {
        let pa = (i as u32) * 4;
        if pa == skip_addr { continue; }
        // The hypothesis being tested is that rd_target was loaded
        // here for a dispatch later in the function. By the time the
        // indirect pass runs, the dispatch instruction itself must
        // already be in reached code (walker would have followed
        // there from the function entry). Skipping unreached words
        // prevents data inside the synthetic last-fn-range bucket
        // from masquerading as a dispatch.
        if !reach.get_word(pa) { continue; }
        let w = words[i];
        // DP family with Rd=15, Rn=rd_target, register operand,
        // any cond (LS/CC/AL etc).
        if (w >> 26) & 0b11 != 0b00 { continue; }
        if (w >> 25) & 1 != 0 { continue; }
        // bits[27:25]=000 with bit 4 = 1 splits into:
        //   bit 7 = 0 → DP register-specified shift (real DP)
        //   bit 7 = 1 → multiply / extra LD-ST / sync primitive
        // The latter aren't DP at all but happen to share Rd/Rn field
        // positions, so without this filter random data words match
        // the dispatch-base check (~670 false hits in REX alone).
        if (w >> 4) & 1 != 0 && (w >> 7) & 1 != 0 { continue; }
        let rd = (w >> 12) & 0xF;
        let rn = (w >> 16) & 0xF;
        if rd != 15 { continue; }
        if rn != rd_target { continue; }
        let opcode = (w >> 21) & 0xF;
        let s_bit = (w >> 20) & 1;
        if matches!(opcode, 0x8..=0xB) && s_bit == 0 { continue; }
        if matches!(opcode, 0xD | 0xF) { continue; }
        return true;
    }
    false
}

/// Scan the entire ROM+REX aperture for runs of N≥3 adjacent
/// unconditional `B` instructions (top byte 0xEA). Each such run is a
/// dispatch table (REX FDRV class-method stubs, jump-tables that the
/// PC-rel walker missed, compiler-emitted switch tables that aren't
/// preceded by a recognizable PC-write). Seed every entry's branch
/// target as a worklist root.
///
/// Threshold of 3: a single 0xEA-shaped data word happens often
/// (especially in REX strings/integers); 3 in a row is rare enough to
/// be a strong signal. Newton's actual dispatch tables run 3..32+
/// entries.
///
/// Bounded by the first non-B-AL word (run terminates).
fn collect_b_run_roots(
    words: &[u32],
    reach: &Bitmap,
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    const MIN_B: usize = 3;
    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut i = 0usize;
    while i + MIN_B <= ROM_WORD_COUNT {
        // Skip until we find a B-AL.
        if (words[i] >> 24) != 0xEA {
            i += 1;
            continue;
        }
        // Permissive run: words that are either B-AL or zero
        // (placeholder/unused slot). Newton's REX FDRV ClassInfo
        // vtables look like
        //
        //   B method0
        //   0   ; placeholder for cleanup
        //   0   ; placeholder for finalize
        //   B method1
        //   B method2
        //   0   ; ...
        //
        // — interleaved B-AL and zero slots. A run that requires
        // strictly-consecutive B-AL would miss every isolated method
        // entry. Group B-AL runs with internal zero gaps as one
        // table; demand ≥MIN_B real B-AL entries (zeros don't count)
        // for the run to qualify.
        let mut j = i;
        while j < ROM_WORD_COUNT {
            let w = words[j];
            if (w >> 24) == 0xEA || w == 0 {
                j += 1;
            } else {
                break;
            }
        }
        // Trim trailing zeros so the run ends on a B-AL.
        while j > i && words[j - 1] == 0 { j -= 1; }
        let b_count = (i..j).filter(|&k| (words[k] >> 24) == 0xEA).count();
        if b_count < MIN_B { i = j.max(i + 1); continue; }
        // Validate: real dispatch tables target prologue-shaped code.
        // Reject runs whose targets land on non-code words (e.g.
        // gROMPublicJumpTable, whose entries point at unresolved
        // patch slots that hold zero-padded pointer values until
        // boot-time fixup). Require ≥75% of B-AL entries to have
        // function-start-shaped targets (after JT-resolution).
        // Zero placeholder slots don't participate in the count.
        let mut good = 0usize;
        for k in i..j {
            if (words[k] >> 24) != 0xEA { continue; }
            let entry_pa = (k as u32) * 4;
            let imm24 = words[k] & 0xFFFFFF;
            let simm = sign_extend(imm24, 24) << 2;
            let target = entry_pa.wrapping_add(8).wrapping_add(simm as u32);
            let final_tgt = match resolve_target_to_rom(words, target) {
                Some(t) => t,
                None => continue,
            };
            let tgt_word = words[(final_tgt >> 2) as usize];
            if is_known_function_start(tgt_word) { good += 1; }
        }
        if good * 4 < b_count * 3 { i = j.max(i + 1); continue; }
        for k in i..j {
            // Skip zero placeholder slots — they're data; only the
            // B-AL entries are method bodies the walker should walk.
            if (words[k] >> 24) != 0xEA { continue; }
            let entry_pa = (k as u32) * 4;
            // Seed the entry PA as a worklist root. Walker marks it
            // reach=true, then Step::Jump processes the target.
            if !reach.get_word(entry_pa) && seen.insert(entry_pa) {
                worklist.push((entry_pa, vec![WalkReason::Seed(SeedSource::BRunEntry { entry_pa })]));
                added += 1;
            }
        }
        i = j.max(i + 1);
    }
    stats.b_run_roots += added;
    added
}

/// Recognize TClassInfo trampoline functions and seed every code
/// pointer reachable from the inline 60-byte struct that precedes
/// them.
///
/// TClassInfo layout (per OS600/Protocols.h, 15 longs = 60 bytes):
///
///   +0x00 fReserved1            (zero)
///   +0x04 fNameDelta            SRO → asciz implementation name
///   +0x08 fInterfaceDelta       SRO → asciz protocol interface name
///   +0x0C fSignatureDelta       SRO → asciz signature
///   +0x10 fBTableDelta          SRO → dispatch table (B-table)  *CODE*
///   +0x14 fEntryProcDelta       SRO → monitor entry proc        *CODE*
///   +0x18 fSizeofBranch         B target / MOV PC,LR / 0        *CODE*
///   +0x1C fAllocBranch          B target / MOV PC,LR / 0        *CODE*
///   +0x20 fFreeBranch           B target / MOV PC,LR / 0        *CODE*
///   +0x24 fDefaultNewBranch     B target / MOV PC,LR / 0        *CODE*
///   +0x28 fDefaultDeleteBranch  B target / MOV PC,LR / 0        *CODE*
///   +0x2C fVersion              (data)
///   +0x30 fFlags                (data)
///   +0x34 fSelectorBranch       B target / MOV PC,LR            *CODE*
///   +0x38 fReserved2            (zero)
///
/// Trailing 4-instruction trampoline (returns struct_base or nil):
///
///   sub  r0, pc, #68     ; r0 = struct_base = pc + 8 - 68
///   mov  pc, lr          ; trampoline returns the struct's base PA
///   mov  r0, #imm        ; alt entry: bail-out function returning <imm>
///   mov  pc, lr          ;            (typically nil, i.e. imm == 0)
///
/// The pattern is highly specific (4 exact instructions in sequence),
/// so this scans the full ROM+REX rather than gating on already-
/// reached code; a TClassInfo whose trampoline isn't a known symbol
/// (3 such cases observed) is still discovered.
fn collect_classinfo_roots(
    words: &[u32],
    reach: &Bitmap,
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    const TRAMP_SUB_R0_PC_68: u32 = 0xE24F_0044;
    const MOV_PC_LR: u32 = 0xE1A0_F00E;
    const STRUCT_BYTES: u32 = 60;
    // Branch slots: B target, MOV PC,LR (default empty), or 0 (no method).
    // The Protocols.h layout puts fSelectorBranch at +0x34 and fReserved2
    // at +0x38, but the 717006 ROM consistently emits the bail-out branch
    // at +0x38 (B → trampoline alt-entry) and leaves +0x34 zero. Scan both
    // positions; the zero slot is harmless to skip.
    const BRANCH_OFFSETS: [u32; 7] = [0x18, 0x1C, 0x20, 0x24, 0x28, 0x34, 0x38];
    // SRO fields whose target is code:
    //   fBTableDelta  → first word of the dispatch table (back-pointer
    //                   to the trampoline, encoded as B-AL).
    //   fEntryProcDelta → monitor entry proc (function start).
    const FBTABLE_OFFSET: u32 = 0x10;
    const FENTRYPROC_OFFSET: u32 = 0x14;

    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    let last = ROM_WORD_COUNT.saturating_sub(4);
    let struct_words = (STRUCT_BYTES / 4) as usize;

    for fn_idx in struct_words..last {
        if words[fn_idx] != TRAMP_SUB_R0_PC_68 { continue; }
        if words[fn_idx + 1] != MOV_PC_LR { continue; }
        // Alt entry: `mov r0, #imm` (any 12-bit rotated imm).
        let w_alt = words[fn_idx + 2];
        if (w_alt & 0xFFFF_F000) != 0xE3A0_0000 { continue; }
        if words[fn_idx + 3] != MOV_PC_LR { continue; }

        let fn_pa = (fn_idx as u32) * 4;
        let struct_base_pa = fn_pa - STRUCT_BYTES;

        // Seed the trampoline function itself — covers TClassInfo
        // entries that aren't named in symbols.txt.
        if !reach.get_word(fn_pa) && seen.insert(fn_pa) {
            worklist.push((fn_pa, vec![WalkReason::Seed(SeedSource::ClassInfoTramp { tramp_pc: fn_pa })]));
            added += 1;
        }

        // Branch slots: accept B-AL or `MOV PC,LR` (default empty).
        // Zero slots have no method registered and need no marking.
        for &off in &BRANCH_OFFSETS {
            let pa = struct_base_pa + off;
            let w = words[(pa >> 2) as usize];
            let is_b_al = (w >> 24) == 0xEA;
            let is_mov_pc_lr = w == MOV_PC_LR;
            if !is_b_al && !is_mov_pc_lr { continue; }
            if !reach.get_word(pa) && seen.insert(pa) {
                worklist.push((pa, vec![WalkReason::Seed(SeedSource::ClassInfoBranchSlot { tramp_pc: fn_pa })]));
                added += 1;
            }
        }

        // Resolve fBTableDelta and fEntryProcDelta to absolute PAs.
        // Each delta is signed 32-bit, self-relative to its field.
        let resolve = |off: u32| -> Option<u32> {
            let field_pa = struct_base_pa + off;
            let delta = words[(field_pa >> 2) as usize] as i32;
            if delta == 0 { return None; }
            let target = (field_pa as i64).wrapping_add(delta as i64);
            if !(0..=ROM_SIZE_BYTES as i64 - 4).contains(&target) { return None; }
            let target = target as u32;
            if target & 3 != 0 { return None; }
            Some(target)
        };
        let btable_start = resolve(FBTABLE_OFFSET);
        let entry_proc = resolve(FENTRYPROC_OFFSET);

        // B-table: iterate [btable_start, entry_proc) and seed every
        // B-AL entry. The B-table layout is `[opt zero header],
        // B back-pointer-to-classinfo, B method0, B method1, ...` —
        // bounding by the entry-proc target catches every method
        // regardless of count, so small classes (e.g. TGrayShrink with
        // a single method, below `collect_b_run_roots`'s 3-entry
        // threshold) don't lose their back-pointer + method pair.
        if let (Some(start), Some(end)) = (btable_start, entry_proc) {
            if end > start {
                let mut pa = start;
                while pa < end {
                    let w = words[(pa >> 2) as usize];
                    if (w >> 24) == 0xEA && (w >> 28) != 0xF
                        && !reach.get_word(pa)
                        && seen.insert(pa)
                    {
                        worklist.push((pa, vec![WalkReason::Seed(SeedSource::ClassInfoBTable { tramp_pc: fn_pa })]));
                        added += 1;
                    }
                    pa = pa.wrapping_add(4);
                }
            }
        }

        // Entry proc: arbitrary function prologue. Seed once gated.
        if let Some(target) = entry_proc {
            let tw = words[(target >> 2) as usize];
            if (tw >> 28) != 0xF
                && is_known_function_start(tw)
                && !reach.get_word(target)
                && seen.insert(target)
            {
                worklist.push((target, vec![WalkReason::Seed(SeedSource::ClassInfoEntryProc { tramp_pc: fn_pa })]));
                added += 1;
            }
        }
    }

    stats.classinfo_roots += added;
    added
}

/// Discover micro-trampoline alt entries that follow Newton's
/// class-info `add pc, ip, #N` dispatch: the two-instruction sequence
///
///   mov r0, pc   (0xE1A0_000F)
///   mov pc, lr   (0xE1A0_F00E)
///
/// At runtime the caller BLs straight into `mov r0, pc`; r0 then
/// holds `cur + 8` = the address of the immediately-following ASCII
/// class-name string. Whether the alt entry is reachable by static
/// analysis depends on whether the surrounding class is referenced
/// at all in the boot path — the canonical pattern at 0x386208 IS
/// reached via `bl 0x386208`, but the analogous pair at 0x3861e4
/// (after the `add pc, ip, #4` epilogue of TClassInfoRegistryImpl::
/// ClassInfo's dispatch helper) has no static caller and was being
/// classified data.
///
/// Detection gate (avoid false positives): the preceding word must
/// be a PC-writing terminal ALU op via `ip` (`add/sub pc, ip, #imm`
/// in any form: bits[27:25]=001, bits[19:16]=1100 (Rn=ip), Rd=15) —
/// this is the trampoline-jump pattern that the walker can't follow
/// (since `ip` is set externally), and also the pattern that
/// invariably precedes these alt entries in the 717006 ROM. Without
/// this gate, the bare `e1a0_000f e1a0_f00e` pair appears in random
/// data too often to seed safely.
fn collect_alt_entry_roots(
    words: &[u32],
    reach: &Bitmap,
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    const MOV_R0_PC: u32 = 0xE1A0_000F;
    const MOV_PC_LR: u32 = 0xE1A0_F00E;
    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    let last = ROM_WORD_COUNT.saturating_sub(2);

    // Index 0 is the prev-word slot; start at 1 so prev exists.
    for i in 1..last {
        if words[i] != MOV_R0_PC { continue; }
        if words[i + 1] != MOV_PC_LR { continue; }
        let prev = words[i - 1];
        // `add/sub pc, Rn=ip, #imm12` (DP-immediate writing PC):
        //   bits[31:28] = 1110 (cond=AL)
        //   bits[27:25] = 001  (DP-immediate)
        //   bits[24:21] = opcode (any: ADD/SUB/RSB/...; we accept
        //                 ADD=4 and SUB=2, which cover the observed
        //                 trampoline forms)
        //   bits[19:16] = 1100 (Rn = ip)
        //   bits[15:12] = 1111 (Rd = pc)
        let cond = (prev >> 28) & 0xF;
        let kind = (prev >> 25) & 0b111;
        let rn = (prev >> 16) & 0xF;
        let rd = (prev >> 12) & 0xF;
        let opcode = (prev >> 21) & 0xF;
        let prev_is_ip_jump =
            cond == 0xE && kind == 0b001 && rn == 0xC && rd == 0xF
                && (opcode == 0x4 || opcode == 0x2);
        if !prev_is_ip_jump { continue; }

        let alt_pa = (i as u32) * 4;
        if reach.get_word(alt_pa) { continue; }
        if !seen.insert(alt_pa) { continue; }
        worklist.push((alt_pa, vec![WalkReason::Seed(SeedSource::AltEntryAfterIpJump { trampoline_pc: alt_pa })]));
        added += 1;
    }

    stats.alt_entry_roots += added;
    added
}

/// Recognize the bounded indexed-dispatch idiom Newton uses for
/// kernel SWI handlers (and similar opcode tables):
///
///   cmp  Rm, #N                ; bounds check
///   b<cc> out_of_range         ; branch past the dispatch on overflow
///   ldr  Rd, [pc, #±imm12]     ; Rd = literal-pool word = table base PA
///   ldr  pc, [Rd, Rm, lsl #2]  ; pc = table[Rm]
///
/// Example: SWIBoot at 0x3ad698 dispatches `cmp r1, #35; bge out;
/// ldr r0, [pc, #-488]; ldr pc, [r0, r1, lsl #2]` — the table at
/// 0x3ad56c..0x3ad5f4 holds 35 SWI handler PAs. The handlers are
/// reachable only through this idiom (not via B-AL runs, vtables,
/// or LDR-pc-rel literal pools), so the walker has to follow it
/// explicitly.
///
/// Walker action:
///   1. At each LDR pc, [Rn, Rm, lsl #2] in reached code, look
///      backwards (within the containing function) for an
///      LDR Rn, [pc, #±imm] that loads the table base, and a
///      `cmp Rm, #imm` plus its conditional branch that bounds
///      the index range.
///   2. Decode the table base from the literal pool, map the
///      conditional-branch type to an entry count (BGE/BHS/BCS:
///      count = imm; BGT/BHI: count = imm + 1).
///   3. For each in-range entry, validate the target is a
///      prologue-shaped function and seed it.
fn collect_indexed_dispatch_roots(
    words: &[u32],
    reach: &Bitmap,
    fn_ranges: &[(u32, u32)],
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    for addr_idx in 0..ROM_WORD_COUNT {
        let addr = (addr_idx as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w = words[addr_idx];
        // LDR pc, [Rn, Rm, lsl #2], cond=AL, P=1 U=1 B=0 W=0 L=1.
        // Encoding: cond | 011 | P U B W L | Rn | Rt | shift_imm | type | 0 | Rm.
        // Required bit fields (top byte): 0xE7 (cond=AL, 011 P=1).
        if (w >> 28) != 0xE { continue; }
        if (w >> 25) & 0b111 != 0b011 { continue; }
        if (w >> 24) & 1 != 1 { continue; }    // P=1 (pre-indexed)
        if (w >> 23) & 1 != 1 { continue; }    // U=1 (positive offset)
        if (w >> 22) & 1 != 0 { continue; }    // B=0 (word access)
        if (w >> 21) & 1 != 0 { continue; }    // W=0
        if (w >> 20) & 1 != 1 { continue; }    // L=1 (load)
        if (w >> 12) & 0xF != 0xF { continue; } // Rt = pc
        if (w >> 4) & 1 != 0 { continue; }     // immediate-shift form
        if (w >> 5) & 0b11 != 0b00 { continue; } // LSL
        if (w >> 7) & 0b11111 != 2 { continue; } // shift_imm = 2 (4-byte stride)
        let rn = (w >> 16) & 0xF;
        let rm = w & 0xF;
        if rn == 15 || rm == 15 { continue; }

        let fn_range = match find_fn_range(fn_ranges, addr) {
            Some(r) => r,
            None => continue,
        };
        let start_idx = (fn_range.0 >> 2) as usize;

        // Walk back up to 16 instructions, finding:
        //   - LDR Rn, [pc, #±imm12] that loaded the table base.
        //   - CMP Rm, #imm that bounded the index.
        //   - The conditional branch immediately following the CMP
        //     (its cond field tells us count = imm or imm+1).
        let mut table_base: Option<u32> = None;
        let mut cmp_imm: Option<u32> = None;
        let mut bound_cond: Option<u32> = None;
        for back in 1..=16u32 {
            let pa = match addr.checked_sub(back * 4) { Some(p) => p, None => break };
            if ((pa >> 2) as usize) < start_idx { break; }
            let pw = words[(pa >> 2) as usize];

            if table_base.is_none() {
                let top = pw >> 16;
                let imm_sign: i32 = match top {
                    0xE59F => 1,
                    0xE51F => -1,
                    _ => 0,
                };
                if imm_sign != 0 && (pw >> 12) & 0xF == rn {
                    let imm12 = (pw & 0xFFF) as i32;
                    let lit_pc = (pa as i64) + 8 + (imm_sign as i64 * imm12 as i64);
                    if lit_pc >= 0
                        && (lit_pc as usize) + 4 <= ROM_SIZE_BYTES
                        && (lit_pc as u32) & 3 == 0
                    {
                        let v = words[(lit_pc as usize) >> 2];
                        if (v as usize) < ROM_SIZE_BYTES && v & 3 == 0 {
                            table_base = Some(v);
                        }
                    }
                }
            }

            if cmp_imm.is_none() {
                // CMP Rm, #imm: cond=AL, opcode=0xA, S=1, I=1, Rn=rm.
                let cond = (pw >> 28) & 0xF;
                let opcode = (pw >> 21) & 0xF;
                let s_bit = (pw >> 20) & 1;
                let bit25 = (pw >> 25) & 1;
                let rn_cmp = (pw >> 16) & 0xF;
                if cond == 0xE && opcode == 0xA && s_bit == 1 && bit25 == 1 && rn_cmp == rm {
                    let rot = ((pw >> 8) & 0xF) * 2;
                    let val8 = pw & 0xFF;
                    cmp_imm = Some(val8.rotate_right(rot));
                    // The conditional branch immediately after the
                    // CMP carries the polarity. Search forward 1..4
                    // insns from the CMP for a B<cond> with the
                    // expected cc.
                    for fwd in 1..=4u32 {
                        let bpa = pa.wrapping_add(fwd * 4);
                        if bpa >= addr { break; }
                        let bw = words[(bpa >> 2) as usize];
                        if (bw >> 25) & 0b111 == 0b101 && (bw >> 24) & 1 == 0 {
                            // Unconditional B (cond=AL) is the
                            // out-of-range branch only if cc came
                            // earlier; skip it here.
                            let bcond = (bw >> 28) & 0xF;
                            if bcond != 0xE && bcond != 0xF {
                                bound_cond = Some(bcond);
                                break;
                            }
                        }
                    }
                }
            }

            if table_base.is_some() && cmp_imm.is_some() { break; }
        }

        let (tbl, imm, cc) = match (table_base, cmp_imm, bound_cond) {
            (Some(t), Some(i), Some(c)) => (t, i, c),
            _ => continue,
        };

        // Map condition to entry count. b<cc> branches OUT of the
        // dispatch; the cc tells us when "out" applies.
        //   GE (0xA), HS=CS (0x2): out when Rm ≥ N → count = N
        //   GT (0xC), HI    (0x8): out when Rm > N → count = N + 1
        // Other ccs (LT/LS/LO/MI/EQ/NE/...) indicate a non-bound
        // pattern; skip.
        let count = match cc {
            0xA | 0x2 => imm as usize,
            0xC | 0x8 => imm as usize + 1,
            _ => continue,
        };
        const MAX_TABLE: usize = 1024;
        let count = count.min(MAX_TABLE);

        for i in 0..count {
            let entry_pa = tbl.wrapping_add((i as u32) * 4);
            if (entry_pa as usize) + 4 > ROM_SIZE_BYTES { break; }
            let entry_val = words[(entry_pa >> 2) as usize];
            let final_tgt = match resolve_target_to_rom(words, entry_val) {
                Some(t) => t,
                None => continue,
            };
            let tgt_idx = (final_tgt >> 2) as usize;
            let tgt_word = words[tgt_idx];
            if (tgt_word >> 28) == 0xF { continue; }
            if !is_known_function_start(tgt_word) { continue; }
            if !reach.get_word(final_tgt) && seen.insert(final_tgt) {
                worklist.push((final_tgt, vec![WalkReason::Seed(SeedSource::IndexedDispatch { dispatch_pc: addr })]));
                added += 1;
            }
        }
    }

    stats.indexed_dispatch_roots += added;
    added
}

/// Scan reached code for `LDR Rt, [pc, #±imm12]` instructions and read
/// each literal. If the literal value points at a prologue-shaped target,
/// seed it as a worklist root. This catches function pointers passed as
/// arguments (e.g. constructor-pointer args to `__vc__FPvT1iPFPv_v`,
/// destructor-pointer args, comparator function pointers, etc.) — call
/// sites where the LDR isn't followed by an STR, so `collect_vtable_roots`
/// doesn't fire.
fn collect_fnptr_literal_roots(
    words: &[u32],
    reach: &Bitmap,
    worklist: &mut Vec<(u32, Vec<WalkReason>)>,
    stats: &mut WalkStats,
) -> usize {
    let mut added = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    for addr_idx in 0..ROM_WORD_COUNT {
        let addr = (addr_idx as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w = words[addr_idx];
        // LDR Rt, [pc, #±imm12], cond=AL: 0xE59Fxxxx (U=1) or 0xE51Fxxxx (U=0).
        let imm_sign: i32 = match w >> 16 {
            0xE59F => 1,
            0xE51F => -1,
            _ => continue,
        };
        let imm12 = (w & 0xFFF) as i32;
        let lit_pc = (addr as i64) + 8 + (imm_sign as i64 * imm12 as i64);
        if lit_pc < 0 || (lit_pc as usize) + 4 > ROM_SIZE_BYTES { continue; }
        if (lit_pc as u32) & 3 != 0 { continue; }
        let val = words[(lit_pc as usize) >> 2];
        if val == 0 { continue; }
        // Function-pointer literal: may be a direct ROM PA or a
        // patch-table VA. Resolve before validating shape.
        let final_tgt = match resolve_target_to_rom(words, val) {
            Some(t) => t,
            None => continue,
        };
        let tgt_idx = (final_tgt >> 2) as usize;
        let tgt_word = words[tgt_idx];
        if (tgt_word >> 28) == 0xF { continue; }
        if !is_known_function_start(tgt_word) { continue; }
        if reach.get_word(final_tgt) { continue; }
        if !seen.insert(final_tgt) { continue; }
        worklist.push((final_tgt, vec![WalkReason::Seed(SeedSource::FnPtrLiteral { ldr_pc: addr })]));
        added += 1;
    }
    stats.fnptr_literal_roots += added;
    added
}

fn write_bitmap(path: &Path, bm: &Bitmap) -> Result<(), String> {
    fs::write(path, &bm.bits)
        .map_err(|e| format!("write {}: {}", path.display(), e))
}

struct InvariantReport {
    oracle_popcount: u64,
    static_popcount: u64,
    oracle_only_count: u64,
    oracle_only_samples: Vec<u32>,
}

fn check_invariant(oracle: &Bitmap, static_bm: &Bitmap) -> InvariantReport {
    let mut oracle_only_count: u64 = 0;
    let mut oracle_only_samples: Vec<u32> = Vec::new();
    for i in 0..ROM_WORD_COUNT {
        let addr = (i as u32) * 4;
        let in_oracle = oracle.get_word(addr);
        let in_static = static_bm.get_word(addr);
        if in_oracle && !in_static {
            oracle_only_count += 1;
            if oracle_only_samples.len() < 32 {
                oracle_only_samples.push(addr);
            }
        }
    }
    InvariantReport {
        oracle_popcount: oracle.popcount(),
        static_popcount: static_bm.popcount(),
        oracle_only_count,
        oracle_only_samples,
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => { eprintln!("classify-rom: {}", e); return ExitCode::from(2); }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("classify-rom: {}", e); ExitCode::FAILURE }
    }
}

fn run(args: Args) -> Result<(), String> {
    let (words, hash) = load_rom_and_rex(&args.rom, &args.rex)?;
    let hash_str = format!("{:08x}", hash);

    let (sym_roots, sym_filter_stats) = load_symbol_roots(&args.symbols, &words)?;
    let mut symbols: Vec<(u32, SeedSource)> = sym_roots
        .into_iter()
        .map(|(pa, name)| (pa, SeedSource::Symbol(name)))
        .collect();
    let vectors: [u32; 8] = [0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x18, 0x1C];
    for v in vectors { symbols.push((v, SeedSource::Vector)); }

    // Manual exceptions for code unreachable by any static pass. See
    // `MANUAL_CODE_ROOTS` for the rationale per entry.
    for &p in MANUAL_CODE_ROOTS {
        symbols.push((p, SeedSource::Manual));
    }

    // C$$ctorvec / C$$dtorvec ranges: each `<prefix>vec$$Base` /
    // `<prefix>vec$$Limit` pair delimits an array of function pointers.
    // Every pointer's target is a ctor/dtor body invoked at runtime
    // by the C++ startup/shutdown loop, with no static control-flow
    // reference — without seeding them, every ctor/dtor body in
    // 0x38a4a4..0x38a930 (the entire C-runtime ctor+dtor block) goes
    // unmarked.
    let vector_ranges = load_vector_ranges(&args.symbols)?;
    let (vector_count, vector_seeds) =
        seed_vector_table_roots(&words, &vector_ranges, &mut symbols);

    // REx-header roots: parse the external REx's entry table at
    // guest PA 0x00800000 and harvest every absolute PA it references
    // (fdrv pointer-to-classinfo, FDRV classinfo scan, pkgl data
    // start). Without these the walker has no way into REX code at
    // all, since the demangled-symbol file is ROM-only.
    let rex_roots = rex_header_roots(&words);
    let mut rex_header_root_count = rex_roots.len();
    for p in rex_roots {
        symbols.push((p, SeedSource::RexHeader));
    }

    // Dedup by PA, keeping the first SeedSource we saw (Symbol seeds
    // come first so a Symbol-tagged root won't lose its name to a
    // later vector/manual/REx duplicate).
    symbols.sort_by_key(|&(pa, _)| pa);
    {
        let mut seen: HashSet<u32> = HashSet::new();
        symbols.retain(|&(pa, _)| seen.insert(pa));
    }

    // Function ranges from the filtered symbol list: half-open spans
    // (sym_n, sym_{n+1}) for use by collect_pc_relative_addr_roots
    // to scope its dispatch-base check. The last entry stretches
    // to the ROM aperture end. Reuses the same filter as the seed
    // list — entries that aren't real function entries shouldn't
    // anchor a "function range".
    let mut fn_addrs: Vec<u32> = symbols.iter().map(|&(pa, _)| pa).collect();
    fn_addrs.sort_unstable();
    fn_addrs.dedup();
    let mut fn_ranges: Vec<(u32, u32)> = fn_addrs
        .windows(2)
        .map(|w| (w[0], w[1]))
        .collect();
    if let Some(&last) = fn_addrs.last() {
        fn_ranges.push((last, ROM_SIZE_BYTES as u32));
    }

    // No JT-thunk pre-marking. The patch-table B-thunks are reached via
    // their VA-keyed entries in symbols.txt — `load_symbol_roots`
    // translates each to its thunk PA, the walker pops the thunk, marks
    // the B-AL word, and follows Step::Jump to the real ROM target.
    // Pre-walk: mark known JT-thunk page PAs as code so the walker
    // breaks on entry. These are pages where the kernel branches in
    // via VA aliasing AND the PA-decoded B target gives a wrong/garbage
    // destination. The walker won't visit them because
    // `resolve_target_to_rom` now follows the VA chain to the final ROM
    // PA — pre-marking only ensures BE-8 byteswap coverage.
    //
    // CRITICAL: don't pre-mark every page mapped by the JT L2s. Those
    // L2s alias many pages, including normal vtables (e.g. PA 0x1b000,
    // 0x1d000) whose PA-decoded B targets ARE valid (or chain-resolve
    // valid via patch-table VAs). Pre-marking those would block the
    // walker from following the vtable's targets — losing real code
    // coverage. Restrict pre-marking to the specific PA ranges we know
    // are pure thunk backing.
    const PURE_THUNK_PAGES: &[(u32, u32)] = &[
        // Patch-table backing: 17 pages × 4 KiB = 0x11000 bytes.
        // Per `docs/NEWTON_INTERNALS.md`, every word is a B-thunk
        // aliased into VA 0x01A00000+ via gROMPatchTablePageTable.
        (0x0000_2000, 0x0001_3000),
        // gROMPublicJumpTable backing: 3 pages aliased into VA
        // 0x01800000+ via gROMPublicJumpTablePageTable. PA-decoded
        // targets look in-ROM but are wrong; the kernel's runtime
        // VA decoding gives the real target (which then chain-
        // resolves through patch-table thunks).
        (0x0001_3000, 0x0001_6000),
        // Secondary-jt backing: 1 page aliased into VA 0x01E00000+
        // via the secondary-jt L2 at PA 0x7EC000.
        (0x007E_E000, 0x007E_E048),
    ];
    let mut initial_reach = Bitmap::new();
    let mut prewalk_thunk_words = 0usize;
    for &(start, end) in PURE_THUNK_PAGES {
        let mut pa = start;
        while pa < end {
            if (pa as usize) + 4 > ROM_SIZE_BYTES { break; }
            let insn = words[(pa >> 2) as usize];
            // Only mark words that actually look like B-AL thunks.
            // Trailing zero-pad / partial-page tails get skipped so
            // post-walk chain resolution can still pick up any
            // straggler thunks via reach-based discovery.
            if (insn >> 24) == 0xEA && !initial_reach.get_word(pa) {
                initial_reach.set_word(pa);
                prewalk_thunk_words += 1;
            }
            pa = pa.wrapping_add(4);
        }
    }

    let (mut reach, mut stats) = walk(&words, &symbols, initial_reach, &fn_ranges);
    stats.rex_header_roots = rex_header_root_count;

    // Post-walk: scan every reached B/BL/Bcc instruction and any
    // reached LDR-pc-rel literal. If the target/literal is a JT VA,
    // chase the chain through `resolve_jt_chain` and mark every thunk
    // PA along the way as code (so BE-8 byteswap covers them). The
    // walker no longer steps into thunk pages — `resolve_target_to_rom`
    // returns the final ROM PA via chain resolution — so without this
    // pass the thunk bytes would stay BE and the kernel's branches
    // through them would fetch garbage.
    let mut public_jt_words = 0usize;
    {
        let mut chain_thunks: HashSet<u32> = HashSet::new();
        for i in 0..ROM_WORD_COUNT {
            let pc = (i as u32) * 4;
            if !reach.get_word(pc) { continue; }
            let w = words[i];
            let cond = (w >> 28) & 0xF;
            if cond == 0xF { continue; }
            // B/BL/Bcc → target VA
            if (w >> 25) & 0b111 == 0b101 {
                let imm24 = w & 0xFFFFFF;
                let simm = sign_extend(imm24, 24) << 2;
                let target = pc.wrapping_add(8).wrapping_add(simm as u32);
                if let Some(c) = resolve_jt_chain(&words, target) {
                    for t in c.thunks { chain_thunks.insert(t); }
                }
            }
            // LDR Rt, [pc, #±imm12] → literal value (any cond)
            let masked = (w >> 16) & 0x0FFF;
            if masked == 0x59F || masked == 0x51F {
                let imm12 = (w & 0xFFF) as i32;
                let sgn = if masked == 0x59F { 1 } else { -1 };
                let lit_pc = (pc as i64) + 8 + (sgn as i64) * (imm12 as i64);
                if lit_pc >= 0 && (lit_pc as usize) + 4 <= ROM_SIZE_BYTES
                    && (lit_pc as u32) & 3 == 0
                {
                    let lit = words[(lit_pc as usize) >> 2];
                    if let Some(c) = resolve_jt_chain(&words, lit) {
                        for t in c.thunks { chain_thunks.insert(t); }
                    }
                }
            }
        }
        for t in chain_thunks {
            if !reach.get_word(t) {
                reach.set_word(t);
                public_jt_words += 1;
            }
        }
    }

    // Manual data-range exclusions: clear reach bits in any range
    // listed in `MANUAL_DATA_RANGES`. Runs after the JT-thunk mark so
    // an exclusion can override a thunk that happens to overlap (e.g.
    // a thunk page that's actually data per a hand-fixed gap).
    let mut manual_data_words_cleared = 0usize;
    for &(start, end) in MANUAL_DATA_RANGES {
        let mut pa = start;
        while pa < end {
            if reach.get_word(pa) {
                reach.clear_word(pa);
                manual_data_words_cleared += 1;
            }
            pa = pa.wrapping_add(4);
        }
    }
    // Dedup sometimes removes some of the REx entries (they may
    // overlap vectors/symbols); report the actual count after merge.
    let _ = &mut rex_header_root_count;

    // Build byte-access-static: reach ∧ is_byte_access(insn).
    let mut ba_static = Bitmap::new();
    let mut kind_counts: [u64; 3] = [0, 0, 0]; // byte / halfword-ish / swpb (informational)
    for i in 0..ROM_WORD_COUNT {
        let addr = (i as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w = words[i];
        if !is_byte_access(w) { continue; }
        ba_static.set_word(addr);
        let classify = classify_kind(w);
        kind_counts[classify] += 1;
    }

    let out_dir: PathBuf = args.out_root.join(&hash_str);
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    write_bitmap(&out_dir.join("byte-access-static.bitmap"), &ba_static)?;
    write_bitmap(&out_dir.join("reach.bitmap"), &reach)?;

    // Invariant check vs the oracle bitmap, if it exists.
    let oracle_path = out_dir.join("byte-access.bitmap");
    let invariant = match fs::read(&oracle_path) {
        Ok(bytes) => {
            let oracle = Bitmap::from_bytes(bytes)
                .map_err(|e| format!("read {}: {}", oracle_path.display(), e))?;
            Some(check_invariant(&oracle, &ba_static))
        }
        Err(_) => None,
    };

    let summary_path = out_dir.join("summary.txt");
    let mut f = fs::File::create(&summary_path)
        .map_err(|e| format!("create {}: {}", summary_path.display(), e))?;
    writeln!(f, "classify-rom byte-access-static summary").ok();
    writeln!(f, "  inputs:").ok();
    writeln!(f, "    rom     = {}", args.rom.display()).ok();
    writeln!(f, "    rex     = {}", args.rex.display()).ok();
    writeln!(f, "    symbols = {}", args.symbols.display()).ok();
    writeln!(f, "  hash(fnv1a32 of rom || rex) = 0x{}", hash_str).ok();
    writeln!(f, "  symbol filter (load_symbol_roots):").ok();
    writeln!(f, "    raw lines:               {}", sym_filter_stats.raw_lines).ok();
    writeln!(f, "    parse-skipped:           {}", sym_filter_stats.parse_skipped).ok();
    writeln!(f, "    misaligned addr:         {}", sym_filter_stats.misaligned).ok();
    writeln!(f, "    out-of-rom addr (>=16M): {}", sym_filter_stats.out_of_rom).ok();
    writeln!(f, "    linker-marker dropped:   {}", sym_filter_stats.linker_marker_dropped).ok();
    writeln!(f, "    g[A-Z]* dropped:         {}", sym_filter_stats.g_prefix_dropped).ok();
    writeln!(f, "    k[A-Z]* dropped:         {}", sym_filter_stats.k_prefix_dropped).ok();
    writeln!(f, "    NV-cond first-word:      {}", sym_filter_stats.nv_cond_dropped).ok();
    writeln!(f, "    not-prologue first-word: {}", sym_filter_stats.not_prologue_dropped).ok();
    writeln!(f, "    data-stop-range:         {}", sym_filter_stats.data_stop_dropped).ok();
    writeln!(f, "    JT-VA resolved → thunk:  {}", sym_filter_stats.jt_va_resolved).ok();
    writeln!(f, "    accepted:                {}", sym_filter_stats.accepted).ok();
    writeln!(f, "  vector tables (ctorvec / dtorvec):").ok();
    writeln!(f, "    ranges discovered:       {}", vector_count).ok();
    writeln!(f, "    function pointers seeded: {}", vector_seeds).ok();
    for (label, base, limit) in &vector_ranges {
        writeln!(f, "    {label}: 0x{:08x}..0x{:08x} ({} bytes)",
                 base, limit, limit - base).ok();
    }
    writeln!(f, "  manual data-range exclusions:").ok();
    writeln!(f, "    words cleared: {}", manual_data_words_cleared).ok();
    writeln!(f, "  gROMPublicJumpTable post-walk mark:").ok();
    writeln!(f, "    pre-walk thunk-page words: {}", prewalk_thunk_words).ok();
    writeln!(f, "    chain-resolved thunk words: {}", public_jt_words).ok();
    writeln!(f, "  walker:").ok();
    writeln!(f, "    symbol roots (post-merge): {}", stats.initial_roots).ok();
    writeln!(f, "    rex-header roots added:    {}", stats.rex_header_roots).ok();
    writeln!(f, "    words walked (with revisits): {}", stats.words_walked).ok();
    writeln!(f, "    NV-cond words skipped:     {}", stats.nv_cond_skips).ok();
    writeln!(f, "    data-range terminations:   {}", stats.data_range_stops).ok();
    writeln!(f, "    vtable passes:             {}", stats.indirect_passes).ok();
    writeln!(f, "    vtables found:             {}", stats.vtables_found).ok();
    writeln!(f, "    vtable method roots added: {}", stats.vtable_roots).ok();
    writeln!(f, "    fnptr literal roots added: {}", stats.fnptr_literal_roots).ok();
    writeln!(f, "    B-run dispatch roots added: {}", stats.b_run_roots).ok();
    writeln!(f, "    PC-rel addr roots added:    {}", stats.pc_rel_addr_roots).ok();
    writeln!(f, "    TClassInfo struct roots:    {}", stats.classinfo_roots).ok();
    writeln!(f, "    alt-entry-after-ip-jump:    {}", stats.alt_entry_roots).ok();
    writeln!(f, "    indexed-dispatch roots:     {}", stats.indexed_dispatch_roots).ok();
    writeln!(f, "    total indirect roots added: {}", stats.indirect_roots_added).ok();
    writeln!(f, "    literal-pool words cleared: {}", stats.literal_targets_cleared).ok();
    writeln!(f, "    rom-soup walk-entries:     {}", stats.rom_soup_entries).ok();
    writeln!(f, "    reachable-code popcount: {}", reach.popcount()).ok();
    writeln!(f, "  byte-access-static.bitmap popcount = {}", ba_static.popcount()).ok();
    writeln!(f, "    of which byte (LDRB/STRB):              {}", kind_counts[0]).ok();
    writeln!(f, "    of which halfword/signed (LDRH/...):    {}", kind_counts[1]).ok();
    writeln!(f, "    of which swpb:                          {}", kind_counts[2]).ok();
    if let Some(rep) = &invariant {
        writeln!(f, "  invariant check (oracle ⊆ static):").ok();
        writeln!(f, "    oracle popcount = {}", rep.oracle_popcount).ok();
        writeln!(f, "    static popcount = {}", rep.static_popcount).ok();
        writeln!(f, "    oracle bits missing from static = {}", rep.oracle_only_count).ok();
        if !rep.oracle_only_samples.is_empty() {
            writeln!(f, "    first {} offending PCs:", rep.oracle_only_samples.len()).ok();
            for pc in &rep.oracle_only_samples {
                let w = words[(*pc >> 2) as usize];
                writeln!(f, "      0x{:08x}  insn=0x{:08x}", pc, w).ok();
            }
        }
    }

    println!("classify-rom: wrote {}", out_dir.display());
    println!("  byte-access-static popcount = {}", ba_static.popcount());
    println!("  (byte={} halfword={} swpb={})", kind_counts[0], kind_counts[1], kind_counts[2]);
    match invariant {
        Some(rep) if rep.oracle_only_count == 0 => {
            println!("  invariant OK: oracle {} ⊆ static {}",
                rep.oracle_popcount, rep.static_popcount);
            Ok(())
        }
        Some(rep) => {
            eprintln!("classify-rom: INVARIANT VIOLATED — {} oracle bits missing from static",
                rep.oracle_only_count);
            eprintln!("  first offending PCs (see summary.txt for details):");
            for pc in rep.oracle_only_samples.iter().take(8) {
                let w = words[(*pc >> 2) as usize];
                eprintln!("    0x{:08x}  insn=0x{:08x}", pc, w);
            }
            Err("invariant violated".into())
        }
        None => {
            println!("  (no oracle bitmap at {} — invariant not checked)",
                oracle_path.display());
            Ok(())
        }
    }
}

fn classify_kind(w: u32) -> usize {
    // LDRB/STRB immediate or register.
    if (w & 0x0E00_0000) == 0x0400_0000 && (w & (1 << 22)) != 0 { return 0; }
    if (w & 0x0E00_0010) == 0x0600_0000 && (w & (1 << 22)) != 0 { return 0; }
    // SWPB.
    if (w & 0x0FF0_0FF0) == 0x0140_0090 { return 2; }
    // Halfword/signed — everything else accepted by is_byte_access.
    1
}
