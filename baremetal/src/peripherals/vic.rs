//! Minimal Newton virtual interrupt controller + tick clock.
//!
//! Stores the state the ROM touches early and returns sensible values:
//!
//!   Ticks register (3.6864 MHz counter) — computed from the A53 generic
//!   timer (CNTPCT_EL0 scaled by CNTFRQ_EL0), reset on init. Reading this
//!   register is the main unblocker for the guest's early polling loop.
//!
//!   Interrupt enable/mask/control registers — stored as plain state;
//!   writes that change the delivery gate (`int_present & int_ctrl & ~fiq_mask`)
//!   are reflected into HCR_EL2.VI / VF on trap return.
//!
//!   Timer match registers — each write rearms the CNTHP_CVAL_EL2 deadline
//!   through `timer::rearm`. When the EL2 physical timer fires, the IRQ
//!   handler in `trap.rs` calls `poll_timer_matches` here to latch the
//!   crossed bit(s) into `int_present`, so the next `update_virq` sets VI.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ---------- Newton tick clock (3.6864 MHz). ----------------------------------

/// A53 CNTPCT_EL0 reading at the moment `init()` was called, captured so we
/// can report ticks as "time since hypervisor started guest", which matches
/// what the guest expects at reset.
static TICK_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Real Newton hardware clocks the tick register at 3.6864 MHz of wall
/// time. QEMU raspi3b's CNTPCT_EL0 advances at roughly 0.8 MHz of wall
/// time, so the raspi3b platform reports a scaled rate (3.6864 MHz × 128)
/// to keep boot-wall-time under a minute. FVP runs the generic timer at
/// the architectural 100 MHz, so it uses the unscaled rate. The choice
/// lives in `crate::platform::NEWTON_TICK_HZ`.
pub use crate::platform::NEWTON_TICK_HZ;

/// CNTPCT_EL0 reading captured at `init()`. Callers doing rate-conversion
/// between CNTPCT and Newton-tick domains anchor at this point (Newton
/// ticks = 0 by definition at the same moment).
pub fn timer_epoch() -> u64 {
    TICK_EPOCH.load(Ordering::Acquire)
}

