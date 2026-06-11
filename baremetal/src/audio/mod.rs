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
//!     flip an `OUTPUT_RUNNING` producer gate, but do NOT touch
//!     `MAI_CTL.ENABLE` — see `pi_hdmi::bringup_mai`. The HDMI link
//!     is established once and never disturbed.
//!   - `on_mai_dma_done()` is the audio subsystem's only "tick"
//!     entry point, fired from the BCM2835 DMA period-completion
//!     IRQ via `peripherals::host_dma::on_completion`. It refills
//!     the cyclic DMA ring with the next period's worth of audio
//!     (real samples from the stereo ring, or silence between
//!     clips) and raises the kernel's output IRQ when the stereo
//!     ring is running low. Same shape as Linux's
//!     `vchan_cyclic_callback` in `drivers/dma/bcm2835-dma.c`.

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
/// in `r2`. The output mask is what `on_mai_dma_done` raises through
/// `vic::raise` when the stereo ring drops below the low-watermark.
/// Subfn 0x1F.
pub fn set_interrupt_mask(_input_mask: u32, _output_mask: u32) {
    #[cfg(nh_audio_null)]
    null::set_interrupt_mask(_input_mask, _output_mask);
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::set_interrupt_mask(_input_mask, _output_mask);
}

/// Stash the two ping-pong output buffer addresses passed by subfn
/// 0x05. Subfn 0x07 later picks one of them by index (`which=0` →
/// buf1, `which=1` → buf2).
pub fn set_output_buffers(_buf1_addr: u32, _buf2_addr: u32) {
    #[cfg(nh_audio_null)]
    null::set_output_buffers(_buf1_addr, _buf2_addr);
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::set_output_buffers(_buf1_addr, _buf2_addr);
}

/// Subfn 0x07 — read `byte_count` bytes of Newton-format audio (BE
/// S16 mono @ 22.05 kHz) from the buffer indexed by `which` (0 or 1),
/// resample + SPDIF-encode + enqueue. Schedule a buffer-complete IRQ
/// for when the tail catches up.
pub fn schedule_output(_which: u32, _byte_count: u32) {
    #[cfg(nh_audio_null)]
    null::schedule_output(_which, _byte_count);
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::schedule_output(_which, _byte_count);
}

/// Subfn 0x0D — enable MAI output (audio packets start emitting on
/// HDMI).
pub fn start_output() {
    #[cfg(nh_audio_null)]
    null::start_output();
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::start_output();
}

/// Subfn 0x0F — disable MAI output. The kernel calls this between
/// clips; without it the HDMI receiver hears whatever residual
/// samples are in the ring.
pub fn stop_output() {
    #[cfg(nh_audio_null)]
    null::stop_output();
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::stop_output();
}

/// Subfn 0x13 — true while [`start_output`] has been called and the
/// ring isn't yet drained.
pub fn output_is_running() -> bool {
    #[cfg(nh_audio_null)]
    {
        return null::output_is_running();
    }
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
    #[cfg(nh_audio_null)]
    null::output_volume_set(_volume);
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::output_volume_set(_volume);
}

/// Subfn 0x18 — return the volume passed to [`output_volume_set`],
/// defaulting to `kOutputVolume_Max = 0` if the kernel queried before
/// it set a value.
pub fn output_volume_get() -> u32 {
    #[cfg(nh_audio_null)]
    {
        return null::output_volume_get();
    }
    #[cfg(nh_audio_pi_hdmi)]
    {
        return pi_hdmi::output_volume_get();
    }
    #[allow(unreachable_code)]
    0
}

/// Per-timer-tick audio pump, called from `trap_irq`'s IRQ tail
/// (`irq_from_guest`). The null backend uses it to fire armed
/// buffer-completion IRQs once a buffer's playback duration has
/// elapsed. The `pi_hdmi` backend drives completion from its own DMA
/// period IRQ (`on_mai_dma_done`) instead, so this is a no-op there —
/// audio liveness on real hardware must not depend on trap rate.
#[inline]
pub fn tick() {
    #[cfg(nh_audio_null)]
    null::tick();
}

/// DMA period-completion hook for the HDMI MAI TX channel,
/// dispatched by `peripherals::host_dma::on_completion`. This is the
/// audio subsystem's natural tick — the only thing that drives ring
/// refills and watermark IRQs. There is intentionally no trap-tail
/// pump entry point: audio liveness must not depend on trap rate,
/// which other hypervisor work is trying to reduce. The shape
/// matches Linux's `vchan_cyclic_callback` in `bcm2835-dma.c`.
/// Compiled exactly where its only caller — `host_dma` — is.
#[cfg(all(feature = "no-semihost", feature = "platform-raspi3b"))]
#[inline]
pub fn on_mai_dma_done() {
    #[cfg(nh_audio_pi_hdmi)]
    pi_hdmi::on_mai_dma_done();
}
