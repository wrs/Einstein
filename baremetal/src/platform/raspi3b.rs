//! QEMU raspi3b / real BCM2837 (Pi 3B, Pi Zero 2 W).
//!
//! Load address 0x80000 is set by linker.ld; this module declares only
//! what the running hypervisor code needs to read at runtime.

pub const NAME: &str = "Cortex-A53 / BCM2837 (Pi 3B, Zero 2 W, QEMU raspi3b)";

// PL011 UART0 on the BCM2837 peripheral window.
pub const UART_BASE: usize = 0x3F20_1000;
pub const UART_CLOCK_HZ: u32 = 48_000_000; // Default Pi firmware / QEMU clock.

/// Identity-map this PA window as Device-nGnRE (else Normal WB). On the Pi
/// this is the BCM2837 peripheral window; everything below is DRAM.
pub const DEVICE_MMIO_START: u64 = 0x3F00_0000;
pub const DEVICE_MMIO_END: u64 = 0x4000_0000;

/// A second Device-nGnRE region (BCM2836 per-core local peripheral), mapped
/// via a 1 GiB L1 block directly. `None` on platforms without a second
/// device window.
pub const DEVICE_MMIO_1GIB_BLOCK: Option<u64> = Some(0x4000_0000);

/// Optional 1 GiB Normal WB DRAM block at the given PA, mapped at L1
/// directly. `None` when DRAM falls inside the L2-table-covered region
/// (true for raspi3b — image + RAM live in L1[0]).
pub const DRAM_1GIB_BLOCK: Option<u64> = None;

/// Newton's hardware tick clock runs at 3.6864 MHz of wall time. We
/// report the natural rate; CNTPCT_EL0 (running at ~62 MHz on QEMU
/// raspi3b) is rate-converted in `vic::ticks()`.
///
/// Earlier code multiplied this by 16 (and before that 128) to make
/// early calibrated delay loops finish promptly. With the 16 ms
/// CNTHP heartbeat in `timer.rs` driving `tick_page::update()`, the
/// guest sees ticks advance even during tight non-trapping polls
/// without any rate scaling, so the multiplier is gone. Keeping the
/// natural rate matches what the kernel computes from `kFreqGenFreq`
/// and matches Einstein's `TInterruptManager::GetTimeInTicks`, which
/// avoids a class of divergence where wall-anchored fast ticks make
/// kernel-armed timer matches fire too early relative to guest
/// instruction throughput (every spurious alarm IRQ allocates from
/// the safe heap and perturbs subsequent allocations — see
/// INVESTIGATION.md).
pub const NEWTON_TICK_HZ: u64 = 3_686_400;

/// Route the EL2 physical timer PPI (CNTHPIRQ) to core 0's IRQ input.
///
/// The BCM2836 has no GIC; PPI delivery is done via a per-core local
/// peripheral at `0x4000_0040`. Without this write the timer fires
/// internally but the IRQ never reaches the CPU.
pub fn install_cnthp_irq_routing() {
    const BCM_LOCAL_CORE0_TIMER_IRQCNTL: *mut u32 = 0x4000_0040 as *mut u32;
    const BCM_CNTHPIRQ_IRQ: u32 = 1 << 2;
    // SAFETY: MMIO at fixed address; the 1 GiB block covering 0x40000000
    // is mapped Device-nGnRE by mmu::init() via DEVICE_MMIO_1GIB_BLOCK.
    unsafe { BCM_LOCAL_CORE0_TIMER_IRQCNTL.write_volatile(BCM_CNTHPIRQ_IRQ) }
}

/// Early per-platform CPU sysreg fixups before we touch anything that
/// reads them. On raspi3b both QEMU and Pi firmware program CNTFRQ_EL0
/// for us, so this is a no-op.
pub fn init_cpu_sysregs() {}

/// The BCM2836 local peripheral does not latch IRQs the way a GIC
/// does: asserting / deasserting happens purely in the timer
/// comparator, so there's no CPU-interface state to ACK or EOI. The
/// handler still calls these so the FVP GICv3 path can hook in.
#[inline]
pub fn irq_ack() -> u32 { 0 }

