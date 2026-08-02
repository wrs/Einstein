//! Slim-ISR state ownership.
//!
//! # The contract, stated once
//!
//! An IRQ taken while EL2 hypervisor code is running — at boot before
//! guest entry, or nested inside a [`cpu::with_irqs_unmasked`] window in
//! a trap handler — is serviced by the *slim* same-EL ISR
//! [`trap::irq_from_el2`]. That path runs nested inside *any* other EL2
//! handler, so it must touch only the bounded set of state it owns:
//!
//!   - VIC tick/match state, via [`timer::on_irq`] (latches crossed
//!     match bits into `vic::int_present`, rearms `CNTHP_CVAL_EL2`).
//!   - host_dma channel CS registers, via `host_dma::on_completion`,
//!     reached through [`platform::dispatch_dma_completions`]; and the
//!     leaf consumers it fans out to — the uart TX ring tail
//!     (`console::on_tx_done`), the audio MAI ring + stereo ring tail +
//!     `vic::raise` (`audio::on_mai_dma_done`), and the SDHOST
//!     controller + flash-persist DMA-save state machine
//!     (`flash_persist::on_sd_dma_done`).
//!   - the GIC CPU interface, via `platform::irq_ack` / `irq_eoi`
//!     (idempotent hardware acknowledge).
//!   - `kprintln`'s own uart ring (it masks IRQs around its critical
//!     section, so it is re-entrant-safe from here).
//!
//! The flip side of the same rule: code running inside
//! [`cpu::with_irqs_unmasked`] must not touch any of the above, because a
//! nested slim ISR may mutate it concurrently.
//!
//! # How the compiler enforces it
//!
//! The two dispatch entry points that *are* slim-ISR-exclusive —
//! [`timer::on_irq`] and [`platform::dispatch_dma_completions`] — each
//! require an [`IrqCap`] argument. An `IrqCap` is a zero-sized token
//! whose only constructor, [`IrqCap::mint`], is `unsafe` and documented
//! to be called from exactly one place: the EL2 IRQ vector entry
//! [`trap::trap_irq`]. A [`cpu::with_irqs_unmasked`] closure has no
//! `IrqCap` in scope, so it *cannot* call the slim-ISR dispatch — the
//! call fails to compile for want of the token. Fabricating one would
//! require writing `unsafe { IrqCap::mint() }`, an auditable, deliberate
//! act, not an accident.
//!
//! The leaf consumers (`console::on_tx_done`, `audio::on_mai_dma_done`,
//! `flash_persist::on_sd_dma_done`, `vic::raise`) are reached *through*
//! `host_dma::on_completion` from the IRQ path, but are also legitimately
//! called from non-IRQ contexts (the uart producer before IRQs are
//! unmasked, audio backend internals, the GPIO/power raisers). They are
//! therefore not token-gated; the gate sits at the IRQ dispatch boundary,
//! which is the boundary the contract is actually about.
//!
//! [`cpu::with_irqs_unmasked`]: crate::arch::cpu::with_irqs_unmasked
//! [`trap::irq_from_el2`]: crate::hv::trap
//! [`trap::trap_irq`]: crate::hv::trap::trap_irq
//! [`timer::on_irq`]: crate::hv::timer::on_irq
//! [`platform::dispatch_dma_completions`]: crate::host::platform::dispatch_dma_completions

/// Capability token proving the caller is running on the EL2 IRQ-vector
/// path. Required by the slim-ISR dispatch entry points
/// (`timer::on_irq`, `platform::dispatch_dma_completions`) so that EL2
/// code running inside a `cpu::with_irqs_unmasked` window — which holds
/// no token — cannot reach the state the slim ISR owns. Zero-sized:
/// `Copy` so a single mint can be handed to both the guest-path and
/// slim-path bodies and on to their dispatch calls.
#[derive(Clone, Copy)]
pub struct IrqCap(());

impl IrqCap {
    /// Mint the token at the EL2 IRQ-vector entry.
    ///
    /// # Safety
    ///
    /// Call this from exactly one place — [`crate::hv::trap::trap_irq`], the
    /// EL2 IRQ vector entry — and never from EL2 code that runs inside a
    /// `cpu::with_irqs_unmasked` window. Minting a token elsewhere
    /// defeats the slim-ISR ownership guarantee documented above.
    #[inline(always)]
    pub unsafe fn mint() -> Self {
        IrqCap(())
    }
}
