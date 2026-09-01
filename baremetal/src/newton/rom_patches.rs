//! Load-time ROM patch *mechanism*: the unified verify-and-write
//! installer, the patch-stub arena allocator, the original-word side
//! table for inline_patch's liveness analyser, and the per-group
//! `apply_*` installers.
//!
//! Every patched *address* (and the original word each site must
//! hold) is a ROM-version fact and lives in `super::rom_ver`; each
//! `apply_*` here starts with `let Some(site) = rom_ver::… else
//! { return; }` so a version module that doesn't know a site simply
//! skips the patch. The verified-original halt in `install_patch`
//! stays the safety net against a site table that drifts from the
//! actual ROM bytes.
//!
//! Background: the 717006 ROM needs a handful of patches to behave
//! sensibly under any emulator / hypervisor. Einstein ships these in
//! `Emulator/JIT/Generic/TJITGenericROMPatch.cpp` and applies them
//! during `TROMImage::CreateImage`. We translate both the word-write
//! patches (`rom_ver::PATCHES`) AND the JIT-specific native-call /
//! injection patches (`DebugStr`, `Debugger`, `RealClockSeconds`,
//! `FTimeInSeconds`, `FDateFromSeconds`). Einstein's JIT catches its
//! custom SWI opcodes; we don't have a JIT, so we rewrite each target
//! function with equivalent inline ARM code that achieves the same
//! net effect.
//!
//! The virtualized-call patches (`__rt_sdiv`, `__rt_udiv`, `symcmp`)
//! are a performance optimization — Einstein injects host code for
//! these so it doesn't have to JIT them — but on our A53 they run
//! natively just fine. Not implemented because omitting them doesn't
//! change correctness.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::hv::hvc_imm::HvcImm;
use crate::kprintln;

use super::rom_ver;

// ============================================================================
// Patch-stub arena
// ============================================================================
//
// Each kernel-side native-primitive patch (DebugStr, Debugger,
// FTimeInSeconds, FDateFromSeconds, ResolveFault wrapper, …) needs a
// few words of guest-visible ROM space to hold its replacement-stub
// body, and the kernel-patched BL/B site needs to know that stub's PC
// to encode the redirect. Picking those PCs by hand is how stubs
// collide silently: a hand-picked FTIME_STUB_PC that overlaps the
// region `patch_und_vector` writes gets clobbered when the trampoline
// installs second, and the kernel's patched `b` then lands in
// trampoline code mid-instruction-stream.
//
// The arena removes the manual address management entirely. Each
// `apply_*` function calls `alloc_patch_stub(n)` at install time and
// gets back the next free PC; allocations never overlap and arena
// overflow halts loudly. Callers that need to address into their own
// stub pass that PC around as a local instead of consulting a global
// constant.
//
// The arena bounds come from `rom_ver::ROM_TAIL.patch_stub_arena_*` —
// the gap between the unused LOCK_HEAP_RANGE_WRAPPER region and the
// FPA bypass stub that `patch_und_vector` owns. 320 bytes total.
// Currently-installed patches need 152 B; the LOCK/UNLOCK/NEW_STACK_PAD
// wrappers (NOT installed) would add another 80 B, comfortably within
// the budget.
static PATCH_STUB_ARENA_CURSOR: AtomicU32 = AtomicU32::new(rom_ver::ROM_TAIL.patch_stub_arena_base);

/// Allocate `n_words` (4 bytes each) inside the patch-stub arena and
/// return the start PC. Halts loudly on overflow so any future stub
/// that pushes past the arena end fails at install time rather than
/// silently corrupting an adjacent stub.
fn alloc_patch_stub(n_words: usize, name: &'static str) -> u32 {
    let bytes = (n_words * 4) as u32;
    let pc = PATCH_STUB_ARENA_CURSOR.fetch_add(bytes, Ordering::SeqCst);
    let new_end = pc + bytes;
    if new_end > rom_ver::ROM_TAIL.patch_stub_arena_end {
        kprintln!(
            "*** patch-stub arena overflow: {} wants {}B at {:#x}; \
             arena end is {:#x}",
            name,
            bytes,
            pc,
            rom_ver::ROM_TAIL.patch_stub_arena_end,
        );
        crate::arch::cpu::halt();
    }
    kprintln!(
        "rom_patch: arena alloc {}B for {} -> {:#010x} (cursor now {:#x})",
        bytes,
        name,
        pc,
        new_end,
    );
    pc
}

use crate::arch::aarch32_emit::{b as arm_b, b_cond as arm_b_cond};

// ============================================================================
// Unified patch installer
// ============================================================================
//
// Every code-word overwrite against the ROM backing goes through
// `install_patch`, which applies one policy for every site: verify the
// expected original word and LOUD HALT on mismatch, record the original
// into the inline-patch side table, and write the new word(s) in the
// correct endianness for their kind. The post-load whole-ROM
// `icache_publish_range` sweep in `load_newton_rom` publishes
// every patched byte to the PoU, so the installer itself does no cache
// maintenance — every caller runs strictly before that sweep.
//
// "Expected original" and the replacement `words` are always expressed in
// guest-numerical form — exactly the value `scripts/disasm-out/rom.dis`
// prints in its second column. The kind selects the storage endianness:
// `Code` words are stored native (the CPU fetches LE; a native u32 write
// of the numerical encoding is what it decodes), `Data` words are stored
// byteswapped so a BE-8 guest `LDR` reads back the numerical value.

#[derive(Copy, Clone, PartialEq, Eq)]
enum WordKind {
    /// ARM instruction encoding — stored native-LE, recorded for the
    /// inline-patch liveness analyser.
    Code,
    /// Literal data word — stored byteswapped for a BE-8 guest load.
    Data,
}

