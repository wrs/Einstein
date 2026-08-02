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
//!   handler in `hv::trap` calls `poll_timer_matches` here to latch the
//!   crossed bit(s) into `int_present`, so the next `update_virq` sets VI.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Match-deadline rearm sink, installed by `main.rs` boot wiring
/// (`hv::timer::rearm`) so this guest model stays free of hv imports.
/// Raw fn pointer, 0 = uninstalled — [`match_rearm`] halts loudly on
/// use before wiring (a guest match-register write cannot happen
/// before the boot wiring runs).
static MATCH_REARM: AtomicUsize = AtomicUsize::new(0);

/// Install the match-deadline rearm sink. Called once from `main.rs`.
pub fn install_match_rearm(sink: fn()) {
    MATCH_REARM.store(sink as usize, Ordering::Release);
}

fn match_rearm() -> fn() {
    let raw = MATCH_REARM.load(Ordering::Acquire);
    if raw == 0 {
        crate::kprintln!(
            "*** vic: no match-rearm sink — main.rs must install_match_rearm() before use ***"
        );
        crate::arch::cpu::halt();
    }
    // SAFETY: the only writer is install_match_rearm, which stores a
    // valid `fn()`; 0 is filtered above.
    unsafe { core::mem::transmute(raw) }
}

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
/// lives in `crate::host::platform::NEWTON_TICK_HZ`.
pub use crate::host::platform::NEWTON_TICK_HZ;

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
///
/// The set of stateful fields here MUST match the set of registers that
/// Einstein's TInterruptManager / TMemory model statefully — because
/// the user's stated invariant is "match Einstein's emulated hardware
/// behavior exactly". Registers Einstein doesn't model (read returns 0
/// from the unknown-bank-#3 default in TMemory.cpp:950-960; writes are
/// silently dropped) MUST NOT be stored here even if the kernel does
/// read-modify-write on them — Einstein's r-m-w sees 0 every read, so
/// ours must too.
#[derive(Default)]
struct VicState {
    // ---- Stateful in Einstein (TInterruptManager:492-507) -------------
    int_present: u32, // 0x0F183000  mIntRaised
    int_ctrl: u32,    // 0x0F183400  mIntCtrlReg
    // int_clear is write-only in Einstein; clears bits in mIntRaised.
    fiq_mask: u32,       // 0x0F183C00  mFIQMask
    int_ed_1: u32,       // 0x0F184000  mIntEDReg1
    int_ed_2: u32,       // 0x0F184400  mIntEDReg2
    int_ed_3: u32,       // 0x0F184800  mIntEDReg3
    match_reg: [u32; 4], // 0x0F182000/400/800/C00  mMatchReg[0..3]
    // Edge-detection state: bit i is set once the corresponding match
    // register has fired since its last write. We only raise the timer
    // interrupt on the rising edge; otherwise the handler clearing
    // int_present would immediately re-raise because `ticks >= match`
    // stays true. (Einstein gets the same effect via its
    // SetTimerMatchRegister / GetTimer / RaiseInterrupt interplay.)
    match_fired: u32,
    // RTC alarm-match: stored alarm value (in seconds since 1904 in the
    // calendar domain) and a single-bit edge-detect latch. Cleared on
    // alarm register write so a new alarm can fire.
    alarm_reg: u32, // 0x0F181400  via SetAlarm/GetAlarm
    alarm_fired: bool,
    // GPIO interrupt registers (Einstein TInterruptManager.h:510,513).
    gpio_r: u32, // 0x0F18C000  mGPIORaised
    gpio_e: u32, // 0x0F18C400  mGPIOCtrlReg
                 // ---- NOT stateful in Einstein ------------------------------------
                 // The following addresses fall through to the unknown-bank-#3 default
                 // in Einstein's TMemory.cpp Bank #3 read path (lines 950-960 = 0)
                 // and write path (lines 1903-1913 = silent log + drop). Do not store
                 // their writes here. Reads return 0. Listed for documentation:
                 //   0x0F110000, 0x0F111400, 0x0F180400, 0x0F185000,
                 //   0x0F18C800 (kHdWr_GPIO_CReg — write goes to ClearGPIO, no state),
                 //   0x0F18CC00, 0x0F18D000, 0x0F18D800, 0x0F18DC00,
                 //   0x0F18E000, 0x0F18E800, 0x0F18EC00.
}

