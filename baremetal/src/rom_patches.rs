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

use crate::hvc_imm::HvcImm;
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
pub const PATCH_STUB_ARENA_BASE: u32 = 0x00FF_FD80;
pub const PATCH_STUB_ARENA_END:  u32 = 0x00FF_FEC0;

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
    // SWIBoot's second instruction-as-data LDR at 0x003ad738 is
    // patched separately, in `apply_fault_handler_ldr_byteswap_patches`,
    // as a B-to-stub. Iter-102 had this as `mov r1, r0` on the
    // assumption that r0 still carried the byteswap-corrected SWI
    // word from the iter-101 stub at 0x003ad69c — true for
    // unconditional SVCs, but the conditional-SVC dispatcher at
    // 0x003add7c does `mrs r0, SPSR`, clobbering r0 with the
    // caller's CPSR. The downstream `mov r1, r0; bic r1, r1,
    // #0xFF000000; cmp r1, #0x23` then sees CPSR-shaped garbage
    // (low 24 bits include the mode field), the bge fires, and
    // boot wedges in the "Undefined SWI" debug stub. The fix is a
    // proper LDR-byteswap stub mirroring the iter-101 site so the
    // re-read works for conditional SVCs too.
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
    // TStackManager: 4 KiB-only stack/heap allocation.
    //
    // The kernel was compiled for ARMv4, where each L2 PTE for a 4 KiB
    // small page held four 2-bit AP fields — one per 1 KiB sub-page —
    // so the MMU enforced read/write/no-access independently on each
    // 1 KiB quarter. TStackManager exploits that: stack/heap "areas"
    // are 33 KiB (= 8 full pages + 1 sub-page), and four consecutive
    // areas share their boundary 4 KiB page, each owning one sub-page
    // of it.
    //
    // ARMv7+ small-page PTEs only have whole-page AP[2:1]; the
    // sub-page AP bits are gone. `fix_stage1_xn_bits` flattens to
    // AP=011, so every shared boundary page becomes whole-page RW for
    // all four owners. A write past the end of one stack's sub-page
    // spills into the heap or stack on the adjacent sub-page of the
    // same physical page.
    //
    // Fix: stop sharing pages, AND restore stack-overflow detection.
    // Four coupled changes accomplish this:
    //   (A) Bump the area stride from 33 KiB to 36 KiB (= 9 full
    //       pages). Areas are now 4 KiB-aligned and 4 KiB-multiple,
    //       so consecutive areas use disjoint pages. 36 KiB (rather
    //       than 32 KiB) gives each area 32 KiB usable above a 4 KiB
    //       guard page — boot scripts run NewtonScript code that
    //       recurses ~50 levels deep through the interpreter and
    //       genuinely use ~32 KiB of stack; the 32 KiB-stride variant
    //       of this patch left the deepest leaf with no headroom.
    //   (B) The compiled per-area "page-aligned base" formula has a
    //       sub-page staircase term that, for the 33 KiB stride, lets
    //       four consecutive 33 KiB areas pack into 33 KiB worth of
    //       4 KiB-aligned pages. With the 36 KiB stride each area is
    //       already 4 KiB-aligned, so the staircase becomes just
    //       `slot * 36864`. The compiler's two-`add` chain produces
    //       this naturally if we drop the divide-by-4 — see below.
    //   (C) Make every fault claim the whole 4 KiB page, not just the
    //       1 KiB sub-page that took the fault. Otherwise the
    //       matching cache in the page-pool would still hand a
    //       partially-owned page from one allocator to another,
    //       mapping the same physical page at two unrelated VAs —
    //       the same page-alias bug, in a different layer.
    //   (D) Bump kGuardBandSize from 3 KiB to 4 KiB so the kernel's
    //       guard region is exactly the bottom 4 KiB page of each
    //       area. The original kernel relied on per-sub-page AP=NA
    //       to make the bottom 3 sub-pages of an area's first page
    //       fault on access — ARMv4 hardware enforced the guard.
    //       ARMv7 has no per-sub-page AP, so we instead make the
    //       guard a *whole-page* hole: the lock-pass at task setup
    //       starts at `bottomOfStack = norm + 4 KiB`, leaving the
    //       bottom page never claimed and never `Remember`ed. Stack
    //       overflow into the bottom page takes a TLB miss, routes
    //       to the same area's TStackInfo, hits ResolveFault's
    //       `FAR < fLowerBounds` branch, returns -10203, and throws
    //       busError. Reliable detection without sub-page AP.
    //
    // The bookkeeping is still per-sub-page; the underlying MMU just
    // collapses everything to whole-page granularity, which is what
    // we want now that no page is ever shared. FMLockHeapRange's
    // per-1 KiB iteration redundantly calls ResolveFault four times
    // per page (each call increments one sub-page's lock count),
    // still correct; FreeSubPagesBetween's per-sub-page sweeps
    // similarly operate at finer granularity than the MMU honours,
    // harmlessly.
    //
    // ARM immediate encoding: #36864 = imm12 0xA09 (rot=A, imm8=9 →
    // ROR(9, 20) = 0x9000). The original ROM uses a different
    // canonical encoding for #33792 (imm12 0xB21) — both forms are
    // valid; we use the assembler's canonical output for #36864.
    //
    // Sites in CheckHeap / VetHeap (0x0027_1Exx) and SaveCPUStateAndStop
    // (0x0001_8F8C, 0x0001_8FA4, 0x0001_90EC) are NOT patched —
    // CheckHeap may not run in our boot path, and the SaveCPUState
    // sites use 0xC008400 as a fixed kernel-globals offset unrelated
    // to per-task area stride.

    // (A) FMNewStack — area stride 33792 → 36864.
    //
    // Three udiv-by-stride sites (`mov r0, #stride; bl __rt_udiv`),
    // two compare-against-stride sites (`cmp r0, #stride`), one
    // clamp value (`mov r7, #stride`), and three multiply-by-stride
    // sites.
    //
    // The multiply-by-stride was emitted as `r * 33` (`add r,r,r,lsl
    // #5`) followed by `lsl #10` to land at `r * 33792`. With the new
    // stride 36864 = 9 * 4096, the same shape becomes `r * 9` (`add
    // r,r,r,lsl #3`) followed by `lsl #12`.
    RomPatch { offset: 0x001F_8EDC, value: 0xE3A0_7A09, name: "FMNewStack: mov r7, #36864 (clamp value)" },
    RomPatch { offset: 0x001F_8EF0, value: 0xE240_1A01, name: "FMNewStack: sub r1, r0, #4096 (was 3072; guard 3K → 4K)" },
    RomPatch { offset: 0x001F_8F18, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, request-addr path)" },
    RomPatch { offset: 0x001F_8F20, value: 0xE080_0180, name: "FMNewStack: add r0, r0, r0, lsl #3 (was lsl #5; *33 → *9)" },
    RomPatch { offset: 0x001F_8F24, value: 0xE049_0600, name: "FMNewStack: sub r0, r9, r0, lsl #12 (was lsl #10; *1024 → *4096)" },
    RomPatch { offset: 0x001F_8F30, value: 0xE280_0A01, name: "FMNewStack: add r0, r0, #4096 (was 3072; maxSize += 4K guard, request-addr path)" },
    RomPatch { offset: 0x001F_8F38, value: 0xE350_0A09, name: "FMNewStack: cmp r0, #36864 (clamp, request-addr path)" },
    RomPatch { offset: 0x001F_8F48, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, request-addr path)" },
    RomPatch { offset: 0x001F_8F5C, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, request-addr path)" },
    RomPatch { offset: 0x001F_8F88, value: 0xE280_0A01, name: "FMNewStack: add r0, r0, #4096 (was 3072; maxSize += 4K guard, any-addr path)" },
    RomPatch { offset: 0x001F_8F90, value: 0xE350_0A09, name: "FMNewStack: cmp r0, #36864 (clamp, any-addr path)" },
    RomPatch { offset: 0x001F_8FA0, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, any-addr path)" },
    RomPatch { offset: 0x001F_9024, value: 0xE08A_118A, name: "FMNewStack: add r1, sl, sl, lsl #3 (was lsl #5; *33 → *9, top-of-area)" },
    RomPatch { offset: 0x001F_902C, value: 0xE080_9601, name: "FMNewStack: add r9, r0, r1, lsl #12 (was lsl #10; *1024 → *4096, top-of-area)" },
    RomPatch { offset: 0x001F_9030, value: 0xE087_0187, name: "FMNewStack: add r0, r7, r7, lsl #3 (was lsl #5; *33 → *9, area-base)" },
    RomPatch { offset: 0x001F_9034, value: 0xE049_0600, name: "FMNewStack: sub r0, r9, r0, lsl #12 (was lsl #10; *1024 → *4096, area-base)" },
    RomPatch { offset: 0x001F_9038, value: 0xE280_2A01, name: "FMNewStack: add r2, r0, #4096 (was 3072; bottomOfStack = norm + 4K, page-aligned)" },

    // (B) FMNewStack — drop the divide-by-4 in the page-aligned-base
    // formula so the existing two-`add` chain produces
    // `base + slot * 36864`.
    //
    // The original formula at 0x001F_9060..0x001F_906C is:
    //   addmi r0, r0, #3            ; round-to-zero for signed /4
    //   asr   r0, r0, #2            ; r0 = slot / 4
    //   add   r0, r1, r0, lsl #12   ; r0 = base + (slot/4)*4096
    //   add   r0, r0, r6, lsl #15   ; r0 = ... + slot*32768
    // The staircase yields a 33 KiB-stride layout: every fourth slot
    // starts on a fresh page, the in-betweens straddle the boundary
    // page. With slot stride = 9 pages (= 36864), every slot starts
    // page-aligned, and the formula simplifies to slot * 9 * 4096.
    // NOPing the addmi+asr leaves r0 = slot going into the third
    // instruction; the third then computes `base + slot*4096`, the
    // fourth adds `slot*32768`, total `base + slot*(4096+32768)
    // = base + slot*36864`. Two patches; the existing two `add`
    // instructions provide the multiplication unchanged.
    RomPatch { offset: 0x001F_9060, value: 0xE1A0_0000, name: "FMNewStack: nop (was addmi r0, r0, #3 — drop /4)" },
    RomPatch { offset: 0x001F_9064, value: 0xE1A0_0000, name: "FMNewStack: nop (was asr r0, r0, #2 — drop /4)" },

    // (A continued) Heap-domain helpers — same stride change.
    //
    // Init__11THeapDomain at 0x001F_8D74 is intentionally NOT patched.
    // It constructs the slot-info array for both stack pools and
    // regular data heaps; sizing it with the larger stride would
    // UNDER-size the array, breaking heap growth past the 33 KiB-
    // sized array's index range. The unpatched 33 KiB divisor
    // OVER-sizes the array for the new stride — wasted memory but
    // functionally safe.
    RomPatch { offset: 0x001F_8E1C, value: 0xE3A0_0A09, name: "THeapDomain::GetStackInfo: mov r0, #36864 (slot-index divisor)" },
    RomPatch { offset: 0x001F_918C, value: 0xE3A0_0A09, name: "FMFree: mov r0, #36864 (slot-index divisor)" },

    // (C) ResolveFault — every fault claims the whole 4 KiB page.
    //
    // The compiled code computes
    //   nSubPage  = (FAR - area_base) >> 10
    //   pageIdx   = nSubPage >> 2
    //   subIdx    = nSubPage & 3
    //   bitmap    = 1 << subIdx
    // and passes `bitmap` as the requested-sub-page mask to
    // FindOrAllocPage, which forwards it to GetMatchingPage. The
    // matcher only accepts a candidate page if every requested
    // sub-page is free, then assigns those sub-pages to the faulting
    // owner. Forcing `bitmap = 0xF` makes the matcher accept only
    // fully-free pages and assign all four sub-pages at once — no
    // two owners can ever share a 4 KiB page after this.
    //
    // `subIdx` is left valid downstream so the sub-page lock-count
    // tail still records the right index for FMLockHeapRange's
    // per-1-KiB lock loop.
    RomPatch { offset: 0x001F_7A0C, value: 0xE3A0_000F, name: "ResolveFault: mov r0, #15 (whole-page bitmap)" },
    RomPatch { offset: 0x001F_7A10, value: 0xE1A0_3000, name: "ResolveFault: mov r3, r0 (drop sub-idx shift)" },
];