/// Read the current ROM word at `idx` in guest-numerical form (the value
/// `rom.dis` prints), accounting for the BE-8 storage convention: code
/// words are stored native, data words are stored byteswapped.
///
/// SAFETY: `rom_ptr` must back the full ROM and `idx * 4 + 4 <= ROM_SIZE`.
unsafe fn read_rom_word_numeric(rom_ptr: *mut u32, idx: usize, kind: WordKind) -> u32 {
    let raw = unsafe { rom_ptr.add(idx).read() };
    match kind {
        WordKind::Code => raw,
        #[cfg(not(nh_guest_test))]
        WordKind::Data => raw.swap_bytes(),
        #[cfg(nh_guest_test)]
        WordKind::Data => raw,
    }
}

/// Install one code-or-data patch against the ROM backing.
///
/// - `pc` is the guest byte address of the first word.
/// - `kind` selects the storage endianness and whether the original is
///   recorded for inline_patch (code words are; data words are not — the
///   liveness analyser only ever decodes instruction words).
/// - `expected_orig` is the guest-numerical word the site must currently
///   hold (read it from `rom.dis`). `None` means "blind write" — used only
///   for fresh patch-stub-arena slots that have no meaningful prior value.
/// - `words` are the guest-numerical replacement word(s), written
///   consecutively from `pc` in `kind`'s endianness.
/// - `optional` downgrades a verify mismatch from a loud halt to a
///   log-and-skip. Reserve it for genuinely optional probes; the default
///   (`false`) halts, because a silent skip of a load-bearing patch
///   guarantees a baffling downstream wedge.
///
/// SAFETY: `rom_ptr` must back the full ROM aperture; `pc` and every
/// word it spans must be word-aligned and within `ROM_SIZE`.
unsafe fn install_patch(
    rom_ptr: *mut u32,
    pc: u32,
    kind: WordKind,
    expected_orig: Option<u32>,
    words: &[u32],
    optional: bool,
    name: &'static str,
) {
    debug_assert!(pc & 3 == 0, "patch pc must be word-aligned");
    let idx = (pc / 4) as usize;
    if let Some(expected) = expected_orig {
        // SAFETY: caller guarantees rom_ptr / pc bounds.
        let prev = unsafe { read_rom_word_numeric(rom_ptr, idx, kind) };
        if prev != expected {
            if optional {
                kprintln!(
                    "rom_patch: optional patch {} at {:#010x}: have {:#010x}, \
                     expected {:#010x}; skipping",
                    name,
                    pc,
                    prev,
                    expected,
                );
                return;
            }
            kprintln!(
                "*** rom_patch: {} at {:#010x} is {:#010x}, expected {:#010x} — \
                 ROM shifted under the patch installer; refusing to continue",
                name,
                pc,
                prev,
                expected,
            );
            crate::arch::cpu::halt();
        }
    }
    // SAFETY: bounds guaranteed by the caller; write each word per kind.
    unsafe {
        for (i, &w) in words.iter().enumerate() {
            let widx = idx + i;
            // Record the original of EVERY code word we overwrite (not
            // just the verified first word) so inline_patch's liveness
            // analyser always sees the pre-patch instruction stream.
            // Blind writes (`expected_orig == None`) target fresh
            // patch-stub-arena slots whose prior bytes are meaningless;
            // nothing to record there.
            if kind == WordKind::Code && expected_orig.is_some() {
                let orig = read_rom_word_numeric(rom_ptr, widx, WordKind::Code);
                record_original(pc + (i as u32) * 4, orig);
            }
            match kind {
                WordKind::Code => crate::hv::guest_mem::write_rom_code_word(rom_ptr, widx, w),
                WordKind::Data => crate::hv::guest_mem::write_rom_data_word(rom_ptr, widx, w),
            }
        }
    }
}