fn read_cntpct() -> u64 {
    let v: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

fn read_cntfrq() -> u64 {
    let v: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
}

// ---------- VIC state --------------------------------------------------------

/// Everything the interrupt-manager register window tracks. Written/read
/// via the `read`/`write` functions below. Single-threaded (core 0 only).
#[derive(Default)]
struct VicState {
    int_present: u32,       // 0x0F183000
    int_ctrl: u32,          // 0x0F183400
    // int_clear is write-only; no state, but guest writes it to ack.
    fiq_mask: u32,          // 0x0F183C00
    int_ed_1: u32,          // 0x0F184000
    int_ed_2: u32,          // 0x0F184400
    int_ed_3: u32,          // 0x0F184800
    match_reg: [u32; 4],    // 0x0F182000/400/800/C00
    // Edge-detection state: bit i is set once the corresponding match
    // register has fired since its last write. We only raise the timer
    // interrupt on the rising edge; otherwise the handler clearing
    // int_present would immediately re-raise because `ticks >= match`
    // stays true.
    match_fired: u32,
    // GPIO-adjacent registers the ROM hits during early probe.
    gpio_r: u32,            // 0x0F18C000
    gpio_e: u32,            // 0x0F18C400
    #[allow(dead_code)] // written via K_HDWR_GPIO_C path; no reads yet
    gpio_c: u32,            // 0x0F18C800 (write clears)
    p0f110000: u32,
    p0f111400: u32,
}

struct VicCell(UnsafeCell<VicState>);
// SAFETY: accessed only from the single EL2 trap handler on core 0.
unsafe impl Sync for VicCell {}

static VIC: VicCell = VicCell(UnsafeCell::new(VicState {
    int_present: 0,
    int_ctrl: 0,
    fiq_mask: 0,
    int_ed_1: 0,
    int_ed_2: 0,
    int_ed_3: 0,
    match_reg: [0; 4],
    match_fired: 0,
    gpio_r: 0,
    gpio_e: 0,
    gpio_c: 0,
    p0f110000: 0,
    p0f111400: 0,
}));

pub fn init() {
    TICK_EPOCH.store(read_cntpct(), Ordering::Release);
    init_calendar();
    crate::kprintln!(
        "vic: timer epoch = {}  CNTFRQ_EL0 = {} Hz  (Newton tick = {} Hz)",
        TICK_EPOCH.load(Ordering::Acquire),
        read_cntfrq(),
        NEWTON_TICK_HZ
    );
}

/// Seconds between 1904-01-01 00:00:00 UTC and 1970-01-01 00:00:00 UTC.
/// Newton OS counts wall-clock seconds since 1904-01-01; host `SYS_TIME`
/// returns seconds since 1970-01-01; the difference is a fixed constant.
const SECS_1904_TO_1970: u32 = 2_082_844_800;

/// Host `time()` seconds since 1904, captured once at hypervisor boot.
/// Paired with `CALENDAR_CNTPCT_BASELINE` to derive "now" without
/// calling back out to semihosting on every guest read.
static CALENDAR_SECONDS_AT_BOOT: AtomicU32 = AtomicU32::new(0);
/// CNTPCT_EL0 at the moment `CALENDAR_SECONDS_AT_BOOT` was captured.
static CALENDAR_CNTPCT_BASELINE: AtomicU64 = AtomicU64::new(0);

/// Capture host wall-clock and pair it with CNTPCT so guest reads of
/// the calendar register return a plausible "seconds since 1904"
/// value. Einstein achieves the same thing by patching the ROM's
/// `RealClockSeconds` routine (`TJITGenericROMPatch.cpp:110`) to call
/// host `time()` — we do it at the MMIO layer instead so we don't
/// depend on the ROM patch firing.
fn init_calendar() {
    const SYS_TIME: u64 = 0x11;
    // The ARM semihosting SYS_TIME call ignores the parameter block;
    // pass a dummy pointer to satisfy the shared `semihost` helper.
    let unix_time: u64 = unsafe {
        let ret: u64;
        core::arch::asm!(
            "hlt #0xF000",
            inout("x0") SYS_TIME => ret,
            in("x1") 0u64,
            options(nostack, preserves_flags),
        );
        ret
    };
    let secs_since_1904 = (unix_time as u32).wrapping_add(SECS_1904_TO_1970);
    CALENDAR_SECONDS_AT_BOOT.store(secs_since_1904, Ordering::Release);
    CALENDAR_CNTPCT_BASELINE.store(read_cntpct(), Ordering::Release);
    // Re-publish the tick page now that calendar_seconds() returns a
    // real value — `stage2::init` already called `tick_page::update`
    // once before this, while the baseline was still zero.
    crate::stage2::tick_page::update();
    crate::kprintln!(
        "vic: calendar = {} seconds since 1904-01-01 (host unix_time={})",
        secs_since_1904, unix_time
    );
}

/// Current "seconds since 1904" as seen by a guest read of the
/// calendar register. Combines the boot-time baseline with elapsed
/// wall seconds computed from CNTPCT_EL0.
pub fn calendar_seconds() -> u32 {
    let base = CALENDAR_SECONDS_AT_BOOT.load(Ordering::Acquire);
    let baseline = CALENDAR_CNTPCT_BASELINE.load(Ordering::Acquire);
    let elapsed_cnt = read_cntpct().wrapping_sub(baseline);
    let freq = read_cntfrq();
    let elapsed_secs = (elapsed_cnt / freq as u64) as u32;
    base.wrapping_add(elapsed_secs)
}

// Interrupt bit layout in int_present — from TInterruptManager.h.
#[allow(dead_code)] // referenced once plumbing is wired through to guest IRQ
const INT_RTC_ALARM: u32 = 0x0000_0004;
const INT_TIMER_0: u32 = 0x0000_0008;
const INT_TIMER_1: u32 = 0x0000_0010;
const INT_TIMER_2: u32 = 0x0000_0020;
const INT_TIMER_3: u32 = 0x0000_0040;
#[allow(dead_code)]
const INT_GPIO: u32 = 0x0100_0000;

/// Latch any timer-match bits whose deadline has passed into `int_present`
/// so the next `update_virq` can assert HCR_EL2.VI. Called from the EL2
/// IRQ handler after CNTHP fires (the primary path), and from the sync
/// trap return as a safety net so MMIO writes that change VIC state see
/// their delivery consequence without waiting for a timer expiry.
pub fn poll_timer_matches() {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *VIC.0.get() };
    let now = ticks();
    let mut raise = 0u32;
    for (i, bit) in [
        (0usize, INT_TIMER_0),
        (1, INT_TIMER_1),
        (2, INT_TIMER_2),
        (3, INT_TIMER_3),
    ] {
        let slot_bit = 1u32 << i;
        let already_fired = (s.match_fired & slot_bit) != 0;
        let crossed = s.match_reg[i] != 0
            && now.wrapping_sub(s.match_reg[i]) < 0x8000_0000;
        if crossed && !already_fired {
            raise |= bit;
            s.match_fired |= slot_bit;
        }
    }
    if raise != 0 {
        s.int_present |= raise;
    }
}

