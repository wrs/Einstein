//! User-driven guest software breakpoints.
//!
//! Works around the QEMU aarch64-gdbstub limitation that drops the
//! AArch32 mode switch: software breakpoints at guest VAs are ignored
//! by the stub, and register inspection during guest execution is
//! garbage. Instead of going through gdb's bp-insertion path we let
//! the host-side hypervisor patch the guest's ROM word with a marker
//! `UDF` instruction, let the guest trap into EL2, and handle the
//! trap in Rust. The user's gdb session sits on a conditional
//! breakpoint at `trap_sync_lower_aarch32` (`bg <addr>` in
//! `scripts/gdb-init`) that fires when the trap's `ELR_EL2` matches
//! the guest PC they care about.
//!
//! Entry points are `#[no_mangle] pub extern "C"` so they can be
//! invoked from gdb via `call install_guest_bp(0x...)` against the
//! remote target. gdb's `bp` / `bp-clear` / `bp-list` helpers in
//! `scripts/gdb-init` wrap these calls.
//!
//! # Scope (v1)
//!
//! - One-shot breakpoints. The UND handler restores the original
//!   instruction before returning to the guest, so each BP fires
//!   exactly once. Reinstall from gdb to re-arm. Re-arming
//!   automatically would need MDCR_EL2/PSTATE.SS single-stepping; it
//!   isn't worth the complexity for interactive debugging.
//! - ROM-range only (`0x00000000..0x01000000`). That range is backed
//!   by `GUEST_ROM` with a stable stage-2 mapping, so the host-side
//!   write always lands in the right page. Guest-RAM-resident code
//!   would need stage-2 awareness and isn't in scope.
//! - Small fixed table (`TABLE_SIZE` slots). Plenty for interactive
//!   work; panicking on overflow would be unhelpful, so `install`
//!   returns an error code instead.
//!
//! # UDF encoding reservation
//!
//! The tracer (`src/tracer.rs`) uses `UDF #imm16` with `imm16` in
//! `0..FN_COUNT` (Newton's table is ~20k entries). We reserve
//! `UDF #0xFF0E` as our marker — safely above any plausible
//! `FN_COUNT`. `handle_und` in `trap.rs` dispatches to us first, so
//! we never reach tracer's code path.
//!
//! # Interaction with snapshots
//!
//! `src/snapshot.rs::maybe_autosave` calls `any_installed()` and
//! skips the autosave while any BP is live — a saved ROM with our
//! marker UDF would halt on resume with "marker at PC=… with no
//! matching table entry". The gating logs a single transition line
//! when it starts/stops suppressing saves. Explicit
//! HVC-#0x20-triggered saves still go through; if you really want a
//! save with BPs installed, you're telling the hypervisor on
//! purpose, and it's your problem to clear them before resume.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::cpu;
use crate::guest_mem;
use crate::kprintln;
use crate::trap::{self, TrapContext};

/// Max number of live breakpoints at once.
pub const TABLE_SIZE: usize = 16;

/// UDF A1 encoding for `UDF #0xFF0E` (verified with
/// `arm-none-eabi-objdump`: `0xE7FFF0FE` → `udf #0xff0e`). Unique
/// enough to distinguish from tracer UDFs (which use `imm16 < FN_COUNT`).
pub const BP_UDF_INSN: u32 = 0xE7FF_F0FE;

/// Top of the ROM stage-2 window; `install` rejects higher addresses.
const ROM_LIMIT: u32 = guest_mem::ROM_SIZE as u32;

/// One entry in the breakpoint table. `ipa == 0` means "empty" (PC 0 is
/// the reset vector — the hypervisor itself jumps here at boot and we
/// never want a BP at 0 anyway, so it's a safe sentinel).
#[derive(Clone, Copy)]
struct Slot {
    ipa: u32,
    orig: u32,
}

/// The active breakpoint table.
///
/// Accessed only from EL2 on core 0 (handle_und path) and from gdb's
/// `call`, which happens with the guest paused at an EL2 stop. No
/// concurrent access by construction; the `AtomicU32` lock below is
/// strictly defensive against a future multicore EL2.
static mut TABLE: [Slot; TABLE_SIZE] = [Slot { ipa: 0, orig: 0 }; TABLE_SIZE];

