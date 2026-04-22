//! Function-level execution trace (single-shot first-touch).
//!
//! When the `trace` cargo feature is on, `build.rs` parses
//! `_Data_/demangled_symbols.txt` and emits three blobs into OUT_DIR
//! which we include here:
//!
//!   fn_addrs.bin       packed u32 LE — sorted ROM-range entry PAs
//!   fn_name_offs.bin   packed u32 LE — offsets into the name pool
//!   fn_names.bin       NUL-separated names (name pool)
//!
//! At ROM load time `init()` walks the table and, for each entry whose
//! current first word matches a known ARM function prologue, stashes
//! the original word in `ORIG_INSN[index]` and overwrites it with
//! `UDF #index` (A1 encoding: `0xE7F0_00F0` with the 16-bit index split
//! across imm12/imm4). Data words mislabeled as functions fail the
//! prologue check and are left alone — the "skipped" counter printed at
//! init time is the canary.
//!
//! At trap time `handle_trace_und` is invoked from `trap.rs::handle_und`
//! when the faulting opcode falls in the UDF A1 encoding range. We log
//! the function's name, restore the original instruction via a direct
//! write to the ROM backing store, invalidate the icache line, rewind
//! ELR to the faulting PC, and ERET. The guest re-executes the restored
//! instruction at native speed, and every subsequent call runs at native
//! speed too — each function logs **once per boot**.
//!
//! This changes the snapshot ROM fingerprint (many ROM words move), so
//! runs with `trace` enabled always cold-boot in practice. That's by
//! design: tracing is a debug mode, not a resume-from-snapshot mode.

use crate::cpu;
use crate::guest_mem;
use crate::kprintln;
use crate::trap::{TrapContext, UND_SAVE_LR_SVC_IPA};

const FN_ADDRS_RAW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_addrs.bin"));
const FN_NAME_OFFS_RAW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_name_offs.bin"));
const NAME_POOL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fn_names.bin"));

const FN_COUNT: usize = FN_ADDRS_RAW.len() / 4;

/// Saved original first-word of each patched function, indexed by the
/// same position as `FN_ADDRS_RAW`. Zero means "not patched" (we either
/// skipped it at init time or its original word really was zero — in
/// the latter case we wouldn't have patched anyway because 0x0000_0000
/// is not a valid prologue).
static mut ORIG_INSN: [u32; FN_COUNT] = [0u32; FN_COUNT];

/// Sequence counter for the trace log. Bumped on every first-touch.
static mut TRACE_SEQ: u32 = 0;

/// UDF A1 encoding mask: cond=AL, bits[27:20]=0x7F, bits[7:4]=0xF.
const UDF_MATCH_MASK: u32 = 0xFFF0_00F0;
const UDF_MATCH_VAL: u32 = 0xE7F0_00F0;

/// The 16-bit index cap imposed by UDF's imm12/imm4 split. If the
/// symbol table ever grows past this, `init()` truncates and warns.
const MAX_INDEX: usize = 0x1_0000;

/// Build a UDF A1 instruction encoding from a 16-bit immediate.
fn encode_udf(imm16: u16) -> u32 {
    let hi12 = (imm16 as u32) >> 4;
    let lo4 = (imm16 as u32) & 0xF;
    UDF_MATCH_VAL | (hi12 << 8) | lo4
}

/// Decode the 16-bit immediate of a UDF A1 encoding. Returns None for
/// any other opcode.
pub fn decode_udf(insn: u32) -> Option<u16> {
    if (insn & UDF_MATCH_MASK) != UDF_MATCH_VAL { return None; }
    let hi12 = (insn >> 8) & 0xFFF;
    let lo4 = insn & 0xF;
    Some(((hi12 << 4) | lo4) as u16)
}