#[inline]
pub fn irq_eoi(_intid: u32) {}

/// BCM has no "spurious" INTID — the IRQ line is only asserted while a
/// real source is firing. `irq_ack` always returns 0, so we pick
/// u32::MAX as a sentinel the handler will never see from `irq_ack`.
#[inline]
pub fn irq_spurious() -> u32 { u32::MAX }

// ---- BCM2835 ARM interrupt controller -------------------------------
//
// CNTHP arrives via the BCM2836 local-peripheral block at 0x4000_0040
// (see `install_cnthp_irq_routing` above). Everything else — DMA
// completion, UART, GPIO, etc. — comes from the BCM2835 IRQ
// controller at ARM physical 0x3F00_B000 (peripheral bus
// 0x7E00_B000). Source-numbering convention (BCM2835 ARM Peripherals
// §7.5 p.112): sources 0..31 live in IRQ_PEND_1, 32..63 in IRQ_PEND_2.
// Enable bits sit at the same bit positions in ENABLE_IRQS_1/2 and
// are write-1-to-set (other bits preserved). Disable bits at
// DISABLE_IRQS_1/2 are write-1-to-clear.

const BCM2835_IC_BASE: usize = 0x3F00_B000;
const BCM2835_IC_PEND_1: *const u32 = (BCM2835_IC_BASE + 0x204) as *const u32;
const BCM2835_IC_PEND_2: *const u32 = (BCM2835_IC_BASE + 0x208) as *const u32;
const BCM2835_IC_ENABLE_1: *mut u32 = (BCM2835_IC_BASE + 0x210) as *mut u32;
const BCM2835_IC_ENABLE_2: *mut u32 = (BCM2835_IC_BASE + 0x214) as *mut u32;

/// Enable a single GPU interrupt source (0..63) at the BCM2835 IRQ
/// controller. Write-1-to-set: other enabled sources are preserved
/// (BCM2835 §7.5 p.116).
#[allow(dead_code)] // First caller is host_dma's init path.
pub fn enable_bcm2835_irq(src: u32) {
    assert!(src < 64);
    // SAFETY: MMIO write at a fixed peripheral address.
    unsafe {
        if src < 32 {
            core::ptr::write_volatile(BCM2835_IC_ENABLE_1, 1u32 << src);
        } else {
            core::ptr::write_volatile(BCM2835_IC_ENABLE_2, 1u32 << (src - 32));
        }
    }
}

/// Read the BCM2835 IRQ_PEND_1 register (sources 0..31). Returns the
/// enabled-AND-pending bitmask — the controller only sets pending bits
/// for sources whose enable bit is set, so the caller can dispatch
/// directly off this value (BCM2835 §7.5 p.115).
#[allow(dead_code)] // First caller is trap_irq's DMA dispatch.
#[inline]
pub fn bcm2835_irq_pending_1() -> u32 {
    // SAFETY: MMIO read at a fixed peripheral address.
    unsafe { core::ptr::read_volatile(BCM2835_IC_PEND_1) }
}

/// Read the BCM2835 IRQ_PEND_2 register (sources 32..63).
#[allow(dead_code)] // Reserved for future UART RX / HDMI sources.
#[inline]
pub fn bcm2835_irq_pending_2() -> u32 {
    // SAFETY: MMIO read at a fixed peripheral address.
    unsafe { core::ptr::read_volatile(BCM2835_IC_PEND_2) }
}

/// BCM2836 per-core IRQ source register for core 0 (local peripheral
/// block). CNTHPIRQ (the EL2 physical timer routed by
/// `install_cnthp_irq_routing`) shows up as bit 2 here — the same bit
/// position the routing register at +0x40 selects.
const BCM2836_CORE0_IRQ_SOURCE: *const u32 = 0x4000_0060 as *const u32;
const BCM2836_CNTHPIRQ: u32 = 1 << 2;

