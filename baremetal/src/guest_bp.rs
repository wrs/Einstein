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
//! `UDF #0xFFFE` as our marker — safely above any plausible
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

/// Per-PC log budgets for the `MakeStoreObject` probes. Each probe is a
/// re-armed marker UDF (no ROM mutation per hit), so we can keep them
/// installed cheaply for the whole boot. We still cap the kprintln
/// volume at the early hits — by the time newt diverges from Einstein,
/// the first ~30 calls per probe are enough to localise the issue.
static NEWSTACK_EXIT_HITS: AtomicU32 = AtomicU32::new(0);
static SETCURHEAP_HITS:    AtomicU32 = AtomicU32::new(0);
static NEWHEAP_HITS:       AtomicU32 = AtomicU32::new(0);
const PROBE_LOG_LIMIT: u32 = 32;

/// Max number of live breakpoints at once.
pub const TABLE_SIZE: usize = 16;

/// UDF A1 encoding for `UDF #0xFFFE`. Unique enough to distinguish
/// from tracer UDFs (which use `imm16 < FN_COUNT`).
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
) = (install_guest_bp, remove_guest_bp, list_guest_bps);

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

    // `NewHeap` entry probe at ROM 0x00310e24
    // (`mov ip, sp` — 0xe1a0c00d). r0 holds the new heap's RAM base
    // (the function returns r7 = r0 + 16 to the caller). If we ever
    // see r0 = 0x0ca6b000, that's the moment the bogus RelocHeap got
    // created in a real allocation pass.
    if faulting_pc == 0x0031_0e24 {
        let cpsr = spsr_und as u32;
        let r0 = ctx.x[0] as u32;
        let n = NEWHEAP_HITS.fetch_add(1, Ordering::Relaxed);
        if n < PROBE_LOG_LIMIT || r0 == 0x0ca6_b000 {
            let lr_idx = crate::banked::lr_slot_for_mode(cpsr);
            kprintln!(
                "probe: NewHeap#{} mode={:#x} r0(base)={:#010x} r1(size)={:#010x} r2={:#010x} lr={:#010x}",
                n, cpsr & 0x1F,
                r0, ctx.x[1] as u32, ctx.x[2] as u32,
                ctx.x[lr_idx] as u32,
            );
        }
        // Original insn = `mov ip, sp` (= ARM r12 := r13). ip is X12 in
        // banked context; sp is the source-mode SP per Table D1-79.
        let sp = crate::banked::sp_for_mode(ctx, cpsr);
        ctx.x[12] = sp as u64;
        lock();
        // SAFETY: re-occupy slot.
        unsafe {
            let table = &mut *core::ptr::addr_of_mut!(TABLE);
            table[slot_idx] = Slot { ipa: faulting_pc, orig: s.orig };
        }
        unlock();
        trap::return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return true;
    }

    // `__ct__9TRefStackFv` post-NewStack probe at ROM 0x001a4948
    // (`add sp, sp, #4`). Logs the NewStack return register r0 and the
    // mode SP / LR to localise where the bogus heap pointer enters
    // newt's globals. Emulates the original instruction so the marker
    // UDF stays armed for every subsequent invocation.
    if faulting_pc == 0x001a_4948 {
        let cpsr = spsr_und as u32;
        let n = NEWSTACK_EXIT_HITS.fetch_add(1, Ordering::Relaxed);
        if n < PROBE_LOG_LIMIT {
            let sp_idx = crate::banked::sp_slot_for_mode(cpsr);
            let lr_idx = crate::banked::lr_slot_for_mode(cpsr);
            kprintln!(
                "probe: TRefStack-NewStack-exit#{} mode={:#x} r0={:#010x} r4={:#010x} sp={:#010x} lr={:#010x}",
                n, cpsr & 0x1F,
                ctx.x[0] as u32, ctx.x[4] as u32,
                ctx.x[sp_idx] as u32, ctx.x[lr_idx] as u32,
            );
        }
        // Emulate `add sp, sp, #4` against the source mode's SP.
        let sp_idx = crate::banked::sp_slot_for_mode(cpsr);
        ctx.x[sp_idx] = (ctx.x[sp_idx] as u32).wrapping_add(4) as u64;
        lock();
        // SAFETY: LOCK gives exclusive access; re-occupy the slot the
        // dispatcher freed at the top of this fn.
        unsafe {
            let table = &mut *core::ptr::addr_of_mut!(TABLE);
            table[slot_idx] = Slot { ipa: faulting_pc, orig: s.orig };
        }
        unlock();
        trap::return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return true;
    }

    // `SetCurrentHeap` entry probe at ROM 0x00142df0
    // (`ldr r1, [pc, #40]`). r0 holds the heap pointer being installed
    // into the current task's globals[-16]. The wedge happens because
    // GetCurrentHeap returns 0x0ca6b010 — which is NOT a real heap. If
    // SetCurrentHeap is ever called with that pointer, it's the source.
    if faulting_pc == 0x0014_2df0 {
        let cpsr = spsr_und as u32;
        let r0 = ctx.x[0] as u32;
        let n = SETCURHEAP_HITS.fetch_add(1, Ordering::Relaxed);
        // Always log when r0 matches the wedge's bogus heap, regardless
        // of the cap — the whole point of this probe is to find that
        // exact moment.
        if n < PROBE_LOG_LIMIT || r0 == 0x0ca6_b010 {
            let lr_idx = crate::banked::lr_slot_for_mode(cpsr);
            kprintln!(
                "probe: SetCurrentHeap#{} mode={:#x} r0(heap)={:#010x} lr={:#010x}",
                n, cpsr & 0x1F,
                r0, ctx.x[lr_idx] as u32,
            );
        }
        // First time we see SetCurrentHeap(0x0ca6b010), arm the
        // stage-2 RO carve-out on the page backing that heap header.
        // Idempotent — `arm_carve_out_at_heap_va` returns None if
        // already armed.
        if r0 == 0x0ca6_b010 {
            let _ = crate::heap_watch::arm_carve_out_at_heap_va(0x0ca6_b000);
        }
        // Emulate `ldr r1, [pc, #40]` — PC at execution = pc+8, so the
        // word loaded is at faulting_pc + 8 + 40. The ROM literal at
        // 0x142e20 is `0x0c10102c` (a g-pointer constant).
        let lit_addr = faulting_pc.wrapping_add(48);
        let lit = guest_mem::read_word_va(lit_addr).unwrap_or(0);
        ctx.x[1] = lit as u64;
        lock();
        // SAFETY: same as above — re-occupy the slot.
        unsafe {
            let table = &mut *core::ptr::addr_of_mut!(TABLE);
            table[slot_idx] = Slot { ipa: faulting_pc, orig: s.orig };
        }
        unlock();
        trap::return_to_guest_from_und(ctx, (faulting_pc + 4) as u64, spsr_und);
        return true;
    }

    // SearchFreeList `ldr r3, [r0]` (ROM 0x00313308) fast path. Emulate
    // the load in EL2 instead of restoring the ROM word — that way the
    // marker UDF stays in place and every subsequent walk re-enters this
    // arm without us paying for restore-word + install_guest_bp ROM
    // churn. Re-occupy the slot manually since the dispatcher above
    // released it.
    if faulting_pc == 0x0031_3308 {
        let r0 = ctx.x[0] as u32;
        match crate::guest_mem::read_word_va(r0) {
            Some(value) => {
                ctx.x[3] = value as u64;
                lock();
                // SAFETY: LOCK gives exclusive access; we're putting back
                // the slot we just freed at the top of the dispatcher.
                unsafe {
                    let table = &mut *core::ptr::addr_of_mut!(TABLE);
                    table[slot_idx] = Slot { ipa: faulting_pc, orig: s.orig };
                }
                unlock();
                trap::return_to_guest_from_und(
                    ctx, (faulting_pc + 4) as u64, spsr_und,
                );
                return true;
            }
            None => {
                kprintln!(
                    "*** SearchFreeList wild r0={:#010x} (stage-1 translate failed) ***",
                    r0
                );
                // Fall through to the dump-and-halt path below.
            }
        }
    }

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

    // If we're at the LDRB-post hook, also dump the word the LDRB was
    // targeting so we can see the raw bytes in memory. Address is
    // r8 + 12..15 (word containing [r8+13]).
    if faulting_pc == 0x0011_D844 {
        let r8 = ctx.x[8] as u32;
        let word_addr = r8.wrapping_add(12);
        let w = crate::guest_mem::read_word_va(word_addr).unwrap_or(0xDEADBEEF);
        kprintln!(
            "    mem @[r8+12]={:#010x} word={:#010x}  bytes=[{:02x},{:02x},{:02x},{:02x}]",
            word_addr, w,
            w as u8, (w >> 8) as u8, (w >> 16) as u8, (w >> 24) as u8
        );
    }
    // If we're at PrimGetEnvDomainName's exit (right after both STRBs),
    // dump the two target bytes so we can see if the writes landed.
    if faulting_pc == 0x0011_D308 || faulting_pc == 0x0011_D328 || faulting_pc == 0x0011_D34C {
        let r3 = ctx.x[3] as u32;
        let r5 = ctx.x[5] as u32;
        kprintln!("    PRIM exit: r3={:#010x} r5={:#010x}", r3, r5);
        for (label, addr) in [("r3-word-aligned", r3 & !3), ("r5-word-aligned", r5 & !3)] {
            let w = crate::guest_mem::read_word_va(addr).unwrap_or(0xDEADBEEF);
            kprintln!(
                "    {} @{:#010x} word={:#010x}  bytes=[{:02x},{:02x},{:02x},{:02x}]",
                label, addr, w,
                w as u8, (w >> 8) as u8, (w >> 16) as u8, (w >> 24) as u8
            );
        }
    }
    if faulting_pc == 0x0011_D29C {
        // Inside PrimGetEnvDomainName, right after `ldr r4, [r7, #16]`
        // loaded list1_ptr into r4. Dump r4 + r7 + env-name match check.
        let r4 = ctx.x[4] as u32;
        let r7 = ctx.x[7] as u32;
        kprintln!("    PRIM @list1_load: r4=list1_ptr={:#010x} r7=entry_base={:#010x}", r4, r7);
        if r4 != 0 {
            let first = crate::guest_mem::read_word_va(r4).unwrap_or(0xDEADBEEF);
            kprintln!("    *list1 = {:#010x}", first);
        }
        let entry_env = crate::guest_mem::read_word_va(r7.wrapping_sub(16))
            .unwrap_or(0xDEADBEEF);
        kprintln!("    entry[0] (env_name) = {:#010x}", entry_env);
    }

    // Halt after our key observation to avoid waiting out the timeout.
    if faulting_pc == 0x0011_D844 {
        kprintln!("    (halting after USR LDRB-post dump — diagnostic scaffolding)");
        crate::cpu::halt();
    }

    // SearchFreeList `ldr r3, [r0]` (ROM 0x00313308) — only reached
    // here when the early dispatcher above failed to translate r0 (i.e.
    // it would fault). Dump heap context + freelist chain and halt.
    if faulting_pc == 0x0031_3308 {
        let r0 = ctx.x[0] as u32;
        let r1 = ctx.x[1] as u32;
        {
            kprintln!(
                "    *** SearchFreeList wild r0 ptr — heap={:#010x} corrupt-next={:#010x} ***",
                r1, r0,
            );
            // Walk heap @ r1: dump the first 128 bytes of the heap header
            // so we can see [+28]=size, [+32]=start, [+72]=saved freelist
            // position, [+92]=flags.
            kprintln!("      heap[{:#010x}] header (128 bytes):", r1);
            for off in (0..128u32).step_by(16) {
                let mut row = [0u32; 4];
                for i in 0..4u32 {
                    row[i as usize] = crate::guest_mem::read_word_va(r1.wrapping_add(off + i * 4))
                        .unwrap_or(0xDEADBEEF);
                }
                kprintln!(
                    "        +{:#04x}  {:#010x} {:#010x} {:#010x} {:#010x}",
                    off, row[0], row[1], row[2], row[3]
                );
            }
            // Walk freelist starting at heap[+72]: ptr to first node, then
            // node->next chain (offset +4 from each node) until we either
            // wrap to heap[+32] (start) or hit the corrupt next.
            let mut p = crate::guest_mem::read_word_va(r1.wrapping_add(72)).unwrap_or(0);
            let start = crate::guest_mem::read_word_va(r1.wrapping_add(32)).unwrap_or(0);
            kprintln!(
                "      freelist walk: heap[+72]={:#010x} heap[+32]={:#010x}",
                p, start
            );
            for step in 0..32u32 {
                let size = crate::guest_mem::read_word_va(p).unwrap_or(0xDEADBEEF);
                let next = crate::guest_mem::read_word_va(p.wrapping_add(4)).unwrap_or(0xDEADBEEF);
                kprintln!(
                    "        node[{:>2}] @{:#010x} size={:#010x} next={:#010x}",
                    step, p, size, next
                );
                if next == r0 {
                    kprintln!("        ↑ this node holds the corrupt next ptr");
                    break;
                }
                if next == 0 || next == start {
                    break;
                }
                p = next;
            }
            kprintln!("    (halting after SearchFreeList wild-r0 dump — diagnostic scaffolding)");
            crate::cpu::halt();
        }
    }

    restore_word(faulting_pc, s.orig);

    // Rewind ELR so the restored instruction re-executes at native
    // speed. Shares the UND-path return logic with the tracer.
    trap::return_to_guest_from_und(ctx, faulting_pc as u64, spsr_und);
    true
}
