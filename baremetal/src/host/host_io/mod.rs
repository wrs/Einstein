//! Host-side display + input plumbing.
//!
//! Two roles:
//!
//! 1. **Outbound display.** `screen::blit` calls its installed blit
//!    sink — wired by `main.rs` to [`push_guest_blit`] — each time it
//!    paints pixels. The active backend forwards a `BlitEvent` plus
//!    its 2 bpp packed payload to whatever sink it owns (host viewer
//!    over semihosting IPC for QEMU/FVP, the VC framebuffer on a
//!    real Pi).
//!
//! 2. **Inbound pen input.** The active backend's [`pump_input`] runs
//!    from the trap-return tail (`hv::trap`); it pulls pen events off
//!    its source and feeds them into [`queue::enqueue_pen_sample`],
//!    which raises `INT_TABLET`. `tablet::handle` subfn 0x16
//!    (`NativeGetSample`) drains via [`pop_pen_sample`].
//!
//! Backend selection: `build.rs::resolve_host_io_backend` picks one
//! from the `host-io-*` Cargo features (opt-in only — the features are
//! NOT in `default`, so `cargo run --features host-io-semihost` works
//! without `--no-default-features`). With no feature enabled the
//! resolver falls back to "null", which turns everything in here into
//! a no-op so guest-tests and CI runs behave as if no host IO were
//! compiled in. The resolver emits `cfg(nh_host_io_<chosen>)`; multiple
//! opt-ins are still a hard error. Each backend implements the
//! [`HostIo`] trait and exports a `static BACKEND`; the one cfg'd
//! `use` below is the only backend dispatch point (same shape as
//! `host::flash_persist`'s `FlashStore`).

pub mod queue;

#[cfg(nh_host_io_null)]
mod null;
#[cfg(nh_host_io_pi_fb)]
pub mod pi_fb;
#[cfg(nh_host_io_semihost)]
mod semihost;

/// Backend interface. Single-threaded EL2 callers; impls do not need
/// to be re-entrant. Backend asymmetries (a panel to report, a resume
/// repaint to synthesise) live in the per-backend overrides of the
/// defaulted methods, not in cfg'd shim code.
pub trait HostIo: Sync {
    /// One-time setup: open transport, send a hello, adopt the splash
    /// framebuffer, …. Called from `kmain` once `vic::init` has
    /// returned.
    fn init(&self);

    /// Called after a snapshot restore, before `eret_to_restored`.
    /// Backends with a display sink push a synthesised full-screen
    /// repaint here (see [`push_full_repaint`]) so their sink re-syncs
    /// with the restored GUEST_FB; input backends drop timing-stale
    /// pending events.
    fn on_resume(&self);

    /// Forward one blit to the host. Must be non-blocking —
    /// `screen::blit` calls this from a sync trap with the guest
    /// stalled. Backends that can't keep up drop events instead of
    /// blocking.
    fn push_blit(&self, ev: &BlitEvent, payload: &[u8]);

    /// True when this backend consumes the packed 2 bpp payload slice
    /// passed to [`push_blit`]. Backends that render from GUEST_FB
    /// directly (pi_fb) or drop blits (null) return false, which lets
    /// `screen::blit` skip assembling the payload entirely; the
    /// `BlitEvent` metadata still flows (with `payload_len` = 0).
    fn wants_payload(&self) -> bool {
        true
    }

    /// Pump the backend's input transport: drain newly-arrived host
    /// pen events, enqueue them as Newton-format samples, and raise
    /// `INT_TABLET`. Called from the trap-return tail (`hv::trap`).
    fn pump_input(&self);

    /// The Newton guest screen geometry `(width, height)` this backend
    /// mandates, or `None` to keep `peripherals::screen`'s model
    /// default (320×480). `main.rs` pulls this once at boot and pushes
    /// it into the screen model.
    fn panel_geometry(&self) -> Option<(u32, u32)> {
        None
    }

    /// Where the Newton surface lands on the backend's physical panel,
    /// for the touch-input calibration transform. `None` when the
    /// backend has no physical panel (null, semihost) or the panel
    /// isn't up yet — touch input then no-ops. Compiled exactly where
    /// its only consumer — `input::calibrate` — is.
    #[cfg(nh_input_mtouch)]
    fn painted_region(&self) -> Option<PaintedRegion> {
        None
    }
}