// (HVC immediates live in `crate::hvc_imm::HvcImm`.)

/// Phase-B canary: PowerOffAndReboot at 0x000E_6BBC. The kernel calls
/// this whenever a fatal init-time check fails (e.g. flash chip
/// identification yields no driver match — see INVESTIGATION.md).
/// Patch the first word with `HVC #HvcImm::LoudHalt` so we halt
/// loudly the FIRST time it fires, with the caller's R0 (reboot
/// reason) and the trace context immediately preceding the call.
pub const POWEROFF_REBOOT_PC: u32 = 0x000E_6BBC;

/// Phase-B canary: `Reboot(long, unsigned long, unsigned char)` at
/// 0x000D_9884. This is the "soft-reboot" path the kernel's exception
/// unwinder calls on an UnhandledException. Same shape as
/// PowerOffAndReboot: patch the first word to `HVC #HvcImm::LoudHalt`
/// so we halt on the first hit with the caller's R0 = reboot reason.
pub const REBOOT_PC: u32 = 0x000D_9884;

/// Phase-B canary: `StopImage` at 0x0038_D174. The kernel reaches
/// StopImage on idle/sleep — it spins in a wait-for-interrupt loop
/// at 0x38d1d4..0x38d1dc until a wake-up bit lands in IntPresent.
/// During diagnostic runs we don't want to spin there forever; patch
/// the first word with `HVC #HvcImm::LoudHalt` so the host stops
/// immediately on entry.
pub const STOP_IMAGE_PC: u32 = 0x0038_D174;

