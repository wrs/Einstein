//! Guest-state snapshot save/load via QEMU semihosting.
//!
//! The goal is to let a debugging session skip past the part of the
//! 717006 boot we already understand. Periodic auto-saves write into
//! a small ring of slots during boot; when the guest eventually
//! fails, we rebuild the hypervisor and resume from the newest valid
//! slot — or from an earlier one if the failure mode is in the last
//! saved window.
//!
//! Because we persist only guest state — not EL2 code addresses —
//! snapshots survive hypervisor rebuilds. That's what makes the
//! workflow useful: edit hypervisor code, rebuild, resume.
//!
//! ## Slots
//!
//! `NUM_SLOTS` files in `/tmp/newton-snapshot-{0..N}.bin`. Saves
//! round-robin on a monotonic sequence counter; the slot with the
//! highest seq on load is the winner. On hypervisor startup,
//! `init()` scans existing slots and seeds the counter so rolling
//! continues across hypervisor restarts — a boot that crashes at
//! seq=12 leaves slots with seqs 9, 10, 11, 12, and the next run
//! resumes from 12 (or, if the user wants an earlier point, they
//! can copy slot 9 onto slot 12 before rerunning).
//!
//! ## Triggers
//!
//! - Periodic: `maybe_autosave()` called from the trap dispatcher.
//!   It saves when `CNTPCT_EL0` has advanced at least
//!   `AUTOSAVE_INTERVAL_MS` since the last save. Wall-clock rather
//!   than trap count because the point is to save developer time —
//!   a pathological abort loop would generate many traps per second
//!   and thrash saves; a quiet guest would barely save at all.
//!   Wall-clock pacing smooths both.
//! - Guest-triggered: HVC `#0x20` from the guest issues an immediate
//!   save (useful inside tests or wedged into specific code paths).
//!
//! ## Semihosting
//!
//! AArch64 HLT `#0xF000` with SYS_OPEN / SYS_WRITE / SYS_READ /
//! SYS_CLOSE (Arm Semihosting for AArch32/64, section 5.3). Paths
//! are resolved against the host process's cwd when QEMU is started
//! with `-semihosting-config enable=on,target=native`.
//!
//! ## Format
//!
//! Little-endian throughout. Header followed by raw memory regions
//! (RAM, FB, SCRATCH_POOL) in a fixed order. Bump `VERSION` when the
//! layout changes so stale files get rejected loudly.
//!
//! A FNV-1a fingerprint of the first 1 KiB of GUEST_ROM is included so
//! a snapshot taken from one guest binary can't accidentally load into
//! a different one. The header also carries a `flash_fingerprint` —
//! FNV-1a over the full 8 MiB GUEST_FLASH at save time — so resume
//! can detect a divergence between the saved CPU/RAM state and the
//! current persistent flash and cold-boot instead. See
//! `src/flash_persist/`.

// On `no-semihost` builds the public entry points early-return, so
// every private helper below (`open`, `peek_seq`, `build_header`,
// `cntpct`, `load`, …) is unreachable. They're still useful to keep
// in the source so swapping back to the semihost-host build path is
// a feature-flag toggle — silence dead-code warnings for them
// in that configuration.
#![cfg_attr(feature = "no-semihost", allow(dead_code))]

use core::arch::asm;
#[cfg(not(feature = "no-semihost"))]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{guest_mem, kprintln, trap::TrapContext};

// ---- semihosting primitives ---------------------------------------

const SYS_OPEN: u64 = 0x01;
const SYS_CLOSE: u64 = 0x02;
const SYS_WRITE: u64 = 0x05;
const SYS_READ: u64 = 0x06;

/// Arm Semihosting SYS_OPEN mode flags (C fopen-style).
const MODE_READ_BINARY: u64 = 0x01; // "rb"
const MODE_WRITE_BINARY: u64 = 0x05; // "wb"

/// Execute one semihosting call. `op` is the SYS_* subfunction ID;
/// `arg` is a pointer to an array of u64 parameters matching the op.
/// Returns the value the semihosting handler places in x0.
#[inline]
unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
    let result: u64;
    // SAFETY: HLT #0xF000 with semihosting enabled in QEMU is a
    // controlled trap the emulator intercepts; it does not crash or
    // drop EL2 state. The arg pointer lifetime covers the call.
    unsafe {
        asm!(
            "hlt #0xF000",
            inout("x0") op => result,
            in("x1") arg as u64,
            options(nostack, preserves_flags),
        );
    }
    result as i64
}

struct FileHandle(u64);

fn open(path: &[u8], mode: u64) -> Option<FileHandle> {
    let args: [u64; 3] = [path.as_ptr() as u64, mode, (path.len() - 1) as u64];
    let h = unsafe { semihost(SYS_OPEN, args.as_ptr()) };
    if h < 0 {
        None
    } else {
        Some(FileHandle(h as u64))
    }
}

fn close(h: FileHandle) {
    let args: [u64; 1] = [h.0];
    let _ = unsafe { semihost(SYS_CLOSE, args.as_ptr()) };
}

fn write_all(h: &FileHandle, data: &[u8]) -> Result<(), &'static str> {
    let args: [u64; 3] = [h.0, data.as_ptr() as u64, data.len() as u64];
    let unwritten = unsafe { semihost(SYS_WRITE, args.as_ptr()) };
    if unwritten == 0 {
        Ok(())
    } else {
        Err("semihost SYS_WRITE short write")
    }
}

