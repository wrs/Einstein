//! Host-side audio output plumbing.
//!
//! Newton produces 22.05 kHz mono 16-bit big-endian PCM in 1872-frame
//! buffers. Einstein's `PMainSoundDriver` (TNativePrimitives.cpp:1062-
//! 1400) forwards those buffers to a `TSoundManager` which talks to
//! whatever the host platform's audio API is (PulseAudio, CoreAudio,
//! …). The Pi Zero 2 W has no jack, so our only output is HDMI audio
//! — feeding IEC 60958 (SPDIF) subframes into the VC4 HDMI block's
//! MAI ("Multi-channel Audio Interconnect") FIFO at 0x3F90_2000. The
//! BCM2835 PCM/I2S peripheral at 0x3F20_3000 only reaches GPIO 18-21
//! (external I²S DAC) and does NOT feed HDMI — see
//! `audio::pi_hdmi` for the driver, and the comment in
//! `docs/REAL_HW_BRINGUP.md` Phase 6 for why the original plan's
//! "BCM2835 PCM/I2S" wording was incorrect.
//!
//! Backend selection: opt-in via the `audio-*` Cargo features and
//! `build.rs::resolve_audio_backend`; the resolver emits
//! `cfg(nh_audio_<chosen>)`. With no feature enabled the fallback is
//! `null`, so QEMU/FVP builds compile cleanly without any host audio
//! plumbing.
//!
//! ## Sound-driver contract
//!
//! `peripherals::sound::handle` calls into the active backend:
//!
//!   - `init()` once from kmain (after the framebuffer is up — HDMI
//!     audio depends on the HDMI link being trained, which the
//!     `display::splash::init` step does for us).
//!   - `set_interrupt_mask(in, out)` from subfn 0x1F. We store the
//!     output mask so the buffer-completion notification raises the
//!     right bit in `vic::int_present`.
//!   - `set_output_buffers(b1, b2)` from subfn 0x05. The kernel
//!     passes two ping-pong buffer addresses; subfn 0x07 later picks
//!     one of them by index.
//!   - `schedule_output(which, byte_count)` from subfn 0x07. Read the
//!     guest-side PCM samples (BE-S16 mono @ 22.05 kHz), resample to
//!     48 kHz stereo, SPDIF-encode and push onto the MAI ring. Track
//!     samples-played; when the corresponding tail catches up,
//!     [`vic::raise`] the stored output interrupt mask so the kernel
//!     calls 0x07 with the next buffer.
//!   - `start_output()` / `stop_output()` / `output_is_running()`
//!     gate MAI_CTL.ENABLE so the HDMI receiver doesn't see stale
//!     audio packets between Newton sound clips.
//!   - `pump()` runs from the trap-IRQ and sync-trap tails to drain
//!     the ring into MAI_DATA. CPU-direct writes; the MAI FIFO is
//!     deep enough that a ~16 ms timer cadence keeps it alive at
//!     48 kHz stereo (~768 frames per cadence vs. 64-frame FIFO is
//!     not enough on its own, but the trap rate is much higher).

#[cfg(nh_audio_null)]
mod null;
#[cfg(nh_audio_pi_hdmi)]
pub mod pi_hdmi;

/// One-time setup. Called from `kmain` once `host_io::init` has
/// returned (so the framebuffer/HDMI link is up if the platform has
/// one to bring up).
pub fn init() {
    #[cfg(nh_audio_null)]
    null::init();
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::init();
}

/// Newton kernel-side interrupt masks: input bit in `r1`, output bit
/// in `r2`. The output mask is what `pump` raises through `vic::raise`
/// after a Newton buffer's worth of samples has been consumed. Subfn
/// 0x1F.
pub fn set_interrupt_mask(_input_mask: u32, _output_mask: u32) {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::set_interrupt_mask(_input_mask, _output_mask);
}

/// Stash the two ping-pong output buffer addresses passed by subfn
/// 0x05. Subfn 0x07 later picks one of them by index (`which=0` →
/// buf1, `which=1` → buf2).
pub fn set_output_buffers(_buf1_addr: u32, _buf2_addr: u32) {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::set_output_buffers(_buf1_addr, _buf2_addr);
}

/// Subfn 0x07 — read `byte_count` bytes of Newton-format audio (BE
/// S16 mono @ 22.05 kHz) from the buffer indexed by `which` (0 or 1),
/// resample + SPDIF-encode + enqueue. Schedule a buffer-complete IRQ
/// for when the tail catches up.
pub fn schedule_output(_which: u32, _byte_count: u32) {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::schedule_output(_which, _byte_count);
}

/// Subfn 0x0D — enable MAI output (audio packets start emitting on
/// HDMI).
pub fn start_output() {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::start_output();
}

/// Subfn 0x0F — disable MAI output. The kernel calls this between
/// clips; without it the HDMI receiver hears whatever residual
/// samples are in the ring.
pub fn stop_output() {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::stop_output();
}

/// Subfn 0x13 — true while [`start_output`] has been called and the
/// ring isn't yet drained.
pub fn output_is_running() -> bool {
    #[cfg(nh_audio_pi_hdmi)]
    {
        return pi_hdmi::output_is_running();
    }
    #[allow(unreachable_code)]
    false
}

/// Subfn 0x17 — kernel-set output volume. Newton's volume is a
/// signed-Q12.20 fader (`kOutputVolume_Min = 0xFFDDBD71`,
/// `kOutputVolume_Max = 0x00000000`, `kOutputVolume_Zero =
/// 0x80000000`); we just store it for [`output_volume_get`] to read
/// back. The HDMI MAI hardware doesn't expose a software fader, and
/// the receiver side handles its own master volume — so muting via
/// software would just need a tighter loop than this initial cut.
pub fn output_volume_set(_volume: u32) {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::output_volume_set(_volume);
}

/// Subfn 0x18 — return the volume passed to [`output_volume_set`],
/// defaulting to `kOutputVolume_Max = 0` if the kernel queried before
/// it set a value.
pub fn output_volume_get() -> u32 {
    #[cfg(nh_audio_pi_hdmi)]
    {
        return pi_hdmi::output_volume_get();
    }
    #[allow(unreachable_code)]
    0
}

/// Drain the ring buffer into MAI_DATA. Called from the trap-IRQ
/// and sync-trap tails in `trap.rs`. Must be non-blocking — `pump`
/// runs with the guest stalled.
pub fn pump() {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::pump();
}