/// Phase-B busError-throw probe: `bl Throw` inside `TStackManager::Fault`
/// at 0x001F_8534. Reached when ResolveFault returned an error code
/// other than 0 / 4 (i.e. `-10203`/`-10204` "FAR out of stack range").
/// The kernel's path is: build exBusError args (r0=&exBusError, r1=info,
/// r2=0) and `bl Throw`. We replace the `bl Throw` with `HVC #LoudHalt`
/// so we capture r0/r1/r5/FAR + caller_lr at the throw site, before the
/// C++ unwinder swallows the context.
pub const BUS_ERROR_THROW_PC: u32 = 0x001F_8534;

/// Phase-B canary: `BootOS` / `ROMBoot` at 0x0001_8688. The AArch32
/// reset vector at VA 0 is `B 0x18688`, so the first execution after
/// the hypervisor's ERET-to-guest lands here. Any subsequent entry is
/// a SOFTWARE RESET. Canary: patch the first word to
/// `HVC #HvcImm::BootOs`; the handler allows the first entry through
/// by emulating the original first insn (`mov r0, #0xb0`) and then
/// halts on every subsequent entry.
pub const BOOTOS_PC: u32 = 0x0001_8688;
/// The original first instruction of `BootOS`: `mov r0, #0xb0`
/// (0xE3A000B0). The HVC handler emulates this on the legitimate
/// first boot by setting r0 = 0xb0 and advancing ELR past the HVC.
pub const BOOTOS_ORIG_INSN: u32 = 0xE3A0_00B0;

// ---- Load-bearing HVC patches ---------------------------------------------
//
// The originally-Phase-B set (L1[0xCD] lazy-grow probes, DAH Layer-γ
// trio, iter-108 splash chain, FPE-entry, ResolveFault probes) has
// been swept. What remains are HVC patches the running hypervisor
// depends on: the Remember post-SWI sentinel reload, the QEMU DAH
// `mrs r1, SPSR` workaround, the Unhandled[NonUserMode]Exception
// halt tripwires, and the PHammerOutTranslator body redirects.

/// `Remember` post-SWI fixup site at 0x00258E50 (after the first
/// `bl GenericSWI`). Handler logs r0 (= the SWI #12 return value),
/// then emulates `mov r8, #237` so the kernel's `r8 = -10003`
/// constant is restored before the `teq` at 0x00258E58. Patched with
/// `HVC #HvcImm::RememberSwiret`.
const REMEMBER_SWIRET_PC:            u32 = 0x0025_8E50;
const REMEMBER_SWIRET_ORIG_INSN:     u32 = 0xE3A0_80ED; // mov r8, #237

/// `mrs r1, SPSR` at DAH entry (4th instruction past the function
/// label, after the DACR setup). Original encoding `0xE14F_1000`. We
/// replace it with `HVC #HvcImm::DahMrsSpsr` so the EL2 handler can
/// supply the architecturally-correct SPSR_abt from the trampoline-
/// saved slot, working around QEMU raspi3b's stale `mrs spsr_abt`.
/// On FVP the trampoline-saved value matches what `mrs r1, SPSR`
/// would have returned, so this patch is functionally a no-op there.
/// Mirrors docs/QEMU_BUGS.md Bug #1's banked-LR workaround.
pub const DAH_MRS_SPSR_PC:           u32 = 0x0039_3144;
const DAH_MRS_SPSR_INSN:             u32 = 0xE14F_1000;


/// `UnhandledException(char* name, void* data, void(*handler)(void*))`
/// at ROM `0x000B_0220`. The first arg `r0` is a pointer to the
/// exception name as an ASCII string (e.g. "evt.ex.abt.perm" for a
/// permission DABT). Patching the entry with HVC and dumping the
/// name string directly is the right wedge tripwire — far cleaner
/// than chasing the downstream Reboot canary and decoding the
/// stack-passed string.
pub const UNHANDLED_EXCEPTION_PC:         u32 = 0x000B_0220;
const UNHANDLED_EXCEPTION_FIRST_INSN:     u32 = 0xE1A0_C00D; // mov ip, sp

/// `UnhandledNonUserModeException(char*, void*, void(*)(void*))` at
/// ROM `0x000B_031C`. Same signature as `UnhandledException` but
/// invoked from non-USR contexts (UND/SVC/ABT). Mirrors the
/// previous probe so we catch both paths at entry.
pub const UNHANDLED_NUM_EXCEPTION_PC:      u32 = 0x000B_031C;
const UNHANDLED_NUM_EXCEPTION_FIRST_INSN:  u32 = 0xE1A0_C00D; // mov ip, sp