fn read_all(h: &FileHandle, buf: &mut [u8]) -> Result<(), &'static str> {
    let args: [u64; 3] = [h.0, buf.as_mut_ptr() as u64, buf.len() as u64];
    let not_read = unsafe { semihost(SYS_READ, args.as_ptr()) };
    if not_read == 0 {
        Ok(())
    } else {
        Err("semihost SYS_READ short read")
    }
}

// ---- snapshot layout ---------------------------------------------

/// "NHSNAP\0\x01" encoded little-endian.
const MAGIC: u64 = 0x0150_414E_5348_4E00;
/// Bump whenever the Header layout changes. Old snapshot files get
/// rejected loudly by `peek_seq` / `load`.
///
/// v3: replaced the 15-entry `gprs` array with the full 31-entry
/// AArch64 GPR view (`x0..x30`). At AArch32→AArch64 exception entry
/// the AArch64 GPR file aliases AArch32 banked registers per ARM ARM
/// DDI 0487 D1.21.1 Table D1-79, so capturing all 31 X registers
/// preserves R0..R12 and the per-mode banked SP/LR (USR/SVC/ABT/UND/
/// IRQ/FIQ) without any AArch32-side stash dance. Removed the
/// `sp_el0 / sp_el1 / elr_el1` fields: those are AArch64-only EL0/EL1
/// special-purpose registers with **no** architectural alias to any
/// AArch32 banked R13/R14, so writing them at restore did nothing
/// useful for an AArch32 guest.
// VERSION = 4: BE-8 migration. Old (BE-32 word-invariant) snapshots
// have RAM/flash bytes in the opposite byte-lane geometry, plus EL1
// SCTLR with EE=0; both are incompatible with a Phase-2 BE-8 boot, so
// the version bump rejects them automatically at load time.
// VERSION = 6: flash moved out of the snapshot file into
// `src/flash_persist/`'s standalone `$HOME/.newton/flash.bin`. Header
// `flash_size` field replaced with `flash_fingerprint` (FNV-1a-32 over
// GUEST_FLASH at save time) used for resume-time coherence.
// VERSION = 7: added the 384 KiB shadow_stub::SCRATCH_POOL as a third
// saved region (guest-visible RW at IPA 0x0600_0000; holds DABT-save
// scratch consumed by later kernel code), and three guest-fault sysreg
// homes (far_el1/esr_el1/ifsr32_el2 = AArch32 DFAR/DFSR/IFSR) plus the
// stub-stash TLS registers (tpidr_el0/tpidrro_el0 = TPIDRURW/TPIDRRO)
// to the header. The version bump rejects v6 (and earlier) files at
// load time, before any field is parsed — see `peek_seq` / `load`.
const VERSION: u32 = 7;

/// Number of rolling slots. Each slot is ~14 MiB, so four slots cost
/// ~56 MiB of host disk and give the user three save windows of
/// rewind space before the oldest gets overwritten.
const NUM_SLOTS: usize = 4;

/// Slot paths. Must be NUL-terminated so `open` can hand them to
/// semihosting SYS_OPEN directly.
const SLOT_PATHS: [&[u8]; NUM_SLOTS] = [
    b"/tmp/newton-snapshot-0.bin\0",
    b"/tmp/newton-snapshot-1.bin\0",
    b"/tmp/newton-snapshot-2.bin\0",
    b"/tmp/newton-snapshot-3.bin\0",
];

