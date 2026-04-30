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

use crate::kprintln;

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
    // Page-allocator probe — patches the `teq r0, #0` at 0x00258EFC
    // (immediately after `bl MonitorDispatchSWI` in TUDomainManager::Get)
    // with HVC #PAGE_GET_PROBE_HVC_IMM. Handler in trap.rs::handle_page_get_probe
    // logs (returned_PA, count, domain_field, caller_lr) and detects
    // duplicate-PA returns, then emulates the original `teq` by
    // setting SPSR_EL2 N/Z bits so the following ldreq/streq behave
    // correctly. Diagnostic; remove once aliasing root cause is
    // identified and fixed.
    RomPatch {
        offset: PAGE_GET_PROBE_PC,
        value: hvc_insn(PAGE_GET_PROBE_HVC_IMM),
        name: "TUDomainManager::Get post-SWI page-allocator probe",
    },
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
const DEBUG_STR_STUB_PC: u32 = 0x00FF_FF30;
const DEBUGGER_STUB_PC:  u32 = 0x00FF_FF38;
const FTIME_STUB_PC:     u32 = 0x00FF_FF40;
const FDATE_STUB_PC:     u32 = 0x00FF_FF60;

/// PC of the ResolveFault wrapper (see `apply_resolve_fault_wrapper`).
/// Sits below the existing 0x00FF_FFxx stubs in the post-UND-trampoline
/// reserved region. 20 words = 80 bytes; safe to grow downward as needed.
const RESOLVE_FAULT_WRAPPER_PC: u32 = 0x00FF_FE00;

/// Entry point of `TStackManager::ResolveFault` that the wrapper invokes.
/// Also re-exported as `RESOLVE_FAULT_ENTRY_PC` for the lazy-L1 probe.
const RESOLVE_FAULT_PC: u32 = 0x001F_7978;
pub const RESOLVE_FAULT_ENTRY_PC: u32 = RESOLVE_FAULT_PC;

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
const LOCK_HEAP_RANGE_WRAPPER_PC: u32 = 0x00FF_FD80;
const UNLOCK_HEAP_RANGE_WRAPPER_PC: u32 = 0x00FF_FDB0;

/// The original first word of LockHeapRange / UnlockHeapRange — the
/// standard `mov ip, sp` AArch32 prologue. Asserted at install time so
/// any future ROM build that shifts the function entries fails loudly.
const LOCK_UNLOCK_ORIG_FIRST_INSN: u32 = 0xE1A0_C00D;

// ---- L1[0xCD] lazy-grow investigation probes (2026-04-26) -----------------
//
// See docs/plans/l1-cd-lazy-investigation.md. The wedge fires at FAR
// 0x0cd07400 (DFSC=5, L1[0xCD]=0x90 lazy). The kernel's expected
// `RememberMappings → Remember → SWI #12 → AllocatePageTable` chain never
// grows L1[0xCD] past 0x90. These probes patch the relevant function
// entry points with HVC instructions so we can see (a) whether
// `Remember` is invoked with a VA in section 0xCD, (b) what SWI #12
// returns, and (c) whether AllocatePageTable runs.

/// HVC immediate fired by the patched first word of
/// `TUDomainManager::Remember (static)` at 0x00258E0C. Handler logs
/// args and the matching L1 entry, then emulates the original
/// `mov ip, sp` so the function prologue continues correctly.
pub const REMEMBER_PROBE_HVC_IMM:    u32 = 0x46;

/// HVC immediate fired by the patched word at 0x00258E50 (immediately
/// after the first `bl GenericSWI` inside Remember). Handler logs r0
/// (= the SWI #12 return value), then emulates `mov r8, #237` so the
/// kernel's `r8 = -10003` constant is restored before the `teq` at
/// 0x00258E58.
pub const REMEMBER_SWIRET_HVC_IMM:   u32 = 0x47;

/// HVC immediate fired by the patched first word of
/// `TUDomainManager::AllocatePageTable (static)` at 0x00259104. Handler
/// logs source-mode LR (= where the call came from), then emulates
/// `mov r2, #0` so the tail call into MonitorDispatchSWI sees the
/// expected r2 value.
pub const ALLOC_PT_PROBE_HVC_IMM:    u32 = 0x48;

/// HVC immediate fired by the patched first word of
/// `Fill__15TRefStructStackFv` at 0x001A4B54. Handler logs the source-
/// mode banked R14 (= caller PC) and source mode bits, then emulates
/// the original `stmfd sp!, {lr}` (push LR onto source-mode stack and
/// decrement source-mode SP by 4) so the function prologue continues
/// correctly. Reachable from both handle_hvc (privileged callers) and
/// handle_und (USR callers — HVC from EL0 is UNDEFINED). See
/// docs/plans/l1-cd-lazy-investigation.md Step 1.
pub const FILL_PROBE_HVC_IMM:        u32 = 0x49;

/// HVC immediate fired by the patched word inside `NewStack` at
/// 0x001F89A8 — the `ldr r1, [sp, #16]` that pulls the LOW output of
/// the just-returned MonitorDispatchSWI. Handler reads `[sp+16]` (LOW)
/// and `[sp+20]` (HIGH) from source-mode SP, logs them, and emulates
/// `ldr r1, [sp, #16]` so r1 := LOW for the following `str r1, [r4]`.
/// This site fires only on the success path (the preceding `bne 0x1f89b8`
/// skips the load-store pair on SWI failure), which is exactly the
/// allocations we want to record. See Step 3 in the plan.
pub const NEW_STACK_PROBE_HVC_IMM:   u32 = 0x4A;

/// HVC immediate fired by the patched first word of
/// `Fault__13TStackManagerFR15TProcessorState` at 0x001F83E4. Handler
/// logs source-mode CPSR + caller LR, the (manager*, processor_state*)
/// pair, and the FAR read from `processor_state->[+0x44]`, then emulates
/// the original `mov ip, sp`. See plan Step 5 prep — confirms whether
/// the kernel's stack-fault dispatcher is reached for the second
/// (FAR=0x0cd07400) abort.
pub const STACK_MGR_FAULT_PROBE_HVC_IMM: u32 = 0x4B;

/// HVC immediate fired by the patched first word of
/// `ResolveFault__13TStackManagerFP10TStackInfo` at 0x001F7978. Handler
/// logs source-mode CPSR + caller LR, the (manager*, info*) pair, and
/// the FAR read from `manager->[+0x40]->[+0x44]`, then emulates the
/// original `mov ip, sp`. Captures both wrapper-reached calls (from
/// 0x1f84e0 → wrapper @0xfffe00 → BL here) and direct calls (from
/// FMLockHeapRange at 0x1f6b94). See plan Step 5 prep.
pub const RESOLVE_FAULT_PROBE_HVC_IMM:   u32 = 0x4C;

/// HVC immediate fired by the patched first word of
/// `NewState__11TIntrpStackFv` at 0x001A46F0. Handler logs source-mode
/// CPSR + caller LR, then emulates the original `mov ip, sp`.
/// Discriminates hypothesis (A) silent-recovery vs (B) recovery-never-
/// returned for fault #2: if NewState fires once before the wedge,
/// recovery from fault #1 returned to USR; if it fires twice (USR + SVC)
/// we have a second recovery-handled-silently path. See plan Step 6b.
pub const NEW_STATE_PROBE_HVC_IMM:       u32 = 0x4D;

/// HVC immediate fired by the patched DataAbortHandler USR-return exit
/// (a `subs pc, lr, #N` or `movs pc, lr` inside the handler at
/// 0x00393114..). Handler logs source-mode CPSR + caller LR + the
/// would-be return PC (`lr` minus the same `#N`). Tells us whether
/// DataAbortHandler is exiting to USR after fault #1's recovery vs.
/// not exiting at all (hypothesis B). See plan Step 6c.
pub const DAH_USR_RETURN_PROBE_HVC_IMM:  u32 = 0x4E;
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