// ---- POutTranslator hook: PHammerOutTranslator concrete-body patches
//
// `gNewtConfig` is patched to `0x8202` (kEnableListener|kDefaultStdioOn|
// kEnableStdout), so `InitREPOut__Fv` (0x12aa44) takes the listener
// branch and stores a `PHammerOutTranslator*` in `gREPout`. Every
// kernel debug print (`REPprintf`, `REPStackTrace`, `REPExceptionNotify`,
// the `printf` jump-table entry, ad-hoc kernel diags, plus TInterpreter
// trace events when the `ns_trace` gate is open) eventually reaches
// the abstract-base thunks at `0x0038_9EA0..EF4` which vtable-dispatch
// into PHammerOutTranslator's concrete methods. Stock those methods
// hand bytes off to a `vfprintf`/`fputc` chain whose stream nobody
// drains, so the bytes vanish.
//
// We replace the body of each method with an `HVC` that forwards args
// to `rep_print` (which renders via `kprintln!` to the EL2 UART) plus
// a small return tail. The dispatch architecture is untouched —
// `gREPout->Print(fmt, ...)` still goes through the natural
// abstract-base thunk and concrete-subclass vtable lookup; we are
// merely the implementation. (Earlier iterations patched the abstract
// base's vtable thunks at 0x389EA0..EF4 with `HVC` and emulated the
// thunk's first `LDR` from EL2; that hack is gone.)
//
// For `Print`/`Putc`/`Flush` the body is overwritten with three words:
// `HVC #imm`, `mov r0, #0`, `mov pc, lr`. The handler renders, ELR
// advances by 4, the natively-executing `mov r0, #0; mov pc, lr`
// returns 0 to the caller. Original body bytes beyond word 2 are
// dead.
//
// For `StackTrace`/`ExceptionNotify` the original body is just
// `mov r0, r1; b REP*` (8 bytes). We patch only word 0, replacing
// `mov r0, r1` with `HVC`. The handler emulates `mov r0, r1`
// (`ctx.x[0] = ctx.x[1]`) before ELR advances; the second word is
// the original `b REPStackTrace`/`b REPExceptionNotify` which fires
// natively, formats, and Prints — landing back in our patched
// `Print` body and out the UART.
//
// `Print`'s args follow standard ARM EABI varargs:
//   r0 = `this` (ignored), r1 = fmt, r2/r3 = first two args, then
//   the rest at the caller's source-mode SP. `src/rep_print.rs`
//   walks the format string and pulls args accordingly.

/// `PHammerOutTranslator::Print` body @ ROM `0x000E_6A90`.
pub const HAMMER_PRINT_PC:               u32 = 0x000E_6A90;
const HAMMER_PRINT_FIRST_INSN:           u32 = 0xE1A0_C00D; // mov ip, sp

/// `PHammerOutTranslator::Putc` body @ ROM `0x000E_6AD0`.
pub const HAMMER_PUTC_PC:                u32 = 0x000E_6AD0;
const HAMMER_PUTC_FIRST_INSN:            u32 = 0xE1A0_C00D; // mov ip, sp

/// `PHammerOutTranslator::Flush` body @ ROM `0x000E_6A50`.
pub const HAMMER_FLUSH_PC:               u32 = 0x000E_6A50;
const HAMMER_FLUSH_FIRST_INSN:           u32 = 0xE1A0_C00D; // mov ip, sp

/// `PHammerOutTranslator::StackTrace` first insn @ ROM `0x000E_6954`.
/// Original body is `mov r0, r1; b REPStackTrace` — we replace word 0
/// only and let the natural `b` run after HVC.
pub const HAMMER_STACKTRACE_PC:          u32 = 0x000E_6954;
const HAMMER_STACKTRACE_FIRST_INSN:      u32 = 0xE1A0_0001; // mov r0, r1

/// `PHammerOutTranslator::ExceptionNotify` first insn @ ROM `0x000E_695C`.
pub const HAMMER_EXCEPTION_NOTIFY_PC:    u32 = 0x000E_695C;
const HAMMER_EXCEPTION_NOTIFY_FIRST_INSN:u32 = 0xE1A0_0001; // mov r0, r1

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

/// `SWIBoot` dispatch-side instruction-as-data load:
/// `ldr r1, [r1, #-4]` at PC 0x003ad738 (with r1 = lr from the
/// preceding `mov r1, lr` at 0x003ad734) re-reads the SWI word so the
/// dispatch table index can be extracted via `bic r1, r1, #0xFF000000`
/// + `cmp r1, #0x23`. The conditional-SVC dispatcher at 0x003add7c
/// does `mrs r0, SPSR` along the way, clobbering the byteswap-
/// corrected SWI word that the iter-101 stub at 0x003ad69c put into
/// r0 — so reusing r0 here (as iter-102 tried to do) only works for
/// unconditional SVCs. Same B-to-stub fix as the other three sites:
/// re-do the LDR and REV the result.
const SWIBOOT_DISPATCH_LDR_PC:        u32 = 0x003a_D738;
const SWIBOOT_DISPATCH_LDR_ORIG_INSN: u32 = 0xE511_1004; // ldr r1, [r1, #-4]
const SWIBOOT_DISPATCH_LDR_RESUME_PC: u32 = 0x003a_D73C;

/// FPE prelude instruction-as-data load:
/// `FP_UndefHandlers_Start` at 0x0038_D8DC reads the faulting FPA
/// instruction into fp via two conditional loads at 0x0038_D930 (EQ:
/// `ldrteq fp, [r9], #0` for USR-source) and 0x0038_D934 (NE:
/// `ldrne fp, [r9]` for non-USR-source). Same byteswap problem as
/// the DAH / UND / SWIBoot sites: with CPSR.E=1 in UND mode the LDR
/// returns the byteswap of the byteswap-stored code word, which has
/// bit 27 clear, sending the FPE down its fall-through chain to
/// `UndefinedInstruction` instead of decoding the real FPA insn.
///
/// Patch shape: replace both LDR sites with conditional branches to
/// a single stub that does `ldr fp, [r9]; rev fp, fp; b resume`.
/// `LDR` uses kernel permissions instead of the original `LDRT`
/// (USR-permissions), but Newton ROM code is always kernel-readable
/// so this is equivalent in practice.
const FPE_LDR_EQ_PC:        u32 = 0x0038_D930;
const FPE_LDR_EQ_ORIG_INSN: u32 = 0x04B9_B000; // ldrteq fp, [r9], #0
const FPE_LDR_NE_PC:        u32 = 0x0038_D934;
const FPE_LDR_NE_ORIG_INSN: u32 = 0x1599_B000; // ldrne  fp, [r9]
const FPE_LDR_RESUME_PC:    u32 = 0x0038_D938;

