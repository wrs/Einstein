//! Einstein-equivalent word-write ROM patches for the 717006 ROM
//! (MP2100 US) — mirrors the `inAddr0` column from every
//! `TJITGenericPatch` in
//! `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp`, restricted
//! to entries that the 717006 ROM id selects (not `kROMPatchVoid`),
//! plus this hypervisor's own ARMv7-compatibility patches (heap
//! chunking, stack-area stride, wrap-detect).
//!
//! What the Einstein-derived patches change (all at main-ROM offsets,
//! applied AFTER byteswap so we write in guest-CPU view):
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
//! Values are precisely what Einstein writes:
//!   - `newTimeBaseMinutes` = 218_799_360 = 0x0D09_5000
//!   - `newTimeBaseSeconds` = 3_281_990_400 = 0xC3A5_1800
//!   - `gNewtConfig` combines `kEnableListener (0x2)`,
//!     `kDefaultStdioOn (0x200)`, `kEnableStdout (0x8000)`.
//!
//! See `Einstein/Emulator/JIT/Generic/TJITGenericROMPatch.cpp` for the
//! full annotated list and the Einstein-side rationale for each.

use super::super::types::RomPatch;

pub(super) const PATCHES_717006: &[RomPatch] = &[
    RomPatch { offset: 0x0000_13F4, orig: 0x0000_0040, value: 0x0000_0001, name: "gDebugger patch" },
    RomPatch { offset: 0x0000_13FC, orig: 0x0000_0000, value: 0x0000_8202, name: "gNewtConfig patch" },
    RomPatch { offset: 0x0008_A20C, orig: 0xE1A0_C00D, value: 0xE1A0_F00E, name: "Ignore setting time" },
    RomPatch { offset: 0x000D_B0D8, orig: 0xE1A0_C00D, value: 0xE3A0_0000, name: "BeaconDetect (1/2)" },
    RomPatch { offset: 0x000D_B0DC, orig: 0xE92D_D9F0, value: 0xE1A0_F00E, name: "BeaconDetect (2/2)" },
    RomPatch { offset: 0x0014_12F8, orig: 0x0A00_0002, value: 0xEA00_0009, name: "Avoid screen calibration" },
    RomPatch { offset: 0x0030_F088, orig: 0xA769_3A00, value: 0xC3A5_1800, name: "Time base (4/4)" },
    RomPatch { offset: 0x0042_0750, orig: 0x0B29_2600, value: 0x0D09_5000, name: "Time base (1/4)" },
    RomPatch { offset: 0x0042_0798, orig: 0x0B29_2600, value: 0x0D09_5000, name: "Time base (2/4)" },
    RomPatch { offset: 0x004D_CA14, orig: 0x0B29_2600, value: 0x0D09_5000, name: "Time base (3/4)" },
    // GetClock / SetAlarm 32-bit-wrap detection: replace `addls`
    // (less-or-equal) with `addcc` (strictly-less) so the kernel
    // doesn't treat *equal* successive tick-register reads as a wrap
    // event. The original code is correct on real hardware where
    // CNTPCT-equivalent always strictly advances between two reads,
    // but our `stage2::TICK_PAGE` mapping only refreshes on hypervisor
    // heartbeat, so two guest tick reads inside one ~16 ms heartbeat
    // window observe identical values. The ls/cc swap keeps real
    // wraps detected (new < old) but ignores the spurious equality.
    // (Root cause: an alarm-loop wedge from spurious wrap detection.)
    // Encoding: cond field [31:28] LS=9 → CC=3; the rest
    // of the instruction (`add Rn, Rn, #1`) is unchanged.
    RomPatch { offset: 0x003A_D430, orig: 0x9281_1001, value: 0x3281_1001, name: "GetClock wrap-detect ls→cc" },
    RomPatch { offset: 0x003A_D46C, orig: 0x9282_2001, value: 0x3282_2001, name: "SetAlarm wrap-detect (1/2) ls→cc" },
    RomPatch { offset: 0x003A_D49C, orig: 0x9282_2001, value: 0x3282_2001, name: "SetAlarm wrap-detect (2/2) ls→cc" },
    // ExpandIMA (TIMACodec's IMA-ADPCM decode loop, 0xE8500): the
    // compiler reads signed halfwords from the step-size table
    // (0x3716A4, indexed by the step index) and the index-adjust table
    // (0x371684, indexed by the nibble code) with the ARMv4 rotate-LDR
    // idiom — `ldr Rd, [Rb, Ri, lsl #1]` + `asr Rd, Rd, #16`. Half of
    // those loads (odd index) are unaligned and trap to the EL2
    // rotate-LDR emulator once per decoded sample, making 0xE863C the
    // top unaligned-fault hotspot during sound playback. Each pair is
    // replaced in place with `add Rd, Rb, Ri, lsl #1` + `ldrsh Rd,
    // [Rd]` — the same signed BE halfword read at `base + 2*i`
    // (aligned, so it never traps), byte-for-byte equivalent to the
    // emulator's `ROR 8*(addr&3)` + `asr #16` result in both the
    // even- and odd-index case. No branch in the ROM targets the
    // second word of any pair. The compressor's twin sites (0xE83xx)
    // are left alone — only the decode loop is hot.
    RomPatch { offset: 0x000E_858C, orig: 0xE792_E081, value: 0xE082_E081, name: "ExpandIMA: add lr, r2, r1, lsl #1 (was rotate-LDR, step table entry)" },
    RomPatch { offset: 0x000E_8590, orig: 0xE1A0_E84E, value: 0xE1DE_E0F0, name: "ExpandIMA: ldrsh lr, [lr] (was asr lr, lr, #16, step table entry)" },
    RomPatch { offset: 0x000E_863C, orig: 0xE79C_2082, value: 0xE08C_2082, name: "ExpandIMA: add r2, ip, r2, lsl #1 (was rotate-LDR, index-adjust)" },
    RomPatch { offset: 0x000E_8640, orig: 0xE1A0_2842, value: 0xE1D2_20F0, name: "ExpandIMA: ldrsh r2, [r2] (was asr r2, r2, #16, index-adjust)" },
    RomPatch { offset: 0x000E_865C, orig: 0xE792_E081, value: 0xE082_E081, name: "ExpandIMA: add lr, r2, r1, lsl #1 (was rotate-LDR, step re-lookup)" },
    RomPatch { offset: 0x000E_8660, orig: 0xE1A0_E84E, value: 0xE1DE_E0F0, name: "ExpandIMA: ldrsh lr, [lr] (was asr lr, lr, #16, step re-lookup)" },
    // SWIBoot's second instruction-as-data LDR at 0x003ad738 is
    // patched separately, via `INSN_AS_DATA_LDRS`, as a B-to-stub —
    // a full LDR-byteswap stub mirroring the site at 0x003ad69c, so
    // the re-read works for conditional SVCs too. A cheaper
    // `mov r1, r0` does not work: it assumes r0 still carries the
    // byteswap-corrected SWI word, which holds for unconditional SVCs
    // but not for the conditional-SVC dispatcher at 0x003add7c, which
    // does `mrs r0, SPSR` and clobbers r0 with the caller's CPSR. The
    // downstream `mov r1, r0; bic r1, r1, #0xFF000000; cmp r1, #0x23`
    // then sees CPSR-shaped garbage (low 24 bits include the mode
    // field), the bge fires, and boot wedges in the "Undefined SWI"
    // debug stub.
    // Force every VM heap to allocate / extend in 4-KiB chunks
    // instead of 1-KiB subpages. The kernel's design partitions
    // shared 4-KiB physical pages into 1-KiB subpages with per-
    // subpage AP, enforced by ARMv4's subpage-AP. ARMv7 has no
    // subpage-AP — `fix_stage1_xn_bits` flattens to AP=011, so a
    // stack write to "its" subpage spills into the heap's adjacent
    // subpage on the same physical page.
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
    RomPatch { offset: 0x0031_0E38, orig: 0xE1A0_6002, value: 0xE3A0_6A01, name: "NewHeap: force chunk_size=4096" },
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
    RomPatch { offset: 0x0014_23A0, orig: 0x0A00_0006, value: 0xE1A0_0000, name: "NewVMHeap: force 4 KiB init path (nop branch)" },
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
    RomPatch { offset: 0x0014_28B8, orig: 0x03A0_4B01, value: 0xE3A0_4A01, name: "ZapHeap: force chunk/lock size = 4096" },
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
    RomPatch { offset: 0x001F_8EDC, orig: 0xE3A0_7B21, value: 0xE3A0_7A09, name: "FMNewStack: mov r7, #36864 (clamp value)" },
    RomPatch { offset: 0x001F_8EF0, orig: 0xE240_1B03, value: 0xE240_1A01, name: "FMNewStack: sub r1, r0, #4096 (was 3072; guard 3K → 4K)" },
    RomPatch { offset: 0x001F_8F18, orig: 0xE3A0_0B21, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, request-addr path)" },
    RomPatch { offset: 0x001F_8F20, orig: 0xE080_0280, value: 0xE080_0180, name: "FMNewStack: add r0, r0, r0, lsl #3 (was lsl #5; *33 → *9)" },
    RomPatch { offset: 0x001F_8F24, orig: 0xE049_0500, value: 0xE049_0600, name: "FMNewStack: sub r0, r9, r0, lsl #12 (was lsl #10; *1024 → *4096)" },
    RomPatch { offset: 0x001F_8F30, orig: 0xE280_0B03, value: 0xE280_0A01, name: "FMNewStack: add r0, r0, #4096 (was 3072; maxSize += 4K guard, request-addr path)" },
    RomPatch { offset: 0x001F_8F38, orig: 0xE350_0B21, value: 0xE350_0A09, name: "FMNewStack: cmp r0, #36864 (clamp, request-addr path)" },
    RomPatch { offset: 0x001F_8F48, orig: 0xE3A0_0B21, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, request-addr path)" },
    RomPatch { offset: 0x001F_8F5C, orig: 0xE3A0_0B21, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, request-addr path)" },
    RomPatch { offset: 0x001F_8F88, orig: 0xE280_0B03, value: 0xE280_0A01, name: "FMNewStack: add r0, r0, #4096 (was 3072; maxSize += 4K guard, any-addr path)" },
    RomPatch { offset: 0x001F_8F90, orig: 0xE350_0B21, value: 0xE350_0A09, name: "FMNewStack: cmp r0, #36864 (clamp, any-addr path)" },
    RomPatch { offset: 0x001F_8FA0, orig: 0xE3A0_0B21, value: 0xE3A0_0A09, name: "FMNewStack: mov r0, #36864 (udiv divisor, any-addr path)" },
    RomPatch { offset: 0x001F_9024, orig: 0xE08A_128A, value: 0xE08A_118A, name: "FMNewStack: add r1, sl, sl, lsl #3 (was lsl #5; *33 → *9, top-of-area)" },
    RomPatch { offset: 0x001F_902C, orig: 0xE080_9501, value: 0xE080_9601, name: "FMNewStack: add r9, r0, r1, lsl #12 (was lsl #10; *1024 → *4096, top-of-area)" },
    RomPatch { offset: 0x001F_9030, orig: 0xE087_0287, value: 0xE087_0187, name: "FMNewStack: add r0, r7, r7, lsl #3 (was lsl #5; *33 → *9, area-base)" },
    RomPatch { offset: 0x001F_9034, orig: 0xE049_0500, value: 0xE049_0600, name: "FMNewStack: sub r0, r9, r0, lsl #12 (was lsl #10; *1024 → *4096, area-base)" },
    RomPatch { offset: 0x001F_9038, orig: 0xE280_2B03, value: 0xE280_2A01, name: "FMNewStack: add r2, r0, #4096 (was 3072; bottomOfStack = norm + 4K, page-aligned)" },

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
    RomPatch { offset: 0x001F_9060, orig: 0x4280_0003, value: 0xE1A0_0000, name: "FMNewStack: nop (was addmi r0, r0, #3 — drop /4)" },
    RomPatch { offset: 0x001F_9064, orig: 0xE1A0_0140, value: 0xE1A0_0000, name: "FMNewStack: nop (was asr r0, r0, #2 — drop /4)" },

    // (A continued) Heap-domain helpers — same stride change.
    //
    // Init__11THeapDomain at 0x001F_8D74 is intentionally NOT patched.
    // It constructs the slot-info array for both stack pools and
    // regular data heaps; sizing it with the larger stride would
    // UNDER-size the array, breaking heap growth past the 33 KiB-
    // sized array's index range. The unpatched 33 KiB divisor
    // OVER-sizes the array for the new stride — wasted memory but
    // functionally safe.
    RomPatch { offset: 0x001F_8E1C, orig: 0xE3A0_0B21, value: 0xE3A0_0A09, name: "THeapDomain::GetStackInfo: mov r0, #36864 (slot-index divisor)" },
    RomPatch { offset: 0x001F_918C, orig: 0xE3A0_0B21, value: 0xE3A0_0A09, name: "FMFree: mov r0, #36864 (slot-index divisor)" },

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
    RomPatch { offset: 0x001F_7A0C, orig: 0xE3A0_0001, value: 0xE3A0_000F, name: "ResolveFault: mov r0, #15 (whole-page bitmap)" },
    RomPatch { offset: 0x001F_7A10, orig: 0xE1A0_3810, value: 0xE1A0_3000, name: "ResolveFault: mov r3, r0 (drop sub-idx shift)" },
];

