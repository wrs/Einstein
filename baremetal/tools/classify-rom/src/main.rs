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
    data_ranges: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut rom: Option<PathBuf> = None;
    let mut rex: Option<PathBuf> = None;
    let mut symbols: Option<PathBuf> = None;
    let mut out_root: Option<PathBuf> = None;
    let mut data_ranges: Option<PathBuf> = None;

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
            "--data-ranges" => data_ranges = Some(take(&mut it, "--data-ranges")?),
            "-h" | "--help" => {
                eprintln!("classify-rom --rom <path> --rex <path> --symbols <path> --out <dir> [--data-ranges <path>]");
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
        data_ranges,
    })
}

/// Parse the half-open `0xSTART 0xEND` pairs produced by
/// `baremetal/scripts/classify-symbols.py`. Blank/comment lines are
/// ignored. Returns ranges sorted by start address — the walker does
/// a linear scan against them (a few hundred entries, so no need for
/// a tree).
fn load_data_ranges(path: &Path) -> Result<Vec<(u32, u32)>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let mut parts = line.split_whitespace();
        let a = parts.next().ok_or_else(||
            format!("{}:{}: missing start", path.display(), lineno + 1))?;
        let b = parts.next().ok_or_else(||
            format!("{}:{}: missing end", path.display(), lineno + 1))?;
        let parse_addr = |s: &str| -> Result<u32, String> {
            let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
                .unwrap_or(s);
            u32::from_str_radix(hex, 16)
                .map_err(|e| format!("{}:{}: {}: {}", path.display(), lineno + 1, s, e))
        };
        let start = parse_addr(a)?;
        let end = parse_addr(b)?;
        if end <= start { continue; }
        out.push((start, end));
    }
    out.sort_unstable();
    Ok(out)
}

/// Half-open address-in-range test against a sorted `ranges` slice.
fn in_any_range(addr: u32, ranges: &[(u32, u32)]) -> bool {
    // Binary search by start. Then check the adjacent entry.
    let idx = ranges.partition_point(|&(s, _)| s <= addr);
    if idx == 0 { return false; }
    let (s, e) = ranges[idx - 1];
    s <= addr && addr < e
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

/// Parse demangled symbols with the same three-gate filter as
/// baremetal/build.rs:46-143.
fn load_symbol_roots(path: &Path) -> Result<Vec<u32>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut out: Vec<u32> = Vec::new();

    for line in text.lines() {
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
            _ => continue,
        };

        let hex = addr_s.strip_prefix("0x").or_else(|| addr_s.strip_prefix("0X"))
            .unwrap_or(addr_s);
        let addr = match u32::from_str_radix(hex, 16) {
            Ok(a) => a,
            Err(_) => continue,
        };

        if addr & 3 != 0 { continue; }
        if addr as usize >= ROM_SIZE_BYTES { continue; }

        if name.contains("$$")
            || name.starts_with("Image$")
            || name.ends_with("$Size")
            || name.ends_with("$Length")
            || name.ends_with("$Base")
            || name.ends_with("$Limit")
            || name.ends_with("$End")
            || name.ends_with("$ZI")
        {
            continue;
        }

        // Accept every symbol that passes the linker-marker filter as a
        // candidate root. Lowercase C functions (toupper, putc, ...) lack
        // the `::` or `(` hints of demangled C++ symbols but are real
        // call targets. Data-symbol roots (gFoo, kFoo) whose first word
        // is NV-cond data terminate the walker immediately; other data
        // roots walk a few words and terminate on gibberish control flow,
        // potentially marking a small run of data as code-reachable. That
        // is a tolerated false-positive risk for the static bitmap; the
        // oracle ⊆ static invariant guarantees no missed executed sites.
        if seen.insert(addr) {
            out.push(addr);
        }
    }
    out.sort_unstable();
    Ok(out)
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
                    if p & 3 != 0 { continue; }
                    if (p as usize) >= ROM_SIZE_BYTES { continue; }
                    let tgt_idx = (p >> 2) as usize;
                    let tgt = words[tgt_idx];
                    if (tgt >> 28) == 0xF { continue; }
                    if !is_known_function_start(tgt) { continue; }
                    out.push(p);
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

