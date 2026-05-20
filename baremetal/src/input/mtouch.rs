//! TSTP MTouch USB digitizer backend.
//!
//! Targets the panel characterised in `docs/MTOUCH.md`: VID 0x0416 /
//! PID 0xC168, full-speed, single configuration, two HID interfaces.
//! Interface 0 is the digitizer (EP 0x81 interrupt-IN, 64-byte
//! reports, ~16 ms cadence); we don't touch interface 1.
//!
//! Lifecycle:
//!
//! 1. `init` enumerates the bus once at boot. If the device is the
//!    MTouch, we issue the activation handshake
//!    `GET_REPORT(Feature, ReportID=3, length=2)` — expected reply
//!    `0x0a 0x00` — and cache the interrupt-IN endpoint.
//! 2. `pump` (called from `input::pump` on every trap/IRQ exit)
//!    issues one polled `interrupt_in` on EP 0x81. If we get a
//!    full 56-byte Report ID 1 frame we parse slot 0 (tip + X + Y),
//!    compare against the previous report, and translate any change
//!    into a [`PenEvent`] inserted into our internal ring.
//! 3. `drain_into_queue` runs over the ring on the same tick and
//!    feeds Einstein-format samples to the host_io pen queue.
//!
//! The activation handshake matches `hid-multitouch`'s standard
//! "Contact Count Max" probe — see Linux
//! `drivers/hid/hid-multitouch.c` `mt_feature_mapping`. Without it
//! the panel stays mute.

use super::{calibrate, PenEvent, PenSource};
use crate::kprintln;
use crate::usb::class::hid;
use crate::usb::enumerate::{self, EndpointDescriptor, UsbDevice};
use crate::usb::host::{dwc2, UsbHostController};
use crate::usb::UsbError;

use core::sync::atomic::{AtomicBool, Ordering};

const TSTP_MTOUCH_VID: u16 = 0x0416;
const TSTP_MTOUCH_PID: u16 = 0xC168;

/// EP 0x81 reports are 56 bytes per `docs/MTOUCH.md`. wMaxPacketSize
/// is 64; we always read up to 64 to match.
const REPORT_BUF_LEN: usize = 64;

/// Internal ring of pending pen events between `pump` (producer)
/// and `drain_into_queue` (consumer). 16 slots is plenty — moves
/// translate to one event each and a single touch produces O(panel
/// height) moves.
const RING_SLOTS: usize = 16;

struct State {
    attached: bool,
    interface: u8,
    ep0_mps: u8,
    addr: u8,
    in_ep_addr: u8,
    in_ep_mps: u16,
    /// Consecutive hard transfer errors from `pump`'s interrupt-IN.
    /// Reset on any success/idle poll; at `DETACH_ERROR_THRESHOLD`
    /// the device is declared gone (panel reboot) and polling stops.
    consec_errors: u32,
    /// Last-seen (pressed, x, y) so we can suppress idle keep-alive
    /// reports (the panel emits ID 1 at ~16 ms regardless of whether
    /// anything changed — see `docs/MTOUCH.md` §Behavior notes).
    last_pressed: bool,
    last_x: u16,
    last_y: u16,
    /// Pending-event ring.
    ring: [Option<PenEvent>; RING_SLOTS],
    ring_head: usize,
    ring_tail: usize,
}

impl State {
    const fn new() -> Self {
        Self {
            attached: false,
            interface: 0,
            ep0_mps: 8,
            addr: 0,
            in_ep_addr: 0x81,
            in_ep_mps: 64,
            consec_errors: 0,
            last_pressed: false,
            last_x: 0,
            last_y: 0,
            ring: [const { None }; RING_SLOTS],
            ring_head: 0,
            ring_tail: 0,
        }
    }

    fn ring_push(&mut self, ev: PenEvent) {
        let next = (self.ring_head + 1) % RING_SLOTS;
        if next == self.ring_tail {
            // Drop on overflow — a missed move is preferable to a
            // stalled producer.
            return;
        }
        self.ring[self.ring_head] = Some(ev);
        self.ring_head = next;
    }

    fn ring_pop(&mut self) -> Option<PenEvent> {
        if self.ring_tail == self.ring_head {
            return None;
        }
        let ev = self.ring[self.ring_tail].take();
        self.ring_tail = (self.ring_tail + 1) % RING_SLOTS;
        ev
    }
}