/// Simple spin-lock around TABLE. 0 = unlocked, 1 = locked.
static LOCK: AtomicU32 = AtomicU32::new(0);

fn lock() {
    while LOCK.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
}
fn unlock() {
    LOCK.store(0, Ordering::Release);
}

/// Return codes shared with gdb. Any negative value is an error.
const OK: i32 = 0;
const ERR_OUT_OF_RANGE: i32 = -1;
const ERR_NOT_ALIGNED: i32 = -2;
const ERR_TABLE_FULL: i32 = -3;
const ERR_ALREADY_PRESENT: i32 = -4;
const ERR_NOT_FOUND: i32 = -5;

/// `#[used]` anchor that pins the gdb-callable entry points so LTO
/// doesn't strip them. The hypervisor itself never references these
/// fns — they're only reached via `call install_guest_bp(...)` from
/// gdb against the remote target.
#[used]
static GUEST_BP_FORCE_KEEP: (
    extern "C" fn(u32) -> i32,
    extern "C" fn(u32) -> i32,
    extern "C" fn(),
    extern "C" fn(u32, &crate::trap::TrapContext),
) = (install_guest_bp, remove_guest_bp, list_guest_bps, bp_hit_anchor);

/// Stable, gdb-friendly stop point for user-installed guest BPs. Called
/// from `handle_user_bp_und` immediately after the slot lookup confirms
/// a real BP hit (i.e. matched a slot in TABLE), and *before* any
/// special-case PC dispatch.
///
/// AAPCS64 layout at entry: `faulting_pc` in `w0`, `ctx` pointer in
/// `x1`. The gdb-init `bp` command filters on `$x0 == <addr>` so the
/// condition stays cheap and DWARF-independent. Carrying `ctx` here
/// (rather than letting the user `up` through `handle_user_bp_und`,
/// where `ctx` is "optimized out" because the function is large enough
/// for LLVM to elide its frame DWARF) makes `ctt` work at the
/// bp-stop frame itself (after a single `up` past the inlined
/// black_box body).
///
/// `#[inline(never)]` plus the GUEST_BP_FORCE_KEEP `#[used]` tuple
/// keep the symbol resolvable. Both args are passed through
/// `core::hint::black_box` so LTO can't drop either; without that, an
/// unused-arg might be missing from DWARF at the frame.
#[no_mangle]
#[inline(never)]
pub extern "C" fn bp_hit_anchor(faulting_pc: u32, ctx: &crate::trap::TrapContext) {
    // SAFETY: empty body — the call/return is the entire purpose.
    core::hint::black_box(faulting_pc);
    core::hint::black_box(ctx);
}

/// Install a one-shot guest breakpoint at ROM IPA `ipa`.
///
/// Returns:
/// - `0..TABLE_SIZE` — installed at that slot index
/// - `ERR_OUT_OF_RANGE` — ipa outside the 16 MiB ROM window
/// - `ERR_NOT_ALIGNED` — ipa not 4-byte aligned
/// - `ERR_TABLE_FULL` — every slot is in use; call `remove_guest_bp` first
/// - `ERR_ALREADY_PRESENT` — a BP already exists at this ipa
///
/// Safety: gdb invokes this with the guest paused at an EL2 stop, so
/// TABLE is quiescent and GUEST_ROM is exclusive.
#[no_mangle]
pub extern "C" fn install_guest_bp(ipa: u32) -> i32 {
    if ipa >= ROM_LIMIT { return ERR_OUT_OF_RANGE; }
    if (ipa & 0x3) != 0 { return ERR_NOT_ALIGNED; }
    if ipa == 0 { return ERR_OUT_OF_RANGE; }

    lock();
    // SAFETY: LOCK gives exclusive access.
    let r = unsafe { install_locked(ipa) };
    unlock();
    r
}

/// Remove a previously installed BP. Returns `OK` or `ERR_NOT_FOUND`.
#[no_mangle]
pub extern "C" fn remove_guest_bp(ipa: u32) -> i32 {
    lock();
    // SAFETY: LOCK gives exclusive access.
    let r = unsafe { remove_locked(ipa) };
    unlock();
    r
}