/// Minimum wall-clock gap between periodic autosaves. Measured
/// against CNTPCT_EL0 in `maybe_autosave`. Chosen to save ~once
/// every couple of seconds during a ROM boot — fast enough to
/// capture progress before an oncoming failure, slow enough to
/// not dominate wall time with 14 MiB semihosting writes.
pub const AUTOSAVE_INTERVAL_MS: u64 = 2_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: u64,
    version: u32,
    _pad0: u32,

    saved_pc: u32,
    saved_cpsr: u32,

    /// AArch64 GPRs x0..x30 captured at save time. Per Table D1-79
    /// these alias AArch32 banked registers by bank name:
    ///   gprs[0..7]     = R0..R7            (always shared)
    ///   gprs[8..12]    = R8_usr..R12_usr   (= R8..R12 in non-FIQ modes)
    ///   gprs[13]       = SP_usr            gprs[14]       = LR_usr
    ///   gprs[15]       = SP_hyp            (unused — guest is at EL1)
    ///   gprs[16]       = LR_irq            gprs[17]       = SP_irq
    ///   gprs[18]       = LR_svc            gprs[19]       = SP_svc
    ///   gprs[20]       = LR_abt            gprs[21]       = SP_abt
    ///   gprs[22]       = LR_und            gprs[23]       = SP_und
    ///   gprs[24..28]   = R8_fiq..R12_fiq
    ///   gprs[29]       = SP_fiq            gprs[30]       = LR_fiq
    /// Per Table D1-85, the upper 32 bits of x16..x30 on AArch32→
    /// AArch64 exception entry are CONSTRAINED UNPREDICTABLE — we
    /// truncate to u32.
    gprs: [u32; 31],

    sctlr_el1: u32,
    /// Explicit padding so the following u64 fields are naturally
    /// aligned without an implicit `repr(C)` hole that
    /// `save_via_semihost` would serialize as uninitialized stack
    /// garbage (nondeterministic file bytes / UB-by-the-book). Mirrors
    /// `_pad0` / `_pad1`.
    _pad2: u32,
    ttbr0_el1: u64,
    ttbr1_el1: u64,
    tcr_el1: u64,
    dacr32_el2: u32,
    _pad3: u32,
    vbar_el1: u64,
    cpacr_el1: u64,
    mair_el1: u64,

    /// AArch32 guest fault sysregs, captured at their AArch64 homes
    /// (DDI 0487): DFAR = `FAR_EL1[31:0]`, DFSR = `ESR_EL1`, IFSR =
    /// `IFSR32_EL2`. The DABT fast trampoline forwards aborts to the
    /// kernel's DAH, which reads DFSR/DFAR natively several instructions
    /// later; an autosave landing in that window must resume with the
    /// fault registers the abort produced, not cold-boot values.
    far_el1: u64,
    esr_el1: u64,
    ifsr32_el2: u32,
    _pad4: u32,

    /// Per-thread TLS scratch (AArch32 TPIDRURW = `TPIDR_EL0`,
    /// TPIDRRO = `TPIDRRO_EL0`). The DABT fast trampoline and the FPA
    /// bypass stub stash R0/R1/R12 here across their bodies; capturing
    /// them is defense-in-depth so a resume that lands at a stub PC the
    /// transient-PC gate somehow let through doesn't restore garbage.
    tpidr_el0: u64,
    tpidrro_el0: u64,

    /// SPSR_<mode> banked sysregs (AArch64-named, accessible via
    /// `mrs/msr spsr_abt` etc.). SPSR_svc is the AArch64 SPSR_EL1
    /// alias (DDI 0487 D13.2 — SPSR_EL1 bits[31:0] are architecturally
    /// mapped to AArch32 SPSR_svc).
    spsr_svc: u32,
    spsr_abt: u32,
    spsr_und: u32,
    spsr_irq: u32,
    spsr_fiq: u32,
    _pad1: u32,

    ram_size: u32,
    fb_size: u32,
    /// Size of the saved `shadow_stub::SCRATCH_POOL` region (guest-
    /// visible RW at IPA 0x0600_0000). Serialized after FB; checked on
    /// load like ram/fb so a layout mismatch rejects the file.
    scratch_size: u32,
    /// FNV-1a-32 over the full 8 MiB GUEST_FLASH at save time. Flash
    /// bytes themselves live in `$HOME/.newton/flash.bin` (managed by
    /// `src/flash_persist/`); this fingerprint lets the resume path
    /// detect a divergence between the on-disk flash and the saved
    /// CPU/RAM state and cold-boot if they don't match.
    flash_fingerprint: u32,

    /// FNV-1a over the first 1024 bytes of GUEST_ROM post-patches.
    /// On load, we recompute and reject the snapshot if it doesn't
    /// match — catches the common error of carrying a guest-test
    /// snapshot into a ROM boot (or vice versa) and ERET-ing into
    /// someone else's code.
    rom_fingerprint: u32,

    /// Monotonically increasing save sequence. The slot with the
    /// highest seq across all `NUM_SLOTS` files is the one `load()`
    /// picks; seq also persists across hypervisor runs so a resumed
    /// session's new saves don't masquerade as older than the
    /// snapshots it started from.
    seq: u64,
}

// ---- save --------------------------------------------------------

/// Monotonically increasing save sequence, persisted across the ring
/// via the Header::seq field.
static SAVE_SEQ: AtomicU64 = AtomicU64::new(1);
/// CNTPCT_EL0 reading at the last successful autosave (0 = never).
static LAST_SAVE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Current value of the save sequence counter — number of rolling
/// saves performed so far in this hypervisor run. Used by debug
/// triggers that want to halt / diverge after a known number of
/// snapshots have been taken (see the `FAKE BUG` demo in trap.rs
/// for the pattern).
#[allow(dead_code)]
pub fn current_seq() -> u64 {
    SAVE_SEQ.load(Ordering::Relaxed)
}

/// Scan the ring for existing saves and seed `SAVE_SEQ` so resumed
/// runs don't reuse sequence numbers. Call exactly once before the
/// first `save()` / `load()`.
///
/// On `no-semihost` builds (real silicon) there is no host filesystem
/// to scan; the whole snapshot subsystem is inert.
pub fn init() {
    #[cfg(feature = "no-semihost")]
    return;
    #[cfg(not(feature = "no-semihost"))]
    {
        let mut max_seq: u64 = 0;
        for slot in 0..NUM_SLOTS {
            if let Some(seq) = peek_seq(SLOT_PATHS[slot]) {
                if seq > max_seq {
                    max_seq = seq;
                }
            }
        }
        SAVE_SEQ.store(max_seq + 1, Ordering::Relaxed);
    }
}

/// Periodic-save hook. Called from the EL2 timer IRQ path
/// (`trap.rs::trap_irq`) so wall-clock progression drives the
/// cadence even when the guest is spinning in an abort loop that
/// never reaches a synchronous trap. Saves iff CNTPCT_EL0 has
/// advanced at least `AUTOSAVE_INTERVAL_MS` since the last save.
pub fn maybe_autosave(ctx: &TrapContext) {
    #[cfg(feature = "no-semihost")]
    {
        let _ = ctx;
        // Snapshot ring itself is inert on real silicon, but the
        // flash-persist backend (e.g. flash-persist-sd) still needs
        // its periodic save. Use the same wall-clock gate as the
        // semihost path so the cadence is identical.
        maybe_flash_autosave();
        return;
    }
    #[cfg(not(feature = "no-semihost"))]
    maybe_autosave_via_semihost(ctx)
}