/// Earliest pending Newton match deadline, or None if all four matches
/// have already fired (or are zero). Returned in the Newton tick domain;
/// callers wanting a CNTPCT-domain deadline must translate themselves.
pub fn next_pending_match() -> Option<u32> {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    let now = ticks();
    let mut best: Option<u32> = None;
    for i in 0..4usize {
        let slot_bit = 1u32 << i;
        if (s.match_fired & slot_bit) != 0 { continue; }
        if s.match_reg[i] == 0 { continue; }
        // Only consider matches in the near future (or already crossed).
        // Distant future deadlines (> 2^31 ticks away) we treat as stale.
        let delta = s.match_reg[i].wrapping_sub(now);
        if delta >= 0x8000_0000 {
            // Already crossed — fire ASAP.
            return Some(now);
        }
        best = Some(match best {
            None => s.match_reg[i],
            Some(cur) => {
                if delta < cur.wrapping_sub(now) { s.match_reg[i] } else { cur }
            }
        });
    }
    best
}

/// Whether any IRQ-class interrupt is currently pending and unmasked.
/// Per TInterruptManager the gate is `int_present & int_ctrl & ~fiq_mask`.
pub fn irq_pending() -> bool {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    let pending = s.int_present & s.int_ctrl & !s.fiq_mask;
    pending != 0
}

/// Likewise for FIQ.
pub fn fiq_pending() -> bool {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    let pending = s.int_present & s.int_ctrl & s.fiq_mask;
    pending != 0
}

/// Current raised interrupt bits. For diagnostics.
pub fn raised() -> u32 {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    s.int_present
}

// ---------- Hardware register addresses --------------------------------------
// Mirroring the subset of TMemoryConsts.h relevant to early boot.

const K_HDWR_PLATFORM_VERS: u64 = 0x0F00_0008;
const K_HDWR_P0F110000: u64 = 0x0F11_0000;
const K_HDWR_HIGH_SPEED_CLCK: u64 = 0x0F11_0400;
const K_HDWR_P0F111400: u64 = 0x0F11_1400;
const K_HDWR_P0F180400: u64 = 0x0F18_0400;
const K_HDWR_CALENDAR_REG: u64 = 0x0F18_1000;
const K_HDWR_ALARM_REG: u64 = 0x0F18_1400;
const K_HDWR_TICKS: u64 = 0x0F18_1800;

const K_HDWR_MATCH_0: u64 = 0x0F18_2000;
const K_HDWR_MATCH_1: u64 = 0x0F18_2400;
const K_HDWR_MATCH_2: u64 = 0x0F18_2800;
const K_HDWR_MATCH_3: u64 = 0x0F18_2C00;

const K_HDWR_INT_PRESENT: u64 = 0x0F18_3000;
const K_HDWR_INT_CTRL: u64 = 0x0F18_3400;
const K_HDWR_INT_CLEAR: u64 = 0x0F18_3800;
const K_HDWR_FIQ_MASK: u64 = 0x0F18_3C00;
const K_HDWR_INT_ED_1: u64 = 0x0F18_4000;
const K_HDWR_INT_ED_2: u64 = 0x0F18_4400;
const K_HDWR_INT_ED_3: u64 = 0x0F18_4800;
const K_HDWR_P0F185000: u64 = 0x0F18_5000;