/// Geometry of the painted Newton region on the backend's scan-out
/// surface, all in *surface* pixels. Produced by
/// [`HostIo::painted_region`]; consumed by `input::calibrate`.
///
/// The surface may be smaller than the physical panel mode (pi_fb's
/// VC-scaled surface, which the firmware/HVS upscales to the panel on
/// scan-out) — that's transparent to calibration because the whole
/// surface maps linearly onto the whole visible panel, so a linear
/// touch→surface map composed with offset/size below stays correct
/// in either case.
#[cfg(nh_input_mtouch)]
#[derive(Copy, Clone)]
pub struct PaintedRegion {
    /// Full scan-out surface size (= panel mode size for a
    /// panel-native surface).
    pub panel_w: u32,
    pub panel_h: u32,
    /// Top-left of the painted Newton region inside the surface.
    pub offset_x: u32,
    pub offset_y: u32,
    /// Painted Newton region size (1:1 = Newton size on a VC-scaled
    /// surface; after aspect-preserving scale on a native one).
    pub painted_w: u32,
    pub painted_h: u32,
}

#[cfg(nh_host_io_null)]
use self::null::BACKEND;
#[cfg(nh_host_io_pi_fb)]
use self::pi_fb::BACKEND;
#[cfg(nh_host_io_semihost)]
use self::semihost::BACKEND;

pub const BLIT_KIND_BLIT: u8 = 1;
/// Kind byte for the resume-time repaint — only produced by backends
/// with a display sink (see [`push_full_repaint`]).
#[cfg(any(nh_host_io_semihost, nh_host_io_pi_fb))]
pub const BLIT_KIND_FULL_REPAINT: u8 = 2;

/// Wire-format header for one blit forwarded to the host viewer.
/// Followed immediately by `payload_len` bytes of 2 bpp packed pixels,
/// MSB-first (pixel 0 in bits 7..6 of byte 0). 24 bytes.
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct BlitEvent {
    pub kind: u8,
    pub mode: u8,
    pub bpp: u8,
    pub _pad: u8,
    pub src_left: u16,
    pub src_top: u16,
    pub src_right: u16,
    pub src_bottom: u16,
    pub dst_left: u16,
    pub dst_top: u16,
    pub dst_right: u16,
    pub dst_bottom: u16,
    pub row_bytes: u16,
    pub payload_len: u16,
}

const _: () = {
    assert!(core::mem::size_of::<BlitEvent>() == 24);
};

/// Wire-format pen event from the host viewer back to the hypervisor.
/// 8 bytes.
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct PenEvent {
    pub kind: u8,   // 1 = down, 2 = move, 3 = up
    pub _pad: u8,
    pub x: u16,
    pub y: u16,
    pub pressure: u16,
}

#[cfg(nh_host_io_semihost)]
pub const PEN_DOWN: u8 = 1;
#[cfg(nh_host_io_semihost)]
pub const PEN_MOVE: u8 = 2;
#[cfg(nh_host_io_semihost)]
pub const PEN_UP: u8 = 3;
/// Power-switch press from the host viewer. `x`, `y`, `pressure` are
/// ignored. Wakes the guest from PowerOff state via
/// `peripherals::vic::raise_power_switch` — equivalent to Einstein's
/// `TPlatformManager::SendPowerSwitchEvent` when the system is off.
#[cfg(nh_host_io_semihost)]
pub const POWER_SWITCH: u8 = 4;

const _: () = {
    assert!(core::mem::size_of::<PenEvent>() == 8);
};

/// One-time setup. Called from `kmain` once `vic::init` has returned.
pub fn init() {
    BACKEND.init();
}

/// Called after `snapshot::load_latest` restores guest state but
/// before `eret_to_restored`. Flushes the shared input queue, then
/// hands off to the backend (which pushes its own full-screen repaint
/// if it owns a display sink — see [`HostIo::on_resume`]).
pub fn on_resume() {
    queue::reset();
    BACKEND.on_resume();
}

/// Blit-sink adapter with the `peripherals::screen::BlitSink`
/// signature, installed into the screen model by `main.rs`. Wraps the
/// raw blit parameters in the viewer wire-format [`BlitEvent`]
/// (kind = [`BLIT_KIND_BLIT`]) and forwards to the active backend.
/// Rects are `(left, top, right, bottom)` in Newton pixels, matching
/// the `BlitEvent` field order.
pub fn push_guest_blit(
    mode: u8,
    bpp: u8,
    src: (u16, u16, u16, u16),
    dst: (u16, u16, u16, u16),
    row_bytes: u16,
    payload: &[u8],
) {
    let ev = BlitEvent {
        kind: BLIT_KIND_BLIT,
        mode,
        bpp,
        _pad: 0,
        src_left: src.0,
        src_top: src.1,
        src_right: src.2,
        src_bottom: src.3,
        dst_left: dst.0,
        dst_top: dst.1,
        dst_right: dst.2,
        dst_bottom: dst.3,
        row_bytes,
        payload_len: payload.len() as u16,
    };
    // Paint-cost accumulator (`nh_diag`) — counterpart of the
    // emulation-cost timer in `peripherals::screen::blit`.
    let t_paint = crate::diag::blit_timing::begin();
    BACKEND.push_blit(&ev, payload);
    crate::diag::blit_timing::PAINT.record_since(t_paint);
}

