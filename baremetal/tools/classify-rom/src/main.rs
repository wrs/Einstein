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

/// If `target` is a VA in the ROM patch-table range, decode the B/BL
/// thunk living at the corresponding phys slot and return the final
/// ROM-PA target. Otherwise `None`. Returns the target unchanged when
/// the slot's word isn't a B/BL — patch-table slots can be patched at
/// boot to non-B forms, but the static (unpatched) image is always a
/// B-AL thunk to the real ROM function.
fn resolve_jt_va(words: &[u32], target: u32) -> Option<u32> {
    let phys = jt_va_to_phys(target)?;
    if (phys as usize) + 4 > ROM_SIZE_BYTES { return None; }
    if phys & 3 != 0 { return None; }
    let w = words[(phys >> 2) as usize];
    // B/BL at the slot's runtime VA: target = va + 8 + sext(imm24)<<2.
    let cond = (w >> 28) & 0xF;
    let kind = (w >> 25) & 0b111;
    if cond == 0xF || kind != 0b101 { return None; }
    let imm24 = w & 0x00FF_FFFF;
    let simm = sign_extend(imm24, 24) << 2;
    let final_tgt = target.wrapping_add(8).wrapping_add(simm as u32);
    if (final_tgt as usize) >= ROM_SIZE_BYTES { return None; }
    if final_tgt & 3 != 0 { return None; }
    Some(final_tgt)
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

/// Normalise a branch / function-pointer target to the in-ROM PA the
/// walker should add as a root. Returns `Some(pa)` for direct ROM-PA
/// targets, or the *thunk* PA for VAs that resolve through one of the
/// kernel's known stage-1 aliases (post-ship patch table, secondary
/// jump-table). Returns `None` for arbitrary out-of-ROM addresses.
///
/// Returning the thunk PA — rather than the resolved final-target PA
/// `resolve_jt_va` produces — lets the walker visit the thunk's B
/// instruction directly, mark it as reached, and naturally follow the
/// B to the final target. The reach bit on the thunk is load-bearing
/// under BE-8: the loader byteswaps every "code" word so the CPU's LE
/// instruction fetch decodes the original BE numerical encoding; if
/// the thunk is left as data, the kernel's branch through the
/// patch-table VA fetches a byteswapped (garbage) instruction and
/// either UND-faults or drifts into invalid behaviour.
///
/// Prologue-shape checks at thunk PAs accept B-AL via
/// `is_known_function_start`, so callers that gate worklist pushes on
/// `is_known_function_start(words[pa>>2])` keep working.
fn resolve_target_to_rom(words: &[u32], target: u32) -> Option<u32> {
    if target & 3 != 0 { return None; }
    if (target as usize) < ROM_SIZE_BYTES { return Some(target); }
    if let Some(thunk_pa) = jt_va_to_phys(target) {
        // Sanity: confirm the slot decodes as a B/BL — patch-table slots
        // can be patched at boot to non-B forms (the runtime patches
        // them post-boot, but the static image we're walking is always
        // a thunk).
        return resolve_jt_va(words, target).map(|_| thunk_pa);
    }
    secondary_jt_va_to_phys(words, target)
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
    worklist: &mut Vec<u32>,
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
            worklist.push(target);
        }
        worklist.push(tbl);
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
}