/// Function-prologue allowlist from baremetal/src/tracer.rs:96-125.
/// Used for indirect-target recovery: values that point to a prologue-shaped
/// word are treated as function pointers.
fn is_known_function_start(w: u32) -> bool {
    if (w & 0xF000_0000) != 0xE000_0000 { return false; }
    if (w & 0x0FFF_0000) == 0x092D_0000 && (w & 0xFFFF) != 0 { return true; }
    if (w & 0x0FFF_F000) == 0x024D_D000 { return true; }
    if (w & 0x0FFF_F000) == 0x028D_C000 { return true; }
    if w == 0xE52D_E004 { return true; }
    if w == 0xE1A0_C00D { return true; }
    if (w & 0x0FFF_0000) == 0x03A0_0000 { return true; }
    if (w & 0x0FFF_0000) == 0x03E0_0000 { return true; }
    if (w & 0x0FFF_0FF0) == 0x01A0_0000 { return true; }
    if (w & 0x0FFF_F000) == 0x059F_0000 { return true; }
    if (w & 0x0FE0_0F10) == 0x0E00_0F10 { return true; }
    if (w & 0x0F00_0000) == 0x0A00_0000 { return true; }
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

    fn popcount(&self) -> u64 {
        self.bits.iter().map(|b| b.count_ones() as u64).sum()
    }
}

enum Step {
    Continue { branch: Option<u32> },
    Jump(u32),
    Stop,
}