const REMEMBER_STATIC_PC:            u32 = 0x0025_8E0C;
const REMEMBER_STATIC_FIRST_INSN:    u32 = 0xE1A0_C00D; // mov ip, sp
const REMEMBER_SWIRET_PC:            u32 = 0x0025_8E50;
const REMEMBER_SWIRET_ORIG_INSN:     u32 = 0xE3A0_80ED; // mov r8, #237
const ALLOC_PT_STATIC_PC:            u32 = 0x0025_9104;
const ALLOC_PT_FIRST_INSN:           u32 = 0xE3A0_2000; // mov r2, #0
pub const FILL_STATIC_PC:            u32 = 0x001A_4B54;
const FILL_FIRST_INSN:               u32 = 0xE92D_4000; // stmfd sp!, {lr}
pub const NEW_STACK_LOW_LDR_PC:      u32 = 0x001F_89A8;
const NEW_STACK_LOW_LDR_INSN:        u32 = 0xE59D_1010; // ldr r1, [sp, #16]
pub const STACK_MGR_FAULT_PC:        u32 = 0x001F_83E4;
const STACK_MGR_FAULT_FIRST_INSN:    u32 = 0xE1A0_C00D; // mov ip, sp
const RESOLVE_FAULT_FIRST_INSN:      u32 = 0xE1A0_C00D; // mov ip, sp
pub const NEW_STATE_PC:              u32 = 0x001A_46F0;
const NEW_STATE_FIRST_INSN:          u32 = 0xE1A0_C00D; // mov ip, sp

/// Two `movs pc, lr` exits inside `DataAbortHandler` (0x00393114).
/// Both use the same encoding (`0xE1B0_F00E`), and both perform an
/// SPSR_abt → CPSR copy + PC := LR, exiting ABT mode. They differ in
/// what LR points to:
///   - 0x00393B80 — kernel-monitor success exit. Reached after
///     FaultMonitorEntry returned 0 and Scheduler picked a task; LR
///     was reloaded from the scheduled task's saved-LR slot, so this
///     is the actual "return to USR task" path.
///   - 0x00393944 — fast-throw exit. Reached when FaultMonitorEntry
///     returned non-0 (or when SPSR check at 0x39314c rejected the
///     pre-abt mode); LR was set to the literal at 0x393974
///     (= 0x01BE319C, `Throw`), so this exit tail-calls Throw with
///     pre-abt CPSR.
/// Both are mode-flip exits; instrumenting both lets us tell whether
/// the kernel went success-or-throw on each abort. The handler reads
/// ELR_EL2 to distinguish call sites.
pub const DAH_USR_RETURN_PC:         u32 = 0x0039_3B80;
pub const DAH_THROW_EXIT_PC:         u32 = 0x0039_3944;
const MOVS_PC_LR_INSN:               u32 = 0xE1B0_F00E;
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

/// `TUDomainManager::Get` post-SWI probe — instrument the kernel's
/// page allocator to log every (caller_lr, returned_PA, count,
/// domain_field) tuple and detect duplicate-PA returns. The patched
/// instruction is the `teq r0, #0` at `0x00258EFC`, immediately after
/// the `bl 0x3ae320 <MonitorDispatchSWI>` at `0x00258EF8`. The probe
/// runs in source mode (typically SVC since Get is called from
/// FMNewStack/AllocNewPage on the SWI fault path); after logging it
/// emulates the original `teq r0, #0` by setting SPSR_EL2 N/Z so the
/// following `ldreq r1, [sp]; streq r1, [r4]` continue correctly.
///
/// See PLAN.md "Static analysis dead end — proceed via runtime
/// probe" for the motivation and STRUCTURES.md "End-to-end page
/// allocation" for the args-buffer layout.
pub const PAGE_GET_PROBE_HVC_IMM:     u32 = 0x53;
pub const PAGE_GET_PROBE_PC:          u32 = 0x0025_8EFC;
// The original instruction at PAGE_GET_PROBE_PC is `teq r0, #0`
// (= 0xE3300000); the HVC handler emulates it by setting SPSR_EL2
// N/Z bits before ERET, so we don't need to re-execute the literal.

/// `PrimRememberMapping(env, va, &TPhys, perm)` probe at ROM
/// `0x00163480`. This is the lower-level L2-write primitive reached
/// from kernel-internal paths that bypass `Remember (static)` — the
/// suspected source of the 9 Group-2 stack-guard aliases per
/// PLAN.md "Aliasing elimination". Original first insn is the
/// standard `mov ip, sp` AArch32 prologue. Replace with HVC; the
/// handler dereferences `&TPhys` to recover the page-aligned PA
/// (= `*(r2+16) >> 12 << 12`), runs the per-PA → first-VA aliasing
/// tracker, then emulates `mov ip, sp` (ctx.x[12] = source-mode
/// banked SP) so the function prologue continues correctly. See
/// PLAN.md "Next iteration — probe `PrimRememberMapping`".
pub const PRIM_REMEMBER_PROBE_HVC_IMM: u32 = 0x54;
pub const PRIM_REMEMBER_PC:            u32 = 0x0016_3480;
const PRIM_REMEMBER_FIRST_INSN:        u32 = 0xE1A0_C00D; // mov ip, sp

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
const NEW_STACK_PAD_WRAPPER_PC:   u32 = 0x00FF_FE80;
/// Original first-word at the BL site — used to assert the patch
/// applies to the expected ROM. `bl 0x001bd7ba4` from PC 0x25238c
/// has offset bytes `(0x1bd7ba4 - 0x252394) = 0x1985810`, off in
/// words = `0x6_6160`, encoded as `0xEB66_1604`.
const NEW_STACK_PAD_BL_ORIG_INSN: u32 = 0xEB66_1604;

/// `PrimForgetMapping(va, &TPhys)` probe at ROM `0x00163514`. The
/// counterpart to `PrimRememberMapping`: removes the (VA, PA)
/// mapping at the L2 layer. Pairing this with the Remember probe
/// lets us discriminate "real" aliases (PA installed at VA' before
/// the prior install at VA was forgotten) from "expected" PA reuse
/// (PA properly forgotten between installs). Function signature is
/// `PrimForgetMapping(va=r0, &TPhys=r1)`. Original first word is
/// `mov ip, sp`. The handler clears the per-PA → first-VA tracker
/// slot iff the cleared VA matches the previously-recorded one;
/// mismatch logs `FORGET MISMATCH:` for diagnosis.
pub const PRIM_FORGET_PROBE_HVC_IMM:   u32 = 0x55;
pub const PRIM_FORGET_PC:              u32 = 0x0016_3514;
const PRIM_FORGET_FIRST_INSN:          u32 = 0xE1A0_C00D; // mov ip, sp

