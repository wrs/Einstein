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
    // B <label> with cond=AL. This is broad (0xEA00_0000..0xEAFF_FFFF),
    // but the allowlist only runs at addresses already vouched for by
    // the symbol table, so "function entry that begins with a raw B"
    // is a thunk / tail-call stub we want to trace. The rare false
    // positive (data word at a symbol-table function address whose
    // top byte happens to be 0xEA) is absorbed by the one-shot UDF
    // restore: the first UDF fire restores the original word and the
    // site is never re-patched.
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
    // UND-trampoline body (see guest_mem::patch_und_vector).
    if (0x00FF_FF00..0x00FF_FF34).contains(&addr) { return true; }
    // DebugStr / Debugger 2-word stubs (see
    // rom_patches::apply_debug_patches).
    if (0x00FF_FF30..0x00FF_FF40).contains(&addr) { return true; }
    // FTimeInSeconds injection stub (5 words at 0x00FF_FF40, see
    // rom_patches::apply_ftime_in_seconds_patch).
    if (0x00FF_FF40..0x00FF_FF54).contains(&addr) { return true; }
    // FDateFromSeconds injection stub (5 words at 0x00FF_FF60).
    if (0x00FF_FF60..0x00FF_FF74).contains(&addr) { return true; }
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
    // Phase B diagnostic: eagerly patch a few REx-scanner entries so that
    // pre-MMU calls (which happen before `enable_patches` fires) still
    // trace. Must run at ROM load time before stage-2 is enabled.
    // SAFETY: single-threaded at boot.
    unsafe { early_patch_for_rex_scanner(); }
}

/// Early-patch a handful of REx-related function entries with UDF so we
/// can see them called pre-MMU. Runs once at load time; `enable_patches`
/// later sees these sites as already-UDF'd and skips them.
unsafe fn early_patch_for_rex_scanner() {
    let targets: [u32; 3] = [0x003137dc, 0x00313818, 0x00313888];
    let rom_base = guest_mem::rom_host_pa() as *mut u32;
    for addr in targets {
        // Find the index in the tracer's FN_ADDRS table.
        let mut idx_opt: Option<usize> = None;
        for i in 0..FN_COUNT {
            if fn_addr(i) == addr {
                idx_opt = Some(i);
                break;
            }
        }
        let Some(idx) = idx_opt else {
            kprintln!("trace: early-patch: {:#x} not in symbol table, skipping", addr);
            continue;
        };
        let word_idx = (addr / 4) as usize;
        // SAFETY: addr is < ROM_SIZE.
        let orig = unsafe { rom_base.add(word_idx).read() };
        // SAFETY: single-threaded at boot.
        unsafe { ORIG_INSN[idx] = orig; }
        let udf = encode_udf(idx as u16);
        // SAFETY: writing to ROM backing (host RAM).
        unsafe { rom_base.add(word_idx).write(udf); }
        kprintln!(
            "trace: early-patch: {:#x} idx={} orig={:#010x} → UDF",
            addr, idx, orig
        );
    }
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

    // Phase B diagnostic: for REx-scanner functions, also dump r0-r4 to
    // see the arguments. These are early-patched so trace fires pre-MMU.
    if matches!(faulting_pc, 0x003137dc | 0x00313818 | 0x00313888) {
        kprintln!(
            "  args: r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} r4={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32, ctx.x[4] as u32
        );
    }


    // Phase B diagnostic: at SearchForFlashDrivers entry, dump the REx
    // base table the kernel uses to index REx blocks. Per our disasm of
    // PrimNextRExConfigEntry (0x11ee60), REx[id] lives at
    //   gGlobalsThatLiveAcrossReboot (VA 0x0c1061c4) + 0x2e8 + id*4
    //   = VA 0x0c1064ac + id*4
    // Also dump ctx.x[0] (r0, i.e. `this` pointer for this TNewInternalFlash
    // method) and the flash-driver registry fields it reads from [r0+*].
    if faulting_pc == 0x0013b908 {
        dump_rex_state(ctx);
    }

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
    crate::trap::return_to_guest_from_und(ctx, faulting_pc as u64, spsr_und);
    true
}

