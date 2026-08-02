//! Shared types for the per-ROM-version constant surface.
//!
//! Every item a version module (`r717006`, `r710031`, …) exports is
//! either a plain scalar or one of these structs. All are `Copy` so
//! the version constants can be `const` items and consumers can bind
//! them with `let Some(x) = rom_ver::… else { … }` without borrow
//! gymnastics.

/// One patched instruction site: the guest PC and the instruction word
/// the site must currently hold (guest-numerical form — the value
/// `scripts/disasm-out/rom.dis` prints). The patch installer verifies
/// `orig_insn` and halts loudly on mismatch, and probe handlers that
/// emulate the displaced instruction rely on it being exactly this
/// encoding.
#[derive(Copy, Clone)]
pub struct ProbeSite {
    pub pc: u32,
    pub orig_insn: u32,
}

/// A single word-write patch against the main ROM (offset < the
/// version's `ROM_IMAGE_SIZE`).
///
/// `orig` is the guest-numerical word the site must currently hold —
/// the installer verifies it and halts loudly on mismatch. The
/// code-vs-data storage decision is driven off the classifier bitmap
/// at install time.
#[derive(Copy, Clone)]
pub struct RomPatch {
    pub offset: u32,
    pub orig: u32,
    pub value: u32,
    pub name: &'static str,
}

/// Where the external REx loads in the ROM aperture and how many REx
/// blocks the ROM image itself embeds (the external REx's id field is
/// patched to `num_embedded_rexes` so NewtonOS's per-id config table
/// finds it — see `loader::load_newton_rom`).
#[derive(Copy, Clone)]
pub struct RexInfo {
    /// ROM-aperture byte offset the external REx is loaded at.
    pub pa_offset: u32,
    /// Number of REx blocks embedded in the ROM image (ids 0..n-1);
    /// the external REx claims id = n.
    pub num_embedded_rexes: u32,
}

/// Placement budget for every hypervisor-owned structure in the ROM
/// aperture tail (between the loaded ROM+REx content and the top of
/// the 16 MiB aperture). Versioned because the free space depends on
/// where this version's REx tail ends.
///
/// Consumers: `rom_patches` (patch-stub arena), `guest_trampolines`
/// (trampoline installers + `register_hyp_code_ranges`, which feeds
/// both `guest_endian`'s byte-order predicate and the snapshot
/// autosave gate), `inline_patch` (SBA stub pool), `diag::tracer`
/// (trampoline pool), and `unaligned_inline` (its inline-patch PC
/// limit is `tracer_pool_base`).
#[derive(Copy, Clone)]
pub struct RomTailLayout {
    /// DABT fast-forward trampoline base (between the REx tail and the
    /// tracer pool).
    pub dabt_fast_tramp: u32,
    /// Tracer trampoline pool (5-word slots, one per traced function).
    pub tracer_pool_base: u32,
    pub tracer_pool_end: u32,
    /// inline-patch SBA stub pool (16-word slots).
    pub sba_stub_pool_base: u32,
    pub sba_stub_pool_end: u32,
    /// Patch-stub arena (`rom_patches::alloc_patch_stub`).
    pub patch_stub_arena_base: u32,
    pub patch_stub_arena_end: u32,
    /// FPA-class UND bypass stub.
    pub fpa_bypass_stub: u32,
    /// UND-vector trampoline body.
    pub und_tramp: u32,
    /// DABT-vector (slow) trampoline body.
    pub dabt_tramp: u32,
    /// UND-return stub (`ldr lr, [pc]; movs pc, lr` + literal).
    pub und_return_stub: u32,
    /// One past the last ROM-tail stub byte (top of the ROM aperture).
    pub stubs_end: u32,
}

/// The BootOS / ROMBoot software-reset canary site.
#[derive(Copy, Clone)]
pub struct BootSites {
    /// `BootOS` entry — the reset vector's branch target. First entry
    /// is the legitimate boot; later entries are software resets.
    pub bootos: ProbeSite,
}

/// The loud-halt canary sites (dev builds only — see
/// `cfg(nh_loud_halt_canaries)`).
#[derive(Copy, Clone)]
pub struct LoudHaltSites {
    pub poweroff_reboot: ProbeSite,
    pub reboot: ProbeSite,
    pub stop_image: ProbeSite,
    /// `bl Throw` inside `TStackManager::Fault` (busError throw site).
    pub bus_error_throw: ProbeSite,
}

