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
//!    `0x0a 0x00` — cache the interrupt-IN endpoint, hand it to the
//!    DWC2 IRQ-driven path (`start_int_in`), and enable BCM2835 GPU
//!    source 9 so the panel's reports arrive as USB IRQs.
//! 2. `on_usb_irq` (called from `trap_irq`'s slim USB fast path the
//!    instant a report completes) harvests the 56-byte Report ID 1
//!    frame, parses slot 0 (tip + X + Y), compares against the
//!    previous report, and translates any change into a [`PenEvent`]
//!    inserted into our internal ring — no polling, so a report is
//!    never dropped because the guest happened not to be trapping.
//! 3. `drain_into_queue` runs over the ring on the same IRQ and feeds
//!    Einstein-format samples to the host_io pen queue.
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
    // Run bus enumeration once, then arm the IRQ-driven interrupt-IN.
    // Failures here aren't fatal — the hypervisor continues with no
    // pen input, the same state as a `pi-bare-metal-display` build.
    let r = dwc2::with(|host| {
        let _ = host.port_reset_and_speed()?;
        let dev = enumerate::enumerate(host)?;
        attach(host, &dev)?;
        // `host` is the concrete `&mut Dwc2` here, so we can hand the
        // cached endpoint to the IRQ-driven path directly.
        let (addr, ep, mps) = with_state(|s| (s.addr, s.in_ep_addr, s.in_ep_mps));
        host.start_int_in(addr, ep, mps)
    });
    match r {
        Ok(()) => {
            // Route the DWC2 IRQ line (BCM2835 GPU source 9) to the CPU
            // so harvested reports arrive as USB IRQs into `trap_irq`.
            #[cfg(feature = "platform-raspi3b")]
            crate::platform::enable_bcm2835_irq(9);
            kprintln!("input-mtouch: attached (IRQ-driven)");
        }
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

/// Trap-tail pump. Touchscreen input is IRQ-driven (`on_usb_irq`),
/// so there is nothing to poll here. Kept as a no-op so the shared
/// `input::pump` seam and its call sites stay backend-agnostic.
pub fn pump() {}

/// Harvest and dispatch one touchscreen report from the USB IRQ.
/// Called from `trap_irq`'s slim USB fast path (ISR context, possibly
/// nested in an EL2 `with_irqs_unmasked` window). Returns `true` if a
/// pen sample was enqueued onto the host_io queue, so the caller can
/// reflect the freshly-raised `INT_TABLET` into the guest's vIRQ on
/// this same trap exit.
pub fn on_usb_irq() -> bool {
    let mut buf = [0u8; REPORT_BUF_LEN];
    let n = match dwc2::service_int_in_irq(&mut buf) {
        Some(n) => n,
        None => return false, // NAK / error / not our channel
    };
    if n >= 6 {
        // First few reports get a one-shot dump to sanity-check the
        // IRQ-driven pipe without opting into log_irqs.
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
    let mut src = Drain;
    super::drain_into_queue(&mut src)
}

/// Drain adapter — `super::drain_into_queue` wants a `PenSource`
/// reference; we use a zero-sized type so a fresh one can be made
/// on every harvest.
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