/// Small helper to emit an ARM `B target` at `src_pc`.
const fn arm_b(src_pc: u32, target: u32) -> u32 {
    let off_bytes = target.wrapping_sub(src_pc.wrapping_add(8)) as i32;
    let off_words = (off_bytes / 4) as u32;
    0xEA00_0000 | (off_words & 0x00FF_FFFF)
}

/// Same as `arm_b` but with an explicit ARM condition field in the
/// high nibble (e.g. `0x0` for EQ, `0x1` for NE). The condition
/// replaces the AL=0xE that `arm_b` hard-codes.
const fn arm_b_cond(src_pc: u32, target: u32, cond: u32) -> u32 {
    let off_bytes = target.wrapping_sub(src_pc.wrapping_add(8)) as i32;
    let off_words = (off_bytes / 4) as u32;
    ((cond & 0xF) << 28) | 0x0A00_0000 | (off_words & 0x00FF_FFFF)
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

    // ns_trace: trick `TraceSetOptions__12TInterpreterFv` into
    // configuring trace mode even when the kernel's tracing options
    // frame (`gVars.tracing` or similar) is NIL.
    //
    // The function reads gVars.tracing into a Ref slot, then at
    // 0x35e7d8 tests `teq r0, #2` (Ref == NIL). On NIL it jumps
    // straight to the "tracing off" exit at 0x35ea18 — which is the
    // case on a stock boot. Flipping the immediate from #2 to #0
    // makes the test never match (genuine Refs are never zero),
    // so the function falls through to the setup-with-NIL-defaults
    // branch which sets `+105 = 1`, `+104 = 1`, and writes NIL to
    // the +112 / +116 / +108 filter slots. With those gates open
    // and the runtime poke of `gInterpreter[+124]=1` from
    // `heap_check::force_interpreter_trace_on`, every NS-level
    // `DoSend / DoMessage / DoFastApply` reaches `Print` with the
    // trace event — which lands in the EL2 UART via the always-on
    // PHammerOutTranslator body patches in
    // `apply_pouttranslator_patches`.
    //
    // Encoding: `teq r0, #N` is `e330_000N`; only the low 12 bits
    // of the immediate change (cond/op/Rn/Rd untouched).
    #[cfg(feature = "ns_trace")]
    {
        // SAFETY: 0x35e7d8 is in the main-ROM region (< 0x0080_0000),
        // word-aligned, and rom_ptr backs the full 8 MiB ROM. The
        // word is code (the original `teq r0, #2` instruction), so
        // we use `write_rom_code_word` so the encoding is stored
        // native-LE for the CPU's instruction fetch.
        unsafe {
            let word_idx = (0x0035_E7D8u32 / 4) as usize;
            let prev = rom_ptr.add(word_idx).read();
            crate::guest_mem::write_rom_code_word(rom_ptr, word_idx, 0xE330_0000);
            kprintln!(
                "rom_patch: {:#010x}: was_host={:#010x} -> intended={:#010x}  ({})",
                0x0035_E7D8u32, prev, 0xE330_0000u32,
                "TraceSetOptions: teq r0, #0 (was #2) — force trace setup even when gVars.tracing is NIL",
            );
        }
        applied += 1;
    }
    // full_ns_trace: change the first store of the TInterpreter
    // trace-mode field at 0x35e7d4 from `mov r7, #0` to `mov r7, #3`.
    // This changes the default to full tracing.
    #[cfg(feature = "full_ns_trace")]
    {
        unsafe {
            let word_idx = (0x0035_E7D4u32 / 4) as usize;
            let prev = rom_ptr.add(word_idx).read();
            crate::guest_mem::write_rom_code_word(rom_ptr, word_idx, 0xE3A0_7003);
            kprintln!(
                "rom_patch: {:#010x}: was_host={:#010x} -> intended={:#010x}  ({})",
                0x0035_E7D4u32, prev, 0xE3A0_7003u32,
                "TraceSetOptions: mov r0, #3 (was #2) — first store to TInterpreter+0x7C",
            );
        }
        applied += 1;
        unsafe {
            let word_idx = (0x000E_6A1Cu32 / 4) as usize;
            let prev = rom_ptr.add(word_idx).read();
            crate::guest_mem::write_rom_code_word(rom_ptr, word_idx, 0xE330_00FF);
            kprintln!(
                "rom_patch: {:#010x}: was_host={:#010x} -> intended={:#010x}  ({})",
                0x000E_6A1Cu32, prev, 0xE330_00FFu32,
                "ConsumeFrame: teq r0, #FF (was #0) — force PrintObject call",
            );
        }
        applied += 1;
        unsafe {
            let word_idx = (0x0033_cb24 / 4) as usize;
            let prev = rom_ptr.add(word_idx).read();
            crate::guest_mem::write_rom_code_word(rom_ptr, word_idx, 0xE3A0_0008);
            kprintln!(
                "rom_patch: {:#010x}: was_host={:#010x} -> intended={:#010x}  ({})",
                0x0033_cb24, prev, 0xE3A0_0008u32,
                "PrintObject: mov r0, #8 — change object depth to 2",
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
        // Loud-halt canaries are dev-only tripwires: on real hardware a
        // user reset or idle/sleep entry would halt the hypervisor.
        // build.rs emits `nh_loud_halt_canaries` for semihost/dev
        // builds and omits it under `no-semihost`.
        #[cfg(nh_loud_halt_canaries)]
        apply_loud_halt_traps(rom_ptr);
        apply_bootos_trap(rom_ptr);
        // Two NewStack/LockHeapRange wrapper strategies were tried and
        // abandoned: a +4 KiB NewStack-size pad (overran the kernel's
        // stack-pool slot stride → ResolveFault loop) and a 4-KiB-rounding
        // LockHeapRange wrapper (pinned subpages owned by other
        // stack_infos). Both lived in the wrong layer; the shipped fix is
        // per-allocator (the ResolveFault whole-page bitmap + ZapHeap
        // patches in PATCHES_717006). Not reinstating either.
        apply_l1_cd_probes(rom_ptr);
        apply_fault_handler_ldr_byteswap_patches(rom_ptr);
        #[cfg(feature = "log_store")]
        apply_storeperm_loadperm_probes(rom_ptr);
    }

    // The loud-halt canaries (StopImage/Reboot/PowerOffAndReboot/
    // busError) are dev-only — absent under no-semihost — so the
    // summary names them only when they were actually installed.
    #[cfg(nh_loud_halt_canaries)]
    const CANARIES: &str = " + loud-halt canaries";
    #[cfg(not(nh_loud_halt_canaries))]
    const CANARIES: &str = "";
    kprintln!("rom_patch: applied {} simple patches + 5 native-call/injection ROM patches{} + BootOS + load-bearing HVC patches + fault-handler LDR byteswap stubs", applied, CANARIES);
}


/// Install the load-bearing HVC patches: the Remember-post-SWI
/// sentinel reload, the QEMU DAH `mrs r1, SPSR` workaround, the
/// `Unhandled[NonUserMode]Exception` halt tripwires, and the
/// PHammerOutTranslator body redirects that route the kernel's REP
/// output into the EL2 UART. The Phase-B diagnostic probes that
/// used to live here (DAH Layer-γ trio, iter-108 splash chain,
/// FPE-entry, ResolveFault entry/exit) have been removed.
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
            HvcImm::RememberSwiret,
            "Remember post-SWI",
        );
        // QEMU raspi3b workaround: patch the kernel's `mrs r1, SPSR`
        // at DAH entry (0x393144) so EL2 can substitute the
        // trampoline-saved SPSR_abt for the stale `mrs spsr_abt`.
        patch_probe(
            rom_ptr,
            DAH_MRS_SPSR_PC,
            DAH_MRS_SPSR_INSN,
            HvcImm::DahMrsSpsr,
            "DataAbortHandler mrs r1, SPSR (QEMU spsr_abt staleness fix)",
        );
        // UnhandledException tripwires — halt cleanly with the kernel-
        // supplied exception-name string instead of letting the boot
        // bury the diagnostic under downstream Reboot / abort noise.
        patch_probe(
            rom_ptr,
            UNHANDLED_EXCEPTION_PC,
            UNHANDLED_EXCEPTION_FIRST_INSN,
            HvcImm::UnhandledException,
            "UnhandledException entry (halt-on-entry tripwire)",
        );
        patch_probe(
            rom_ptr,
            UNHANDLED_NUM_EXCEPTION_PC,
            UNHANDLED_NUM_EXCEPTION_FIRST_INSN,
            HvcImm::UnhandledNumException,
            "UnhandledNonUserModeException entry (halt-on-entry tripwire)",
        );
        // PHammerOutTranslator concrete-body patches: route every
        // `gREPout->{Print,Putc,Flush,StackTrace,ExceptionNotify}`
        // call into the EL2 UART. Always on (no feature gate).
        apply_pouttranslator_patches(rom_ptr);
    }
}