const K_HDWR_GPIO_R: u64 = 0x0F18_C000;
const K_HDWR_GPIO_E: u64 = 0x0F18_C400;
const K_HDWR_GPIO_C: u64 = 0x0F18_C800;

/// Live tick count, scaled from A53 wall clock to Newton's 3.6864 MHz rate.
pub fn ticks() -> u32 {
    let epoch = TICK_EPOCH.load(Ordering::Acquire);
    let now = read_cntpct();
    let elapsed = now.wrapping_sub(epoch);
    let freq = read_cntfrq();
    // ticks = elapsed * NEWTON_TICK_HZ / freq. Reorder to keep within u64.
    let ticks = (elapsed as u128 * NEWTON_TICK_HZ as u128 / freq as u128) as u64;
    ticks as u32
}

// ---------- MMIO dispatch ----------------------------------------------------

/// True if `ipa` is handled by this module. Keeps `mmio::read/write`
/// tidy without forcing it to know every register address.
pub fn owns(ipa: u64) -> bool {
    match ipa {
        K_HDWR_PLATFORM_VERS
        | K_HDWR_P0F110000
        | K_HDWR_HIGH_SPEED_CLCK
        | K_HDWR_P0F111400
        | K_HDWR_P0F180400
        | K_HDWR_CALENDAR_REG
        | K_HDWR_ALARM_REG
        | K_HDWR_TICKS
        | K_HDWR_MATCH_0
        | K_HDWR_MATCH_1
        | K_HDWR_MATCH_2
        | K_HDWR_MATCH_3
        | K_HDWR_INT_PRESENT
        | K_HDWR_INT_CTRL
        | K_HDWR_INT_CLEAR
        | K_HDWR_FIQ_MASK
        | K_HDWR_INT_ED_1
        | K_HDWR_INT_ED_2
        | K_HDWR_INT_ED_3
        | K_HDWR_P0F185000
        | K_HDWR_GPIO_R
        | K_HDWR_GPIO_E
        | K_HDWR_GPIO_C => true,
        _ => false,
    }
}

pub fn read(ipa: u64) -> u32 {
    // SAFETY: single-threaded access from the trap handler.
    let s = unsafe { &mut *VIC.0.get() };
    match ipa {
        // Einstein's TPlatformManager::GetVersion returns the constant
        // 5 (`Emulator/Platform/TPlatformManager.cpp:110`). Newton's
        // native apps read this register to know the Einstein-era
        // platform driver revision.
        K_HDWR_PLATFORM_VERS => 5,
        K_HDWR_P0F110000 => s.p0f110000,
        K_HDWR_HIGH_SPEED_CLCK => 0x0000_0090, // kHighSpeedClockVal per TMemoryConsts
        K_HDWR_P0F111400 => s.p0f111400,
        K_HDWR_CALENDAR_REG => calendar_seconds(),
        K_HDWR_ALARM_REG => 0,
        K_HDWR_TICKS => ticks(),
        K_HDWR_MATCH_0 => s.match_reg[0],
        K_HDWR_MATCH_1 => s.match_reg[1],
        K_HDWR_MATCH_2 => s.match_reg[2],
        K_HDWR_MATCH_3 => s.match_reg[3],
        K_HDWR_INT_PRESENT => s.int_present,
        K_HDWR_INT_CTRL => s.int_ctrl,
        K_HDWR_FIQ_MASK => s.fiq_mask,
        K_HDWR_INT_ED_1 => s.int_ed_1,
        K_HDWR_INT_ED_2 => s.int_ed_2,
        K_HDWR_INT_ED_3 => s.int_ed_3,
        K_HDWR_GPIO_R => s.gpio_r,
        K_HDWR_GPIO_E => s.gpio_e,
        _ => halt_vic_unreachable("read", ipa, 0),
    }
}