fn sign_extend(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

/// Returns true if `w` sets LR to the address immediately after it
/// (Newton's hand-rolled manual-BL idiom: a following PC-write is a call,
/// not a terminal jump, because the callee returns via LR).
fn sets_lr_to_return_here(w: u32) -> bool {
    // MOV LR, PC → LR = PC+8-of-this-insn = (this_pc + 8). The following
    // insn at (this_pc + 4) executes; a PC-write at (this_pc + 4) is a
    // call whose return address is (this_pc + 8) = (call_pc + 4). Fall
    // through from the PC-write is live.
    if w == 0xE1A0_E00F { return true; }
    // ADD LR, PC, #0 — equivalent effect to MOV LR, PC on ARM (imm12 = 0).
    if w == 0xE28F_E000 { return true; }
    false
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
/// `is_byte_access`. `prev_w` is the preceding word (used to recognise
/// Newton's manual-BL idiom). `in_table` says the walker is currently
/// traversing a jump-table fall-through — the caller sets this after a
/// PC-writing instruction; while true, unconditional `B` is interpreted
/// as a table entry (push target, keep walking) rather than a terminal
/// jump.
fn step(w: u32, pc: u32, prev_w: u32, in_table: bool) -> Step {
    let cond = (w >> 28) & 0xF;
    let prev_sets_lr = sets_lr_to_return_here(prev_w);

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

    // SWI (unconditional).
    if cond == 0xE && (w >> 24) & 0xF == 0xF {
        return Step::Stop;
    }

    // UDF (A1 encoding family).
    if cond == 0xE && (w & 0x0FF0_00F0) == 0x0700_00F0 {
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
}

/// Walk from roots, closing over indirect-targets by scanning unreached
/// word-aligned values for function-pointer-shaped data. Returns the
/// reachable-code bitmap. The caller then intersects with
/// `is_byte_access` to produce the final byte-access-static bitmap.
fn walk(
    words: &[u32],
    initial_roots: &[u32],
    data_ranges: &[(u32, u32)],
) -> (Bitmap, WalkStats) {
    let mut stats = WalkStats::default();
    let mut reach = Bitmap::new();
    let mut worklist: Vec<u32> = initial_roots
        .iter()
        .copied()
        .filter(|a| !in_any_range(*a, data_ranges))
        .collect();
    stats.initial_roots = worklist.len();

    let mut pass = 0u32;
    loop {
        pass += 1;
        while let Some(pc) = worklist.pop() {
            let mut cur = pc;
            let mut prev_w: u32 = 0;
            let mut in_table = false;
            loop {
                if (cur as usize) >= ROM_SIZE_BYTES || cur & 3 != 0 { break; }
                if reach.get_word(cur) { break; }
                // Curated data ranges from classify-symbols.py terminate
                // the walker. These are contiguous extents of data
                // symbols (vtables, string tables, literal pools
                // between functions, ...) that real code never steps
                // through; if we fall into one we've mis-identified a
                // function boundary, and continuing would mark data
                // as reachable code.
                if in_any_range(cur, data_ranges) {
                    stats.data_range_stops += 1;
                    break;
                }
                let w = words[(cur >> 2) as usize];
                if (w >> 28) == 0xF {
                    stats.nv_cond_skips += 1;
                    break;
                }
                reach.set_word(cur);
                stats.words_walked += 1;
                let step_result = step(w, cur, prev_w, in_table);
                // Update table state for the next iteration: entering a
                // table when the current insn writes PC (dispatch), staying
                // in a table while we keep seeing unconditional `B` entries,
                // leaving once any other word appears.
                let cond = (w >> 28) & 0xF;
                let is_b_al = ((w >> 25) & 0b111) == 0b101
                    && cond == 0xE
                    && ((w >> 24) & 1) == 0;
                in_table = if is_pc_write(w) {
                    true
                } else if in_table && is_b_al {
                    true
                } else {
                    false
                };
                match step_result {
                    Step::Continue { branch: Some(t) } => {
                        worklist.push(t);
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
                        cur = t;
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
            words, &reach, data_ranges, &mut worklist, &mut stats,
        );

        stats.indirect_passes = pass;
        stats.indirect_roots_added += new_roots;
        if new_roots == 0 { break; }
    }

    (reach, stats)
}

/// Scan reached code for the LDR-PC-rel + STR-to-this install pair,
/// chase the literal to a vtable, enumerate its method pointers, and
/// add each as a worklist root. Returns the number of method roots
/// pushed this call.
fn collect_vtable_roots(
    words: &[u32],
    reach: &Bitmap,
    data_ranges: &[(u32, u32)],
    worklist: &mut Vec<u32>,
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
        if in_any_range(vtable_addr, data_ranges) { continue; }
        if !seen.insert(vtable_addr) { continue; }

        // Enumerate consecutive method pointers at vtable_addr. Stop
        // at the first word that doesn't point at a prologue-shaped
        // target, or that points into a known data range. Also bound
        // the scan so a runaway (pointer-shape noise) can't walk the
        // whole ROM.
        const MAX_VTABLE_ENTRIES: usize = 256;
        let mut entries_added = 0usize;
        for j in 0..MAX_VTABLE_ENTRIES {
            let vptr_addr = vtable_addr.wrapping_add((j as u32) * 4);
            if (vptr_addr as usize) + 4 > ROM_SIZE_BYTES { break; }
            let p = words[(vptr_addr as usize) >> 2];
            if p == 0 { break; }
            if p & 3 != 0 { break; }
            if (p as usize) >= ROM_SIZE_BYTES { break; }
            if in_any_range(p, data_ranges) { break; }
            let tgt_idx = (p >> 2) as usize;
            let tgt_word = words[tgt_idx];
            if (tgt_word >> 28) == 0xF { break; }
            if !is_known_function_start(tgt_word) { break; }
            if !reach.get_word(p) {
                worklist.push(p);
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

    let mut symbols = load_symbol_roots(&args.symbols)?;
    let vectors: [u32; 8] = [0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x18, 0x1C];
    for v in vectors { symbols.push(v); }

    // REx-header roots: parse the external REx's entry table at
    // guest PA 0x00800000 and harvest every absolute PA it references
    // (fdrv pointer-to-classinfo, FDRV classinfo scan, pkgl data
    // start). Without these the walker has no way into REX code at
    // all, since the demangled-symbol file is ROM-only.
    let rex_roots = rex_header_roots(&words);
    let mut rex_header_root_count = rex_roots.len();
    symbols.extend(rex_roots);
    symbols.sort_unstable();
    symbols.dedup();

    let data_ranges = match &args.data_ranges {
        Some(p) => load_data_ranges(p)?,
        None => Vec::new(),
    };

    let (reach, mut stats) = walk(&words, &symbols, &data_ranges);
    stats.rex_header_roots = rex_header_root_count;
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
    writeln!(f, "  walker:").ok();
    writeln!(f, "    symbol roots (post-merge): {}", stats.initial_roots).ok();
    writeln!(f, "    rex-header roots added:    {}", stats.rex_header_roots).ok();
    writeln!(f, "    words walked (with revisits): {}", stats.words_walked).ok();
    writeln!(f, "    NV-cond words skipped:     {}", stats.nv_cond_skips).ok();
    writeln!(f, "    data-range terminations:   {}", stats.data_range_stops).ok();
    writeln!(f, "    vtable passes:             {}", stats.indirect_passes).ok();
    writeln!(f, "    vtables found:             {}", stats.vtables_found).ok();
    writeln!(f, "    vtable method roots added: {}", stats.vtable_roots).ok();
    writeln!(f, "    total indirect roots added: {}", stats.indirect_roots_added).ok();
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
