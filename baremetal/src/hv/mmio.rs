//! MMIO dispatch for trapped guest accesses to Newton peripheral space.
//!
//! Every access that lands here comes from a stage-2 fault — the IPA
//! is outside our mapped ROM / RAM / flash / framebuffer regions.
//! The router normalizes BE-8 sub-word accesses (via [`crate::hv::be8`])
//! onto word-granular register accesses, looks the IPA up in
//! [`layout::MMIO_WINDOWS`] (first match wins), and acts on the
//! window's [`MmioPolicy`]:
//!
//!   * `Peripheral(id)` — dispatch to the model named by the closed
//!     [`PeriphId`] enum (vic / dma / pcmcia / serial / asic). A new
//!     model means a new variant, so a forgotten dispatch arm is a
//!     compile error. Unmodelled registers inside a peripheral window
//!     halt loudly inside the model.
//!   * `ReadZeroDropWrite` — probe/absent windows: reads return 0,
//!     writes are dropped (Einstein's "unknown bank" default, with the
//!     per-window rationale next to each `layout` definition).
//!   * `HaltUnknown` — loud halt with full context ([`halt_on_unknown`]).
//!
//! IPAs outside every window also halt loudly. Per Phase A (see
//! baremetal/PLAN.md and baremetal/CLAUDE.md): unknown sub-cases
//! return a loud error, not a silent stub value — silent drops mask
//! exactly the bugs the halts are meant to surface. When you find
//! yourself guessing what a register should return, build a probe run
//! and check Einstein's behaviour first — see `probe/FINDINGS.md`.

use crate::hv::be8;
use crate::hv::layout::{MmioPolicy, MmioWindow, PeriphId};
use crate::peripherals::{asic::Asic, dma::Dma, pcmcia::Pcmcia, serial::Serial, vic::Vic};
use crate::{arch::cpu, hv::layout, kprintln};

/// Uniform contract for a peripheral model routed by this file.
///
/// Every model dispatched by [`periph_read`]/[`periph_write`] below
/// implements the methods, so a model that forgets one fails to compile
/// rather than silently falling through. Dispatch stays static: the
/// router matches on the window's [`PeriphId`] and calls the trait
/// methods on the per-module zero-sized markers ([`Vic`], [`Dma`],
/// [`Pcmcia`], [`Serial`], [`Asic`]) — no `dyn`, no vtable.
///
/// `peek` is the side-effect-free read used by the BE-8 sub-word splice
/// and extraction (see [`peek_word`]): it must observe the same value
/// `read` would return without advancing any read side effect. The
/// default forwards to `read`, valid only for models whose reads are
/// genuinely side-effect-free — verified per model (vic/dma/pcmcia/
/// serial all read pure state or recomputed clocks). The one stateful
/// read in this dispatch, the ROM-serial-chip bit index, lives in
/// `peripherals::asic`, which overrides `peek` for real.
pub trait MmioPeripheral {
    /// Word read of the register at `ipa`, side effects included.
    fn read(ipa: u64) -> u32;
    /// Word write of `value` to the register at `ipa`.
    fn write(ipa: u64, value: u32);
    /// Side-effect-free read of the register at `ipa`.
    fn peek(ipa: u64) -> u32 {
        Self::read(ipa)
    }
}

/// First matching window for `ipa`, per the manifest's declared order
/// (finer windows precede the `HW_WINDOW` catch-all).
fn window_for(ipa: u64) -> Option<&'static MmioWindow> {
    layout::MMIO_WINDOWS.iter().find(|w| w.contains(ipa))
}

/// True when `ipa` falls in a serial-model window — the byte-addressed
/// peripheral class whose sub-word accesses bypass the BE-8 lane
/// transform (see [`read`]/[`write`]).
#[cfg(not(nh_guest_test))]
fn is_serial_window(ipa: u64) -> bool {
    matches!(
        window_for(ipa),
        Some(w) if matches!(w.policy, MmioPolicy::Peripheral(PeriphId::Serial))
    )
}