/// Replace `PHammerOutTranslator`'s output method bodies with HVC
/// stubs that forward to `rep_print` in EL2. See the constant block
/// above for the detailed rationale and patch shape.
unsafe fn apply_pouttranslator_patches(rom_ptr: *mut u32) {
    // Print/Putc/Flush: 3-word body replacement.
    //   word 0: HVC #imm
    //   word 1: mov r0, #0   (e3a00000)
    //   word 2: mov pc, lr   (e1a0f00e)
    const MOV_R0_0:  u32 = 0xE3A0_0000;
    const MOV_PC_LR: u32 = 0xE1A0_F00E;

    let bodies = [
        (HAMMER_PRINT_PC, HAMMER_PRINT_FIRST_INSN, HvcImm::HammerPrint,
         "PHammerOutTranslator::Print body"),
        (HAMMER_PUTC_PC, HAMMER_PUTC_FIRST_INSN, HvcImm::HammerPutc,
         "PHammerOutTranslator::Putc body"),
        (HAMMER_FLUSH_PC, HAMMER_FLUSH_FIRST_INSN, HvcImm::HammerFlush,
         "PHammerOutTranslator::Flush body"),
    ];
    for &(pc, expected, hvc, name) in &bodies {
        let idx = (pc / 4) as usize;
        // SAFETY: rom_ptr backs full 8 MiB main ROM; pc < 0x80_0000.
        let prev = unsafe { rom_ptr.add(idx).read() };
        if prev != expected {
            kprintln!(
                "rom_patch: ERROR — {} at {:#010x} is {:#010x}, expected {:#010x}; skipping",
                name, pc, prev, expected,
            );
            continue;
        }
        unsafe {
            crate::guest_mem::write_rom_code_word(rom_ptr, idx,     hvc.insn());
            crate::guest_mem::write_rom_code_word(rom_ptr, idx + 1, MOV_R0_0);
            crate::guest_mem::write_rom_code_word(rom_ptr, idx + 2, MOV_PC_LR);
        }
        record_original(pc, prev);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> HVC #{:#x} + mov r0,#0 + mov pc,lr  ({})",
            pc, prev, hvc as u32, name,
        );
    }

    // StackTrace/ExceptionNotify: word-0 only. Original second word
    // (`b REP*`) runs natively after HVC; handler emulates `mov r0, r1`.
    // SAFETY: rom_ptr backs the full main ROM; both PCs are validated
    // < 0x80_0000 by their constants.
    unsafe {
        patch_probe(
            rom_ptr,
            HAMMER_STACKTRACE_PC,
            HAMMER_STACKTRACE_FIRST_INSN,
            HvcImm::HammerStackTrace,
            "PHammerOutTranslator::StackTrace body (mov r0,r1 → HVC)",
        );
        patch_probe(
            rom_ptr,
            HAMMER_EXCEPTION_NOTIFY_PC,
            HAMMER_EXCEPTION_NOTIFY_FIRST_INSN,
            HvcImm::HammerExceptionNotify,
            "PHammerOutTranslator::ExceptionNotify body (mov r0,r1 → HVC)",
        );
    }
}