/// Apply the version's word-write patches (`rom_ver::PATCHES`) plus the
/// native-call / injection / probe patch groups to the byteswapped main
/// ROM backing. Caller must own `rom_ptr`; the word-write patches live
/// entirely in the main-ROM image (offsets < `rom_ver::ROM_IMAGE_SIZE`),
/// so overlap with the external REx is not a concern.
///
/// SAFETY: `rom_ptr` must point to at least `ROM_SIZE` bytes of
/// writable backing, and all patch offsets are checked to be in range
/// and word-aligned before the write.
pub unsafe fn apply_rom_patches(rom_ptr: *mut u32) {
    let mut applied = 0usize;
    for p in rom_ver::PATCHES {
        debug_assert!(
            (p.offset as usize) < rom_ver::ROM_IMAGE_SIZE,
            "patch offset must be in main ROM"
        );
        // `install_patch` verifies `p.orig`, halts on mismatch, and
        // records code-word originals for the inline-patch analyser.
        let kind = if crate::hv::guest_mem::rom_word_is_code((p.offset / 4) as usize) {
            WordKind::Code
        } else {
            WordKind::Data
        };
        // SAFETY: bounds-checked against the main-ROM region.
        unsafe {
            install_patch(
                rom_ptr,
                p.offset,
                kind,
                Some(p.orig),
                &[p.value],
                /*optional=*/ false,
                p.name,
            );
        }
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({})",
            p.offset,
            p.orig,
            p.value,
            p.name,
        );
        applied += 1;
    }

    // ns_trace gate patch — see `rom_ver::NS_TRACE_PATCH` for the full
    // rationale (opens the TInterpreter trace gates even when
    // gVars.tracing is NIL).
    #[cfg(feature = "ns_trace")]
    if let Some(p) = rom_ver::NS_TRACE_PATCH {
        // SAFETY: the site is in the main-ROM region, word-aligned,
        // and rom_ptr backs the full ROM. The original `teq` is a
        // code word.
        unsafe {
            install_patch(
                rom_ptr,
                p.offset,
                WordKind::Code,
                Some(p.orig),
                &[p.value],
                /*optional=*/ false,
                p.name,
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
    // SAFETY: rom_ptr has the full ROM.
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
        // Sub-page ownership is fixed per-allocator (the ResolveFault
        // whole-page bitmap + ZapHeap entries in `rom_ver::PATCHES`),
        // not by wrapping NewStack/LockHeapRange. A +4 KiB NewStack-size
        // pad overruns the kernel's stack-pool slot stride (→
        // ResolveFault loop), and a 4-KiB-rounding LockHeapRange wrapper
        // pins subpages owned by other stack_infos; both sit in the
        // wrong layer.
        apply_l1_cd_probes(rom_ptr);
        apply_fault_handler_ldr_byteswap_patches(rom_ptr);
        // `cfg!`, not `#[cfg]`: the probe *sites* are version data and
        // the installer compiles in every build; only the install runs
        // are feature-gated (their trap-dispatch arms and handlers are
        // `#[cfg]`-gated, so patching without the feature would trap
        // into a non-existent handler).
        if cfg!(feature = "log_store") {
            apply_storeperm_loadperm_probes(rom_ptr);
        }
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
/// output into the EL2 UART.
unsafe fn apply_l1_cd_probes(rom_ptr: *mut u32) {
    unsafe {
        // Remember post-SWI fixup: the kernel's `r8 = -10003` sentinel
        // value is loaded after a `bl GenericSWI`. Our handler logs the
        // SWI return and re-establishes the constant before the
        // following `teq`. Required for the kernel's Remember path.
        if let Some(site) = rom_ver::REMEMBER_SWIRET {
            patch_probe(
                rom_ptr,
                site.pc,
                site.orig_insn,
                HvcImm::RememberSwiret,
                "Remember post-SWI",
            );
        }
        // QEMU raspi3b workaround: patch the kernel's `mrs r1, SPSR`
        // at DAH entry so EL2 can substitute the trampoline-saved
        // SPSR_abt for the stale `mrs spsr_abt`.
        if let Some(site) = rom_ver::DAH_MRS_SPSR {
            patch_probe(
                rom_ptr,
                site.pc,
                site.orig_insn,
                HvcImm::DahMrsSpsr,
                "DataAbortHandler mrs r1, SPSR (QEMU spsr_abt staleness fix)",
            );
        }
        // UnhandledException tripwires — halt cleanly with the kernel-
        // supplied exception-name string instead of letting the boot
        // bury the diagnostic under downstream Reboot / abort noise.
        if let Some(sites) = rom_ver::UNHANDLED {
            patch_probe(
                rom_ptr,
                sites.user.pc,
                sites.user.orig_insn,
                HvcImm::UnhandledException,
                "UnhandledException entry (halt-on-entry tripwire)",
            );
            patch_probe(
                rom_ptr,
                sites.non_user.pc,
                sites.non_user.orig_insn,
                HvcImm::UnhandledNumException,
                "UnhandledNonUserModeException entry (halt-on-entry tripwire)",
            );
        }
        // PHammerOutTranslator concrete-body patches: route every
        // `gREPout->{Print,Putc,Flush,StackTrace,ExceptionNotify}`
        // call into the EL2 UART. Always on (no feature gate).
        apply_pouttranslator_patches(rom_ptr);
        // Notification entry probes: every on-screen notice
        // (`Notify` / `ErrorNotify` / `ActionErrorNotify`) is echoed
        // to the EL2 UART. Always on, like the REP output above.
        apply_notify_probes(rom_ptr);
    }
}

/// Install the notification entry probes (`rom_ver::NOTIFY_PROBES`):
/// `Notify(RefVar const&)`'s first insn (`mov r2, r0`) and the
/// `mov ip, sp` prologues of `ErrorNotify` / `ActionErrorNotify` become
/// HVCs. The handlers print the notice's args and emulate the displaced
/// instruction, so the dialog still appears on screen exactly as before
/// — the probe only adds a `notify:` line on serial, which is how a
/// headless power-cycle loop can tell a "clean" boot from one that put
/// up an error notice (e.g. an unexpected REx dialog).
unsafe fn apply_notify_probes(rom_ptr: *mut u32) {
    let Some(sites) = rom_ver::NOTIFY_PROBES else {
        return;
    };
    for (site, imm, name) in [
        (sites.notify, HvcImm::NotifyEntry, "Notify entry probe"),
        (sites.error_notify, HvcImm::ErrorNotifyEntry, "ErrorNotify entry probe"),
        (sites.action_error_notify, HvcImm::ActionErrorNotifyEntry, "ActionErrorNotify entry probe"),
    ] {
        let insn = imm.insn();
        unsafe {
            install_patch(
                rom_ptr,
                site.pc,
                WordKind::Code,
                Some(site.orig_insn),
                &[insn],
                /*optional=*/ false,
                name,
            );
        }
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({}, HVC #{:#x})",
            site.pc, site.orig_insn, insn, name, imm as u32,
        );
    }
}

/// Replace `PHammerOutTranslator`'s output method bodies with HVC
/// stubs that forward to `rep_print` in EL2.
///
/// `gNewtConfig` is patched to `0x8202` (kEnableListener|kDefaultStdioOn|
/// kEnableStdout), so `InitREPOut__Fv` takes the listener branch and
/// stores a `PHammerOutTranslator*` in `gREPout`. Every kernel debug
/// print (`REPprintf`, `REPStackTrace`, `REPExceptionNotify`, the
/// `printf` jump-table entry, ad-hoc kernel diags, plus TInterpreter
/// trace events when the `ns_trace` gate is open) eventually reaches
/// the abstract-base thunks which vtable-dispatch into
/// PHammerOutTranslator's concrete methods. Stock those methods hand
/// bytes off to a `vfprintf`/`fputc` chain whose stream nobody drains,
/// so the bytes vanish.
///
/// We replace the body of each method with an `HVC` that forwards args
/// to `rep_print` (which renders via `kprintln!` to the EL2 UART) plus
/// a small return tail. The dispatch architecture is untouched —
/// `gREPout->Print(fmt, ...)` still goes through the natural
/// abstract-base thunk and concrete-subclass vtable lookup; we are
/// merely the implementation.
///
/// For `Print`/`Putc`/`Flush` the body is overwritten with three words:
/// `HVC #imm`, `mov r0, #0`, `mov pc, lr`. The handler renders, ELR
/// advances by 4, the natively-executing `mov r0, #0; mov pc, lr`
/// returns 0 to the caller. Original body bytes beyond word 2 are
/// dead.
///
/// For `StackTrace`/`ExceptionNotify` the original body is just
/// `mov r0, r1; b REP*` (8 bytes). We patch only word 0, replacing
/// `mov r0, r1` with `HVC`. The handler emulates `mov r0, r1`
/// (`ctx.x[0] = ctx.x[1]`) before ELR advances; the second word is
/// the original `b REPStackTrace`/`b REPExceptionNotify` which fires
/// natively, formats, and Prints — landing back in our patched
/// `Print` body and out the UART.
///
/// `Print`'s args follow standard ARM EABI varargs:
///   r0 = `this` (ignored), r1 = fmt, r2/r3 = first two args, then
///   the rest at the caller's source-mode SP. `diag::rep_print`
///   walks the format string and pulls args accordingly.
unsafe fn apply_pouttranslator_patches(rom_ptr: *mut u32) {
    let Some(hammer) = rom_ver::HAMMER else {
        return;
    };

    // Print/Putc/Flush: 3-word body replacement.
    //   word 0: HVC #imm
    //   word 1: mov r0, #0   (e3a00000)
    //   word 2: mov pc, lr   (e1a0f00e)
    const MOV_R0_0: u32 = 0xE3A0_0000;
    const MOV_PC_LR: u32 = 0xE1A0_F00E;

    let bodies = [
        (
            hammer.print,
            HvcImm::HammerPrint,
            "PHammerOutTranslator::Print body",
        ),
        (
            hammer.putc,
            HvcImm::HammerPutc,
            "PHammerOutTranslator::Putc body",
        ),
        (
            hammer.flush,
            HvcImm::HammerFlush,
            "PHammerOutTranslator::Flush body",
        ),
    ];
    for &(site, hvc, name) in &bodies {
        // SAFETY: rom_ptr backs the full main ROM; site.pc is in it.
        // These bodies are load-bearing (the kernel's REP output path);
        // install_patch halts on a first-word mismatch rather than
        // skipping, and records all three overwritten originals.
        unsafe {
            install_patch(
                rom_ptr,
                site.pc,
                WordKind::Code,
                Some(site.orig_insn),
                &[hvc.insn(), MOV_R0_0, MOV_PC_LR],
                /*optional=*/ false,
                name,
            );
        }
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> HVC #{:#x} + mov r0,#0 + mov pc,lr  ({})",
            site.pc,
            site.orig_insn,
            hvc as u32,
            name,
        );
    }

    // StackTrace/ExceptionNotify: word-0 only. Original second word
    // (`b REP*`) runs natively after HVC; handler emulates `mov r0, r1`.
    // SAFETY: rom_ptr backs the full main ROM.
    unsafe {
        patch_probe(
            rom_ptr,
            hammer.stack_trace.pc,
            hammer.stack_trace.orig_insn,
            HvcImm::HammerStackTrace,
            "PHammerOutTranslator::StackTrace body (mov r0,r1 → HVC)",
        );
        patch_probe(
            rom_ptr,
            hammer.exception_notify.pc,
            hammer.exception_notify.orig_insn,
            HvcImm::HammerExceptionNotify,
            "PHammerOutTranslator::ExceptionNotify body (mov r0,r1 → HVC)",
        );
    }

    // PHammerInTranslator — the REP input path, fed from the host.
    //
    // FrameAvailable: 2-word body replacement (`HVC` + `mov pc, lr`);
    // the handler sets r0 = "host line queued".
    // SAFETY: rom_ptr backs the full main ROM; the sites are in it.
    unsafe {
        install_patch(
            rom_ptr,
            hammer.in_frame_available.pc,
            WordKind::Code,
            Some(hammer.in_frame_available.orig_insn),
            &[HvcImm::HammerFrameAvailable.insn(), MOV_PC_LR],
            /*optional=*/ false,
            "PHammerInTranslator::FrameAvailable body",
        );
    }
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> HVC #{:#x} + mov pc,lr  (PHammerInTranslator::FrameAvailable body)",
        hammer.in_frame_available.pc,
        hammer.in_frame_available.orig_insn,
        HvcImm::HammerFrameAvailable as u32,
    );

    // ProduceFrame: NOP the FILE*-NULL gate (fopen of the Hammer
    // console device fails here, leaving FILE* = 0, and the original
    // `beq` would skip the read), then replace the `bl fgets` with an
    // HVC whose handler fills the line buffer from the host queue.
    // The rest of ProduceFrame (MakeString + ParseString) runs
    // natively on the filled buffer.
    const NOP_MOV_R0_R0: u32 = 0xE1A0_0000;
    // SAFETY: as above.
    unsafe {
        install_patch(
            rom_ptr,
            hammer.in_file_gate.pc,
            WordKind::Code,
            Some(hammer.in_file_gate.orig_insn),
            &[NOP_MOV_R0_R0],
            /*optional=*/ false,
            "PHammerInTranslator::ProduceFrame FILE* gate (beq → nop)",
        );
        patch_probe(
            rom_ptr,
            hammer.in_fgets.pc,
            hammer.in_fgets.orig_insn,
            HvcImm::HammerFgets,
            "PHammerInTranslator::ProduceFrame fgets (bl → HVC)",
        );
    }
}

/// Patch the kernel's known `LDR` sites that read the faulting
/// (or trapping) instruction word as data (`rom_ver::INSN_AS_DATA_LDRS`
/// plus the conditional FPE pair in `rom_ver::FPE_LDRS`).
///
/// Under load-time BE-8 byteswap of code-marked memory, the kernel's
/// CPSR.E=1 `LDR` returns the bytes in the wrong order — the
/// numerical value is the byteswap of the original instruction
/// encoding the kernel was compiled to recognise.
///
/// Each site is replaced with `B stub`, where the stub re-emits the
/// LDR, byteswaps the result with `REV Rd, Rd`, and falls through
/// with `B resume` (resume = site + 4). The kernel was compiled for
/// ARMv4 (no REV) but the host CPU is ARMv8 / Cortex-A53 in AArch32
/// mode — which decodes every ARMv6+ instruction including REV. Three
/// words per stub.
///
/// REV (A1) encoding: `cond 0110 1011 1111 Rd 1111 0011 Rm`. For
/// `REV Rd, Rd`: 0xE6BF_0F30 | (Rd << 12) | Rd.
///
/// The FPE prelude's pair is special-shaped: two conditional sites
/// (BEQ for USR-source, BNE for non-USR-source) both branch to one
/// shared stub whose LDR is given explicitly (`FpeLdrSites::stub_ldr`,
/// the unconditional form of the original `ldrteq`).
unsafe fn apply_fault_handler_ldr_byteswap_patches(rom_ptr: *mut u32) {
    unsafe {
        for entry in rom_ver::INSN_AS_DATA_LDRS {
            let orig = entry.site.orig_insn;
            let pc = entry.site.pc;
            let resume_pc = pc + 4;
            let rd = (orig >> 12) & 0xF;
            let rev = 0xE6BF_0F30 | (rd << 12) | rd;
            let stub_pc = alloc_patch_stub(3, entry.name);
            let stub: [u32; 3] = [
                orig,                             // the displaced LDR
                rev,                              // REV Rd, Rd
                arm_b(stub_pc + 0x08, resume_pc), // B resume
            ];
            write_stub_words(rom_ptr, stub_pc, &stub);

            // The redirect site. Load-bearing (the kernel's own fault
            // handlers read garbage without it), so a mismatch halts
            // via install_patch instead of skipping.
            let insn = arm_b(pc, stub_pc);
            install_patch(
                rom_ptr,
                pc,
                WordKind::Code,
                Some(orig),
                &[insn],
                /*optional=*/ false,
                entry.name,
            );
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} → B stub @ {:#x}, byteswap)",
                pc,
                orig,
                insn,
                entry.name,
                stub_pc,
            );
        }

        // FPE prelude: two conditional sites (BEQ for USR-source,
        // BNE for non-USR-source) both pointing at the same byteswap
        // stub.
        if let Some(fpe) = rom_ver::FPE_LDRS {
            let rd = (fpe.stub_ldr >> 12) & 0xF;
            let rev = 0xE6BF_0F30 | (rd << 12) | rd;
            let fpe_stub_pc = alloc_patch_stub(3, "FPE prelude faulting-insn LDR byteswap");
            let fpe_stub: [u32; 3] = [fpe.stub_ldr, rev, arm_b(fpe_stub_pc + 0x08, fpe.resume_pc)];
            write_stub_words(rom_ptr, fpe_stub_pc, &fpe_stub);
            for (site, cond, label) in [
                (fpe.eq_site, 0x0u32, "FPE ldrteq fp,[r9]"),
                (fpe.ne_site, 0x1u32, "FPE ldrne  fp,[r9]"),
            ] {
                let insn = arm_b_cond(site.pc, fpe_stub_pc, cond);
                install_patch(
                    rom_ptr,
                    site.pc,
                    WordKind::Code,
                    Some(site.orig_insn),
                    &[insn],
                    /*optional=*/ false,
                    label,
                );
                kprintln!(
                    "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} → B{} stub @ {:#x}, byteswap)",
                    site.pc,
                    site.orig_insn,
                    insn,
                    label,
                    if cond == 0 { "EQ" } else { "NE" },
                    fpe_stub_pc,
                );
            }
        }
    }
}

