//! Einstein-equivalent ROM patches applied at load time.
//!
//! Phase A baseline: the 717006 ROM needs a handful of patches to
//! behave sensibly under any emulator / hypervisor. Einstein ships
//! these in `Emulator/JIT/Generic/TJITGenericROMPatch.cpp` and applies
//! them during `TROMImage::CreateImage`. Skipping them during our own
//! ROM load is what left the boot going sideways — most of these are
//! "disable a function that would otherwise hang" or "set a kernel-
//! globals flag that selects a boot path the rest of Einstein is
//! built around".
//!
//! We translate both the *word-write* patches (TJITGenericPatch in
//! Einstein's tree) AND the JIT-specific native-call / injection
//! patches (TJITGenericPatchNativeCall / TJITGenericPatchNativeInjection
//! — `DebugStr`, `Debugger`, `RealClockSeconds`, `FTimeInSeconds`,
//! `FDateFromSeconds`). Einstein's JIT catches its custom SWI opcodes;
//! we don't have a JIT, so we rewrite each target function with
//! equivalent inline ARM code that achieves the same net effect.
//!
//! The virtualized-call patches (`__rt_sdiv`, `__rt_udiv`, `symcmp`)
//! are a performance optimization — Einstein injects host code for
//! these so it doesn't have to JIT them — but on our A53 they run
//! natively just fine. Not implemented because omitting them doesn't
//! change correctness.
//!
//! What the simple patches change (all at main-ROM offsets, applied
//! AFTER byteswap so we write in guest-CPU view):
//!
//! - `0x0000_13F4` ← 1               — `gDebugger` on: ROM takes the
//!   debugger-enabled codepath (selects the driver path we need).
//! - `0x0000_13FC` ← 0x0000_8202     — `gNewtConfig`:
//!   kEnableListener | kDefaultStdioOn | kEnableStdout.
//! - `0x0008_A20C` ← MOV PC, LR      — `Ignore setting time` (the
//!   real ROM would call RTC hardware we don't model).
//! - `0x000D_B0D8`/`0x000D_B0DC`     — BeaconDetect no-op
//!   (MOV R0,#0 ; MOV PC,LR). Einstein disables the geoport beacon
//!   detect loop; on our hypervisor the same loop would spin forever
//!   on a peripheral we don't model.
//! - `0x0014_12F8` ← B +0x24          — Avoid screen calibration.
//! - `0x0030_F088`, `0x0042_0750`, `0x0042_0798`, `0x004D_CA14` —
//!   "Year 2010" time-base constants. Newer time base minutes /
//!   seconds so NewtonOS time arithmetic stays inside the valid range.
//!
//! See `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp` for the
//! full annotated list and the Einstein-side rationale for each.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::kprintln;

// ============================================================================
// Patch-stub arena
// ============================================================================
//
// Each kernel-side native-primitive patch (DebugStr, Debugger,
// FTimeInSeconds, FDateFromSeconds, ResolveFault wrapper, …) needs a
// few words of guest-visible ROM space to hold its replacement-stub
// body, and the kernel-patched BL/B site needs to know that stub's PC
// to encode the redirect. Picking those PCs by hand is exactly how the
// iter-87 wedge happened: FTIME_STUB_PC = 0x00FF_FF40 silently overlapped
// the UND trampoline `patch_und_vector` writes at 0x00FF_FF00..0x00FF_FF60,
// the trampoline ran second, the stub was clobbered, and the kernel's
// patched `b 0x00FF_FF40` at 0x89B80 fell into trampoline code mid-
// instruction-stream. Even after that fix the audit found a second
// latent collision (NEW_STACK_PAD_WRAPPER at 0x00FF_FE80 vs FTIME/FDATE
// at 0x00FF_FE70/0x00FF_FE84).
//
// The arena removes the manual address management entirely. Each
// `apply_*` function calls `alloc_patch_stub(n)` at install time and
// gets back the next free PC; allocations never overlap and arena
// overflow halts loudly. Callers that need to address into their own
// stub (e.g. RESOLVE_FAULT_WRAPPER's bl-site offset) pass that PC
// around as a local instead of consulting a global constant.
//
// The arena lives in the gap between the unused
// LOCK_HEAP_RANGE_WRAPPER region (`0x00FF_FD80`) and the FPA bypass
// stub at `0x00FF_FEC0` that `patch_und_vector` owns. 320 bytes total.
// Currently-installed patches need 152 B; the LOCK/UNLOCK/NEW_STACK_PAD
// wrappers (NOT installed) would add another 80 B, comfortably within
// the budget.
const PATCH_STUB_ARENA_BASE: u32 = 0x00FF_FD80;
const PATCH_STUB_ARENA_END:  u32 = 0x00FF_FEC0;

static PATCH_STUB_ARENA_CURSOR: AtomicU32 = AtomicU32::new(PATCH_STUB_ARENA_BASE);

/// Allocate `n_words` (4 bytes each) inside the patch-stub arena and
/// return the start PC. Halts loudly on overflow so any future stub
/// that pushes past `PATCH_STUB_ARENA_END` fails at install time
/// rather than silently corrupting an adjacent stub.
fn alloc_patch_stub(n_words: usize, name: &'static str) -> u32 {
    let bytes = (n_words * 4) as u32;
    let pc = PATCH_STUB_ARENA_CURSOR.fetch_add(bytes, Ordering::SeqCst);
    let new_end = pc + bytes;
    if new_end > PATCH_STUB_ARENA_END {
        kprintln!(
            "*** patch-stub arena overflow: {} wants {}B at {:#x}; \
             arena end is {:#x}",
            name, bytes, pc, PATCH_STUB_ARENA_END,
        );
        crate::cpu::halt();
    }
    kprintln!(
        "rom_patch: arena alloc {}B for {} -> {:#010x} (cursor now {:#x})",
        bytes, name, pc, new_end,
    );
    pc
}

/// A single word-write patch against the main ROM (IPA 0..0x00800000).
#[derive(Copy, Clone)]
struct RomPatch {
    offset: u32,
    value:  u32,
    name:   &'static str,
}

