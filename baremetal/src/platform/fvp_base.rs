//! ARM FVP_Base_RevC-2xAEMvA.
//!
//! Boot recipe lives in `baremetal/scripts/fvp` (and the user's
//! `reference_fvp_base_revc.md` memory). Summary: RVBAR=0x80000000,
//! has_el3=0, secure_memory=0, PL011 UART0 enabled, stdout.
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

/// Route CNTHP to the CPU's IRQ input.
///
/// TODO(fvp): this is a stub until the GICv3 init lands. Booting
/// guest-tests that never arm a timer works without it; anything that
/// depends on CNTHP delivery (the 1 ms tick-page heartbeat, guest
/// Newton match registers) will stall here until we wire up:
///   - GICD_CTLR.EnableGrp1NS
///   - GICR_WAKER (clear ProcessorSleep, wait ChildrenAsleep)
///   - GICR_ISENABLER0 bit 26 (CNTHP PPI, INTID 26)
///   - GICR_IPRIORITYR[26], GICR_IGROUPR0 bit 26
///   - ICC_PMR_EL1, ICC_IGRPEN1_EL1
pub fn install_cnthp_irq_routing() {
    // No-op for now — see TODO above.
}

/// Early per-platform CPU sysreg fixups before we touch anything that
/// reads them. FVP boots with `has_el3=0` so there's no secure firmware
/// to program CNTFRQ_EL0; the generic-timer counter ticks at the Base
/// Platform's architectural 100 MHz regardless, so we publish that
/// rate to software. Writable from EL2 because EL2 is the highest
/// implemented exception level.
pub fn init_cpu_sysregs() {
    const CNTFRQ_HZ: u64 = 100_000_000;
    // SAFETY: sysreg write with no memory side effects; CNTFRQ_EL0 is
    // writable at EL2 when EL3 is not implemented.
    unsafe {
        core::arch::asm!(
            "msr cntfrq_el0, {}",
            "isb",
            in(reg) CNTFRQ_HZ,
            options(nostack, preserves_flags),
        );
    }
}