/// `IdleProc__18TAlertEventHandlerFP10TUMsgTokenPUlP7TAEvent` probe
/// at ROM `0x000309EC`. This is the alert-handler idle-poll function
/// where the alrt-task DABT decoded by commit 0ed81e20 originates:
/// IdleProc reads `r0 = this->[+20]; r0 += 0x8c; r5 = CList::At(r0,
/// 0)` and gets a junk dialog pointer (`0xE3360000` = ARM `teq r6,
/// #0` instruction-encoded bytes that ended up in the CList entries
/// array). The downstream `bl CheckAlertDone` (0x30A3C / 0x30A64)
/// then `bl CheckButton`, and `ldr r0, [r0, #12]` at PC 0x0002EABC
/// faults with FAR=0xe336000c.
///
/// Probe fires at IdleProc's first instruction (the standard
/// `mov ip, sp` AArch32 prologue). Handler reads:
///   - `this = ctx.x[0]` (TAlertEventHandler*)
///   - `inner = *(this + 0x14)`
///   - CList header at `inner + 0x8c`: count [+0], elem_size [+4],
///     entries_base [+0x10]
///   - first few entries (= the dialog pointers)
/// then emulates `mov ip, sp` so the function continues.
///
/// The point: capture CList contents on every IdleProc call so we
/// see when the junk pointer first appears. Combined with the
/// timeline ordering against other probes, that pinpoints the
/// corrupting writer.
pub const IDLEPROC_PROBE_HVC_IMM:      u32 = 0x56;
pub const IDLEPROC_PROBE_PC:           u32 = 0x0003_09EC;
const IDLEPROC_FIRST_INSN:             u32 = 0xE1A0_C00D; // mov ip, sp

/// `__nw__FUi` (operator new) probe — paired ENTRY / RETURN hooks
/// at ROM `0x00318ee8` and `0x00318f1c` so we can correlate
/// `(size_requested, returned_addr, caller_LR)` per allocation and
/// detect overlapping live blocks.
///
/// The user's hypothesis (2026-04-28) is that the corruption at the
/// alrt task's TAlertEventHandler CList header is heap allocator
/// chaos — two distinct `__nw__` calls overlap in physical address
/// because the allocator's free-list / block bookkeeping is broken
/// under our flat AP=11. The probe tests this directly: log every
/// (size, addr) pair, watch for overlaps.
///
/// Entry insn at 0x318ee8 is the standard `mov ip, sp`. Return-site
/// insn at 0x318f1c is `mov r0, r4` (loading the allocated address
/// from the saved register before the function returns). Both get
/// patched with HVCs; the handlers preserve the original effect
/// (ctx.x[12]=sp at entry; ctx.x[0]=ctx.x[4] at return).
pub const NW_ENTRY_PROBE_HVC_IMM:  u32 = 0x57;
pub const NW_ENTRY_PROBE_PC:       u32 = 0x0031_8EE8;
const NW_ENTRY_FIRST_INSN:         u32 = 0xE1A0_C00D; // mov ip, sp

pub const NW_RETURN_PROBE_HVC_IMM: u32 = 0x58;
pub const NW_RETURN_PROBE_PC:      u32 = 0x0031_8F1C;
const NW_RETURN_ORIG_INSN:         u32 = 0xE1A0_0004; // mov r0, r4

/// `__dl__FPv` (operator delete) probe at ROM `0x00318F28`. The
/// original word is a single `b free` (target `0x01BD2958` in REx).
/// The handler reads `r0` (= block to free), clears the matching
/// NW_TABLE entry, then redirects ELR_EL2 to the free entry so the
/// guest tail-calls into the actual free implementation.
///
/// Pairs with the `__nw__` entry/return probes to give a live-allocation
/// tracker that distinguishes legitimate recycle (alloc → free → alloc
/// at same address) from the kernel-allocator overlap bug we suspect.
pub const DL_PROBE_HVC_IMM:        u32 = 0x59;
pub const DL_PROBE_PC:             u32 = 0x0031_8F28;
const DL_ORIG_INSN:                u32 = 0xEA62_E68A; // b 0x01bd2958 <free>
/// Branch target of `__dl__`'s `b free`. Used by the HVC handler to
/// set ELR_EL2 so execution continues into free after we record the
/// deallocation.
pub const DL_FREE_TARGET_PC:       u32 = 0x01BD_2958;

/// `LockHeapRange` user-shim entry probe — patches the standard
/// `mov ip, sp` prologue at ROM `0x001F_8AB4` with HVC. The handler
/// logs `(r0=base, r1=limit, r2=lock_id_byte, caller_lr)` so we can
/// see exactly what range every kernel/REx caller asks the
/// `FMLockHeapRange` SWI to lock. The boot wedge after the iter-12
/// 36-KiB stack patch is an infinite ResolveFault loop where
/// `limit` is one 4-KiB page past the heap's current top — this
/// probe is the first step in identifying which caller computes
/// the bad `limit`.
pub const LOCK_HEAP_RANGE_PROBE_HVC_IMM: u32 = 0x5A;
pub const LOCK_HEAP_RANGE_PROBE_PC:      u32 = 0x001F_8AB4;
const LOCK_HEAP_RANGE_FIRST_INSN:        u32 = 0xE1A0_C00D; // mov ip, sp

/// `ExtendVMHeap` entry probe — patches the standard `mov ip, sp`
/// prologue at ROM `0x0031_091C` with HVC. The handler logs
/// `(r0=heap, r1=requested_size, current_top=heap[+0x2c],
/// chunk_size=heap[+0x38], reserved_end=heap[+0x28], caller_lr)`
/// so we can see whether the allocator's requested-extend size
/// covers the full block being placed near the heap top.
///
/// Iter 15 wedge analysis: `LockHeapRange #76` extended only one
/// 4-KiB chunk (base=0xc646000 limit=0xc647000) but the placed
/// 420-byte `TUnicodeCompressor` object spilled past the new top
/// at offset +0xa1 onwards. Either `ExtendVMHeap` is being called
/// with a too-small `r1` or its rounding/chunk-size logic isn't
/// covering the full allocation footprint.
pub const EXTEND_VM_HEAP_PROBE_HVC_IMM:  u32 = 0x5B;
pub const EXTEND_VM_HEAP_PROBE_PC:       u32 = 0x0031_091C;
const EXTEND_VM_HEAP_FIRST_INSN:         u32 = 0xE1A0_C00D; // mov ip, sp

/// `NewBlock` entry probe — patches the standard `mov ip, sp`
/// prologue at ROM `0x0031_1DB8` with HVC. The handler captures
/// `(r0=requested_size, sp, caller_lr)` and stores it in a
/// per-sp ring so the exit probe can pair the call.
///
/// Iter 16 ruled out ExtendVMHeap as the heap-extend cause; the
/// 420-byte compressor block at `0xc646f60` (spilling past the
/// heap top of `0xc647000`) must come from NewBlock's freelist
/// placement. This probe + the matching exit probe log every
/// `(size, returned_block, caller_lr)` triple to find which
/// allocation lands there.
pub const NEW_BLOCK_ENTRY_PROBE_HVC_IMM: u32 = 0x5C;
pub const NEW_BLOCK_ENTRY_PROBE_PC:      u32 = 0x0031_1DB8;
const NEW_BLOCK_ENTRY_FIRST_INSN:        u32 = 0xE1A0_C00D; // mov ip, sp

/// `NewBlock` success-return probe — patches the `mov r0, r6` at
/// ROM `0x0031_1ED8` (one instruction before the LDMDB return).
/// The handler logs `(returned_block=r6, size=from-pending,
/// caller_lr=from-pending)` and emulates `mov r0, r6` so the
/// LDMDB at 0x311EDC fires normally and returns the value.
pub const NEW_BLOCK_RETURN_PROBE_HVC_IMM: u32 = 0x5D;
pub const NEW_BLOCK_RETURN_PROBE_PC:      u32 = 0x0031_1ED8;
const NEW_BLOCK_RETURN_FIRST_INSN:        u32 = 0xE1A0_0006; // mov r0, r6