/// True if the EL2 hyp-timer IRQ (CNTHPIRQ) is currently asserted at
/// core 0's local-peripheral IRQ-source register. The line is level —
/// it stays set until `timer::on_irq` rearms CNTHP_CVAL_EL2 — so a
/// high-rate co-pending source (the USB interrupt-IN re-arm) can test
/// this to decide whether it may early-return without starving the
/// timer.
#[allow(dead_code)] // First caller is trap_irq's slim USB fast path.
#[inline]
pub fn cnthp_irq_pending() -> bool {
    // SAFETY: MMIO read; the 1 GiB block at 0x4000_0000 is mapped
    // Device-nGnRE via DEVICE_MMIO_1GIB_BLOCK.
    unsafe { core::ptr::read_volatile(BCM2836_CORE0_IRQ_SOURCE) & BCM2836_CNTHPIRQ != 0 }
}

// ---- EL2 IRQ-entry dispatch (BCM2835 pending-register decode) -------
//
// These two functions own the BCM2835-specific pending-register decode
// that used to be inlined as `#[cfg(nh_real_hw)]` blocks in
// `trap_irq` / `irq_from_*`. Keeping it here makes the IRQ path in
// `trap` free of platform cfg blocks: the platform layer (which already
// owns `irq_ack`/`irq_eoi`) now also owns the host IRQ-controller
// dispatch. On QEMU raspi3b (semihost, i.e. not `nh_real_hw`) there is
// no BCM2835 DMA engine to service, so both are no-ops.

/// Drain any completed BCM2835 DMA channels (UART TX ch5, MAI TX ch4,
/// SD TX) by reading IRQ_PEND_1 and forwarding each pending channel to
/// `host_dma::on_completion`. Called from both the slim same-EL ISR and
/// the guest-path IRQ body.
#[cfg(nh_real_hw)]
#[inline]
pub fn dispatch_dma_completions(_cap: crate::slim_isr::IrqCap) {
    use crate::host_dma;
    let pend1 = bcm2835_irq_pending_1();
    for &ch in &[
        host_dma::UART_TX_CHANNEL,
        host_dma::MAI_TX_CHANNEL,
        host_dma::SD_TX_CHANNEL,
    ] {
        if pend1 & (1u32 << (16 + ch)) != 0 {
            host_dma::on_completion(ch);
        }
    }
}

#[cfg(not(nh_real_hw))]
#[inline]
pub fn dispatch_dma_completions(_cap: crate::slim_isr::IrqCap) {}

/// Slim USB interrupt-IN fast path (real-hw touchscreen). The IRQ-driven
/// DWC2 channel re-arms every frame, so source 9 fires at up to ~1 kHz
/// (mostly NAKs) — far above the ~62 Hz the heavy guest-IRQ body is
/// built for. Harvest the report here, off that path, regardless of
/// interruptee, and report back whether the heavy body can be skipped.
/// `UsbOnly` is returned only when USB is the *sole* cause: the
/// level-triggered CNTHP timer and our DMA channels must still reach the
/// IRQ body, so we check them before signalling a skip. (CNTHP is level
/// — it simply re-fires if we returned too early — but we'd then spin on
/// every USB IRQ and starve it, so test it.)
#[cfg(nh_real_hw)]
#[inline]
pub fn poll_usb_fast_path() -> super::UsbFastPath {
    use crate::host_dma;
    let pend1 = bcm2835_irq_pending_1();
    if pend1 & (1 << 9) == 0 {
        return super::UsbFastPath::NotUsb;
    }
    let enqueued = crate::input::on_usb_irq();
    let other_bcm = pend1
        & ((1 << (16 + host_dma::UART_TX_CHANNEL))
            | (1 << (16 + host_dma::MAI_TX_CHANNEL))
            | (1 << (16 + host_dma::SD_TX_CHANNEL)));
    if other_bcm == 0 && !cnthp_irq_pending() {
        super::UsbFastPath::UsbOnly { enqueued }
    } else {
        super::UsbFastPath::UsbCoPending
    }
}

#[cfg(not(nh_real_hw))]
#[inline]
pub fn poll_usb_fast_path() -> super::UsbFastPath {
    super::UsbFastPath::NotUsb
}