/// `PHammerOutTranslator` concrete-body patch sites (the kernel's REP
/// output path, rerouted into the EL2 UART).
#[derive(Copy, Clone)]
pub struct HammerSites {
    pub print: ProbeSite,
    pub putc: ProbeSite,
    pub flush: ProbeSite,
    pub stack_trace: ProbeSite,
    pub exception_notify: ProbeSite,
}

/// `UnhandledException` / `UnhandledNonUserModeException` entry
/// tripwires.
#[derive(Copy, Clone)]
pub struct UnhandledExceptionSites {
    pub user: ProbeSite,
    pub non_user: ProbeSite,
}

/// The Newton UND-dispatch-table slots for DebugStr / Debugger, each
/// redirected to a 2-word stub that stashes LR and HVCs to EL2.
#[derive(Copy, Clone)]
pub struct DebugUndSlots {
    pub debug_str: ProbeSite,
    pub debugger: ProbeSite,
}

/// `RealClockSeconds` body-replacement site: the entry PC plus the
/// original prologue words the replacement overwrites (3 instructions
/// + the literal slot's displaced word), all verified at install time.
#[derive(Copy, Clone)]
pub struct RealClockSite {
    pub entry: u32,
    pub prologue_origs: [u32; 4],
}

/// An Einstein-style injection patch: one instruction at `patch` is
/// replaced with a branch to an arena stub that re-does the displaced
/// work plus the injection, then branches to `resume_pc`.
#[derive(Copy, Clone)]
pub struct InjectionSite {
    pub patch: ProbeSite,
    pub resume_pc: u32,
}

/// A kernel `LDR` that reads an instruction word *as data* (fault
/// handlers decoding the faulting insn). Under the load-time BE-8
/// byteswap of code-marked words the read returns byteswapped bytes;
/// each site is redirected to a 3-word stub that re-does the LDR and
/// `REV`s the result. The stub is derived from `site.orig_insn` (same
/// LDR, Rd from bits[15:12]); the resume PC is `site.pc + 4`.
#[derive(Copy, Clone)]
pub struct InsnAsDataLdr {
    pub site: ProbeSite,
    pub name: &'static str,
}

/// The FPE prelude's conditional pair of instruction-as-data LDRs
/// (EQ for USR-source, NE for non-USR-source), both redirected to one
/// shared byteswap stub.
#[derive(Copy, Clone)]
pub struct FpeLdrSites {
    pub eq_site: ProbeSite,
    pub ne_site: ProbeSite,
    /// The unconditional LDR the stub executes (the `ldrteq` form's
    /// T-variant is dropped — ROM is always kernel-readable).
    pub stub_ldr: u32,
    pub resume_pc: u32,
}

/// `StorePermObject` entry + `LoadPermObject` return probe sites
/// (`log_store` feature).
#[derive(Copy, Clone)]
pub struct StoreProbeSites {
    pub store_perm_entry: ProbeSite,
    pub load_perm_ret: ProbeSite,
}

/// Kernel-global VAs consumed by the diagnostics layer (task dumps,
/// heap checks, stack-manager invariant walks). `None` in a version
/// module makes those diagnostics print an "unavailable" one-liner
/// and return.
#[derive(Copy, Clone)]
pub struct KernelGlobals {
    /// `gScheduler` (TScheduler*).
    pub scheduler_ptr: u32,
    /// `gCurrentTask` (TTask*).
    pub current_task: u32,
    /// `gWantSchedule` flag.
    pub want_schedule: u32,
    /// `gHoldSchedule` count.
    pub hold_schedule: u32,
    /// `gCurrentGlobals` (task globals pointer).
    pub current_globals: u32,
    /// `gObjectTable` — the TObjectTable instance itself (not a
    /// pointer slot).
    pub object_table: u32,
    /// Secondary TPhys object-table pointer slots (see `GetPhys`).
    pub object_table_a_ptr: u32,
    pub object_table_b_ptr: u32,
    /// `TObjectHeap*` global written by `InitObjects__Fv`.
    pub object_heap_ptr: u32,
    /// `gInterpreter` pointer slot (TInterpreter singleton).
    pub interpreter_ptr: u32,
    /// `gStackManagerHeap` literal; TStackManager* is at `+4`.
    pub stack_mgr_heap_literal: u32,
}
