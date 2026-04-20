//! EL2 physical timer driver (CNTHP) for async Newton-match delivery.
//!
//! We use the A53 generic timer's EL2 physical-timer channel (CNTHP) to fire
//! an IRQ at the CNTPCT_EL0 value that corresponds to the nearest pending
//! Newton timer-match register. When the guest writes a match register, we
//! recompute the nearest deadline and reprogram CNTHP_CVAL_EL2. When the
//! timer fires, the IRQ lands at the EL2 vector table; the handler latches
//! the match into `vic::int_present`, sets HCR_EL2.VI if appropriate, and
//! reprograms CNTHP for the next deadline.
//!
//! On the BCM2836/2837 (Pi 3B, Pi Zero 2 W, QEMU raspi3b) the CNTHP PPI is
//! routed through the per-core "ARM local" peripheral at 0x4000_0000 rather
//! than a GIC. We program the core-0 timer IRQ-control register to route
//! CNTHPIRQ to the IRQ input.
//!
//! The older "poll on every sync trap" path in `trap.rs` is left in place as
//! a safety net for machines where the generic timer IRQ isn't wired up, but
//! correctness no longer depends on it: a guest sitting in WFI with no MMIO
//! traffic will still receive timer interrupts.

use crate::{kprintln, vic};

/// BCM2836 per-core timer IRQ-control register, core 0. Bit layout:
///   [0] CNTPSIRQ  -> IRQ
///   [1] CNTPNSIRQ -> IRQ
///   [2] CNTHPIRQ  -> IRQ     <- we set this
///   [3] CNTVIRQ   -> IRQ
///   [4..7] same sources routed to FIQ
const BCM_LOCAL_CORE0_TIMER_IRQCNTL: *mut u32 = 0x4000_0040 as *mut u32;
const BCM_CNTHPIRQ_IRQ: u32 = 1 << 2;

/// CNTHP_CTL_EL2: ENABLE=1, IMASK=0 → timer fires an IRQ when
/// CNTPCT_EL0 crosses CNTHP_CVAL_EL2.
const CNTHP_CTL_ENABLE: u64 = 1 << 0;

/// A CNTPCT_EL0 value far enough in the future that it won't fire for the
/// life of any realistic boot. Used when there's no pending Newton match to
/// schedule against — the timer stays armed but effectively idle.
const CVAL_FAR_FUTURE: u64 = u64::MAX / 2;

fn read_cntfrq() -> u64 {
    let v: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

/// Enable the EL2 physical timer with IMASK cleared, arm it far in the
/// future, and route the CNTHPIRQ PPI to the core's IRQ input. Safe to call
/// once, from kmain before the first guest ERET.
pub fn init() {
    // Route CNTHPIRQ (PPI from the EL2 physical timer) to the core's IRQ
    // line via the BCM2836 per-core timer control register. Without this
    // the timer signal fires internally but never reaches the IRQ pin.
    // SAFETY: the 1 GiB block covering 0x40000000 is mapped Device-nGnRE
    // by mmu::init().
    unsafe {
        BCM_LOCAL_CORE0_TIMER_IRQCNTL.write_volatile(BCM_CNTHPIRQ_IRQ);
    }

    // Start with the deadline far in the future and IMASK clear so the
    // timer never accidentally fires before the first guest match arm.
    program_cval(CVAL_FAR_FUTURE);

    // SAFETY: enabling CNTHP doesn't affect current execution until CVAL
    // is reached.
    unsafe {
        core::arch::asm!(
            "msr cnthp_ctl_el2, {}",
            "isb",
            in(reg) CNTHP_CTL_ENABLE,
            options(nostack, preserves_flags),
        );
    }

    kprintln!(
        "timer: CNTHP armed, CNTFRQ={} Hz, CNTHPIRQ -> core0 IRQ",
        read_cntfrq()
    );
}

fn program_cval(cval: u64) {
    // SAFETY: EL2 sysreg write. No side effects other than rearming the timer.
    unsafe {
        core::arch::asm!(
            "msr cnthp_cval_el2, {}",
            "isb",
            in(reg) cval,
            options(nostack, preserves_flags),
        );
    }
}

/// Translate a Newton-domain tick value into the CNTPCT_EL0 domain, given
/// the epoch captured by `vic::init()`.
fn newton_ticks_to_cntpct(newton_ticks: u32) -> u64 {
    let epoch_cntpct = vic::timer_epoch();
    let newton_hz = vic::NEWTON_TICK_HZ as u128;
    let cnt_hz = read_cntfrq() as u128;
    // `newton_ticks` is a 32-bit value in the guest's ticks domain; we don't
    // try to handle wraparound across the full 64-bit range here — the
    // Newton kernel rearms match_reg often enough that this is fine.
    let scaled = (newton_ticks as u128 * cnt_hz) / newton_hz;
    epoch_cntpct.wrapping_add(scaled as u64)
}

/// Recompute the nearest pending Newton match and reprogram CNTHP_CVAL_EL2.
/// Called after any write to match_reg[i] or int_ctrl, and from the IRQ
/// handler after clearing a fired match bit.
pub fn rearm() {
    let next_match = match vic::next_pending_match() {
        Some(t) => t,
        None => {
            program_cval(CVAL_FAR_FUTURE);
            return;
        }
    };
    let cval = newton_ticks_to_cntpct(next_match);
    program_cval(cval);
}

/// Called from the EL2 IRQ vector on any physical-IRQ delivery. We only
/// wire up CNTHP, so any IRQ here is a timer expiry: latch whatever Newton
/// matches have been crossed, rearm for the next one, and let trap.rs's
/// shared `update_virq` set HCR_EL2.VI for delivery to the guest.
pub fn on_irq() {
    // Re-evaluate the Newton VIC and decide which matches have crossed
    // their threshold.
    vic::poll_timer_matches();
    // The match that woke us is now latched in vic::int_present; rearm
    // for the next pending deadline so we don't re-fire immediately.
    rearm();
}
