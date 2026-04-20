//! Minimal Newton virtual interrupt controller + timer.
//!
//! Stores the state the ROM touches early and returns sensible values:
//!
//!   Ticks register (3.6864 MHz counter) — computed from the A53 generic
//!   timer (CNTPCT_EL0 scaled by CNTFRQ_EL0), reset on init. Reading this
//!   register is the main unblocker for the guest's early polling loop.
//!
//!   Interrupt enable/mask/control registers — stored as plain state. No
//!   actual interrupts delivered in this iteration; the guest's kernel
//!   proceeds as if no peripheral has raised anything yet.
//!
//!   Timer match registers — stored, not yet compared against ticks.
//!
//! A later iteration will inject vIRQ/vFIQ via HCR_EL2.VI / VF when the
//! scheduler match register fires.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

// ---------- Newton tick clock (3.6864 MHz). ----------------------------------

/// A53 CNTPCT_EL0 reading at the moment `init()` was called, captured so we
/// can report ticks as "time since hypervisor started guest", which matches
/// what the guest expects at reset.
static TICK_EPOCH: AtomicU64 = AtomicU64::new(0);

const NEWTON_TICK_HZ: u64 = 3_686_400;

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
    // GPIO-adjacent registers the ROM hits during early probe.
    gpio_r: u32,            // 0x0F18C000
    gpio_e: u32,            // 0x0F18C400
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
    gpio_r: 0,
    gpio_e: 0,
    gpio_c: 0,
    p0f110000: 0,
    p0f111400: 0,
}));

pub fn init() {
    TICK_EPOCH.store(read_cntpct(), Ordering::Release);
    crate::kprintln!(
        "vic: timer epoch = {}  CNTFRQ_EL0 = {} Hz  (Newton tick = {} Hz)",
        TICK_EPOCH.load(Ordering::Acquire),
        read_cntfrq(),
        NEWTON_TICK_HZ
    );
}

// Interrupt bit layout in int_present — from TInterruptManager.h.
const INT_RTC_ALARM: u32 = 0x0000_0004;
const INT_TIMER_0: u32 = 0x0000_0008;
const INT_TIMER_1: u32 = 0x0000_0010;
const INT_TIMER_2: u32 = 0x0000_0020;
const INT_TIMER_3: u32 = 0x0000_0040;
#[allow(dead_code)]
const INT_GPIO: u32 = 0x0100_0000;

/// Called periodically (after every MMIO trap). Inject whatever timer /
/// enabled-IRQ bits look "due" into int_present so the VI/VF update
/// path can deliver them on the next ERET.
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
        if s.match_reg[i] != 0
            && now.wrapping_sub(s.match_reg[i]) < 0x8000_0000
            && (s.int_present & bit) == 0
        {
            raise |= bit;
        }
    }

    // If the kernel has enabled any IRQ / FIQ source but no timer matches
    // have fired yet, raise whatever IS enabled once every ~10000 ticks
    // so the kernel can make progress past its "waiting for first
    // interrupt" state. This is a pure bring-up shim and is not what the
    // real hardware does — remove once the guest is past scheduler init.
    static mut LAST_WAKE: u32 = 0;
    let last = unsafe { LAST_WAKE };
    if now.wrapping_sub(last) > 10_000 {
        let enabled_no_timer = s.int_ctrl & !(INT_TIMER_0 | INT_TIMER_1 | INT_TIMER_2 | INT_TIMER_3);
        if enabled_no_timer != 0 && (s.int_present & enabled_no_timer) == 0 {
            raise |= enabled_no_timer & (INT_RTC_ALARM | INT_TIMER_3 | 0x0100_0000);
            unsafe { LAST_WAKE = now; }
        }
    }

    if raise != 0 {
        s.int_present |= raise;
    }
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
        K_HDWR_PLATFORM_VERS => 0,
        K_HDWR_P0F110000 => s.p0f110000,
        K_HDWR_HIGH_SPEED_CLCK => 0x0000_0090, // kHighSpeedClockVal per TMemoryConsts
        K_HDWR_P0F111400 => s.p0f111400,
        K_HDWR_CALENDAR_REG => 0, // seconds since epoch; 0 is acceptable
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
        _ => 0,
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
    match ipa {
        K_HDWR_P0F110000 => s.p0f110000 = value,
        K_HDWR_P0F111400 => s.p0f111400 = value,
        K_HDWR_P0F180400 => { /* misc write, ignore */ }
        K_HDWR_MATCH_0 => s.match_reg[0] = value,
        K_HDWR_MATCH_1 => s.match_reg[1] = value,
        K_HDWR_MATCH_2 => s.match_reg[2] = value,
        K_HDWR_MATCH_3 => s.match_reg[3] = value,
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
        _ => {}
    }
}
