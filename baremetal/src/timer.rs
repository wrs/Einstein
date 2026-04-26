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
//!   fvp-base — GICv3 (TODO; currently a no-op — see platform::fvp_base).
//! See `crate::platform::install_cnthp_irq_routing`.

use crate::{kprintln, peripherals::vic, platform};

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
///
/// Also clamps the deadline to a 16 ms fallback heartbeat even if the VIC
/// match is far in the future. The non-trapping tick page
/// (`stage2::TICK_PAGE`) only advances when `stage2::tick_page::update()`
/// runs, and that is driven off the CNTHP IRQ — if CNTHP is only armed
/// for rare VIC matches the guest's busy-wait delay loops would spin
/// forever on a stale tick value. 16 ms (~60 Hz) is fast enough that
/// early calibration loops see at least one tick update per poll cycle,
/// and slow enough to keep trace volume manageable once the kernel is
/// past early boot.
pub fn rearm() {
    let cnt_hz = read_cntfrq();
    // SAFETY: read-only sysreg.
    let now: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
            options(nomem, nostack, preserves_flags));
    }
    let heartbeat_cval = now.wrapping_add(cnt_hz / 64); // ~16 ms
    let cval = match vic::next_pending_match() {
        Some(t) => {
            let vic_cval = newton_ticks_to_cntpct(t);
            // Fire whichever deadline comes first.
            if vic_cval.wrapping_sub(now) < heartbeat_cval.wrapping_sub(now) {
                vic_cval
            } else {
                heartbeat_cval
            }
        }
        None => heartbeat_cval,
    };
    program_cval(cval);
}

/// Called from the EL2 IRQ vector on any physical-IRQ delivery. We only
/// wire up CNTHP, so any IRQ here is a timer expiry: latch whatever Newton
/// matches have been crossed, refresh the non-trapping tick page, rearm
/// for the next deadline, and let trap.rs's shared `update_virq` set
/// HCR_EL2.VI for delivery to the guest.
pub fn on_irq() {
    // Re-evaluate the Newton VIC and decide which matches have crossed
    // their threshold.
    vic::poll_timer_matches();
    // RTC alarm shares the heartbeat: latch INT_RTC_ALARM if the wall-
    // clock calendar has crossed the alarm value. Edge-detect inside
    // poll_alarm prevents re-firing.
    vic::poll_alarm();
    // Refresh the non-trapping tick register so the guest's busy-wait
    // delay loops observe a fresh counter value on their next load.
    // Without this the loops at BootOS:0x19FCC / 0x18F38 would spin
    // against a stale tick value until the next IRQ fired for some
    // other reason.
    crate::stage2::tick_page::update();
    // The match that woke us is now latched in vic::int_present; rearm
    // for the next pending deadline so we don't re-fire immediately.
    rearm();
}
