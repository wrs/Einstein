//! Wall-clock helpers on the generic timer. The firmware's armstub
//! programs CNTFRQ_EL0 (19.2 MHz on the Pi; QEMU uses 62.5 MHz) and
//! the counter runs from reset, so CNTPCT_EL0 is usable with the MMU
//! off and no setup of our own (ARM ARM D11.1 "The Generic Timer").

/// Microseconds since the counter started.
pub fn now_us() -> u64 {
    let freq: u64;
    let cnt: u64;
    // SAFETY: sysreg reads, side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) cnt,
            options(nomem, nostack, preserves_flags));
    }
    // A zero CNTFRQ means nothing programmed it. Rather than divide
    // by zero, assume the Pi's 19.2 MHz crystal — every timeout then
    // still fires, just possibly at a scaled rate — and the banner's
    // caller can't tell the difference on real hardware where the
    // armstub always sets it.
    let freq = if freq == 0 { 19_200_000 } else { freq };
    // cnt * 1e6 overflows u64 only after ~2^44 ticks (days at 62.5 MHz).
    cnt.wrapping_mul(1_000_000) / freq
}

/// Microseconds elapsed since `since` (a `now_us` value).
pub fn elapsed_us(since: u64) -> u64 {
    now_us().wrapping_sub(since)
}