struct VicCell(UnsafeCell<VicState>);
// SAFETY: accessed only from the single EL2 trap handler on core 0.
//
// Borrow invariant: no `&mut VicState` borrow (via `VIC.0.get()`) may be
// live across any point where EL2 IRQs are unmasked. The only such point
// today is `platform::pause_system`'s WFI loop, which unmasks IRQs at EL2
// so a nested `trap_irq` can run; that nested handler re-borrows VIC state
// (`poll_timer_matches`, `vic::raise`, …). A `&mut` held across the unmask
// window would alias the nested borrow — undefined behavior. Hold borrows
// only for the duration of a single `read`/`write`/`poll_*` call, never
// across a WFI/unmask.
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
    alarm_reg: 0,
    alarm_fired: false,
    gpio_r: 0,
    gpio_e: 0,
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

/// Subtract this from the host wall-clock seconds before publishing to
/// the guest. Einstein's NS time-base patches (`FTimeInSeconds`,
/// `Time base (1..4/4)`) re-express seconds-since-1904 as NS-encoded
/// seconds-since-2008, which only fits in the 30-bit signed NS Ref
/// while seconds-since-2008 stays below 2²⁹ ≈ 17.0 years — i.e. until
/// approximately 2025-01-08. Past that point the encoded value crosses
/// into i32-negative territory and `SetSysAlarm` writes a hardware
/// alarm register pointing into the past, IRQ-looping the alarm
/// dispatcher. Until the 2026 epoch shift is wired up, just pretend
/// it's 6 years earlier than wall-clock and stay inside the safe
/// window. 6 × 365 × 86400 = 189,216,000.
const RTC_HOST_TIME_OFFSET_SECONDS: u32 = 189_216_000;

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
    #[cfg(nh_semihost)]
    let unix_time: u64 = {
        const SYS_TIME: u64 = 0x11;
        // The ARM semihosting SYS_TIME call ignores the parameter
        // block; pass a dummy pointer to satisfy the shared `semihost`
        // helper.
        // SAFETY: HLT #0xF000 is the AArch64 semihosting trap.
        unsafe {
            let ret: u64;
            core::arch::asm!(
                "hlt #0xF000",
                inout("x0") SYS_TIME => ret,
                in("x1") 0u64,
                options(nostack, preserves_flags),
            );
            ret
        }
    };
    // Without `nh_semihost` there is no host clock to
    // ask. Use a compile-time-baked seed: midnight 2026-05-16 UTC.
    // Newton runs reasonably with any plausible RTC; "wrong by hours"
    // only matters for user-visible dates. A future Phase will read a
    // real RTC chip if we add one.
    #[cfg(not(nh_semihost))]
    let unix_time: u64 = 1_778_889_600; // 2026-05-16 00:00:00 UTC
    let secs_since_1904 = (unix_time as u32)
        .wrapping_add(SECS_1904_TO_1970)
        .wrapping_sub(RTC_HOST_TIME_OFFSET_SECONDS);
    CALENDAR_SECONDS_AT_BOOT.store(secs_since_1904, Ordering::Release);
    CALENDAR_CNTPCT_BASELINE.store(read_cntpct(), Ordering::Release);
    // (main.rs re-seeds the tick page right after `vic::init` returns,
    // so the page picks up the real calendar_seconds() value — the
    // earlier post-stage-2 seed ran while the baseline was still zero.)
    crate::kprintln!(
        "vic: calendar = {} seconds since 1904-01-01 (host unix_time={}, offset={}s back)",
        secs_since_1904,
        unix_time,
        RTC_HOST_TIME_OFFSET_SECONDS
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
const INT_RTC_ALARM: u32 = 0x0000_0004;
const INT_TIMER_0: u32 = 0x0000_0008;
const INT_TIMER_1: u32 = 0x0000_0010;
const INT_TIMER_2: u32 = 0x0000_0020;
const INT_TIMER_3: u32 = 0x0000_0040;
const INT_DMA_CH5: u32 = 0x0000_1000; // sound output / tablet rcv
pub const INT_GPIO: u32 = 0x0100_0000;
/// Tablet (digitizer) pen-event interrupt. Einstein's
/// `TInterruptManager.h:81 kTabletIntMask`. Raised by
/// `host_io::queue::enqueue_pen_sample` when a fresh sample lands
/// on the input queue.
pub const INT_TABLET: u32 = 0x1000_0000;
use crate::diag::diag_util::LogBudget;
static VIC_DMA_RAISE_LOG: LogBudget = LogBudget::new(8);
static VIC_DMA_CLEAR_LOG: LogBudget = LogBudget::new(8);
static VIC_DMA_CTRL_LOG: LogBudget = LogBudget::new(16);
static VIC_DMA_FIQ_LOG: LogBudget = LogBudget::new(16);

/// Public raiser: OR `mask` into `int_present`. The next `update_virq`
/// (called at every sync-trap exit and after `timer::on_irq`) reflects
/// the change into HCR_EL2.VI / VF, so the guest takes a virtual IRQ
/// on the next ERET if the unmask gates allow it.
///
/// Used by external raisers — DMA channel completion
/// (`dma::poll_tx` / `dma::poll_rx`),
/// GPIO line events (HVC test trigger / future native-event hooks),
/// RTC alarm match, etc. Timer matches go through the existing
/// `poll_timer_matches` edge-detect path.
pub fn raise(mask: u32) {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *VIC.0.get() };
    s.int_present |= mask;
    // Sound-DMA raises: log the first few so IRQ delivery into the
    // guest is auditable (present + enabled + unmasked = deliverable).
    if mask & INT_DMA_CH5 != 0 {
        if VIC_DMA_RAISE_LOG.allow_or_every(64) {
            crate::kprintln!(
                "vic: raise dma mask={:#x} ipres={:#x} ictrl={:#x} fiq={:#x} deliverable={:#x}",
                mask,
                s.int_present,
                s.int_ctrl,
                s.fiq_mask,
                s.int_present & s.int_ctrl & !s.fiq_mask
            );
        }
    }
}