struct StateCell(core::cell::UnsafeCell<State>);
// SAFETY: single-core EL2; access funnels through `with_state` from
// the trap-return tail.
unsafe impl Sync for StateCell {}

static STATE: StateCell = StateCell(core::cell::UnsafeCell::new(State::new()));
static INIT_DONE: AtomicBool = AtomicBool::new(false);

fn with_state<R, F: FnOnce(&mut State) -> R>(f: F) -> R {
    // SAFETY: see StateCell.
    let s = unsafe { &mut *STATE.0.get() };
    f(s)
}

pub fn init() {
    if INIT_DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    // Run bus enumeration once. Failures here aren't fatal — the
    // hypervisor continues with no pen input, which is the same
    // state as a `pi-bare-metal-display` build.
    let r = dwc2::with(|host| {
        let _ = host.port_reset_and_speed()?;
        let dev = enumerate::enumerate(host)?;
        attach(host, &dev)
    });
    match r {
        Ok(()) => kprintln!("input-mtouch: attached"),
        Err(UsbError::NotReady) => {
            kprintln!("input-mtouch: DWC2 not ready; pen input disabled");
        }
        Err(e) => kprintln!("input-mtouch: attach failed: {:?}", e),
    }
}

fn attach<H: crate::usb::host::UsbHostController>(
    host: &mut H,
    dev: &UsbDevice,
) -> crate::usb::UsbResult<()> {
    if dev.vendor_id() != TSTP_MTOUCH_VID || dev.product_id() != TSTP_MTOUCH_PID {
        kprintln!(
            "input-mtouch: ignoring device VID={:#06x} PID={:#06x}",
            dev.vendor_id(),
            dev.product_id()
        );
        return Err(UsbError::NotReady);
    }
    // Interface 0 is the digitizer; look up its interrupt-IN endpoint.
    let in_ep: EndpointDescriptor = match dev.first_in_endpoint(0) {
        Some(ep) => ep,
        None => {
            kprintln!("input-mtouch: interface 0 has no IN endpoint");
            return Err(UsbError::NotReady);
        }
    };

    // Activation handshake — GET_REPORT(Feature, ReportID=3, len=2).
    // Per HID 1.11 §8.6, when the device has multiple Report IDs the
    // reply is prefixed with the Report ID byte, so we expect
    // [0x03, 0x0A] ([ReportID=3, ContactCountMax=10]) — *not* the
    // [0x0a, 0x00] documented in MTOUCH.md, which was captured
    // through hid-multitouch (which strips the ID byte).
    let mut feat = [0u8; 2];
    let n = hid::get_report(
        host,
        dev.address,
        dev.device.max_packet_size0,
        0,
        hid::HID_REPORT_FEATURE,
        3,
        &mut feat,
    )?;
    if n < 2 {
        kprintln!("input-mtouch: short activation reply ({} bytes)", n);
    } else if feat != [0x03, 0x0A] {
        kprintln!(
            "input-mtouch: unusual activation reply {:?} (expected [3, 10])",
            feat
        );
    }
    // No SET_IDLE: not required by MTouch (the GET_REPORT above
    // *is* the activation). Issuing it produces a transient
    // DATA_TGL_ERR on the STATUS stage with this panel firmware.

    with_state(|s| {
        s.attached = true;
        s.interface = 0;
        s.ep0_mps = dev.device.max_packet_size0;
        s.addr = dev.address;
        s.in_ep_addr = in_ep.address;
        s.in_ep_mps = in_ep.max_packet_size;
    });
    Ok(())
}

/// Consecutive hard-error count at which the device is declared
/// detached. The dwc2 core already retries 3× internally per
/// attempt, so 8 attempts ≈ a sustained ~128 ms outage at the 16 ms
/// pump cadence — far beyond any transient the panel produces when
/// alive.
const DETACH_ERROR_THRESHOLD: u32 = 8;