/// Phase B diagnostic: dump the REx state the kernel sees at the moment
/// SearchForFlashDrivers is entered. Purpose: find out if our external
/// REx at PA 0x00800000 actually made it into the kernel's REx base
/// table, and what entries the kernel sees there.
fn dump_rex_state(ctx: &TrapContext) {
    kprintln!("  === REx diagnostic at SearchForFlashDrivers entry ===");
    kprintln!(
        "  r0 (this) = {:#010x}  r1 = {:#010x}  r2 = {:#010x}  r3 = {:#010x}",
        ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32,
    );

    // REx base table: VA 0x0c1064ac + id*4 for id in 0..4.
    for id in 0u32..4 {
        let va = 0x0c1064ac + id * 4;
        let pa = crate::trap::guest_tl_translate(va);
        let val = pa.and_then(guest_mem::read_word_pa);
        kprintln!(
            "  REx[{}] at VA {:#x} → PA {:?} → {:?}",
            id, va, pa, val
        );
    }

    // Dump 256 bytes around gGlobalsThatLiveAcrossReboot (VA 0x0c1061c4)
    // to see the whole struct including fields around the REx table at +0x2e8.
    for off in (0..0x400_u32).step_by(16) {
        let va = 0x0c1061c4 + off;
        let pa = crate::trap::guest_tl_translate(va);
        match pa {
            Some(p) => {
                let w0 = guest_mem::read_word_pa(p).unwrap_or(0);
                let w1 = guest_mem::read_word_pa(p + 4).unwrap_or(0);
                let w2 = guest_mem::read_word_pa(p + 8).unwrap_or(0);
                let w3 = guest_mem::read_word_pa(p + 12).unwrap_or(0);
                if (w0 | w1 | w2 | w3) != 0 {
                    kprintln!(
                        "  gGlob[+{:#05x}] VA {:#010x}: {:#010x} {:#010x} {:#010x} {:#010x}",
                        off, va, w0, w1, w2, w3
                    );
                }
            }
            None => { break; }
        }
    }

    // Check what's at the REx physical addresses. VAs:
    for (label, va) in [
        ("ROM[0x71FC4C]", 0x0071FC4C_u32),
        ("ROM[0x800000] (external REx)", 0x00800000_u32),
    ] {
        let pa = crate::trap::guest_tl_translate(va);
        let w0 = pa.and_then(guest_mem::read_word_pa);
        let w1 = pa.map(|p| p.wrapping_add(4)).and_then(guest_mem::read_word_pa);
        kprintln!(
            "  {} VA {:#x} → PA {:?}: magic={:?} {:?}",
            label, va, pa, w0, w1
        );
    }

    // Compare two PAs: 0x0400d1c4 (pre-MMU globals per rex-dabt r4) vs
    // 0x0401_01c4 (what our stage-1 walker claims VA 0x0c1061c4 maps to).
    // If these contain the same bytes, either both were written or they alias.
    kprintln!("  --- Pre-MMU globals PA 0x0400d1c4 ---");
    for off in [0u32, 0x4, 0x8, 0x10, 0x18, 0x1c, 0x20, 0x24, 0x28, 0x30, 0x220, 0x228, 0x2e8, 0x2ec, 0x2f0, 0x2f4, 0x2f8, 0x2fc, 0x30c] {
        let pa = 0x0400_d1c4 + off;
        let v = guest_mem::read_word_pa(pa);
        kprintln!("    +{:#05x} PA {:#x} = {:?}", off, pa, v);
    }
    kprintln!("  --- Walker-claimed post-MMU PA 0x0401_01c4 (stage-1 walk of VA 0x0c1061c4) ---");
    for off in [0u32, 0x4, 0x8, 0x220, 0x228, 0x2e8, 0x2ec, 0x2f0, 0x2f4, 0x2f8] {
        let pa = 0x0401_01c4 + off;
        let v = guest_mem::read_word_pa(pa);
        kprintln!("    PA {:#x} = {:?}", pa, v);
    }
    // Also dump the raw stage-1 L1 entry for VA 0x0c100000 to confirm.
    let l1_pa = 0x0400_0000 + 0xC1 * 4;
    let l1 = guest_mem::read_word_pa(l1_pa);
    kprintln!("  guest L1[0xC1] at PA {:#x} = {:?}", l1_pa, l1);
    if let Some(l1_v) = l1 {
        // Decode: ty = bits[1:0]. For coarse (01), L2 base = entry & 0xFFFFFC00.
        let ty = l1_v & 3;
        kprintln!("    type = {} ({})", ty,
            match ty { 0 => "fault", 1 => "coarse", 2 => "section", _ => "fine/super" });
        if ty == 1 {
            let l2_base = l1_v & 0xFFFF_FC00;
            kprintln!("    L2 base PA = {:#x}", l2_base);
            // Dump the full L2 table (256 entries)
            for i in 0..16 {
                let pa = l2_base + (i as u32) * 4;
                let v = guest_mem::read_word_pa(pa);
                kprintln!("    L2[{:#04x}] at PA {:#x} = {:?}", i, pa, v);
            }
            // And the specific L2 entry for VA 0x0c1061c4: l2_idx = 0x6
            let va = 0x0c1061c4_u32;
            let l2_idx = (va >> 12) & 0xFF;
            let l2_entry_pa = l2_base + l2_idx * 4;
            let l2_entry = guest_mem::read_word_pa(l2_entry_pa);
            kprintln!(
                "    L2[{:#x}] for VA {:#x} at PA {:#x} = {:?}",
                l2_idx, va, l2_entry_pa, l2_entry
            );
        } else if ty == 2 {
            let pa = (l1_v & 0xFFF00000) | (0x0c1061c4 & 0x000FFFFF);
            kprintln!("    section → PA {:#x}", pa);
        }
    }
    kprintln!("  === end REx diagnostic ===");
}