#[cfg(feature = "no-semihost")]
fn maybe_flash_autosave() {
    let now = cntpct();
    let freq = cntfrq();
    if freq == 0 {
        return;
    }
    let last = LAST_SAVE_TICKS.load(Ordering::Relaxed);
    if last != 0 {
        let interval_ticks = (AUTOSAVE_INTERVAL_MS * freq) / 1_000;
        if now.wrapping_sub(last) < interval_ticks {
            return;
        }
    }
    LAST_SAVE_TICKS.store(now, Ordering::Relaxed);
    // The SD write blocks EL2 for hundreds of ms; unmask IRQs so the
    // audio MAI ring stays fed and CNTHP keeps rearming while it runs.
    crate::cpu::with_irqs_unmasked(|| crate::flash_persist::maybe_save());
}

#[cfg(not(feature = "no-semihost"))]
fn maybe_autosave_via_semihost(ctx: &TrapContext) {
    let now = cntpct();
    let freq = cntfrq();
    let last = LAST_SAVE_TICKS.load(Ordering::Relaxed);
    let interval_ticks = (AUTOSAVE_INTERVAL_MS * freq) / 1_000;
    if last != 0 && now.wrapping_sub(last) < interval_ticks {
        return;
    }

    // Gate autosaves while guest BPs are live: the saved ROM would
    // contain our marker UDF, and the loader on the next boot would
    // halt with "marker at PC=… with no matching table entry". Log
    // the transition (gating on → off) so the user can tell their
    // debug session is suppressing autosave, without spamming every
    // 2 s. See `src/guest_bp.rs`.
    static AUTOSAVE_GATED: AtomicBool = AtomicBool::new(false);
    if crate::guest_bp::any_installed() {
        let was = AUTOSAVE_GATED.swap(true, Ordering::Relaxed);
        if !was {
            kprintln!(
                "snapshot: autosave gated — guest_bp active (autosaves will resume when all BPs are cleared)"
            );
        }
        return;
    } else if AUTOSAVE_GATED.swap(false, Ordering::Relaxed) {
        kprintln!("snapshot: autosave resumed — no guest_bp active");
    }

    // Gate autosaves when the IRQ that woke us didn't come from the
    // AArch32 guest. The CNTHP physical IRQ also fires while EL2 is
    // already running (e.g. `pause_system` waiting in `wfi`), in
    // which case `SPSR_EL2` / `ELR_EL2` hold the EL2 hypervisor's
    // PSTATE / PC and `ctx` is the EL2 register file — saving any of
    // those would poison the slot and a later resume would ERET into
    // EL2 hypervisor code at an EL2 PC. SPSR_EL2 bit M[4]=1 indicates
    // the previous PSTATE was AArch32 (DDI 0487 D13.2 / D1.21.1); any
    // other value means we were nested inside EL2 and must skip.
    let spsr_el2 = read_sysreg64("spsr_el2");
    if (spsr_el2 & (1 << 4)) == 0 {
        return;
    }

    // Gate autosaves when the guest PC is inside a hypervisor-owned
    // transient region whose correct execution depends on hidden
    // scratch state (TPIDRURW, RAM stash slots, staged ERET PC).
    //
    //    - Tracer trampoline pool (0x00900000..0x00E00000): each slot
    //      HVCs, ERETs to the original first instruction, then chains
    //      through slot[2]..slot[4] back to orig_pc+4. If an IRQ
    //      fires mid-slot, the saved PC lands inside the slot body —
    //      a resume ERETs back there without the HVC-side state the
    //      slot was written to assume.
    //
    //    - Hypervisor ROM tail 0x00FFFF00..0x01000000: UND trampoline,
    //      SBA post-emulation trampoline, DABT-bounce trampoline, UND
    //      return stub. These depend on TPIDRURW scratch and RAM save
    //      slots staged immediately before ERET.
    //
    // Banked SP/LR for non-USR modes are NOT a reason to skip: the
    // trap context already captures all 31 X registers, which alias
    // every AArch32 banked R8..R14 per Table D1-79.
    //
    // Skipping here costs nothing: the bad moments are transient
    // (microseconds), and the next IRQ (≤16 ms later) retries and
    // almost always finds a stable state. LAST_SAVE_TICKS is NOT
    // updated on a skip, so net autosave cadence stays ~2 s.
    let guest_pc = read_sysreg64("elr_el2") as u32;
    if pc_in_hypervisor_transient_region(guest_pc) {
        return;
    }

    // Either first save, or enough wall-clock has passed.
    //
    // Flush flash first so the snapshot's `flash_fingerprint` describes
    // the bytes that just made it to disk. If flash save fails it'll
    // re-mark dirty internally and retry on the next tick; we still
    // try the snapshot save (any divergence is caught by the
    // fingerprint check on resume).
    //
    // The flash store's SD write (real-hardware backend) blocks EL2
    // for hundreds of ms; unmask IRQs so audio/CNTHP stay serviced
    // while it runs. The semihost backend's write is fast and
    // unaffected.
    crate::cpu::with_irqs_unmasked(|| crate::flash_persist::maybe_save());

    let mut gprs = [0u64; 31];
    for i in 0..31 {
        gprs[i] = ctx.x[i];
    }
    if save(&gprs).is_ok() {
        LAST_SAVE_TICKS.store(now, Ordering::Relaxed);
    }
}