pub fn pump() {
    // Cheap idle path when no device attached.
    if !with_state(|s| s.attached) {
        return;
    }
    let (addr, ep_addr, mps) =
        with_state(|s| (s.addr, s.in_ep_addr, s.in_ep_mps));
    let mut buf = [0u8; REPORT_BUF_LEN];
    let n = match dwc2::with(|host| host.interrupt_in(addr, ep_addr, mps, &mut buf)) {
        Ok(n) => {
            with_state(|s| s.consec_errors = 0);
            n
        }
        // Idle NAK / no data this frame — the normal quiet-panel case.
        Err(UsbError::Timeout) => {
            with_state(|s| s.consec_errors = 0);
            0
        }
        // Port down: the panel rebooted out from under us (its USB
        // hub function dies with it). Detach immediately — every
        // further attempt against a downed port would burn the full
        // transfer timeout inside the trap tail and starve the guest.
        Err(UsbError::NotReady) => {
            detach("port down (panel reset?)");
            return;
        }
        // Hard wire errors (XACT_ERR after the core's own retries,
        // babble, AHB): tolerate transients, detach when sustained.
        Err(e) => {
            let errs = with_state(|s| {
                s.consec_errors = s.consec_errors.saturating_add(1);
                s.consec_errors
            });
            if errs >= DETACH_ERROR_THRESHOLD {
                kprintln!("input-mtouch: {} consecutive errors (last {:?})", errs, e);
                detach("persistent transfer errors");
            }
            return;
        }
    };
    if n >= 6 {
        // First few successful packets get a one-shot byte dump so
        // we can sanity-check the USB pipe without opting into
        // log_irqs. Self-throttles after `MAX` reports.
        use core::sync::atomic::{AtomicUsize, Ordering};
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        const MAX: usize = 4;
        let k = SEEN.fetch_add(1, Ordering::Relaxed);
        if k < MAX {
            kprintln!(
                "input-mtouch: report #{} ({} bytes): id={:#x} tip={} x={} y={}",
                k, n,
                buf[0],
                buf[1] & 0x01,
                u16::from_le_bytes([buf[2], buf[3]]),
                u16::from_le_bytes([buf[4], buf[5]]),
            );
        }
        decode_and_enqueue(&buf[..n]);
    }
    // Drain any new events into the host_io queue.
    let mut src = Drain;
    super::drain_into_queue(&mut src);
}

/// Declare the device gone and stop polling it. Touch input is lost
/// until the next boot — hot re-enumeration after the port comes
/// back is not implemented yet (needs a port reset + address/config
/// replay; tracked in the change description). The point here is
/// damage containment: a dead device must not keep consuming
/// trap-tail time.
fn detach(why: &str) {
    with_state(|s| {
        s.attached = false;
        s.consec_errors = 0;
    });
    kprintln!("input-mtouch: detached — {}", why);
}

/// Drain adapter — `super::drain_into_queue` wants a `PenSource`
/// reference; we use a zero-sized type so a fresh one can be made
/// on every `pump`.
struct Drain;

impl PenSource for Drain {
    fn poll(&mut self) -> Option<PenEvent> {
        with_state(|s| s.ring_pop())
    }
}

fn decode_and_enqueue(report: &[u8]) {
    if report[0] != 0x01 {
        // Not a Report ID 1 frame; ignore.
        return;
    }
    let pressed = (report[1] & 0x01) != 0;
    let x = u16::from_le_bytes([report[2], report[3]]);
    let y = u16::from_le_bytes([report[4], report[5]]);

    with_state(|s| {
        let same = pressed == s.last_pressed && x == s.last_x && y == s.last_y;
        if same {
            return;
        }
        let prev_pressed = s.last_pressed;
        s.last_pressed = pressed;
        s.last_x = x;
        s.last_y = y;

        match (prev_pressed, pressed) {
            (false, true) => {
                if let Some((nx, ny)) = calibrate::panel_to_newton(x, y) {
                    s.ring_push(PenEvent::Down { x: nx, y: ny });
                    // One log line per tap onset — not gated on
                    // log_irqs because tap-cadence is bounded by
                    // human input, doesn't flood. Moves stay silent.
                    kprintln!(
                        "input-mtouch: Down at panel ({},{}) -> newton ({},{})",
                        x, y, nx, ny
                    );
                } else {
                    kprintln!(
                        "input-mtouch: Down at panel ({},{}) DROPPED (letterbox)",
                        x, y
                    );
                }
            }
            (true, true) => {
                if let Some((nx, ny)) = calibrate::panel_to_newton(x, y) {
                    s.ring_push(PenEvent::Move { x: nx, y: ny });
                }
            }
            (true, false) => {
                s.ring_push(PenEvent::Up);
                kprintln!("input-mtouch: Up");
            }
            (false, false) => {}
        }
    });
}