/// Synthesise and push a full-screen repaint of the guest framebuffer
/// (kind = [`BLIT_KIND_FULL_REPAINT`]). Used by display-owning
/// backends' `on_resume` so their sink re-syncs with the restored
/// GUEST_FB; the caller supplies its own notion of the Newton screen
/// geometry, so this shim stays geometry-free.
#[cfg(any(nh_host_io_semihost, nh_host_io_pi_fb))]
fn push_full_repaint(w: u32, h: u32, bpp: u32) {
    let row_bytes = (w * bpp).div_ceil(8);
    let fb_len = (row_bytes * h) as usize;
    // SAFETY: `fb_host_pa` is the base of the static GUEST_FB backing
    // (FRAMEBUFFER_SIZE = 2 MiB); every supported geometry keeps
    // fb_len ≪ 2 MiB.
    let payload = unsafe {
        core::slice::from_raw_parts(crate::hv::guest_mem::fb_host_pa() as *const u8, fb_len)
    };
    let ev = BlitEvent {
        kind: BLIT_KIND_FULL_REPAINT,
        mode: 0,
        bpp: bpp as u8,
        _pad: 0,
        src_left: 0,
        src_top: 0,
        src_right: w as u16,
        src_bottom: h as u16,
        dst_left: 0,
        dst_top: 0,
        dst_right: w as u16,
        dst_bottom: h as u16,
        row_bytes: row_bytes as u16,
        payload_len: payload.len() as u16,
    };
    BACKEND.push_blit(&ev, payload);
}

/// Pull a single pen sample off the input queue. Returns
/// `Some((packed_sample, ticks))` matching Einstein's
/// `TScreenManager::GetSample` semantics, or `None` if the queue is
/// empty.
pub fn pop_pen_sample() -> Option<(u32, u32)> {
    queue::pop()
}

/// Pump the backend's input transport — see [`HostIo::pump_input`].
pub fn pump_input() {
    BACKEND.pump_input();
}

/// Whether the active backend consumes blit payloads — see
/// [`HostIo::wants_payload`]. Pulled once by `main.rs` at boot and
/// installed into `peripherals::screen` alongside the blit sink.
pub fn wants_payload() -> bool {
    BACKEND.wants_payload()
}

/// The Newton screen geometry the active backend mandates — see
/// [`HostIo::panel_geometry`]. Pulled once by `main.rs` at boot.
pub fn panel_geometry() -> Option<(u32, u32)> {
    BACKEND.panel_geometry()
}

/// Panel transform for the touch-input calibration — see
/// [`HostIo::painted_region`]. Compiled only where its one consumer
/// (`input::calibrate`) is.
#[cfg(nh_input_mtouch)]
pub fn painted_region() -> Option<PaintedRegion> {
    BACKEND.painted_region()
}

/// Encode pen event into Einstein's packed sample format. Mirrors
/// `TScreenManager::PenDown` in `Emulator/Screen/TScreenManager.cpp`:
/// `((x & 0x7FF) << 21) | ((y & 0x7FF) << 7) | (pressure & 0x0F)`.
/// Compiled only for the pen-event producers (the semihost host-IO
/// backend, the mtouch input backend, and the serial debug pen
/// injector).
#[cfg(any(
    nh_host_io_semihost,
    nh_input_mtouch,
    feature = "serial-pen-inject"
))]
pub fn pack_pen_sample(x: u16, y: u16, pressure: u16) -> u32 {
    ((x as u32 & 0x7FF) << 21) | ((y as u32 & 0x7FF) << 7) | (pressure as u32 & 0x0F)
}

/// Einstein's `kPenDownSample` / `kPenUpSample` markers from
/// `TScreenManager.cpp:932-940` — inserted before a x/y packed sample
/// at the pen-down edge / on pen-up.
#[cfg(any(
    nh_host_io_semihost,
    nh_input_mtouch,
    feature = "serial-pen-inject"
))]
pub const PEN_DOWN_SAMPLE_MARKER: u32 = 0x0000_000D;
#[cfg(any(
    nh_host_io_semihost,
    nh_input_mtouch,
    feature = "serial-pen-inject"
))]
pub const PEN_UP_SAMPLE_MARKER: u32 = 0x0000_000E;
