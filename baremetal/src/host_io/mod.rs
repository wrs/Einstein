//! Host-side display + input plumbing.
//!
//! Two roles:
//!
//! 1. **Outbound display.** `screen::blit` calls [`push_blit`] each time
//!    it paints pixels. The active backend forwards a `BlitEvent` plus
//!    its 2 bpp packed payload to whatever sink it owns (host viewer
//!    over semihosting IPC for QEMU/FVP, a real LCD on Pico 2 W).
//!
//! 2. **Inbound pen input.** The active backend's [`pump_input`] runs
//!    from the trap-return tail (`trap.rs`); it pulls pen events off
//!    its source and feeds them into [`queue::enqueue_pen_sample`],
//!    which raises `INT_TABLET`. `tablet::handle` subfn 0x16
//!    (`NativeGetSample`) drains via [`pop_pen_sample`].
//!
//! Backend selection is by Cargo feature: exactly one of
//! `host-io-null`, `host-io-semihost`, `host-io-pico` must be enabled
//! (`build.rs` enforces this). The default is `host-io-null`, which
//! turns everything in here into a no-op so guest-tests and CI runs
//! behave like the old fb_dump-less world.

pub mod queue;

#[cfg(feature = "host-io-null")]
mod null;
#[cfg(feature = "host-io-semihost")]
mod semihost;

pub const BLIT_KIND_BLIT: u8 = 1;
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

pub const PEN_DOWN: u8 = 1;
pub const PEN_MOVE: u8 = 2;
pub const PEN_UP: u8 = 3;

const _: () = {
    assert!(core::mem::size_of::<PenEvent>() == 8);
};

/// One-time setup: open transport, send a hello, …. Called from
/// `kmain` once `vic::init` has returned.
pub fn init() {
    #[cfg(feature = "host-io-null")]
    null::init();
    #[cfg(feature = "host-io-semihost")]
    semihost::init();
}

/// Called after `snapshot::load_latest` restores guest state but
/// before `eret_to_restored`. Flushes the input queue and pushes a
/// synthesised full-screen blit so the host viewer's backing store
/// re-syncs with the restored GUEST_FB.
pub fn on_resume() {
    queue::reset();
    let payload = current_fb_bytes();
    let ev = BlitEvent {
        kind: BLIT_KIND_FULL_REPAINT,
        mode: 0,
        bpp: crate::peripherals::screen::SCREEN_BPP as u8,
        _pad: 0,
        src_left: 0,
        src_top: 0,
        src_right: crate::peripherals::screen::SCREEN_WIDTH as u16,
        src_bottom: crate::peripherals::screen::SCREEN_HEIGHT as u16,
        dst_left: 0,
        dst_top: 0,
        dst_right: crate::peripherals::screen::SCREEN_WIDTH as u16,
        dst_bottom: crate::peripherals::screen::SCREEN_HEIGHT as u16,
        row_bytes: crate::peripherals::screen::FB_ROW_BYTES as u16,
        payload_len: payload.len() as u16,
    };
    push_blit(&ev, payload);
    #[cfg(feature = "host-io-null")]
    null::on_resume();
    #[cfg(feature = "host-io-semihost")]
    semihost::on_resume();
}

/// Forward one blit to the host. Must be non-blocking — `screen::blit`
/// calls this from a sync trap with the guest stalled. Backends that
/// can't keep up drop events instead of blocking.
pub fn push_blit(ev: &BlitEvent, payload: &[u8]) {
    #[cfg(feature = "host-io-null")]
    null::push_blit(ev, payload);
    #[cfg(feature = "host-io-semihost")]
    semihost::push_blit(ev, payload);
}

/// Pull a single pen sample off the input queue. Returns
/// `Some((packed_sample, ticks))` matching Einstein's
/// `TScreenManager::GetSample` semantics, or `None` if the queue is
/// empty.
pub fn pop_pen_sample() -> Option<(u32, u32)> {
    queue::pop()
}

/// Pump the backend's input transport: drain newly-arrived host pen
/// events, enqueue them as Newton-format samples, and raise
/// `INT_TABLET`. Called from the trap-return tail (`trap.rs`).
pub fn pump_input() {
    #[cfg(feature = "host-io-null")]
    null::pump_input();
    #[cfg(feature = "host-io-semihost")]
    semihost::pump_input();
}

/// Return a slice of the 320×480 2 bpp framebuffer for the full-repaint
/// payload. GUEST_FB is hypervisor-managed linear-LE, so no byte-swap
/// needed.
fn current_fb_bytes() -> &'static [u8] {
    const FB_LEN: usize =
        (crate::peripherals::screen::SCREEN_WIDTH
            * crate::peripherals::screen::SCREEN_HEIGHT
            / 4) as usize;
    // SAFETY: `fb_host_pa` is the base of the static GUEST_FB backing.
    // FB_LEN is in bounds (FRAMEBUFFER_SIZE is 2 MiB; FB_LEN ≈ 38 KiB).
    unsafe {
        core::slice::from_raw_parts(crate::guest_mem::fb_host_pa() as *const u8, FB_LEN)
    }
}

/// Encode pen event into Einstein's packed sample format. Mirrors
/// `TScreenManager::PenDown` in `Emulator/Screen/TScreenManager.cpp`:
/// `((x & 0x7FF) << 21) | ((y & 0x7FF) << 7) | (pressure & 0x0F)`.
pub fn pack_pen_sample(x: u16, y: u16, pressure: u16) -> u32 {
    ((x as u32 & 0x7FF) << 21) | ((y as u32 & 0x7FF) << 7) | (pressure as u32 & 0x0F)
}

/// Einstein's `kPenDownSample` / `kPenUpSample` markers from
/// `TScreenManager.cpp:932-940` — inserted before a x/y packed sample
/// at the pen-down edge / on pen-up.
pub const PEN_DOWN_SAMPLE_MARKER: u32 = 0x0000_000D;
pub const PEN_UP_SAMPLE_MARKER: u32 = 0x0000_000E;