/// Patch the kernel's four known `LDR` sites that read the faulting
/// (or trapping) instruction word as data:
///
///   - DataAbortHandler 0x003931e4 (`ldr r0, [lr]`)
///   - UndefinedInstruction 0x0038ce9c (`ldr r1, [lr, #-4]`)
///   - SWIBoot 0x003ad69c (`ldr r0, [lr, #-4]`)
///   - SWIBoot dispatch 0x003ad738 (`ldr r1, [r1, #-4]` with r1 = lr)
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

    // SWIBoot dispatch site: target r1, base register is r1 (which the
    // preceding `mov r1, lr` at 0x003ad734 has loaded with LR_svc).
    //   +0x00 LDR r1, [r1, #-4]                    e5111004
    //   +0x04 REV r1, r1                           e6bf_1f31
    //   +0x08 B   SWIBOOT_DISPATCH_LDR_RESUME_PC   arm_b(...)
    let swiboot_dispatch_stub_pc =
        alloc_patch_stub(3, "SWIBoot dispatch-insn LDR byteswap");
    let swiboot_dispatch_stub: [u32; 3] = [
        0xE511_1004, // LDR r1, [r1, #-4]
        0xE6BF_1F31, // REV r1, r1
        arm_b(swiboot_dispatch_stub_pc + 0x08, SWIBOOT_DISPATCH_LDR_RESUME_PC),
    ];

    // FPE prelude: target fp (= r11). Both 0x38d930 and 0x38d934 land
    // here (one via BEQ, one via BNE), and both load fp from [r9].
    //   +0x00 LDR fp, [r9]                e599_b000
    //   +0x04 REV fp, fp                  e6bf_bf3b
    //   +0x08 B   FPE_LDR_RESUME_PC       arm_b(...)
    let fpe_stub_pc = alloc_patch_stub(3, "FPE prelude faulting-insn LDR byteswap");
    let fpe_stub: [u32; 3] = [
        0xE599_B000, // LDR fp, [r9]
        0xE6BF_BF3B, // REV fp, fp
        arm_b(fpe_stub_pc + 0x08, FPE_LDR_RESUME_PC),
    ];

    unsafe {
        write_stub_words(rom_ptr, dah_stub_pc, &dah_stub);
        write_stub_words(rom_ptr, und_stub_pc, &und_stub);
        write_stub_words(rom_ptr, swiboot_stub_pc, &swiboot_stub);
        write_stub_words(rom_ptr, swiboot_dispatch_stub_pc, &swiboot_dispatch_stub);

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

        // SWIBoot dispatch site (iter-104).
        let swib_disp_idx = (SWIBOOT_DISPATCH_LDR_PC / 4) as usize;
        let prev = rom_ptr.add(swib_disp_idx).read();
        if prev != SWIBOOT_DISPATCH_LDR_ORIG_INSN {
            kprintln!(
                "rom_patch: ERROR — SWIBoot dispatch LDR at {:#010x} is {:#010x}, expected {:#010x}; skipping byteswap stub",
                SWIBOOT_DISPATCH_LDR_PC, prev, SWIBOOT_DISPATCH_LDR_ORIG_INSN,
            );
        } else {
            let insn = arm_b(SWIBOOT_DISPATCH_LDR_PC, swiboot_dispatch_stub_pc);
            crate::guest_mem::write_rom_code_word(rom_ptr, swib_disp_idx, insn);
            record_original(SWIBOOT_DISPATCH_LDR_PC, prev);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (SWIBoot dispatch ldr r1,[r1,-4] → B stub @ {:#x}, byteswap)",
                SWIBOOT_DISPATCH_LDR_PC, prev, insn, swiboot_dispatch_stub_pc,
            );
        }

        // FPE prelude: two conditional sites (BEQ for USR-source,
        // BNE for non-USR-source) both pointing at the same byteswap
        // stub. Stub installed here; per-site B's installed below.
        write_stub_words(rom_ptr, fpe_stub_pc, &fpe_stub);
        for (pc, expected, cond, label) in [
            (FPE_LDR_EQ_PC, FPE_LDR_EQ_ORIG_INSN, 0x0u32, "FPE ldrteq fp,[r9]"),
            (FPE_LDR_NE_PC, FPE_LDR_NE_ORIG_INSN, 0x1u32, "FPE ldrne  fp,[r9]"),
        ] {
            let idx = (pc / 4) as usize;
            let prev = rom_ptr.add(idx).read();
            if prev != expected {
                kprintln!(
                    "rom_patch: ERROR — {} at {:#010x} is {:#010x}, expected {:#010x}; skipping byteswap branch",
                    label, pc, prev, expected,
                );
                continue;
            }
            let insn = arm_b_cond(pc, fpe_stub_pc, cond);
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
            record_original(pc, prev);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} → B{} stub @ {:#x}, byteswap)",
                pc, prev, insn, label,
                if cond == 0 { "EQ" } else { "NE" },
                fpe_stub_pc,
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
    hvc_imm: HvcImm,
    name: &'static str,
) {
    let idx = (pc / 4) as usize;
    let new_insn = hvc_imm.insn();
    let imm = hvc_imm as u32;
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
    let debugstr_stub: [u32; 2] = [0xE1A0_700E, HvcImm::DebugStr.insn()];
    let debugger_stub: [u32; 2] = [0xE1A0_700E, HvcImm::Debugger.insn()];
    unsafe {
        write_stub_words(rom_ptr, debug_str_stub_pc, &debugstr_stub);
        write_stub_words(rom_ptr, debugger_stub_pc,  &debugger_stub);

        let word = (0x0038_CE6C / 4) as usize;
        let prev = rom_ptr.add(word).read();
        let insn = arm_b(0x0038_CE6C, debug_str_stub_pc);
        crate::guest_mem::write_rom_code_word(rom_ptr, word, insn);
        kprintln!(
            "rom_patch: 0x0038ce6c: {:#010x} -> {:#010x}  (DebugStr → B {:#x}, HVC #{:#x})",
            prev, insn, debug_str_stub_pc, HvcImm::DebugStr as u32,
        );
        let word = (0x0038_CE70 / 4) as usize;
        let prev = rom_ptr.add(word).read();
        let insn = arm_b(0x0038_CE70, debugger_stub_pc);
        crate::guest_mem::write_rom_code_word(rom_ptr, word, insn);
        kprintln!(
            "rom_patch: 0x0038ce70: {:#010x} -> {:#010x}  (Debugger → B {:#x}, HVC #{:#x})",
            prev, insn, debugger_stub_pc, HvcImm::Debugger as u32,
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
/// with a branch to a stub that subtracts `safeIntervalDeltaSeconds`
/// and performs the NS-integer `<< 2`, then branches to the epilogue
/// — net effect `r0 = (r0 - delta) << 2`. Einstein's equivalent at
/// `TJITGenericROMPatch.cpp:150` uses `T_ROM_PATCH` which *replaces*
/// the original instruction (per `TJITGenericROMPatch.h:283` "return
/// ioUnit if the next instruction is to be executed"), so the
/// original `LSL #2` does **not** run after Einstein's callback.
unsafe fn apply_ftime_in_seconds_patch(rom_ptr: *mut u32) {
    const PATCH_PC: u32 = 0x0008_9B80;
    const RETURN_PC: u32 = 0x0008_9B84; // original LDMDB epilogue
    let ftime_stub_pc = alloc_patch_stub(5, "FTimeInSeconds stub");
    // Stub body (5 words):
    //   +0x00 LDR r12, [pc, #8]           ; load delta from +0x10
    //   +0x04 SUB r0, r0, r12             ; r0 = r0 - delta
    //   +0x08 MOV r0, r0, LSL #2          ; NS-integer encode
    //   +0x0C B <RETURN_PC>               ; resume at the epilogue
    //   +0x10 .word safeIntervalDeltaSeconds
    let stub_b = arm_b(ftime_stub_pc + 0x0C, RETURN_PC);
    let stub: [u32; 5] = [
        0xE59F_C008,        // LDR r12, [pc, #8]
        0xE040_000C,        // SUB r0, r0, r12
        0xE1A0_0100,        // MOV r0, r0, LSL #2
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

/// Patch the first word of each of `PowerOffAndReboot` (0x000E_6BBC),
/// `Reboot` (0x000D_9884), and `StopImage` (0x0038_D174) with a single
/// `HVC #HvcImm::LoudHalt`. The handler in `trap::handle_hvc` dumps
/// the calling context (R0..R3, mode, caller LR) and halts — we never
/// resume. Catches the boot-fail-and-reboot loop AND the idle/sleep
/// wait-for-wakeup spin the FIRST time either fires, instead of letting
/// the run go on for tens of thousands of repeated tracer entries
/// before timeout.
#[cfg(nh_loud_halt_canaries)]
unsafe fn apply_loud_halt_traps(rom_ptr: *mut u32) {
    let insn = HvcImm::LoudHalt.insn();
    for (pc, name) in [
        (POWEROFF_REBOOT_PC, "PowerOffAndReboot"),
        (REBOOT_PC, "Reboot"),
        (STOP_IMAGE_PC, "StopImage"),
        (BUS_ERROR_THROW_PC, "BusErrorThrow"),
    ] {
        let idx = (pc / 4) as usize;
        unsafe {
            let prev = rom_ptr.add(idx).read();
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} loud-halt, HVC #{:#x})",
                pc, prev, insn, name, HvcImm::LoudHalt as u32,
            );
        }
    }
}

/// Software-reset canary at `BootOS` (0x0001_8688). Overwrite the
/// first word with `HVC #HvcImm::BootOs`; the handler distinguishes
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
    let insn = HvcImm::BootOs.insn();
    unsafe {
        crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
    }
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (BootOS canary, HVC #{:#x})",
        BOOTOS_PC, prev, insn, HvcImm::BootOs as u32,
    );
}

/// First instruction of `StorePermObject` (ROM 0x002D_F998) —
/// `mov ip, sp` (`0xE1A0_C00D`). Replaced with `HVC
/// #StorePermObjEntry`; the handler dereferences R0 (a `RefVar
/// const&`) to recover the Ref being stored, pretty-prints it via
/// `newton-objects`, emulates the original `mov ip, sp` (writes
/// `ctx.x[12] = source-mode SP`), and advances ELR past the HVC so
/// the function's prologue picks up at instruction 2 (`push {…}`).
#[cfg(feature = "log_store")]
const STORE_PERM_OBJECT_PC: u32 = 0x002D_F998;
#[cfg(feature = "log_store")]
const STORE_PERM_OBJECT_ORIG_INSN: u32 = 0xE1A0_C00D;

/// `mov r0, r4` immediately before `LoadPermObject`'s `ldmdb`
/// epilogue at ROM 0x002D_F4C0 (`0xE1A0_0004`). The function saves
/// the Ref returned by `Read__18TStoreObjectReaderFv` into R4,
/// runs the destructor chain, then `mov r0, r4` to restore the
/// return Ref before `ldmdb`. Replacing this site with `HVC
/// #LoadPermObjRet` lets us pretty-print the Ref about to be
/// returned. Handler emulates `r0 = r4` (R0 and R4 are unbanked
/// across USR/UND so a direct `ctx.x[0] = ctx.x[4]` is correct in
/// either dispatch path) and advances ELR.
#[cfg(feature = "log_store")]
const LOAD_PERM_OBJECT_RET_PC: u32 = 0x002D_F4C0;
#[cfg(feature = "log_store")]
const LOAD_PERM_OBJECT_RET_ORIG_INSN: u32 = 0xE1A0_0004;

/// Install the StorePermObject entry probe + LoadPermObject
/// return-site probe. Pair: each call to StorePermObject pretty-
/// prints the RefArg being stored, each return from LoadPermObject
/// pretty-prints the Ref being handed back. Used to investigate
/// whether the flash-store round-trip is corrupting the Ref graph
/// (Phase B "infinite recursion during default-alarm setup"
/// stall). Gated by the `log_store` Cargo feature — when off, the
/// trap dispatch arms and handlers in `src/trap.rs` are cfg'd out,
/// so leaving these patches uninstalled is required to avoid
/// trapping into a non-existent handler.
#[cfg(feature = "log_store")]
unsafe fn apply_storeperm_loadperm_probes(rom_ptr: *mut u32) {
    for (pc, orig, imm, name) in [
        (
            STORE_PERM_OBJECT_PC,
            STORE_PERM_OBJECT_ORIG_INSN,
            HvcImm::StorePermObjEntry,
            "StorePermObject entry probe",
        ),
        (
            LOAD_PERM_OBJECT_RET_PC,
            LOAD_PERM_OBJECT_RET_ORIG_INSN,
            HvcImm::LoadPermObjRet,
            "LoadPermObject return probe",
        ),
    ] {
        let idx = (pc / 4) as usize;
        let prev = unsafe { rom_ptr.add(idx).read() };
        if prev != orig {
            kprintln!(
                "rom_patch: ERROR — {} site at {:#010x} is {:#010x}, expected {:#010x}; skipping",
                name, pc, prev, orig,
            );
            continue;
        }
        let insn = imm.insn();
        unsafe {
            crate::guest_mem::write_rom_code_word(rom_ptr, idx, insn);
        }
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({}, HVC #{:#x})",
            pc, prev, insn, name, imm as u32,
        );
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
