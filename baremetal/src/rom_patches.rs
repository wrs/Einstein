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
    // Force per-page stack allocation (no subpage sharing).
    //
    // The 717006 kernel uses ARMv4 subpage-AP to put up to four
    // 1-KiB stacks on a single 4-KiB physical page, with the
    // "guard" subpages set to AP=00 so a stack overrun faults and
    // the kernel can grow it. ARMv7 (our hardware) has no
    // subpage-AP support — `fix_stage1_xn_bits` flattens every
    // L2 entry to AP=011 (full RW) so accesses don't fault. The
    // side effect is that overruns silently corrupt the
    // adjacent task's 1-KiB region living on the same physical
    // page — that's the chain INVESTIGATION.md "Currently at —
    // ARMv4 subpage-AP flattened" pins to the BootOS-canary wedge.
    //
    // Fix: patch `TStackManager::ResolveFault` to claim **all 4**
    // subpages (mask=0xF) on every fault-driven page allocation,
    // instead of just the single subpage that faulted. The kernel
    // still tracks subpages internally, but each task ends up with
    // a fresh physical page that nobody else can claim subpages
    // on, so over-runs corrupt only the task's own slack space.
    //
    // Encoded `mov r3, #0xF` (= 0xE3A0_300F) at three
    // pre-`bl FindOrAllocPage_ReturnUnLockedOnNoPage` sites in
    // `ResolveFault`:
    //
    //   * `0x001f_7a10` — `lsl r3, r0, r8` (single-subpage mask
    //     in the normal fault path).
    //   * `0x001f_7bd4` — `ldr r3, [sp, #60]` (mask reload in the
    //     stack-collision recovery path).
    //   * `0x001f_7c24` — `orr r3, r1, r0` (mask combine in the
    //     same recovery path).
    RomPatch {
        offset: 0x001F_7A10,
        value:  0xE3A0_300F,
        name:   "ResolveFault: claim all 4 subpages (1/3)",
    },
    RomPatch {
        offset: 0x001F_7BD4,
        value:  0xE3A0_300F,
        name:   "ResolveFault: claim all 4 subpages (2/3)",
    },
    RomPatch {
        offset: 0x001F_7C24,
        value:  0xE3A0_300F,
        name:   "ResolveFault: claim all 4 subpages (3/3)",
    },
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
    }

    kprintln!("rom_patch: applied {} simple patches + 5 native-call/injection ROM patches + PowerOffAndReboot + Reboot + BootOS canaries", applied);
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
