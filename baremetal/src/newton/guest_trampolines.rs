//! Guest-visible AArch32 trampolines installed into the ROM aperture.
//!
//! The hypervisor rewrites the guest's UND / DABT / PABT exception
//! vectors to branch into small hand-assembled AArch32 stubs that live
//! in otherwise-unused ROM tail space. Those stubs save banked state and
//! `HVC` into EL2 (or, for the common DABT cases, fast-forward straight
//! to the kernel's DataAbortHandler without an EL2 round trip). This
//! module owns:
//!   * the trampoline address-range constants (`UND_TRAMP_OFFSET`,
//!     `FPA_BYPASS_STUB_OFFSET`, `DABT_TRAMP_OFFSET`,
//!     `DABT_FAST_TRAMP_OFFSET`, `UND_RETURN_STUB_OFFSET`, …),
//!   * the installers (`patch_und_vector`, `patch_dabt_vector`,
//!     `install_dabt_fast_trampoline`, `install_und_vector_swap_*`), and
//!   * `register_hyp_code_ranges` — registers every runtime-written
//!     code region with the layout manifest, whose `is_hyp_code` query
//!     is shared by `guest_endian` (don't byte-swap these words) and
//!     `snapshot` (a guest PC parked here is mid-trampoline and must
//!     not anchor an autosave).
//!
//! `guest_mem` keeps the memory-access layer (typed PA reads/writes,
//! the stage-1 walker, the ROM/RAM/FB backing stores); this module is
//! purely the trampoline assembler. The branch / literal-load encoders
//! it uses come from `crate::arch::aarch32_emit` (compile-time-verified).
//!
//! The I-cache publish for everything written here is the single
//! whole-ROM `icache_publish_range` sweep at the end of
//! `loader::load_newton_rom`, which runs strictly after every
//! installer in this module.

use crate::hv::guest_mem::{rom_host_pa, write_rom_code_word, write_rom_data_word};
use crate::hv::hvc_imm::HvcImm;
use crate::kprintln;

use super::rom_ver;

/// Install the AArch32 UND-vector trampoline.
///
/// The trampoline body lives in the 16 MiB ROM region at offset
/// `UND_TRAMP_OFFSET` — well past the REx tail (Einstein.rex ends
/// ~0x0084_7000) and in guaranteed-zero padding that the kernel
/// can't plausibly touch. A 64-byte ROM region this deep is free
/// game for us. The vector at VA 0x04 branches to it.
///
/// It must not sit at ROM offset 0x80 (inside the 256-byte header
/// that reads as zeros in the raw dump) even though that region also
/// looks free: the 717006 kernel reads it as a table, so turning
/// those zeros into instructions breaks the boot. Only placement far
/// beyond the REx tail avoids that aliasing.
///
/// Trampoline body:
///   +0x00: ee0dcf50  mcr p15,0,r12,c13,c0,2 ; TPIDRURW <- R12 (save orig R12)
///   +0x04: e59fc050  ldr r12, [pc, #0x50]  ; literal at +0x5C: save VA
///   +0x08: e58c000c  str r0, [r12, #0x0C]  ; save pre-UND R0      (+0x0C)
///   +0x0C: e58c1010  str r1, [r12, #0x10]  ; save pre-UND R1      (+0x10)
///   +0x10: e58ce000  str lr, [r12]         ; save R14_und         (+0x00)
///   +0x14: e14f0000  mrs r0, SPSR          ; r0 = SPSR_und
///   +0x18: e58c0004  str r0, [r12, #4]     ; save SPSR_und        (+0x04)
///   +0x1C: e58c2014  str r2, [r12, #0x14]  ; save pre-UND R2      (+0x14)
///   +0x20: e200101f  and r1, r0, #0x1F     ; r1 = faulting mode bits
///   +0x24: e38110c0  orr r1, r1, #0xC0     ; r1 |= I/F mask
///   +0x28: e35100d0  cmp r1, #0xD0         ; == USR (0x10) + IF ?
///   +0x2C: 03a010df  moveq r1, #0xDF       ; if USR → use SYS (same bank)
///   +0x30: e129f001  msr cpsr_c, r1        ; switch to faulting mode
///   +0x34: e58cd018  str sp, [r12, #0x18]  ; save banked SP       (+0x18)
///   +0x38: e58ce01c  str lr, [r12, #0x1C]  ; save banked LR       (+0x1C)
///   +0x3C: e321f0db  msr cpsr_c, #0xdb     ; → UND (I/F masked)
///   +0x40: e59c2014  ldr r2, [r12, #0x14]  ; restore pre-UND R2
///   +0x44: e321f0d3  msr cpsr_c, #0xd3     ; → SVC (I/F masked)
///   +0x48: e1a0000e  mov r0, lr            ; r0 = R14_svc
///   +0x4C: e58c0008  str r0, [r12, #8]     ; save LR_svc          (+0x08)
///   +0x50: e321f0db  msr cpsr_c, #0xdb     ; → UND (I/F masked)
///   +0x54: e1400170  hvc #0x10             ; UND_TAG — enter EL2
///   +0x58: eafffffe  b .                   ; trap if we ever return
///   +0x5C: 0c004f00  .word UND_SAVE_BASE_VA (RAM-mirror VA)
///
/// Historical note on the SVC bounce: per ARM ARM Table D1-79,
/// AArch32 R14_svc is the AArch64 X18 register at AArch32→AArch64
/// exception entry (and `ELR_EL1` is an AArch64-only EL1 register
/// with no architectural alias to R14_svc). The trampoline could
/// therefore read LR_svc directly from `ctx.x[18]` at EL2 entry,
/// without the brief `msr cpsr_c, #0xd3` mode bounce.
/// `MRS X, LR_svc` is **NOT** a defined AArch64 sysreg encoding —
/// MRS (Banked register) is AArch32-only per F7.1.115 — so reads of
/// `LR_svc` as if it were a sysreg always come back as 0 / undefined
/// regardless of platform; that was a misdiagnosis.
///
/// Why save R0 and R1 first: the trampoline clobbers R0 (to hold the
/// save-slot VA for the SPSR/LR stores) and R1 (to carry SPSR_und
/// across the mode bounce). Without persisting the pre-UND values,
/// the guest's first two argument registers are scrambled whenever
/// the tracer UDFs a function entry — caught in Phase B as a bogus
/// PA 0x78 write from StoreToPhysAddress, which was actually
/// AddPgPAndPermWithPageTable's prologue shuffling the clobbered R0
/// (0x0C00_4F00) and R1 (LR_svc) into R7 and R4 before using them
/// as a page-table base. `handle_und` restores `ctx.x[0]` and
/// `ctx.x[1]` from these slots at entry; by the time execution ERETs
/// back to the guest the registers are intact. R12 is preserved by the
/// opening `MCR p15,0,r12,c13,c0,2` which stashes the original R12 into
/// TPIDRURW (TPIDR_EL0 in AArch64); `handle_und` reads `tpidr_el0` and
/// restores `ctx.x[12]`. TPIDRURW is ARMv6+ architectural state that
/// SA-1100 (ARMv4) did not have, and the Newton ROM never touches it,
/// so claiming it as the R12 save slot is safe. This matters for
/// mid-function UDF sites, where the faulting instruction can
/// legitimately use R12 as base/data/offset; the tracer's
/// function-entry assumption (`MOV R12, R13` on every prologue) does
/// not hold there.
///
/// Branch encoding at VA 0x04: `b UND_TRAMP_OFFSET`.
///   imm24 = (UND_TRAMP_OFFSET - (0x04 + 8)) / 4
///
/// Note: the guest's stage-1 L1[0x0F] maps VA 0x00F00000-
/// 0x00FFFFFF identity to the ROM, so VA 0x00FFFF00 is the PC
/// the CPU lands at. The literal holds a VA, which the guest's
/// stage-1 translates through L1[0xC0] coarse -> L2[0x04] small
/// page -> PA 0x04005F00 (RAM). We can't use the raw IPA 0x04005F00
/// as the literal because the guest's L1[0x40] section maps VA
/// 0x0400_xxxx to PA 0x0000_xxxx (ROM, RO under stage-2) post-MMU.
///
/// Safety: caller must hold exclusive access to the ROM backing
/// store. Writes 13 words at the trampoline offset + 1 word at 0x04.
const UND_TRAMP_OFFSET: usize = rom_ver::ROM_TAIL.und_tramp as usize;