/// Closed dispatch: window's `PeriphId` → model read.
fn periph_read(id: PeriphId, ipa: u64) -> u32 {
    match id {
        PeriphId::Vic => Vic::read(ipa),
        PeriphId::Dma => Dma::read(ipa),
        PeriphId::Pcmcia => Pcmcia::read(ipa),
        PeriphId::Serial => Serial::read(ipa),
        PeriphId::Asic => Asic::read(ipa),
    }
}

/// Closed dispatch: window's `PeriphId` → model peek (side-effect-free).
#[cfg(not(nh_guest_test))]
fn periph_peek(id: PeriphId, ipa: u64) -> u32 {
    match id {
        PeriphId::Vic => Vic::peek(ipa),
        PeriphId::Dma => Dma::peek(ipa),
        PeriphId::Pcmcia => Pcmcia::peek(ipa),
        PeriphId::Serial => Serial::peek(ipa),
        PeriphId::Asic => Asic::peek(ipa),
    }
}

/// Closed dispatch: window's `PeriphId` → model write.
fn periph_write(id: PeriphId, ipa: u64, value: u32) {
    match id {
        PeriphId::Vic => Vic::write(ipa, value),
        PeriphId::Dma => Dma::write(ipa, value),
        PeriphId::Pcmcia => Pcmcia::write(ipa, value),
        PeriphId::Serial => Serial::write(ipa, value),
        PeriphId::Asic => Asic::write(ipa, value),
    }
}

pub fn read(ctx: &crate::arch::trap_context::TrapContext, ipa: u64, sas: u8, elr: u64) -> u32 {
    // BE-8 (production builds): byte/halfword accesses from the guest
    // land at the natural IPA (the CPU does the byte-lane transform
    // itself). Guest-test builds run the guest LE under the legacy
    // inline-patch path, where inline-stub byte/halfword accesses are
    // pre-XOR'd by 3/2; un-XOR here.
    #[cfg(nh_guest_test)]
    let ipa = be8::unxor_sub_word(ipa, sas);

    // BE-8 sub-word reads. Two peripheral classes with two
    // sub-word conventions, both taken from Einstein's `TMemory::ReadBP`
    // as the oracle.
    #[cfg(not(nh_guest_test))]
    if sas < 2 {
        // Byte-addressed peripherals (the serial windows) model each
        // register at its byte offset and return the register byte
        // directly — Einstein's `TMemory::ReadBP` dispatches a serial
        // byte read straight to `ReadRegister(offset)`
        // (TMemory.cpp:1518-1541), e.g. status reg 0x4400 → 0x80, with
        // NO BE-8 lane transform. Pass these through to the natural
        // offset and mask to the sub-word width.
        if is_serial_window(ipa) {
            return be8::mask_for_size(read_word(ctx, ipa, elr, sas), sas);
        }
        // Word-addressed peripherals hold genuine 32-bit registers; a
        // guest LDRB at lane 0 under BE-8 observes bits[31:24] — the
        // same lane the write splice (`be8::splice_byte`) targets, so
        // write-then-read of a single byte round-trips. Read the
        // aligned word side-effect-free and extract the addressed lane.
        let aligned = ipa & !0x3;
        let word = match peek_word(aligned) {
            Some(w) => w,
            // The aligned word isn't in any window; fall through to the
            // full read path so the unknown-address case still halts
            // loudly.
            None => return be8::mask_for_size(read_word(ctx, aligned, elr, sas), sas),
        };
        return be8::extract_sub_word(word, ipa, sas);
    }

    be8::mask_for_size(read_word(ctx, ipa, elr, sas), sas)
}

/// Word-granular read of a modelled register, side effects included.
/// `read` (above) handles the sub-word lane transform on top of this.
/// Halts loudly on a genuinely-unknown address. `sas` is forwarded for
/// the halt label; the modelled-register dispatch itself is
/// word-granular.
fn read_word(ctx: &crate::arch::trap_context::TrapContext, ipa: u64, elr: u64, sas: u8) -> u32 {
    match window_for(ipa).map(|w| w.policy) {
        Some(MmioPolicy::Peripheral(id)) => periph_read(id, ipa),
        Some(MmioPolicy::ReadZeroDropWrite) => 0,
        Some(MmioPolicy::HaltUnknown) | None => halt_on_unknown(ctx, "read", ipa, sas, 0, elr),
    }
}

