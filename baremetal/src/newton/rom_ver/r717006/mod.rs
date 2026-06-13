//! ROM-version constants for the 717006 ROM (MP2100 US) — the fully
//! bring-up'd version. Every address here is verified against
//! `scripts/disasm-out/rom.dis`; the `orig_insn` fields double as the
//! install-time safety net (the patch installer halts on mismatch).

mod patches;

use super::common_2x;
use super::types::*;
use crate::hv::hooks::UndDiagHints;

pub const NAME: &str = "717006 (MP2100 US)";

/// Byte size of the main ROM image file (`roms/newton.rom`). Every
/// `RomPatch.offset` and probe PC in this module is below it.
pub const ROM_IMAGE_SIZE: usize = 0x0080_0000;

/// End of this version's own code addresses in the ROM aperture: the
/// main ROM plus the REx window, i.e. the whole 16 MiB aperture below
/// the jump-table alias at VA 0x01A0_0000. Distinct from
/// `hv::layout::high_rom_end()`, which is the MP2x00 *hardware* bus
/// aperture (the write-absorb window) and identical for every ROM.
pub const ROM_CODE_END: u32 = 0x0100_0000;

/// External-REx placement: second 8 MiB of the aperture; the 717006
/// ROM embeds exactly one REx (id 0, at base_size 0x71FC4C), so the
/// external Einstein.rex claims id 1.
pub const REX: RexInfo = RexInfo {
    pa_offset: 0x0080_0000,
    num_embedded_rexes: 1,
};

/// Hypervisor-structure placement in the ROM-aperture tail. The
/// Einstein.rex tail ends ~0x0084_7000; everything below sits in the
/// guaranteed-free space above it.
pub const ROM_TAIL: RomTailLayout = RomTailLayout {
    dabt_fast_tramp:       0x008F_FF00,
    tracer_pool_base:      0x0090_0000,
    tracer_pool_end:       0x00E0_0000,
    stub_pool_base:        0x00E0_0000,
    stub_pool_end:         0x00FF_FF00,
    patch_stub_arena_base: 0x00FF_FD80,
    patch_stub_arena_end:  0x00FF_FEC0,
    fpa_bypass_stub:       0x00FF_FEC0,
    und_tramp:             0x00FF_FF00,
    dabt_tramp:            0x00FF_FFA8,
    und_return_stub:       0x00FF_FFE4,
    stubs_end:             0x0100_0000,
};

pub const PATCHES: &[RomPatch] = patches::PATCHES_717006;
pub const NS_TRACE_PATCH: Option<RomPatch> = Some(patches::NS_TRACE_PATCH_717006);

/// `BootOS` / `ROMBoot` at 0x0001_8688. The AArch32 reset vector at
/// VA 0 is `B 0x18688`, so the first execution after the hypervisor's
/// ERET-to-guest lands here; any subsequent entry is a software reset.
/// Original first insn: `mov r0, #0xb0`.
pub const BOOT: Option<BootSites> = Some(BootSites {
    bootos: ProbeSite { pc: 0x0001_8688, orig_insn: 0xE3A0_00B0 },
});

/// Loud-halt canary sites (see `rom_patches::apply_loud_halt_traps`):
///  - `PowerOffAndReboot` — fatal init-time checks route here.
///  - `Reboot(long, ULong, UChar)` — the exception unwinder's
///    soft-reboot path.
///  - `StopImage` — idle/sleep wait-for-interrupt entry.
///  - the `bl Throw` inside `TStackManager::Fault` (busError throw).
/// Originals: `mov ip, sp` prologues ×2, the StopImage `mrc` CPU-ID
/// read, and the `bl Throw` encoding.
pub const LOUD_HALT: Option<LoudHaltSites> = Some(LoudHaltSites {
    poweroff_reboot: ProbeSite { pc: 0x000E_6BBC, orig_insn: 0xE1A0_C00D },
    reboot:          ProbeSite { pc: 0x000D_9884, orig_insn: 0xE1A0_C00D },
    stop_image:      ProbeSite { pc: 0x0038_D174, orig_insn: 0xEE10_0F10 },
    bus_error_throw: ProbeSite { pc: 0x001F_8534, orig_insn: 0xEB67_AB18 },
});

/// `PHammerOutTranslator` concrete-method bodies. Print/Putc/Flush get
/// 3-word body replacements; StackTrace/ExceptionNotify get word-0-only
/// patches (original word: `mov r0, r1`, re-emulated by the handler).
pub const HAMMER: Option<HammerSites> = Some(HammerSites {
    print:            ProbeSite { pc: 0x000E_6A90, orig_insn: 0xE1A0_C00D },
    putc:             ProbeSite { pc: 0x000E_6AD0, orig_insn: 0xE1A0_C00D },
    flush:            ProbeSite { pc: 0x000E_6A50, orig_insn: 0xE1A0_C00D },
    stack_trace:      ProbeSite { pc: 0x000E_6954, orig_insn: 0xE1A0_0001 },
    exception_notify: ProbeSite { pc: 0x000E_695C, orig_insn: 0xE1A0_0001 },
});