/// `WriteRun__18TUnicodeCompressorFv` entry probe — patches the
/// standard `mov ip, sp` prologue at ROM `0x00256EEC` with HVC.
/// Handler logs `(this, this->count [+0x9c], this->byte_a0
/// [+0xa0], this->w98 [+0x98], buffer_b first 4 bytes [+0xa1],
/// caller_lr)`. Pinpoints whether `count` is corrupted at the
/// moment WriteRun is invoked (iter 17 hypothesis: count > 870
/// means uninitialized post-NewBlock).
pub const WRITE_RUN_PROBE_HVC_IMM:        u32 = 0x5E;
pub const WRITE_RUN_PROBE_PC:             u32 = 0x0025_6EEC;
const WRITE_RUN_FIRST_INSN:               u32 = 0xE1A0_C00D; // mov ip, sp

/// `WriteChunk__18TUnicodeCompressorFPvl` entry probe — patches
/// `mov ip, sp` at ROM `0x0025700C`. Handler logs `(this, ptr,
/// length, this->count, caller_lr)` so we can identify the kernel
/// site that calls WriteChunk on a compressor whose count holds
/// stale heap garbage.
pub const WRITE_CHUNK_PROBE_HVC_IMM:      u32 = 0x5F;
pub const WRITE_CHUNK_PROBE_PC:           u32 = 0x0025_700C;
const WRITE_CHUNK_FIRST_INSN:             u32 = 0xE1A0_C00D; // mov ip, sp

/// `New__18TUnicodeCompressorFv` probe — patches the function's
/// FIRST instruction at ROM `0x00256C7C` (a leaf function that
/// doesn't push). The original first insn is `mov r1, #0`. The
/// handler logs `(this=r0, caller_lr)` so we can confirm whether
/// New is ever invoked for the wedging compressor.
pub const COMP_NEW_PROBE_HVC_IMM:         u32 = 0x60;
pub const COMP_NEW_PROBE_PC:              u32 = 0x0025_6C7C;
const COMP_NEW_FIRST_INSN:                u32 = 0xE3A0_1000; // mov r1, #0

/// `Reset__18TUnicodeCompressorFv` probe — patches the function's
/// FIRST instruction at ROM `0x00256ED8`. Original is
/// `mov r1, #0`; handler logs `(this=r0, caller_lr)`.
pub const COMP_RESET_PROBE_HVC_IMM:       u32 = 0x61;
pub const COMP_RESET_PROBE_PC:            u32 = 0x0025_6ED8;
const COMP_RESET_FIRST_INSN:              u32 = 0xE3A0_1000; // mov r1, #0

/// `WriteChunk` count-load probe — patches the
/// `ldr r0, [r4, #156]` at ROM `0x00257074`. The probe fires
/// once per loop iteration (right before the count check).
/// Handler logs `(this=r4, count_value)` and emulates the load.
/// Iter 19 saw count flip 0 → 0x20000110 between WriteChunk
/// entry and WriteRun entry; this probe pinpoints the iteration
/// where the jump happens.
pub const WC_LOAD_PROBE_HVC_IMM:          u32 = 0x62;
pub const WC_LOAD_PROBE_PC:               u32 = 0x0025_7074;
const WC_LOAD_FIRST_INSN:                 u32 = 0xE594_009C; // ldr r0, [r4, #156]

/// `WriteChunk` count-store probe — patches the
/// `str r1, [r4, #156]` at ROM `0x00257090` (PATH B's count++).
/// Handler logs `(this=r4, r1_value, count_in_memory_before)`
/// and emulates the store.
pub const WC_STORE_PROBE_HVC_IMM:         u32 = 0x63;
pub const WC_STORE_PROBE_PC:              u32 = 0x0025_7090;
const WC_STORE_FIRST_INSN:                u32 = 0xE584_109C; // str r1, [r4, #156]

/// `WriteChunk` count-reload probe — patches the
/// `ldr r0, [r4, #156]` at ROM `0x0025709C` (re-read just
/// before the cmp #255 / bl WriteRun decision). Handler logs
/// `(this=r4, count_in_memory)` and emulates the load.
pub const WC_RELOAD_PROBE_HVC_IMM:        u32 = 0x64;
pub const WC_RELOAD_PROBE_PC:             u32 = 0x0025_709C;
const WC_RELOAD_FIRST_INSN:               u32 = 0xE594_009C; // ldr r0, [r4, #156]

/// `WriteChunk` count-add probe — patches the
/// `add r1, r0, #1` at ROM `0x0025708C` (PATH B's increment).
/// Logs r0 right before the add fires and emulates the add by
/// setting r1 = r0 + 1. Pins whether r0 is correct after the
/// WC-load probe's ERET.
pub const WC_ADD_PROBE_HVC_IMM:           u32 = 0x65;
pub const WC_ADD_PROBE_PC:                u32 = 0x0025_708C;
const WC_ADD_FIRST_INSN:                  u32 = 0xE280_1001; // add r1, r0, #1

/// `WriteChunk` post-WC-load probe — patches the
/// `cmp r0, #0` at ROM `0x00257078`, the FIRST instruction
/// after the count-load. Logs r0 right after the WC-load
/// probe's ERET to verify the probe's r0 update propagates.
/// Emulates cmp by updating NZ flags via SPSR.
pub const WC_POSTLOAD_PROBE_HVC_IMM:      u32 = 0x66;
pub const WC_POSTLOAD_PROBE_PC:           u32 = 0x0025_7078;
const WC_POSTLOAD_FIRST_INSN:             u32 = 0xE350_0000; // cmp r0, #0

/// `WriteChunk` post-LDRB probe — patches the
/// `teq r1, sl` at ROM `0x00257084`, immediately after the
/// shadow-stub-patched `ldrb r1, [r4, #160]` at 0x00257080.
/// Logs r0 right after the LDRB stub returns. Iter-26 statically
/// proved `pick_scratch_regs` for the LDRB picks (R12, R2) — never
/// R0; this probe's job is to confirm at runtime. If r0 here is
/// the sentinel set by the WC-postload probe (`0x12345678` under
/// the iter-25 sentinel test, or the real count value otherwise),
/// the LDRB stub is innocent and the WC-add corruption arises
/// later (TEQ flag effect on bne path, or async IRQ between this
/// probe and WC-add).
///
/// Emulates teq r1, sl by setting Z = (r1 == sl), N = MSB(r1 ^ sl)
/// in saved SPSR. C, V unchanged (TEQ leaves them untouched per
/// ARM ARM A8.8.236). Caveat (iter-27): the SPSR write to
/// UND_SAVE_SPSR_IPA does NOT reach banked SPSR_und (the UND-return
/// stub uses the banked register, not memory). Iter-28 attempted
/// to fix the plumbing via MSR SPSR_cxsf in the stub but
/// QEMU raspi3b mishandles that instruction. The flag-emulation
/// here is therefore advisory; the WC-bne probe (iter-29) emulates
/// the bne control-flow decision directly via ELR_EL2 instead.
pub const WC_POSTLDRB_PROBE_HVC_IMM:      u32 = 0x67;
pub const WC_POSTLDRB_PROBE_PC:           u32 = 0x0025_7084;
const WC_POSTLDRB_FIRST_INSN:             u32 = 0xE131_000A; // teq r1, sl

/// `WriteChunk` BNE probe — patches the `bne 0x2570c0` at ROM
/// `0x00257088`. Logs r0 right at the conditional branch, between
/// WC-postldrb (0x257084) and WC-add (0x25708c). Bypasses SPSR by
/// computing Z = (r1 == sl) directly from `ctx.x[1]` and `ctx.x[10]`
/// (TEQ at 0x257084 is read-only, so r1 still holds the LDRB
/// result). The handler routes ELR_EL2 to the BNE target
/// (0x002570C0) when Z=0, or to the fall-through (0x0025708C) when
/// Z=1, sidestepping the QEMU raspi3b MSR SPSR quirk discovered in
/// iter-28.
pub const WC_BNE_PROBE_HVC_IMM:           u32 = 0x68;
pub const WC_BNE_PROBE_PC:                u32 = 0x0025_7088;
const WC_BNE_FIRST_INSN:                  u32 = 0x1A00_000C; // bne 0x2570c0
pub const WC_BNE_TAKEN_TARGET:            u32 = 0x0025_70C0;
pub const WC_BNE_FALLTHROUGH_TARGET:      u32 = 0x0025_708C;

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