/// Side-effect-free peek of the word at `ipa`, used by the sub-word
/// write splice and the sub-word read extraction. Unlike
/// `read_word` it (a) routes through each model's `peek` (so e.g. the
/// ROM-serial-chip bit index doesn't advance, and write-only ASIC
/// registers report 0 instead of misfiring the unknown-read halt), and
/// (b) returns `None` for an IPA outside every window rather than
/// halting — the caller decides what to do with an unknown aligned
/// word.
#[cfg(not(nh_guest_test))]
fn peek_word(ipa: u64) -> Option<u32> {
    match window_for(ipa).map(|w| w.policy) {
        Some(MmioPolicy::Peripheral(id)) => Some(periph_peek(id, ipa)),
        Some(MmioPolicy::ReadZeroDropWrite) => Some(0),
        Some(MmioPolicy::HaltUnknown) | None => None,
    }
}

pub fn write(
    ctx: &crate::arch::trap_context::TrapContext,
    ipa: u64,
    sas: u8,
    value: u32,
    elr: u64,
) {
    // BE-8 (production): byte/halfword accesses land at the natural
    // IPA. Splice the sub-word value into the addressed lane of the
    // surrounding word so the peripheral, which dispatches at word-
    // aligned register addresses, sees the full register's post-write
    // state. Guest-test mode keeps the legacy un-XOR path.
    #[cfg(nh_guest_test)]
    let ipa = be8::unxor_sub_word(ipa, sas);
    // Byte-addressed peripherals (the serial windows) pass the sub-word
    // value through unspliced at its natural offset — Einstein's
    // `TMemory::WriteBP` dispatches a serial byte write straight to
    // `WriteRegister(offset, inByte)` (TMemory.cpp:2435-2457), with NO
    // BE-8 lane transform. `serial::write` consumes the low byte
    // directly, matching the symmetric byte read above.
    #[cfg(not(nh_guest_test))]
    let (ipa, value) = if sas >= 2 || is_serial_window(ipa) {
        (ipa, value)
    } else {
        // Side-effect-free read of the surrounding word.
        // None → the aligned word is outside every window; splice onto
        // 0 and let the write dispatch below halt loudly with the
        // spliced value.
        let aligned = ipa & !0x3;
        let prev = peek_word(aligned).unwrap_or(0);
        let spliced = match sas {
            0 => be8::splice_byte(prev, ipa, value),
            _ => be8::splice_halfword(prev, ipa, value),
        };
        (aligned, spliced)
    };
    // Tick-page sub-word write catch-net. The tick page at
    // `layout::TICK_PAGE_IPA` is stage-2 RO (see
    // `stage2::install_tick_page`). Under BE-8 the original sub-word
    // write may have been spliced into a word at this point, but the
    // address still lies in the tick page; halt so we notice if any
    // guest code legitimately writes here. Fix when / if it fires:
    // route through `backed_*_write` on `stage2::TICK_PAGE`.
    if sas < 2 && (layout::TICK_PAGE_IPA..layout::TICK_PAGE_IPA + 0x1000).contains(&ipa) {
        kprintln!();
        kprintln!(
            "*** tick-page sub-word write reached mmio::write — \
             IPA={:#010x} size={} value={:#010x} @ELR={:#x}",
            ipa,
            sas_label(sas),
            value,
            elr
        );
        kprintln!(
            "  (inline stub wrote to stage-2 RO tick page. See the \
             'MMIO routing' section of the inline-stub plan — route \
             back through backed_*_write on stage2::TICK_PAGE.)"
        );
        cpu::halt();
    }
    match window_for(ipa).map(|w| w.policy) {
        Some(MmioPolicy::Peripheral(id)) => periph_write(id, ipa, value),
        Some(MmioPolicy::ReadZeroDropWrite) => {}
        Some(MmioPolicy::HaltUnknown) | None => halt_on_unknown(ctx, "write", ipa, sas, value, elr),
    }
}

