//! ARM FVP_Base_RevC-2xAEMvA.
//!
//! Boot recipe lives in `baremetal/scripts/fvp` (and the user's
//! `reference_fvp_base_revc.md` memory). Summary: RVBAR=0x80000000,
//! has_el3=1, secure_memory=0, PL011 UART0 enabled, stdout.
//!
//! We run the model with `has_el3=1`, so the CPU resets into EL3 and
//! `boot.s` runs a minimal EL3 stub (wake the GICv3 redistributor,
//! set GICD_CTLR.DS, program CNTFRQ/CNTCR, then ERET to NS-EL2) before
//! the hypervisor proper starts at EL2. `boot.s` is the ground truth
//! for that sequence — it branches on `CurrentEL`, so the same image
//! also boots a `has_el3=0` model by entering directly at EL2, but the
//! shipped `scripts/fvp` invocation uses `has_el3=1`.
//!
//! The hypervisor code and the guest ROM live in the non-secure DRAM
//! that starts at 0x80000000. Device windows of interest are UART0 at
//! 0x1C090000 and GICv3 at 0x2F000000.

pub const NAME: &str = "FVP_Base_RevC-2xAEMvA (AEMv8/v9-A, cluster0.cpu0, EL2)";

/// PL011 UART0 on the Base Platform motherboard.
pub const UART_BASE: usize = 0x1C09_0000;

/// FVP's PL011 ignores the baud clock when `untimed_fifos=1` (the default)
/// — characters flush immediately. We still program sensible divisors so
/// the driver behaves the same on a hypothetical real board.
pub const UART_CLOCK_HZ: u32 = 14_745_600;

/// Device-nGnRE window covering PL011 UARTs (0x1C09_0000), system regs
/// (0x1C01_0000), VRAM (0x1800_0000), and the GICv3 at 0x2F00_0000.
/// Conservative: map the low 1 GiB of the Base Platform's MMIO aperture
/// as Device so we don't need finer-grained tables for peripherals we
/// haven't called out yet.
pub const DEVICE_MMIO_START: u64 = 0x0800_0000;
pub const DEVICE_MMIO_END: u64 = 0x4000_0000;

/// FVP's L1[1] (0x4000_0000..0x8000_0000) is unmapped in the Base
/// Platform NS view; no second Device region needed.
pub const DEVICE_MMIO_1GIB_BLOCK: Option<u64> = None;

/// DRAM lives at 0x8000_0000 (L1[2]); the L2 covering 0..1 GiB doesn't
/// reach it, so we install a dedicated 1 GiB Normal WB block here. The
/// hypervisor image, the embedded Newton ROM, page tables, and the
/// guest's IPA backing all sit inside this block.
pub const DRAM_1GIB_BLOCK: Option<u64> = Some(0x8000_0000);

/// On FVP, CNTFRQ_EL0 reports the real 100 MHz generic-timer rate and
/// CNTPCT_EL0 advances with wall time (within a few percent). Report
/// Newton ticks at the hardware-accurate 3.6864 MHz — no scaling fudge
/// needed.
pub const NEWTON_TICK_HZ: u64 = 3_686_400;

/// Route CNTHP to the CPU's IRQ input via the GICv3. Brings the whole
/// GIC up (ICC_SRE_EL2, distributor, redistributor, CPU interface)
/// from EL2 because there is no TF-A / secure OS to do it — `boot.s`'s
/// EL3 stub only wakes the redistributor and sets GICD_CTLR.DS so the
/// EL2 path here can reach the GICR_* / distributor registers. See
/// `super::gicv3` for the bare-metal initialisation sequence.
pub fn install_cnthp_irq_routing() {
    super::gicv3::init();
    super::gicv3::enable_ppi(super::gicv3::INTID_CNTHP);
}

/// Ack the currently-asserted IRQ and return its INTID. Called at the
/// top of the EL2 IRQ handler before touching any physical state.
#[inline]
pub fn irq_ack() -> u32 {
    super::gicv3::ack()
}

/// End-of-interrupt for an INTID previously returned by `irq_ack`.
/// Deasserts the CPU's IRQ line and re-arms the GIC for the next one.
#[inline]
pub fn irq_eoi(intid: u32) {
    super::gicv3::eoi(intid);
}

/// Spurious INTID (no pending interrupt). The handler skips `on_irq`
/// but still runs the common tail when it sees this.
#[inline]
pub fn irq_spurious() -> u32 {
    super::gicv3::INTID_SPURIOUS
}

/// EL2 IRQ-entry dispatch hooks. The FVP host has no BCM2835 DMA engine
/// or USB touchscreen — those are real-Pi-only — so both are no-ops.
/// They exist so the IRQ path in `trap` is free of platform cfg blocks
/// (the BCM2835 versions live in `raspi3b.rs`).
#[inline]
pub fn dispatch_dma_completions(_cap: crate::arch::slim_isr::IrqCap) {}

#[inline]
pub fn poll_usb_fast_path() -> super::UsbFastPath {
    super::UsbFastPath::NotUsb
}

/// Early per-platform CPU sysreg fixups before we touch anything that
/// reads them. On FVP with `has_el3=1` (our chosen config — see
/// `scripts/fvp` and the EL3 stub in `boot.s` for why), CNTFRQ_EL0 is
/// writable only at EL3; the stub programs it before ERETing to EL2,
/// so this is a no-op.
pub fn init_cpu_sysregs() {}