/// CardFaultMonProc `bl Throw` at ROM `0x0004_E660`. This is the
/// second of two `bl Throw` sites inside
/// `CardFaultMonProc__12TCardDomainsFlPv` (the other is at
/// `0x0004_E528`); iter-35 pinned the firing site to
/// `0x0004_E660` via `caller_lr=0x0004_E664`. Catching here gives
/// us the kernel-side fault frame BEFORE Throw unwinds anything
/// — `sp..sp+0x64` holds the 25-word `TProcessorState` populated
/// by `GetFaultState__FP15TProcessorState` at `0x0004_E4FC`. Per
/// the surrounding disassembly: `sp+0x44` = FAR, `sp+0x48` =
/// DFSR/access bits (bit 0 tested as the "USR vs kernel access"
/// distinction), `sp+0x58` = the offending USR-mode PC. r0 is
/// loaded from `[0x003712C4]` and re-loaded as `*(0x003712C4)` —
/// the pointer to the exception-name C-string ("evt.ex.abt.perm").
/// r4 indicates the path taken (0 = main "no matching domain",
/// 5 = NotifyTaskBlocked already invoked).
pub const CARDFAULT_THROW_PROBE_HVC_IMM:  u32 = 0x6B;
pub const CARDFAULT_THROW_PROBE_PC:       u32 = 0x0004_E660;
const CARDFAULT_THROW_FIRST_INSN:         u32 = 0xEB6E_52CD; // bl 0x1be319c <Throw>

/// `Lookup__11TFlashStoreFUliR7TObjRef` entry at ROM `0x000C_747C`
/// (TFlashStore::Lookup). Iter-37 pinned the wedge call chain to
/// `?→UnlockStore→DoCommit→FindSuperceeder→Lookup→Set`, with the
/// faulting `r0` for Set's `str r1, [r0, #8]` coming from Lookup's
/// `mov r0, r6` at 0x000C_74AC, where r6 was loaded from r3 at
/// 0x000C_7494. Iter-38 captures (r0..r3, lr, sp) per call to
/// pinpoint the caller passing a wild &TObjRef OUT-param. The
/// handler keeps a 64-slot ring buffer and halts on the first
/// call with bit-31 of r3 set (= 0x80000110 family).
pub const LOOKUP_ENTRY_PROBE_HVC_IMM:    u32 = 0x6C;
pub const LOOKUP_ENTRY_PROBE_PC:         u32 = 0x000C_747C;
const LOOKUP_ENTRY_FIRST_INSN:           u32 = 0xE1A0_C00D; // mov ip, sp

/// `FindSuperceeder__7TObjRefFR7TObjRef` entry at ROM `0x0014_88A0`.
/// Iter-38 confirmed Lookup is reached with r3=0x80000110 from the
/// DoCommit-c96c8 path via FindSuperceeder's tail call. Iter-39
/// bisects: this probe fires BEFORE FindSuperceeder runs, capturing
/// (r0, r1, r2, r3, lr, sp). If r1 here is already 0x80000110, the
/// bug is in DoCommit (its `add r1, sp, #120` somehow yields a
/// kernel-space VA). If r1 is sane (= sp+120 stack address), then
/// something between FindSuperceeder entry and Lookup entry is
/// overwriting r3 — even though the disassembly says only `mov
/// r3, r1` writes r3.
pub const FINDSUPER_ENTRY_PROBE_HVC_IMM: u32 = 0x6D;
pub const FINDSUPER_ENTRY_PROBE_PC:      u32 = 0x0014_88A0;
const FINDSUPER_ENTRY_FIRST_INSN:        u32 = 0xE1A0_3001; // mov r3, r1

/// Mid-FindSuperceeder probe at ROM `0x0014_88C4` (`mov r0, ip` =
/// 0xE1A0_000C). Iter-40 bisects shadow_stub-stub-corruption vs
/// post-stub-chain corruption: if r3 is sane (0x0c328e90) at this
/// PC, the corruption happens in the b 0x01afef70 → b 0x000c747c
/// chain (only 2 instructions, both unconditional branches with
/// no register writes — extremely suspicious). If r3 is already
/// wild here, the corruption happened earlier in the body (either
/// in the shadow_stub stub at 1488ac, or in the natural ldrb /
/// teq / mov / ldr / bic / mov sequence at 1488a4..1488c0).
pub const FINDSUPER_MID_PROBE_HVC_IMM:   u32 = 0x6E;
pub const FINDSUPER_MID_PROBE_PC:        u32 = 0x0014_88C4;
const FINDSUPER_MID_FIRST_INSN:          u32 = 0xE1A0_000C; // mov r0, ip

/// `Throw` entry at ROM `0x000B_00C8`. The kernel exception-throw
/// primitive — every Throw goes through this. r0 = exception name
/// pointer, r1 = data, r2 = handler. LR at entry = caller's resume
/// PC, which uniquely identifies the throw site. iter-43 logs every
/// Throw call (with name string, args, caller LR) so the bus-abort
/// throw site is identified without probing 20+ candidate sites.
pub const THROW_ENTRY_PROBE_HVC_IMM:     u32 = 0x6F;
pub const THROW_ENTRY_PROBE_PC:          u32 = 0x000B_00C8;
const THROW_ENTRY_FIRST_INSN:            u32 = 0xE1A0_C00D; // mov ip, sp

/// `PhysBlock__11TFlashBlockFv` first insn at ROM `0x000c_0cc4`.
/// Iter-43 pinned the `evt.ex.abt.bus` original throw to the
/// `bl PhysBlock` site at 0xc0cb8 (caller_lr=0xc0cbc). The fault
/// is suspected on PhysBlock's first instruction `ldr r1, [r0, #8]`
/// when r0 (TFlashBlock* this) is wild. iter-44 patches the ldr
/// with HVC; the handler captures r0 and dumps caller context, then
/// either emulates the load (if r0 looks sane) or halts.
pub const PHYSBLOCK_ENTRY_PROBE_HVC_IMM: u32 = 0x70;
pub const PHYSBLOCK_ENTRY_PROBE_PC:      u32 = 0x000C_0CC4;
const PHYSBLOCK_ENTRY_FIRST_INSN:        u32 = 0xE590_1008; // ldr r1, [r0, #8]

/// Wrapper at ROM `0x000c_0cac` — the unnamed function whose
/// `bl PhysBlock; ldmdb fp, ...; b LogEntryOffset` faults at the
/// ldmdb (iter-44). Probe its first insn (mov ip, sp) to capture
/// incoming fp / sp / lr / r0..r3 on every call. Halt when fp is
/// wild (bit-31 set).
pub const C0CAC_ENTRY_PROBE_HVC_IMM:     u32 = 0x71;
pub const C0CAC_ENTRY_PROBE_PC:          u32 = 0x000C_0CAC;
const C0CAC_ENTRY_FIRST_INSN:            u32 = 0xE1A0_C00D; // mov ip, sp

