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
//!     IRQ via `host_dma::on_completion`. It refills
//!     the cyclic DMA ring with the next period's worth of audio
//!     (real samples from the stereo ring, or silence between
//!     clips) and raises the kernel's output IRQ when the stereo
//!     ring is running low. Same shape as Linux's
//!     `vchan_cyclic_callback` in `drivers/dma/bcm2835-dma.c`.

#[cfg(nh_audio_null)]
mod null;
#[cfg(nh_audio_pi_hdmi)]
pub mod pi_hdmi;

/// Backend interface — the sound-driver contract above, as a trait.
/// Single-threaded EL2 callers; impls do not need to be re-entrant.
/// Backend asymmetries live in the defaulted methods: the null
/// backend paces completions from the trap-tail [`AudioBackend::tick`]
/// while `pi_hdmi` completes from its DMA period IRQ
/// ([`AudioBackend::on_mai_dma_done`]) — each overrides exactly the
/// pump it uses.
pub trait AudioBackend: Sync {
    /// One-time setup. Called from `kmain` (before the slow flash
    /// load; the HDMI link is already trained by `display::splash`).
    fn init(&self);

    /// Subfn 0x1F — store the kernel's interrupt masks: input bit in
    /// `r1`, output bit in `r2`. The output mask is what the
    /// completion path raises through `vic::raise`.
    fn set_interrupt_mask(&self, input_mask: u32, output_mask: u32);

    /// Subfn 0x05 — stash the two ping-pong output buffer addresses.
    /// Subfn 0x07 later picks one of them by index (`which=0` → buf1,
    /// `which=1` → buf2).
    fn set_output_buffers(&self, buf1_addr: u32, buf2_addr: u32);

    /// Subfn 0x07 — read `byte_count` bytes of Newton-format audio
    /// (BE S16 mono @ 22.05 kHz) from the buffer indexed by `which`,
    /// resample + SPDIF-encode + enqueue. Schedule a buffer-complete
    /// IRQ for when the tail catches up.
    fn schedule_output(&self, which: u32, byte_count: u32);

    /// Subfn 0x0D — enable audio output.
    fn start_output(&self);

    /// Subfn 0x0F — disable audio output. The kernel calls this
    /// between clips.
    fn stop_output(&self);

    /// Subfn 0x13 — true while output is started and the ring isn't
    /// yet drained.
    fn output_is_running(&self) -> bool;

    /// Subfn 0x17 — kernel-set output volume (signed-Q12.20 fader;
    /// `kOutputVolume_Max = 0`). Stored for [`Self::output_volume_get`]
    /// to read back; no backend has a software fader.
    fn output_volume_set(&self, volume: u32);

    /// Subfn 0x18 — return the volume passed to
    /// [`Self::output_volume_set`], defaulting to `kOutputVolume_Max
    /// = 0` if the kernel queried before it set a value.
    fn output_volume_get(&self) -> u32;

    /// Per-timer-tick audio pump, called from `trap_irq`'s IRQ tail.
    /// The null backend fires armed buffer-completion IRQs here once
    /// a buffer's playback duration has elapsed; `pi_hdmi` keeps the
    /// default no-op — audio liveness on real hardware must not
    /// depend on trap rate.
    fn tick(&self) {}

    /// DMA period-completion hook for the HDMI MAI TX channel,
    /// dispatched by `host_dma::on_completion`. `pi_hdmi`'s natural
    /// tick (refills the cyclic ring, raises the watermark IRQ —
    /// same shape as Linux's `vchan_cyclic_callback` in
    /// `bcm2835-dma.c`); default no-op for backends without an MAI
    /// ring. Compiled exactly where its only caller — `host_dma` —
    /// is.
    #[cfg(nh_real_hw)]
    fn on_mai_dma_done(&self) {}
}

#[cfg(nh_audio_null)]
use self::null::BACKEND;
#[cfg(nh_audio_pi_hdmi)]
use self::pi_hdmi::BACKEND;

/// One-time setup. Called from `kmain` — see [`AudioBackend::init`].
pub fn init() {
    BACKEND.init();
}

/// Subfn 0x1F — see [`AudioBackend::set_interrupt_mask`].
pub fn set_interrupt_mask(input_mask: u32, output_mask: u32) {
    BACKEND.set_interrupt_mask(input_mask, output_mask);
}

/// Subfn 0x05 — see [`AudioBackend::set_output_buffers`].
pub fn set_output_buffers(buf1_addr: u32, buf2_addr: u32) {
    BACKEND.set_output_buffers(buf1_addr, buf2_addr);
}

/// Subfn 0x07 — see [`AudioBackend::schedule_output`].
pub fn schedule_output(which: u32, byte_count: u32) {
    BACKEND.schedule_output(which, byte_count);
}

/// Subfn 0x0D — see [`AudioBackend::start_output`].
pub fn start_output() {
    BACKEND.start_output();
}

/// Subfn 0x0F — see [`AudioBackend::stop_output`].
pub fn stop_output() {
    BACKEND.stop_output();
}

/// Subfn 0x13 — see [`AudioBackend::output_is_running`].
pub fn output_is_running() -> bool {
    BACKEND.output_is_running()
}

/// Subfn 0x17 — see [`AudioBackend::output_volume_set`].
pub fn output_volume_set(volume: u32) {
    BACKEND.output_volume_set(volume);
}

/// Subfn 0x18 — see [`AudioBackend::output_volume_get`].
pub fn output_volume_get() -> u32 {
    BACKEND.output_volume_get()
}

/// Per-timer-tick audio pump, called from `trap_irq`'s IRQ tail
/// (`irq_from_guest`) — see [`AudioBackend::tick`].
#[inline]
pub fn tick() {
    BACKEND.tick();
}

/// DMA period-completion hook, dispatched by `host_dma::on_completion`
/// — see [`AudioBackend::on_mai_dma_done`]. Compiled exactly where its
/// only caller — `host_dma` — is.
#[cfg(nh_real_hw)]
#[inline]
pub fn on_mai_dma_done() {
    BACKEND.on_mai_dma_done();
}