/// Helper: replace one ROM word with an HVC, halting loudly if the
/// previous word doesn't match the recorded original. A mismatch means
/// the ROM has shifted under us (different ROM image or earlier patch
/// stomped the same offset) and the probe handler's emulation of the
/// "original" first instruction would be wrong — a skip here would
/// guarantee a baffling downstream wedge, so `install_patch` halts.
unsafe fn patch_probe(
    rom_ptr: *mut u32,
    pc: u32,
    expected_orig: u32,
    hvc_imm: HvcImm,
    name: &'static str,
) {
    let new_insn = hvc_imm.insn();
    let imm = hvc_imm as u32;
    // SAFETY: caller of apply_rom_patches has already bounded rom_ptr.
    unsafe {
        install_patch(
            rom_ptr,
            pc,
            WordKind::Code,
            Some(expected_orig),
            &[new_insn],
            /*optional=*/ false,
            name,
        );
    }
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} probe, HVC #{:#x})",
        pc,
        expected_orig,
        new_insn,
        name,
        imm
    );
}

/// Iter-50: side-table of `(pc, original_instruction)` pairs for ROM
/// PCs that `patch_probe` has overwritten with an HVC. inline_patch's
/// liveness analyser consults this table via `read_original` so it
/// sees the pre-patch instruction stream — necessary because
/// `apply_rom_patches` runs before any inline stub is installed, and
/// without this table the analyser misclassifies probe-HVCs
/// (e.g. picks R12 as scratch_ea at FindSuperceeder body's
/// 0x001488ac because the original `mov r0, ip` at 0x001488c4 has
/// been replaced with HVC #0x6E for the FINDSUPER_MID probe).
///
/// Capacity = 256: covers every code word any installer overwrites —
/// `install_patch` records ALL overwritten code words (multi-word
/// bodies included) plus the `rom_ver::PATCHES` code entries, not just
/// the HVC probe sites. When the table fills, `record_original` halts
/// loudly — silently dropping entries makes `inline_patch`'s liveness
/// analyser see the patched HVC instead of the original, leading to
/// subtle scratch-register misanalysis at nearby inline
/// stub sites. Single-threaded boot use, so a plain `static mut` with
/// index counter is safe.
const ORIG_CAP: usize = 256;
static mut ORIG_PCS: [u32; ORIG_CAP] = [0; ORIG_CAP];
static mut ORIG_INSNS: [u32; ORIG_CAP] = [0; ORIG_CAP];
static mut ORIG_N: usize = 0;