/// FPA-class UND bypass stub at `0x00FF_FEC0`. The UND vector at IPA
/// 0x04 branches here first; the stub checks if the faulting instruction
/// is FPA-class (coprocessor field = 1 or 2) and, if so, branches
/// directly to `FP_UndefHandlers_Start_JT` at `0x38d874` — exactly the
/// path SA-110 hardware took on the original Newton (UND vector held
/// `b 0x1a031f4` which thunked through 0x38d874 to FP_UndefHandlers_Start
/// at 0x38d8dc). Non-FPA UNDs fall through to the existing trampoline
/// at `UND_TRAMP_OFFSET`, which captures source-mode banked state and
/// HVCs into EL2 for the rest of our handlers (tracer UDFs, software
/// breakpoints, SWP/SWPB, deprecated CP15, etc.).
///
/// Stub layout (16 words = 64 bytes):
///   +0x00: ee0d_cf50  mcr p15,0,r12,c13,c0,2  ; save R12 → TPIDRURW
///   +0x04: e51e_c004  ldr r12, [lr, #-4]      ; R12 = faulting insn
///   +0x08: e20c_c40f  and r12, r12, #0x0F000000 ; isolate bits[27:24]
///   +0x0C: e35c_040c  cmp r12, #0x0C000000    ; LDC/STC variant?
///   +0x10: 135c_040d  cmpne r12, #0x0D000000  ; LDC/STC variant?
///   +0x14: 135c_040e  cmpne r12, #0x0E000000  ; CDP/MCR/MRC?
///   +0x18: 1a00_xxxx  bne UND_TRAMP_OFFSET    ; bits[27:24] not FPA-class
///   +0x1C: e51e_c004  ldr r12, [lr, #-4]      ; reload insn (was masked above)
///   +0x20: e20c_cc0f  and r12, r12, #0xF00    ; isolate cp_num bits[11:8]
///   +0x24: e35c_0c01  cmp r12, #0x100         ; cp_num == 1?
///   +0x28: 135c_0c02  cmpne r12, #0x200       ; cp_num == 2?
///   +0x2C: 1a00_xxxx  bne UND_TRAMP_OFFSET    ; cp_num not 1 or 2
///   +0x30: ee1d_cf50  mrc p15,0,r12,c13,c0,2  ; restore R12 (FPA path)
///   +0x34: ea00_xxxx  b FPE_JT (= 0x38d874)
///   +0x38: ee1d_cf50  mrc p15,0,r12,c13,c0,2  ; restore R12 (non-FPA path)
///   +0x3C: ea00_xxxx  b UND_TRAMP_OFFSET
///
/// The two `bne UND_TRAMP_OFFSET` early-outs at +0x18 and +0x2C share
/// the not-FPA exit at +0x38; consolidating saves 8 bytes vs branching
/// to a per-failure restore-and-jump pair.
///
/// Why route through 0x38d874 (the JT slot) rather than 0x38d8dc
/// (FP_UndefHandlers_Start directly): preserves the post-ship-patch
/// indirection — if the kernel later patches the FPE entry to a REx
/// override, the JT slot picks up the override automatically.
///
/// The stub uses TPIDRURW as the R12 save slot (matching what the
/// existing trampoline does), so the kernel's FPE sees R12 with its
/// original USR value when it executes its own `mcr p15,0,r12,...`-
/// less prologue. SP_und / LR_und / SPSR_und are untouched — exactly
/// the architectural state SA-110 hardware delivered to the FPE.
pub const FPA_BYPASS_STUB_OFFSET: usize = rom_ver::ROM_TAIL.fpa_bypass_stub as usize;

/// DABT-vector trampoline body. Installed at ROM offset 0x00FF_FFA8.
/// Saves LR_abt/SP_abt/SPSR_abt natively from
/// ABT mode, then bounces to SVC to save SP_svc/SPSR_svc/LR_svc.
///
/// (Per Table D1-79, AArch32 R13_svc / R14_svc / SPSR_svc are reachable
/// from AArch64 EL2 as `ctx.x[19]` / `ctx.x[18]` / `spsr_el1`
/// respectively, so the SVC bounce is not strictly necessary. It is
/// retained because the alignment-fault fast path's HVC-entry handler
/// reads from `DABT_SAVE_PA` directly; refactoring that to use ctx.x[]
/// is a follow-up.)
///
/// The literal at the end of the trampoline is swapped between
/// pre/post-MMU VAs by `install_und_vector_swap_{pre,post}_mmu`.
///
/// Save layout at DABT_SAVE_PA:
///   +0x00: LR_abt
///   +0x04: SP_abt
///   +0x08: SPSR_abt (= pre-abort CPSR)
///   +0x0C: SP_svc
///   +0x10: SPSR_svc
///   +0x14: LR_svc
pub const DABT_TRAMP_OFFSET: usize = rom_ver::ROM_TAIL.dabt_tramp as usize;

/// iter-59: AArch32-side fast-forward DABT trampoline. Installed in
/// the head of the stub pool (which has plenty of free space).
///
/// Routes by DFSC straight from AArch32 ABT mode without an EL2 round
/// trip in the common kernel-handled cases:
///
///   DFSC == 0x07 (translation, page)     → branch kernel DAH
///   DFSC == 0x0F (permission, page)      → branch kernel DAH
///   DFSC == 0x0D (permission, section)   → branch kernel DAH
///   DFSC == 0x06 (access flag, page)     → branch kernel DAH
///   DFSC == 0x03 (access flag, section)  → branch kernel DAH
///   anything else                        → fall through to DABT_TRAMP_OFFSET
///                                          (slow EL2 path: DFSC=0x05
///                                          translation-section needs
///                                          DFSR.Domain synthesis from
///                                          L1[FAR>>20][8:5] — see
///                                          install_dabt_fast_trampoline
///                                          docs; alignment, domain,
///                                          external, recursive aborts
///                                          all also fall through)
///
/// VA 0x10 branches here instead of directly at DABT_TRAMP_OFFSET so
/// the fast path doesn't pay the DABT_TRAMP's lr/sp/spsr saves either.
/// The slow-fall-through invokes the existing DABT_TRAMP_OFFSET (which
/// does the saves and HVCs into EL2 for the rare cases).
///
/// Located in the unused tail between Einstein.rex (ends ~0x847000)
/// and the tracer trampoline pool (starts at 0x900000). 64 words
/// reserved; the trampoline body uses ~16.
pub const DABT_FAST_TRAMP_OFFSET: usize = rom_ver::ROM_TAIL.dabt_fast_tramp as usize;
/// Save area for the DABT trampoline, at `HYP_TRAMP_SCRATCH_BASE + 0xA0`
/// = IPA 0x0600_00A0 (the first page of the SCRATCH_POOL). Identity-
/// mapped, so the same address works pre-MMU and post-MMU and no
/// literal swap is required. See `trap::HYP_TRAMP_SCRATCH_BASE`.
pub const DABT_SAVE_PA: u32 = crate::hv::trap::HYP_TRAMP_SCRATCH_BASE + 0xA0;