/// One-shot wake flag for `pause_system`. Set by `raise_power_switch` so
/// the EL2 WFI loop exits even when the corresponding IRQ bit is masked
/// out of `int_ctrl` by `kPowerOffMask` (`TInterruptManager.h:83`).
/// Mirrors Einstein's `mEmulatorCondVar->Signal()` back-door in
/// `RaiseGPIO` (TInterruptManager.cpp:472): the suspended CPU resumes
/// regardless of whether the IRQ would pass `mIntRaised & mIntCtrlReg`.
static WAKE_REQUEST: AtomicBool = AtomicBool::new(false);

/// True while the guest is parked inside subfn 0x0E `PowerOffSystem`
/// (deep-sleep WFI from `CyclePower__Fv`). Set/cleared by
/// `peripherals::platform::pause_system` around its WFI loop. Read by
/// `input::drain_into_queue` to implement the Einstein "first tap acts
/// as the power button" hack — see `AndroidGlue.cpp:205-216`. Subfn
/// 0x0D `PauseSystem` (idle loop) does NOT set this: the guest is
/// already powered on then, and a synthetic power-switch press would
/// be interpreted as a power-down request.
static POWERED_OFF: AtomicBool = AtomicBool::new(false);

/// Power-switch press from the host-IO transport. Mirrors Einstein's
/// `TPlatformManager::RaisePlatformInterrupt() -> RaiseGPIO(0x00000001)`
/// (TPlatformManager.cpp:484, TInterruptManager.cpp:458):
/// sets bit 0 in `mGPIORaised`, and if `mGPIOCtrlReg` has that line
/// enabled, raises `kGPIOIntMask` so the kernel sees it. We additionally
/// set `WAKE_REQUEST` so `pause_system` returns to the guest — `kGPIOIntMask`
/// is not in `kPowerOffMask` (0x0C400000), so the IRQ would otherwise stay
/// invisible while the system is in PowerOff state. Compiled only for
/// the two transports that deliver power-switch presses (the semihost
/// host viewer and the mtouch first-tap-wakes hack).
#[cfg(any(nh_host_io_semihost, nh_input_mtouch))]
pub fn raise_power_switch() {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *VIC.0.get() };
    s.gpio_r |= 0x0000_0001;
    if s.gpio_e & 0x0000_0001 != 0 {
        s.int_present |= INT_GPIO;
    }
    WAKE_REQUEST.store(true, Ordering::Release);
}

/// Consume the pending wake request, if any. Atomically swaps the flag
/// to false and returns its prior value.
pub fn take_wake_request() -> bool {
    WAKE_REQUEST.swap(false, Ordering::AcqRel)
}

/// Mark the guest as parked in deep-sleep PowerOffSystem WFI. Called by
/// `pause_system` for subfn 0x0E only.
pub fn set_powered_off(v: bool) {
    POWERED_OFF.store(v, Ordering::Release);
}

/// True while the guest is in subfn 0x0E `PowerOffSystem` WFI. Only
/// the mtouch backend's first-tap-wakes hack consults it.
#[cfg(nh_input_mtouch)]
pub fn is_powered_off() -> bool {
    POWERED_OFF.load(Ordering::Acquire)
}

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
        let crossed = s.match_reg[i] != 0 && now.wrapping_sub(s.match_reg[i]) < 0x8000_0000;
        if crossed && !already_fired {
            raise |= bit;
            s.match_fired |= slot_bit;
        }
    }
    if raise != 0 {
        s.int_present |= raise;
    }
}