/// True iff at least one breakpoint is currently installed. Used by
/// `snapshot::maybe_autosave` to avoid persisting a ROM image that
/// contains our marker UDF — see the module-level note on snapshot
/// interaction.
#[cfg_attr(feature = "no-semihost", allow(dead_code))]
pub fn any_installed() -> bool {
    lock();
    // SAFETY: LOCK gives exclusive access.
    let any = unsafe {
        let table = &*core::ptr::addr_of!(TABLE);
        table.iter().any(|s| s.ipa != 0)
    };
    unlock();
    any
}

/// Print the live BP table to the hypervisor console. Handy when gdb
/// and the hypervisor log both have the user's attention.
#[no_mangle]
pub extern "C" fn list_guest_bps() {
    lock();
    // SAFETY: LOCK gives exclusive access.
    let table = unsafe { core::ptr::addr_of!(TABLE).read() };
    unlock();
    kprintln!("guest_bp: active breakpoints");
    let mut any = false;
    for (i, s) in table.iter().enumerate() {
        if s.ipa != 0 {
            kprintln!("  slot {}: ipa={:#010x} orig={:#010x}", i, s.ipa, s.orig);
            any = true;
        }
    }
    if !any { kprintln!("  (none)"); }
}

// ---- internals ---------------------------------------------------------

unsafe fn install_locked(ipa: u32) -> i32 {
    // SAFETY: caller holds LOCK; single-threaded otherwise.
    let table = unsafe { &mut *core::ptr::addr_of_mut!(TABLE) };

    // Duplicate check first.
    if table.iter().any(|s| s.ipa == ipa) {
        return ERR_ALREADY_PRESENT;
    }
    let slot_idx = match table.iter().position(|s| s.ipa == 0) {
        Some(i) => i,
        None => return ERR_TABLE_FULL,
    };

    let rom_base = guest_mem::rom_host_pa() as *mut u32;
    let word_index = (ipa / 4) as usize;
    // SAFETY: ipa < ROM_LIMIT and 4-byte aligned, so word_index is in
    // bounds of the 16 MiB GUEST_ROM backing store.
    let orig = unsafe { rom_base.add(word_index).read() };

    // Refuse to patch something that's already our marker — means a
    // prior install leaked, or the user is double-installing under a
    // race. Either way, don't clobber the stale slot's orig.
    if orig == BP_UDF_INSN {
        return ERR_ALREADY_PRESENT;
    }

    // SAFETY: same bounds as the read; host-side writes bypass stage-2
    // RO, which only constrains guest writes.
    unsafe { rom_base.add(word_index).write(BP_UDF_INSN); }
    let host_va = (rom_base as u64).wrapping_add((word_index as u64) * 4);
    // SAFETY: cache ops are always safe; we want the guest fetch path
    // to see the new word on the next execution.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            options(nostack, preserves_flags),
        );
    }
    cpu::ic_ivau(host_va);
    // SAFETY: cache ops, publish stores.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }

    table[slot_idx] = Slot { ipa, orig };
    // Suppress the per-install kprintln for the SearchFreeList re-arm
    // path — handle_user_bp_und re-installs on every benign walk and
    // would otherwise saturate the log. Genuine bp installations from
    // gdb/main land here only once and stay loud.
    if ipa != 0x0031_3308 {
        kprintln!(
            "guest_bp: installed at {:#010x} (slot {}, orig={:#010x})",
            ipa, slot_idx, orig
        );
    }
    slot_idx as i32
}

unsafe fn remove_locked(ipa: u32) -> i32 {
    // SAFETY: caller holds LOCK.
    let table = unsafe { &mut *core::ptr::addr_of_mut!(TABLE) };
    let slot_idx = match table.iter().position(|s| s.ipa == ipa) {
        Some(i) => i,
        None => return ERR_NOT_FOUND,
    };
    let orig = table[slot_idx].orig;
    restore_word(ipa, orig);
    table[slot_idx] = Slot { ipa: 0, orig: 0 };
    kprintln!("guest_bp: removed bp at {:#010x} (slot {})", ipa, slot_idx);
    OK
}