// ---------------------------------------------------------------------
// UND-path guest resume (through the UND-return stub above)
// ---------------------------------------------------------------------

/// UND-path return. Must NOT use `return_to_guest` — that calls
/// `msr spsr_el2, <val>`, which on QEMU raspi3b has a documented side
/// effect: it clobbers SPSR_EL1 (= AArch32 SPSR_svc) with the value
/// being written. Since the UND trampoline HVCs from UND mode, `<val>`
/// is the pre-UND CPSR (e.g. 0x1D3 for SVC mode); that pollutes the
/// guest's live SPSR_svc from USR → SVC, and the kernel's subsequent
/// `movs pc, lr` at SWIBoot's epilog stays in SVC instead of returning
/// to USR. Stalls Phase B at DFAR=0x0c001000 in SVC on `pop {r4, r5}`
/// at PC 0x3ae3ec.
///
/// Workaround (suggested by the verification agent on 2026-04-23):
/// don't write SPSR_EL2 at all. Instead, ERET into a `ldr lr, [pc,
/// #0]; movs pc, lr` stub at `UND_RETURN_STUB_VA`. SPSR_EL2 stays as
/// the CPU's auto-saved value from HVC entry (= UND, mode 0x1B), so
/// the ERET lands in UND mode. The stub loads the target PC from a
/// post-LDR literal we write to the ROM backing, then `movs pc, lr`
/// architecturally — the CPU copies SPSR_und (still the pre-UND
/// CPSR, preserved since UND entry) into CPSR, and R14_und into PC.
/// No `msr spsr_el2`, no SPSR_EL1 side-effect.
pub fn return_to_guest_from_und(_ctx: &mut crate::arch::trap_context::TrapContext, elr: u64, _spsr: u64) {
    // iter-87 diag: catch the case where we're about to ERET to USR
    // mode at a PC inside our own trampoline window. That's never
    // legitimate; the only trampoline-internal ERET target is the
    // UND_RETURN_STUB which lives outside this range.
    // iter-87 diag: only flag ERET to the trampoline body proper —
    // ranges 0xffff00..0xffff60 (UND_TRAMP) and 0xffec0..0xffefc
    // (FPA bypass). UND_RETURN_STUB at 0xffffe4 is a legitimate
    // ERET target.
    let mode = (_spsr as u32) & 0x1F;
    let elr32 = elr as u32;
    let in_und_tramp = elr32 >= UND_TRAMP_OFFSET as u32
        && elr32 < (UND_TRAMP_OFFSET as u32 + 0x60);
    let in_fpa_bypass = elr32 >= FPA_BYPASS_STUB_OFFSET as u32
        && elr32 < (FPA_BYPASS_STUB_OFFSET as u32 + 0x40);
    if mode == 0x10 && (in_und_tramp || in_fpa_bypass) {
        kprintln!(
            "*** return_to_guest_from_und: USR target inside trampoline body! \
             elr={:#x} spsr={:#x} — about to wedge",
            elr, _spsr,
        );
        crate::hv::trap::und::dump_und_history();
        kprintln!(
            "  elr_el2={:#x} caller-trace below; halting before ERET",
            crate::arch::trap_context::read_sysreg!("elr_el2"),
        );
        crate::arch::cpu::halt();
    }
    // Write target PC to the stub's literal slot, then ERET into the
    // stub in UND mode (by leaving SPSR_EL2 alone). The stub does
    // `ldr lr, [pc, #0]; movs pc, lr` — CPU restores CPSR from SPSR_und
    // (preserved since UND entry) and PC from the literal.
    //
    // Using a literal in the stub (rather than staging the return PC
    // into LR_und = ctx.x[22] per Table D1-79) is the simpler and
    // platform-portable choice: `ic ivau` on the literal address is
    // a single barrier-coupled instruction, whereas the X22 path
    // would require relying on AArch64-ERET-to-AArch32 to faithfully
    // route x[22] into R14_und across both QEMU raspi3b and FVP, and
    // the ROM-backing flush is needed regardless.
    let literal_host =
        rom_host_pa() as usize + UND_RETURN_STUB_LITERAL_OFFSET;
    // The UND_RETURN_STUB does `ldr lr, [pc, #0]` to load this literal,
    // running under BE-8 with CPSR.E=1. Host bytes must be BE-encoded
    // so the guest's LDR returns `elr` numerically — write swap of elr.
    // Guest-test mode doesn't run BE-8; identity write.
    #[cfg(not(nh_guest_test))]
    let literal_value = (elr as u32).swap_bytes();
    #[cfg(nh_guest_test)]
    let literal_value = elr as u32;
    // SAFETY: bounded write in ROM backing; EL2-owned. Flush via D-cache
    // clean + I-cache invalidate so the guest fetch path sees the new
    // literal.
    unsafe {
        core::ptr::write_volatile(literal_host as *mut u32, literal_value);
        core::arch::asm!(
            "dc cvau, {0}",
            "dsb ish",
            "ic ivau, {0}",
            "dsb ish",
            "isb",
            in(reg) literal_host as u64,
            options(nostack, preserves_flags),
        );
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "isb",
            elr = in(reg) UND_RETURN_STUB_VA as u64,
            options(nostack, preserves_flags),
        );
    }
}


/// Register the regions the hypervisor populates at runtime with native
/// (little-endian) AArch32 instruction words — rather than
/// guest-authored data — with the layout manifest's hyp-code-range
/// table (`layout::register_hyp_code_range`). Called once from
/// `main.rs` before the ROM is loaded; `layout::is_hyp_code` is the
/// query shared by `guest_endian::pa_is_rom_code` (don't byte-swap
/// these words) and the snapshot autosave gate (a guest PC parked here
/// is mid-trampoline).
///
/// Covers every runtime-written code region: the DABT fast trampoline,
/// the tracer trampoline pool, and the contiguous patch-stub arena / FPA
/// bypass stub / UND-return stub / UND trampoline tail. None of these
/// regions hold guest data the hypervisor reads back through
/// `guest_endian`, so the byte-order predicate and the autosave-gating
/// predicate want exactly the same set.
pub fn register_hyp_code_ranges() {
    // Tracer trampoline pool. Registered from the `rom_ver::ROM_TAIL`
    // fields (not via `crate::diag::tracer`) because the `tracer`
    // module is `#[cfg(feature = "trace")]`-only, while these ranges
    // must be registered in every build. The pool is empty ROM tail when
    // the feature is off, so registering the range unconditionally is
    // harmless.
    let tail = rom_ver::ROM_TAIL;

    // DABT fast trampoline (between Einstein.rex tail and tracer pool).
    crate::hv::layout::register_hyp_code_range(
        "DABT fast trampoline",
        tail.dabt_fast_tramp,
        tail.tracer_pool_base,
    );
    crate::hv::layout::register_hyp_code_range(
        "tracer trampoline pool",
        tail.tracer_pool_base,
        tail.tracer_pool_end,
    );
    // Patch-stub arena → FPA bypass stub → UND trampoline → UND-return
    // stub, all contiguous in the ROM aperture tail.
    crate::hv::layout::register_hyp_code_range(
        "patch-stub arena + ROM-tail stubs",
        tail.patch_stub_arena_base,
        tail.stubs_end,
    );
}