/// Patches for the 717006 ROM (MP2100 US) — mirrors the `inAddr0`
/// column from every `TJITGenericPatch` in
/// `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp`, restricted
/// to entries that the 717006 ROM id selects (not `kROMPatchVoid`).
///
/// Values are precisely what Einstein writes:
///   - `newTimeBaseMinutes` = 218_799_360 = 0x0D09_5000
///   - `newTimeBaseSeconds` = 3_281_990_400 = 0xC3A5_1800
///   - `gNewtConfig` combines `kEnableListener (0x2)`,
///     `kDefaultStdioOn (0x200)`, `kEnableStdout (0x8000)`.
const PATCHES_717006: &[RomPatch] = &[
    RomPatch { offset: 0x0000_13F4, value: 0x0000_0001, name: "gDebugger patch" },
    RomPatch { offset: 0x0000_13FC, value: 0x0000_8202, name: "gNewtConfig patch" },
    // (Dedup attempt 2026-04-28: even after relocating the hypervisor
    // UND/DABT/SBA trampoline scratch out of the L1[0xc0] self-map
    // region (HYP_TRAMP_SCRATCH_BASE moved to 0x0600_F000),
    // ROM-patching the alternate L2 descriptors (L2[0x2,4,5,6,8] of
    // the L2 PT at PA=0x00001400) STILL wedges boot — verify-mmu
    // aliases drop 15→0 but DABT loops at FAR=0xc004bf8 (subpage 2
    // of PA=0x04005000 via VA=0xc004XXX). So the kernel DOES use the
    // alternate VAs at runtime — just via base+offset indirection
    // rather than direct literals (the literal grep was misleading).
    // Dropping the descriptors unmaps the subpages → DABT loop in
    // BootOS post-HandleDebugCard. Pivot is Option β = stage-2 PA
    // splitting at the duplicate VA. See INVESTIGATION.md for the
    // full diagnostic.)
    RomPatch { offset: 0x0008_A20C, value: 0xE1A0_F00E, name: "Ignore setting time" },
    RomPatch { offset: 0x000D_B0D8, value: 0xE3A0_0000, name: "BeaconDetect (1/2)" },
    RomPatch { offset: 0x000D_B0DC, value: 0xE1A0_F00E, name: "BeaconDetect (2/2)" },
    RomPatch { offset: 0x0014_12F8, value: 0xEA00_0009, name: "Avoid screen calibration" },
    RomPatch { offset: 0x0030_F088, value: 0xC3A5_1800, name: "Time base (4/4)" },
    RomPatch { offset: 0x0042_0750, value: 0x0D09_5000, name: "Time base (1/4)" },
    RomPatch { offset: 0x0042_0798, value: 0x0D09_5000, name: "Time base (2/4)" },
    RomPatch { offset: 0x004D_CA14, value: 0x0D09_5000, name: "Time base (3/4)" },
    // GetClock / SetAlarm 32-bit-wrap detection: replace `addls`
    // (less-or-equal) with `addcc` (strictly-less) so the kernel
    // doesn't treat *equal* successive tick-register reads as a wrap
    // event. The original code is correct on real hardware where
    // CNTPCT-equivalent always strictly advances between two reads,
    // but our `stage2::TICK_PAGE` mapping only refreshes on hypervisor
    // heartbeat, so two guest tick reads inside one ~16 ms heartbeat
    // window observe identical values. The ls/cc swap keeps real
    // wraps detected (new < old) but ignores the spurious equality.
    // See INVESTIGATION.md "alarm-loop wedge from spurious wrap
    // detection". Encoding: cond field [31:28] LS=9 → CC=3; the rest
    // of the instruction (`add Rn, Rn, #1`) is unchanged.
    RomPatch { offset: 0x003A_D430, value: 0x3281_1001, name: "GetClock wrap-detect ls→cc" },
    RomPatch { offset: 0x003A_D46C, value: 0x3282_2001, name: "SetAlarm wrap-detect (1/2) ls→cc" },
    RomPatch { offset: 0x003A_D49C, value: 0x3282_2001, name: "SetAlarm wrap-detect (2/2) ls→cc" },
    // Force every VM heap to allocate / extend in 4-KiB chunks
    // instead of 1-KiB subpages. The kernel's design partitions
    // shared 4-KiB physical pages into 1-KiB subpages with per-
    // subpage AP, enforced by ARMv4's subpage-AP. ARMv7 has no
    // subpage-AP — `fix_stage1_xn_bits` flattens to AP=011, so a
    // stack write to "its" subpage spills into the heap's adjacent
    // subpage on the same physical page. See
    // `INVESTIGATION.md` "Subpage-AP decoded" for the full picture.
    //
    // Surgical fix: make heap[+0x38] (= chunk_size, written by
    // NewHeap from its 3rd arg) always 4096. Then `ExtendVMHeap`
    // grows heaps in whole 4-KiB pages — each heap page is
    // exclusively owned, never shared with another VA.
    //
    // (1) `NewHeap` 0x00310E38 originally `mov r6, r2`
    //     (`0xE1A0_6002`) reads chunk_size from r2 into r6. Replace
    //     with `mov r6, #4096` (`0xE3A0_6A01`) so the heap struct
    //     always records 4 KiB regardless of caller's intent. Per
    //     ARM ARM A8.8.103: MOV (immediate) encoding A1: cond=1110
    //     (e), op=1110_0011_1010 (3a), SBZ=0000, Rd=0110 (6),
    //     immediate12 = 0xa01 (= imm8=0x01, rot=0xa → ROR(0x01, 20)
    //     = 0x1000 = 4096).
    RomPatch { offset: 0x0031_0E38, value: 0xE3A0_6A01, name: "NewHeap: force chunk_size=4096" },
    // (2) `NewVMHeap` 0x001423A0 originally `beq 0x001423C0`
    //     (`0x0A00_0006`) skips the 4-KiB-init path when the
    //     `kFlagAllocateInPages` (bit 30) flag is clear. Replace
    //     with `nop` (= `mov r0, r0`, `0xE1A0_0000`) so the
    //     function always falls through to the 4-KiB rounding +
    //     `r5 = 4096`. Initial chunk size for LockHeapRange ends
    //     up 4 KiB; ExtendVMHeap reads heap[+0x38]=4096 (set by
    //     patch #1) so subsequent extensions are also 4 KiB.
    //
    //     The flag's other side-effect — `addne r1, r1, #4096` at
    //     0x142368 adding slack to the heap-area size — is left
    //     untouched. Most heap allocations are already 4-KiB
    //     aligned; the only exception we've observed is heap #6
    //     (50 KiB), which would round up to 52 KiB. NewHeapArea's
    //     internal alignment likely accommodates this; if not,
    //     we'll need a third patch making the addne unconditional.
    RomPatch { offset: 0x0014_23A0, value: 0xE1A0_0000, name: "NewVMHeap: force 4 KiB init path (nop branch)" },
    // ZapHeap (0x00142844) builds a heap on top of a heap-area produced
    // by GetHeapAreaInfo. When the caller passes a flag byte of 0
    // (the common case), `0x001428B8 moveq r4, #1024` (`0x03A04B01`)
    // sets r4 = 1 KiB, which then drives both the initial
    // SetHeapLimits / LockHeapRange / UnlockHeapRange round-trip
    // (locking only a 1-KiB sliver at the heap base) and the
    // chunk_size argument to the subsequent NewHeap. The NewHeap
    // chunk_size gets force-overridden to 4096 by the patch above,
    // but the initial 1-KiB lock leaves only one of the page's four
    // 1-KiB subpages claimed under ARMv7's flat AP=011 — the other
    // three subpages are nominally unowned and could leak ownership
    // to other allocators. Mirror the existing NewHeap encoding
    // (`mov r6, #4096` = `0xE3A0_6A01`) but for r4: `mov r4, #4096`
    // = `0xE3A0_4A01`. The companion `movne r4, #4096` at
    // `0x001428BC` becomes a no-op (still sets r4 = 4096 when the
    // flag arm is taken — same value our patch produces). See
    // `docs/STRUCTURES.md` "1-KiB allocator audit" for the full
    // catalogue of 1-KiB sites.
    RomPatch { offset: 0x0014_28B8, value: 0xE3A0_4A01, name: "ZapHeap: force chunk/lock size = 4096" },
    // FMNewStack + heap-domain 33→36 KiB + 3→4 KiB guard. Coordinated
    // re-attempt covering the 17 sites in FMNewStack itself (size
    // constants, guard offsets, slot-stride encodings) plus the 3
    // divisor sites in surrounding heap-domain functions:
    //   * `Init__11THeapDomain` at 0x001F_8D74 — divides pool size
    //     by 33 KiB to compute slot count for the slot_info array.
    //   * `GetStackInfo__11THeapDomain` at 0x001F_8E1C — divides VA
    //     offset by 33 KiB to map VA → slot index.
    //   * `FMFree__13TStackManager` at 0x001F_918C — same divisor
    //     in the slot-index path.
    //
    // ARM immediate encoding: #36864 = imm12 0xA09 (rot=0xA, imm8=9
    // → ROR(9,20) = 0x9000); #4096 = imm12 0xA01 (rot=0xA, imm8=1
    // → ROR(1,20) = 0x1000).
    //
    // Sites in CheckHeap / VetHeap (0x0027_1Exx) and SaveCPUStateAndStop
    // (0x0001_8F8C, 0x0001_8FA4, 0x0001_90EC) are NOT patched —
    // CheckHeap may not run in our boot path, and the SaveCPUState
    // sites use 0xC008400 as a fixed kernel-globals offset unrelated
    // to per-task stack stride.

    // FMNewStack (17 patches):
    RomPatch { offset: 0x001F_8EDC, value: 0xE3A0_7A09, name: "FMNewStack: mov r7, #36864 (was 33792)" },
    RomPatch { offset: 0x001F_8EF0, value: 0xE240_1A01, name: "FMNewStack: sub r1, r0, #4096 (was 3072)" },
    RomPatch { offset: 0x001F_8F18, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (divisor)" },
    RomPatch { offset: 0x001F_8F20, value: 0xE080_0180, name: "FMNewStack: add r0, r0, r0, lsl #3 (×9)" },
    RomPatch { offset: 0x001F_8F24, value: 0xE049_0600, name: "FMNewStack: sub r0, r9, r0, lsl #12 (×4096)" },
    RomPatch { offset: 0x001F_8F30, value: 0xE280_0A01, name: "FMNewStack: add r0, r0, #4096 (guard)" },
    RomPatch { offset: 0x001F_8F38, value: 0xE350_0A09, name: "FMNewStack: cmp r0, #36864" },
    RomPatch { offset: 0x001F_8F48, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (divisor)" },
    RomPatch { offset: 0x001F_8F5C, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (divisor)" },
    RomPatch { offset: 0x001F_8F88, value: 0xE280_0A01, name: "FMNewStack: add r0, r0, #4096 (guard, alt)" },
    RomPatch { offset: 0x001F_8F90, value: 0xE350_0A09, name: "FMNewStack: cmp r0, #36864 (alt)" },
    RomPatch { offset: 0x001F_8FA0, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (alt-path divisor)" },
    RomPatch { offset: 0x001F_9024, value: 0xE08A_118A, name: "FMNewStack: add r1, sl, sl, lsl #3 (×9)" },
    RomPatch { offset: 0x001F_902C, value: 0xE080_9601, name: "FMNewStack: add r9, r0, r1, lsl #12 (×4096)" },
    RomPatch { offset: 0x001F_9030, value: 0xE087_0187, name: "FMNewStack: add r0, r7, r7, lsl #3 (×9)" },
    RomPatch { offset: 0x001F_9034, value: 0xE049_0600, name: "FMNewStack: sub r0, r9, r0, lsl #12 (×4096, end-of-slot)" },
    RomPatch { offset: 0x001F_9038, value: 0xE280_2A01, name: "FMNewStack: add r2, r0, #4096 (base = slot+guard)" },

    // Heap-domain divisor patches.
    //
    // Init__11THeapDomain (0x001F_8D74) was patched alongside
    // FMNewStack but reverted: that function constructs THeapDomain
    // for BOTH stack pools and regular data heaps. Patching the
    // divisor to 36 KiB correctly sizes the slot_info array for
    // stack pools but UNDER-sizes the bookkeeping for data heaps,
    // breaking ExtendVMHeap when the heap grows past what the
    // patched-down array can index. The unpatched 33 KiB divisor
    // OVER-sizes the array for stack pools (108 slots vs 99 actually
    // used) — wasted memory but functionally safe; FMNewStack only
    // ever computes indices in the 0..99 range.
    //
    // GetStackInfo and FMFree are stack-only paths, so the 36-KiB
    // divisor stays — correct slot index computation requires the
    // matching slot stride.
    RomPatch { offset: 0x001F_8E1C, value: 0xE3A0_0A09, name: "GetStackInfo: mov r0, #36864 (slot index divisor)" },
    RomPatch { offset: 0x001F_918C, value: 0xE3A0_0A09, name: "FMFree: mov r0, #36864 (slot index divisor)" },
    // GetMatchingPage = always-return-0 stub (iter 23). Forces every
    // FindOrAllocPage_ReturnUnLockedOnNoPage call into the cache-miss
    // branch → AllocNewPage → fresh PA from TUPageManager::Get. Without
    // this stub, the kernel's TStackManager reuses an existing
    // TStackPage's PA across distinct VAs (heap/stack/scratch) by relying
    // on ARMv4 subpage-AP — exactly the alias the iter-21 stage-1 walk
    // pinned for PA=0x04084000 (heap VA 0x0c646000 ↔ stack VA 0x0ccc8000).
    // Under flat AP=11 the alias collapses and the compressor's count
    // gets clobbered by another task's exception-frame push.
    //
    // Stub layout (replaces the original prologue at 0x001F_86B4):
    //   0x001F_86B4: mov r0, #0   (was: mov ip, sp = 0xE1A0_C00D)
    //   0x001F_86B8: bx lr         (was: push {r4-r10, fp, ip, lr, pc}
    //                                    = 0xE92D_DFF0)
    RomPatch { offset: 0x001F_86B4, value: 0xE3A0_0000, name: "GetMatchingPage: mov r0, #0 (return 0 — no shareable page)" },
    RomPatch { offset: 0x001F_86B8, value: 0xE12F_FF1E, name: "GetMatchingPage: bx lr (skip prologue+body)" },
    // iter-80: trick `TraceSetOptions__12TInterpreterFv` into
    // configuring trace mode even when the kernel's tracing
    // options frame (`gVars.tracing` or similar) is NIL.
    //
    // The function reads gVars.tracing into a Ref slot, then at
    // 0x35e7d8 tests `teq r0, #2` (Ref == NIL). On NIL it jumps
    // straight to the "tracing off" exit at 0x35ea18 — which is
    // the case on a stock boot. Our iter-79 force-enable poke
    // of `gInterpreter[+124]` causes `DoSend` to call
    // `TraceSend → TraceMethod`, but TraceMethod's *inner* gates
    // at +105 / +112 / +74 (all configured by TraceSetOptions)
    // suppress the actual `Print` call. Result: no trace output.
    //
    // Flipping the immediate from #2 to #0 turns the test into
    // `teq r0, #0`. Genuine Refs are never zero, so the test
    // never matches — TraceSetOptions falls through to the
    // setup-with-NIL-defaults branch which sets `+105 = 1`,
    // `+104 = 1`, and writes NIL to the +112 / +116 / +108
    // filter slots. With those gates open and our
    // `gInterpreter[+124]` poke in place, every `DoSend /
    // DoMessage / DoFastApply` reaches `Print` with the trace
    // event — surfaced via iter-79's Print thunk hook.
    //
    // Encoding: `teq r0, #N` is `e330_000N`; only the low 12
    // bits of the immediate change (cond/op/Rn/Rd untouched).
    RomPatch { offset: 0x0035_E7D8, value: 0xE330_0000, name: "TraceSetOptions: teq r0, #0 (was #2) — force trace setup even when gVars.tracing is NIL" },
    // Force exclusive per-stack page allocation by short-circuiting
    // `TStackManager::GetMatchingPage` to always return 0 (= "no
    // shareable page found"). This forces every `FindOrAllocPage` call
    // into the cache-miss path → `AllocNewPage` → fresh PA from
    // `TUPageManager::Get`. With this, no two stacks ever share a 4-KiB
    // physical page; ARMv7's loss of subpage-AP no longer matters
    // because an overrun stays inside this task's exclusive PA.
    //
    // GetMatchingPage entry is at 0x001f86b4. Its first instruction
    // (`mov ip, sp` = `0xE1A0_C00D`) is replaced with `mov r0, #0`
    // (= `0xE3A0_0000`); the second instruction (`push {r4-r10, fp,
    // ip, lr, pc}` = `0xE92D_DFF0`) is replaced with `bx lr`
    // (= `0xE12F_FF1E`). Together these form a two-instruction stub
    // that returns 0 immediately without touching the stack frame.
    //
    // Paired with the 4-iteration wrapper installed by
    // `apply_resolve_fault_wrapper`. Each ResolveFault iter:
    //   - iter 0 hits the first-allocation branch (page_table[N]=null),
    //     calls FindOrAllocPage which (with cache disabled) allocates
    //     a fresh page exclusively for this stack and sets sub 0's
    //     owner via PageMatchFound(mask=1<<0).
    //   - iters 1..3 hit the existing-page branch (page_table[N] is
    //     now set), find sub.owner=NULL (only sub 0 was assigned), and
    //     fall through to SetSubPageInfo(sub_idx) → success_tail.
    //
    // The kernel's existing-page comparison at 0x1f7a4c-0x1f7a5c reads
    // a word containing page_idx_hi/lo bytes that were written via
    // STRB (XOR-3 byte-swizzled by shadow_stub). The word read gives
    // wrong data for sub_idx=3 (the high half of the word reads into
    // the refcount region in LE byte-order). To avoid that path, iters
    // 1-3 must hit r9==0 (sub unowned) so the comparison is skipped —
    // which requires PageMatchFound to NOT preemptively assign all 4
    // subs. That's why we don't patch `0x1f7a10` to mask=0xF; we let
    // PageMatchFound set only sub 0 and rely on per-iter
    // SetSubPageInfo for the rest.
];

/// HVC immediates that the ROM-patched DebugStr / Debugger trap sites
/// use to reach the hypervisor. Must match the dispatch in
/// `trap::handle_hvc`.
pub const DEBUG_STR_HVC_IMM: u32 = 0x40;
pub const DEBUGGER_HVC_IMM: u32 = 0x41;

/// Phase-B canary: PowerOffAndReboot at 0x000E_6BBC. The kernel calls
/// this whenever a fatal init-time check fails (e.g. flash chip
/// identification yields no driver match — see INVESTIGATION.md).
/// Under our hypervisor that means the boot has gone wrong but the
/// kernel thinks rebooting will help — it won't, the same failure
/// recurs and the trace fills with hundreds of post-mortem repetitions.
///
/// Patch the first word with `HVC #POWEROFF_REBOOT_HVC_IMM` so we
/// halt loudly the FIRST time it fires, with the caller's R0 (reboot
/// reason) and the trace context immediately preceding the call.
pub const POWEROFF_REBOOT_PC: u32 = 0x000E_6BBC;
pub const POWEROFF_REBOOT_HVC_IMM: u32 = 0x42;

/// Phase-B canary: `Reboot(long, unsigned long, unsigned char)` at
/// 0x000D_9884. This is the "soft-reboot" path the kernel's exception
/// unwinder calls on an UnhandledException (the path that bypassed
/// our PowerOffAndReboot canary and wedged into a reboot loop during
/// the 2026-04-23 StartupProtocolRegistry stall). Same canary shape:
/// patch the first word to `HVC #REBOOT_HVC_IMM` so we halt on the
/// first hit with the caller's R0 = reboot reason.
pub const REBOOT_PC: u32 = 0x000D_9884;
pub const REBOOT_HVC_IMM: u32 = 0x43;

/// Phase-B canary: `BootOS` / `ROMBoot` at 0x0001_8688. The AArch32
/// reset vector at VA 0 is `B 0x18688`, so the first execution after
/// the hypervisor's ERET-to-guest lands here. Any subsequent entry is
/// a SOFTWARE RESET — regardless of whether the kernel took the
/// `Reboot` / `PowerOffAndReboot` path (already canaried) or jumped
/// directly to the reset vector via some other mechanism (watchdog,
/// MOV PC,#0, etc.). Canary: patch the first word to `HVC #0x44`; the
/// handler allows the first entry through by emulating the original
/// first insn (`mov r0, #0xb0`) and then halts on every subsequent
/// entry.
pub const BOOTOS_PC: u32 = 0x0001_8688;
pub const BOOTOS_HVC_IMM: u32 = 0x44;
/// The original first instruction of `BootOS`: `mov r0, #0xb0`
/// (0xE3A000B0). The HVC handler emulates this on the legitimate
/// first boot by setting r0 = 0xb0 and advancing ELR past the HVC.
pub const BOOTOS_ORIG_INSN: u32 = 0xE3A0_00B0;

/// AArch32 `HVC #imm16` encoding at unconditional (cond=AL).
const fn hvc_insn(imm: u32) -> u32 {
    0xE140_0070 | ((imm & 0xFFF0) << 4) | (imm & 0xF)
}

/// ROM offsets reserved for the per-patch stubs. All sit in the
/// post-UND-trampoline region at 0x00FFFFxx — `tracer::in_reserved_range`
/// excludes them so they're never UDF-patched by the function tracer.
///
/// Each DebugStr / Debugger stub is 2 words:
///   MOV r7, LR    — copy the AArch32 source-mode LR into r7, a non-
///                   banked GPR. Source mode is SVC for the ROM call
///                   sites; LR in SVC is R14_svc, which per ARM ARM
///                   Table D1-79 lives in `ctx.x[18]` from EL2, not
///                   `ctx.x[14]` (= LR_usr). Stashing into r7 (= R7,
///                   shared across all non-FIQ modes, ctx.x[7])
///                   sidesteps that mapping question entirely.
///   HVC #imm      — trap to EL2
/// Entry point of `TStackManager::ResolveFault` that the wrapper invokes.
const RESOLVE_FAULT_PC: u32 = 0x001F_7978;

/// PC of the `bl ResolveFault` call inside `TStackManager::Fault` —
/// the site we re-target to the wrapper.
const FAULT_BL_RESOLVE_PC: u32 = 0x001F_84E0;

/// PC of the `bl ResolveFault` call inside `FMLockHeapRange`. Same
/// retarget so all `ResolveFault` invocations go through the wrapper.
const FMLOCK_BL_RESOLVE_PC: u32 = 0x001F_6B94;

/// `LockHeapRange` (caller-side glue at 0x001F_8AB4) and
/// `UnlockHeapRange` (0x001F_8B88) — the two ABI entry points whose
/// `(base, limit, lock_id?)` parms are forwarded to FMLockHeapRange /
/// FMUnlockHeapRange via SafeUserRequestEntry req-ids 6 and 7.
///
/// We patch each function's first instruction (`mov ip, sp`) with a
/// `b WRAPPER`. The wrapper aligns r0 (base) DOWN to a 4-KiB boundary
/// and r1 (limit) UP to a 4-KiB boundary, then re-enters the real
/// function at +4 (skipping the patched-out `mov ip, sp` after
/// replicating it locally).
///
/// Why: the kernel's design partitions a 4-KiB physical page into
/// four 1-KiB subpages, each VA-owned by a different heap/stack/driver
/// object via ARMv4 subpage-AP. ARMv7 has no subpage-AP — once we
/// flatten to AP=011 (full RW), any user write hits all four subpages.
/// FMLockHeapRange's per-1-KiB-subpage `ResolveFault` iteration
/// happily allocates subpage 1 of an existing page to a different
/// owner than subpage 0; that's the bug. Forcing every caller's range
/// to a 4-KiB boundary makes FMLockHeapRange iterate exactly four
/// times per page, claiming all four subpages for ONE owner. The
/// `page_table[page_index]` slot then names a page exclusively owned
/// by that caller; subsequent callers asking for adjacent VA pages
/// can't accidentally land on the same physical page.
///
/// This SUBSUMES the per-call-site fix (29 distinct LockHeapRange
/// callers) and matches the existing NewHeap/NewVMHeap chunk_size=4096
/// patches at the heap-allocation layer.
const LOCK_HEAP_RANGE_PC: u32 = 0x001F_8AB4;
const UNLOCK_HEAP_RANGE_PC: u32 = 0x001F_8B88;

/// The original first word of LockHeapRange / UnlockHeapRange — the
/// standard `mov ip, sp` AArch32 prologue. Asserted at install time so
/// any future ROM build that shifts the function entries fails loudly.
const LOCK_UNLOCK_ORIG_FIRST_INSN: u32 = 0xE1A0_C00D;

// ---- L1[0xCD] lazy-grow investigation probes (2026-04-26) -----------------
//
// See docs/plans/l1-cd-lazy-investigation.md. The kernel's expected
// `RememberMappings → Remember → SWI #12 → AllocatePageTable` chain
// drove the original probe set. Most of those probes were swept in
// the BE-8 migration Phase 0; the surviving 0x47 (REMEMBER_SWIRET) is
// retained as load-bearing instrumentation for the Remember post-SWI
// fixup.

/// HVC immediate fired by the patched word at 0x00258E50 (immediately
/// after the first `bl GenericSWI` inside Remember). Handler logs r0
/// (= the SWI #12 return value), then emulates `mov r8, #237` so the
/// kernel's `r8 = -10003` constant is restored before the `teq` at
/// 0x00258E58.
pub const REMEMBER_SWIRET_HVC_IMM:   u32 = 0x47;

/// QEMU raspi3b workaround: replace the `mrs r1, SPSR` at the head of
/// `DataAbortHandler` (PC 0x00393144) with an HVC. Empirically (see
/// qemu7.log lines 2237/2252 and the `[mrs] -- mrs DIVERGES FROM SAVED
/// SLOT --` marker in the dabt-forward log) `mrs spsr_abt` from EL2
/// returns a stale value relative to the trampoline-saved SPSR_abt.
/// Writing `msr spsr_abt, <saved>` from EL2 before ERETing to DAH did
/// **not** propagate to the kernel's later AArch32 ABT-mode `mrs r1,
/// SPSR` — the kernel still saw the stale value and branched to the
/// throw exit at 0x393158, rebooting on the L1[0xCD]=0x90 fault that
/// the recovery path would otherwise have grown lazily. The handler
/// for this HVC reads the trampoline-saved SPSR_abt at
/// `DABT_SAVE_PA + 8` and writes it into `ctx.x[1]`, so the kernel's
/// next instruction (`and r1, r1, #31`) sees the architecturally-
/// correct mode bits regardless of QEMU's staleness. On FVP the
/// trampoline-saved value matches what `mrs r1, SPSR` would have
/// returned, so this patch is functionally a no-op there. Mirrors
/// docs/QEMU_BUGS.md Bug #1's banked-LR workaround.
pub const DAH_MRS_SPSR_HVC_IMM:          u32 = 0x4F;

const REMEMBER_SWIRET_PC:            u32 = 0x0025_8E50;
const REMEMBER_SWIRET_ORIG_INSN:     u32 = 0xE3A0_80ED; // mov r8, #237

/// `mrs r1, SPSR` at DAH entry (4th instruction past the function
/// label, after the DACR setup). Original encoding `0xE14F_1000`. We
/// replace it with `HVC #DAH_MRS_SPSR_HVC_IMM` so the EL2 handler can
/// supply the architecturally-correct SPSR_abt from the trampoline-
/// saved slot, working around QEMU raspi3b's stale `mrs spsr_abt`.
pub const DAH_MRS_SPSR_PC:           u32 = 0x0039_3144;
const DAH_MRS_SPSR_INSN:             u32 = 0xE14F_1000;

/// `cmp r0, #0` immediately after `bl FaultMonitorEntry` at PC
/// `0x00393984` (DAH's post-monitor-call branch decision). The
/// preceding `bl` jumps through the post-ship patch table at
/// `0x01af7bf4` to whatever current FaultMonitorEntry the kernel has
/// installed. After the call, r0 = 0 means "monitor handled the fault,
/// take the recovery path"; non-zero means "monitor declined, fall
/// through toward RebootIfFaultWasInStack".
///
/// On Einstein, the abort #16 at FAR=0x0CD07400 (mode=USR, DFSC=5)
/// recovers — i.e. FaultMonitorEntry returns 0 there. Our hypervisor
/// reaches the throw exit on the same fault, which only happens if
/// FaultMonitorEntry returns non-zero. Patch this `cmp` with an HVC so
/// EL2 can log the return value and emulate the `cmp r0, #0` flag
/// update, leaving the `beq 0x393a30` at `0x393988` to branch as the
/// kernel intended.
pub const DAH_FME_RET_HVC_IMM:        u32 = 0x50;
pub const DAH_FME_RET_PC:             u32 = 0x0039_3984;
const DAH_FME_RET_INSN:               u32 = 0xE3500000;

/// Static FaultMonitorEntry entry at PC `0x0011FC60`. The post-ship
/// patch table at `0x01AF7BF4` is the symbol target the kernel actually
/// calls; if its slot redirects through the static entry (no REx
/// override active), this probe fires and gives us the input fault
/// mask plus the implementation flow. Original first insn:
/// `mov ip, sp = 0xE1A0_C00D`. Replace with `HVC #DAH_FME_ENTRY_HVC_IMM`;
/// the handler emulates `mov ip, sp` (writes ctx.x[12] = ctx.x[13]),
/// logs r0 (= input fault mask), and returns. If the probe doesn't
/// fire, the post-ship slot is redirected to a different (REx-side)
/// implementation we don't have disasm for.
pub const DAH_FME_ENTRY_HVC_IMM:      u32 = 0x51;
pub const FME_STATIC_PC:              u32 = 0x0011_FC60;
const FME_STATIC_FIRST_INSN:          u32 = 0xE1A0_C00D;

/// `ldr r1, [pc, #1588]` at DAH PC `0x00393318`, the load that
/// initialises `r1` with `gKernelGlobals` (= `0x0C100FF8`) before the
/// OR-chain at `0x393320..0x393344` builds the fault bitmask passed to
/// `FaultMonitorEntry`. Original encoding `0xE59F_1634`. We replace it
/// with `HVC #DAH_OR_CHAIN_HVC_IMM`; the handler reads
/// `*gKernelGlobals` (= curr_task), then `curr_task->[+0x74/+0x78/+0x7c]`
/// (TUDomainManager pointers) and each monitor's `[+0x10]` (the OR'd
/// value), logs them, and emulates the original load by writing the
/// literal `0x0C100FF8` into `ctx.x[1]`. Natural ERET resumes at
/// `0x39331c` (the kernel's `ldr r1, [r1]`) so the kernel proceeds
/// normally with `r1 = 0x0C100FF8`.
///
/// Diagnostic for the γ probe: distinguishes (E-1) curr_task changed
/// between fault #2 entry and Reboot from (E-2) same task but
/// monitor[+0x74] mutated. See INVESTIGATION.md "γ probe outcome".
pub const DAH_OR_CHAIN_HVC_IMM:       u32 = 0x52;
pub const DAH_OR_CHAIN_PC:            u32 = 0x0039_3318;
const DAH_OR_CHAIN_INSN:              u32 = 0xE59F_1634;
pub const G_KERNEL_GLOBALS_VA:        u32 = 0x0C10_0FF8;

/// `TTask::Init`'s `bl NewStack` site at ROM `0x0025238c`. The probe
/// in the previous iteration showed this BL is the user-mode caller
/// upstream of 11 of 12 Group-2 aliased PAs. The aliasing comes from
/// adjacent stacks deliberately sharing 4-KiB boundary pages
/// (ARMv4 subpage-AP design); under our flat AP=011 the boundary
/// becomes a real PA alias.
///
/// **Per-stack 4 KiB padding (Option A in PLAN.md):** redirect the
/// BL to a wrapper at `NEW_STACK_PAD_WRAPPER_PC` which adds 0x1000
/// to `r1` (size argument) before tail-calling the real `NewStack`
/// at `NEW_STACK_PC`. The 4 KiB pad gives every stack one extra
/// page beyond its requested size; the kernel's per-domain
/// "next stack VA" pointer decrements by the new (larger) size on
/// each allocation, so adjacent stacks no longer share boundary
/// pages. The encoding `add r1, r1, #4180` (84 + 4096) doesn't fit
/// in a single ARM imm12, so the wrapper is the cleanest place to
/// inject the +4 KiB.
const NEW_STACK_PAD_BL_PC:        u32 = 0x0025_238C;
/// The BL at `NEW_STACK_PAD_BL_PC` originally targets the post-ship
/// patch-table thunk for NewStack at `0x001BD7BA4` (visible in the
/// disasm as a `<NewStack>` label inside the 0x01A00000..0x01C20000
/// patch-table region). The wrapper preserves that target via
/// tail-call so any future REx-side override of NewStack still
/// applies. The static body is at `0x001F8968` (also labelled
/// `<NewStack>` — the user-mode SWI shim) but going through the
/// thunk is the architecturally correct path.
const NEW_STACK_THUNK_PC:         u32 = 0x001B_D7BA4;
/// Original first-word at the BL site — used to assert the patch
/// applies to the expected ROM. `bl 0x001bd7ba4` from PC 0x25238c
/// has offset bytes `(0x1bd7ba4 - 0x252394) = 0x1985810`, off in
/// words = `0x6_6160`, encoded as `0xEB66_1604`.
const NEW_STACK_PAD_BL_ORIG_INSN: u32 = 0xEB66_1604;

/// `UnhandledException(char* name, void* data, void(*handler)(void*))`
/// at ROM `0x000B_0220`. The first arg `r0` is a pointer to the
/// exception name as an ASCII string (e.g. "evt.ex.abt.perm" for a
/// permission DABT). Patching the entry with HVC and dumping the
/// name string directly is the right wedge tripwire — far cleaner
/// than chasing the downstream Reboot canary and decoding the
/// stack-passed string.
pub const UNHANDLED_EXCEPTION_HVC_IMM:    u32 = 0x69;
pub const UNHANDLED_EXCEPTION_PC:         u32 = 0x000B_0220;
const UNHANDLED_EXCEPTION_FIRST_INSN:     u32 = 0xE1A0_C00D; // mov ip, sp

/// `UnhandledNonUserModeException(char*, void*, void(*)(void*))` at
/// ROM `0x000B_031C`. Same signature as `UnhandledException` but
/// invoked from non-USR contexts (UND/SVC/ABT). Mirrors the
/// previous probe so we catch both paths at entry.
pub const UNHANDLED_NUM_EXCEPTION_HVC_IMM: u32 = 0x6A;
pub const UNHANDLED_NUM_EXCEPTION_PC:      u32 = 0x000B_031C;
const UNHANDLED_NUM_EXCEPTION_FIRST_INSN:  u32 = 0xE1A0_C00D; // mov ip, sp

/// FPE-entry probe at `FP_UndefHandlers_Start + 0x3C` = `0x0038_D918`.
/// Original first insn is `mov ip, sp` (`0xE1A0_C00D`); replace with
/// `HVC #FPE_ENTRY_PROBE_HVC_IMM`. The handler:
///
///   1. Counts FPE entries (per-call counter).
///   2. On entry #2 (= forward #2 = mvfs in SetSystemVolume that wedges
///      the FPE on the IP-corruption trap), calls
///      `crate::tarmac::emit_start()` to open the TarmacTrace window.
///      The matching `emit_stop()` fires from the unrecognised-UND
///      halt path in `handle_und`.
///   3. Emulates the original `mov ip, sp` by setting
///      `ctx.x[12] = ctx.x[23]` (= sp_und, since the FPE always runs
///      in UND mode after iter-84's bypass delivers UND naturally).
///
/// The trace bracketed by entry #2's start and the halt's stop
/// captures every instruction + register write inside the FPE call
/// that wedges — small enough to grep for the moment R12 transitions
/// from `0x0c005fc0` (post-`mov ip, sp`) to `0x003900c8` (at the
/// trap), pinning the IP-clobber site.
pub const FPE_ENTRY_PROBE_HVC_IMM: u32 = 0x80;
pub const FPE_ENTRY_PROBE_PC:      u32 = 0x0038_D918;
const FPE_ENTRY_FIRST_INSN:        u32 = 0xE1A0_C00D; // mov ip, sp

/// `safeIntervalDeltaSeconds` from `TJITGenericROMPatch.cpp:144` —
/// seconds between 1993-01-01 and 2008-01-01, Einstein's Y2010 fix
/// constant.
const SAFE_INTERVAL_DELTA_SECONDS: u32 = 473_299_200;

/// DataAbortHandler instruction-as-data load:
/// `ldr r0, [lr]` at PC 0x003931e4 reads the faulting word so the
/// kernel can decode the abort. Under our load-time byteswap of code-
/// marked memory, a CPSR.E=1 LDR returns the byteswapped encoding —
/// the kernel cannot recognise its own opcodes. Replace the LDR with
/// a branch to a 6-word stub that does the LDR and byteswaps r0
/// back to BE-natural before resuming at 0x003931e8.
const DAH_FAULT_LDR_PC:        u32 = 0x0039_31E4;
const DAH_FAULT_LDR_ORIG_INSN: u32 = 0xE59E_0000; // ldr r0, [lr]
const DAH_FAULT_LDR_RESUME_PC: u32 = 0x0039_31E8;

/// UndefinedInstruction handler instruction-as-data load:
/// `ldr r1, [lr, #-4]` at PC 0x0038ce9c reads the faulting word so
/// the kernel can compare against UDF marker patterns. Same byteswap
/// problem as the DAH site; same fix shape but targets r1 with a
/// resume at 0x0038cea0.
const UND_FAULT_LDR_PC:        u32 = 0x0038_CE9C;
const UND_FAULT_LDR_ORIG_INSN: u32 = 0xE51E_1004; // ldr r1, [lr, #-4]
const UND_FAULT_LDR_RESUME_PC: u32 = 0x0038_CEA0;

/// `SWIBoot` handler instruction-as-data load:
/// `ldr r0, [lr, #-4]` at PC 0x003ad69c reads the SWI instruction so
/// the kernel can extract the SWI immediate (and dispatch type from
/// bits[27:24]). Without this fix the byteswapped read makes every
/// SWI dispatch to the wrong handler, wedging the boot in a tight
/// loop at MonitorDispatchSWI 0x3ae320 (svc 0x1b) → SWIBoot →
/// unhandled-SWI fallback → return → svc 0x1b again.
const SWIBOOT_LDR_PC:        u32 = 0x003a_D69C;
const SWIBOOT_LDR_ORIG_INSN: u32 = 0xE51E_0004; // ldr r0, [lr, #-4]
const SWIBOOT_LDR_RESUME_PC: u32 = 0x003a_D6A0;

/// Small helper to emit an ARM `B target` at `src_pc`.
const fn arm_b(src_pc: u32, target: u32) -> u32 {
    let off_bytes = target.wrapping_sub(src_pc.wrapping_add(8)) as i32;
    let off_words = (off_bytes / 4) as u32;
    0xEA00_0000 | (off_words & 0x00FF_FFFF)
}

/// Same as `arm_b`, but emits `BL` (link bit set).
const fn arm_bl(src_pc: u32, target: u32) -> u32 {
    let off_bytes = target.wrapping_sub(src_pc.wrapping_add(8)) as i32;
    let off_words = (off_bytes / 4) as u32;
    0xEB00_0000 | (off_words & 0x00FF_FFFF)
}

/// Apply Einstein's word-write patches to the byteswapped main ROM
/// backing. Caller must own `rom_ptr`; the patches live entirely in the
/// main-ROM half (offsets < 0x0080_0000), so overlap with Einstein.rex
/// loaded at 0x0080_0000 is not a concern.
///
/// SAFETY: `rom_ptr` must point to at least `0x0080_0000` bytes of
/// writable backing, and all patch offsets are checked to be in range
/// and word-aligned before the write.
pub unsafe fn apply_717006_patches(rom_ptr: *mut u32) {
    let mut applied = 0usize;
    for p in PATCHES_717006 {
        debug_assert!(p.offset & 3 == 0, "patch offset must be word-aligned");
        debug_assert!((p.offset as usize) < 0x0080_0000, "patch offset must be in main ROM");
        let word_idx = (p.offset / 4) as usize;
        // SAFETY: bounds-checked against the 8 MiB main-ROM region.
        // Dispatch via the classifier bitmap so each entry is treated
        // correctly under BE-8: instruction patches go in as native u32
        // (= LE encoding of the BE numerical value); data patches are
        // byte-swapped before storage so a guest LDR reads the
        // intended value.
        unsafe {
            let prev = rom_ptr.add(word_idx).read();
            crate::guest_mem::write_rom_word_by_kind(rom_ptr, word_idx, p.value);
            kprintln!(
                "rom_patch: {:#010x}: was_host={:#010x} -> intended={:#010x}  ({})",
                p.offset, prev, p.value, p.name,
            );
        }
        applied += 1;
    }

    // Einstein's TJITGenericPatchNativeCall / TJITGenericPatchNativeInjection
    // patches, translated from SWI-dispatch into inline ARM so we don't
    // need a JIT layer:
    //   * DebugStr / Debugger          — HVC trap to EL2
    //   * RealClockSeconds             — inline MMIO calendar read
    //   * FTimeInSeconds (injection)   — modify r0 via stub, branch to epilogue
    //   * FDateFromSeconds (injection) — modify r1 via stub, branch to epilogue
    // SAFETY: rom_ptr has the full 8 MiB ROM.
    unsafe {
        apply_debug_patches(rom_ptr);
        apply_real_clock_seconds_patch(rom_ptr);
        apply_ftime_in_seconds_patch(rom_ptr);
        apply_fdate_from_seconds_patch(rom_ptr);
        apply_poweroff_reboot_trap(rom_ptr);
        apply_reboot_trap(rom_ptr);
        apply_bootos_trap(rom_ptr);
        apply_resolve_fault_wrapper(rom_ptr);
        // `apply_new_stack_pad_wrapper` is NOT installed — the
        // call-site +4 KiB pad changes the size *requested* of
        // NewStack but not the kernel's stack-pool slot stride. The
        // result is that fewer-but-larger stacks fit per pool and
        // the kernel's per-task pool index runs past the upper
        // bound on the (N+1)-th stack, producing an infinite
        // ResolveFault-loop at `FAR == info_bounds.end + 3`. See
        // INVESTIGATION.md "Option A pad attempt — wedges on
        // info_bounds overflow" for the full diagnostic.
        let _ = apply_new_stack_pad_wrapper;
        // `apply_lock_heap_range_wrapper` stays in the source for
        // reference but is NOT installed. The 4-KiB-rounding wrapper at
        // LockHeapRange/UnlockHeapRange entry corrupts adjacent
        // allocations because FMLockHeapRange's `parms[+8] != 0`
        // flag-set loop writes `[TStackPage+subpage]+44 = 1` for the
        // widened range, pinning subpages owned by OTHER stack_infos
        // (Pattern A driver inits use lock_id=1; see
        // `docs/STRUCTURES.md` "TStackManager"). The right fix is
        // per-allocator (see ZapHeap patch in PATCHES_717006); the
        // wrapper itself is the wrong layer.
        let _ = apply_lock_heap_range_wrapper;
        apply_l1_cd_probes(rom_ptr);
        apply_fault_handler_ldr_byteswap_patches(rom_ptr);
    }

    kprintln!("rom_patch: applied {} simple patches + 5 native-call/injection ROM patches + PowerOffAndReboot + Reboot + BootOS + ResolveFault-wrapper + L1[0xCD] probes + fault-handler LDR byteswap stubs", applied);
}

/// Install the HVC probes that survive past the BE-8 migration. Most
/// of the iter-50..89 lazy-grow / heap / soup / textdecomp probes were
/// swept in Phase 0 of the BE-8 migration; what remains is the small
/// set of operational probes used by the running hypervisor (DAH /
/// FaultMonitorEntry / OR-chain / FPE-entry / Remember-post-SWI /
/// UnhandledException tripwires). See PLAN_BE8_MIGRATION.md "Phase 0".
unsafe fn apply_l1_cd_probes(rom_ptr: *mut u32) {
    unsafe {
        // Remember post-SWI fixup: the kernel's `r8 = -10003` sentinel
        // value is loaded after a `bl GenericSWI`. Our handler logs the
        // SWI return and re-establishes the constant before the
        // following `teq`. Required for the kernel's Remember path.
        patch_probe(
            rom_ptr,
            REMEMBER_SWIRET_PC,
            REMEMBER_SWIRET_ORIG_INSN,
            hvc_insn(REMEMBER_SWIRET_HVC_IMM),
            "Remember post-SWI",
            REMEMBER_SWIRET_HVC_IMM,
        );
        // QEMU raspi3b workaround: patch the kernel's `mrs r1, SPSR`
        // at DAH entry (0x393144) so EL2 can substitute the
        // trampoline-saved SPSR_abt for the stale `mrs spsr_abt`.
        patch_probe(
            rom_ptr,
            DAH_MRS_SPSR_PC,
            DAH_MRS_SPSR_INSN,
            hvc_insn(DAH_MRS_SPSR_HVC_IMM),
            "DataAbortHandler mrs r1, SPSR (QEMU spsr_abt staleness fix)",
            DAH_MRS_SPSR_HVC_IMM,
        );
        // Layer-γ probe: `cmp r0, #0` after `bl FaultMonitorEntry` at
        // 0x00393984. Captures FaultMonitorEntry's return value so we
        // can compare against Einstein's recovery path.
        patch_probe(
            rom_ptr,
            DAH_FME_RET_PC,
            DAH_FME_RET_INSN,
            hvc_insn(DAH_FME_RET_HVC_IMM),
            "DAH FaultMonitorEntry return cmp r0, #0",
            DAH_FME_RET_HVC_IMM,
        );
        // Layer-γ probe: static FaultMonitorEntry entry at 0x0011FC60.
        // Captures input fault mask in r0 if the post-ship slot
        // redirects through the static entry.
        patch_probe(
            rom_ptr,
            FME_STATIC_PC,
            FME_STATIC_FIRST_INSN,
            hvc_insn(DAH_FME_ENTRY_HVC_IMM),
            "FaultMonitorEntry static entry (mov ip, sp)",
            DAH_FME_ENTRY_HVC_IMM,
        );
        // Layer-γ probe: DAH OR-chain entry at 0x00393318 (`ldr r1,
        // [pc, #1588]` loading gKernelGlobals VA). Captures curr_task
        // and its monitor list to distinguish (E-1) curr_task changed
        // from (E-2) same task with mutated monitor list.
        patch_probe(
            rom_ptr,
            DAH_OR_CHAIN_PC,
            DAH_OR_CHAIN_INSN,
            hvc_insn(DAH_OR_CHAIN_HVC_IMM),
            "DAH OR-chain entry (ldr r1, gKernelGlobals)",
            DAH_OR_CHAIN_HVC_IMM,
        );
        // UnhandledException tripwires — halt cleanly with the kernel-
        // supplied exception-name string instead of letting the boot
        // bury the diagnostic under downstream Reboot / abort noise.
        patch_probe(
            rom_ptr,
            UNHANDLED_EXCEPTION_PC,
            UNHANDLED_EXCEPTION_FIRST_INSN,
            hvc_insn(UNHANDLED_EXCEPTION_HVC_IMM),
            "UnhandledException entry (halt-on-entry tripwire)",
            UNHANDLED_EXCEPTION_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            UNHANDLED_NUM_EXCEPTION_PC,
            UNHANDLED_NUM_EXCEPTION_FIRST_INSN,
            hvc_insn(UNHANDLED_NUM_EXCEPTION_HVC_IMM),
            "UnhandledNonUserModeException entry (halt-on-entry tripwire)",
            UNHANDLED_NUM_EXCEPTION_HVC_IMM,
        );
        // FPE-entry probe — load-bearing FP-bypass plumbing. Per the
        // BE-8 migration plan this is kept regardless of the iter-50..89
        // probe sweep: the handler emulates the FPE call without actually
        // running the FP undef trampoline.
        patch_probe(
            rom_ptr,
            FPE_ENTRY_PROBE_PC,
            FPE_ENTRY_FIRST_INSN,
            hvc_insn(FPE_ENTRY_PROBE_HVC_IMM),
            "FP_UndefHandlers_Start mov ip, sp (FPE bypass)",
            FPE_ENTRY_PROBE_HVC_IMM,
        );
    }
}

/// Patch the kernel's three known `LDR` sites that read the faulting
/// (or trapping) instruction word as data:
///
///   - DataAbortHandler 0x003931e4 (`ldr r0, [lr]`)
///   - UndefinedInstruction 0x0038ce9c (`ldr r1, [lr, #-4]`)
///   - SWIBoot 0x003ad69c (`ldr r0, [lr, #-4]`)
///
/// Under load-time BE-8 byteswap of code-marked memory, the kernel's
/// CPSR.E=1 `LDR` returns the bytes in the wrong order — the
/// numerical value is the byteswap of the original instruction
/// encoding the kernel was compiled to recognise.
///
/// Each site is replaced with `B stub`, where the stub re-emits the
/// LDR, byteswaps the result with `REV Rd, Rd`, and falls through
/// with `B resume`. The kernel was compiled for ARMv4 (no REV) but
/// the host CPU is ARMv8 / Cortex-A53 in AArch32 mode — which decodes
/// every ARMv6+ instruction including REV. Three words per stub.
///
/// REV (A1) encoding: `cond 0110 1011 1111 Rd 1111 0011 Rm`. For
/// `REV Rd, Rd`: 0xE6BF_0F30 | (Rd << 12) | Rd.
unsafe fn apply_fault_handler_ldr_byteswap_patches(rom_ptr: *mut u32) {
    // DABT site: target r0.
    //   +0x00 LDR r0, [lr]                e59e0000
    //   +0x04 REV r0, r0                  e6bf_0f30
    //   +0x08 B   DAH_FAULT_LDR_RESUME_PC arm_b(...)
    let dah_stub_pc = alloc_patch_stub(3, "DAH faulting-insn LDR byteswap");
    let dah_stub: [u32; 3] = [
        0xE59E_0000, // LDR r0, [lr]
        0xE6BF_0F30, // REV r0, r0
        arm_b(dah_stub_pc + 0x08, DAH_FAULT_LDR_RESUME_PC),
    ];

    // UND site: target r1.
    //   +0x00 LDR r1, [lr, #-4]           e51e1004
    //   +0x04 REV r1, r1                  e6bf_1f31
    //   +0x08 B   UND_FAULT_LDR_RESUME_PC arm_b(...)
    let und_stub_pc = alloc_patch_stub(3, "UND faulting-insn LDR byteswap");
    let und_stub: [u32; 3] = [
        0xE51E_1004, // LDR r1, [lr, #-4]
        0xE6BF_1F31, // REV r1, r1
        arm_b(und_stub_pc + 0x08, UND_FAULT_LDR_RESUME_PC),
    ];

    // SWIBoot site: target r0. Same encoding as DAH but offset is -4.
    //   +0x00 LDR r0, [lr, #-4]           e51e0004
    //   +0x04 REV r0, r0                  e6bf_0f30
    //   +0x08 B   SWIBOOT_LDR_RESUME_PC   arm_b(...)
    let swiboot_stub_pc = alloc_patch_stub(3, "SWIBoot SWI-insn LDR byteswap");
    let swiboot_stub: [u32; 3] = [
        0xE51E_0004, // LDR r0, [lr, #-4]
        0xE6BF_0F30, // REV r0, r0
        arm_b(swiboot_stub_pc + 0x08, SWIBOOT_LDR_RESUME_PC),
    ];

    unsafe {
        write_stub_words(rom_ptr, dah_stub_pc, &dah_stub);
        write_stub_words(rom_ptr, und_stub_pc, &und_stub);
        write_stub_words(rom_ptr, swiboot_stub_pc, &swiboot_stub);

        // DAH site
        let dah_idx = (DAH_FAULT_LDR_PC / 4) as usize;
        let prev = rom_ptr.add(dah_idx).read();
        if prev != DAH_FAULT_LDR_ORIG_INSN {
            kprintln!(
                "rom_patch: ERROR — DAH faulting-insn LDR at {:#010x} is {:#010x}, expected {:#010x}; skipping byteswap stub",
                DAH_FAULT_LDR_PC, prev, DAH_FAULT_LDR_ORIG_INSN,
            );
        } else {
            let insn = arm_b(DAH_FAULT_LDR_PC, dah_stub_pc);
            crate::guest_mem::write_rom_code_word(rom_ptr, dah_idx, insn);
            record_original(DAH_FAULT_LDR_PC, prev);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (DAH ldr r0,[lr] → B stub @ {:#x}, byteswap)",
                DAH_FAULT_LDR_PC, prev, insn, dah_stub_pc,
            );
        }

        // UND site
        let und_idx = (UND_FAULT_LDR_PC / 4) as usize;
        let prev = rom_ptr.add(und_idx).read();
        if prev != UND_FAULT_LDR_ORIG_INSN {
            kprintln!(
                "rom_patch: ERROR — UND faulting-insn LDR at {:#010x} is {:#010x}, expected {:#010x}; skipping byteswap stub",
                UND_FAULT_LDR_PC, prev, UND_FAULT_LDR_ORIG_INSN,
            );
        } else {
            let insn = arm_b(UND_FAULT_LDR_PC, und_stub_pc);
            crate::guest_mem::write_rom_code_word(rom_ptr, und_idx, insn);
            record_original(UND_FAULT_LDR_PC, prev);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (UND ldr r1,[lr,-4] → B stub @ {:#x}, byteswap)",
                UND_FAULT_LDR_PC, prev, insn, und_stub_pc,
            );
        }

        // SWIBoot site
        let swib_idx = (SWIBOOT_LDR_PC / 4) as usize;
        let prev = rom_ptr.add(swib_idx).read();
        if prev != SWIBOOT_LDR_ORIG_INSN {
            kprintln!(
                "rom_patch: ERROR — SWIBoot LDR at {:#010x} is {:#010x}, expected {:#010x}; skipping byteswap stub",
                SWIBOOT_LDR_PC, prev, SWIBOOT_LDR_ORIG_INSN,
            );
        } else {
            let insn = arm_b(SWIBOOT_LDR_PC, swiboot_stub_pc);
            crate::guest_mem::write_rom_code_word(rom_ptr, swib_idx, insn);
            record_original(SWIBOOT_LDR_PC, prev);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (SWIBoot ldr r0,[lr,-4] → B stub @ {:#x}, byteswap)",
                SWIBOOT_LDR_PC, prev, insn, swiboot_stub_pc,
            );
        }
    }
}

/// Helper: replace one ROM word with an HVC, panicking loudly if the
/// previous word doesn't match the recorded original. A mismatch means
/// the ROM has shifted under us (different ROM image or earlier patch
/// stomped the same offset) and the probe handler's emulation of the
/// "original" first instruction would be wrong.
unsafe fn patch_probe(
    rom_ptr: *mut u32,
    pc: u32,
    expected_orig: u32,
    new_insn: u32,
    name: &'static str,
    imm: u32,
) {
    let idx = (pc / 4) as usize;
    // SAFETY: caller of apply_717006_patches has already bounded rom_ptr.
    // The probe always rewrites a code word (the original first
    // instruction → an HVC), so the host read returns the BE numerical
    // value of the instruction directly under BE-8 (write_rom_code_word
    // stores native u32 = LE encoding = BE numerical when fetched LE).
    let prev = unsafe { rom_ptr.add(idx).read() };
    if prev != expected_orig {
        kprintln!(
            "rom_patch: ERROR — {} at {:#010x} is {:#010x}, expected {:#010x}; skipping HVC #{:#x} probe",
            name, pc, prev, expected_orig, imm
        );
        return;
    }
    unsafe { crate::guest_mem::write_rom_code_word(rom_ptr, idx, new_insn); }
    record_original(pc, prev);
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} probe, HVC #{:#x})",
        pc, prev, new_insn, name, imm
    );
}

/// Iter-50: side-table of `(pc, original_instruction)` pairs for ROM
/// PCs that `patch_probe` has overwritten with an HVC. shadow_stub's
/// liveness analyser consults this table via `read_original` so it
/// sees the pre-patch instruction stream — necessary because
/// `apply_717006_patches` runs BEFORE `shadow_stub::patch_rom_from_bitmap`,
/// and without this table the analyser misclassifies probe-HVCs
/// (e.g. picks R12 as scratch_ea at FindSuperceeder body's
/// 0x001488ac because the original `mov r0, ip` at 0x001488c4 has
/// been replaced with HVC #0x6E for the FINDSUPER_MID probe).
///
/// Capacity = 128: comfortably covers the ~70 probes installed today
/// (the soup-index + flash-store + per-stall probes added through
/// iter-89). When the table fills, `record_original` warns and
/// `read_original` returns None for the missing entries, which in turn
/// makes `shadow_stub`'s liveness analyser see the patched HVC instead
/// of the original — leading to subtle scratch-register misanalysis at
/// nearby SBA / unaligned-inline stub sites. Single-threaded boot use,
/// so a plain `static mut` with index counter is safe.
const ORIG_CAP: usize = 128;
static mut ORIG_PCS:    [u32; ORIG_CAP] = [0; ORIG_CAP];
static mut ORIG_INSNS:  [u32; ORIG_CAP] = [0; ORIG_CAP];
static mut ORIG_N:      usize = 0;

fn record_original(pc: u32, orig: u32) {
    // SAFETY: single-threaded boot path; `apply_717006_patches`
    // runs once on core 0 before the guest is ERET'd in. Use raw
    // pointers so we don't trip the rust_2024_compatibility lint
    // about shared/mutable references to a static mut.
    unsafe {
        let n_ptr = core::ptr::addr_of_mut!(ORIG_N);
        let n = n_ptr.read();
        if n >= ORIG_CAP {
            // Silently dropping entries causes shadow_stub's liveness
            // analyser to see the patched HVC instead of the original
            // instruction at this PC, leading to mis-classified scratch
            // registers at nearby SBA / unaligned-inline stub sites and
            // hard-to-diagnose downstream corruption. Bump ORIG_CAP and
            // rebuild rather than letting boot continue with a partial
            // table.
            kprintln!(
                "rom_patch: FATAL — ORIG_PCS table full ({} entries, ORIG_CAP={}) \
                 trying to record PC={:#010x}; bump ORIG_CAP in src/rom_patches.rs",
                n, ORIG_CAP, pc
            );
            crate::cpu::halt();
        }
        let pcs = core::ptr::addr_of_mut!(ORIG_PCS) as *mut u32;
        let insns = core::ptr::addr_of_mut!(ORIG_INSNS) as *mut u32;
        pcs.add(n).write(pc);
        insns.add(n).write(orig);
        n_ptr.write(n + 1);
    }
}

/// Look up the original (pre-patch) instruction at `pc`. Returns
/// `Some(orig)` if `patch_probe` previously rewrote that PC, else
/// `None` — callers fall back to reading current ROM bytes.
pub fn read_original(pc: u32) -> Option<u32> {
    // SAFETY: single-threaded after boot; ORIG_N is monotonic-up
    // and the slots below it are immutable post-patch. Read via raw
    // pointers to satisfy the rust_2024_compatibility static-mut-ref
    // lint without taking shared references to a static mut.
    unsafe {
        let n = core::ptr::addr_of!(ORIG_N).read();
        let pcs = core::ptr::addr_of!(ORIG_PCS) as *const u32;
        let insns = core::ptr::addr_of!(ORIG_INSNS) as *const u32;
        for i in 0..n {
            if pcs.add(i).read() == pc {
                return Some(insns.add(i).read());
            }
        }
    }
    None
}

/// (Previously we patched every `T28F016_SA_SVDriver` method to emit
/// a NATIVE_PRIM(0, subfn) call, short-circuiting the real-Intel-chip
/// protocol the ROM driver speaks against our plain-RAM flash backing.
/// That worked as far as trace 142 but left the ROM's own method
/// prologues half-overwritten, and the write-verify path still
/// rebooted because endianness/lane assumptions didn't line up with
/// what the kernel then read back. The correct fix is to restore the
/// REx-based substitution so the kernel picks Einstein.rex's
/// `TEinsteinFlashDriver` from the 'fdrv' entry — the same mechanism
/// every other Einstein-provided driver uses. That investigation is
/// parked.)
/// Replace the UND-table slots at 0x0038CE6C (DebugStr) and 0x0038CE70
/// (Debugger) with branches to small stubs that stash the guest's LR
/// into r7 and then HVC to EL2. Einstein's callbacks do
/// `SetRegister(15, LR + 4)` for DebugStr and `SetRegister(15, LR + 8)`
/// for Debugger (`Emulator/JIT/Generic/TJITGenericROMPatch.cpp:76-102`);
/// our HVC handler reads the stashed LR (ctx.x[7]) and advances ELR_EL2
/// by the matching delta.
///
/// The MOV/HVC pair doesn't fit inline: 0x0038CE6C and 0x0038CE70 are
/// adjacent entries in the Newton UND-dispatch table, each reachable
/// as an independent BL target, so neither can occupy two words.
unsafe fn apply_debug_patches(rom_ptr: *mut u32) {
    let debug_str_stub_pc = alloc_patch_stub(2, "DebugStr stub");
    let debugger_stub_pc  = alloc_patch_stub(2, "Debugger stub");
    // MOV r7, lr = E1A0_700E ; HVC #imm
    let debugstr_stub: [u32; 2] = [0xE1A0_700E, hvc_insn(DEBUG_STR_HVC_IMM)];
    let debugger_stub: [u32; 2] = [0xE1A0_700E, hvc_insn(DEBUGGER_HVC_IMM)];
    unsafe {
        write_stub_words(rom_ptr, debug_str_stub_pc, &debugstr_stub);
        write_stub_words(rom_ptr, debugger_stub_pc,  &debugger_stub);

        let word = (0x0038_CE6C / 4) as usize;
        let prev = rom_ptr.add(word).read();
        let insn = arm_b(0x0038_CE6C, debug_str_stub_pc);
        crate::guest_mem::write_rom_code_word(rom_ptr, word, insn);
        kprintln!(
            "rom_patch: 0x0038ce6c: {:#010x} -> {:#010x}  (DebugStr → B {:#x}, HVC #{:#x})",
            prev, insn, debug_str_stub_pc, DEBUG_STR_HVC_IMM,
        );
        let word = (0x0038_CE70 / 4) as usize;
        let prev = rom_ptr.add(word).read();
        let insn = arm_b(0x0038_CE70, debugger_stub_pc);
        crate::guest_mem::write_rom_code_word(rom_ptr, word, insn);
        kprintln!(
            "rom_patch: 0x0038ce70: {:#010x} -> {:#010x}  (Debugger → B {:#x}, HVC #{:#x})",
            prev, insn, debugger_stub_pc, DEBUGGER_HVC_IMM,
        );
    }
}

/// Writes a sequence of ARM instruction encodings to the ROM backing.
/// All entries here are code (HVC stub bodies, branch targets, etc.) so
/// they go through `write_rom_code_word`.
unsafe fn write_stub_words(rom_ptr: *mut u32, base: u32, words: &[u32]) {
    unsafe {
        for (i, w) in words.iter().copied().enumerate() {
            let idx = ((base + (i as u32) * 4) / 4) as usize;
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, w);
        }
    }
}

/// Replace RealClockSeconds at 0x00255578 with a 4-word stub that reads
/// the MMIO calendar register (populated by `peripherals::vic::
/// calendar_seconds` via `stage2::tick_page::update`) and returns.
/// Einstein's equivalent is the native-call patch at
/// `TJITGenericROMPatch.cpp:110` that calls host `time()`; we serve the
/// same value from a different layer, so the callback is a simple
/// read-register-then-return.
unsafe fn apply_real_clock_seconds_patch(rom_ptr: *mut u32) {
    const ENTRY: u32 = 0x0025_5578;
    // 0x00255578: LDR r0, [pc, #4]        -- load literal at 0x00255584
    // 0x0025557C: LDR r0, [r0]            -- dereference calendar address
    // 0x00255580: MOV PC, LR              -- return
    // 0x00255584: .word 0x0F181000        -- calendar MMIO IPA
    //
    // First 3 words are ARM instructions (code); the literal at +12 is
    // data that the LDR at +0 loads into r0. Under BE-8 the LDR is
    // byteswapping, so the literal must be written as data (BE-encoded
    // bytes on host).
    let insns: [u32; 3] = [0xE59F_0004, 0xE590_0000, 0xE1A0_F00E];
    let literal: u32 = 0x0F18_1000;
    unsafe {
        for (i, w) in insns.iter().copied().enumerate() {
            let offset = ENTRY + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            let prev = rom_ptr.add(idx).read();
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, w);
            kprintln!(
                "rom_patch: {:#010x}: was_host={:#010x} -> insn={:#010x}  (RealClockSeconds)",
                offset, prev, w,
            );
        }
        let lit_offset = ENTRY + 12;
        let lit_idx = (lit_offset / 4) as usize;
        let prev = rom_ptr.add(lit_idx).read();
        crate::guest_mem::write_rom_data_word(rom_ptr, lit_idx, literal);
        kprintln!(
            "rom_patch: {:#010x}: was_host={:#010x} -> lit={:#010x}  (RealClockSeconds literal)",
            lit_offset, prev, literal,
        );
    }
}

/// FTimeInSeconds injection patch: replace the last shift before the
/// function epilogue (at 0x00089B80, originally `MOV r0, r0, LSL #2`)
/// with a branch to a stub that subtracts `safeIntervalDeltaSeconds`,
/// performs both the callback's `<< 2` and the original instruction's
/// `<< 2` as a single `LSL #4`, then branches back to the epilogue.
/// Einstein's equivalent at `TJITGenericROMPatch.cpp:150`.
unsafe fn apply_ftime_in_seconds_patch(rom_ptr: *mut u32) {
    const PATCH_PC: u32 = 0x0008_9B80;
    const RETURN_PC: u32 = 0x0008_9B84; // original LDMDB epilogue
    let ftime_stub_pc = alloc_patch_stub(5, "FTimeInSeconds stub");
    // Stub body (5 words):
    //   +0x00 LDR r12, [pc, #8]           ; load delta from +0x10
    //   +0x04 SUB r0, r0, r12             ; r0 = r0 - delta
    //   +0x08 MOV r0, r0, LSL #4          ; callback << 2 + original << 2
    //   +0x0C B <RETURN_PC>               ; resume at the epilogue
    //   +0x10 .word safeIntervalDeltaSeconds
    let stub_b = arm_b(ftime_stub_pc + 0x0C, RETURN_PC);
    let stub: [u32; 5] = [
        0xE59F_C008,        // LDR r12, [pc, #8]
        0xE040_000C,        // SUB r0, r0, r12
        0xE1A0_0200,        // MOV r0, r0, LSL #4
        stub_b,             // B RETURN_PC
        SAFE_INTERVAL_DELTA_SECONDS,
    ];
    let patch_insn = arm_b(PATCH_PC, ftime_stub_pc);
    unsafe {
        write_stub_and_patch(rom_ptr, ftime_stub_pc, &stub, PATCH_PC, patch_insn, "FTimeInSeconds");
    }
}

/// FDateFromSeconds injection patch: replace the `MOV r0, sp` at
/// 0x0008A8A8 with a branch to a stub that adds `safeIntervalDeltaSeconds`
/// to r1, re-executes `MOV r0, sp`, and branches to the instruction
/// after the patch site. Einstein's equivalent at
/// `TJITGenericROMPatch.cpp:160`.
unsafe fn apply_fdate_from_seconds_patch(rom_ptr: *mut u32) {
    const PATCH_PC: u32 = 0x0008_A8A8;
    const RETURN_PC: u32 = 0x0008_A8AC; // next instruction after the patched MOV
    let fdate_stub_pc = alloc_patch_stub(5, "FDateFromSeconds stub");
    let stub_b = arm_b(fdate_stub_pc + 0x0C, RETURN_PC);
    let stub: [u32; 5] = [
        0xE59F_C008,        // LDR r12, [pc, #8]
        0xE081_100C,        // ADD r1, r1, r12
        0xE1A0_000D,        // MOV r0, sp (= MOV r0, r13) — original instruction
        stub_b,             // B RETURN_PC
        SAFE_INTERVAL_DELTA_SECONDS,
    ];
    let patch_insn = arm_b(PATCH_PC, fdate_stub_pc);
    unsafe {
        write_stub_and_patch(rom_ptr, fdate_stub_pc, &stub, PATCH_PC, patch_insn, "FDateFromSeconds");
    }
}

/// Replace the first word of `PowerOffAndReboot` (0x000E_6BBC) with a
/// single `HVC #POWEROFF_REBOOT_HVC_IMM`. The handler in
/// `trap::handle_hvc` dumps the calling context (R0 = reboot reason,
/// LR via banked-reg path, mode, ELR) and halts — we never resume.
/// This catches the boot-fail-and-reboot loop the FIRST time it fires
/// instead of seeing 350k repeated tracer entries before timeout.
unsafe fn apply_poweroff_reboot_trap(rom_ptr: *mut u32) {
    let idx = (POWEROFF_REBOOT_PC / 4) as usize;
    let insn = hvc_insn(POWEROFF_REBOOT_HVC_IMM);
    unsafe {
        let prev = rom_ptr.add(idx).read();
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (PowerOffAndReboot canary, HVC #{:#x})",
            POWEROFF_REBOOT_PC, prev, insn, POWEROFF_REBOOT_HVC_IMM,
        );
    }
}

/// Same canary pattern as `apply_poweroff_reboot_trap`, but for the
/// soft-reboot path `Reboot(long, unsigned long, unsigned char)` at
/// 0x000D_9884. UnhandledException → Reboot → ROMBoot is the loop the
/// kernel falls into when an exception isn't caught (observed during
/// StartupProtocolRegistry); catching here reports the reboot reason
/// (R0) immediately rather than letting the second boot cycle mask
/// it.
unsafe fn apply_reboot_trap(rom_ptr: *mut u32) {
    let idx = (REBOOT_PC / 4) as usize;
    let insn = hvc_insn(REBOOT_HVC_IMM);
    unsafe {
        let prev = rom_ptr.add(idx).read();
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (Reboot canary, HVC #{:#x})",
            REBOOT_PC, prev, insn, REBOOT_HVC_IMM,
        );
    }
}

/// Software-reset canary at `BootOS` (0x0001_8688). Overwrite the
/// first word with `HVC #BOOTOS_HVC_IMM`; the handler distinguishes
/// the legitimate first boot from a reset by counting entries. Panics
/// at install time if the current first word isn't the expected
/// `mov r0, #0xb0` (0xE3A000B0) — a ROM change would silently break
/// the emulation path, so we want a loud notification at install.
unsafe fn apply_bootos_trap(rom_ptr: *mut u32) {
    let idx = (BOOTOS_PC / 4) as usize;
    // SAFETY: bounded; patch runs on the main ROM half.
    let prev = unsafe { rom_ptr.add(idx).read() };
    if prev != BOOTOS_ORIG_INSN {
        kprintln!(
            "rom_patch: ERROR — BootOS first word is {:#010x}, expected {:#010x}; skipping canary",
            prev, BOOTOS_ORIG_INSN,
        );
        return;
    }
    let insn = hvc_insn(BOOTOS_HVC_IMM);
    unsafe {
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
    }
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (BootOS canary, HVC #{:#x})",
        BOOTOS_PC, prev, insn, BOOTOS_HVC_IMM,
    );
}

/// Force-per-page stack allocation by re-running `TStackManager::ResolveFault`
/// four times per kernel-side fault, once per 1-KiB subpage of the faulting
/// 4-KiB page.
///
/// **Why.** The 717006 kernel uses ARMv4 subpage-AP to put up to four 1-KiB
/// stacks on a single 4-KiB physical page, with guard subpages set to AP=00 so
/// a stack overrun faults and the kernel can grow lazily. ARMv7 (our hardware)
/// has no subpage-AP support — `fix_stage1_xn_bits` flattens every L2 entry to
/// AP=011 (full RW) so the kernel can run at all, but the side effect is that
/// after the kernel "grows" one subpage, ARMv7 unconditionally exposes the
/// other three subpages of the same page as RW. The kernel never gets the
/// chance to take a fault on those siblings, so its bookkeeping believes only
/// one subpage is allocated while physically all four are accessible.
///
/// **Fix.** Trick the kernel into completing the bookkeeping for all four
/// subpages on the first fault. Insert a thin wrapper at WRAPPER_PC that:
///
///   1. Reads the original FAR from `this->[+64]->[+68]` and saves it.
///   2. Aligns the FAR to the 4-KiB page base.
///   3. Calls the real `ResolveFault` four times, each time with the FAR
///      pointing at one of the four 1-KiB subpages of that page (offsets
///      `0x000`, `0x400`, `0x800`, `0xC00`).
///   4. Restores the original FAR and returns.
///
/// The first call hits the first-allocation path (page slot is null →
/// `FindOrAllocPage(mask=1<<0)` allocates the page and assigns subpage 0 to
/// this stack). The next three hit the existing-page path with `owner==NULL`
/// and call `SetSubPageInfo` to assign subpages 1/2/3. The full success-tail
/// (SetRestrictedPage / RememberMappings / refcount bump) runs four times —
/// once per subpage — so the kernel's per-subpage bookkeeping matches the
/// physical reality that all four subpages of the page are now accessible.
///
/// Replaces an earlier set of three `mov r3, #0xF` patches at the
/// `bl FindOrAllocPage` call sites in ResolveFault. Those patches forced
/// each FindOrAllocPage call to claim all four subpages via `PageMatchFound`
/// with `mask=0xF`, but only updated subpage *ownership* — the other
/// per-subpage bookkeeping (refcount0/1, RememberMappings entries) was still
/// done for only the faulting subpage. Under sustained allocation pressure
/// (TInterpreter ctor's 256 KiB of lazy stacks) the kernel's allocator
/// drifted into a state where `Remember` on a fresh L1 lazy section returned
/// an unhandled error, propagating to `Reboot(-10075)`. The wrapper approach
/// avoids that drift by literally running the kernel's full per-subpage path
/// four times, making the kernel's view of "which subpages got allocated"
/// agree with what ARMv7 has actually exposed.
unsafe fn apply_resolve_fault_wrapper(rom_ptr: *mut u32) {
    // ARM AArch32 wrapper code at WRAPPER_PC. 23 words = 92 bytes.
    //
    // Important register choices:
    //   - r10 (sl) holds the sub_idx loop counter. AAPCS preserves r4-r11
    //     across `bl`, while r12 (ip) is intra-procedure scratch and may
    //     be clobbered by ResolveFault. Using r12 as the counter would
    //     read garbage after the bl and break the loop.
    //
    // The page boundary must be computed relative to `info->base_va`
    // (= info->[+20]) — adjacent stack slots in `FMNewStack` are placed
    // 33 KiB apart, so `info->base_va` is *not* 4-KiB-aligned in general.
    // Aligning the FAR to a host 4-KiB boundary would cross a stack-region
    // boundary and trip ResolveFault's bound check (returning -10203
    // "out of range below" for a sub-page that lives in the previous
    // stack's slot).
    //
    // Layout (offsets from WRAPPER_PC):
    //   +0x00  push  {r4-r10, lr}
    //   +0x04  mov   r4, r0                    ; r4 = TStackManager*
    //   +0x08  mov   r5, r1                    ; r5 = TStackInfo*
    //   +0x0c  ldr   r6, [r0, #64]             ; r6 = ProcessorState*
    //   +0x10  ldr   r8, [r6, #68]             ; r8 = original FAR (for restore)
    //   +0x14  ldr   r9, [r5, #20]             ; r9 = info->base_va
    //   +0x18  sub   r7, r8, r9                ; r7 = orig_FAR - base = offset
    //   +0x1c  mov   r7, r7, lsr #12           ; round down to 4-KiB-page within the stack
    //   +0x20  mov   r7, r7, lsl #12
    //   +0x24  add   r7, r9, r7                ; r7 = info->base_va + page_offset
    //                                          ;     = page_base_FAR (subpage 0 of this page)
    //   +0x28  mov   r10, #0                   ; r10 = sub_idx counter (callee-saved across bl)
    //   +0x2c  add   r0, r7, r10, lsl #10      ; r0 = page_base_FAR + sub*1024
    //   +0x30  str   r0, [r6, #68]             ; FAR = page_base_FAR + sub*1024
    //   +0x34  mov   r0, r4                    ; r0 = TStackManager*
    //   +0x38  mov   r1, r5                    ; r1 = TStackInfo*
    //   +0x3c  bl    ResolveFault              ; original kernel function
    //   +0x40  cmp   r0, #0
    //   +0x44  bne   done                      ; on error: skip remaining iterations
    //   +0x48  add   r10, r10, #1
    //   +0x4c  cmp   r10, #4
    //   +0x50  blt   iter (back to +0x2c)
    //   +0x54  mov   r0, #0                    ; reach done with r0=0 (success)
    //   +0x58  done: str r8, [r6, #68]         ; restore original FAR
    //   +0x5c  pop   {r4-r10, pc}
    //
    // NOTE on iter return codes: stock ResolveFault returns -10203 /
    // -10204 if the FAR we passed is out of the stack's [info[24], info[28])
    // range. For an edge-page FAR aligned to subpage 0 of the page, sub
    // indices below info[24] (= the kernel's actual stack lower bound,
    // not the page boundary computed from info[20]) generate -10203.
    // We treat those as "subpage belongs to another stack — skip" and
    // only propagate r0==4 (FindOrAllocPage failure) to the caller.
    let resolve_fault_wrapper_pc = alloc_patch_stub(24, "ResolveFault wrapper");
    let bl_pc = resolve_fault_wrapper_pc + 0x3C;
    let stub: [u32; 24] = [
        0xE92D_47F0,                            // +0x00 push {r4-r10, lr}
        0xE1A0_4000,                            // +0x04 mov r4, r0
        0xE1A0_5001,                            // +0x08 mov r5, r1
        0xE590_6040,                            // +0x0c ldr r6, [r0, #64]
        0xE596_8044,                            // +0x10 ldr r8, [r6, #68]
        0xE595_9014,                            // +0x14 ldr r9, [r5, #20]
        0xE048_7009,                            // +0x18 sub r7, r8, r9
        0xE1A0_7627,                            // +0x1c mov r7, r7, lsr #12
        0xE1A0_7607,                            // +0x20 mov r7, r7, lsl #12
        0xE089_7007,                            // +0x24 add r7, r9, r7
        0xE3A0_A000,                            // +0x28 mov r10, #0
        0xE087_050A,                            // +0x2c add r0, r7, r10, lsl #10
        0xE586_0044,                            // +0x30 str r0, [r6, #68]
        0xE1A0_0004,                            // +0x34 mov r0, r4
        0xE1A0_1005,                            // +0x38 mov r1, r5
        arm_bl(bl_pc, RESOLVE_FAULT_PC),        // +0x3c bl ResolveFault
        0xE350_0000,                            // +0x40 cmp r0, #0
        0x1A00_0003,                            // +0x44 bne done — propagate ANY error (was beq on r0==4 only)
        0xE28A_A001,                            // +0x48 add r10, r10, #1
        0xE35A_0004,                            // +0x4c cmp r10, #4
        0xBAFF_FFF5,                            // +0x50 blt iter (offset -11 words from PC+8)
        0xE3A0_0000,                            // +0x54 mov r0, #0  (all iters done → success)
        0xE586_8044,                            // +0x58 done: str r8, [r6, #68]
        0xE8BD_87F0,                            // +0x5c pop {r4-r10, pc}
    ];
    unsafe {
        for (i, w) in stub.iter().copied().enumerate() {
            let offset = resolve_fault_wrapper_pc + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, w);
        }

        // Patch the `bl ResolveFault` site inside `Fault` (0x001f84e0).
        let idx = (FAULT_BL_RESOLVE_PC / 4) as usize;
        let prev = rom_ptr.add(idx).read();
        let insn = arm_bl(FAULT_BL_RESOLVE_PC, resolve_fault_wrapper_pc);
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (Fault → ResolveFaultWrapper @{:#x})",
            FAULT_BL_RESOLVE_PC, prev, insn, resolve_fault_wrapper_pc,
        );

        // (FMLockHeapRange BL not patched — covers early bring-up paths
        // that allocate single physical pages eagerly, where
        // multi-iter would over-claim and break boot. We tried wrapping
        // it 2026-04-28; the wrapper's "swallow -10203" behaviour
        // masks legitimate out-of-bounds errors from FMLockHeapRange's
        // range-iteration, causing the kernel to retry forever.
        // See INVESTIGATION.md for that diagnostic run.)
        let _ = FMLOCK_BL_RESOLVE_PC;

    }
}

/// Install the per-stack 4 KiB padding wrapper (Option A in PLAN.md).
/// `TTask::Init`'s `bl NewStack` at `0x0025238c` is the upstream
/// caller for 11 of 12 Group-2 verify-mmu aliases. Each adjacent
/// stack pair shares a 4 KiB boundary page (ARMv4 subpage-AP-era
/// design); ARMv7 has no subpage AP, so under our flat AP=011 the
/// boundary is a real PA alias. Adding 4 KiB to every stack
/// allocation request bumps the kernel's per-domain "next stack
/// VA" pointer by an extra page on each call, eliminating the
/// shared-boundary pattern.
///
/// Wrapper layout at `NEW_STACK_PAD_WRAPPER_PC` (2 words = 8 B):
///   +0x00  add r1, r1, #4096   — pad the size argument
///   +0x04  b   <NewStack thunk> — tail-call into the kernel function
///
/// The original BL site's link register (set by the caller's BL
/// to the wrapper) is preserved unchanged across the tail-call
/// `b`, so when NewStack eventually returns, control flows back
/// to TTask::Init's post-BL site (0x00252390) as if the call had
/// gone directly.
unsafe fn apply_new_stack_pad_wrapper(rom_ptr: *mut u32) {
    let wrapper_pc = alloc_patch_stub(2, "NewStack pad wrapper");
    // ARM `add r1, r1, #4096` (= 0xE2811A01) — imm12 with rot=0xA,
    // imm8=0x01 → ROR(0x01, 20) = 0x1000.
    let add_r1_4k = 0xE281_1A01u32;
    let b_target  = arm_b(wrapper_pc + 4, NEW_STACK_THUNK_PC);
    let stub: [u32; 2] = [add_r1_4k, b_target];
    unsafe {
        for (i, w) in stub.iter().copied().enumerate() {
            let offset = wrapper_pc + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, w);
        }

        // Patch TTask::Init's BL to point at the wrapper instead of
        // directly at the post-ship NewStack thunk.
        let idx = (NEW_STACK_PAD_BL_PC / 4) as usize;
        let prev = rom_ptr.add(idx).read();
        if prev != NEW_STACK_PAD_BL_ORIG_INSN {
            kprintln!(
                "rom_patch: ERROR — TTask::Init bl NewStack site at {:#010x} is {:#010x}, expected {:#010x}; skipping per-stack pad wrapper",
                NEW_STACK_PAD_BL_PC, prev, NEW_STACK_PAD_BL_ORIG_INSN,
            );
            return;
        }
        let new_bl = arm_bl(NEW_STACK_PAD_BL_PC, wrapper_pc);
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, new_bl);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (TTask::Init bl NewStack → +4 KiB pad wrapper @{:#x})",
            NEW_STACK_PAD_BL_PC, prev, new_bl, wrapper_pc,
        );
    }
}

/// Install the LockHeapRange / UnlockHeapRange entry-point wrappers
/// that round (base, limit) to 4-KiB boundaries before the original
/// function body runs. See the constant doc-comments at
/// `LOCK_HEAP_RANGE_PC` for the full rationale.
///
/// Wrapper layout (9 words = 36 bytes per stub):
///   +0x00  mov  ip, sp                ; replicate patched-out 1st insn
///   +0x04  lsr  r0, r0, #12           ; r0 = base & ~0xFFF (align down)
///   +0x08  lsl  r0, r0, #12
///   +0x0c  sub  r1, r1, #1            ; r1 = end_inclusive (orig limit-1)
///   +0x10  orr  r1, r1, #0xC00        ; |= bits 10-11 (subpage 3 marker)
///   +0x14  orr  r1, r1, #0x3F0        ; |= bits 4-9
///   +0x18  orr  r1, r1, #0x0F         ; |= bits 0-3
///   ;      r1 is now (end_inclusive | 0xFFF) — last byte of the 4-KiB
///   ;      page that contains the caller's original end.
///   +0x1c  add  r1, r1, #1            ; restore "limit" form (one past end)
///   +0x20  b    <function entry>+4    ; rejoin at second instruction
///
/// The 0xFFF mask is split into 0xC00 + 0x3F0 + 0x0F because ARMv7's
/// 8-bit-rotated immediate encoding can't represent 0xFFF in a single
/// op. The three pieces are individually encodable:
///   - 0xC00: imm8=0x0C, rot_imm=12 → ROR(0x0C, 24) = 0xC00
///   - 0x3F0: imm8=0x3F, rot_imm=14 → ROR(0x3F, 28) = 0x3F0
///   - 0x0F:  imm8=0x0F, rot_imm=0  → 0x0F
///
/// **Why both LockHeapRange AND UnlockHeapRange are patched.** The
/// kernel's lock/unlock invariant requires the unlock range to match
/// the lock range exactly. If we widen the lock to 4 KiB but leave
/// the unlock at 1-KiB granularity, the extra subpages stay locked
/// indefinitely. Symmetric patching keeps the kernel's per-subpage
/// refcounts balanced.
unsafe fn apply_lock_heap_range_wrapper(rom_ptr: *mut u32) {
    let stub_template: [u32; 9] = [
        0xE1A0_C00D,                                                    // +0x00 mov ip, sp
        0xE1A0_0620,                                                    // +0x04 lsr r0, r0, #12
        0xE1A0_0600,                                                    // +0x08 lsl r0, r0, #12
        0xE241_1001,                                                    // +0x0c sub r1, r1, #1
        0xE381_1C0C,                                                    // +0x10 orr r1, r1, #0xC00
        0xE381_1E3F,                                                    // +0x14 orr r1, r1, #0x3F0
        0xE381_100F,                                                    // +0x18 orr r1, r1, #0x0F
        0xE281_1001,                                                    // +0x1c add r1, r1, #1
        0,                                                              // +0x20 b function+4 (filled in)
    ];

    for (wrapper_pc, fn_pc, name) in [
        (alloc_patch_stub(9, "LockHeapRange wrapper"),   LOCK_HEAP_RANGE_PC,   "LockHeapRange"),
        (alloc_patch_stub(9, "UnlockHeapRange wrapper"), UNLOCK_HEAP_RANGE_PC, "UnlockHeapRange"),
    ] {
        let mut stub = stub_template;
        stub[8] = arm_b(wrapper_pc + 0x20, fn_pc + 4);

        unsafe {
            for (i, w) in stub.iter().copied().enumerate() {
                let idx = ((wrapper_pc + (i as u32) * 4) / 4) as usize;
                crate::guest_mem::write_rom_code_word(rom_ptr, idx, w);
            }

            let fn_idx = (fn_pc / 4) as usize;
            let prev = rom_ptr.add(fn_idx).read();
            if prev != LOCK_UNLOCK_ORIG_FIRST_INSN {
                kprintln!(
                    "rom_patch: ERROR — {} first word is {:#010x}, expected {:#010x}; \
                     skipping 4-KiB wrapper",
                    name, prev, LOCK_UNLOCK_ORIG_FIRST_INSN,
                );
                continue;
            }
            let branch = arm_b(fn_pc, wrapper_pc);
            crate::guest_mem::write_rom_code_word(rom_ptr, fn_idx, branch);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} → 4-KiB wrapper @{:#x})",
                fn_pc, prev, branch, name, wrapper_pc,
            );
        }
    }
}

/// Shared helper for the two injection patches: write a 5-word stub at
/// `stub_pc` (4 instruction words + 1 trailing data literal) and a
/// 1-word branch at `patch_pc`. The first 4 stub words are written as
/// code (native LE u32 for BE-8 fetch), the 5th word is written as
/// data (byteswapped on host so a BE-8 LDR reads back the literal).
unsafe fn write_stub_and_patch(
    rom_ptr: *mut u32,
    stub_pc: u32,
    stub: &[u32; 5],
    patch_pc: u32,
    patch_insn: u32,
    name: &'static str,
) {
    unsafe {
        for (i, w) in stub.iter().copied().enumerate() {
            let offset = stub_pc + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            if i < 4 {
                crate::guest_mem::write_rom_code_word(rom_ptr, idx, w);
            } else {
                crate::guest_mem::write_rom_data_word(rom_ptr, idx, w);
            }
        }
        let idx = (patch_pc / 4) as usize;
        let prev = rom_ptr.add(idx).read();
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, patch_insn);
        kprintln!(
            "rom_patch: {:#010x}: was_host={:#010x} -> {:#010x}  ({}: B {:#x}, 5-word stub)",
            patch_pc, prev, patch_insn, name, stub_pc,
        );
    }
}

// Rust-side tests would live here, but this crate is `no_std` (it
// defines its own `#[panic_handler]`), so `cargo test` can't link
// the built-in test crate. Verification happens via
// `guest-tests/tests/test_rom_patches.S` (HVC-handler behaviour) and
// the real-ROM boot path (which exercises every patch the Newton
// kernel reaches).