/// `UnhandledException` / `UnhandledNonUserModeException` entries —
/// halt-on-entry tripwires with the kernel-supplied exception-name
/// string in R0.
pub const UNHANDLED: Option<UnhandledExceptionSites> = Some(UnhandledExceptionSites {
    user:     ProbeSite { pc: 0x000B_0220, orig_insn: 0xE1A0_C00D },
    non_user: ProbeSite { pc: 0x000B_031C, orig_insn: 0xE1A0_C00D },
});

/// `Remember` post-SWI fixup site (after the first `bl GenericSWI`).
/// The handler logs the SWI return and re-emulates `mov r8, #237` so
/// the kernel's `r8 = -10003` sentinel is restored before the
/// following `teq` at 0x00258E58.
pub const REMEMBER_SWIRET: Option<ProbeSite> =
    Some(ProbeSite { pc: 0x0025_8E50, orig_insn: 0xE3A0_80ED });

/// `mrs r1, SPSR` at DataAbortHandler entry (4th insn past the label).
/// Replaced with an HVC so EL2 can substitute the trampoline-saved
/// SPSR_abt (QEMU raspi3b `mrs spsr_abt` staleness — QEMU_BUGS Bug #1).
pub const DAH_MRS_SPSR: Option<ProbeSite> =
    Some(ProbeSite { pc: 0x0039_3144, orig_insn: 0xE14F_1000 });

/// Newton UND-dispatch-table slots for DebugStr / Debugger. The
/// originals are the kernel's debugger UND-marker words.
pub const DEBUG_UND_SLOTS: Option<DebugUndSlots> = Some(DebugUndSlots {
    debug_str: ProbeSite { pc: 0x0038_CE6C, orig_insn: 0xE600_0310 },
    debugger:  ProbeSite { pc: 0x0038_CE70, orig_insn: 0xE600_0210 },
});

/// `RealClockSeconds` at 0x0025_5578 — body replaced with a 4-word
/// MMIO-calendar read. `prologue_origs` are the four displaced words
/// (mov ip,sp / push / sub fp / sub sp), verified at install.
pub const REAL_CLOCK_SECONDS: Option<RealClockSite> = Some(RealClockSite {
    entry: 0x0025_5578,
    prologue_origs: [0xE1A0_C00D, 0xE92D_D810, 0xE24C_B004, 0xE24D_D008],
});

/// FTimeInSeconds injection: the last shift before the epilogue
/// (`MOV r0, r0, LSL #2` at 0x0008_9B80) is replaced with a branch to
/// an arena stub computing `r0 = (r0 - delta) << 2`, resuming at the
/// LDMDB epilogue. Einstein: TJITGenericROMPatch.cpp:150.
pub const FTIME_IN_SECONDS: Option<InjectionSite> = Some(InjectionSite {
    patch: ProbeSite { pc: 0x0008_9B80, orig_insn: 0xE1A0_0100 },
    resume_pc: 0x0008_9B84,
});

/// FDateFromSeconds injection: the `MOV r0, sp` at 0x0008_A8A8 is
/// replaced with a branch to an arena stub that adds the delta to r1,
/// re-does the MOV, and resumes. Einstein: TJITGenericROMPatch.cpp:160.
pub const FDATE_FROM_SECONDS: Option<InjectionSite> = Some(InjectionSite {
    patch: ProbeSite { pc: 0x0008_A8A8, orig_insn: 0xE1A0_000D },
    resume_pc: 0x0008_A8AC,
});

/// The kernel's four `LDR` sites that read a (byteswapped-at-load)
/// instruction word as data — each redirected to a 3-word
/// LDR + REV + branch-back stub. See `types::InsnAsDataLdr`.
pub const INSN_AS_DATA_LDRS: &[InsnAsDataLdr] = &[
    // DataAbortHandler `ldr r0, [lr]` — reads the faulting word so the
    // kernel can decode the abort.
    InsnAsDataLdr {
        site: ProbeSite { pc: 0x0039_31E4, orig_insn: 0xE59E_0000 },
        name: "DAH ldr r0,[lr]",
    },
    // UndefinedInstruction `ldr r1, [lr, #-4]` — reads the faulting
    // word for the UDF-marker compare.
    InsnAsDataLdr {
        site: ProbeSite { pc: 0x0038_CE9C, orig_insn: 0xE51E_1004 },
        name: "UND ldr r1,[lr,-4]",
    },
    // SWIBoot `ldr r0, [lr, #-4]` — reads the SWI insn to extract the
    // immediate. Without the fix every SWI dispatches to the wrong
    // handler and boot wedges in the MonitorDispatchSWI loop.
    InsnAsDataLdr {
        site: ProbeSite { pc: 0x003A_D69C, orig_insn: 0xE51E_0004 },
        name: "SWIBoot ldr r0,[lr,-4]",
    },
    // SWIBoot dispatch `ldr r1, [r1, #-4]` (r1 = lr from the preceding
    // `mov r1, lr`) — re-reads the SWI word for the dispatch-table
    // index; needed because conditional SVCs clobber r0 via
    // `mrs r0, SPSR` before the downstream `bic`/`cmp`.
    InsnAsDataLdr {
        site: ProbeSite { pc: 0x003A_D738, orig_insn: 0xE511_1004 },
        name: "SWIBoot dispatch ldr r1,[r1,-4]",
    },
];