/// Install the DABT-vector intercept stub at `DABT_TRAMP_OFFSET` and
/// patch VA 0x10 to branch to it. Serves two roles:
///   (1) `HVC #DIAG_TAG` for Phase-B debugging: halt with banked-reg
///       dump on any unexpected DABT the kernel doesn't own.
///   (2) `HVC #ALIGN_TAG` for hypervisor-wide rotate-LDR emulation.
///       SCTLR.A=1 (forced by our CP15 shim) means every unaligned
///       LDR/STR alignment-faults here; the handler decodes+emulates
///       SA-1100 rotate-LDR semantics and ERETs past the faulting insn.
///
/// Stub layout (15 words = 60 bytes). Saves R0 / R1 to TPIDR scratch
/// regs and LR_abt / SP_abt / SPSR_abt to a fixed RAM slot *before*
/// the DFSR check, because the alignment-fault fast path needs the
/// pre-abt mode bits and faulting PC available to AArch64 EL2 from
/// guaranteed-stable storage rather than from `mrs spsr_abt` (which
/// is fine on FVP but historically unreliable on QEMU raspi3b — see
/// Bug #1 in docs/QEMU_BUGS.md). LR_abt / SP_abt themselves are
/// also available in `ctx.x[20]` / `ctx.x[21]` per Table D1-79;
/// keeping the RAM stash simplifies the trampoline → fast-path
/// handoff (the trampoline writes them anyway as part of its
/// ABT-mode-native register save).
///
///   +0x00: ee0d_0f50  mcr p15,0,r0,c13,c0,2  ; save r0 → TPIDRURW
///   +0x04: ee0d_1f70  mcr p15,0,r1,c13,c0,3  ; save r1 → TPIDRRO
///   +0x08: e59f_0028  ldr r0, [pc, #0x28]    ; r0 = DABT_SAVE_VA literal
///   +0x0C: e580_e000  str lr, [r0]           ; save LR_abt  @ +0x00
///   +0x10: e580_d004  str sp, [r0, #4]       ; save SP_abt  @ +0x04
///   +0x14: e14f_1000  mrs r1, spsr           ; r1 = SPSR_abt
///   +0x18: e580_1008  str r1, [r0, #8]       ; save SPSR_abt @ +0x08
///   +0x1C: ee15_0f10  mrc p15,0,r0,c5,c0,0   ; r0 = DFSR
///   +0x20: e200_000f  and r0, r0, #0xF       ; mask FS[3:0]
///   +0x24: e350_0001  cmp r0, #1             ; alignment fault?
///   +0x28: 0a00_0000  beq align_path (+0x30) ; → HVC #ALIGN_TAG
///   +0x2C: e140_0171  hvc #0x11 (DIAG_TAG)
///   +0x30: e140_0173  hvc #0x13 (ALIGN_TAG) — align path target
///   +0x34: eaff_fffe  b .                    ; guard
///   +0x38: literal     DABT_SAVE_VA
///
/// DABT_SAVE layout at IPA 0x0400_5FA0 (pre-MMU) / VA 0x0C00_4FA0
/// (post-MMU):
///   +0x00: LR_abt    (= faulting_pc + 8 for ARM DABT)
///   +0x04: SP_abt
///   +0x08: SPSR_abt  (= pre-abt CPSR)
///
/// The pre/post-MMU literal swap is piggy-backed on the UND
/// trampoline's swap in `install_und_vector_swap_{pre,post}_mmu`.
///
/// SAFETY: writes 1 word at VA 0x10 + 15 words in the ROM tail
/// reserved region; caller must own the ROM backing.
pub unsafe fn patch_dabt_vector(rom_ptr: *mut u32) {
    unsafe {
        if let Some(dah_va) = rom_ver::DATA_ABORT_HANDLER_VA {
            // VA 0x10 → branch to the iter-59 fast trampoline (which
            // dispatches by DFSC; common cases jump straight to kernel
            // DAH; uncommon cases fall through to the slow DABT_TRAMP).
            let imm24 = ((DABT_FAST_TRAMP_OFFSET as u32).wrapping_sub(0x10 + 8) / 4) & 0x00FF_FFFF;
            let branch_insn = 0xEA00_0000 | imm24;
            write_rom_code_word(rom_ptr, 4, branch_insn);    // 0x10: b DABT_FAST_TRAMP_OFFSET

            install_dabt_fast_trampoline(rom_ptr, dah_va);
        } else {
            // The kernel's DataAbortHandler VA is unknown for this ROM
            // version — no fast-forward path. VA 0x10 branches straight
            // at the slow trampoline: every DABT takes the EL2 HVC path
            // (`GuestOs::handle_dabt_dispatch`), whose forwardable arms
            // halt loudly in turn.
            let imm24 = ((DABT_TRAMP_OFFSET as u32).wrapping_sub(0x10 + 8) / 4) & 0x00FF_FFFF;
            write_rom_code_word(rom_ptr, 4, 0xEA00_0000 | imm24);
        }

        let db = DABT_TRAMP_OFFSET / 4;
        write_rom_code_word(rom_ptr, db +  0, 0xEE0D_0F50); // mcr p15,0,r0,c13,c0,2
        write_rom_code_word(rom_ptr, db +  1, 0xEE0D_1F70); // mcr p15,0,r1,c13,c0,3
        write_rom_code_word(rom_ptr, db +  2, 0xE59F_0028); // ldr r0, [pc, #0x28] → DABT_SAVE_VA
        write_rom_code_word(rom_ptr, db +  3, 0xE580_E000); // str lr, [r0]           LR_abt
        write_rom_code_word(rom_ptr, db +  4, 0xE580_D004); // str sp, [r0, #4]       SP_abt
        write_rom_code_word(rom_ptr, db +  5, 0xE14F_1000); // mrs r1, spsr
        write_rom_code_word(rom_ptr, db +  6, 0xE580_1008); // str r1, [r0, #8]       SPSR_abt
        write_rom_code_word(rom_ptr, db +  7, 0xEE15_0F10); // mrc p15,0,r0,c5,c0,0   DFSR
        write_rom_code_word(rom_ptr, db +  8, 0xE200_000F); // and r0, r0, #0xF
        write_rom_code_word(rom_ptr, db +  9, 0xE350_0001); // cmp r0, #1
        write_rom_code_word(rom_ptr, db + 10, 0x0A00_0000); // beq +0x0 (word 12 = ALIGN hvc)
        write_rom_code_word(rom_ptr, db + 11, HvcImm::DabtDispatch.insn()); // DABT-trampoline fall-through
        write_rom_code_word(rom_ptr, db + 12, HvcImm::Align.insn()); // hvc #0x13 (ALIGN_TAG)
        write_rom_code_word(rom_ptr, db + 13, 0xEAFF_FFFE); // b . (guard)
        // Literal slot — read by the LDR at db+2 under BE-8 (CPSR.E=1),
        // so write as data (BE-encoded host bytes).
        write_rom_data_word(rom_ptr, db + 14, DABT_SAVE_PA);
    }
}