/// Restore the original word at ipa and invalidate icache line. Does
/// NOT touch the table — the caller decides whether to clear the slot.
fn restore_word(ipa: u32, orig: u32) {
    let rom_base = guest_mem::rom_host_pa() as *mut u32;
    let word_index = (ipa / 4) as usize;
    // SAFETY: bounds guaranteed by install-time check (ipa < ROM_LIMIT,
    // aligned). Single-threaded access via LOCK or the UND handler.
    unsafe { rom_base.add(word_index).write(orig); }
    let host_va = (rom_base as u64).wrapping_add((word_index as u64) * 4);
    // SAFETY: cache op.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            options(nostack, preserves_flags),
        );
    }
    cpu::ic_ivau(host_va);
    // SAFETY: cache op.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

/// UND handler entry point. Called from `handle_und` in `trap.rs`.
/// Returns `true` if we recognised the marker and handled the trap
/// (restored the word + issued `return_to_guest`), `false` otherwise so
/// the caller can fall through to the tracer / unknown paths.
pub fn handle_user_bp_und(
    ctx: &mut TrapContext,
    faulting_pc: u32,
    spsr_und: u64,
    insn: u32,
) -> bool {
    if insn != BP_UDF_INSN { return false; }

    lock();
    // SAFETY: LOCK gives exclusive access.
    let slot = unsafe {
        let table = &mut *core::ptr::addr_of_mut!(TABLE);
        let idx = table.iter().position(|s| s.ipa == faulting_pc);
        idx.map(|i| {
            let s = table[i];
            table[i] = Slot { ipa: 0, orig: 0 };
            (i, s)
        })
    };
    unlock();

    let (slot_idx, s) = match slot {
        Some(x) => x,
        None => {
            kprintln!(
                "*** guest_bp: marker UDF at PC={:#x} but no matching slot",
                faulting_pc
            );
            return false;
        }
    };

    // Stable gdb stop point. Fires for every legitimate BP hit,
    // before any special-case PC dispatch. The gdb-init `bp` command
    // sets a conditional breakpoint here filtered on `faulting_pc`.
    // ctx is forwarded so `ctt` works directly at the bp-stop frame.
    bp_hit_anchor(faulting_pc, ctx);

    kprintln!(
        "guest_bp: HIT at {:#010x} (slot {}) — restored, one-shot consumed",
        faulting_pc, slot_idx
    );

    // Dump-and-continue path: print ctx.x[0..12] (= R0..R12 for non-
    // FIQ source modes per Table D1-79) plus the source mode's banked
    // R13/R14 looked up by mode bits, plus the saved CPSR. Then
    // restore the original ROM word and ERET so the breakpointed
    // instruction runs at native speed.
    //
    // For HVC #DIAG_TAG-style banked-reg dumps including all modes,
    // patch the relevant guest vector to HVC #DIAG_TAG instead and
    // let `handle_diag` halt with the full picture.
    let cpsr = spsr_und as u32;
    kprintln!(
        "  bp detail pc={:#010x} cpsr={:#010x} mode={:#x}",
        faulting_pc, cpsr, cpsr & 0x1F
    );
    for i in 0..13 {
        kprintln!("    r{:<2} = {:#010x}", i, ctx.x[i] as u32);
    }
    kprintln!(
        "    r13 = {:#010x}  (SP of mode {:#x} via Table D1-79)",
        crate::banked::sp_for_mode(ctx, cpsr), cpsr & 0x1F
    );
    kprintln!(
        "    r14 = {:#010x}  (LR of mode {:#x} via Table D1-79)",
        crate::banked::lr_for_mode(ctx, cpsr), cpsr & 0x1F
    );
    kprintln!("    r15 = {:#010x}  (= pc at bp)", faulting_pc);

    restore_word(faulting_pc, s.orig);

    // Rewind ELR so the restored instruction re-executes at native
    // speed. Shares the UND-path return logic with the tracer.
    trap::return_to_guest_from_und(ctx, faulting_pc as u64, spsr_und);
    true
}
