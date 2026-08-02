//! Centralised registry of every HVC immediate the hypervisor uses.
//!
//! `#[repr(u32)]` enum: variants in two contiguous blocks.
//!
//! 1. **Guest-test ABI** (`0x10..` — `GuestTestPrintByte..GuestTestRepRender`):
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

// `Trace` is unused unless its cfg-feature is on, but we keep it in the
// enum unconditionally so the dispatch site can match without `#[cfg]`
// boilerplate. Suppress the dead-code lint.
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
    /// `DebugStr` ROM-patch trap (logs guest string in r0).
    /// (`HVC_DEBUG_STR`)
    DebugStr,
    /// `Debugger` ROM-patch trap (logs site, no host debugger).
    /// (`HVC_DEBUGGER`)
    Debugger,
    /// Inject a host-side pen sample directly into `host_io::queue`,
    /// bypassing the backend's input transport. r0 = packed sample
    /// word (Einstein format), r1 = sample time in Newton ticks.
    /// Used by `test_tablet.S` to verify the queue + IRQ-raise +
    /// `NativeGetSample` drain path without needing a paired viewer.
    /// (`HVC_INJECT_PEN`)
    GuestInjectPen,
    /// Guest-test: render a REP/Hammer format string via the
    /// production `rep_print` interpreter into a guest-supplied
    /// buffer (rather than the UART line buffer), so a test can
    /// byte-assert the VaArgs/specifier ABI. r0 = format string
    /// pointer, r1 = out buffer pointer, r2 = first vararg,
    /// r3 = second vararg, [sp+0..] = third+ varargs; on return
    /// r0 = number of bytes rendered. Only meaningful in
    /// `nh_guest_test` builds — the dispatcher arm is cfg-gated, so
    /// a production guest issuing it hits the unknown-HVC halt.
    /// (`HVC_REP_RENDER`)
    GuestTestRepRender,

    // ---- Hypervisor-internal HVCs (auto-incremented) --------------
    //
    // Not in the test ABI; auto-incremented from after the block
    // above. Adding a variant just appends. Tests don't issue these
    // directly, so reordering / shifting their values is safe.
    /// Diagnostic halt: dump banked regs + stage-1 walk + halt the
    /// host. For ad-hoc hand-patches: write this HVC into any guest
    /// code site to get a halt-with-full-register-dump there. Also
    /// reached from the DABT dispatch path for shapes it can't
    /// forward (`trap::dabt::handle_dabt_dispatch`).
    Diag,
    /// DABT fast-trampoline fall-through. The trampoline at
    /// `DABT_TRAMP_OFFSET` checks DFSR.status == 0x01 (alignment)
    /// via BEQ → `HVC #Align`; on non-alignment it falls through to
    /// this HVC. Handler dispatches forwardable DFSCs to the
    /// kernel's `DataAbortHandler` (and patches DFSR.Domain from the
    /// L1 entry for translation/permission/access-flag faults
    /// where ARMv7 leaves the field UNK). Non-forwardable DFSCs
    /// fall through to `handle_diag` for the diagnostic halt.
    DabtDispatch,
    /// Loud-halt tripwire. Patched at the first instruction of
    /// `Reboot`, `PowerOffAndReboot`, and `StopImage` — three sites
    /// the kernel reaches when it's giving up or going idle. The
    /// handler prints PC + r0..r3 + caller LR and halts the host;
    /// the run terminates immediately instead of spinning.
    LoudHalt,
    /// `BootOS` canary — first/2nd-entry detection.
    BootOs,
    /// `Remember` post-SWI fixup — re-establish r8 sentinel.
    RememberSwiret,
    /// QEMU raspi3b `mrs r1, SPSR_abt` workaround at DAH entry.
    DahMrsSpsr,
    /// Tracer trampoline slot[0] entry (one HVC per traced function;
    /// the trampoline contains the rest of the prologue + branch-back).
    Trace,
    /// `UnhandledException` halt-on-entry tripwire.
    UnhandledException,
    /// `UnhandledNonUserModeException` halt-on-entry tripwire.
    UnhandledNumException,
    /// Full kernel-state dump on demand (scheduler, ports, monitors).
    TaskDump,
    /// Dump one kernel object by its 32-bit ID (passed in r0).
    DumpObjectById,
    /// `PHammerOutTranslator::Print` body — kernel REP printf
    /// output. The body's prologue is replaced with `HVC`, so the
    /// EL2 handler renders fmt+args via `rep_print` and returns 0.
    /// Concrete-subclass patch (not an abstract-base thunk hook):
    /// `gREPout` already points at PHammerOutTranslator on every
    /// boot (gNewtConfig=0x8202 sets kEnableListener), so natural
    /// vtable dispatch reaches the patched body.
    HammerPrint,
    /// `PHammerOutTranslator::Putc` body — single-char REP output.
    HammerPutc,
    /// `PHammerOutTranslator::Flush` body — explicit flush.
    HammerFlush,
    /// `PHammerOutTranslator::StackTrace` first insn (replaces
    /// the original `mov r0, r1`); the next word is the original
    /// `b REPStackTrace` and runs natively after HVC.
    HammerStackTrace,
    /// `PHammerOutTranslator::ExceptionNotify` first insn (replaces
    /// the original `mov r0, r1`); the next word is the original
    /// `b REPExceptionNotify` and runs natively after HVC.
    HammerExceptionNotify,
    /// Entry probe at `StorePermObject` (ROM 0x002D_F998).
    /// Replaces the function's first instruction (`mov ip, sp`).
    /// Handler dereferences R0 (a `RefVar const&`) to recover the
    /// stored Ref, pretty-prints it via `newton-objects`,
    /// emulates `mov ip, sp`, and advances ELR.
    StorePermObjEntry,
    /// Return probe at `LoadPermObject` (ROM 0x002D_F4C0).
    /// Replaces the `mov r0, r4` immediately before the function's
    /// `ldmdb` epilogue. R4 holds the Ref returned by the inner
    /// `Read__18TStoreObjectReaderFv`. Handler pretty-prints R4
    /// via `newton-objects`, emulates `r0 = r4`, and advances ELR
    /// so the epilogue's `ldmdb` returns the same Ref to the caller.
    LoadPermObjRet,
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