/// Install the iter-59 fast-forward DABT trampoline at
/// `DABT_FAST_TRAMP_OFFSET` (in the unused tail between Einstein.rex
/// and the tracer pool). VA 0x10's branch (set by `patch_dabt_vector`)
/// targets here. Layout:
///
///   ft+0:  mcr p15,0,r0,c13,c0,2     ; TPIDRURW = R0 (save)
///   ft+1:  mcr p15,0,r1,c13,c0,3     ; TPIDRRO = R1 (save)
///   ; iter-105 (FVP fix #1): mirror the slow trampoline's save of
///   ; LR_abt / SP_abt / SPSR_abt into the DABT_SAVE_PA scratch slots.
///   ; The DAH-MRS-SPSR HVC patch reads SPSR_abt from the slot, so
///   ; if only fast-path DABTs fire (the common case on FVP, where
///   ; DFSC=5 section faults are rare during early boot) the slot
///   ; stays at 0 and the kernel's `mrs r1, SPSR` substitution gets
///   ; bogus data.
///   ft+2:  ldr r0, [pc, #L_SAVE_PA]  ; r0 = DABT_SAVE_PA literal
///   ft+3:  str lr, [r0]              ; LR_abt
///   ft+4:  str sp, [r0, #4]          ; SP_abt
///   ft+5:  mrs r1, spsr              ; r1 = SPSR_abt
///   ft+6:  str r1, [r0, #8]          ; SPSR_abt
///   ; iter-105 (FVP fix #2): c7 cache-maintenance MCR filter. The
///   ; kernel's `CleanPageInDcache` etc. issue DCCMVAC/DCIMVAC/...
///   ; with VAs that may be unmapped at the time. Cortex-A53 silently
///   ; no-ops these (per its TRM); FVP_Base_RevC's strict AEM raises
///   ; a translation fault. Forwarding the fault to DAH is fatal —
///   ; DAH treats any SVC-mode DABT as "deep toast". Filter here:
///   ; if the faulting instruction is `MCR p15, 0, Rt, c7, CRm, opc2`,
///   ; just advance past it and return to the pre-abt context. The
///   ; cache invalidation itself is a no-op on a coherent A53/AEM,
///   ; matching Einstein's `TARMProcessor` case-7 silent-no-op.
///   ;
///   ; The faulting word is read in BE-8 (CPSR.E=1) and so returns
///   ; the byteswap of the kernel's compiled-for encoding — REV
///   ; brings it back to the kernel-format word, which we mask
///   ; against the c7-MCR pattern.
///   ;
///   ; Pattern (any cond, opc1=0, MCR, CRn=7, p15, RES1 bit 4):
///   ;   bits[27:16] == 0xE07
///   ;   bits[11:8]  == 0xF
///   ;   bit[4]      == 1
///   ;   mask 0x0FFF_0F10, expected 0x0E07_0F10
///   ft+7:  ldr r1, [lr, #-8]         ; r1 = faulting instr (BE-8 view)
///   ft+8:  rev r1, r1                ; r1 = kernel-format word
///   ft+9:  ldr r0, [pc, #L_C7_MASK]  ; r0 = 0x0FFF_0F10
///   ft+10: and r1, r1, r0            ; r1 &= mask
///   ft+11: ldr r0, [pc, #L_C7_PATT]  ; r0 = 0x0E07_0F10
///   ft+12: teq r1, r0
///   ft+13: beq C7_NOOP               ; → ft+35
///   ; existing DFSC dispatch
///   ft+14: mrc p15,0,r0,c5,c0,0      ; R0 = DFSR
///   ft+15: and r0, r0, #0xF          ; R0 = DFSC[3:0]
///   ft+16: cmp r0, #7                ; translation, page (most common)
///   ft+17: beq FAST_FWD              ; → ft+30
///   ft+18: cmp r0, #15               ; permission, page
///   ft+19: beq FAST_FWD
///   ft+20: cmp r0, #9                ; domain, first level / section
///   ft+21: beq FAST_FWD
///   ft+22: cmp r0, #13               ; permission, section
///   ft+23: beq FAST_FWD
///   ft+24: cmp r0, #6                ; access flag, page
///   ft+25: beq FAST_FWD
///   ft+26: cmp r0, #3                ; access flag, section
///   ft+27: beq FAST_FWD
///   ; Slow-path fall-through: both R0 and R1 were clobbered (R0 by
///   ; the DABT_SAVE_PA load + DFSR read, R1 by the SPSR_abt save and
///   ; the c7-MCR check). Restore both before branching so the slow
///   ; trampoline's own `mcr ...,c13,c0,2/3` sees the original
///   ; pre-abt values when it re-saves them.
///   ft+28: mrc p15,0,r0,c13,c0,2     ; restore R0 from TPIDRURW
///   ft+29: mrc p15,0,r1,c13,c0,3     ; restore R1 from TPIDRRO
///   ft+30: b SLOW_DABT_TRAMP         ; → DABT_TRAMP_OFFSET (slow EL2 path)
///   ; FAST_FWD:
///   ft+31: mrc p15,0,r0,c13,c0,2     ; restore R0 from TPIDRURW
///   ft+32: mrc p15,0,r1,c13,c0,3     ; restore R1 from TPIDRRO
///   ft+33: ldr pc, [pc, #-4]         ; pc+8-4 = ft+33+4 → literal at ft+34
///   ft+34: literal: DAH VA           ; rom_ver::DATA_ABORT_HANDLER_VA
///   ; C7_NOOP:
///   ft+35: mrc p15,0,r0,c13,c0,2     ; restore R0
///   ft+36: mrc p15,0,r1,c13,c0,3     ; restore R1
///   ft+37: subs pc, lr, #4           ; ERET to faulting_PC + 4
///                                    ;   (LR_abt = faulting_PC + 8)
///   ; literals
///   ft+38: literal: DABT_SAVE_PA     ; for `ldr r0` at ft+2
///   ft+39: literal: 0x0FFF_0F10      ; for `ldr r0` at ft+9
///   ft+40: literal: 0x0E07_0F10      ; for `ldr r0` at ft+11
///
/// 41 words × 4 = 164 bytes; reserved region is 256 bytes so 92
/// bytes of slack remain.
///
/// Cost reduction: taking an EL2 round-trip on every DABT is
/// measurably expensive — 20.8 M HVC #DIAG_TAG hits in ~30 s of wall
/// (DFSCs 0x07/0x0F dominating, all forwarded to kernel DAH).
/// Bypassing the EL2 round-trip for those cases is a direct win —
/// same kernel-side execution, no hypervisor overhead. The
/// LR/SP/SPSR save adds ~5 instructions to the fast path, negligible
/// relative to the EL2 round-trip it avoids.
///
/// DFSC=0x05 (translation, section) is *deliberately excluded* from
/// the fast path. For section-level translation faults ARMv7 leaves
/// DFSR.Domain UNK (= 0); the kernel's
/// `GetDomainAndFaultMonitorFromDomainNumber(0)` then returns no
/// monitor and DAH throws `evt.ex.abt.bus`. Only the slow EL2 path
/// (`handle_dabt_dispatch`) synthesises DFSR.Domain from
/// L1[FAR>>20][8:5] before forwarding to DAH, so DFSC=5 has to fall
/// through to it.
/// Section-level faults fire only on first touch of a 1 MiB section
/// (~tens of times per boot for freshly-allocated stacks), so the
/// slow-path cost is negligible.
///
/// SAFETY: writes 41 words in the reserved range
/// `DABT_FAST_TRAMP_OFFSET .. + 41*4`. Caller owns the ROM backing.
pub unsafe fn install_dabt_fast_trampoline(rom_ptr: *mut u32, dah_va: u32) {
    unsafe {
        let ft = DABT_FAST_TRAMP_OFFSET / 4;

        // `from`/`to` are word slot indices within this trampoline.
        // The absolute PC of slot N is `DABT_FAST_TRAMP_OFFSET + N*4`.
        let pc_of = |slot: usize| (DABT_FAST_TRAMP_OFFSET as u32) + (slot as u32) * 4;
        // `beq` from slot `from` to slot `to`.
        let beq = |from: usize, to: usize| -> u32 {
            crate::arch::aarch32_emit::b_cond(pc_of(from), pc_of(to), crate::arch::aarch32_emit::COND_EQ)
        };
        // Unconditional `b` to a far target (`DABT_TRAMP_OFFSET`); args
        // are absolute byte offsets.
        let b_far = |from_byte_offset: u32, to_byte_offset: u32| -> u32 {
            crate::arch::aarch32_emit::b(from_byte_offset, to_byte_offset)
        };
        // `ldr r0, [pc, #imm12]` literal-load. `from`/`to` are word slot
        // indices; the literal lives at slot `to`.
        let ldr_r0_lit =
            |from: usize, to: usize| -> u32 { crate::arch::aarch32_emit::ldr_rd_lit(pc_of(from), 0, pc_of(to)) };

        // Slot writes (instructions, native-LE u32 = LE encoding for
        // BE-8 instruction fetch):
        write_rom_code_word(rom_ptr, ft +  0, 0xEE0D_0F50); // mcr p15,0,r0,c13,c0,2
        write_rom_code_word(rom_ptr, ft +  1, 0xEE0D_1F70); // mcr p15,0,r1,c13,c0,3
        // iter-105: save LR_abt / SP_abt / SPSR_abt to DABT_SAVE_PA so
        // the DAH-MRS-SPSR HVC handler reads a current value, not a
        // stale-or-zero leftover from a long-ago slow-path DABT.
        write_rom_code_word(rom_ptr, ft +  2, ldr_r0_lit(2, 38));  // ldr r0, [pc, #..] = DABT_SAVE_PA
        write_rom_code_word(rom_ptr, ft +  3, 0xE580_E000); // str lr, [r0]
        write_rom_code_word(rom_ptr, ft +  4, 0xE580_D004); // str sp, [r0, #4]
        write_rom_code_word(rom_ptr, ft +  5, 0xE14F_1000); // mrs r1, spsr
        write_rom_code_word(rom_ptr, ft +  6, 0xE580_1008); // str r1, [r0, #8]
        // iter-105: c7 cache-maintenance MCR filter — see header.
        write_rom_code_word(rom_ptr, ft +  7, 0xE51E_1008); // ldr r1, [lr, #-8]
        write_rom_code_word(rom_ptr, ft +  8, 0xE6BF_1F31); // rev r1, r1
        write_rom_code_word(rom_ptr, ft +  9, ldr_r0_lit(9, 39));  // ldr r0, [pc, #..] = mask
        write_rom_code_word(rom_ptr, ft + 10, 0xE001_1000); // and r1, r1, r0
        write_rom_code_word(rom_ptr, ft + 11, ldr_r0_lit(11, 40)); // ldr r0, [pc, #..] = pattern
        write_rom_code_word(rom_ptr, ft + 12, 0xE131_0000); // teq r1, r0
        write_rom_code_word(rom_ptr, ft + 13, beq(13, 35)); // beq C7_NOOP
        // existing DFSC dispatch
        write_rom_code_word(rom_ptr, ft + 14, 0xEE15_0F10); // mrc p15,0,r0,c5,c0,0 (DFSR)
        write_rom_code_word(rom_ptr, ft + 15, 0xE200_000F); // and r0, r0, #0xF
        write_rom_code_word(rom_ptr, ft + 16, 0xE350_0007); // cmp r0, #7
        write_rom_code_word(rom_ptr, ft + 17, beq(17, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 18, 0xE350_000F); // cmp r0, #15
        write_rom_code_word(rom_ptr, ft + 19, beq(19, 31)); // beq FAST_FWD
        // DFSC=0x09 (Domain fault, first level / section). The Newton
        // kernel raises this deliberately at TCardSocket::GetChipInfo
        // (PC ~0x55714+): the PCMCIA probe maps the candidate window
        // VA via a section descriptor with domain=15 ("No access" in
        // DACR=0x00055555), then accesses it inside a setjmp +
        // AddExceptionHandler frame. The kernel's own DataAbortHandler
        // must reach the user exception handler to longjmp past the
        // probe — forward this DFSC to the kernel rather than DIAG'ing.
        // Verified against ARM ARM B4.1.52 + Table B3-23 (short-
        // descriptor FSR encoding: 0b01001 = first-level domain fault).
        // (DFSC=0x05 is deliberately excluded — see the doc comment
        // above for the rationale.)
        write_rom_code_word(rom_ptr, ft + 20, 0xE350_0009); // cmp r0, #9
        write_rom_code_word(rom_ptr, ft + 21, beq(21, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 22, 0xE350_000D); // cmp r0, #13
        write_rom_code_word(rom_ptr, ft + 23, beq(23, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 24, 0xE350_0006); // cmp r0, #6
        write_rom_code_word(rom_ptr, ft + 25, beq(25, 31)); // beq FAST_FWD
        write_rom_code_word(rom_ptr, ft + 26, 0xE350_0003); // cmp r0, #3
        write_rom_code_word(rom_ptr, ft + 27, beq(27, 31)); // beq FAST_FWD
        // Slow-path fall-through: BOTH R0 and R1 were clobbered (R0
        // by the SAVE_PA literal load + DFSR read, R1 by SPSR_abt and
        // the c7-MCR check). Restore both so the slow DABT_TRAMP's
        // `mcr p15,0,r0/r1,c13,c0,{2,3}` sees the original pre-abt
        // values when it re-saves them to TPIDR.
        write_rom_code_word(rom_ptr, ft + 28, 0xEE1D_0F50); // mrc p15,0,r0,c13,c0,2 (restore R0)
        write_rom_code_word(rom_ptr, ft + 29, 0xEE1D_1F70); // mrc p15,0,r1,c13,c0,3 (restore R1)
        write_rom_code_word(rom_ptr, ft + 30, b_far(
            (DABT_FAST_TRAMP_OFFSET as u32).wrapping_add(30 * 4),
            DABT_TRAMP_OFFSET as u32,
        ));                                                  // b SLOW_DABT_TRAMP
        // FAST_FWD:
        write_rom_code_word(rom_ptr, ft + 31, 0xEE1D_0F50); // mrc p15,0,r0,c13,c0,2 (restore R0)
        write_rom_code_word(rom_ptr, ft + 32, 0xEE1D_1F70); // mrc p15,0,r1,c13,c0,3 (restore R1)
        write_rom_code_word(rom_ptr, ft + 33, 0xE51F_F004); // ldr pc, [pc, #-4]
        write_rom_data_word(rom_ptr, ft + 34, dah_va);
        // C7_NOOP: cache-maintenance MCR is a no-op on a coherent
        // host. Restore the registers we stashed and ERET to
        // faulting_PC + 4 in the pre-abt mode (LR_abt = pc + 8 at
        // entry, so subs lr - 4 = faulting_PC + 4).
        write_rom_code_word(rom_ptr, ft + 35, 0xEE1D_0F50); // mrc p15,0,r0,c13,c0,2 (restore R0)
        write_rom_code_word(rom_ptr, ft + 36, 0xEE1D_1F70); // mrc p15,0,r1,c13,c0,3 (restore R1)
        write_rom_code_word(rom_ptr, ft + 37, 0xE25E_F004); // subs pc, lr, #4
        // Literal slots — all loaded via BE-8 LDR.
        write_rom_data_word(rom_ptr, ft + 38, DABT_SAVE_PA);
        write_rom_data_word(rom_ptr, ft + 39, 0x0FFF_0F10);
        write_rom_data_word(rom_ptr, ft + 40, 0x0E07_0F10);
    }
}

/// `movs pc, lr` stub in the ROM trampoline region. See the installation
/// site in `patch_und_vector` and `return_to_guest_from_und` in `hv::trap::und`
/// for rationale. Must not overlap the DABT trampoline, which spans
/// `DABT_TRAMP_OFFSET .. DABT_TRAMP_OFFSET + 15*4`  (= 0x00FF_FFA8 ..
/// 0x00FF_FFE4 inclusive of the literal word at `db+14`). Placing the
/// stub at the first aligned word past that literal keeps both
/// trampolines non-overlapping; the stub is 3 words (12 bytes) so it
/// ends at 0x00FF_FFF0, still inside ROM.
///
/// The overlap matters even when it looks harmless: at 0x00FF_FFE0 the
/// stub coincides byte-for-byte with the DABT-trampoline's literal
/// slot, and the clobbered first word (0x0400_5FA0 / 0x0C00_4FA0,
/// written by `install_und_vector_swap_*`) decodes as an
/// EQ-conditional LDC. QEMU raspi3b's TCG model treats that as a NOP;
/// FVP Base RevC raises an UNDEFINED exception, so the UND return path
/// halts with an "unrecognised UND" in early boot.
pub const UND_RETURN_STUB_OFFSET: usize = rom_ver::ROM_TAIL.und_return_stub as usize;
pub const UND_RETURN_STUB_VA: u32 = UND_RETURN_STUB_OFFSET as u32;
/// Offset of the target-PC literal inside the stub (written by Rust
/// handler before ERET).
pub const UND_RETURN_STUB_LITERAL_OFFSET: usize = UND_RETURN_STUB_OFFSET + 8;

pub unsafe fn patch_und_vector(rom: *mut u32) {
    // The trampoline's save-slot address is held in the literal at
    // offset 0x30. Pre-MMU we use the RAM *IPA* 0x0400_5F00 directly
    // (since VA == IPA with the MMU off, and 0x0400_5F00 is inside
    // our stage-2 RAM mapping). Once the guest enables its stage-1
    // MMU, VA 0x0400_xxxx aliases ROM (read-only) under the kernel's
    // L1[0x40] section, so `install_und_vector_swap_post_mmu()` swaps
    // the literal to the VA 0x0C00_4F00, which the kernel's
    // L1[0xC0] coarse → L2[0x04] small page maps back to RAM.

    // UND vector at IPA 0x04 → branch to FPA bypass stub. The stub
    // routes FPA-class UNDs straight to the kernel's FPE handler
    // (matching SA-110 hardware behaviour) and falls through to the
    // existing trampoline for everything else.
    //
    // ARM B (immediate): cond=AL=0xE, opcode=1010, imm24 = (target -
    // (PC+8)) / 4. PC at IPA 0x04 = 0x04, PC+8 = 0x0C.
    let imm24_to_bypass =
        ((FPA_BYPASS_STUB_OFFSET as u32 - 0x0C) / 4) & 0x00FF_FFFF;
    let branch_to_bypass = 0xEA00_0000 | imm24_to_bypass;

    // SAFETY: offsets below all sit inside the reserved ROM-tail
    // window (`rom_ver::ROM_TAIL`), well under ROM_SIZE (= 16 MiB)
    // and inside the range checked by `tracer::in_reserved_range`.
    unsafe {
        if let Some(fpe_jt) = rom_ver::FPE_JT_VA {
            write_rom_code_word(rom, 1, branch_to_bypass);   // 0x04: b FPA_BYPASS_STUB_OFFSET

            // FPA-class UND bypass stub. See `FPA_BYPASS_STUB_OFFSET`
            // doc comment for the per-word commentary; reproduced here
            // alongside the encodings.
            //
            // Two-stage check: bits[27:24] in {0xC, 0xD, 0xE} (LDC/STC/CDP/MCR),
            // *then* bits[11:8] in {1, 2} (FPA cp_num). The first stage rules
            // out UDF (bits[27:24]=0x7), software breakpoints and tracer
            // UDFs (also bits[27:24]=0x7), and other non-coprocessor
            // UND-causing insns. The second stage rules out
            // VFP/Advanced-SIMD (cp_num 10/11) — though those don't appear
            // in 717006 ROM, the check keeps the stub future-proof.
            let s = FPA_BYPASS_STUB_OFFSET / 4;
            write_rom_code_word(rom, s +  0, 0xEE0D_CF50);  // mcr p15,0,r12,c13,c0,2
            write_rom_code_word(rom, s +  1, 0xE51E_C004);  // ldr r12, [lr, #-4]
            write_rom_code_word(rom, s +  2, 0xE20C_C40F);  // and r12, r12, #0x0F000000
            write_rom_code_word(rom, s +  3, 0xE35C_040C);  // cmp r12, #0x0C000000
            write_rom_code_word(rom, s +  4, 0x135C_040D);  // cmpne r12, #0x0D000000
            write_rom_code_word(rom, s +  5, 0x135C_040E);  // cmpne r12, #0x0E000000
            write_rom_code_word(rom, s +  6, 0x1A00_0006);  // bne stub+0x38 (fall_through)
            write_rom_code_word(rom, s +  7, 0xE51E_C004);  // ldr r12, [lr, #-4]  (reload)
            write_rom_code_word(rom, s +  8, 0xE20C_CC0F);  // and r12, r12, #0xF00
            write_rom_code_word(rom, s +  9, 0xE35C_0C01);  // cmp r12, #0x100
            write_rom_code_word(rom, s + 10, 0x135C_0C02);  // cmpne r12, #0x200
            write_rom_code_word(rom, s + 11, 0x1A00_0001);  // bne stub+0x38 (fall_through)
            write_rom_code_word(rom, s + 12, 0xEE1D_CF50);  // mrc p15,0,r12,c13,c0,2  (FPA)
            // b FPE_JT (`rom_ver::FPE_JT_VA`). PC at this insn = stub+0x34.
            // PC+8 = stub+0x3C. imm24 = (target - PC+8) / 4.
            let pc_plus_8 = (FPA_BYPASS_STUB_OFFSET + 0x34 + 8) as i32;
            let target_fpe = fpe_jt as i32;
            let imm24_fpe =
                (((target_fpe - pc_plus_8) >> 2) as u32) & 0x00FF_FFFF;
            write_rom_code_word(rom, s + 13, 0xEA00_0000 | imm24_fpe); // b FPE_JT
            write_rom_code_word(rom, s + 14, 0xEE1D_CF50);  // mrc p15,0,r12,c13,c0,2  (fall_through)
            // b UND_TRAMP_OFFSET. PC+8 = stub+0x44. offset = -4 bytes
            // = -1 word; imm24 = 0xFFFFFF.
            let pc_plus_8b = (FPA_BYPASS_STUB_OFFSET + 0x3C + 8) as i32;
            let target_tramp = UND_TRAMP_OFFSET as i32;
            let imm24_tramp =
                (((target_tramp - pc_plus_8b) >> 2) as u32) & 0x00FF_FFFF;
            write_rom_code_word(rom, s + 15, 0xEA00_0000 | imm24_tramp); // b UND_TRAMP
        } else {
            // The kernel FPE entry is unknown for this ROM version —
            // skip the FPA bypass stub and point the UND vector
            // straight at the trampoline; FPA-class UNDs reach EL2 and
            // halt loudly there.
            let imm24_to_tramp =
                ((UND_TRAMP_OFFSET as u32 - 0x0C) / 4) & 0x00FF_FFFF;
            write_rom_code_word(rom, 1, 0xEA00_0000 | imm24_to_tramp);
        }

        let base = UND_TRAMP_OFFSET / 4;
        write_rom_code_word(rom, base +  0, 0xEE0D_CF50);  // mcr p15,0,r12,c13,c0,2
        write_rom_code_word(rom, base +  1, 0xE59F_C050);  // ldr r12, [pc, #0x50]
        write_rom_code_word(rom, base +  2, 0xE58C_000C);  // str r0, [r12, #0x0C]
        write_rom_code_word(rom, base +  3, 0xE58C_1010);  // str r1, [r12, #0x10]
        write_rom_code_word(rom, base +  4, 0xE58C_E000);  // str lr, [r12]
        write_rom_code_word(rom, base +  5, 0xE14F_0000);  // mrs r0, SPSR
        write_rom_code_word(rom, base +  6, 0xE58C_0004);  // str r0, [r12, #4]
        write_rom_code_word(rom, base +  7, 0xE58C_2014);  // str r2, [r12, #0x14]
        write_rom_code_word(rom, base +  8, 0xE200_101F);  // and r1, r0, #0x1F
        write_rom_code_word(rom, base +  9, 0xE381_10C0);  // orr r1, r1, #0xC0
        write_rom_code_word(rom, base + 10, 0xE351_00D0);  // cmp r1, #0xD0
        write_rom_code_word(rom, base + 11, 0x03A0_10DF);  // moveq r1, #0xDF
        write_rom_code_word(rom, base + 12, 0xE129_F001);  // msr cpsr_c, r1
        write_rom_code_word(rom, base + 13, 0xE58C_D018);  // str sp, [r12, #0x18]
        write_rom_code_word(rom, base + 14, 0xE58C_E01C);  // str lr, [r12, #0x1C]
        write_rom_code_word(rom, base + 15, 0xE321_F0DB);  // msr cpsr_c, #0xdb (UND)
        write_rom_code_word(rom, base + 16, 0xE59C_2014);  // ldr r2, [r12, #0x14]
        write_rom_code_word(rom, base + 17, 0xE321_F0D3);  // msr cpsr_c, #0xd3 (SVC)
        write_rom_code_word(rom, base + 18, 0xE1A0_000E);  // mov r0, lr
        write_rom_code_word(rom, base + 19, 0xE58C_0008);  // str r0, [r12, #8]
        write_rom_code_word(rom, base + 20, 0xE321_F0DB);  // msr cpsr_c, #0xdb (UND)
        write_rom_code_word(rom, base + 21, HvcImm::Und.insn());  // hvc #0x10
        write_rom_code_word(rom, base + 22, 0xEAFF_FFFE);  // b . (trap)
        // Literal slot — loaded by the `ldr r12, [pc, #0x50]` at base+1
        // under BE-8, so write as data.
        write_rom_data_word(rom, base + 23, crate::hv::trap::HYP_TRAMP_SCRATCH_BASE);

        // UND-return stub. See `return_to_guest_from_und` in
        // `hv::trap::und` for
        // why this exists — QEMU raspi3b's `msr spsr_el2, <val>` from
        // AArch64 EL2 clobbers SPSR_EL1 (= AArch32 SPSR_svc) as a side
        // effect. The UND-return path must avoid writing SPSR_EL2, so
        // we ERET into this stub while still in UND mode, then
        // architecturally restore CPSR via `movs pc, lr`.
        //
        // Layout: load target PC from a PC-relative literal (which the
        // Rust handler writes before each ERET), then `movs pc, lr`.
        // The literal route avoids relying on AArch64→AArch32 GPR
        // plumbing for the post-ERET R14: per Table D1-79, X14 maps to
        // R14_usr regardless of target mode (R14_und lives in X22), so
        // the obvious "stash return PC in ctx.x[14]" pattern would
        // overwrite R14_usr instead.
        //
        //   +0x00: e59fe000  ldr lr, [pc, #0]    ; lr = *(stub + 8)
        //   +0x04: e1b0f00e  movs pc, lr         ; CPSR = SPSR_und, PC = lr
        //   +0x08: <target PC literal, updated per ERET>
        //
        // The stub deliberately stops at three words. Extending it to
        // also reload banked SPSR_und from UND_SAVE_SPSR_IPA via `MSR
        // SPSR_cxsf, lr` (which would let flag-emulating probes' SPSR
        // updates propagate) breaks the boot on QEMU raspi3b with
        // cascading data aborts at the next kernel store (0x186b4).
        // Loads + movs without the MSR are stable; only the MSR
        // triggers the regression. Suspected QEMU raspi3b
        // banked-SPSR-write quirk (consistent with docs/QEMU_BUGS.md
        // Bug #1's family of `banked_spsr[]` indexing issues; possibly
        // distinct because Bug #1 is about EL2 → AArch32 propagation,
        // whereas this is AArch32-internal MSR SPSR from UND mode).
        // Emulating the bne at 0x257088 directly via ELR_EL2 in an HVC
        // handler is the path forward; it sidesteps SPSR entirely.
        let stub = UND_RETURN_STUB_OFFSET / 4;
        write_rom_code_word(rom, stub + 0, 0xE59F_E000); // ldr lr, [pc, #0]
        write_rom_code_word(rom, stub + 1, 0xE1B0_F00E); // movs pc, lr
        // Literal slot — loaded via the `ldr lr, [pc, #0]` at stub+0
        // under BE-8 each ERET; the runtime updater is
        // `return_to_guest_from_und`. Placeholder is data.
        write_rom_data_word(rom, stub + 2, 0xDEAD_C0DE);
    }

    // No per-range I-cache publish here: the single
    // `icache_publish_range` sweep over the whole ROM aperture at the
    // end of `load_newton_rom` runs strictly after this function and
    // covers the UND vector word at IPA 0x04, the trampoline body, the
    // FPA bypass stub, UND_TRAMP, and UND_RETURN_STUB with the same
    // DC CVAU; DSB; IC IVAU; DSB; ISB sequence over a wider range.
}

/// Swap the trampoline's save-slot literal from the pre-MMU RAM IPA
/// (0x0400_5F00) to the post-MMU kernel VA (0x0C00_4F00). Called when
/// the guest turns on its stage-1 MMU — past that point, VA
/// 0x0400_xxxx aliases ROM under the kernel's L1[0x40] section and
/// the pre-MMU literal would make the first STR in the trampoline
/// fault on a read-only page.
pub unsafe fn install_und_vector_swap_post_mmu() {
    // SAFETY: single-word write to each trampoline's slot-base literal.
    // Caller must hold exclusive access to the ROM backing. Swaps the
    // UND trampoline and the DABT diagnostic trampoline.
    //
    // With HYP_TRAMP_SCRATCH_BASE relocated into the SCRATCH_POOL IPA
    // window, the same value works both pre-MMU (stage-1 off → IPA →
    // stage-2 → host SCRATCH_POOL) and post-MMU (kernel L1[0x60] →
    // IPA → stage-2). The swap therefore writes the same literal as
    // the pre-MMU install path; kept as a callable no-op to preserve
    // the install/uninstall contract for future changes that might
    // re-introduce a swap.
    //
    // The literals are LDR-loaded data — under BE-8 they must be
    // byte-swapped on host so a CPSR.E=1 LDR returns the intended value.
    unsafe {
        let rom = rom_host_pa() as *mut u32;
        let base = UND_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, base + 23, crate::hv::trap::HYP_TRAMP_SCRATCH_BASE);
        let db = DABT_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, db + 14, DABT_SAVE_PA);
    }
}

/// Revert the trampoline's save-slot literal back to the pre-MMU value.
/// Called when the guest turns its stage-1 MMU off — typically the
/// SWIBoot→ROMBoot soft-reset path. (Now a no-op given
/// HYP_TRAMP_SCRATCH_BASE works pre + post-MMU; retained for symmetry
/// with the post-MMU swap.)
pub unsafe fn install_und_vector_swap_pre_mmu() {
    // SAFETY: same as the post-MMU swap above. Literal slots are data
    // under BE-8.
    unsafe {
        let rom = rom_host_pa() as *mut u32;
        let base = UND_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, base + 23, crate::hv::trap::HYP_TRAMP_SCRATCH_BASE);
        let db = DABT_TRAMP_OFFSET / 4;
        write_rom_data_word(rom, db + 14, DABT_SAVE_PA);
    }
}
