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
    int_present: u32,       // 0x0F183000  mIntRaised
    int_ctrl: u32,          // 0x0F183400  mIntCtrlReg
    // int_clear is write-only in Einstein; clears bits in mIntRaised.
    fiq_mask: u32,          // 0x0F183C00  mFIQMask
    int_ed_1: u32,          // 0x0F184000  mIntEDReg1
    int_ed_2: u32,          // 0x0F184400  mIntEDReg2
    int_ed_3: u32,          // 0x0F184800  mIntEDReg3
    match_reg: [u32; 4],    // 0x0F182000/400/800/C00  mMatchReg[0..3]
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
    alarm_reg: u32,         // 0x0F181400  via SetAlarm/GetAlarm
    alarm_fired: bool,
    // GPIO interrupt registers (Einstein TInterruptManager.h:510,513).
    gpio_r: u32,            // 0x0F18C000  mGPIORaised
    gpio_e: u32,            // 0x0F18C400  mGPIOCtrlReg
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
const INT_RTC_ALARM: u32 = 0x0000_0004;
const INT_TIMER_0: u32 = 0x0000_0008;
const INT_TIMER_1: u32 = 0x0000_0010;
const INT_TIMER_2: u32 = 0x0000_0020;
const INT_TIMER_3: u32 = 0x0000_0040;
const INT_DMA_CH3: u32 = 0x0000_0400;   // sound input
const INT_DMA_CH5: u32 = 0x0000_1000;   // sound output / tablet rcv
pub const INT_GPIO: u32 = 0x0100_0000;

/// Public raiser: OR `mask` into `int_present`. The next `update_virq`
/// (called at every sync-trap exit and after `timer::on_irq`) reflects
/// the change into HCR_EL2.VI / VF, so the guest takes a virtual IRQ
/// on the next ERET if the unmask gates allow it.
///
/// Used by external raisers — DMA channel completion (`dma::write`),
/// GPIO line events (HVC test trigger / future native-event hooks),
/// RTC alarm match, etc. Timer matches go through the existing
/// `poll_timer_matches` edge-detect path.
pub fn raise(mask: u32) {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *VIC.0.get() };
    s.int_present |= mask;
}

/// Diagnostic: force-raise the two sound-DMA IRQ bits (DMA channel 3 and
/// channel 5). Called from a wedge-detector in `trap_irq` to test the
/// hypothesis that the Phase-B stall after `TSoundServer::TheMain`
/// stack-collision is the kernel waiting for a sound-DMA-complete IRQ
/// that we never fire. Einstein's `TNullSoundManager::StartOutput` /
/// `ScheduleOutputBuffer` raise the same bits via
/// `mInterruptManager->RaiseInterrupt(mOutputIntMask)`; we emulate it
/// here without modeling the actual DMA transfer.
pub fn inject_sound_dma_irq() {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *VIC.0.get() };
    s.int_present |= INT_DMA_CH3 | INT_DMA_CH5;
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

/// Diagnostic: raw `int_present` register.
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
const K_HDWR_GPIO_CC00: u64 = 0x0F18_CC00;
const K_HDWR_GPIO_D000: u64 = 0x0F18_D000;
const K_HDWR_GPIO_D800: u64 = 0x0F18_D800;
const K_HDWR_GPIO_DC00: u64 = 0x0F18_DC00;
const K_HDWR_GPIO_E000: u64 = 0x0F18_E000;
const K_HDWR_GPIO_E800: u64 = 0x0F18_E800;
const K_HDWR_GPIO_EC00: u64 = 0x0F18_EC00;

/// Synthetic Newton-tick counter. Bumped by `tick_advance` from the
/// hypervisor's tick-page update path — once on every guest sync trap
/// and once on every CNTHP heartbeat. **Decoupled from wall clock.**
///
/// Why not wall-anchored: under QEMU TCG with `--features trace,quiet`
/// and the shadow-stub UDF emulator we execute ~100× fewer guest
/// instructions per host wall-second than Einstein's JIT does (each
/// HVC trampoline alone costs ~30 µs). When the kernel's polling loops
/// (TBIOInterface::WaitBIOStatus, TDelayTimer::TimedOut callers, etc.)
/// arm a wall-anchored Newton-tick deadline, our wall-clock-derived
/// tick value crosses that deadline after far fewer poll iterations
/// than Einstein's, perturbing the kernel's heap allocator interleave
/// and steering `__nw__FUi(184)` towards a VA range that aliases
/// pckm's stack page. See INVESTIGATION.md.
///
/// The synthetic clock advances proportional to **guest progress**
/// (each sync trap ≈ a fixed slice of guest instructions), so
/// timeout-bounded polling loops iterate about as many times in our
/// run as in Einstein's, regardless of how slowly the host wall clock
/// is moving.
///
/// Δ per `tick_advance` call is calibrated empirically. Einstein's
/// `TBIOInterface::WaitBIOStatus` polls `TDelayTimer::TimedOut` 65
/// times against a 400-tick threshold, i.e. ≈ 6.15 ticks per poll
/// iteration; rounded up to 8 to allow some slack.
///
/// Calendar / RTC: we deliberately do *not* try to keep this clock
/// synchronised with wall time. `calendar_seconds()` still reads
/// CNTPCT directly so RTC reads return plausible "seconds since 1904"
/// values, but the kernel's tick-domain math no longer agrees with
/// those seconds (a 1-second wall interval will not advance the tick
/// register by 3,686,400 in this scheme). Real-time-clock semantics
/// are not load-bearing for the Phase B boot trajectory.
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
const TICK_ADVANCE_PER_HEARTBEAT: u32 = 1024;

/// Synthetic-tick reader. Returns the current count without advancing
/// it; advancement happens via `tick_advance` from the tick-page
/// update path.
pub fn ticks() -> u32 {
    SYNTH_TICKS.load(Ordering::Acquire)
}