/// RTC alarm match: rising-edge fire when `calendar_seconds() >= alarm_reg`
/// and `alarm_reg != 0`. Call alongside `poll_timer_matches`. Edge-detect
/// via `alarm_fired` cleared on alarm-register write.
pub fn poll_alarm() {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *VIC.0.get() };
    if s.alarm_reg == 0 || s.alarm_fired {
        return;
    }
    let now = calendar_seconds();
    // wrapping_sub < 0x8000_0000 covers both "now == alarm" and "now
    // moments past alarm" without losing to wraparound.
    if now.wrapping_sub(s.alarm_reg) < 0x8000_0000 {
        s.alarm_fired = true;
        s.int_present |= INT_RTC_ALARM;
    }
}

/// Earliest pending Newton match deadline, or None if all four matches
/// have already fired (or are zero). Returned in the Newton tick domain;
/// callers wanting a CNTPCT-domain deadline must translate themselves.
/// Only the synthetic-tick fast-forward in `heartbeat_tick_update`
/// consults it; without `nh_semihost` ticks are wall-anchored and the
/// fast-forward path is compiled out.
#[cfg(nh_semihost)]
pub fn next_pending_match() -> Option<u32> {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    let now = ticks();
    let mut best: Option<u32> = None;
    for i in 0..4usize {
        let slot_bit = 1u32 << i;
        if (s.match_fired & slot_bit) != 0 {
            continue;
        }
        if s.match_reg[i] == 0 {
            continue;
        }
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
                if delta < cur.wrapping_sub(now) {
                    s.match_reg[i]
                } else {
                    cur
                }
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

/// Diagnostic: raw `int_present` register (raised interrupt bits).
pub fn int_present_raw() -> u32 {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    s.int_present
}

/// Diagnostic: raw `int_ctrl` register (per-bit IRQ enable).
pub fn int_ctrl_raw() -> u32 {
    // SAFETY: single-threaded.
    let s = unsafe { &*VIC.0.get() };
    s.int_ctrl
}

// ---------- Hardware register addresses --------------------------------------
// Mirroring the subset of TMemoryConsts.h relevant to early boot.

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
const K_HDWR_P0F184C00: u64 = 0x0F18_4C00;
const K_HDWR_P0F185000: u64 = 0x0F18_5000;

const K_HDWR_GPIO_R: u64 = 0x0F18_C000;
const K_HDWR_GPIO_E: u64 = 0x0F18_C400;
const K_HDWR_GPIO_C: u64 = 0x0F18_C800;
const K_HDWR_GPIO_CC00: u64 = 0x0F18_CC00;
const K_HDWR_GPIO_D000: u64 = 0x0F18_D000;
const K_HDWR_GPIO_D400: u64 = 0x0F18_D400;
const K_HDWR_GPIO_D800: u64 = 0x0F18_D800;
const K_HDWR_GPIO_DC00: u64 = 0x0F18_DC00;
const K_HDWR_GPIO_E000: u64 = 0x0F18_E000;
const K_HDWR_GPIO_E800: u64 = 0x0F18_E800;
const K_HDWR_GPIO_EC00: u64 = 0x0F18_EC00;

/// Synthetic Newton-tick counter used **only on QEMU/FVP hosts**
/// (`cfg(nh_semihost)`). On real Pi silicon
/// (`cfg(not(nh_semihost))`) `ticks()` is wall-anchored to CNTPCT_EL0 and this
/// counter is unread — the advance functions still write to it for
/// code-path symmetry, but those writes have no observable effect.
///
/// Why not wall-anchored on QEMU: under QEMU TCG with `--features
/// trace,quiet` and the inline-patch UDF emulator we execute ~100×
/// fewer guest instructions per host wall-second than Einstein's JIT
/// does (each HVC trampoline alone costs ~30 µs). When the kernel's
/// polling loops (TBIOInterface::WaitBIOStatus, TDelayTimer::TimedOut
/// callers, etc.) arm a wall-anchored Newton-tick deadline, our
/// wall-clock-derived tick value crosses that deadline after far
/// fewer poll iterations than Einstein's, perturbing the kernel's
/// heap allocator interleave and steering `__nw__FUi(184)` towards a
/// VA range that aliases pckm's stack page.
///
/// The synthetic clock advances proportional to **guest progress**
/// (each sync trap ≈ a fixed slice of guest instructions), so
/// timeout-bounded polling loops iterate about as many times in our
/// QEMU run as in Einstein's, regardless of how slowly the host
/// wall clock is moving.
///
/// Δ per `tick_advance` call is calibrated empirically. Einstein's
/// `TBIOInterface::WaitBIOStatus` polls `TDelayTimer::TimedOut` 65
/// times against a 400-tick threshold, i.e. ≈ 6.15 ticks per poll
/// iteration; rounded up to 8 to allow some slack.
///
/// On real silicon the QEMU instruction-anchored rationale doesn't
/// apply — guest code runs at native A53 speed, hypervisor trap
/// overhead is microseconds (not tens of µs), and `SafeShortTimerDelay`
/// loops that intend "10 ms wall" actually want 10 ms of wall time.
/// The wall-anchored `ticks()` path taken without `nh_semihost` reads
/// CNTPCT_EL0 directly and scales by `NEWTON_TICK_HZ / CNTFRQ_EL0`.
static SYNTH_TICKS: AtomicU32 = AtomicU32::new(0);

/// How many synthetic ticks each guest sync trap adds. Tuned
/// against Einstein's per-poll tick consumption in the BIO chip-detect
/// loop (≈ 6.15 ticks/poll for a 400-tick threshold over 65 polls).
/// See `SYNTH_TICKS` doc.
const TICK_ADVANCE_PER_TRAP: u32 = 6;

/// How many synthetic ticks each CNTHP heartbeat adds. The heartbeat
/// is the only tick source for non-trapping busy-wait loops like
/// `SafeShortTimerDelay` (BootOS:0x18f08), which read the tick page
/// in a tight loop without ever leaving guest mode. Without this, a
/// wait of N ticks would never complete because nothing bumps
/// SYNTH_TICKS during the loop.
///
/// Tuned to make a 11 058-tick `SafeShortTimerDelay` (the first
/// BootOS calibration call, originally 3 ms wall) complete in ≤ 11
/// heartbeats ≈ 176 ms wall — i.e., ~50× slower than the kernel's
/// wall-time intent, which is fine since real-time semantics aren't
/// load-bearing for boot. Values much smaller make BootOS calibration
/// crawl; values much larger let preemption-tier deadlines (73 720
/// ticks for the 20 ms slice) fire on every heartbeat regardless of
/// guest progress, defeating the instruction-anchored model.
#[cfg(nh_semihost)]
const TICK_ADVANCE_PER_HEARTBEAT: u32 = 1024;

/// Current Newton-tick value as seen by guest reads of `kHdWr_Ticks`
/// (0x0F181800) — both via MMIO trap (`vic::read`) and via the
/// non-trapping `TICK_PAGE`.
///
/// On real silicon (`cfg(not(nh_semihost))`) this is wall-
/// anchored: CNTPCT_EL0 elapsed since `init()` scaled by
/// `NEWTON_TICK_HZ / CNTFRQ_EL0`. A `SafeShortTimerDelay` that asks
/// for ~10 ms of ticks gets ~10 ms of wall time, modulo the
/// `TICK_PAGE` republish granularity (every sync trap, plus the
/// ~16 ms CNTHP heartbeat for non-trapping busy waits).
///
/// On QEMU/FVP this returns the `SYNTH_TICKS` synthetic counter,
/// advanced by `tick_advance_*` from the tick-page update path. See
/// the `SYNTH_TICKS` doc-comment for the QEMU-specific rationale.
pub fn ticks() -> u32 {
    #[cfg(not(nh_semihost))]
    {
        let elapsed = read_cntpct().wrapping_sub(TICK_EPOCH.load(Ordering::Acquire));
        let freq = read_cntfrq() as u128;
        ((elapsed as u128 * NEWTON_TICK_HZ as u128) / freq) as u32
    }
    #[cfg(nh_semihost)]
    SYNTH_TICKS.load(Ordering::Acquire)
}

/// Bump SYNTH_TICKS by the sync-trap delta. Called from
/// the sync-trap-exit tick-page update in `newton::os` (= every guest sync trap
/// via `trap_sync_lower_aarch32`).
pub fn tick_advance_sync_trap() -> u32 {
    let prev = SYNTH_TICKS.fetch_add(TICK_ADVANCE_PER_TRAP, Ordering::AcqRel);
    prev.wrapping_add(TICK_ADVANCE_PER_TRAP)
}

/// Bump SYNTH_TICKS by the heartbeat delta. Called from
/// `timer::on_irq` (every CNTHP heartbeat) so that non-trapping
/// busy-wait loops still see ticks advance. Unused without `nh_semihost`,
/// where `ticks()` is wall-anchored.
#[cfg(nh_semihost)]
pub fn tick_advance_heartbeat() -> u32 {
    let prev = SYNTH_TICKS.fetch_add(TICK_ADVANCE_PER_HEARTBEAT, Ordering::AcqRel);
    prev.wrapping_add(TICK_ADVANCE_PER_HEARTBEAT)
}

/// CNTHP heartbeat tick update: advance synthetic ticks for any guest
/// parked in WFI / a non-trapping busy-wait, plus jump past any
/// pending match deadline if the guest is making no sync-trap
/// progress at all.
///
/// Without this hook:
/// * A guest parked in `SafeShortTimerDelay` (a tight loop reading
///   `TICK_PAGE` non-trapping) would never see ticks advance, since
///   sync traps stop firing. The `TICK_ADVANCE_PER_HEARTBEAT` bump
///   here is the only forward motion that loop sees.
/// * A guest parked in WFI on a Newton-tick *match* deadline would
///   wait until the heartbeat-sized advance crossed the match, which
///   for a 0x12000-tick (= 73 720) preemption deadline is ≈ 18
///   heartbeats ≈ 288 ms wall. Fine for boot; for short matches the
///   fast-forward branch below trims it to a single heartbeat.
///
/// "No guest progress" is detected by SYNTH_TICKS being unchanged
/// from the value after the *previous* heartbeat's update: if the
/// guest had taken any sync trap, `tick_advance_sync_trap` would
/// have bumped SYNTH_TICKS in between. Don't fast-forward when the
/// guest is making progress — doing so would let the heartbeat skip
/// past a polling loop's deadline before the loop iterated as many
/// times as the kernel intended.
///
/// On real silicon (`cfg(not(nh_semihost))`) `ticks()` is wall-anchored, so
/// match deadlines are crossed naturally by CNTPCT advancing; the
/// fast-forward is moot. The matching
/// heartbeat tick-page republish in `newton::os` publishes the current
/// wall-anchored `ticks()` into the guest's read-only tick page.
pub fn heartbeat_tick_update() {
    #[cfg(not(nh_semihost))]
    return;
    #[cfg(nh_semihost)]
    {
        static LAST_HEARTBEAT_TICK: AtomicU32 = AtomicU32::new(0);
        let last = LAST_HEARTBEAT_TICK.load(Ordering::Acquire);
        let cur = SYNTH_TICKS.load(Ordering::Acquire);
        let no_guest_progress = cur == last;

        // Apply the heartbeat's own tick advance — needed so non-trapping
        // busy-wait loops on TICK_PAGE see forward progress at all.
        let after = tick_advance_heartbeat();

        // If the guest hadn't moved since the last heartbeat AND there's
        // a pending match deadline, jump past it. This trims wake latency
        // for WFI-on-match scenarios from "deadline / Δ_heartbeat * 16 ms"
        // down to one heartbeat.
        let final_value = if no_guest_progress {
            if let Some(deadline) = next_pending_match() {
                let target = if deadline.wrapping_sub(after) < 0x8000_0000 {
                    // Deadline is at or after the just-advanced value.
                    deadline.wrapping_add(1)
                } else {
                    // Deadline already crossed; nothing to do.
                    after
                };
                SYNTH_TICKS.store(target, Ordering::Release);
                target
            } else {
                after
            }
        } else {
            after
        };

        LAST_HEARTBEAT_TICK.store(final_value, Ordering::Release);
    }
}

// ---------- MMIO dispatch ----------------------------------------------------

/// Marker for the [`crate::hv::mmio::MmioPeripheral`] router. The register
/// state lives in the module-level `VIC` cell; this zero-sized type only
/// names the model for static dispatch. The router sends every access
/// inside the VIC windows (`layout::MMIO_WINDOWS` entries with
/// `PeriphId::Vic`) here; unmodelled addresses inside those windows
/// halt via [`halt_vic_unknown`].
pub struct Vic;

impl crate::hv::mmio::MmioPeripheral for Vic {
    fn read(ipa: u64) -> u32 {
        read(ipa)
    }
    fn write(ipa: u64, value: u32) {
        write(ipa, value)
    }
}

fn read(ipa: u64) -> u32 {
    // SAFETY: single-threaded access from the trap handler.
    let s = unsafe { &mut *VIC.0.get() };
    match ipa {
        // ---- Stateful in Einstein -------------------------------------
        // HighSpeedClck: TMemory.cpp:898-900 returns kHighSpeedClockVal
        // = 0x90 (TMemoryConsts.h:208).
        K_HDWR_HIGH_SPEED_CLCK => 0x0000_0090,
        K_HDWR_CALENDAR_REG => calendar_seconds(),
        K_HDWR_ALARM_REG => s.alarm_reg,
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

        // GPIO input (PCMCIA door-lock + misc sense lines).
        // Einstein returns all-ones = "no cards / switches open".
        K_HDWR_GPIO_D400 => 0xFFFF_FFFF,

        // kHdWr_P0F184C00 (TMemoryConsts.h:101, "R"): Einstein's TMemory.cpp
        // Bank #3 read path (lines 803-960) has NO specific handler for this
        // address — it falls through to the "unknown bank #3" default at
        // lines 950-960, which returns 0. (There is no Einstein code
        // returning an "all-ok high" 0xFFFFFFFF for this address.)
        // Bit 21 of this register gates a kernel polling
        // path at ROM 0x00019d34 / 0x00019d90 / 0x00019e34 (`tst r1,
        // #0x00200000`); returning 0 makes us take the same branches as
        // Einstein.
        K_HDWR_P0F184C00 => 0,

        // ---- Not modeled by Einstein → returns 0 by default ------------
        // Einstein TMemory.cpp Bank #3 read path (lines 803-960) has no
        // specific handler for these addresses; the unknown-bank-#3
        // default at lines 950-960 returns 0. Match that: storing the
        // written value and reading it back would diverge whenever the
        // kernel does read-modify-write here
        // (TGPIOInterface::DisableInterrupt etc.), since Einstein's
        // r-m-w sees 0 on every read.
        K_HDWR_P0F110000
        | K_HDWR_P0F111400
        | K_HDWR_P0F180400
        | K_HDWR_P0F185000
        | K_HDWR_INT_CLEAR  // write-only by convention; Einstein returns 0
        | K_HDWR_GPIO_C
        | K_HDWR_GPIO_CC00
        | K_HDWR_GPIO_D000
        | K_HDWR_GPIO_D800
        | K_HDWR_GPIO_DC00
        | K_HDWR_GPIO_E000
        | K_HDWR_GPIO_E800
        | K_HDWR_GPIO_EC00 => 0,

        _ => halt_vic_unknown("read", ipa, 0),
    }
}

fn write(ipa: u64, value: u32) {
    // SAFETY: single-threaded access.
    let s = unsafe { &mut *VIC.0.get() };
    // Log architecturally-significant VIC writes for diagnostic purposes.
    // Budget-limited so we don't drown in logs.
    static WRITE_LOG: LogBudget = LogBudget::new(32);
    let interesting = matches!(
        ipa,
        K_HDWR_MATCH_0
            | K_HDWR_MATCH_1
            | K_HDWR_MATCH_2
            | K_HDWR_MATCH_3
            | K_HDWR_INT_CTRL
            | K_HDWR_FIQ_MASK
            | K_HDWR_INT_ED_1
            | K_HDWR_INT_ED_2
            | K_HDWR_INT_ED_3
    );
    if interesting && WRITE_LOG.allow() {
        crate::kprintln!("vic: write IPA={:#010x} <- {:#010x}", ipa, value);
    }
    let mut match_reprogrammed = false;
    match ipa {
        // ---- Stateful in Einstein -------------------------------------
        // HighSpeedClck: Einstein has no specific write handler in
        // TMemory.cpp Bank #3 write path; falls to default no-op.
        K_HDWR_HIGH_SPEED_CLCK => { /* no-op per Einstein default */ }
        // Tick counter: same — no Einstein write handler. The kernel
        // doesn't actually need to set it; ticks come from the timer.
        K_HDWR_TICKS => { /* no-op per Einstein default */ }
        // Calendar: Einstein TMemory.cpp:1855-1857 calls SetRealTimeClock
        // which mutates host time tracking. We model the calendar as
        // a host-clock-derived value and don't have a "set" path; the
        // kernel rarely writes this. Accept silently — diverges from
        // Einstein only in the case where the guest sets the RTC and
        // then reads it back, which doesn't happen in normal boot.
        K_HDWR_CALENDAR_REG => { /* no-op (would call host SetRealTimeClock in Einstein) */ }
        // Alarm: Einstein TMemory.cpp:1858-1860 calls SetAlarm which
        // stores the value. Match that. Clear our edge-detect latch so
        // a new match can fire.
        K_HDWR_ALARM_REG => {
            s.alarm_reg = value;
            s.alarm_fired = false;
        }
        // MatchReg{0..3}: Einstein TMemory.cpp:1861-1881 calls
        // SetTimerMatchRegister which updates the match value and
        // recomputes the next deadline. We re-arm the EL2 timer via
        // timer::rearm() at the end.
        K_HDWR_MATCH_0 => {
            s.match_reg[0] = value;
            s.match_fired &= !0b0001;
            match_reprogrammed = true;
        }
        K_HDWR_MATCH_1 => {
            s.match_reg[1] = value;
            s.match_fired &= !0b0010;
            match_reprogrammed = true;
        }
        K_HDWR_MATCH_2 => {
            s.match_reg[2] = value;
            s.match_fired &= !0b0100;
            match_reprogrammed = true;
        }
        K_HDWR_MATCH_3 => {
            s.match_reg[3] = value;
            s.match_fired &= !0b1000;
            match_reprogrammed = true;
        }
        // IntCtrlReg: Einstein TMemory.cpp:1882-1884 calls
        // SetIntCtrlReg which stores the value (TInterruptManager.cpp).
        K_HDWR_INT_CTRL => {
            let before = s.int_ctrl;
            s.int_ctrl = value;
            // Sound-DMA enable-bit edges, first few only: shows when
            // the kernel arms/disarms its sound IRQ source.
            if ((before ^ value) & INT_DMA_CH5) != 0 {
                if VIC_DMA_CTRL_LOG.allow() {
                    crate::kprintln!(
                        "vic: int_ctrl {:#x} -> {:#x} (dma5 {})",
                        before,
                        value,
                        if value & INT_DMA_CH5 != 0 {
                            "on"
                        } else {
                            "off"
                        }
                    );
                }
            }
        }
        // IntClear: Einstein TMemory.cpp:1885-1887 calls ClearInterrupts
        // which does `mIntRaised &= ~inMask`. Match that.
        K_HDWR_INT_CLEAR => {
            let before = s.int_present;
            s.int_present &= !value;
            // Sound-DMA acks, first few + 1-in-64: the counterpart of
            // `vic: raise dma` — pairs of raise/clear prove the guest
            // is servicing the sound IRQ.
            if value & INT_DMA_CH5 != 0 {
                if VIC_DMA_CLEAR_LOG.allow_or_every(64) {
                    crate::kprintln!(
                        "vic: clear dma value={:#x} ipres {:#x} -> {:#x}",
                        value,
                        before,
                        s.int_present
                    );
                }
            }
        }
        // FIQMask: Einstein TMemory.cpp:1888-1890 calls SetFIQMask.
        K_HDWR_FIQ_MASK => {
            let before = s.fiq_mask;
            s.fiq_mask = value;
            // Sound-DMA FIQ-routing edges, first few only.
            if ((before ^ value) & INT_DMA_CH5) != 0 {
                if VIC_DMA_FIQ_LOG.allow() {
                    crate::kprintln!("vic: fiq_mask {:#x} -> {:#x}", before, value);
                }
            }
        }
        // IntEDReg{1..3}: Einstein TMemory.cpp:1891-1899 calls
        // SetIntEDReg{1..3}.
        K_HDWR_INT_ED_1 => s.int_ed_1 = value,
        K_HDWR_INT_ED_2 => s.int_ed_2 = value,
        K_HDWR_INT_ED_3 => s.int_ed_3 = value,
        // GPIO_E (Ctrl): Einstein TMemory.cpp:1898-1900 calls
        // SetGPIOCtrlReg which stores the new ctrl value.
        K_HDWR_GPIO_E => s.gpio_e = value,
        // GPIO_C (Clear): Einstein TMemory.cpp:1901-1902 calls
        // ClearGPIO which does `mGPIORaised &= ~inMask` — the GPIO
        // raised register, not int_present.
        K_HDWR_GPIO_C => s.gpio_r &= !value,

        // ---- Stateful in Einstein for READ, but no write handler ------
        // These addresses have a read handler in Einstein (returning
        // some live state that's mutated by other paths — RaiseInterrupt,
        // RaiseGPIO, etc.) but no write handler — so writes fall through
        // to the "unknown bank #3" write default at TMemory.cpp:1903-1913
        // and silently drop. The kernel may try to write here through
        // a "convenient name" path; match the silent drop instead of
        // halting.
        K_HDWR_INT_PRESENT | K_HDWR_GPIO_R => { /* drop per Einstein */ }

        // ---- Not modeled by Einstein at all → silent drop --------------
        // These addresses fall through to Einstein's "unknown bank #3"
        // write default at TMemory.cpp:1903-1913 (FLogLine + drop). Match
        // that — no state change, no error.
        K_HDWR_P0F110000 | K_HDWR_P0F111400 | K_HDWR_P0F180400 | K_HDWR_P0F185000
        | K_HDWR_GPIO_CC00 | K_HDWR_GPIO_D000 | K_HDWR_GPIO_D800 | K_HDWR_GPIO_DC00
        | K_HDWR_GPIO_E000 | K_HDWR_GPIO_E800 | K_HDWR_GPIO_EC00 => { /* drop per Einstein */ }

        _ => halt_vic_unknown("write", ipa, value),
    }
    if match_reprogrammed {
        // A match register changed — recompute the nearest deadline and
        // reprogram CNTHP_CVAL_EL2 (via the installed `hv::timer::rearm`
        // sink) so the async timer path delivers.
        (match_rearm())();
    }
}

/// Loud halt for an access inside a VIC-policy window that no match
/// arm above recognises. Per Phase A, extend the model deliberately
/// (with the Einstein cross-reference) rather than silently stubbing.
fn halt_vic_unknown(op: &'static str, ipa: u64, value: u32) -> ! {
    crate::kprintln!();
    crate::kprintln!(
        "*** vic::{} IPA={:#010x} val={:#010x} — inside a VIC window but not a modelled register ***",
        op, ipa, value
    );
    crate::kprintln!(
        "  (add the register to peripherals/vic.rs with its Einstein \
         cross-reference, or fix the layout window.)"
    );
    crate::arch::cpu::halt();
}