/// `safeIntervalDeltaSeconds` from `TJITGenericROMPatch.cpp:144` —
/// seconds between 1993-01-01 and 2008-01-01, Einstein's Y2010 fix
/// constant.
const SAFE_INTERVAL_DELTA_SECONDS: u32 = 473_299_200;

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
        unsafe {
            let prev = rom_ptr.add(word_idx).read();
            rom_ptr.add(word_idx).write(p.value);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({})",
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
    }

    kprintln!("rom_patch: applied {} simple patches + 5 native-call/injection ROM patches + PowerOffAndReboot + Reboot + BootOS + ResolveFault-wrapper + L1[0xCD] probes", applied);
}

/// Install the three HVC probes for the L1[0xCD] lazy-grow investigation.
/// See the comment block by `REMEMBER_PROBE_HVC_IMM` above for context.
unsafe fn apply_l1_cd_probes(rom_ptr: *mut u32) {
    unsafe {
        patch_probe(
            rom_ptr,
            REMEMBER_STATIC_PC,
            REMEMBER_STATIC_FIRST_INSN,
            hvc_insn(REMEMBER_PROBE_HVC_IMM),
            "Remember static entry",
            REMEMBER_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            REMEMBER_SWIRET_PC,
            REMEMBER_SWIRET_ORIG_INSN,
            hvc_insn(REMEMBER_SWIRET_HVC_IMM),
            "Remember post-SWI",
            REMEMBER_SWIRET_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            ALLOC_PT_STATIC_PC,
            ALLOC_PT_FIRST_INSN,
            hvc_insn(ALLOC_PT_PROBE_HVC_IMM),
            "AllocatePageTable static entry",
            ALLOC_PT_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            FILL_STATIC_PC,
            FILL_FIRST_INSN,
            hvc_insn(FILL_PROBE_HVC_IMM),
            "Fill__15TRefStructStackFv prologue",
            FILL_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            NEW_STACK_LOW_LDR_PC,
            NEW_STACK_LOW_LDR_INSN,
            hvc_insn(NEW_STACK_PROBE_HVC_IMM),
            "NewStack post-SWI ldr r1, [sp,#16]",
            NEW_STACK_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            STACK_MGR_FAULT_PC,
            STACK_MGR_FAULT_FIRST_INSN,
            hvc_insn(STACK_MGR_FAULT_PROBE_HVC_IMM),
            "TStackManager::Fault prologue",
            STACK_MGR_FAULT_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            RESOLVE_FAULT_ENTRY_PC,
            RESOLVE_FAULT_FIRST_INSN,
            hvc_insn(RESOLVE_FAULT_PROBE_HVC_IMM),
            "TStackManager::ResolveFault prologue",
            RESOLVE_FAULT_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            NEW_STATE_PC,
            NEW_STATE_FIRST_INSN,
            hvc_insn(NEW_STATE_PROBE_HVC_IMM),
            "NewState__11TIntrpStackFv prologue",
            NEW_STATE_PROBE_HVC_IMM,
        );
        // DataAbortHandler exit probes — same encoding at two PCs.
        patch_probe(
            rom_ptr,
            DAH_USR_RETURN_PC,
            MOVS_PC_LR_INSN,
            hvc_insn(DAH_USR_RETURN_PROBE_HVC_IMM),
            "DataAbortHandler success exit (movs pc, lr)",
            DAH_USR_RETURN_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            DAH_THROW_EXIT_PC,
            MOVS_PC_LR_INSN,
            hvc_insn(DAH_USR_RETURN_PROBE_HVC_IMM),
            "DataAbortHandler throw exit (movs pc, lr)",
            DAH_USR_RETURN_PROBE_HVC_IMM,
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
        // PrimRememberMapping prologue probe at 0x00163480. The
        // lower-level L2-write primitive that the kernel calls from
        // paths bypassing `Remember (static)`; suspected source of
        // the Group-2 stack-guard aliases. Handler captures
        // (env, va, PA, perm) and runs the per-PA → first-VA
        // aliasing tracker, then emulates the original `mov ip, sp`.
        patch_probe(
            rom_ptr,
            PRIM_REMEMBER_PC,
            PRIM_REMEMBER_FIRST_INSN,
            hvc_insn(PRIM_REMEMBER_PROBE_HVC_IMM),
            "PrimRememberMapping prologue",
            PRIM_REMEMBER_PROBE_HVC_IMM,
        );
        // PrimForgetMapping prologue probe at 0x00163514. Companion to
        // the Remember probe — clears the per-PA → first-VA tracker
        // slot when the kernel forgets a mapping, so subsequent
        // re-installs at a different VA only register as aliases when
        // they LACK a preceding forget.
        patch_probe(
            rom_ptr,
            PRIM_FORGET_PC,
            PRIM_FORGET_FIRST_INSN,
            hvc_insn(PRIM_FORGET_PROBE_HVC_IMM),
            "PrimForgetMapping prologue",
            PRIM_FORGET_PROBE_HVC_IMM,
        );
        // IdleProc__18TAlertEventHandler probe at 0x000309EC. Captures
        // the alrt-task's TAlertEventHandler CList state on every
        // idle-poll call so we can see when the corrupting junk
        // pointer (= 0xE3360000, decoded as ARM teq r6, #0 bytes)
        // first appears in CList entries[0..N]. Per the alrt-task
        // DABT decoding in commit 0ed81e20.
        patch_probe(
            rom_ptr,
            IDLEPROC_PROBE_PC,
            IDLEPROC_FIRST_INSN,
            hvc_insn(IDLEPROC_PROBE_HVC_IMM),
            "IdleProc__18TAlertEventHandler prologue",
            IDLEPROC_PROBE_HVC_IMM,
        );
        // __nw__ entry/return probe pair. Entry captures
        // (size, caller_LR); return captures the allocated address
        // and pairs them via a per-CPU pending-call slot. Together
        // they let the handler log every operator-new allocation
        // with full (size, addr, caller_LR) and detect overlaps
        // between live blocks (the user-suspected mode for the alrt
        // CList corruption).
        patch_probe(
            rom_ptr,
            NW_ENTRY_PROBE_PC,
            NW_ENTRY_FIRST_INSN,
            hvc_insn(NW_ENTRY_PROBE_HVC_IMM),
            "__nw__FUi entry",
            NW_ENTRY_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            NW_RETURN_PROBE_PC,
            NW_RETURN_ORIG_INSN,
            hvc_insn(NW_RETURN_PROBE_HVC_IMM),
            "__nw__FUi return",
            NW_RETURN_PROBE_HVC_IMM,
        );
        // __dl__FPv (operator delete) probe — captures every free
        // and clears the matching NW_TABLE slot so we can tell
        // legitimate recycle from real overlap. Handler also
        // redirects ELR_EL2 to the free entry (DL_FREE_TARGET_PC)
        // since the original instruction was a `b free` that we've
        // overwritten with our HVC.
        patch_probe(
            rom_ptr,
            DL_PROBE_PC,
            DL_ORIG_INSN,
            hvc_insn(DL_PROBE_HVC_IMM),
            "__dl__FPv tail-call to free",
            DL_PROBE_HVC_IMM,
        );
        // LockHeapRange shim entry probe — captures (base, limit,
        // lock_id, caller_lr) on every user-mode LockHeapRange call.
        // Diagnoses the iter-13 ResolveFault loop where limit is
        // one page past heap top.
        patch_probe(
            rom_ptr,
            LOCK_HEAP_RANGE_PROBE_PC,
            LOCK_HEAP_RANGE_FIRST_INSN,
            hvc_insn(LOCK_HEAP_RANGE_PROBE_HVC_IMM),
            "LockHeapRange entry",
            LOCK_HEAP_RANGE_PROBE_HVC_IMM,
        );
        // ExtendVMHeap entry probe — captures (heap, requested_size,
        // current_top, chunk_size, reserved_end, caller_lr) per call.
        // Diagnoses the iter-15 wedge where the allocator places a
        // 420-byte block past the freshly-extended heap top.
        patch_probe(
            rom_ptr,
            EXTEND_VM_HEAP_PROBE_PC,
            EXTEND_VM_HEAP_FIRST_INSN,
            hvc_insn(EXTEND_VM_HEAP_PROBE_HVC_IMM),
            "ExtendVMHeap entry",
            EXTEND_VM_HEAP_PROBE_HVC_IMM,
        );
        // NewBlock entry+return probes — capture every block
        // allocation's (size, returned_addr, caller_lr) triple to
        // find the one placing a 420-byte block at 0xc646f60.
        patch_probe(
            rom_ptr,
            NEW_BLOCK_ENTRY_PROBE_PC,
            NEW_BLOCK_ENTRY_FIRST_INSN,
            hvc_insn(NEW_BLOCK_ENTRY_PROBE_HVC_IMM),
            "NewBlock entry",
            NEW_BLOCK_ENTRY_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            NEW_BLOCK_RETURN_PROBE_PC,
            NEW_BLOCK_RETURN_FIRST_INSN,
            hvc_insn(NEW_BLOCK_RETURN_PROBE_HVC_IMM),
            "NewBlock success-return",
            NEW_BLOCK_RETURN_PROBE_HVC_IMM,
        );
        // WriteRun entry probe — captures (this, count, buffer_b
        // first 4 bytes, caller_lr) per call. Identifies which
        // compressor instance is wedging WriteRun with count >871.
        patch_probe(
            rom_ptr,
            WRITE_RUN_PROBE_PC,
            WRITE_RUN_FIRST_INSN,
            hvc_insn(WRITE_RUN_PROBE_HVC_IMM),
            "WriteRun entry",
            WRITE_RUN_PROBE_HVC_IMM,
        );
        // WriteChunk entry probe — captures the kernel caller
        // that mis-uses the compressor (count holds heap junk).
        patch_probe(
            rom_ptr,
            WRITE_CHUNK_PROBE_PC,
            WRITE_CHUNK_FIRST_INSN,
            hvc_insn(WRITE_CHUNK_PROBE_HVC_IMM),
            "WriteChunk entry",
            WRITE_CHUNK_PROBE_HVC_IMM,
        );
        // New__18TUnicodeCompressor / Reset__18TUnicodeCompressor
        // probes — confirms whether either is invoked for the
        // wedging compressor instance (we expect "no").
        patch_probe(
            rom_ptr,
            COMP_NEW_PROBE_PC,
            COMP_NEW_FIRST_INSN,
            hvc_insn(COMP_NEW_PROBE_HVC_IMM),
            "TUnicodeCompressor::New entry",
            COMP_NEW_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            COMP_RESET_PROBE_PC,
            COMP_RESET_FIRST_INSN,
            hvc_insn(COMP_RESET_PROBE_HVC_IMM),
            "TUnicodeCompressor::Reset entry",
            COMP_RESET_PROBE_HVC_IMM,
        );
        // WriteChunk count-load probe — re-enabled iter 24 after
        // confirming the wedge fires identically without it.
        patch_probe(
            rom_ptr,
            WC_LOAD_PROBE_PC,
            WC_LOAD_FIRST_INSN,
            hvc_insn(WC_LOAD_PROBE_HVC_IMM),
            "WriteChunk count-load",
            WC_LOAD_PROBE_HVC_IMM,
        );
        // Iter 24: probe the count store and the immediate
        // re-read so we can see whether the store wrote the
        // expected value and what the re-read sees.
        patch_probe(
            rom_ptr,
            WC_STORE_PROBE_PC,
            WC_STORE_FIRST_INSN,
            hvc_insn(WC_STORE_PROBE_HVC_IMM),
            "WriteChunk count-store",
            WC_STORE_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            WC_RELOAD_PROBE_PC,
            WC_RELOAD_FIRST_INSN,
            hvc_insn(WC_RELOAD_PROBE_HVC_IMM),
            "WriteChunk count-reload",
            WC_RELOAD_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            WC_ADD_PROBE_PC,
            WC_ADD_FIRST_INSN,
            hvc_insn(WC_ADD_PROBE_HVC_IMM),
            "WriteChunk count-add",
            WC_ADD_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            WC_POSTLOAD_PROBE_PC,
            WC_POSTLOAD_FIRST_INSN,
            hvc_insn(WC_POSTLOAD_PROBE_HVC_IMM),
            "WriteChunk post-load cmp",
            WC_POSTLOAD_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            WC_POSTLDRB_PROBE_PC,
            WC_POSTLDRB_FIRST_INSN,
            hvc_insn(WC_POSTLDRB_PROBE_HVC_IMM),
            "WriteChunk post-LDRB teq",
            WC_POSTLDRB_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            WC_BNE_PROBE_PC,
            WC_BNE_FIRST_INSN,
            hvc_insn(WC_BNE_PROBE_HVC_IMM),
            "WriteChunk bne (control-flow emulator)",
            WC_BNE_PROBE_HVC_IMM,
        );
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
        patch_probe(
            rom_ptr,
            CARDFAULT_THROW_PROBE_PC,
            CARDFAULT_THROW_FIRST_INSN,
            hvc_insn(CARDFAULT_THROW_PROBE_HVC_IMM),
            "CardFaultMonProc bl Throw (pre-Throw fault-frame capture)",
            CARDFAULT_THROW_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            LOOKUP_ENTRY_PROBE_PC,
            LOOKUP_ENTRY_FIRST_INSN,
            hvc_insn(LOOKUP_ENTRY_PROBE_HVC_IMM),
            "TFlashStore::Lookup entry (ring + halt-on-wild-r3)",
            LOOKUP_ENTRY_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            FINDSUPER_ENTRY_PROBE_PC,
            FINDSUPER_ENTRY_FIRST_INSN,
            hvc_insn(FINDSUPER_ENTRY_PROBE_HVC_IMM),
            "FindSuperceeder entry (ring + halt-on-wild-r1)",
            FINDSUPER_ENTRY_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            FINDSUPER_MID_PROBE_PC,
            FINDSUPER_MID_FIRST_INSN,
            hvc_insn(FINDSUPER_MID_PROBE_HVC_IMM),
            "FindSuperceeder mid-body @1488c4 (capture r3 post-stub)",
            FINDSUPER_MID_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            THROW_ENTRY_PROBE_PC,
            THROW_ENTRY_FIRST_INSN,
            hvc_insn(THROW_ENTRY_PROBE_HVC_IMM),
            "Throw entry (log every kernel exception throw)",
            THROW_ENTRY_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            PHYSBLOCK_ENTRY_PROBE_PC,
            PHYSBLOCK_ENTRY_FIRST_INSN,
            hvc_insn(PHYSBLOCK_ENTRY_PROBE_HVC_IMM),
            "PhysBlock entry (capture r0 = TFlashBlock* this; halt if wild)",
            PHYSBLOCK_ENTRY_PROBE_HVC_IMM,
        );
        patch_probe(
            rom_ptr,
            C0CAC_ENTRY_PROBE_PC,
            C0CAC_ENTRY_FIRST_INSN,
            hvc_insn(C0CAC_ENTRY_PROBE_HVC_IMM),
            "wrapper @c0cac entry (capture incoming fp; halt if wild)",
            C0CAC_ENTRY_PROBE_HVC_IMM,
        );
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
    let prev = unsafe { rom_ptr.add(idx).read() };
    if prev != expected_orig {
        kprintln!(
            "rom_patch: ERROR — {} at {:#010x} is {:#010x}, expected {:#010x}; skipping HVC #{:#x} probe",
            name, pc, prev, expected_orig, imm
        );
        return;
    }
    unsafe { rom_ptr.add(idx).write(new_insn); }
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} probe, HVC #{:#x})",
        pc, prev, new_insn, name, imm
    );
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
    // MOV r7, lr = E1A0_700E ; HVC #imm
    let debugstr_stub: [u32; 2] = [0xE1A0_700E, hvc_insn(DEBUG_STR_HVC_IMM)];
    let debugger_stub: [u32; 2] = [0xE1A0_700E, hvc_insn(DEBUGGER_HVC_IMM)];
    unsafe {
        write_stub_words(rom_ptr, DEBUG_STR_STUB_PC, &debugstr_stub);
        write_stub_words(rom_ptr, DEBUGGER_STUB_PC,  &debugger_stub);

        let word = (0x0038_CE6C / 4) as usize;
        let prev = rom_ptr.add(word).read();
        let insn = arm_b(0x0038_CE6C, DEBUG_STR_STUB_PC);
        rom_ptr.add(word).write(insn);
        kprintln!(
            "rom_patch: 0x0038ce6c: {:#010x} -> {:#010x}  (DebugStr → B {:#x}, HVC #{:#x})",
            prev, insn, DEBUG_STR_STUB_PC, DEBUG_STR_HVC_IMM,
        );
        let word = (0x0038_CE70 / 4) as usize;
        let prev = rom_ptr.add(word).read();
        let insn = arm_b(0x0038_CE70, DEBUGGER_STUB_PC);
        rom_ptr.add(word).write(insn);
        kprintln!(
            "rom_patch: 0x0038ce70: {:#010x} -> {:#010x}  (Debugger → B {:#x}, HVC #{:#x})",
            prev, insn, DEBUGGER_STUB_PC, DEBUGGER_HVC_IMM,
        );
    }
}