fn sas_label(sas: u8) -> &'static str {
    match sas {
        0 => "B",
        1 => "H",
        2 => "W",
        _ => "?",
    }
}

/// Per Phase A's "instrument every unknown thing" rule, any IPA that
/// lands in a `HaltUnknown` window (or outside every window) halts
/// here with full context. Silent drops mask exactly the divergence
/// we're trying to see — a guest write to a dropped IPA whose value
/// the kernel later reads back is one of the most common ways a
/// run-away Thumb / bad-function-pointer bug slips in. Extend the
/// peripheral models (or add a window) to service the IPA this halts
/// on.
fn halt_on_unknown(
    ctx: &crate::arch::trap_context::TrapContext,
    op: &'static str,
    ipa: u64,
    sas: u8,
    value: u32,
    elr: u64,
) -> ! {
    let width = match sas {
        0 => "B",
        1 => "H",
        2 => "W",
        _ => "D",
    };
    let region = if layout::HW_WINDOW.contains(ipa) {
        "inside 0x0F00_0000..0x0F40_0000 (Newton hardware window — add to a peripheral model)"
    } else {
        "outside known windows (unmapped IPA — decide whether to model it or widen stage-2)"
    };
    // Raw sysreg readback. These survive across the trap entry on
    // AArch64 (handler runs at EL2 and nothing in the dispatch path
    // before us writes them). On real silicon FAR_EL1 can carry
    // uninitialised junk if the guest hasn't taken a stage-1 fault
    // yet, so it's printed raw rather than synthesised into the IPA.
    let (esr, hpfar, far_el2, far_el1, spsr) = unsafe {
        let (a, b, c, d, e): (u64, u64, u64, u64, u64);
        core::arch::asm!(
            "mrs {0}, esr_el2",
            "mrs {1}, hpfar_el2",
            "mrs {2}, far_el2",
            "mrs {3}, far_el1",
            "mrs {4}, spsr_el2",
            out(reg) a, out(reg) b, out(reg) c, out(reg) d, out(reg) e,
            options(nomem, nostack, preserves_flags),
        );
        (a, b, c, d, e)
    };
    kprintln!();
    kprintln!("*** unknown MMIO {} halted ***", op);
    kprintln!(
        "  IPA    = {:#010x}  {}  value={:#010x}  @ELR={:#x}",
        ipa,
        width,
        value,
        elr
    );
    kprintln!("  region: {}", region);
    kprintln!("  guest GPRs (AArch64 view, x[0..15] alias AArch32 r0..r15):");
    for row in 0..4 {
        let i = row * 4;
        kprintln!(
            "    r{:02}={:#018x}  r{:02}={:#018x}  r{:02}={:#018x}  r{:02}={:#018x}",
            i,
            ctx.x[i],
            i + 1,
            ctx.x[i + 1],
            i + 2,
            ctx.x[i + 2],
            i + 3,
            ctx.x[i + 3],
        );
    }
    kprintln!("  raw sysregs:");
    kprintln!("    ESR_EL2   = {:#018x}", esr);
    kprintln!(
        "    HPFAR_EL2 = {:#018x}  (FIPA<<8; IPA[51:12]={:#x})",
        hpfar,
        (hpfar >> 4) & 0xFFFFFFFFFF
    );
    kprintln!("    FAR_EL2   = {:#018x}", far_el2);
    kprintln!(
        "    FAR_EL1   = {:#018x}  (may be junk if guest hasn't taken a stage-1 fault yet)",
        far_el1
    );
    kprintln!("    SPSR_EL2  = {:#018x}", spsr);
    kprintln!(
        "  (Phase A contract: every unknown sub-case is a loud trip-wire, not a silent stub.)"
    );
    cpu::halt();
}