pub fn write(ipa: u64, value: u32) {
    // SAFETY: single-threaded access.
    let s = unsafe { &mut *VIC.0.get() };
    // Log architecturally-significant VIC writes for diagnostic purposes.
    // Budget-limited so we don't drown in logs.
    static mut LOG_N: usize = 0;
    let interesting = matches!(ipa,
        K_HDWR_MATCH_0 | K_HDWR_MATCH_1 | K_HDWR_MATCH_2 | K_HDWR_MATCH_3
        | K_HDWR_INT_CTRL | K_HDWR_FIQ_MASK
        | K_HDWR_INT_ED_1 | K_HDWR_INT_ED_2 | K_HDWR_INT_ED_3
    );
    if interesting {
        let n = unsafe { let v = LOG_N; LOG_N += 1; v };
        if n < 32 {
            crate::kprintln!("vic: write IPA={:#010x} <- {:#010x}", ipa, value);
        }
    }
    let mut match_reprogrammed = false;
    match ipa {
        K_HDWR_P0F110000 => s.p0f110000 = value,
        // High-speed clock control: the kernel writes the expected
        // configuration value (`kHighSpeedClockVal` = 0x90) once at
        // boot. Modeled as a no-op write — the clock is always
        // configured from our perspective.
        K_HDWR_HIGH_SPEED_CLCK => { /* no-op per Einstein */ }
        K_HDWR_P0F111400 => s.p0f111400 = value,
        K_HDWR_P0F180400 => { /* misc write, ignore */ }
        // Tick counter is derived from CNTPCT; the kernel writes this
        // occasionally to reset the tick epoch (e.g. during calibration
        // loops). We could re-anchor TICK_EPOCH but Einstein's tick
        // register is also free-running from the host clock, so the
        // kernel works either way. Accept the write as a no-op; if the
        // guest later depends on the reset, halt here.
        K_HDWR_TICKS => { /* no-op — we derive ticks from CNTPCT */ }
        // Calendar / alarm regs — stub writes to accept "set calendar"
        // / "set alarm" without modeling RTC.
        K_HDWR_CALENDAR_REG => { /* no-op — RTC not modeled */ }
        K_HDWR_ALARM_REG => { /* no-op — RTC alarm not modeled */ }
        K_HDWR_MATCH_0 => { s.match_reg[0] = value; s.match_fired &= !0b0001; match_reprogrammed = true; }
        K_HDWR_MATCH_1 => { s.match_reg[1] = value; s.match_fired &= !0b0010; match_reprogrammed = true; }
        K_HDWR_MATCH_2 => { s.match_reg[2] = value; s.match_fired &= !0b0100; match_reprogrammed = true; }
        K_HDWR_MATCH_3 => { s.match_reg[3] = value; s.match_fired &= !0b1000; match_reprogrammed = true; }
        K_HDWR_INT_CTRL => s.int_ctrl = value,
        K_HDWR_INT_CLEAR => {
            // Writing clears the matching bits in int_present.
            s.int_present &= !value;
        }
        K_HDWR_FIQ_MASK => s.fiq_mask = value,
        K_HDWR_INT_ED_1 => s.int_ed_1 = value,
        K_HDWR_INT_ED_2 => s.int_ed_2 = value,
        K_HDWR_INT_ED_3 => s.int_ed_3 = value,
        K_HDWR_P0F185000 => { /* misc, ignore */ }
        K_HDWR_GPIO_E => s.gpio_e = value,
        K_HDWR_GPIO_C => s.int_present &= !value, // many devices clear via this pattern
        _ => halt_vic_unreachable("write", ipa, value),
    }
    if match_reprogrammed {
        // A match register changed — recompute the nearest deadline and
        // reprogram CNTHP_CVAL_EL2 so the async timer path delivers.
        crate::timer::rearm();
    }
}

/// Fallback halt for match-arm branches that shouldn't be reachable
/// because `owns()` filters the same address set. If they do fire,
/// Phase A says halt loudly rather than silently stubbing.
fn halt_vic_unreachable(op: &'static str, ipa: u64, value: u32) -> ! {
    crate::kprintln!();
    crate::kprintln!(
        "*** vic::{} IPA={:#010x} val={:#010x} — owns() says mine but match has no arm ***",
        op, ipa, value
    );
    crate::kprintln!(
        "  (bug in peripherals/vic.rs: owns() and read/write disagree.)"
    );
    crate::cpu::halt();
}