/// Bump SYNTH_TICKS by the sync-trap delta. Called from
/// `stage2::tick_page::update_from_sync_trap` (= every guest sync trap
/// via `trap_sync_lower_aarch32`).
pub fn tick_advance_sync_trap() -> u32 {
    let prev = SYNTH_TICKS.fetch_add(TICK_ADVANCE_PER_TRAP, Ordering::AcqRel);
    prev.wrapping_add(TICK_ADVANCE_PER_TRAP)
}

/// Bump SYNTH_TICKS by the heartbeat delta. Called from
/// `timer::on_irq` (every CNTHP heartbeat) so that non-trapping
/// busy-wait loops still see ticks advance.
pub fn tick_advance_heartbeat() -> u32 {
    let prev = SYNTH_TICKS.fetch_add(TICK_ADVANCE_PER_HEARTBEAT, Ordering::AcqRel);
    prev.wrapping_add(TICK_ADVANCE_PER_HEARTBEAT)
}

/// Back-compat alias for the sync-trap path. Older callers used
/// `tick_advance()` for both paths; new code should pick the
/// matching variant.
pub fn tick_advance() -> u32 {
    tick_advance_sync_trap()
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
pub fn heartbeat_tick_update() {
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
        | K_HDWR_GPIO_C
        | K_HDWR_GPIO_CC00
        | K_HDWR_GPIO_D000
        | K_HDWR_GPIO_D800
        | K_HDWR_GPIO_DC00
        | K_HDWR_GPIO_E000
        | K_HDWR_GPIO_E800
        | K_HDWR_GPIO_EC00 => true,
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
        // ---- Stateful in Einstein -------------------------------------
        // PlatformVers: TPlatformManager::GetVersion() returns 5
        // (Emulator/Platform/TPlatformManager.cpp:110).
        K_HDWR_PLATFORM_VERS => 5,
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

        // ---- Not modeled by Einstein → returns 0 by default ------------
        // Einstein TMemory.cpp Bank #3 read path (lines 803-960) has no
        // specific handler for these addresses; the unknown-bank-#3
        // default at lines 950-960 returns 0. Match that. The previous
        // round-trip behavior diverged whenever the kernel did
        // read-modify-write here (TGPIOInterface::DisableInterrupt etc.)
        // — but Einstein's r-m-w sees 0 every read, so the user-visible
        // effect must be 0 here too.
        K_HDWR_P0F110000
        | K_HDWR_P0F111400
        | K_HDWR_P0F180400
        | K_HDWR_P0F185000
        | K_HDWR_GPIO_C
        | K_HDWR_GPIO_CC00
        | K_HDWR_GPIO_D000
        | K_HDWR_GPIO_D800
        | K_HDWR_GPIO_DC00
        | K_HDWR_GPIO_E000
        | K_HDWR_GPIO_E800
        | K_HDWR_GPIO_EC00 => 0,

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
        K_HDWR_MATCH_0 => { s.match_reg[0] = value; s.match_fired &= !0b0001; match_reprogrammed = true; }
        K_HDWR_MATCH_1 => { s.match_reg[1] = value; s.match_fired &= !0b0010; match_reprogrammed = true; }
        K_HDWR_MATCH_2 => { s.match_reg[2] = value; s.match_fired &= !0b0100; match_reprogrammed = true; }
        K_HDWR_MATCH_3 => { s.match_reg[3] = value; s.match_fired &= !0b1000; match_reprogrammed = true; }
        // IntCtrlReg: Einstein TMemory.cpp:1882-1884 calls
        // SetIntCtrlReg which stores the value (TInterruptManager.cpp).
        K_HDWR_INT_CTRL => s.int_ctrl = value,
        // IntClear: Einstein TMemory.cpp:1885-1887 calls ClearInterrupts
        // which does `mIntRaised &= ~inMask`. Match that.
        K_HDWR_INT_CLEAR => s.int_present &= !value,
        // FIQMask: Einstein TMemory.cpp:1888-1890 calls SetFIQMask.
        K_HDWR_FIQ_MASK => s.fiq_mask = value,
        // IntEDReg{1..3}: Einstein TMemory.cpp:1891-1899 calls
        // SetIntEDReg{1..3}.
        K_HDWR_INT_ED_1 => s.int_ed_1 = value,
        K_HDWR_INT_ED_2 => s.int_ed_2 = value,
        K_HDWR_INT_ED_3 => s.int_ed_3 = value,
        // GPIO_E (Ctrl): Einstein TMemory.cpp:1898-1900 calls
        // SetGPIOCtrlReg which stores the new ctrl value.
        K_HDWR_GPIO_E => s.gpio_e = value,
        // GPIO_C (Clear): Einstein TMemory.cpp:1901-1902 calls
        // ClearGPIO which does `mGPIORaised &= ~inMask`. Previously we
        // applied this to int_present (wrong register). Match Einstein.
        K_HDWR_GPIO_C => s.gpio_r &= !value,

        // ---- Not modeled by Einstein → silent drop --------------------
        // These addresses fall through to Einstein's "unknown bank #3"
        // write default at TMemory.cpp:1903-1913 (FLogLine + drop). Match
        // that — no state change, no error.
        K_HDWR_P0F110000
        | K_HDWR_P0F111400
        | K_HDWR_P0F180400
        | K_HDWR_P0F185000
        | K_HDWR_GPIO_CC00
        | K_HDWR_GPIO_D000
        | K_HDWR_GPIO_D800
        | K_HDWR_GPIO_DC00
        | K_HDWR_GPIO_E000
        | K_HDWR_GPIO_E800
        | K_HDWR_GPIO_EC00 => { /* drop per Einstein */ }

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