/// ns_trace gate patch: trick `TraceSetOptions__12TInterpreterFv` into
/// configuring trace mode even when the kernel's tracing options
/// frame (`gVars.tracing` or similar) is NIL.
///
/// The function reads gVars.tracing into a Ref slot, then at
/// 0x35e7d8 tests `teq r0, #2` (Ref == NIL). On NIL it jumps
/// straight to the "tracing off" exit at 0x35ea18 — which is the
/// case on a stock boot. Flipping the immediate from #2 to #0
/// makes the test never match (genuine Refs are never zero),
/// so the function falls through to the setup-with-NIL-defaults
/// branch which sets `+105 = 1`, `+104 = 1`, and writes NIL to
/// the +112 / +116 / +108 filter slots. With those gates open
/// and the runtime poke of `gInterpreter[+124]=1` from
/// `heap_check::force_interpreter_trace_on`, every NS-level
/// `DoSend / DoMessage / DoFastApply` reaches `Print` with the
/// trace event — which lands in the EL2 UART via the always-on
/// PHammerOutTranslator body patches.
///
/// Encoding: `teq r0, #N` is `e330_000N`; only the low 12 bits
/// of the immediate change (cond/op/Rn/Rd untouched).
pub(super) const NS_TRACE_PATCH_717006: RomPatch = RomPatch {
    offset: 0x0035_E7D8,
    orig:   0xE330_0002,
    value:  0xE330_0000,
    name:   "TraceSetOptions: teq r0, #0 (was #2) — force trace setup even when gVars.tracing is NIL",
};