/// True if `insn` matches one of a small set of ARM opcodes that the
/// Newton 717006 kernel uses at function entry. Strict allowlist —
/// anything not on this list is treated as data, because binary data
/// words can coincidentally pass a structural "looks like code" test.
///
/// cond is always AL (0xE) in this list; Newton's kernel doesn't open
/// functions with conditional instructions. Each mask below assumes
/// bits[31:28]=0xE and matches the opcode shape below it:
///
///   PUSH / STMFD sp!, {reglist}           0xE92D_xxxx (reglist != 0)
///   SUB sp, sp, #imm                      0xE24D_Dxxx
///   ADD ip, sp, #imm                      0xE28D_Cxxx
///   STR lr, [sp, #-4]!                    0xE52D_E004
///   MOV ip, sp                            0xE1A0_C00D
///   MOV Rd, #imm                          0xE3A0_xxxx
///   MVN Rd, #imm                          0xE3E0_xxxx
///   MOV Rd, Rm (register, no shift)       0xE1A0_xxxx
///   LDR Rd, [pc, #imm]                    0xE59F_xxxx
///   MRC/MCR p15, ...                      0xEEx0_xFyx (x = don't-care bits)
///   B <label>                             0xEA00_0000..0xEAFF_FFFF
fn is_known_function_start(w: u32) -> bool {
    // Require cond = AL for every shape below.
    if (w & 0xF000_0000) != 0xE000_0000 { return false; }

    // PUSH / STMFD sp!, {reglist}
    if (w & 0x0FFF_0000) == 0x092D_0000 && (w & 0xFFFF) != 0 { return true; }
    // SUB sp, sp, #imm (any imm12)
    if (w & 0x0FFF_F000) == 0x024D_D000 { return true; }
    // ADD ip, sp, #imm
    if (w & 0x0FFF_F000) == 0x028D_C000 { return true; }
    // STR lr, [sp, #-4]!
    if w == 0xE52D_E004 { return true; }
    // MOV ip, sp
    if w == 0xE1A0_C00D { return true; }
    // MOV Rd, #imm (imm12 form, no flag set)
    if (w & 0x0FFF_0000) == 0x03A0_0000 { return true; }
    // MVN Rd, #imm
    if (w & 0x0FFF_0000) == 0x03E0_0000 { return true; }
    // MOV Rd, Rm (register form, LSL #0 implicit, no flag set)
    if (w & 0x0FFF_0FF0) == 0x01A0_0000 { return true; }
    // LDR Rd, [pc, #imm] (literal pool; very common in Newton kernel)
    if (w & 0x0FFF_F000) == 0x059F_0000 { return true; }
    // MRC/MCR p15: bits[27:24]=0xE, [11:8]=0xF (coproc 15), bit 4=1,
    //   bit 20 = 1 (MRC) or 0 (MCR). Match both.
    if (w & 0x0FE0_0F10) == 0x0E00_0F10 { return true; }
    // B <label> with cond=AL
    if (w & 0x0F00_0000) == 0x0A00_0000 { return true; }

    false
}

fn read_u32_le(slice: &[u8], i: usize) -> u32 {
    let o = i * 4;
    u32::from_le_bytes([slice[o], slice[o + 1], slice[o + 2], slice[o + 3]])
}

fn fn_addr(i: usize) -> u32 { read_u32_le(FN_ADDRS_RAW, i) }
fn fn_name_off(i: usize) -> usize { read_u32_le(FN_NAME_OFFS_RAW, i) as usize }

fn fn_name(i: usize) -> &'static str {
    let start = fn_name_off(i);
    // Scan to NUL in the pool; entries are NUL-terminated.
    let mut end = start;
    while end < NAME_POOL.len() && NAME_POOL[end] != 0 {
        end += 1;
    }
    core::str::from_utf8(&NAME_POOL[start..end]).unwrap_or("<non-utf8>")
}

/// Ranges that are not safe to replace with a trace UDF:
///   - VA 0x00..0x20 is the ARM vector table. Patching the Reset
///     entry at 0x00 UDFs on the first fetch before the guest has
///     valid banked state; patching 0x04/0x10 collides with the
///     hypervisor's own UND-trampoline and DABT-intercept patches.
///   - VA 0x00FFFF00..0x00FFFF34 holds the UND-trampoline body
///     installed by `patch_und_vector` in guest_mem.rs. Any symbol
///     coincidentally landing in there would replace the code that
///     delivers the trap we're trying to see.
fn in_reserved_range(addr: u32) -> bool {
    if addr < 0x0000_0020 { return true; }
    if (0x00FF_FF00..0x00FF_FF34).contains(&addr) { return true; }
    false
}

/// True once `enable_patches()` has installed the trace UDFs.
static mut PATCHES_ENABLED: bool = false;

/// Called from `load_newton_rom` as the last load-time step. Does not
/// touch the ROM — the actual patching is deferred to
/// `enable_patches()` which runs when the guest turns on its stage-1
/// MMU. Rationale: the UND trampoline's save slot at VA 0x0C00_4F00
/// only translates to the correct RAM IPA via the guest's own L1
/// table, so pre-MMU trace UDFs would lose LR_und and land us at a
/// bogus PC.
pub fn init() {
    kprintln!(
        "trace: deferred patching of {} candidate entries until guest stage-1 MMU is on",
        FN_COUNT
    );
}