/// FPE prelude's conditional instruction-as-data LDR pair at
/// FP_UndefHandlers_Start: `ldrteq fp, [r9], #0` (USR-source) and
/// `ldrne fp, [r9]` (non-USR). Both route to one shared stub doing
/// `ldr fp, [r9]; rev fp, fp; b resume`. The stub's plain LDR uses
/// kernel permissions instead of the original LDRT — Newton ROM code
/// is always kernel-readable, so this is equivalent in practice.
pub const FPE_LDRS: Option<FpeLdrSites> = Some(FpeLdrSites {
    eq_site: ProbeSite { pc: 0x0038_D930, orig_insn: 0x04B9_B000 },
    ne_site: ProbeSite { pc: 0x0038_D934, orig_insn: 0x1599_B000 },
    stub_ldr: 0xE599_B000, // ldr fp, [r9]
    resume_pc: 0x0038_D938,
});

/// `StorePermObject` entry (`mov ip, sp` prologue) + the `mov r0, r4`
/// before `LoadPermObject`'s ldmdb epilogue (`log_store` probes).
pub const STORE_PROBES: Option<StoreProbeSites> = Some(StoreProbeSites {
    store_perm_entry: ProbeSite { pc: 0x002D_F998, orig_insn: 0xE1A0_C00D },
    load_perm_ret:    ProbeSite { pc: 0x002D_F4C0, orig_insn: 0xE1A0_0004 },
});

/// Notification entry probes (see `rom_patches::apply_notify_probes`).
/// Originals: `mov r2, r0` (Notify's first insn) and `mov ip, sp`
/// prologues ×2 — verified against rom.dis.
pub const NOTIFY_PROBES: Option<NotifySites> = Some(NotifySites {
    notify:              ProbeSite { pc: 0x0014_6584, orig_insn: 0xE1A0_2000 },
    error_notify:        ProbeSite { pc: 0x0014_65A4, orig_insn: 0xE1A0_C00D },
    action_error_notify: ProbeSite { pc: 0x0014_6648, orig_insn: 0xE1A0_C00D },
});

/// The kernel's `DataAbortHandler` entry VA — the original target of
/// the ROM's VA 0x10 branch before our DABT trampoline insertion.
/// Routine faults (translation / permission / access-flag) are
/// forwarded here so the kernel handles on-demand paging itself.
pub const DATA_ABORT_HANDLER_VA: Option<u32> = Some(0x0039_3114);

/// `FP_UndefHandlers_Start_JT` — the ROM jump-table slot that thunks
/// into the kernel FPE. FPA-class UNDs are routed here (both by the
/// in-ROM bypass stub and by EL2 on a bypass miss). Routing through
/// the JT slot (not the handler body at 0x38d8dc) preserves the
/// post-ship-patch indirection.
pub const FPE_JT_VA: Option<u32> = Some(0x0038_D874);

/// Kernel globals consumed by the diagnostics layer. See
/// `types::KernelGlobals` for per-field meaning; sources are noted in
/// `docs/STRUCTURES.md` and the module docs of `diag::{task_dump,
/// heap_check, trap_diag}`.
pub const KERNEL_GLOBALS: Option<KernelGlobals> = Some(KernelGlobals {
    scheduler_ptr:          0x0C10_0FD0,
    current_task:           0x0C10_1000,
    want_schedule:          0x0C10_0FD4,
    hold_schedule:          0x0C10_0FD8,
    current_globals:        0x0C10_105C,
    object_table:           0x0C10_FC34,
    object_table_a_ptr:     0x0C10_1164,
    object_table_b_ptr:     0x0C10_0FC8,
    object_heap_ptr:        0x0C10_5548,
    interpreter_ptr:        0x0C10_5458,
    stack_mgr_heap_literal: 0x0C10_4C08,
});

/// UND-history caller-LR heuristics for the SWP wedge signature
/// (`TULockingSemaphore::Swap` reached from Acquire / Release) — see
/// `hv::trap::und::record_und_history`. Offsets follow the compiled
/// prologues: Acquire pushes 10 words (caller LR at SP+32; the outer
/// Grabber-constructor caller at SP+92), Release pushes 5 (SP+12).
pub const WEDGE_DIAG: Option<UndDiagHints> = Some(UndDiagHints {
    swap_helper_pc:        0x003A_E204,
    acquire_ret_lr:        0x0025_A2C8,
    acquire_caller_sp_off: 32,
    acquire_outer_sp_off:  92,
    release_ret_lr:        0x0025_A338,
    release_caller_sp_off: 12,
});

/// `SearchFreeList` body word the guest-bp machinery re-arms on every
/// benign walk — its per-install log line is suppressed to keep the
/// console usable. See `diag::guest_bp`.
pub const BP_REARM_QUIET_IPA: Option<u32> = Some(0x0031_3308);

// NewtonOS-2.x-shared defaults.
pub use common_2x::{KERNEL_TTBR0_BASE, SAFE_INTERVAL_DELTA_SECONDS};
