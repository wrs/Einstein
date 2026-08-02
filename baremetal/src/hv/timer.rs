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
//! Routing the CNTHP PPI to the CPU's IRQ input is host-specific:
//!   raspi3b  — BCM2836 per-core "ARM local" peripheral at 0x4000_0040.
//!   fvp-base — GICv3 (brought up by `platform::fvp_base`, which calls
//!              `gicv3::init` + `enable_ppi(INTID_CNTHP)`).
//! See `crate::host::platform::install_cnthp_irq_routing`.

use crate::{kprintln, host::platform};
use crate::hv::hooks::{ActiveGuest, GuestOs};

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
    // line. Implementation differs per host (BCM local peripheral vs
    // GICv3); without it the timer signal fires internally but never
    // reaches the IRQ pin.
    platform::install_cnthp_irq_routing();

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

    // Kick off the heartbeat so the EL2 IRQ handler gets control
    // periodically even before the guest arms any Newton match.
    rearm();
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

/// Reprogram CNTHP_CVAL_EL2 for the next 16 ms heartbeat. The
/// heartbeat exists purely to give the EL2 IRQ vector control
/// periodically — primarily so `tick_page::update` runs even when the
/// guest is in a non-trapping loop, and so `poll_timer_matches` runs
/// even when no sync trap has fired recently.
///
/// We do *not* arm against a specific Newton-tick match deadline.
/// With instruction-anchored synthetic ticks (see
/// `vic::SYNTH_TICKS`) there is no fixed wall-time → tick mapping,
/// so a wall-anchored CNTPCT deadline can't be derived from a Newton
/// tick value. Instead, every sync trap advances synthetic ticks via
/// `tick_advance` and runs `poll_timer_matches` — match deliveries
/// happen at sync-trap granularity, which is plenty fine for the
/// kernel's preemption / alarm cadence.
pub fn rearm() {
    let cnt_hz = read_cntfrq();
    // SAFETY: read-only sysreg.
    let now: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
            options(nomem, nostack, preserves_flags));
    }
    let heartbeat_cval = now.wrapping_add(cnt_hz / 64); // ~16 ms
    program_cval(heartbeat_cval);
}

/// Called from the EL2 IRQ vector on any physical-IRQ delivery. We only
/// wire up CNTHP, so any IRQ here is a heartbeat expiry: drive forward
/// progress for any guest parked in WFI on a Newton-match deadline,
/// refresh the non-trapping tick page, latch crossed matches, and rearm
/// for the next heartbeat. trap.rs's shared `update_virq` then sets
/// HCR_EL2.VI for delivery to the guest.
///
/// Takes a [`crate::arch::slim_isr::IrqCap`] so it can only be reached from the
/// EL2 IRQ-vector path, never from a `cpu::with_irqs_unmasked` window —
/// see `slim_isr` for the ownership contract.
pub fn on_irq(_cap: crate::arch::slim_isr::IrqCap) {
    // Stale-TLB guard. The hypervisor rewrites guest stage-1 PTEs
    // behind the guest's back (fix_stage1_xn_bits, the shadow-stub
    // alias redirects, the scratch-pool install) without targeted TLB
    // maintenance at the rewrite sites — and the guest can't TLBI
    // after writes it never made. Flushing the EL1&0 stage-1 TLB
    // here bounds any stale entry's lifetime to one ~16 ms
    // heartbeat. Replacing this blanket flush with targeted TLBIs at
    // each rewrite site is tracked in PLAN.md.
    crate::hv::trap::cp15::invalidate_tlb();

    // Guest-OS heartbeat body: advance the tick model for any guest
    // parked in WFI / a non-trapping busy-wait, latch crossed matches,
    // and republish the tick page. See the `on_heartbeat` hook impl.
    ActiveGuest::on_heartbeat();
    // The match that woke us is now latched in the guest interrupt
    // model; rearm for the next 16 ms heartbeat so we don't re-fire
    // immediately.
    rearm();
}
