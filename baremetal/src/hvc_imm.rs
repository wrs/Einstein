//! Centralised registry of every HVC immediate the hypervisor uses.
//!
//! `#[repr(u32)]` enum: variants in two contiguous blocks.
//!
//! 1. **Guest-test ABI** (`0x10..` — `GuestTestPrintByte..Debugger`):
//!    test binaries issue these as `hvc #imm` literals via the
//!    `HVC_*` macros in `guest-tests/common/test_runtime.S`. The
//!    block is anchored at 0x10 so the auto-incrementing rest can't
//!    eat into it. Tests must keep using the macros — touching this
//!    block requires updating both the enum and the .S header in
//!    lockstep.
//!
//! 2. **Hypervisor-internal** (no anchor): everything else,
//!    auto-incremented. Adding a variant just appends; the compiler
//!    catches anything that would collide with a guest-test anchor
//!    or a previously-anchored value. The whole point of putting
//!    these in one enum is so the iter-109 collision (DAH_FME_RET
//!    silently sharing 0x50 with TRACE_TAG) becomes a build error.
//!
//! Both ends of an HVC live in the same build (the patcher writes
//! `HVC #imm` into ROM; the dispatcher matches `imm` after the trap),
//! so the absolute discriminant doesn't need to be stable across
//! builds. Reordering or removing variants does invalidate any saved
//! snapshot — same as any ROM-patch change.

// Trace / ns_trace variants are unused unless their cfg-feature is on,
// but we keep them in the enum unconditionally so the dispatch site
// can match without `#[cfg]` boilerplate. Suppress the dead-code lint.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HvcImm {
    // ---- Guest-test ABI block (anchored at 0x10) ------------------
    //
    // Test binaries issue these as `hvc #imm` literals via the
    // `HVC_*` macros in `guest-tests/common/test_runtime.S`. Keep
    // these two lists in lockstep — the discriminant numbers below
    // ARE the test ABI.
    /// Guest-test: print one ASCII byte from r0. (`HVC_PRINT_BYTE`)
    GuestTestPrintByte = 0x10,
    /// Guest-test: print r0 as hex. (`HVC_PRINT_HEX`)
    GuestTestPrintHex,
    /// Guest-test: test passed (r0 = code), halt. (`HVC_PASS`)
    GuestTestPass,
    /// Guest-test: test failed (r0 = code), halt. (`HVC_FAIL`)
    GuestTestFail,
    /// Guest-test: mark/breadcrumb (r0 = optional). (`HVC_MARK`)
    GuestMark,
    /// Guest-test: raise `vic::INT_GPIO` for IRQ-delivery test.
    /// (`HVC_GPIO_TRIGGER`)
    GpioTrigger,
    /// `handle_und` entry tag. The UND-trampoline issues this to
    /// hand control to EL2 after saving source state; some tests
    /// also fire it directly via `HVC_UND` to verify the path.
    Und,
    /// Alignment-fault path: in-ROM stub redirects mis-aligned
    /// LDR/STR through this so EL2 emulates the rotate.
    /// (`HVC_ALIGN`)
    Align,
    /// Save the four-slot rolling guest-state snapshot.
    /// (`HVC_SNAPSHOT`)
    Snapshot,
    /// Shadow-stub bulk-patch request: scan + patch a guest IPA
    /// range for byte/halfword accesses. (`HVC_SHADOW_PATCH_RANGE`)
    ShadowPatchRange,
    /// `DebugStr` ROM-patch trap (logs guest string in r0).
    /// (`HVC_DEBUG_STR`)
    DebugStr,
    /// `Debugger` ROM-patch trap (logs site, no host debugger).
    /// (`HVC_DEBUGGER`)
    Debugger,

    // ---- Hypervisor-internal HVCs (auto-incremented) --------------
    //
    // Not in the test ABI; auto-incremented from after the block
    // above. Adding a variant just appends. Tests don't issue these
    // directly, so reordering / shifting their values is safe.
    /// Phase-B diagnostic: dump banked regs + stage-1 walk + halt.
    Diag,
    /// Shadow-stub byte/halfword-access return path: signals that
    /// the patched access has completed and we can resume the
    /// original (post-patched) PC.
    SbaRetry,
    /// `PowerOffAndReboot` canary — kernel rebooting.
    PowerOffReboot,
    /// `Reboot` canary — kernel rebooting.
    Reboot,
    /// `BootOS` canary — first/2nd-entry detection.
    BootOs,
    /// `Remember` post-SWI fixup — re-establish r8 sentinel.
    RememberSwiret,
    /// QEMU raspi3b `mrs r1, SPSR_abt` workaround at DAH entry.
    DahMrsSpsr,
    /// Tracer trampoline slot[0] entry (one HVC per traced function;
    /// the trampoline contains the rest of the prologue + branch-back).
    Trace,
    /// Static `FaultMonitorEntry` entry probe (input fault mask).
    DahFmeEntry,
    /// DAH OR-chain entry probe (curr_task + monitor list capture).
    DahOrChain,
    /// `cmp r0, #0` after `bl FaultMonitorEntry` in DAH —
    /// captures FME's return value, emulates the cmp, returns.
    DahFmeRet,
    /// `UnhandledException` halt-on-entry tripwire.
    UnhandledException,
    /// `UnhandledNonUserModeException` halt-on-entry tripwire.
    UnhandledNumException,
    /// Full kernel-state dump on demand (scheduler, ports, monitors).
    TaskDump,
    /// Dump one kernel object by its 32-bit ID (passed in r0).
    DumpObjectById,
    /// `Print__14POutTranslatorFPCce` thunk (capture kernel
    /// REP printf output). `ns_trace` feature.
    PrintProbe,
    /// `Putc` thunk. `ns_trace` feature.
    PutcProbe,
    /// `Flush` thunk. `ns_trace` feature.
    FlushProbe,
    /// `StackTrace` thunk. `ns_trace` feature.
    StackTraceProbe,
    /// `ExceptionNotify` thunk. `ns_trace` feature.
    ExNotifyProbe,
    /// `FP_UndefHandlers_Start + 0x3C` — FPE-entry counter +
    /// `mov ip, sp` emulation.
    FpeEntryProbe,
    /// Iter-108 splash-chain diagnostic probes (TNotebook
    /// InitToolbox / DrawSplashScreen / InitScriptGlobals
    /// inflection points). All sites share one immediate; the
    /// handler distinguishes by ELR.
    SplashProbe,
}

impl HvcImm {
    /// AArch32 `HVC #imm` instruction encoding for this variant.
    /// Layout: `1110 0001 0100 imm12_hi 0111 imm4_lo`, where the
    /// 16-bit immediate splits across bits 19:8 (high 12) and
    /// bits 3:0 (low 4).
    #[inline]
    pub const fn insn(self) -> u32 {
        let imm = self as u32;
        0xE140_0070 | ((imm & 0xFFF0) << 4) | (imm & 0xF)
    }
}