/// True if `pc` is inside a hypervisor-installed trampoline or stub
/// whose correct execution depends on hidden scratch state (TPIDRURW,
/// RAM save slots, banked-mode SP/LR captures, staged ERET PC). A
/// snapshot taken at such a PC cannot be faithfully resumed.
///
/// Ranges covered (constants verified against `guest_mem` /
/// `rom_patches`):
///   - `0x008FFF00..0x00900000` — DABT fast trampoline
///     (`guest_mem::DABT_FAST_TRAMP_OFFSET`, 41 words). Saves
///     LR_abt/SP_abt/SPSR_abt and stashes R0/R1 in TPIDRURW/TPIDRRO;
///     handles the dominant fault stream, so a nontrivial fraction of
///     wall time sits here.
///   - `0x00900000..0x00E00000` — tracer trampoline pool
///     (`tracer::TRAMPOLINE_IPA..TRAMPOLINE_END`).
///   - `0x00FFFD80..0x01000000` — patch-stub arena
///     (`rom_patches::PATCH_STUB_ARENA_BASE`), FPA bypass stub
///     (`guest_mem::FPA_BYPASS_STUB_OFFSET` = 0x00FFFEC0), UND
///     trampoline (0x00FFFF00), DABT trampoline (0x00FFFFA8), and UND
///     return stub (0x00FFFFE4). The FPA stub and DABT trampoline also
///     stash R0/R1/R12 in TPIDRURW/TPIDRRO.
///
/// The tracer pool is only populated when the `trace` feature is on,
/// but checking always is cheap and harmless — nothing the guest
/// does naturally lands ELR_EL2 in that range otherwise.
///
/// Delegates to `guest_mem::is_hypervisor_code_region` — the single
/// source of truth for these ranges, shared with
/// `guest_endian::pa_is_rom_code` so the two lists can't drift.
fn pc_in_hypervisor_transient_region(pc: u32) -> bool {
    crate::guest_mem::is_hypervisor_code_region(pc)
}

/// Write a snapshot to the next ring slot. Called from periodic
/// autosaves and from the HVC #0x20 handler.
///
/// `gprs` must hold x0..x30 of the guest at save time (the AArch64
/// view that aliases AArch32 R0..R12 and every banked SP/LR per
/// Table D1-79); ELR_EL2 and SPSR_EL2 give the PC and CPSR to resume
/// at.
pub fn save(gprs: &[u64; 31]) -> Result<(), &'static str> {
    #[cfg(feature = "no-semihost")]
    {
        let _ = gprs;
        return Err("snapshot unavailable on no-semihost builds");
    }
    #[cfg(not(feature = "no-semihost"))]
    save_via_semihost(gprs)
}

#[cfg(not(feature = "no-semihost"))]
fn save_via_semihost(gprs: &[u64; 31]) -> Result<(), &'static str> {
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let slot = (seq as usize) % NUM_SLOTS;
    let path = SLOT_PATHS[slot];

    let header = build_header(gprs, seq);

    let fh = open(path, MODE_WRITE_BINARY).ok_or("semihost SYS_OPEN failed")?;

    // SAFETY: header is a plain-old-data struct on the stack.
    let header_bytes = unsafe {
        core::slice::from_raw_parts(
            &header as *const Header as *const u8,
            core::mem::size_of::<Header>(),
        )
    };
    write_all(&fh, header_bytes)?;

    // SAFETY: the backing stores are static mut u8 arrays; we take a
    // read-only view for the duration of the semihosting write, no
    // concurrent writer is possible on single-core EL2.
    let ram = unsafe {
        core::slice::from_raw_parts(
            guest_mem::ram_host_pa() as *const u8,
            guest_mem::RAM_SIZE,
        )
    };
    write_all(&fh, ram)?;

    let fb = unsafe {
        core::slice::from_raw_parts(
            guest_mem::fb_host_pa() as *const u8,
            guest_mem::FRAMEBUFFER_SIZE,
        )
    };
    write_all(&fh, fb)?;

    // SCRATCH_POOL is mapped RW into the guest (stage-2 at IPA
    // 0x0600_0000) and holds cross-trap state — notably the DABT
    // trampoline's LR_abt/SP_abt/SPSR_abt save slots, which patched
    // kernel code reads back several instructions after the abort. It
    // is guest-visible state, so it belongs in the snapshot.
    let scratch = unsafe {
        core::slice::from_raw_parts(
            crate::shadow_stub::scratch_pool_host_pa() as *const u8,
            crate::shadow_stub::SCRATCH_POOL_SIZE,
        )
    };
    write_all(&fh, scratch)?;

    // Flash bytes live in `$HOME/.newton/flash.bin` (see
    // `flash_persist`), not in this snapshot file. The header carries
    // a FNV-1a fingerprint of GUEST_FLASH so resume can detect a
    // divergence between persistent-flash and saved CPU/RAM state.

    close(fh);

    kprintln!(
        "snapshot: seq={} saved PC={:#x} CPSR={:#x} to slot {}",
        header.seq,
        header.saved_pc,
        header.saved_cpsr,
        slot,
    );
    Ok(())
}