/// Install the trace UDFs. Invoked from the CP15 SCTLR write path in
/// `trap.rs` at the M=0 → M=1 rising edge. Idempotent — subsequent
/// calls are no-ops.
///
/// Safety: must run on core 0 with exclusive access to GUEST_ROM while
/// the guest is paused at the SCTLR-write trap. Stage-2 RO only
/// governs *guest* writes; our host-side writes to the ROM backing
/// are always permitted.
pub unsafe fn enable_patches() {
    // SAFETY: single-threaded.
    let was = unsafe {
        let v = PATCHES_ENABLED;
        PATCHES_ENABLED = true;
        v
    };
    if was { return; }

    let rom_base = guest_mem::rom_host_pa() as *mut u32;
    let n = FN_COUNT.min(MAX_INDEX);
    if FN_COUNT > MAX_INDEX {
        kprintln!(
            "trace: symbol table has {} entries, truncating to UDF imm16 cap {}",
            FN_COUNT, MAX_INDEX
        );
    }

    let mut patched = 0usize;
    let mut skipped_reserved = 0usize;
    let mut skipped_shape = 0usize;

    for i in 0..n {
        let addr = fn_addr(i);
        if in_reserved_range(addr) { skipped_reserved += 1; continue; }
        let word_index = (addr / 4) as usize;
        // SAFETY: addr filtered in build.rs to < ROM_SIZE and word-aligned.
        let orig = unsafe { rom_base.add(word_index).read() };
        if !is_known_function_start(orig) {
            skipped_shape += 1;
            continue;
        }
        // SAFETY: single-threaded static access.
        unsafe { ORIG_INSN[i] = orig; }
        let patched_word = encode_udf(i as u16);
        // SAFETY: same bounds as the read.
        unsafe { rom_base.add(word_index).write(patched_word); }
        patched += 1;
    }

    // Publish the stores and flush the entire guest icache so the
    // next fetch from any patched site sees the UDF.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }

    kprintln!(
        "trace: patched {} / {} entries; {} skipped (reserved range), {} skipped (non-function shape)",
        patched, n, skipped_reserved, skipped_shape
    );
}

/// Handle a trap from a UDF injected by `init()`. Returns true if the
/// opcode belongs to the tracer; false otherwise so the caller falls
/// through to the unknown-UND halt path. `faulting_pc` is the guest PA
/// of the faulting instruction; `spsr_und` is the pre-UND CPSR we want
/// to resume with.
pub fn handle_trace_und(
    ctx: &mut TrapContext,
    faulting_pc: u32,
    spsr_und: u64,
    insn: u32,
) -> bool {
    let index = match decode_udf(insn) {
        Some(i) => i as usize,
        None => return false,
    };

    if index >= FN_COUNT {
        kprintln!(
            "trace: UDF index {} out of range ({} entries); PC={:#x}",
            index, FN_COUNT, faulting_pc
        );
        return false;
    }

    let expected_addr = fn_addr(index);
    if expected_addr != faulting_pc {
        kprintln!(
            "trace: UDF index {} expected PC={:#x} but faulted at {:#x}",
            index, expected_addr, faulting_pc
        );
        return false;
    }

    // SAFETY: single-threaded; ORIG_INSN[index] was populated at init
    // time iff we patched the site.
    let orig = unsafe { ORIG_INSN[index] };
    if orig == 0 {
        kprintln!(
            "trace: UDF at {:#x} index {} but ORIG_INSN is zero — un-patched site?",
            faulting_pc, index
        );
        return false;
    }

    // SAFETY: single-threaded.
    let seq = unsafe {
        TRACE_SEQ = TRACE_SEQ.wrapping_add(1);
        TRACE_SEQ
    };

    // SPSR_und preserves the pre-UND CPSR. Bits[4:0] name the mode the
    // caller was running in. The UND trampoline briefly switches to
    // SVC and saves R14_svc at `UND_SAVE_LR_SVC_IPA`; for SVC callers
    // (the common Newton-kernel case) that slot holds the real caller
    // LR. For other modes the slot holds whatever the last SVC R14
    // happened to be, so we label the column with the mode name to
    // make staleness obvious.
    let mode = (spsr_und as u32) & 0x1F;
    let mode_label = match mode {
        0x10 => "usr",
        0x11 => "fiq",
        0x12 => "irq",
        0x13 => "svc",
        0x17 => "abt",
        0x1B => "und",
        0x1F => "sys",
        _    => "???",
    };
    let lr = if mode == 0x13 {
        guest_mem::read_word_pa(UND_SAVE_LR_SVC_IPA).unwrap_or(0)
    } else {
        // Caller-mode LR not reliably reachable without a mode-
        // specific save — leave ctx.x[14] here as the best-effort
        // value. Under QEMU raspi3b it's usually 0 on this path.
        ctx.x[14] as u32
    };

    kprintln!(
        "trace {:5} PC={:#010x} LR={:#010x} ({}) {}",
        seq,
        faulting_pc,
        lr,
        mode_label,
        fn_name(index)
    );

    // Restore the original instruction in the ROM backing. A direct
    // host-side write bypasses stage-2 RO (which only governs guest
    // writes). The guest's icache may still hold the UDF — flush the
    // line so the upcoming ERET re-fetches the restored word.
    let rom_base = guest_mem::rom_host_pa() as *mut u32;
    let word_index = (faulting_pc / 4) as usize;
    // SAFETY: faulting_pc < ROM_SIZE (validated via fn_addr == faulting_pc
    // and build.rs filters to < 0x0100_0000). Single-threaded.
    unsafe {
        rom_base.add(word_index).write(orig);
    }
    let host_va = (rom_base as u64).wrapping_add((word_index as u64) * 4);
    cpu::ic_ivau(host_va);

    // Rewind to re-execute the restored instruction.
    crate::trap::return_to_guest_trace(ctx, faulting_pc as u64, spsr_und);
    true
}