unsafe fn write_stub_words(rom_ptr: *mut u32, base: u32, words: &[u32]) {
    unsafe {
        for (i, w) in words.iter().copied().enumerate() {
            let idx = ((base + (i as u32) * 4) / 4) as usize;
            rom_ptr.add(idx).write(w);
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
    let words: [u32; 4] = [0xE59F_0004, 0xE590_0000, 0xE1A0_F00E, 0x0F18_1000];
    unsafe {
        for (i, w) in words.iter().copied().enumerate() {
            let offset = ENTRY + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            let prev = rom_ptr.add(idx).read();
            rom_ptr.add(idx).write(w);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (RealClockSeconds)",
                offset, prev, w,
            );
        }
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
    // Stub body at FTIME_STUB_PC (5 words):
    //   +0x00 LDR r12, [pc, #8]           ; load delta from +0x10
    //   +0x04 SUB r0, r0, r12             ; r0 = r0 - delta
    //   +0x08 MOV r0, r0, LSL #4          ; callback << 2 + original << 2
    //   +0x0C B <RETURN_PC>               ; resume at the epilogue
    //   +0x10 .word safeIntervalDeltaSeconds
    let stub_b = arm_b(FTIME_STUB_PC + 0x0C, RETURN_PC);
    let stub: [u32; 5] = [
        0xE59F_C008,        // LDR r12, [pc, #8]
        0xE040_000C,        // SUB r0, r0, r12
        0xE1A0_0200,        // MOV r0, r0, LSL #4
        stub_b,             // B RETURN_PC
        SAFE_INTERVAL_DELTA_SECONDS,
    ];
    let patch_insn = arm_b(PATCH_PC, FTIME_STUB_PC);
    unsafe {
        write_stub_and_patch(rom_ptr, FTIME_STUB_PC, &stub, PATCH_PC, patch_insn, "FTimeInSeconds");
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
    let stub_b = arm_b(FDATE_STUB_PC + 0x0C, RETURN_PC);
    let stub: [u32; 5] = [
        0xE59F_C008,        // LDR r12, [pc, #8]
        0xE081_100C,        // ADD r1, r1, r12
        0xE1A0_000D,        // MOV r0, sp (= MOV r0, r13) — original instruction
        stub_b,             // B RETURN_PC
        SAFE_INTERVAL_DELTA_SECONDS,
    ];
    let patch_insn = arm_b(PATCH_PC, FDATE_STUB_PC);
    unsafe {
        write_stub_and_patch(rom_ptr, FDATE_STUB_PC, &stub, PATCH_PC, patch_insn, "FDateFromSeconds");
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
        rom_ptr.add(idx).write(insn);
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
        rom_ptr.add(idx).write(insn);
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
        rom_ptr.add(idx).write(insn);
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
    let bl_pc = RESOLVE_FAULT_WRAPPER_PC + 0x3C;
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
            let offset = RESOLVE_FAULT_WRAPPER_PC + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            rom_ptr.add(idx).write(w);
        }

        // Patch the `bl ResolveFault` site inside `Fault` (0x001f84e0).
        let idx = (FAULT_BL_RESOLVE_PC / 4) as usize;
        let prev = rom_ptr.add(idx).read();
        let insn = arm_bl(FAULT_BL_RESOLVE_PC, RESOLVE_FAULT_WRAPPER_PC);
        rom_ptr.add(idx).write(insn);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (Fault → ResolveFaultWrapper @{:#x})",
            FAULT_BL_RESOLVE_PC, prev, insn, RESOLVE_FAULT_WRAPPER_PC,
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
    // ARM `add r1, r1, #4096` (= 0xE2811A01) — imm12 with rot=0xA,
    // imm8=0x01 → ROR(0x01, 20) = 0x1000.
    let add_r1_4k = 0xE281_1A01u32;
    let b_target  = arm_b(NEW_STACK_PAD_WRAPPER_PC + 4, NEW_STACK_THUNK_PC);
    let stub: [u32; 2] = [add_r1_4k, b_target];
    unsafe {
        for (i, w) in stub.iter().copied().enumerate() {
            let offset = NEW_STACK_PAD_WRAPPER_PC + (i as u32) * 4;
            let idx = (offset / 4) as usize;
            rom_ptr.add(idx).write(w);
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
        let new_bl = arm_bl(NEW_STACK_PAD_BL_PC, NEW_STACK_PAD_WRAPPER_PC);
        rom_ptr.add(idx).write(new_bl);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (TTask::Init bl NewStack → +4 KiB pad wrapper @{:#x})",
            NEW_STACK_PAD_BL_PC, prev, new_bl, NEW_STACK_PAD_WRAPPER_PC,
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
        (LOCK_HEAP_RANGE_WRAPPER_PC,   LOCK_HEAP_RANGE_PC,   "LockHeapRange"),
        (UNLOCK_HEAP_RANGE_WRAPPER_PC, UNLOCK_HEAP_RANGE_PC, "UnlockHeapRange"),
    ] {
        let mut stub = stub_template;
        stub[8] = arm_b(wrapper_pc + 0x20, fn_pc + 4);

        unsafe {
            for (i, w) in stub.iter().copied().enumerate() {
                let idx = ((wrapper_pc + (i as u32) * 4) / 4) as usize;
                rom_ptr.add(idx).write(w);
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
            rom_ptr.add(fn_idx).write(branch);
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} → 4-KiB wrapper @{:#x})",
                fn_pc, prev, branch, name, wrapper_pc,
            );
        }
    }
}

/// Shared helper for the two injection patches: write a 5-word stub at
/// `stub_pc` and a 1-word branch at `patch_pc`.
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
            rom_ptr.add(idx).write(w);
        }
        let idx = (patch_pc / 4) as usize;
        let prev = rom_ptr.add(idx).read();
        rom_ptr.add(idx).write(patch_insn);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({}: B {:#x}, 5-word stub)",
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
