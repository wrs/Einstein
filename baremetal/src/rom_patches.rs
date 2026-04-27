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
        0xE350_0004,                            // +0x40 cmp r0, #4
        0x0A00_0003,                            // +0x44 beq done (skip 3 insns to +0x58)
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
        // multi-iter would over-claim and break boot.)
        let _ = FMLOCK_BL_RESOLVE_PC;

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