fn record_original(pc: u32, orig: u32) {
    // SAFETY: single-threaded boot path; `apply_rom_patches`
    // runs once on core 0 before the guest is ERET'd in. Use raw
    // pointers so we don't trip the rust_2024_compatibility lint
    // about shared/mutable references to a static mut.
    unsafe {
        let n_ptr = core::ptr::addr_of_mut!(ORIG_N);
        let n = n_ptr.read();
        if n >= ORIG_CAP {
            // Silently dropping entries causes inline_patch's liveness
            // analyser to see the patched HVC instead of the original
            // instruction at this PC, leading to mis-classified scratch
            // registers at nearby inline stub sites and
            // hard-to-diagnose downstream corruption. Bump ORIG_CAP and
            // rebuild rather than letting boot continue with a partial
            // table.
            kprintln!(
                "rom_patch: FATAL — ORIG_PCS table full ({} entries, ORIG_CAP={}) \
                 trying to record PC={:#010x}; bump ORIG_CAP in src/newton/rom_patches.rs",
                n,
                ORIG_CAP,
                pc
            );
            crate::arch::cpu::halt();
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

/// Replace the UND-table slots for DebugStr / Debugger
/// (`rom_ver::DEBUG_UND_SLOTS`) with branches to small stubs that
/// stash the guest's LR into r7 and then HVC to EL2. Einstein's
/// callbacks do `SetRegister(15, LR + 4)` for DebugStr and
/// `SetRegister(15, LR + 8)` for Debugger
/// (`Emulator/JIT/Generic/TJITGenericROMPatch.cpp:76-102`);
/// our HVC handler reads the stashed LR (ctx.x[7]) and advances ELR_EL2
/// by the matching delta.
///
/// The MOV/HVC pair doesn't fit inline: the two slots are adjacent
/// entries in the Newton UND-dispatch table, each reachable as an
/// independent BL target, so neither can occupy two words.
unsafe fn apply_debug_patches(rom_ptr: *mut u32) {
    let Some(slots) = rom_ver::DEBUG_UND_SLOTS else {
        return;
    };
    let debug_str_stub_pc = alloc_patch_stub(2, "DebugStr stub");
    let debugger_stub_pc = alloc_patch_stub(2, "Debugger stub");
    // MOV r7, lr = E1A0_700E ; HVC #imm
    let debugstr_stub: [u32; 2] = [0xE1A0_700E, HvcImm::DebugStr.insn()];
    let debugger_stub: [u32; 2] = [0xE1A0_700E, HvcImm::Debugger.insn()];
    unsafe {
        write_stub_words(rom_ptr, debug_str_stub_pc, &debugstr_stub);
        write_stub_words(rom_ptr, debugger_stub_pc, &debugger_stub);

        // UND-table slot originals verified against rom.dis: the
        // DebugStr/Debugger entries hold the UNDEFINED-space debugger
        // marker words.
        for (site, stub_pc, hvc, what) in [
            (
                slots.debug_str,
                debug_str_stub_pc,
                HvcImm::DebugStr,
                "DebugStr",
            ),
            (
                slots.debugger,
                debugger_stub_pc,
                HvcImm::Debugger,
                "Debugger",
            ),
        ] {
            let insn = arm_b(site.pc, stub_pc);
            install_patch(
                rom_ptr,
                site.pc,
                WordKind::Code,
                Some(site.orig_insn),
                &[insn],
                /*optional=*/ false,
                what,
            );
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} → B {:#x}, HVC #{:#x})",
                site.pc,
                site.orig_insn,
                insn,
                what,
                stub_pc,
                hvc as u32,
            );
        }
    }
}

/// Writes a sequence of ARM instruction encodings into a fresh
/// patch-stub-arena slot. All entries here are code (HVC stub bodies,
/// branch targets, etc.). Arena slots have no meaningful prior
/// contents, so this is `install_patch`'s one sanctioned blind-write
/// caller (`expected_orig = None`: no verify, no original recorded).
unsafe fn write_stub_words(rom_ptr: *mut u32, base: u32, words: &[u32]) {
    unsafe {
        install_patch(
            rom_ptr,
            base,
            WordKind::Code,
            None,
            words,
            /*optional=*/ false,
            "patch-stub arena slot",
        );
    }
}

/// Replace `RealClockSeconds` with a 4-word stub that reads the MMIO
/// calendar register (populated by `peripherals::vic::calendar_seconds`
/// via `stage2::tick_page::publish`) and returns. Einstein's equivalent
/// is the native-call patch at `TJITGenericROMPatch.cpp:110` that calls
/// host `time()`; we serve the same value from a different layer, so
/// the callback is a simple read-register-then-return.
unsafe fn apply_real_clock_seconds_patch(rom_ptr: *mut u32) {
    let Some(site) = rom_ver::REAL_CLOCK_SECONDS else {
        return;
    };
    let entry = site.entry;
    // entry+0x00: LDR r0, [pc, #4]        -- load literal at entry+0x0C
    // entry+0x04: LDR r0, [r0]            -- dereference calendar address
    // entry+0x08: MOV PC, LR              -- return
    // entry+0x0C: .word TICK_PAGE_IPA     -- calendar MMIO IPA
    //
    // First 3 words are ARM instructions (code); the literal at +12 is
    // data that the LDR at +0 loads into r0. Under BE-8 the LDR is
    // byteswapping, so the literal must be written as data (BE-encoded
    // bytes on host).
    // Originals (`site.prologue_origs`) are the function's own prologue
    // (mov ip,sp / push {r4,fp,ip,lr,pc} / sub fp,ip,#4 / sub sp,sp,#8),
    // verified against rom.dis.
    let insns: [u32; 3] = [0xE59F_0004, 0xE590_0000, 0xE1A0_F00E];
    let literal: u32 = crate::hv::layout::TICK_PAGE_IPA as u32;
    unsafe {
        for (i, w) in insns.iter().copied().enumerate() {
            let offset = entry + (i as u32) * 4;
            let orig = site.prologue_origs[i];
            install_patch(
                rom_ptr,
                offset,
                WordKind::Code,
                Some(orig),
                &[w],
                /*optional=*/ false,
                "RealClockSeconds body",
            );
            kprintln!(
                "rom_patch: {:#010x}: {:#010x} -> insn={:#010x}  (RealClockSeconds)",
                offset,
                orig,
                w,
            );
        }
        // The literal slot overwrites a CODE word (the prologue's
        // `sub sp, sp, #8`) with a DATA word, so install_patch's
        // single-kind read/write contract doesn't fit: verify and
        // record the original via the Code view, then write as data so
        // the kernel's BE-8 LDR reads the literal back numerically.
        let lit_offset = entry + 12;
        let lit_idx = (lit_offset / 4) as usize;
        let lit_orig = site.prologue_origs[3];
        let prev = read_rom_word_numeric(rom_ptr, lit_idx, WordKind::Code);
        if prev != lit_orig {
            kprintln!(
                "*** rom_patch: RealClockSeconds literal at {:#010x} is {:#010x}, expected {:#010x} — ROM shifted under the patch installer; refusing to continue",
                lit_offset, prev, lit_orig,
            );
            crate::arch::cpu::halt();
        }
        record_original(lit_offset, prev);
        crate::hv::guest_mem::write_rom_data_word(rom_ptr, lit_idx, literal);
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> lit={:#010x}  (RealClockSeconds literal)",
            lit_offset,
            prev,
            literal,
        );
    }
}

/// FTimeInSeconds injection patch: replace the last shift before the
/// function epilogue (originally `MOV r0, r0, LSL #2`) with a branch
/// to a stub that subtracts `safeIntervalDeltaSeconds` and performs
/// the NS-integer `<< 2`, then branches to the epilogue — net effect
/// `r0 = (r0 - delta) << 2`. Einstein's equivalent at
/// `TJITGenericROMPatch.cpp:150` uses `T_ROM_PATCH` which *replaces*
/// the original instruction (per `TJITGenericROMPatch.h:283` "return
/// ioUnit if the next instruction is to be executed"), so the
/// original `LSL #2` does **not** run after Einstein's callback.
unsafe fn apply_ftime_in_seconds_patch(rom_ptr: *mut u32) {
    let Some(site) = rom_ver::FTIME_IN_SECONDS else {
        return;
    };
    let ftime_stub_pc = alloc_patch_stub(5, "FTimeInSeconds stub");
    // Stub body (5 words):
    //   +0x00 LDR r12, [pc, #8]           ; load delta from +0x10
    //   +0x04 SUB r0, r0, r12             ; r0 = r0 - delta
    //   +0x08 MOV r0, r0, LSL #2          ; NS-integer encode
    //   +0x0C B <resume_pc>               ; resume at the epilogue
    //   +0x10 .word safeIntervalDeltaSeconds
    let stub_b = arm_b(ftime_stub_pc + 0x0C, site.resume_pc);
    let stub: [u32; 5] = [
        0xE59F_C008, // LDR r12, [pc, #8]
        0xE040_000C, // SUB r0, r0, r12
        0xE1A0_0100, // MOV r0, r0, LSL #2
        stub_b,      // B resume_pc
        rom_ver::SAFE_INTERVAL_DELTA_SECONDS,
    ];
    let patch_insn = arm_b(site.patch.pc, ftime_stub_pc);
    // Original at the patch site verified against rom.dis: lsl r0, r0, #2.
    unsafe {
        write_stub_and_patch(
            rom_ptr,
            ftime_stub_pc,
            &stub,
            site.patch.pc,
            site.patch.orig_insn,
            patch_insn,
            "FTimeInSeconds",
        );
    }
}

/// FDateFromSeconds injection patch: replace the `MOV r0, sp` at the
/// patch site with a branch to a stub that adds
/// `safeIntervalDeltaSeconds` to r1, re-executes `MOV r0, sp`, and
/// branches to the instruction after the patch site. Einstein's
/// equivalent at `TJITGenericROMPatch.cpp:160`.
unsafe fn apply_fdate_from_seconds_patch(rom_ptr: *mut u32) {
    let Some(site) = rom_ver::FDATE_FROM_SECONDS else {
        return;
    };
    let fdate_stub_pc = alloc_patch_stub(5, "FDateFromSeconds stub");
    let stub_b = arm_b(fdate_stub_pc + 0x0C, site.resume_pc);
    let stub: [u32; 5] = [
        0xE59F_C008, // LDR r12, [pc, #8]
        0xE081_100C, // ADD r1, r1, r12
        0xE1A0_000D, // MOV r0, sp (= MOV r0, r13) — original instruction
        stub_b,      // B resume_pc
        rom_ver::SAFE_INTERVAL_DELTA_SECONDS,
    ];
    let patch_insn = arm_b(site.patch.pc, fdate_stub_pc);
    // Original at the patch site verified against rom.dis: mov r0, sp.
    unsafe {
        write_stub_and_patch(
            rom_ptr,
            fdate_stub_pc,
            &stub,
            site.patch.pc,
            site.patch.orig_insn,
            patch_insn,
            "FDateFromSeconds",
        );
    }
}

/// Patch the first word of each of `PowerOffAndReboot`, `Reboot`, and
/// `StopImage` with a single `HVC #HvcImm::LoudHalt`, plus the
/// `bl Throw` inside `TStackManager::Fault` (busError). The handler in
/// `diag::trap_diag::handle_loud_halt` dumps the calling context
/// (R0..R3, mode, caller LR) and halts — we never resume. Catches the
/// boot-fail-and-reboot loop AND the idle/sleep wait-for-wakeup spin
/// the FIRST time either fires, instead of letting the run go on for
/// tens of thousands of repeated tracer entries before timeout.
#[cfg(nh_loud_halt_canaries)]
unsafe fn apply_loud_halt_traps(rom_ptr: *mut u32) {
    let Some(lh) = rom_ver::LOUD_HALT else {
        return;
    };
    let insn = HvcImm::LoudHalt.insn();
    // Originals verified against rom.dis: the first three are function
    // prologues (mov ip,sp ×2 / mrc p15 CPU-ID read); the fourth is the
    // busError `bl Throw`.
    for (site, name) in [
        (lh.poweroff_reboot, "PowerOffAndReboot"),
        (lh.reboot, "Reboot"),
        (lh.stop_image, "StopImage"),
        (lh.bus_error_throw, "BusErrorThrow"),
    ] {
        unsafe {
            install_patch(
                rom_ptr,
                site.pc,
                WordKind::Code,
                Some(site.orig_insn),
                &[insn],
                /*optional=*/ false,
                name,
            );
        }
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({} loud-halt, HVC #{:#x})",
            site.pc,
            site.orig_insn,
            insn,
            name,
            HvcImm::LoudHalt as u32,
        );
    }
}

/// Software-reset canary at `BootOS`. Overwrite the first word with
/// `HVC #HvcImm::BootOs`; the handler distinguishes the legitimate
/// first boot from a reset by counting entries. `install_patch` halts
/// at install time if the current first word isn't the expected
/// `mov r0, #0xb0` — a ROM change would silently break the handler's
/// emulation of the displaced instruction, so we want a loud
/// notification at install.
unsafe fn apply_bootos_trap(rom_ptr: *mut u32) {
    let Some(boot) = rom_ver::BOOT else {
        return;
    };
    let insn = HvcImm::BootOs.insn();
    // SAFETY: bounded; patch runs on the main ROM half.
    unsafe {
        install_patch(
            rom_ptr,
            boot.bootos.pc,
            WordKind::Code,
            Some(boot.bootos.orig_insn),
            &[insn],
            /*optional=*/ false,
            "BootOS canary",
        );
    }
    kprintln!(
        "rom_patch: {:#010x}: {:#010x} -> {:#010x}  (BootOS canary, HVC #{:#x})",
        boot.bootos.pc,
        boot.bootos.orig_insn,
        insn,
        HvcImm::BootOs as u32,
    );
}

/// Install the StorePermObject entry probe + LoadPermObject
/// return-site probe (`rom_ver::STORE_PROBES`). Pair: each call to
/// StorePermObject pretty-prints the RefArg being stored, each return
/// from LoadPermObject pretty-prints the Ref being handed back. Used
/// to investigate whether the flash-store round-trip is corrupting
/// the Ref graph. Gated by the `log_store` Cargo feature — when off,
/// the trap dispatch arms and handlers are cfg'd out, so leaving
/// these patches uninstalled is required to avoid trapping into a
/// non-existent handler (hence the `cfg!` gate at the call site).
unsafe fn apply_storeperm_loadperm_probes(rom_ptr: *mut u32) {
    let Some(probes) = rom_ver::STORE_PROBES else {
        return;
    };
    for (site, imm, name) in [
        (
            probes.store_perm_entry,
            HvcImm::StorePermObjEntry,
            "StorePermObject entry probe",
        ),
        (
            probes.load_perm_ret,
            HvcImm::LoadPermObjRet,
            "LoadPermObject return probe",
        ),
    ] {
        let insn = imm.insn();
        unsafe {
            install_patch(
                rom_ptr,
                site.pc,
                WordKind::Code,
                Some(site.orig_insn),
                &[insn],
                /*optional=*/ false,
                name,
            );
        }
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({}, HVC #{:#x})",
            site.pc,
            site.orig_insn,
            insn,
            name,
            imm as u32,
        );
    }
}

/// Shared helper for the two injection patches: write a 5-word stub at
/// `stub_pc` (4 instruction words + 1 trailing data literal, fresh
/// arena slots — blind writes) and a 1-word branch at `patch_pc`
/// (verified against `expected_orig`, halting on mismatch).
unsafe fn write_stub_and_patch(
    rom_ptr: *mut u32,
    stub_pc: u32,
    stub: &[u32; 5],
    patch_pc: u32,
    expected_orig: u32,
    patch_insn: u32,
    name: &'static str,
) {
    unsafe {
        install_patch(
            rom_ptr,
            stub_pc,
            WordKind::Code,
            None,
            &stub[..4],
            /*optional=*/ false,
            "patch-stub arena slot",
        );
        install_patch(
            rom_ptr,
            stub_pc + 16,
            WordKind::Data,
            None,
            &stub[4..],
            /*optional=*/ false,
            "patch-stub arena literal",
        );
        install_patch(
            rom_ptr,
            patch_pc,
            WordKind::Code,
            Some(expected_orig),
            &[patch_insn],
            /*optional=*/ false,
            name,
        );
        kprintln!(
            "rom_patch: {:#010x}: {:#010x} -> {:#010x}  ({}: B {:#x}, 5-word stub)",
            patch_pc,
            expected_orig,
            patch_insn,
            name,
            stub_pc,
        );
    }
}

// Rust-side tests would live here, but this crate is `no_std` (it
// defines its own `#[panic_handler]`), so `cargo test` can't link
// the built-in test crate. Verification happens via
// `guest-tests/tests/test_rom_patches.S` (HVC-handler behaviour) and
// the real-ROM boot path (which exercises every patch the Newton
// kernel reaches).