/// Walk from roots, closing over indirect-targets by scanning unreached
/// word-aligned values for function-pointer-shaped data. Returns the
/// reachable-code bitmap. The caller then intersects with
/// `is_byte_access` to produce the final byte-access-static bitmap.
fn walk(
    words: &[u32],
    initial_roots: &[u32],
    fn_ranges: &[(u32, u32)],
) -> (Bitmap, WalkStats) {
    let mut stats = WalkStats::default();
    let mut reach = Bitmap::new();
    let mut worklist: Vec<u32> = initial_roots
        .iter()
        .copied()
        .collect();
    stats.initial_roots = worklist.len();

    let mut pass = 0u32;
    loop {
        pass += 1;
        while let Some(pc) = worklist.pop() {
            let mut cur = pc;
            let mut prev_w: u32 = 0;
            let mut in_table = false;
            // LR-target tracker for multi-insn manual-BL idioms.
            // When `lr_target == Some(cur + 4)` at a PC-writing
            // instruction, the call returns at cur+4 — even when
            // the LR setup happened several insns earlier. Cleared
            // when a non-PC, non-call instruction overwrites LR.
            let mut lr_target: Option<u32> = None;
            loop {
                if (cur as usize) >= ROM_SIZE_BYTES || cur & 3 != 0 { break; }
                if reach.get_word(cur) { break; }
                let w = words[(cur >> 2) as usize];
                if (w >> 28) == 0xF {
                    stats.nv_cond_skips += 1;
                    break;
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
                        words, cur, prev_w, fn_ranges, &mut worklist,
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
                            worklist.push(root);
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
                        cur = match resolve_target_to_rom(words, t) {
                            Some(pa) => pa,
                            None => break,
                        };
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
/// `LDR Rt, [pc, #±imm12]` (cond=AL) issued from a word currently in
/// `reach`. Returns the number of bits cleared.
///
/// This is a static-only signal: a literal-pool word is reached as
/// CODE by the walker (e.g. via a conditional branch) but the kernel
/// also reads it as DATA via the LDR. Under BE-8, code and data
/// loads have different byte-order requirements; since the LDR-data
/// reading is the load-bearing path (constants used by the kernel),
/// we mark the word as data.
fn clear_literal_pool_targets_from_reach(words: &[u32], reach: &mut Bitmap) -> u64 {
    let mut targets: Vec<u32> = Vec::new();
    for i in 0..ROM_WORD_COUNT {
        let addr = (i as u32) * 4;
        if !reach.get_word(addr) { continue; }
        let w = words[i];
        let imm_sign: i32 = match w >> 16 {
            0xE59F => 1,
            0xE51F => -1,
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
    worklist: &mut Vec<u32>,
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
        worklist.push(target);
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
    worklist: &mut Vec<u32>,
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
                worklist.push(entry_pa);
                added += 1;
            }
        }
        i = j.max(i + 1);
    }
    stats.b_run_roots += added;
    added
}

/// Recognize TClassInfo trampoline functions and walk the inline
/// 60-byte struct that precedes them.
///
/// Newton's class metadata layout (TClassInfo, 15 longs = 60 bytes)
/// terminates in a 4-instruction tail-stub:
///
///   sub  r0, pc, #68     ; r0 = struct_base = pc + 8 - 68
///   mov  pc, lr          ; trampoline returns the struct's base PA
///   mov  r0, #imm        ; alt entry: bail-out function returning <imm>
///   mov  pc, lr          ;            (typically nil, i.e. imm == 0)
///
/// The 60 bytes preceding the trampoline are the struct itself, with
/// these "Branch" fields holding inline `B method` slots:
/// fSizeofBranch, fAllocBranch, fFreeBranch, fDefaultNewBranch,
/// fDefaultDeleteBranch, fSelectorBranch, fReserved2. Each slot is
/// either a real `B` to a method body, the inline `mov pc, lr` empty
/// stub, or zero.
///
/// `collect_b_run_roots` catches dense runs of these (≥3 consecutive
/// B-AL words), but isolated branches — most importantly the
/// `B alt_entry` slot at +0x38 that points at the bail-out function —
/// fall below that threshold and remain unreached. Recognising the
/// trampoline pattern lets us seed the entire struct precisely.
///
/// The pattern is highly specific (4 exact instructions in sequence),
/// so this scans the full ROM+REX rather than gating on already-
/// reached code; a TClassInfo whose trampoline isn't a known symbol
/// (3 such cases observed) is still discovered.
fn collect_classinfo_roots(
    words: &[u32],
    reach: &Bitmap,
    worklist: &mut Vec<u32>,
    stats: &mut WalkStats,
) -> usize {
    const TRAMP_SUB_R0_PC_68: u32 = 0xE24F_0044;
    const MOV_PC_LR: u32 = 0xE1A0_F00E;
    const STRUCT_BYTES: u32 = 60;

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

        // Seed the trampoline function itself — covers TClassInfo
        // entries that aren't named in code-symbols.txt.
        if !reach.get_word(fn_pa) && seen.insert(fn_pa) {
            worklist.push(fn_pa);
            added += 1;
        }

        // Scan the 60-byte struct for B-AL slots. Each one is a
        // method-dispatch branch; seeding it lets the walker
        // Step::Jump to the target. The B->alt_entry slot at +0x38
        // naturally pulls in `fn + 8` this way.
        let sb_idx = fn_idx - struct_words;
        for k_off in 0..struct_words {
            let w = words[sb_idx + k_off];
            if (w >> 24) != 0xEA { continue; }
            let pa = (sb_idx as u32 + k_off as u32) * 4;
            if !reach.get_word(pa) && seen.insert(pa) {
                worklist.push(pa);
                added += 1;
            }
        }
    }

    stats.classinfo_roots += added;
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
    worklist: &mut Vec<u32>,
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
                worklist.push(final_tgt);
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
    worklist: &mut Vec<u32>,
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
        worklist.push(final_tgt);
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

    // Function ranges from code-symbols.txt: half-open spans
    // (sym_n, sym_{n+1}) for use by collect_pc_relative_addr_roots
    // to scope its dispatch-base check. The last entry stretches
    // to the ROM aperture end.
    let mut fn_addrs = load_symbol_roots(&args.symbols)?;
    fn_addrs.sort_unstable();
    fn_addrs.dedup();
    let mut fn_ranges: Vec<(u32, u32)> = fn_addrs
        .windows(2)
        .map(|w| (w[0], w[1]))
        .collect();
    if let Some(&last) = fn_addrs.last() {
        fn_ranges.push((last, ROM_SIZE_BYTES as u32));
    }

    let (reach, mut stats) = walk(&words, &symbols, &fn_ranges);
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
    writeln!(f, "    indexed-dispatch roots:     {}", stats.indexed_dispatch_roots).ok();
    writeln!(f, "    total indirect roots added: {}", stats.indirect_roots_added).ok();
    writeln!(f, "    literal-pool words cleared: {}", stats.literal_targets_cleared).ok();
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