fn build_header(gprs_u64: &[u64; 31], seq: u64) -> Header {
    let mut gprs = [0u32; 31];
    for i in 0..31 {
        gprs[i] = gprs_u64[i] as u32;
    }
    Header {
        magic: MAGIC,
        version: VERSION,
        _pad0: 0,
        saved_pc: read_sysreg64("elr_el2") as u32,
        saved_cpsr: read_sysreg64("spsr_el2") as u32,
        gprs,
        sctlr_el1: read_sysreg64("sctlr_el1") as u32,
        _pad2: 0,
        ttbr0_el1: read_sysreg64("ttbr0_el1"),
        ttbr1_el1: read_sysreg64("ttbr1_el1"),
        tcr_el1: read_sysreg64("tcr_el1"),
        dacr32_el2: read_sysreg64("dacr32_el2") as u32,
        _pad3: 0,
        vbar_el1: read_sysreg64("vbar_el1"),
        cpacr_el1: read_sysreg64("cpacr_el1"),
        mair_el1: read_sysreg64("mair_el1"),
        far_el1: read_sysreg64("far_el1"),
        esr_el1: read_sysreg64("esr_el1"),
        ifsr32_el2: read_sysreg64("ifsr32_el2") as u32,
        _pad4: 0,
        tpidr_el0: read_sysreg64("tpidr_el0"),
        tpidrro_el0: read_sysreg64("tpidrro_el0"),
        spsr_svc: read_sysreg64("spsr_svc") as u32,
        spsr_abt: read_sysreg64("spsr_abt") as u32,
        spsr_und: read_sysreg64("spsr_und") as u32,
        spsr_irq: read_sysreg64("spsr_irq") as u32,
        spsr_fiq: read_sysreg64("spsr_fiq") as u32,
        _pad1: 0,
        ram_size: guest_mem::RAM_SIZE as u32,
        fb_size: guest_mem::FRAMEBUFFER_SIZE as u32,
        scratch_size: crate::shadow_stub::SCRATCH_POOL_SIZE as u32,
        flash_fingerprint: crate::flash_persist::fingerprint(),
        rom_fingerprint: rom_fingerprint(),
        seq,
    }
}

// ---- generic timer (wall clock) ----------------------------------

fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: MRS of a RO sysreg has no side effects.
    unsafe {
        asm!("mrs {}, cntpct_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: as above.
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

fn rom_fingerprint() -> u32 {
    // FNV-1a over the first 1 KiB of GUEST_ROM after all load-time
    // patches have been applied. Distinct guest binaries (different
    // test builds, ROM vs test) diverge in those bytes.
    // SAFETY: reading static backing store; single-threaded.
    let bytes = unsafe {
        core::slice::from_raw_parts(guest_mem::rom_host_pa() as *const u8, 1024)
    };
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---- load --------------------------------------------------------

/// State recovered from a snapshot, ready for `eret_to_restored`.
#[derive(Clone, Copy)]
pub struct RestoreState {
    pub pc: u32,
    pub cpsr: u32,
    /// Full AArch64 x0..x30 view; `eret_to_restored` writes these
    /// back to the GPR file before ERET so AArch32 banked R0..R14
    /// land in the right slot per Table D1-79.
    pub gprs: [u32; 31],
}

/// Scan the ring for the slot with the highest `seq` and load it.
/// Missing files / bad magic / mismatched fingerprint are dropped
/// silently; if no slot qualifies we return None and the caller
/// cold-boots.
pub fn load_latest() -> Option<RestoreState> {
    #[cfg(feature = "no-semihost")]
    return None;
    #[cfg(not(feature = "no-semihost"))]
    {
        let mut best: Option<(u64, &[u8])> = None;
        for slot in 0..NUM_SLOTS {
            let path = SLOT_PATHS[slot];
            if let Some(seq) = peek_seq(path) {
                if best.map_or(true, |(s, _)| seq > s) {
                    best = Some((seq, path));
                }
            }
        }
        let (seq, path) = best?;
        kprintln!("snapshot: latest valid slot is seq={}", seq);
        load(path)
    }
}

/// Read just the `seq` field of a slot without pulling in the rest
/// of the file. Used by `load_latest` to pick the winner and by
/// `init` to seed the save counter across runs.
fn peek_seq(path: &[u8]) -> Option<u64> {
    let fh = open(path, MODE_READ_BINARY)?;
    let mut header_buf = [0u8; core::mem::size_of::<Header>()];
    let read_result = read_all(&fh, &mut header_buf);
    close(fh);
    read_result.ok()?;
    let header: Header =
        unsafe { core::ptr::read_unaligned(header_buf.as_ptr() as *const Header) };
    if header.magic != MAGIC || header.version != VERSION {
        return None;
    }
    if header.rom_fingerprint != rom_fingerprint() {
        return None;
    }
    Some(header.seq)
}

/// Load a specific slot by path. Public so callers can resume from
/// a user-selected slot (e.g. when the latest slot is at the
/// failure and you want the one before).
pub fn load(path: &[u8]) -> Option<RestoreState> {
    let fh = open(path, MODE_READ_BINARY)?;

    let mut header_buf = [0u8; core::mem::size_of::<Header>()];
    if read_all(&fh, &mut header_buf).is_err() {
        close(fh);
        return None;
    }
    // SAFETY: read-unaligned in case the buffer alignment is <8; the
    // bytes were written from a valid Header on the same target ABI.
    let header: Header =
        unsafe { core::ptr::read_unaligned(header_buf.as_ptr() as *const Header) };

    if header.magic != MAGIC {
        kprintln!(
            "snapshot: bad magic {:#x} (want {:#x}); ignoring",
            header.magic, MAGIC
        );
        close(fh);
        return None;
    }
    if header.version != VERSION {
        kprintln!(
            "snapshot: version {} doesn't match expected {}; ignoring",
            header.version, VERSION
        );
        close(fh);
        return None;
    }
    if header.ram_size as usize != guest_mem::RAM_SIZE
        || header.fb_size as usize != guest_mem::FRAMEBUFFER_SIZE
        || header.scratch_size as usize != crate::shadow_stub::SCRATCH_POOL_SIZE
    {
        kprintln!(
            "snapshot: region sizes don't match (ram={} fb={} scratch={}); ignoring",
            header.ram_size, header.fb_size, header.scratch_size
        );
        close(fh);
        return None;
    }

    let current_fp = rom_fingerprint();
    if header.rom_fingerprint != current_fp {
        kprintln!(
            "snapshot: ROM fingerprint mismatch (file={:#010x} current={:#010x}); ignoring (snapshot is from a different guest binary)",
            header.rom_fingerprint, current_fp
        );
        close(fh);
        return None;
    }

    // SAFETY: backing stores are static mut u8 arrays; we overwrite
    // them entirely before the guest runs again.
    let ram = unsafe {
        core::slice::from_raw_parts_mut(
            guest_mem::ram_host_pa() as *mut u8,
            guest_mem::RAM_SIZE,
        )
    };
    if read_all(&fh, ram).is_err() {
        close(fh);
        return None;
    }

    let fb = unsafe {
        core::slice::from_raw_parts_mut(
            guest_mem::fb_host_pa() as *mut u8,
            guest_mem::FRAMEBUFFER_SIZE,
        )
    };
    if read_all(&fh, fb).is_err() {
        close(fh);
        return None;
    }

    // SCRATCH_POOL — written in the same fixed order as `save`.
    let scratch = unsafe {
        core::slice::from_raw_parts_mut(
            crate::shadow_stub::scratch_pool_host_pa() as *mut u8,
            crate::shadow_stub::SCRATCH_POOL_SIZE,
        )
    };
    if read_all(&fh, scratch).is_err() {
        close(fh);
        return None;
    }

    close(fh);

    // Flash coherence check: GUEST_FLASH has already been populated
    // by `flash_persist::try_load()` earlier in `kmain`. If the
    // persistent flash diverges from what the snapshot expected, the
    // saved CPU state may reference flash addresses with newer or
    // older content than it assumes — cold-boot rather than risk it.
    let current_flash_fp = crate::flash_persist::fingerprint();
    if header.flash_fingerprint != current_flash_fp {
        kprintln!(
            "snapshot: flash fingerprint mismatch (file={:#010x} current={:#010x}); cold-booting",
            header.flash_fingerprint, current_flash_fp
        );
        return None;
    }

    restore_sysregs(&header);

    // Seed SAVE_SEQ so saves after this resume extend the ring
    // rather than reusing the same slot we just loaded.
    SAVE_SEQ.store(header.seq + 1, Ordering::Relaxed);
    // Reset the autosave pacing so the first post-resume save
    // happens after AUTOSAVE_INTERVAL_MS, not immediately.
    LAST_SAVE_TICKS.store(cntpct(), Ordering::Relaxed);

    kprintln!(
        "snapshot: loaded seq={} guest PC={:#x} CPSR={:#x} from {}",
        header.seq,
        header.saved_pc,
        header.saved_cpsr,
        core::str::from_utf8(&path[..path.len() - 1]).unwrap_or("?"),
    );

    Some(RestoreState {
        pc: header.saved_pc,
        cpsr: header.saved_cpsr,
        gprs: header.gprs,
    })
}

fn restore_sysregs(h: &Header) {
    write_sysreg64("sctlr_el1", h.sctlr_el1 as u64);
    write_sysreg64("ttbr0_el1", h.ttbr0_el1);
    write_sysreg64("ttbr1_el1", h.ttbr1_el1);
    write_sysreg64("tcr_el1", h.tcr_el1);
    write_sysreg64("dacr32_el2", h.dacr32_el2 as u64);
    write_sysreg64("vbar_el1", h.vbar_el1);
    write_sysreg64("cpacr_el1", h.cpacr_el1);
    write_sysreg64("mair_el1", h.mair_el1);
    // AArch32 guest fault sysregs at their AArch64 homes (DDI 0487):
    // DFAR = FAR_EL1, DFSR = ESR_EL1, IFSR = IFSR32_EL2. Restoring
    // these lets a resume that lands between a DABT and the kernel DAH's
    // native DFSR/DFAR read see the fault registers the abort produced.
    write_sysreg64("far_el1", h.far_el1);
    write_sysreg64("esr_el1", h.esr_el1);
    write_sysreg64("ifsr32_el2", h.ifsr32_el2 as u64);
    // Per-thread TLS scratch (TPIDRURW/TPIDRRO). The trampoline stubs
    // stash R0/R1/R12 here; restoring keeps a stub-PC resume coherent.
    write_sysreg64("tpidr_el0", h.tpidr_el0);
    write_sysreg64("tpidrro_el0", h.tpidrro_el0);
    // SP_EL0 / SP_EL1 / ELR_EL1 are AArch64-only EL0/EL1 registers
    // with no architectural alias to AArch32 banked R13/R14. AArch32
    // SP_usr / SP_svc / LR_svc are restored via the GPR file (x13,
    // x19, x18) per Table D1-79 in `eret_to_restored`.
    write_sysreg64("spsr_svc", h.spsr_svc as u64);
    write_sysreg64("spsr_abt", h.spsr_abt as u64);
    write_sysreg64("spsr_und", h.spsr_und as u64);
    write_sysreg64("spsr_irq", h.spsr_irq as u64);
    write_sysreg64("spsr_fiq", h.spsr_fiq as u64);
    // The guest's stage-1 MMU config just jumped from whatever cold-boot
    // EL1 state happened to be (all zeros) to the saved post-boot state.
    // Any TLB entries cached during EL2 setup are stale and will cause
    // the resumed guest to fault on its own vector table. Invalidate all
    // EL1 TLB entries (stage-1 + intermediate stage-1→stage-2) for this
    // VMID, then DSB + ISB so the guest's first fetch sees fresh walks.
    // SAFETY: TLBI is an unprivileged operation at EL2 that only affects
    // cached translations.
    unsafe {
        asm!(
            "tlbi alle1",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

// ---- sysreg helpers ----------------------------------------------

macro_rules! sr_reader {
    ($name:expr) => {{
        let v: u64;
        // SAFETY: MRS has no side effects.
        unsafe {
            asm!(
                concat!("mrs {}, ", $name),
                out(reg) v,
                options(nomem, nostack, preserves_flags),
            );
        }
        v
    }};
}

fn read_sysreg64(reg: &'static str) -> u64 {
    match reg {
        "elr_el2" => sr_reader!("elr_el2"),
        "spsr_el2" => sr_reader!("spsr_el2"),
        "sctlr_el1" => sr_reader!("sctlr_el1"),
        "ttbr0_el1" => sr_reader!("ttbr0_el1"),
        "ttbr1_el1" => sr_reader!("ttbr1_el1"),
        "tcr_el1" => sr_reader!("tcr_el1"),
        "dacr32_el2" => sr_reader!("dacr32_el2"),
        "vbar_el1" => sr_reader!("vbar_el1"),
        "cpacr_el1" => sr_reader!("cpacr_el1"),
        "mair_el1" => sr_reader!("mair_el1"),
        // AArch32 SPSR_svc is architecturally mapped to AArch64
        // SPSR_EL1 (DDI 0487 D13.2). Read via spsr_el1.
        "spsr_svc" => sr_reader!("spsr_el1"),
        "spsr_abt" => sr_reader!("spsr_abt"),
        "spsr_und" => sr_reader!("spsr_und"),
        "spsr_irq" => sr_reader!("spsr_irq"),
        "spsr_fiq" => sr_reader!("spsr_fiq"),
        // AArch32 fault-register homes (DDI 0487): DFAR = FAR_EL1,
        // DFSR = ESR_EL1, IFSR = IFSR32_EL2.
        "far_el1" => sr_reader!("far_el1"),
        "esr_el1" => sr_reader!("esr_el1"),
        "ifsr32_el2" => sr_reader!("ifsr32_el2"),
        "tpidr_el0" => sr_reader!("tpidr_el0"),
        "tpidrro_el0" => sr_reader!("tpidrro_el0"),
        // A name not in the table means a header field was wired to a
        // sysreg this dispatch doesn't know about — a programming error
        // that must never silently read 0. Halt loudly.
        other => {
            kprintln!("snapshot: read_sysreg64 unknown register '{}'", other);
            crate::cpu::halt();
        }
    }
}

macro_rules! sr_writer {
    ($name:expr, $v:expr) => {{
        // SAFETY: sysreg writes are point-effect; callers follow up
        // with a barrier / isb when ordering matters.
        unsafe {
            asm!(
                concat!("msr ", $name, ", {}"),
                in(reg) $v,
                options(nostack, preserves_flags),
            );
        }
    }};
}

fn write_sysreg64(reg: &'static str, v: u64) {
    match reg {
        "sctlr_el1" => sr_writer!("sctlr_el1", v),
        "ttbr0_el1" => sr_writer!("ttbr0_el1", v),
        "ttbr1_el1" => sr_writer!("ttbr1_el1", v),
        "tcr_el1" => sr_writer!("tcr_el1", v),
        "dacr32_el2" => sr_writer!("dacr32_el2", v),
        "vbar_el1" => sr_writer!("vbar_el1", v),
        "cpacr_el1" => sr_writer!("cpacr_el1", v),
        "mair_el1" => sr_writer!("mair_el1", v),
        "spsr_svc" => sr_writer!("spsr_el1", v),
        "spsr_abt" => sr_writer!("spsr_abt", v),
        "spsr_und" => sr_writer!("spsr_und", v),
        "spsr_irq" => sr_writer!("spsr_irq", v),
        "spsr_fiq" => sr_writer!("spsr_fiq", v),
        "far_el1" => sr_writer!("far_el1", v),
        "esr_el1" => sr_writer!("esr_el1", v),
        "ifsr32_el2" => sr_writer!("ifsr32_el2", v),
        "tpidr_el0" => sr_writer!("tpidr_el0", v),
        "tpidrro_el0" => sr_writer!("tpidrro_el0", v),
        // As in `read_sysreg64`: an unknown name means a restore would
        // be silently dropped. Halt loudly instead.
        other => {
            kprintln!("snapshot: write_sysreg64 unknown register '{}'", other);
            crate::cpu::halt();
        }
    }
}
