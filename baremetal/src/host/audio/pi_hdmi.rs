//! VC4 HDMI audio backend for the Pi Zero 2 W.
//!
//! Feeds Newton's 22.05 kHz mono 16-bit PCM into the HDMI MAI ("Multi-
//! channel Audio Interconnect") block at 0x3F90_2000, which the VC4
//! HDMI encoder embeds into the video blanking interval of the picture
//! the `display::splash` / `host_io::pi_fb` framebuffer is already
//! driving. The BCM2835 PCM/I2S peripheral at 0x3F20_3000 only reaches
//! GPIO 18-21 (external I²S DAC) and is irrelevant to HDMI.
//!
//! ## Data path
//!
//! ```text
//!   Newton BE-S16 mono @ 22.05 kHz
//!         ↓ schedule_output: read guest buffer, 2× upsample (S&H)
//!         ↓                 + duplicate to stereo
//!   LE-S16 stereo @ 44.1 kHz, in a host RING buffer
//!         ↓ refill_mai_dma_ring: SPDIF-encode (24-bit shift + parity)
//!   IEC 60958 subframes, in the DMA TX ring
//!         ↓ cyclic DMA (channel 4, DREQ 17) → MAI_DATA register
//!         ↓ VC4 hardware
//!   HDMI audio packets in video blank → receiver speakers
//! ```
//!
//! ## Why 44.1 kHz
//!
//! Standard HDMI audio rates are 32 / 44.1 / 48 / 88.2 / 96 / 176.4 /
//! 192 kHz. 44.1 kHz is an exact 2× of Newton's 22.05 kHz, so the
//! resampler is a trivial sample-and-hold; no interpolator needed for
//! the initial cut. Quality is dominated by Newton's source material
//! (8-bit mu-law alerts upsampled to S16), not by our upsampler.
//!
//! ## DMA-fed MAI
//!
//! MAI_DATA is fed by a cyclic BCM2835 DMA chain (channel 4, paced
//! by DREQ 17), the same shape as Circle's
//! `hdmisoundbasedevice.cpp` and Linux's dmaengine cyclic transfer.
//! `refill_mai_dma_ring()` (called from the DMA period-completion IRQ
//! and from `schedule_output`) only
//! SPDIF-encodes ring frames into the DMA TX ring; the hardware
//! drains it without CPU involvement, so a quiet stretch of the
//! guest can't underrun the FIFO as long as the ring holds encoded
//! frames. See "DMA TX ring for HDMI MAI" below.
//!
//! ## Clock derivation
//!
//! Two clocks matter and both are read at runtime — no hard-coded
//! rates, no fabricated fallbacks:
//!
//! - **Pixel clock** (for CTS): queried via `TAG_GET_CLOCK_RATE_MEASURED`
//!   then `TAG_GET_CLOCK_RATE` on `CLOCK_ID_PIXEL`. If both fail
//!   we refuse to come up; a wrong CTS produces exactly the
//!   "crunchy" symptom we're trying to avoid.
//! - **HSM clock** (the MAI sample-rate divider's input): read
//!   directly from the BCM2835 Clock Manager (CM_HSMCTL + CM_HSMDIV
//!   plus the A2W_PLLx_* control registers for the selected source
//!   PLL). The CM register layout is `drivers/clk/bcm/clk-bcm2835.c`
//!   in the kernel — the BCM2835 ARM Peripherals manual does not
//!   document the A2W_* set.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::{kprintln, peripherals::vic};

/// Full system data-synchronization barrier. Ensures all prior memory
/// accesses (including Device-nGnRE MMIO writes) have completed before
/// the next instruction. Use this — not cache maintenance — when we
/// need a write to MMIO to be visible to the hardware before we read
/// status from a related register.
#[inline(always)]
fn dsb_sy() {
    // SAFETY: barrier instruction with no operands.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags, nomem)) };
}

// ---------- Constants -------------------------------------------------------

/// VC4 has two HDMI MMIO regions on BCM2710 (Pi Zero 2 W) / BCM2837
/// (Pi 3). The kernel driver declares both in its device-tree
/// binding (`reg = <0x7e902000 0x600>, <0x7e808000 0x100>`) and
/// dispatches each register access to the matching base via the
/// `vc4_hdmi_register_map` table in `drivers/gpu/drm/vc4/
/// vc4_hdmi_regs.h`:
///
/// - **HDMI base** (`0x7E90_2000` / ARM-PA `0x3F90_2000`): control,
///   scheduler, info-frame RAM, MAI_CHANNEL_MAP/CONFIG,
///   AUDIO_PACKET_CONFIG, CRP_CFG, CTS_0/1. Tagged `VC4_HDMI_REG`.
/// - **HD base** (`0x7E80_8000` / ARM-PA `0x3F80_8000`): MAI control
///   and data path — MAI_CTL/THR/FMT/DATA/SMP, VID_CTL, CSC matrix.
///   Tagged `VC4_HD_REG`.
///
/// Both regions land inside the `[0x3F00_0000, 0x4000_0000)` window
/// `mmu::init` maps Device-nGnRE.
const HDMI_BASE: usize = 0x3F90_2000;
const HD_BASE: usize = 0x3F80_8000;

// HD-base registers (vc4_hdmi_regs.h `VC4_HD_REG(...)`).
const HDMI_MAI_CTL: usize = HD_BASE + 0x0014;
const HDMI_MAI_THR: usize = HD_BASE + 0x0018;
const HDMI_MAI_FMT: usize = HD_BASE + 0x001C;
const HDMI_MAI_DATA: usize = HD_BASE + 0x0020;
const HDMI_MAI_SMP: usize = HD_BASE + 0x002C;

// HDMI-base registers (vc4_hdmi_regs.h `VC4_HDMI_REG(...)`).
const HDMI_MAI_CHANNEL_MAP: usize = HDMI_BASE + 0x0090;
const HDMI_MAI_CONFIG: usize = HDMI_BASE + 0x0094;
const HDMI_AUDIO_PACKET_CONFIG: usize = HDMI_BASE + 0x009C;
const HDMI_RAM_PACKET_CONFIG: usize = HDMI_BASE + 0x00A0;
const HDMI_RAM_PACKET_STATUS: usize = HDMI_BASE + 0x00A4;
const HDMI_CRP_CFG: usize = HDMI_BASE + 0x00A8;
const HDMI_CTS_0: usize = HDMI_BASE + 0x00AC;
const HDMI_CTS_1: usize = HDMI_BASE + 0x00B0;
const HDMI_SCHEDULER_CONTROL: usize = HDMI_BASE + 0x00C0;
const HDMI_TX_PHY_CTL0: usize = HDMI_BASE + 0x02C4;
/// HDMI RAM packet write window — 36 bytes per packet slot, slot N at
/// `+0x400 + 0x24 * N`. Per `vc4_hdmi_write_infoframe` the slot for an
/// info-frame is `frame_type - 0x80`; the Audio InfoFrame's CEA-861
/// type byte is `0x84`, so it lands in slot 4.
const HDMI_RAM_PACKET_START: usize = HDMI_BASE + 0x0400;

// ---- BCM2835 Clock Manager (CM) — for reading the HSM clock ---------------
//
// The HSM ("HDMI State Machine") clock is what vc4_hdmi.c binds as
// `audio_clock` on BCM2837 / BCM2710 (Pi 3 / Zero 2 W):
//
//   drivers/gpu/drm/vc4/vc4_hdmi.c:vc4_hdmi_audio_set_mai_clock
//     clk_set_rate(vc4_hdmi->audio_clock, samplerate * 384);
//     hsm_rate = clk_get_rate(vc4_hdmi->audio_clock);
//     rational_best_approximation(hsm_rate, samplerate, …);
//
// Linux's `clk-bcm2835.c` exposes the CM register layout. The pieces
// we need to compute HSM's actual rate:
//
//   CM base                  0x3F10_1000   (peripheral base + 0x101000)
//   CM_HSMCTL                +0x88   src mux, enable
//   CM_HSMDIV                +0x8C   12.12 integer.fractional divider
//   A2W_PLLD_CTRL            +0x1140 PLLD integer NDIV (bits 0..9), PDIV (bits 12..14)
//   A2W_PLLD_FRAC            +0x1240 PLLD fractional NDIV (bits 0..19)
//   A2W_PLLD_PER             +0x1540 PLLD per-output divider (bits 0..7)
//   A2W_PLLD_ANA0            +0x1050 PLLD analog block; ANA1 = ANA0+4
//                                    carries the feedback pre-divider
//                                    bit (BIT(14) on 2835-family)
//
// All offsets confirmed against `drivers/clk/bcm/clk-bcm2835.c`. PLLD
// is the typical HSM source on Pi 3/Zero 2 W. If CM_HSMCTL.SRC ever
// names a different PLL we add the equivalent A2W_PLLx_* offsets.
const CM_BASE: usize = 0x3F10_1000;
const CM_HSMCTL: usize = CM_BASE + 0x88;
const CM_HSMDIV: usize = CM_BASE + 0x8C;
const CM_CTL_SRC_MASK: u32 = 0xF;
const A2W_PLLA_CTRL: usize = CM_BASE + 0x1100;
const A2W_PLLA_FRAC: usize = CM_BASE + 0x1200;
const A2W_PLLA_PER: usize = CM_BASE + 0x1500;
const A2W_PLLA_ANA0: usize = CM_BASE + 0x1010;
const A2W_PLLC_CTRL: usize = CM_BASE + 0x1120;
const A2W_PLLC_FRAC: usize = CM_BASE + 0x1220;
const A2W_PLLC_PER: usize = CM_BASE + 0x1520;
const A2W_PLLC_ANA0: usize = CM_BASE + 0x1030;
const A2W_PLLD_CTRL: usize = CM_BASE + 0x1140;
const A2W_PLLD_FRAC: usize = CM_BASE + 0x1240;
const A2W_PLLD_PER: usize = CM_BASE + 0x1540;
const A2W_PLLD_ANA0: usize = CM_BASE + 0x1050;
/// `A2W_PLL_CTRL_PDIV_MASK`/`SHIFT` — post-VCO divider inside the PLL.
const A2W_PLL_CTRL_PDIV_SHIFT: u32 = 12;
const A2W_PLL_CTRL_PDIV_MASK: u32 = 0x7 << A2W_PLL_CTRL_PDIV_SHIFT;
/// `bcm2835_ana_default.fb_prediv_mask` — ANA1 bit that halves the
/// feedback path, doubling the effective NDIV/FDIV. (On BCM2711 these
/// bits are repurposed as VCO-range bits, but Zero 2 W is 2835-family.)
const A2W_PLL_ANA1_FB_PREDIV: u32 = 1 << 14;

/// BCM283x crystal frequency. Fixed at 19.2 MHz on every Pi from the
/// original through the Pi 3B+ / Zero 2 W (Pi 4 moved to 54 MHz).
const BCM283X_OSC_HZ: u32 = 19_200_000;

// ---- MAI_CTL bits (from `drivers/gpu/drm/vc4/vc4_regs.h`) ----------------
//
// VC4_HD_MAI_CTL_RESET    BIT(0)
// VC4_HD_MAI_CTL_ERRORF   BIT(1)
// VC4_HD_MAI_CTL_ERRORE   BIT(2)
// VC4_HD_MAI_CTL_ENABLE   BIT(3)
// VC4_HD_MAI_CTL_CHNUM    VC4_MASK(7, 4)  shift 4
// VC4_HD_MAI_CTL_PAREN    BIT(8)
// VC4_HD_MAI_CTL_FLUSH    BIT(9)
// VC4_HD_MAI_CTL_EMPTY    BIT(10)         RO
// VC4_HD_MAI_CTL_FULL     BIT(11)         RO
// VC4_HD_MAI_CTL_WHOLSMP  BIT(12)
// VC4_HD_MAI_CTL_CHALIGN  BIT(13)
// VC4_HD_MAI_CTL_BUSY     BIT(14)         RO
// VC4_HD_MAI_CTL_DLATE    BIT(15)
const MAI_CTL_RESET: u32 = 1 << 0;
const MAI_CTL_ERRORF: u32 = 1 << 1;
const MAI_CTL_ERRORE: u32 = 1 << 2;
const MAI_CTL_ENABLE: u32 = 1 << 3;
const MAI_CTL_CHNUM_SHIFT: u32 = 4;
const MAI_CTL_FLUSH: u32 = 1 << 9;
#[allow(dead_code)] // RO FIFO status bit; kept for register documentation.
const MAI_CTL_EMPTY: u32 = 1 << 10; // RO; FIFO drained-to-empty indicator.
#[allow(dead_code)] // RO FIFO status bit; kept for register documentation.
const MAI_CTL_FULL: u32 = 1 << 11; // RO; FIFO-full indicator (DMA-paced now).
const MAI_CTL_WHOLSMP: u32 = 1 << 12;
const MAI_CTL_CHALIGN: u32 = 1 << 13;
#[allow(dead_code)] // RO FIFO status bit; kept for register documentation.
const MAI_CTL_BUSY: u32 = 1 << 14; // RO.
#[allow(dead_code)] // setting DLATE may be needed on some receivers; keep the constant available.
const MAI_CTL_DLATE: u32 = 1 << 15;

// ---- AUDIO_PACKET_CONFIG bits (from vc4_regs.h) --------------------------
//
// VC4_HDMI_AUDIO_PACKET_ZERO_DATA_ON_SAMPLE_FLAT        BIT(29)
// VC4_HDMI_AUDIO_PACKET_ZERO_DATA_ON_INACTIVE_CHANNELS  BIT(24)
// VC4_HDMI_AUDIO_PACKET_FORCE_SAMPLE_PRESENT            BIT(19)
// VC4_HDMI_AUDIO_PACKET_FORCE_B_FRAME                   BIT(18)
// VC4_HDMI_AUDIO_PACKET_B_FRAME_IDENTIFIER  VC4_MASK(13, 10)  shift 10
// VC4_HDMI_AUDIO_PACKET_AUDIO_LAYOUT                    BIT(9)
// VC4_HDMI_AUDIO_PACKET_FORCE_AUDIO_LAYOUT              BIT(8)
// VC4_HDMI_AUDIO_PACKET_CEA_MASK            VC4_MASK(7, 0)    shift 0
const AUDIO_PACKET_CONFIG_ZERO_DATA_ON_SAMPLE_FLAT: u32 = 1 << 29;
const AUDIO_PACKET_CONFIG_ZERO_DATA_ON_INACTIVE_CHANNELS: u32 = 1 << 24;
const AUDIO_PACKET_CONFIG_B_FRAME_IDENTIFIER_SHIFT: u32 = 10;
const AUDIO_PACKET_CONFIG_CEA_MASK_STEREO: u32 = 0b11;

// ---- CRP_CFG bits (from vc4_regs.h) --------------------------------------
//
// VC4_HDMI_CRP_USE_MAI_BUS_SYNC_FOR_CTS  BIT(26)
// VC4_HDMI_CRP_CFG_DISABLE               BIT(25)
// VC4_HDMI_CRP_CFG_EXTERNAL_CTS_EN       BIT(24)
// VC4_HDMI_CRP_CFG_N                     VC4_MASK(19, 0)  shift 0
const CRP_CFG_EXTERNAL_CTS_EN: u32 = 1 << 24;
const CRP_CFG_N_SHIFT: u32 = 0;
const CRP_CFG_N_MASK: u32 = (1 << 20) - 1;

// ---- MAI_CONFIG bits (from vc4_regs.h) -----------------------------------
//
// VC4_HDMI_MAI_CONFIG_FORMAT_REVERSE  BIT(27)
// VC4_HDMI_MAI_CONFIG_BIT_REVERSE     BIT(26)
// VC4_HDMI_MAI_CHANNEL_MASK           VC4_MASK(15, 0)  shift 0
const MAI_CONFIG_BIT_REVERSE: u32 = 1 << 26;
const MAI_CONFIG_FORMAT_REVERSE: u32 = 1 << 27;
const MAI_CONFIG_CHANNEL_MASK_STEREO: u32 = 0b11;

// ---- MAI_FMT bits (from vc4_regs.h) --------------------------------------
//
// VC4_HDMI_MAI_FORMAT_AUDIO_FORMAT  VC4_MASK(23, 16)  shift 16
// VC4_HDMI_MAI_FORMAT_SAMPLE_RATE   VC4_MASK(15, 8)   shift 8
const MAI_FMT_AUDIO_FORMAT_SHIFT: u32 = 16;
const MAI_FMT_SAMPLE_RATE_SHIFT: u32 = 8;
/// `enum { VC4_HDMI_MAI_FORMAT_PCM = 2, VC4_HDMI_MAI_FORMAT_HBR = 200 }`
/// in `drivers/gpu/drm/vc4/vc4_regs.h`.
const MAI_FORMAT_PCM: u32 = 2;
/// `enum VC4_HDMI_MAI_SAMPLE_RATE_44100 = 8` in `vc4_regs.h`.
const MAI_SAMPLE_RATE_CODE_44_1_KHZ: u32 = 8;

// ---- RAM_PACKET_CONFIG bits (from vc4_regs.h) ----------------------------
//
// VC4_HDMI_RAM_PACKET_ENABLE  BIT(16)
const RAM_PACKET_ENABLE: u32 = 1 << 16;
// Audio Info Frame is CEA-861 packet type 0x84; the RAM-packet slot
// index in `HDMI_RAM_PACKET_CONFIG` is `type - 0x80`, per
// `vc4_hdmi_write_infoframe`.
const RAM_PACKET_AUDIO_SLOT: u32 = 4;

// ---- MAI_SMP field layout (from vc4_regs.h) ------------------------------
//
// VC4_HD_MAI_SMP_N  VC4_MASK(31, 8)  shift 8
// VC4_HD_MAI_SMP_M  VC4_MASK( 7, 0)  shift 0
const MAI_SMP_N_MASK: u32 = 0xFFFF_FF00;
const MAI_SMP_N_SHIFT: u32 = 8;
const MAI_SMP_M_MASK: u32 = 0x0000_00FF;
const MAI_SMP_M_SHIFT: u32 = 0;

// ---- SCHEDULER_CONTROL bits (from vc4_regs.h) ----------------------------
//
// VC4_HDMI_SCHEDULER_CONTROL_MANUAL_FORMAT          BIT(15)
// VC4_HDMI_SCHEDULER_CONTROL_IGNORE_VSYNC_PREDICTS  BIT(5)
// VC4_HDMI_SCHEDULER_CONTROL_VERT_ALWAYS_KEEPOUT    BIT(3)
// VC4_HDMI_SCHEDULER_CONTROL_HDMI_ACTIVE            BIT(1)  RO
// VC4_HDMI_SCHEDULER_CONTROL_MODE_HDMI              BIT(0)
//
// We only read SCHEDULER_CONTROL to verify the firmware brought up
// HDMI mode (vs DVI). Writes belong in the firmware's modeset path.
const SCHEDULER_CONTROL_MODE_HDMI: u32 = 1 << 0;

// ---- HDMI TX PHY bits (Circle `TxPhyControl0`, Linux `phy_rng_enable`) -----
const TX_PHY_CTL0_RNG_POWER_DOWN: u32 = 1 << 25;

// ---- IEC 60958 / SPDIF ---------------------------------------------------
//
// Bits 0..3 of each subframe carry the software preamble identifier
// — the hardware uses these to insert the on-wire biphase preamble
// and to find 192-frame block boundaries via
// `VC4_HDMI_AUDIO_PACKET_B_FRAME_IDENTIFIER` in HDMI_AUDIO_PACKET_CONFIG.
//
// The shipped configuration uses ALSA's IEC958 preamble convention
// (0x8 for the block-start B-frame), which the hardware's
// AUDIO_PACKET_CONFIG B_FRAME_IDENTIFIER is paired against below.
//
// Alternatives tried and why they lost: two known-good preamble
// conventions exist — Linux/alsa-lib (0x8/0x8) and Circle (0xF/0xF).
// During bring-up a *bare* 0x8 block-start-only preamble (Circle's
// shape but with the ALSA nibble) caused intermittent boot hangs
// (~5/6 boots) in the kernel's post-StartOutput polling loop, where
// 0xF booted reliably. The resolution was not "use 0xF" but to
// supply ALSA's *full* software-preamble set (Z=0x8 on left
// block-start, X=0x2 on other left subframes, Y=0x4 on every right
// subframe) together with the complete channel-status bytes — that
// combination boots reliably and matches what the ALSA IEC958 plugin
// emits, so the hardware's block detection has the framing it
// expects. That is the configuration the code below bakes in.
const IEC958_B_FRAME_PREAMBLE_ALSA: u32 = 0x8;

/// Newton source audio parameters (Einstein PulseAudio backend,
/// TPulseAudioSoundManager.cpp).
#[allow(dead_code)] // referenced in comments; kept for future resampler work.
const NEWTON_RATE_HZ: u32 = 22050;

// ---- HDMI audio configuration ---------------------------------------------
//
// The shipped configuration follows the working Linux/Circle VC4 path:
// one known-good combination of sample rate, IEC channel-status mode and
// MAI_CTL / AUDIO_PACKET_CONFIG bits. The rationale for each choice — and
// why the alternatives lose — lives with the constant or register write
// it governs.

/// HDMI output audio rate. 44.1 kHz — an exact 2× of Newton's 22.05 kHz
/// source, so the resampler is a trivial sample-and-hold.
const HDMI_RATE_HZ: u32 = 44_100;

// Non-Linux infrastructure still called out explicitly:
// - HSM is inherited from the firmware-owned HDMI modeset. Directly poking
//   CM_HSMCTL/CM_HSMDIV while the firmware encoder is live is not equivalent
//   to Linux's KMS + common-clock-framework path and produced quiet hiss.
// - Normal Newton playback writes the Audio InfoFrame once at bringup, not on
//   every ALSA prepare/start (our stream format is fixed).

/// IEC 60958 block size — 192 frames. The B-frame preamble marks the
/// start of each block; subsequent frames use M/W preambles which the
/// hardware inserts for us (we just set frame-counter % 192 == 0 → set
/// the B-preamble bits in our subframe).
const IEC958_BLOCK_FRAMES: u32 = 192;

const fn mai_sample_rate_code() -> u32 {
    MAI_SAMPLE_RATE_CODE_44_1_KHZ
}

/// N for the HDMI ACR (Audio Clock Regeneration) packet.
///
/// Matches Linux's `vc4_hdmi_set_n_cts`:
/// ```c
/// n = 128 * samplerate / 1000;
/// ```
///
/// For 44.1 kHz this gives 5644 (slightly less than the HDMI 1.4
/// spec table's recommended 6272, but Linux uses this formula
/// rather than the table).
const fn hdmi_acr_n() -> u32 {
    128 * HDMI_RATE_HZ / 1000
}

/// Ring capacity in stereo frames. Newton ping-pongs two 1872-sample
/// buffers — at the 2× upsample to 44.1 kHz that's 7488 frames total
/// queued before `refill_mai_dma_ring` has drained the first half.
/// 8192 frames (~186 ms) gives enough headroom for the full ping-pong
/// without overrun. Power-of-two for cheap modulo with `& MASK`.
/// (16384 produces intermittent boot hangs, suspected to be
/// BSS-layout-related; 8192 is the minimum that fits the ping-pong
/// cleanly.)
const RING_FRAMES: usize = 8192;
const RING_MASK: usize = RING_FRAMES - 1;

// ---------- State ---------------------------------------------------------

/// One stereo frame in the ring: lower 16 bits = left, upper = right.
/// Stored as LE-S16 already byte-swapped from Newton's BE-S16; the
/// SPDIF encoder in `refill_mai_dma_ring` shifts each channel up into
/// the subframe payload position.
#[repr(transparent)]
#[derive(Copy, Clone, Default)]
struct StereoFrame(u32);

#[repr(align(64))]
struct RingState {
    /// Producer index (frames written, monotonic).
    head: AtomicU32,
    /// Consumer index (frames played, monotonic).
    tail: AtomicU32,
    /// `schedule_output` writes encoded stereo frames here;
    /// `refill_mai_dma_ring` pulls them out and SPDIF-encodes them into
    /// the DMA TX ring. `UnsafeCell` because the
    /// producer writes individual slots through a `*mut` derived from a
    /// shared `&RingState` — plain (non-`UnsafeCell`) interior mutation
    /// through a shared reference is UB under Rust's aliasing model.
    /// The head/tail atomics serialise which slots each side touches;
    /// the same pattern as `MAI_TX_RING` below.
    frames: core::cell::UnsafeCell<[StereoFrame; RING_FRAMES]>,
}

// SAFETY: single-CPU EL2; producer (schedule_output) and consumer
// (refill_mai_dma_ring) are serialised by EL2 trap context and by the
// head/tail
// ordering, the same as the `MaiTxRing` Sync rationale below.
unsafe impl Sync for RingState {}

fn ring_state() -> &'static RingState {
    static STATE: RingState = RingState {
        head: AtomicU32::new(0),
        tail: AtomicU32::new(0),
        frames: core::cell::UnsafeCell::new([StereoFrame(0); RING_FRAMES]),
    };
    &STATE
}

static INIT_DONE: AtomicBool = AtomicBool::new(false);
static OUTPUT_RUNNING: AtomicBool = AtomicBool::new(false);
static OUTPUT_INT_MASK: AtomicU32 = AtomicU32::new(0);
static INPUT_INT_MASK: AtomicU32 = AtomicU32::new(0);
/// CNTPCT_EL0 timestamp of the last `vic::raise(output_mask)` in
/// `maybe_raise_watermark_irq`. Zero before the first IRQ. Rate-limit
/// floor for the
/// level-triggered nudge below.
///
/// The nudge is LEVEL-triggered, matching Einstein's CoreAudio
/// oracle: `TCoreAudioSoundManager::RenderCallback` raises the
/// output interrupt on *every* render quantum while the buffer
/// holds less than one Newton buffer — including when it is fully
/// empty — and only `StopOutput` ends the stream of interrupts.
/// The kernel terminates that stream itself: when it has nothing
/// left to play it answers a nudge with a zero-size
/// `ScheduleOutput` (→ `stop_output`, see the PulseAudio oracle's
/// `else if (mOutputIsRunning) StopOutput();`) or calls subfn 0x0F
/// directly.
///
/// Level-triggering depends on that zero-size → `stop_output`
/// translation: ignore zero-size schedules and `OUTPUT_RUNNING` never
/// drops, the nudges never stop, and `sndm` sits in an IRQ storm.
/// Gating to edge-triggered (one IRQ per `schedule_output`) is not the
/// fix for that — it starves the kernel of the post-drain IRQ it needs to
/// notice a finished clip (the "sndm wedge after the last buffer").
static LAST_IRQ_TICKS: AtomicU64 = AtomicU64::new(0);

// ---- DMA TX ring for HDMI MAI ---------------------------------------
//
// A CPU-fed MAI feed can't tolerate trap-tail latency: at the
// 88.2 kHz subframe rate the FIFO empties in ~725 µs, and a late
// refill raises `MAI_CTL.DLATE` (a chip-reported underrun), which on
// this touchscreen-integrated panel manifests as the panel powering
// down and rebooting. Hence DMA:
//
// MAI is driven via BCM2835 DMA channel 4 paced by DREQ 17
// (BCM2835 §4.2.1.3 p.61 — Circle's `DREQSourceHDMI = 17` for
// RASPPI <= 3 confirms; Pi 4 uses 10). The layout mirrors Linux's
// dmaengine cyclic transfer (drivers/dma/bcm2835-dma.c
// `bcm2835_dma_prep_dma_cyclic`):
//
//   * N control blocks, each covering one period of the ring.
//   * Each CB's `NEXTCONBK` points to the next CB; the last CB's
//     `NEXTCONBK` loops back to CB[0]. The chain runs forever.
//   * Each CB sets `TI.INTEN`, so the BCM2835 IRQ controller fires
//     once per period completion. The IRQ handler does NOT advance
//     any DMA pointer — that's hardware's job via the chain. It
//     just increments our consumer counter so `refill_mai_dma_ring`
//     knows how much of the ring is safe to overwrite.
//   * Single-word transfers (no BURST flag), matching the bare
//     `dmas = <&dma 17>` DT cookie Linux builds the TI from.
//
// The DMA is armed exactly once in `mai_dma_init_cyclic`. It never
// stops. Start/stop of Newton clips is purely a producer-side gate
// (`OUTPUT_RUNNING`); the wire keeps emitting valid silence
// subframes between clips, so the HDMI receiver never sees a stream
// interruption and never renegotiates the link.

/// One CB per period × `N_PERIODS` periods per ring loop. CoreAudio's
/// natural period is 512 frames at 44.1 kHz ≈ 11.6 ms = 1024
/// subframes. With 8 periods × 2048 subframes each, the IRQ cadence
/// is ~23 ms and the full ring loops every ~186 ms.
///
/// The period MUST be shorter than one Newton ping-pong buffer
/// (1872 frames ≈ 42.4 ms): the period IRQ is what drives the
/// watermark nudge, so if a whole buffer can drain inside a single
/// period the kernel is asked for the next buffer too late and every
/// clip stutters at the buffer seams.
const N_PERIODS: usize = 8;
const PERIOD_SLOTS: usize = 2048;
const MAI_TX_RING_LEN: usize = N_PERIODS * PERIOD_SLOTS;

#[repr(C, align(64))]
struct MaiTxRing(core::cell::UnsafeCell<[u32; MAI_TX_RING_LEN]>);

// SAFETY: single-CPU EL2; the producer (refill_mai_dma_ring) and
// consumer (DMA + on_mai_dma_done) are serialised by EL2 trap
// context, the same as the stereo `RingState` above.
unsafe impl Sync for MaiTxRing {}

static MAI_TX_RING: MaiTxRing = MaiTxRing(core::cell::UnsafeCell::new([0u32; MAI_TX_RING_LEN]));

/// Cyclic CB chain — one CB per period, last `NEXTCONBK` loops back
/// to CB[0]. 32-byte aligned per BCM2835 §4.2.1.1 p.40 (the inner
/// `DmaCb` carries `#[repr(C, align(32))]`).
#[repr(C, align(32))]
struct MaiCbChain([crate::host::host_dma::DmaCb; N_PERIODS]);

static mut MAI_TX_CBS: MaiCbChain =
    MaiCbChain([const { crate::host::host_dma::DmaCb::zero() }; N_PERIODS]);

/// Producer cursor in subframes since cyclic-DMA arm. Monotonic u64
/// (wraps in practice never — 2^64 subframes is millions of years
/// of audio). Ring index = `(MAI_TX_HEAD.load() % MAI_TX_RING_LEN) as usize`.
/// Marks the furthest position holding *any* valid content — real
/// audio or silence pad — and never rewinds. Advanced only by
/// `refill_mai_dma_ring` after it writes new content.
static MAI_TX_HEAD: AtomicU64 = AtomicU64::new(0);

/// Furthest subframe position holding *real* guest audio (same
/// monotonic coordinate as `MAI_TX_HEAD`). Everything in
/// `[MAI_REAL_HEAD, MAI_TX_HEAD)` is silence pad, and
/// `refill_mai_dma_ring` overwrites it in place when the guest's next
/// buffer arrives — late guest audio must never queue *behind* pad
/// that was staged while waiting for it (that was an audible mid-clip
/// gap of up to the full ring). May lag arbitrarily far behind
/// `MAI_TX_HEAD` (e.g. across idle stretches); the refill clamps it
/// up to the safe write floor before use.
static MAI_REAL_HEAD: AtomicU64 = AtomicU64::new(0);

/// Completed-period count since cyclic DMA was armed. The consumer
/// cursor (in subframes) is `MAI_PERIODS_DONE * PERIOD_SLOTS`; the
/// DMA is currently reading the period at
/// `[consumer, consumer + PERIOD_SLOTS)`. Monotonic u64 — same
/// rationale as `MAI_TX_HEAD` for not worrying about wrap.
///
/// Advanced ONLY by `observe_consumer_periods`, which derives the
/// current period from the channel's live CONBLK_AD register — the
/// period-completion IRQ carries no count of its own (a late dispatch
/// coalesces several completions into one delivery, which is exactly
/// when an IRQ-counting estimate would fall behind for good).
static MAI_PERIODS_DONE: AtomicU64 = AtomicU64::new(0);

/// True once the cyclic chain has been armed. Before this,
/// `refill_mai_dma_ring`
/// is a no-op for the DMA side — only the stereo→MAI staging code
/// runs (which is harmless because nothing reads the ring).
static MAI_CYCLIC_ARMED: AtomicBool = AtomicBool::new(false);
static LAST_PERIOD_IRQ_TICKS: AtomicU64 = AtomicU64::new(0);
/// Last volume passed to `output_volume_set`; reported back by
/// `output_volume_get`. Default = `kOutputVolume_Max = 0`.
static OUTPUT_VOLUME: AtomicU32 = AtomicU32::new(0);
/// The two ping-pong buffer addresses passed to subfn 0x05.
static BUF1_ADDR: AtomicU32 = AtomicU32::new(0);
static BUF2_ADDR: AtomicU32 = AtomicU32::new(0);
/// IEC 60958 block phase for the subframe pair at monotonic position
/// `pos` (the `MAI_TX_HEAD` coordinate): frame index within the
/// 192-frame channel-status block, used to place the B-preamble.
/// Deriving the phase from stream *position* (rather than a counter
/// advanced per write) keeps the on-wire block cadence intact when a
/// refill rewrites pad slots in place or the safe-floor clamp skips
/// positions without writing them: whatever lands at position `pos`
/// always carries the phase the receiver expects there. The boot-time
/// pre-fill starts at position 0, so phase 0 is anchored to the first
/// subframe the wire ever carries.
fn iec_block_phase(pos_subframes: u64) -> u32 {
    ((pos_subframes / 2) % IEC958_BLOCK_FRAMES as u64) as u32
}
/// IEC 60958-3 consumer channel-status, 192 bits = 24 bytes, one bit
/// per frame mapped into bit 30 (C) of each subframe across a block.
///
/// Matches ALSA's `snd_pcm_iec958_default_status`, after its hwparams
/// fixups for stereo 16-bit PCM at 44.1 kHz:
///
/// - Byte 0:
///     bit 0 = 0   pro/consumer (0 = consumer)
///     bit 1 = 0   audio/non-audio (0 = linear-PCM audio)
///     bit 2 = 0   copyright asserted (ALSA default)
///     bits 3..5 = 0  no pre-emphasis
///     bits 6..7 = 0  mode 0 (the only mode currently defined)
/// - Byte 1 = 0x82 category code PCM coder + original
/// - Byte 2 = 0    source 0 / channel 0 ("do-not-take-into-account")
/// - Byte 3:
///     bits 0..3 = 0  sample frequency = 44.1 kHz
///     bits 4..5 = 0  clock accuracy = Level II (±1000 ppm)
///     bits 6..7 = 0  reserved
/// - Byte 4:
///     bit 0 = 0   maximum audio sample word length = 20 bits
///     bits 1..3 = 0b001  sample word length = 16 bits
///     bits 4..7 = 0  original sample frequency "not indicated"
/// - Bytes 5..23 = 0 (reserved / unused for consumer mode 0).
const CHANNEL_STATUS_BYTES: [u8; 24] = [
    0x00, // byte 0: consumer PCM, no pre-emphasis
    0x82, // byte 1: original + PCM coder
    0x00, // byte 2: source/channel = don't-care
    0x00, // byte 3: 44.1 kHz, accuracy Level II
    0x02, // byte 4: max=20-bit, word-length=16-bit
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

// ---------- Public entry points -------------------------------------------

pub struct PiHdmiBackend;

impl super::AudioBackend for PiHdmiBackend {
    fn init(&self) {
        init()
    }
    fn set_interrupt_mask(&self, input_mask: u32, output_mask: u32) {
        set_interrupt_mask(input_mask, output_mask)
    }
    fn set_output_buffers(&self, buf1_addr: u32, buf2_addr: u32) {
        set_output_buffers(buf1_addr, buf2_addr)
    }
    fn schedule_output(&self, which: u32, byte_count: u32) {
        schedule_output(which, byte_count)
    }
    fn start_output(&self) {
        start_output()
    }
    fn stop_output(&self) {
        stop_output()
    }
    fn output_is_running(&self) -> bool {
        output_is_running()
    }
    fn output_volume_set(&self, volume: u32) {
        output_volume_set(volume)
    }
    fn output_volume_get(&self) -> u32 {
        output_volume_get()
    }
    // `tick` keeps the default no-op: this backend completes from its
    // own DMA period IRQ, never from trap rate.
    #[cfg(nh_real_hw)]
    fn on_mai_dma_done(&self) {
        on_mai_dma_done()
    }
}

pub static BACKEND: PiHdmiBackend = PiHdmiBackend;

/// Read `(CNTPCT_EL0, CNTFRQ_EL0)` — wall-clock tick count and its
/// frequency. Used to measure the inter-period interval in
/// `on_mai_dma_done` so a late DMA completion (EL2 stall) is detected.
fn read_timer() -> (u64, u64) {
    let now: u64;
    let freq: u64;
    // SAFETY: sysreg reads, side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
            options(nomem, nostack, preserves_flags));
    }
    (now, freq)
}

pub fn init() {
    // We don't probe HDMI link state here — `display::splash` has
    // configured the framebuffer and the HDMI encoder by the time we
    // run. If the user booted without a monitor, MAI writes still
    // complete (the FIFO is on-chip); they just never escape the SoC.
    if !bringup_mai() {
        // bringup_mai already logged the specific failure. Leave
        // INIT_DONE clear so every per-sound entry point becomes a
        // safe no-op.
        return;
    }
    // Bring up the BCM2835 DMA channel that feeds MAI_DATA paced by
    // DREQ 17. Failure (channel not firmware-enabled) leaves
    // `host_dma::is_mai_ready()` false; the cyclic-arm step below
    // bails too and `refill_mai_dma_ring` becomes a silent no-op for
    // DMA — the wire
    // stays silent but the rest of the hypervisor runs.
    if !crate::host::host_dma::init_mai_tx() {
        kprintln!(
            "audio_pi_hdmi: host_dma::init_mai_tx FAILED (channel {} not enabled by firmware)",
            crate::host::host_dma::MAI_TX_CHANNEL
        );
    }
    // Build the cyclic CB chain and arm DMA. After this returns
    // true, the DMA controller is running forever, feeding the MAI
    // FIFO at DREQ-paced wire rate. `refill_mai_dma_ring` thereafter
    // only refreshes
    // the ring contents (real audio over silence, silence over
    // silence) ahead of the consumer pointer.
    if mai_dma_init_cyclic() {
        let (cs, dbg) = crate::host::host_dma::mai_tx_diag();
        kprintln!(
            "audio_pi_hdmi: cyclic MAI DMA armed ch={} cs={:#x} dbg={:#x} \
             periods={} period_slots={}",
            crate::host::host_dma::MAI_TX_CHANNEL,
            cs,
            dbg,
            N_PERIODS,
            PERIOD_SLOTS,
        );
    }
    INIT_DONE.store(true, Ordering::Release);

    kprintln!(
        "audio_pi_hdmi: MAI initialised, output {} Hz stereo PCM",
        HDMI_RATE_HZ
    );
}

pub fn set_interrupt_mask(input_mask: u32, output_mask: u32) {
    INPUT_INT_MASK.store(input_mask, Ordering::Relaxed);
    OUTPUT_INT_MASK.store(output_mask, Ordering::Relaxed);
}

pub fn set_output_buffers(buf1_addr: u32, buf2_addr: u32) {
    BUF1_ADDR.store(buf1_addr, Ordering::Relaxed);
    BUF2_ADDR.store(buf2_addr, Ordering::Relaxed);
}

pub fn output_volume_set(volume: u32) {
    OUTPUT_VOLUME.store(volume, Ordering::Relaxed);
}

pub fn output_volume_get() -> u32 {
    OUTPUT_VOLUME.load(Ordering::Relaxed)
}

pub fn start_output() {
    if !INIT_DONE.load(Ordering::Acquire) {
        return;
    }
    OUTPUT_RUNNING.store(true, Ordering::Release);
    // `MAI_CTL.ENABLE` is set once in `bringup_mai` and is left on
    // for the lifetime of the hypervisor. Toggling it per clip
    // (Newton calls subfn 0x0F StopOutput → 0x0D StartOutput
    // between sounds) causes the HDMI receiver to renegotiate the
    // audio capability of the link, which on the touchscreen-panel
    // we target manifests as a full panel boot — see the
    // doc-comment on `stop_output` for the symptom chain.
    // `refill_mai_dma_ring`
    // keeps the MAI FIFO continuously fed (real samples while
    // OUTPUT_RUNNING is true, silence padding otherwise) so we
    // never need to toggle ENABLE to stop audible playback.
}

/// Subfn 0x0F. The Newton kernel calls this between sound clips. We
/// drop OUTPUT_RUNNING so `maybe_raise_watermark_irq` stops
/// nudging the kernel, and we discard any unplayed stereo samples
/// (the kernel will queue fresh ones for the next clip). We
/// deliberately do NOT clear `MAI_CTL.ENABLE` or assert RESET: doing
/// that drops the HDMI link's audio capability long enough for the
/// receiver to renegotiate, which on the touchscreen-panel target
/// reboots the whole panel (and its USB-attached touchscreen, hence
/// the dwc2 XACT_ERR storm that follows). `bringup_mai` set ENABLE
/// once at init time and leaves it on; `refill_mai_dma_ring` keeps the
/// MAI FIFO fed with silence between clips so the wire stays continuous.
pub fn stop_output() {
    OUTPUT_RUNNING.store(false, Ordering::Release);
    // Drop any in-flight samples — start fresh on the next clip.
    let ring = ring_state();
    let head = ring.head.load(Ordering::Acquire);
    ring.tail.store(head, Ordering::Release);
}

/// Subfn 0x13. Per TCoreAudioSoundManager::OutputIsRunning
/// (TCoreAudioSoundManager.cpp:321-325): `return !mOutputBuffer->IsEmpty();`
/// — the kernel sees "running" iff there's still queued audio to
/// play out. Once the ring is empty, OutputIsRunning returns false
/// and the kernel knows the current clip is done.
pub fn output_is_running() -> bool {
    let ring = ring_state();
    let head = ring.head.load(Ordering::Acquire);
    let tail = ring.tail.load(Ordering::Acquire);
    head != tail
}

pub fn schedule_output(which: u32, byte_count: u32) {
    if !INIT_DONE.load(Ordering::Acquire) {
        return;
    }
    let base = if which == 0 {
        BUF1_ADDR.load(Ordering::Relaxed)
    } else {
        BUF2_ADDR.load(Ordering::Relaxed)
    };
    // Per TCoreAudioSoundManager::ScheduleOutput
    // (TCoreAudioSoundManager.cpp:261-273): queue the samples and
    // return. The IRQ is NOT raised here (note Einstein's explicit
    // commented-out `// RaiseOutputInterrupt();` at line 271) — IRQ
    // generation is the consumer's job, fired from the playback
    // side (our `maybe_raise_watermark_irq`) when the buffer is
    // running low.
    if byte_count == 0 {
        // Einstein's null/PulseAudio backends treat a zero-size schedule
        // as the end of the current output run. Keep HDMI MAI physically
        // streaming, but stop the Newton-facing producer state so the
        // kernel does not wait forever for another buffer-done edge.
        stop_output();
        return;
    }
    if base == 0 {
        return;
    }
    let input_samples = (byte_count / 2) as usize;
    let ring = ring_state();
    let mut head = ring.head.load(Ordering::Acquire);
    let tail_start = ring.tail.load(Ordering::Acquire);
    let space = RING_FRAMES - ((head.wrapping_sub(tail_start)) as usize);
    // Partial-fill on overrun so we don't silently lose entire
    // buffers. Audible glitch is bounded to the missing tail.
    let writable_input_samples = core::cmp::min(input_samples, space / 2);
    if writable_input_samples < input_samples {
        ring_overrun_log(input_samples * 2, space);
    }
    let mut input_idx = 0usize;
    while input_idx < writable_input_samples {
        let va = base + (input_idx as u32) * 2;
        let s_be = match crate::hv::guest_endian::guest_read_u16_va(va) {
            Some(v) => v as i16,
            None => 0,
        };
        let frame = encode_stereo_frame(s_be);
        let slot = (head as usize) & RING_MASK;
        // SAFETY: head is our exclusive producer index; the
        // consumer never reads slots beyond `tail < head`.
        unsafe {
            let base = ring.frames.get() as *mut StereoFrame;
            *base.add(slot) = frame;
            let slot2 = (head.wrapping_add(1) as usize) & RING_MASK;
            *base.add(slot2) = frame;
        }
        head = head.wrapping_add(2);
        input_idx += 1;
    }
    ring.head.store(head, Ordering::Release);

    // Immediately stage what we just queued into the MAI DMA ring
    // so the new audio reaches the wire promptly instead of waiting
    // up to one period (~23 ms) for the next DMA-completion IRQ.
    // No watermark-IRQ check here: the kernel JUST handed us data;
    // nudging it again would spin.
    refill_mai_dma_ring();
}

/// Top up the MAI DMA ring ahead of the consumer pointer. Called on
/// the natural audio clock (the DMA period-completion IRQ in
/// `on_mai_dma_done`) and on producer activity (the end of
/// `schedule_output`). NOT called from the trap-tail loop — audio
/// liveness must not depend on trap rate, which we want to reduce.
///
/// This is the bare-metal analogue of Linux's
/// `vchan_cyclic_callback` → `snd_pcm_period_elapsed` →
/// "refill the period that just completed" chain.
///
/// Phases:
/// 1. Drain real stereo frames the kernel queued via
///    `schedule_output` and SPDIF-encode them into the MAI ring,
///    starting at `MAI_REAL_HEAD` — *overwriting* any silence pad
///    staged past that point while we were waiting for the guest.
/// 2. Pad with silence subframes beyond all real content so the DMA
///    never reads stale slots: up to the active cushion while a clip
///    is in flight, or the whole safe window when output is idle.
///    The DMA never sees an underrun — between clips it reads the
///    silence we wrote.
fn refill_mai_dma_ring() {
    if !INIT_DONE.load(Ordering::Acquire) {
        return;
    }
    if !MAI_CYCLIC_ARMED.load(Ordering::Acquire) {
        return;
    }

    let ring = ring_state();
    let head_stereo = ring.head.load(Ordering::Acquire);
    let mut tail_stereo = ring.tail.load(Ordering::Acquire);

    // Consumer cursor in subframes, monotonic u64, resynced from the
    // DMA channel's live CONBLK_AD so it tracks the hardware even
    // when the period IRQ is late (see `observe_consumer_periods`).
    // The DMA is reading [consumer, consumer + PERIOD_SLOTS) right
    // now; that entire period is OFF-LIMITS to writes — racing the
    // DMA's current read produces torn IEC subframes (audible as a
    // periodic click at the period rate, ~43 Hz).
    let periods_done = observe_consumer_periods("refill");
    let consumer = periods_done.saturating_mul(PERIOD_SLOTS as u64);
    // Minimum-safe write position: one full period ahead of the
    // period the DMA is currently reading.
    let safe_floor = consumer.saturating_add(PERIOD_SLOTS as u64);
    // Maximum write position: keep the write tail from wrapping into
    // the DMA's current period from behind.
    let write_cap = consumer.saturating_add((MAI_TX_RING_LEN - PERIOD_SLOTS) as u64);

    let mut head = MAI_TX_HEAD.load(Ordering::Relaxed);
    let original_head = head;
    // If `head` has fallen below the floor (a long EL2 stall let the
    // DMA lap the producer), skip forward. The skipped slots retain
    // whatever subframe pattern they already held from earlier laps —
    // still valid IEC, just stale content.
    let adjusted_for_consumer = head < safe_floor;
    if adjusted_for_consumer {
        head = safe_floor;
    }

    // Cushion of staged content ahead of the consumer while a clip is
    // in flight. Must cover the longest gap between refill
    // opportunities or the DMA reaches slots we haven't written: EL2
    // stalls up to ~71 ms between period-IRQ dispatches have been
    // captured on hardware, so 4 periods (~93 ms; ~70 ms beyond the
    // period being read) rides those out. The cost is stop latency:
    // SPDIF-encoded real audio can't be recalled, so a clip the
    // kernel stops keeps sounding for up to the cushion depth.
    const ACTIVE_TARGET_AHEAD_SUBFRAMES: usize = 4 * PERIOD_SLOTS;
    let active_target_end = consumer
        .saturating_add(ACTIVE_TARGET_AHEAD_SUBFRAMES as u64)
        .min(write_cap);

    // Phase 1: drain real samples from the stereo ring, starting
    // where real content last ended — rewound below `head` when
    // silence pad was staged past it, so late guest audio is never
    // parked behind queued silence. No OUTPUT_RUNNING gate — the
    // kernel calls subfn 0x07 ScheduleOutputBuffer *before* subfn
    // 0x0D StartOutput, so the first buffer must stage before
    // OUTPUT_RUNNING is true. If the stereo ring is empty the loop
    // simply doesn't execute.
    let real_start = MAI_REAL_HEAD.load(Ordering::Relaxed).max(safe_floor);
    let mut real_pos = real_start;
    while tail_stereo != head_stereo && real_pos + 2 <= active_target_end {
        // SAFETY: tail_stereo < head_stereo was the invariant
        // when we entered; the slot we read is the consumer's
        // exclusive domain until we advance `tail_stereo`.
        let frame = unsafe {
            let slot = (tail_stereo as usize) & RING_MASK;
            let p = (ring.frames.get() as *const StereoFrame).add(slot);
            (*p).0
        };
        let left = (frame & 0xFFFF) as i16;
        let right = ((frame >> 16) & 0xFFFF) as i16;
        write_iec_pair_at(real_pos, left, right);
        real_pos += 2;
        tail_stereo = tail_stereo.wrapping_add(1);
    }
    ring.tail.store(tail_stereo, Ordering::Release);
    if real_pos > real_start {
        MAI_REAL_HEAD.store(real_pos, Ordering::Release);
        flush_ring_range(real_start, real_pos);
    }
    if real_pos > head {
        head = real_pos;
    }

    // Phase 2: pad with digital silence beyond all valid content.
    // While a clip is in flight, pad only to the active cushion —
    // phase 1 overwrites the pad in place if the guest's next buffer
    // arrives before the DMA gets there, so it is only audible on
    // true starvation. With output stopped and the stereo ring
    // drained, fill the whole safe window: idle audio then keeps
    // looping independent of guest progress, and the receiver never
    // sees a stream interruption.
    let idle = !OUTPUT_RUNNING.load(Ordering::Acquire) && tail_stereo == head_stereo;
    let pad_target_end = if idle { write_cap } else { active_target_end };
    let pad_start = head;
    let mut pad_pos = pad_start;
    while pad_pos + 2 <= pad_target_end {
        write_iec_pair_at(pad_pos, 0, 0);
        pad_pos += 2;
    }
    if pad_pos > pad_start {
        flush_ring_range(pad_start, pad_pos);
        head = pad_pos;
    }

    // A consumer-driven head adjustment means an EL2 stall let the
    // DMA lap our producer cursor — always worth a line.
    if adjusted_for_consumer {
        kprintln!(
            "audio_pi_hdmi: refill head adjusted {} -> {} (consumer={}, real_end={}, pad_end={})",
            original_head,
            safe_floor,
            consumer,
            real_pos,
            pad_pos
        );
    }

    if head != original_head {
        MAI_TX_HEAD.store(head, Ordering::Release);
    }
}

/// SPDIF-encode one stereo pair and write it into the MAI TX ring at
/// monotonic position `pos`. `pos` is always even (the safe floor is
/// period-aligned and every write advances by 2), and the ring length
/// is even, so a pair never straddles the ring end. The caller is
/// responsible for cache-flushing the written range.
fn write_iec_pair_at(pos: u64, left: i16, right: i16) {
    let (sf_l, sf_r) = encode_iec958_pair(left, right, iec_block_phase(pos));
    let slot = (pos % MAI_TX_RING_LEN as u64) as usize;
    // SAFETY: callers stage only positions in
    // [consumer + PERIOD_SLOTS, consumer + RING_LEN - PERIOD_SLOTS),
    // away from the period the DMA is reading; single-CPU EL2
    // serialises the producer and IRQ paths as documented on
    // `MaiTxRing`.
    unsafe {
        let buf = &mut *MAI_TX_RING.0.get();
        buf[slot] = sf_l;
        buf[slot + 1] = sf_r;
    }
}

/// Push the ring slots covering monotonic positions `[start, end)`
/// out of L1/L2 to RAM so the DMA, which reads via the uncached bus
/// alias (BCM2835 §1.2.3), sees what we wrote rather than stale
/// cache. `end - start` never exceeds the ring length (writes are
/// capped a period short of the consumer's lap), so at most one wrap
/// split is needed.
fn flush_ring_range(start: u64, end: u64) {
    if end <= start {
        return;
    }
    let ring_arm_phys = unsafe { (*MAI_TX_RING.0.get()).as_ptr() } as u64;
    let word = core::mem::size_of::<u32>();
    let len = (end - start) as usize;
    let start_slot = (start % MAI_TX_RING_LEN as u64) as usize;
    if start_slot + len <= MAI_TX_RING_LEN {
        crate::arch::cpu::dc_civac_range(
            ring_arm_phys + (start_slot * word) as u64,
            len * word,
        );
    } else {
        let first = MAI_TX_RING_LEN - start_slot;
        crate::arch::cpu::dc_civac_range(
            ring_arm_phys + (start_slot * word) as u64,
            first * word,
        );
        crate::arch::cpu::dc_civac_range(ring_arm_phys, (len - first) * word);
    }
}

/// Advance `MAI_PERIODS_DONE` to match the period the DMA channel is
/// actually reading, per its live CONBLK_AD (the bus address of the
/// CB the channel is executing right now). This is the ONLY place the
/// consumer advances: the period-completion IRQ carries no count of
/// its own — a late dispatch coalesces several completions into one
/// delivery, which is exactly when an IRQ-counting estimate falls
/// behind and every refill writes into the period the DMA is reading
/// (persistent torn audio). Deriving the count from CONBLK on every
/// observation (each period IRQ *and* each refill) also means the
/// count can never *lead* the hardware, so the old non-converging
/// "estimate leads CONBLK" state cannot arise.
///
/// The observation is modular (which of the `N_PERIODS` CBs), so a
/// stall long enough to lap the whole ring (> ~186 ms) folds; the
/// safe-floor clamp in `refill_mai_dma_ring` self-heals the ring
/// state either way. A jump of 2+ periods in one observation means a
/// full period passed with no IRQ dispatch and no refill — an EL2
/// stall exceeded ~23 ms — and is worth a line naming the observer.
fn observe_consumer_periods(source: &str) -> u64 {
    let mut periods_done = MAI_PERIODS_DONE.load(Ordering::Acquire);
    let conblk = crate::host::host_dma::mai_tx_conblk();
    let mut found = None;
    for i in 0..N_PERIODS {
        // SAFETY: address-of only — single-threaded EL2, and the CB
        // array is never moved after init.
        let cb_addr = unsafe { core::ptr::addr_of!(MAI_TX_CBS.0[i]) } as u64;
        if crate::host::host_dma::bus_addr_ram(cb_addr) == conblk {
            found = Some(i);
            break;
        }
    }
    let Some(actual) = found else {
        // Channel paused, or CONBLK read mid-CB-load; keep the last
        // count and let the next observation catch up.
        return periods_done;
    };
    let expected = (periods_done % N_PERIODS as u64) as usize;
    let lag = (actual + N_PERIODS - expected) % N_PERIODS;
    if lag != 0 {
        periods_done = MAI_PERIODS_DONE.fetch_add(lag as u64, Ordering::AcqRel) + lag as u64;
        if lag >= 2 {
            kprintln!(
                "audio_pi_hdmi: consumer jumped {} periods to {} (seen at {}) — EL2 stall exceeded a period",
                lag,
                periods_done,
                source
            );
        }
    }
    periods_done
}

/// Raise the kernel-side "give us more" IRQ when the stereo ring is
/// running low. Linux's equivalent is `RaiseOutputInterrupt` fired
/// from `vchan_cyclic_callback`'s `snd_pcm_period_elapsed`. This
/// runs strictly from `on_mai_dma_done` — never from
/// `schedule_output`, since the kernel just gave us data; nudging
/// it again immediately would spin.
///
/// `LOW_WATERMARK_FRAMES` is the queue depth at which we ask the
/// kernel for more audio. LEVEL-triggered, per the Einstein oracle
/// (`TCoreAudioSoundManager::RenderCallback`): while output is
/// running and the queue is below one Newton buffer — including
/// fully drained — every render quantum raises the interrupt. The
/// kernel ends the stream itself via `stop_output` (subfn 0x0F or a
/// zero-size `ScheduleOutput`). Our "render quantum" is the DMA
/// period IRQ (`on_mai_dma_done`); the 11 ms floor below bounds the
/// rate if periods ever fire in bursts. See the [`LAST_IRQ_TICKS`]
/// doc-comment for why this must not be edge-triggered.
fn maybe_raise_watermark_irq() {
    if !OUTPUT_RUNNING.load(Ordering::Acquire) {
        return;
    }
    let ring = ring_state();
    let head = ring.head.load(Ordering::Acquire);
    let tail = ring.tail.load(Ordering::Acquire);
    let queued = head.wrapping_sub(tail);
    const LOW_WATERMARK_FRAMES: u32 = 2000;
    if queued >= LOW_WATERMARK_FRAMES {
        return;
    }
    const NUDGE_INTERVAL_MS: u64 = 11;
    // SAFETY: sysreg reads, side-effect free.
    let (now, freq) = unsafe {
        let now: u64;
        let freq: u64;
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
            options(nomem, nostack, preserves_flags));
        (now, freq)
    };
    let interval = freq * NUDGE_INTERVAL_MS / 1000;
    let last = LAST_IRQ_TICKS.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < interval {
        return;
    }
    let output_mask = OUTPUT_INT_MASK.load(Ordering::Relaxed);
    if output_mask == 0 {
        // No mask installed yet (subfn 0x1F hasn't run).
        return;
    }
    vic::raise(output_mask);
    LAST_IRQ_TICKS.store(now, Ordering::Relaxed);
}

// ---------- Internals -----------------------------------------------------

/// Extra right-shift applied to every guest sample on top of the
/// Newton volume gain (2 bits = −12 dB). The panel was observed to
/// reboot ~250 ms into the full-scale boot chime; low-amplitude output
/// streamed for 14+ s without incident, so speaker-amp current →
/// supply brownout is the prime suspect. This attenuation keeps the
/// chime below the brownout threshold. Set to 0 for full-scale output
/// if the brownout is later root-caused and fixed at the supply.
const GUEST_SAMPLE_ATTENUATION_SHIFT: u32 = 2;

/// Q15 output gain from the Newton volume word, per Einstein's
/// `TSoundManager::OutputVolumeNormalized` (TSoundManager.cpp:78-92):
///
/// ```c
/// if (mOutputVolume == kOutputVolume_Zero /* 0x80000000 */) v = 0.0;
/// else if (mOutputVolume == kOutputVolume_Max /* 0x00000000 */) v = 1.0;
/// else v = (mOutputVolume - kOutputVolume_Min)
///        / (double)(0xffffffff - kOutputVolume_Min);
/// ```
///
/// with `kOutputVolume_Min = 0xFFDDBD71`. The subtraction is C++
/// unsigned arithmetic — values in (Min..0xFFFFFFFF] map linearly to
/// (0..1]. Einstein hands the float to the host mixer; we have no
/// hardware fader on the MAI path, so the gain is applied to the
/// samples themselves.
fn output_gain_q15() -> u32 {
    const VOLUME_ZERO: u32 = 0x8000_0000;
    const VOLUME_MAX: u32 = 0x0000_0000;
    const VOLUME_MIN: u32 = 0xFFDD_BD71;
    let vol = OUTPUT_VOLUME.load(Ordering::Relaxed);
    match vol {
        VOLUME_ZERO => 0,
        VOLUME_MAX => 1 << 15,
        v => {
            let num = v.wrapping_sub(VOLUME_MIN) as u64;
            let den = (u32::MAX - VOLUME_MIN) as u64;
            ((num << 15) / den).min(1 << 15) as u32
        }
    }
}

fn encode_stereo_frame(mono_be_sample: i16) -> StereoFrame {
    let gain = output_gain_q15() as i32;
    let scaled =
        (((mono_be_sample as i32) * gain) >> 15) >> GUEST_SAMPLE_ATTENUATION_SHIFT;
    // Newton is mono — duplicate to both channels.
    let lo = (scaled as i16 as u16) as u32;
    StereoFrame(lo | (lo << 16))
}

/// Encode a (left, right) 16-bit pair into two IEC 60958 subframes.
/// `frame_idx_in_block` is the position within the 192-frame block;
/// when it's 0, the left subframe carries the B-frame preamble bits
/// that mark the start of a block.
fn encode_iec958_pair(left: i16, right: i16, frame_idx_in_block: u32) -> (u32, u32) {
    let c_l = channel_status_bit(frame_idx_in_block);
    let c_r = c_l;
    let mut sf_l = build_iec958_subframe(left, c_l);
    let mut sf_r = build_iec958_subframe(right, c_r);
    // ALSA's IEC958 plugin supplies all software preamble nibbles:
    // Z=0x8 on left block-start, X=0x2 on other left subframes,
    // Y=0x4 on every right subframe. (Bring-up also tried Circle's
    // block-start-only marker and an all-suppressed variant; only the
    // full ALSA preamble set booted reliably — see the IEC 60958
    // preamble note above.)
    let left_preamble = if frame_idx_in_block == 0 { 0x8 } else { 0x2 };
    sf_l = (sf_l & !0xF) | left_preamble;
    sf_r = (sf_r & !0xF) | 0x4;
    (sf_l, sf_r)
}

fn build_iec958_subframe(sample: i16, channel_status_bit: u32) -> u32 {
    // 16-bit signed → 24-bit-positioned payload in bits 27..4.
    // Sign-extending into the top bits of the 24-bit field is what
    // gives a proper signed 24-bit representation.
    let sample24 = ((sample as i32) << 8) & 0x00FF_FFFF;
    let payload = ((sample24 as u32) << 4) & 0x0FFF_FFF0;
    // Bits 28 = validity (0 = valid), 29 = user (0), 30 = channel
    // status (per CS bytes), 31 = parity (computed last).
    let mut subframe = payload | (channel_status_bit << 30);
    if even_parity(subframe) {
        subframe |= 0x8000_0000;
    }
    subframe
}

/// IEC 60958 even-parity over bits 4..30 inclusive.
fn even_parity(v: u32) -> bool {
    let masked = v & 0x7FFF_FFF0;
    (masked.count_ones() & 1) != 0
}

fn channel_status_bit(frame_idx_in_block: u32) -> u32 {
    // Emit the full IEC 60958 channel-status bytes (the shipped
    // configuration). Bring-up bisected this against variants that
    // suppressed all CS, or sent only CS byte 3 / byte 4, to isolate a
    // receiver re-sync click; the full-CS variant was the one that
    // booted and played reliably.
    let byte_idx = (frame_idx_in_block / 8) as usize;
    let bit_idx = (frame_idx_in_block % 8) as u32;
    if byte_idx >= CHANNEL_STATUS_BYTES.len() {
        return 0;
    }
    ((CHANNEL_STATUS_BYTES[byte_idx] >> bit_idx) & 1) as u32
}

/// Build the cyclic CB chain (one CB per period, last CB looping
/// back to CB[0]), pre-fill the whole MAI ring with silence
/// subframes, cache-flush both, and arm DMA channel 4 with CB[0].
/// The DMA runs forever after this — see the doc-comment on
/// `MAI_TX_CBS` for the rationale.
///
/// Returns `true` if the DMA channel was actually armed. Returns
/// `false` (without arming) if `host_dma::init_mai_tx` had previously
/// failed to bring up the channel (firmware reservation, etc.).
fn mai_dma_init_cyclic() -> bool {
    use crate::host::host_dma::{
        self, bus_addr_periph, bus_addr_ram, DmaCb, DREQ_HDMI, TI_DEST_DREQ, TI_INTEN,
        TI_PERMAP_SHIFT, TI_SRC_INC, TI_WAIT_RESP,
    };
    if !host_dma::is_mai_ready() {
        return false;
    }
    if MAI_CYCLIC_ARMED.load(Ordering::Acquire) {
        return true;
    }

    // Pre-fill the entire ring with valid silent IEC subframes.
    // Without this the first ~186 ms after DMA arm would emit the
    // BSS-zero pattern: a stream of 0x00000000 u32s, which has no
    // IEC preamble and no parity bit set, and which the receiver
    // either mutes or rejects. After the pre-fill MAI_TX_HEAD points
    // one full ring ahead of the consumer; the first
    // on_mai_dma_done call will see `ahead = RING_LEN - PERIOD_SLOTS`
    // and write nothing, which is the correct steady-state shape.
    let ring_ptr = unsafe { (*MAI_TX_RING.0.get()).as_mut_ptr() };
    unsafe {
        let buf = &mut *MAI_TX_RING.0.get();
        // Iterate in stereo pairs (L then R); `iec_block_phase`
        // anchors phase 0 to position 0, so the receiver sees a
        // proper Z/X/Y preamble cadence from the first subframe.
        for i in (0..MAI_TX_RING_LEN).step_by(2) {
            let (sf_l, sf_r) = encode_iec958_pair(0, 0, iec_block_phase(i as u64));
            buf[i] = sf_l;
            buf[i + 1] = sf_r;
        }
    }
    MAI_TX_HEAD.store(MAI_TX_RING_LEN as u64, Ordering::Release);
    MAI_REAL_HEAD.store(0, Ordering::Release);
    MAI_PERIODS_DONE.store(0, Ordering::Release);

    // Build the CB chain.
    let ring_arm_phys = ring_ptr as u64;
    // Linux's `bcm2835_dma_prep_dma_cyclic` builds the TI from the DT
    // dreq cookie. For vc4_hdmi the DT entry is `dmas = <&dma 17>` —
    // bare DREQ number, no flag bits — so `BURST_LENGTH(17) = 0`
    // (single-word transfers; the BCM2835_DMA_BURST = BIT(30) flag
    // isn't in the cookie), `WIDE_SOURCE/DEST(17) = 0`, and
    // `WAIT_RESP(17) = BCM2835_DMA_WAIT_RESP`. The MEM_TO_DEV
    // direction adds `D_DREQ | S_INC`, and INT_EN goes on every CB
    // that closes a period (which is every CB in our N_PERIODS
    // chain). Note this is not `BURST_LENGTH(2)`: that is the
    // vc4_hdmi.c slave-config `maxburst = 2` value, which the
    // bcm2835-dma driver IGNORES at runtime in favor of the binary
    // BURST cookie flag.
    let ti = (DREQ_HDMI << TI_PERMAP_SHIFT)
        | TI_SRC_INC
        | TI_DEST_DREQ
        | TI_WAIT_RESP
        | TI_INTEN;
    let dest_bus = bus_addr_periph(HDMI_MAI_DATA as u32);
    // SAFETY: single-threaded init from kmain before DMA is armed.
    unsafe {
        for i in 0..N_PERIODS {
            let cb = &mut MAI_TX_CBS.0[i];
            cb.ti = ti;
            cb.source_ad = bus_addr_ram(
                ring_arm_phys + (i * PERIOD_SLOTS * core::mem::size_of::<u32>()) as u64,
            );
            cb.dest_ad = dest_bus;
            cb.txfr_len = (PERIOD_SLOTS * core::mem::size_of::<u32>()) as u32;
            cb.stride = 0;
            let next_i = (i + 1) % N_PERIODS;
            let next_cb_arm_phys =
                &MAI_TX_CBS.0[next_i] as *const DmaCb as u64;
            cb.nextconbk = bus_addr_ram(next_cb_arm_phys);
        }
    }

    // Push the ring (static zero) + CB chain to RAM so the DMA,
    // reading via the uncached bus alias 0xC000_0000 (BCM2835
    // §1.2.3), sees zeros and the live CB chain rather than stale
    // cache lines.
    crate::arch::cpu::dc_civac_range(
        ring_arm_phys,
        MAI_TX_RING_LEN * core::mem::size_of::<u32>(),
    );
    crate::arch::cpu::dc_civac_range(
        core::ptr::addr_of!(MAI_TX_CBS) as u64,
        core::mem::size_of::<MaiCbChain>(),
    );

    // Arm with CB[0]. arm_mai_tx writes CONBLK_AD and sets CS.ACTIVE;
    // the DMA controller fetches CB[0], reads `PERIOD_SLOTS` words,
    // then loads CB[0].nextconbk = &CB[1], reads its `PERIOD_SLOTS`
    // words, … and so on forever (the last CB points back to CB[0]).
    // SAFETY: MAI_TX_CBS is `'static`; the chain is stable for the
    // lifetime of the hypervisor.
    unsafe {
        host_dma::arm_mai_tx(&MAI_TX_CBS.0[0]);
    }
    MAI_CYCLIC_ARMED.store(true, Ordering::Release);
    true
}

/// DMA period-completion hook — the audio subsystem's natural clock.
/// Called from `host_dma::on_completion` when the MAI TX channel
/// raises its IRQ at a CB boundary. The hardware has already
/// advanced through the chain on its own; we
///   1. resync the period counter from the channel's CONBLK_AD (so
///      `refill_mai_dma_ring` knows how much of the ring is safe to
///      overwrite),
///   2. refill the freed period from the stereo ring + silence,
///   3. decide whether to nudge the kernel for more audio.
/// This shape mirrors Linux's `vchan_cyclic_callback` →
/// `snd_pcm_period_elapsed` flow in `bcm2835-dma.c`. No other call
/// site is needed for the audio "tick" — explicitly NOT the trap
/// tail, which the rest of the hypervisor is trying to thin out.
pub fn on_mai_dma_done() {
    let (now, freq) = read_timer();
    let last = LAST_PERIOD_IRQ_TICKS.swap(now, Ordering::AcqRel);
    let delta_us = if last == 0 {
        0
    } else {
        now.wrapping_sub(last).saturating_mul(1_000_000) / freq.max(1)
    };
    // CONBLK_AD is the sole consumer authority (see
    // `observe_consumer_periods`); the IRQ is just the guaranteed
    // observation point — and the audio clock for the watermark
    // nudge below.
    let periods_done = observe_consumer_periods("period-irq");

    refill_mai_dma_ring();
    maybe_raise_watermark_irq();

    // Only a LATE period IRQ is worth a line: an EL2 stall ate into the
    // refill margin — exactly the precondition for consumer drift and
    // audible tearing. A late period dumps the DMA CS / DEBUG registers
    // and MAI_CTL so wire-level errors (DLATE etc.) land in the same
    // line.
    //
    // "Late" = more than ~1.7 period-times between dispatches. Must
    // clear the boot-time polling quantization: a ~10 ms poll cadence
    // on top of the 23.2 ms period puts worst-case healthy dt at
    // ~33 ms (the 30.06 ms lines in earlier captures were false
    // positives at a 30 ms threshold).
    if delta_us > 40_000 {
        // SAFETY: MMIO read.
        let ctl = unsafe { read_volatile(HDMI_MAI_CTL as *const u32) };
        let ring = ring_state();
        let head = ring.head.load(Ordering::Acquire);
        let tail = ring.tail.load(Ordering::Acquire);
        let queued = head.wrapping_sub(tail);
        let mai_head = MAI_TX_HEAD.load(Ordering::Acquire);
        let consumer = periods_done.saturating_mul(PERIOD_SLOTS as u64);
        let (dma_cs, dma_dbg) = crate::host::host_dma::mai_tx_diag();
        kprintln!(
            "audio_pi_hdmi: late period {} queued={} MAI_CTL={:#x} dt_us={} ahead={} dma_cs={:#x} dma_dbg={:#x}",
            periods_done,
            queued,
            ctl,
            delta_us,
            mai_head.saturating_sub(consumer),
            dma_cs,
            dma_dbg
        );
    }
}

fn enable_hdmi_phy_rng() {
    // Circle clears TxPhyControl0.RngPowerDown on RPi <= 3 before
    // enabling MAI; Linux does the same through vc4_hdmi->phy_rng_enable.
    // The firmware modeset may leave this powered down, so make it explicit.
    // SAFETY: HDMI_TX_PHY_CTL0 is MMIO in the Device-nGnRE window.
    unsafe {
        let before = read_volatile(HDMI_TX_PHY_CTL0 as *const u32);
        write_volatile(HDMI_TX_PHY_CTL0 as *mut u32, before & !TX_PHY_CTL0_RNG_POWER_DOWN);
        dsb_sy();
    }
}

/// Resolve the live pixel clock in Hz, preferring the measured rate
/// over the firmware's "configured" rate. Returns `None` if both
/// mailbox queries fail or report 0 — the caller is then responsible
/// for refusing to come up rather than fabricating a CTS value.
fn pixel_clock_hz() -> Option<(u32, &'static str)> {
    if let Ok(hz) = crate::host::mailbox::get_clock_rate_measured(crate::host::mailbox::CLOCK_ID_PIXEL) {
        if hz != 0 {
            return Some((hz, "measured"));
        }
    }
    if let Ok(hz) = crate::host::mailbox::get_clock_rate(crate::host::mailbox::CLOCK_ID_PIXEL) {
        if hz != 0 {
            return Some((hz, "configured"));
        }
    }
    None
}

/// Empirically-known-good TMDS pixel clock for the shipped 1024×600
/// touchscreen panel. The firmware mailbox `CLOCK_ID_PIXEL` reports
/// ~85.5 MHz for this panel (it returns a PLL *source* rate, not the
/// post-divider on-wire TMDS rate), but Linux's working CTS on the
/// same panel (CTS=0xC7F8 = 51192 with N=5644 @ 44.1 kHz) back-solves
/// to mode->clock ≈ 51.2 MHz. We have no KMS modeline to read, so this
/// constant is the override used when the mailbox reading is the
/// known-bad one.
const PANEL_PIXEL_CLOCK_HZ: u64 = 51_200_000;

/// Decide which pixel clock to feed into CTS, given the live mailbox
/// reading.
///
/// A correct TMDS pixel clock for any HDMI mode Newton drives lands
/// well under 80 MHz (the panel's is ~51 MHz; even 720p60 is 74.25
/// MHz). The shipped panel's firmware mailbox instead reports a PLL
/// source rate of ~85.5 MHz — the *only* documented bad reading — so
/// any reading at/above 80 MHz is treated as that PLL-source artifact
/// and replaced with `PANEL_PIXEL_CLOCK_HZ`. A reading in the
/// plausible TMDS range (25..80 MHz) is the real post-divider rate and
/// is used directly, so a different monitor/mode gets a CTS computed
/// from its own clock. Returns `(clock_hz, provenance_label)`.
fn cts_pixel_clock_hz(measured_hz: u32) -> (u64, &'static str) {
    const TMDS_PLAUSIBLE_LO_HZ: u32 = 25_000_000;
    const PLL_SOURCE_ARTIFACT_LO_HZ: u32 = 80_000_000;
    if (TMDS_PLAUSIBLE_LO_HZ..PLL_SOURCE_ARTIFACT_LO_HZ).contains(&measured_hz) {
        (measured_hz as u64, "mailbox")
    } else {
        // ≥80 MHz: the known-bad PLL-source reading (≈85.5 MHz on the
        // shipped panel). <25 MHz: implausibly low — also distrusted.
        (PANEL_PIXEL_CLOCK_HZ, "panel-override")
    }
}

/// CTS for the HDMI ACR packet: `pixel_clock_hz * n / (128 * fs)`,
/// Linux's `vc4_hdmi_set_n_cts` formula. One home for the math so the
/// boot log and the register write can't disagree.
fn compute_cts(pixel_clock_hz: u64, n: u32) -> u32 {
    ((pixel_clock_hz * n as u64) / (128 * HDMI_RATE_HZ as u64)) as u32
}

/// Resolve the HSM ("HDMI State Machine") clock rate in Hz. This is
/// the `audio_clock` vc4_hdmi.c divides down with MAI_SMP to produce
/// the 44.1 kHz sample edge. Reading it correctly is the only way to
/// avoid pitch drift in the output stream.
///
/// Path: `CM_HSMCTL.SRC` selects a PLL output (PLLA-per / PLLC-per /
/// PLLD-per / oscillator). `CM_HSMDIV` divides that down with a
/// 12.12 integer.fractional divider. Each per-output PLL is itself
/// `OSC * (NDIV + FRAC / 2^20) / PDIV / PER_DIV`, with NDIV doubled
/// (FRAC too) when the ANA1 feedback pre-divider bit is set — all
/// fields living in the A2W_PLLx_* registers.
///
/// All offsets and field widths verified against
/// `drivers/clk/bcm/clk-bcm2835.c` (which is the kernel's only
/// authoritative source — the BCM2835 ARM Peripherals manual does
/// not document the A2W_PLL* register set at all).
///
/// Returns `None` for unsupported sources or an apparently-disabled
/// HSM clock; callers must refuse to come up rather than fabricate
/// an audio rate from thin air.
fn read_audio_clock_hz() -> Option<u32> {
    // SAFETY: CM is in the Device-nGnRE peripheral window mapped by mmu::init.
    let (ctl, div) = unsafe {
        (
            read_volatile(CM_HSMCTL as *const u32),
            read_volatile(CM_HSMDIV as *const u32),
        )
    };

    let src = ctl & CM_CTL_SRC_MASK;
    // 12.12 integer.fractional divider (clk-bcm2835 declares
    // `int_bits = 12, frac_bits = 12` for `BCM2835_CLOCK_HSM`).
    let divi = (div >> 12) & 0xFFF;
    let divf = div & 0xFFF;
    if divi == 0 && divf == 0 {
        return None;
    }
    let divider_q12: u64 = (divi as u64) * 4096 + (divf as u64);

    // Source PLL output rate, matching `bcm2835_pll_get_rate` +
    // `bcm2835_pll_divider_get_rate` in clk-bcm2835.c exactly:
    //
    // - A2W_PLLx_CTRL bits 0..9 are the integer NDIV; bits 12..14 are
    //   PDIV, a post-VCO divider (firmware normally programs 1).
    // - A2W_PLLx_FRAC bits 0..19 are the fractional NDIV.
    // - ANA1 (= ANA0 + 4) bit 14 is the feedback pre-divider: when
    //   set, the feedback path is halved, so the effective NDIV and
    //   FDIV are DOUBLED (`if (using_prediv) { ndiv *= 2; fdiv *= 2; }`).
    //   The Pi firmware runs PLLD with this bit set; ignoring it
    //   computes the VCO at half its true rate — which made the MAI
    //   sample clock land at 88.2 kHz and every HDMI sound play at
    //   double speed.
    // - A2W_PLLx_PER bits 0..7 divide the (post-PDIV) VCO down to the
    //   *_PER lane (fixed_divider = 1 for PLLA/C/D per Linux).
    fn read_pll_per_hz(
        ctrl_reg: usize,
        frac_reg: usize,
        per_reg: usize,
        ana0_reg: usize,
    ) -> Option<u32> {
        let ctrl = unsafe { read_volatile(ctrl_reg as *const u32) };
        let frac = unsafe { read_volatile(frac_reg as *const u32) };
        let per = unsafe { read_volatile(per_reg as *const u32) };
        let ana1 = unsafe { read_volatile((ana0_reg + 4) as *const u32) };
        let mut ndiv = (ctrl & 0x3FF) as u64;
        let mut frac20 = (frac & 0xFFFFF) as u64;
        let pdiv = ((ctrl & A2W_PLL_CTRL_PDIV_MASK) >> A2W_PLL_CTRL_PDIV_SHIFT) as u64;
        let per_div = (per & 0xFF) as u64;
        if ndiv == 0 || pdiv == 0 || per_div == 0 {
            return None;
        }
        if ana1 & A2W_PLL_ANA1_FB_PREDIV != 0 {
            ndiv *= 2;
            frac20 *= 2;
        }
        // VCO = OSC * (NDIV + FRAC / 2^20) / PDIV
        // PER = VCO / PER_DIV
        let osc = BCM283X_OSC_HZ as u64;
        let vco = (osc * ndiv + (osc * frac20) / (1u64 << 20)) / pdiv;
        Some((vco / per_div) as u32)
    }
    let src_hz = match src {
        1 => BCM283X_OSC_HZ,
        4 => read_pll_per_hz(A2W_PLLA_CTRL, A2W_PLLA_FRAC, A2W_PLLA_PER, A2W_PLLA_ANA0)?,
        5 => read_pll_per_hz(A2W_PLLC_CTRL, A2W_PLLC_FRAC, A2W_PLLC_PER, A2W_PLLC_ANA0)?,
        6 => read_pll_per_hz(A2W_PLLD_CTRL, A2W_PLLD_FRAC, A2W_PLLD_PER, A2W_PLLD_ANA0)?,
        _ => return None, // SRC 0=GND, 2/3=test, 7=HDMI-aux; none make sense for HSM
    };

    // HSM rate = src / (DIVI + DIVF / 4096) = src * 4096 / divider_q12.
    let rate = ((src_hz as u64) * 4096) / divider_q12;
    Some(rate as u32)
}

/// Bring the MAI block up. Returns `false` if any prerequisite clock
/// read fails (caller must then leave INIT_DONE unset so all the
/// per-sound entry points become no-ops). Both clock values land in
/// the boot log so a misread is visible without instrumentation.
fn bringup_mai() -> bool {
    // The pixel clock determines CTS. We refuse to fabricate a value:
    // a wrong CTS yields a wrong receiver-recovered audio clock, which
    // is exactly the "crunchy" symptom we're trying to avoid.
    let (pixel_clock_hz, pixel_clock_src) = match pixel_clock_hz() {
        Some(v) => v,
        None => {
            kprintln!(
                "audio_pi_hdmi: ERROR — pixel-clock mailbox query returned no \
                 usable rate; audio disabled"
            );
            return false;
        }
    };

    // HSM clock (the MAI sample-rate divider's input). Read from the
    // BCM2835 Clock Manager — the same path Linux's clk-bcm2835 walks.
    let audio_clock_hz = match read_audio_clock_hz() {
        Some(hz) if (1_000_000..1_000_000_000).contains(&hz) => hz,
        Some(hz) => {
            kprintln!(
                "audio_pi_hdmi: ERROR — HSM clock {} Hz out of expected range; audio disabled",
                hz
            );
            return false;
        }
        None => {
            kprintln!("audio_pi_hdmi: ERROR — could not read HSM clock from CM; audio disabled");
            return false;
        }
    };

    // ACR N matches `vc4_hdmi_set_n_cts` verbatim: n = 128 * fs / 1000.
    let n: u32 = hdmi_acr_n();
    // CTS follows Linux's formula
    //   cts = (pixel_clock_hz * n) / (128 * samplerate)
    // using the live pixel clock — *except* for the one known-bad
    // mailbox reading documented in `cts_pixel_clock_hz`. The pixel
    // clock that actually goes into CTS (and its provenance) is
    // resolved there in one place, then logged below as `cts_pixel`.
    let (cts_pixel_hz, cts_pixel_src) = cts_pixel_clock_hz(pixel_clock_hz);
    let cts: u32 = compute_cts(cts_pixel_hz, n);

    // MAI_SMP per `vc4_hdmi_audio_set_mai_clock`:
    //   rational_best_approximation(audio_clock, samplerate, max_n, max_m+1, &n, &m)
    //   HDMI_WRITE(HDMI_MAI_SMP, (n << 8) | (m - 1))
    // The N field is 24 bits, M field is 8 bits; pass the field's
    // numerical max + 1 to rational_best_approximation.
    let (smp_n, smp_m) = rational_best_approximation(
        audio_clock_hz,
        HDMI_RATE_HZ,
        (MAI_SMP_N_MASK >> MAI_SMP_N_SHIFT) as u64,
        ((MAI_SMP_M_MASK >> MAI_SMP_M_SHIFT) + 1) as u64,
    );
    let smp_val = ((smp_n & (MAI_SMP_N_MASK >> MAI_SMP_N_SHIFT)) << MAI_SMP_N_SHIFT)
        | ((smp_m.saturating_sub(1)) & (MAI_SMP_M_MASK >> MAI_SMP_M_SHIFT));

    kprintln!(
        "audio_pi_hdmi: pixel_clock={} Hz (src={}), cts_pixel={} Hz ({}), \
         audio_clock={} Hz, N={}, CTS={}, MAI_SMP n={} m={} ({:#x})",
        pixel_clock_hz,
        pixel_clock_src,
        cts_pixel_hz,
        cts_pixel_src,
        audio_clock_hz,
        n,
        cts,
        smp_n,
        smp_m,
        smp_val
    );

    // Channel map: per `vc4_hdmi_channel_map` for channel_mask=0b11
    //   for i in 0..8: if mask & (1<<i): map |= i << (3*i)
    // Bit 0 set → 0 << 0 = 0; bit 1 set → 1 << 3 = 0x8. Total 0x8.
    let channel_map: u32 = 0x8;
    let channel_mask: u32 = 0b11;
    // B_FRAME_IDENTIFIER — the 4-bit value the hardware scans bits 0..3
    // of each IEC subframe for to detect block starts. Must match the
    // software preamble we emit (ALSA's 0x8 block-start nibble).
    // Linux/Circle set the packetizer's zero-data flags; we do too.
    // (Bring-up also tried FORCE_SAMPLE_PRESENT and FORCE_B_FRAME here;
    // neither helped the receiver re-sync click, so both stay clear,
    // matching Linux/Circle.)
    let audio_packet_config: u32 = (IEC958_B_FRAME_PREAMBLE_ALSA
        << AUDIO_PACKET_CONFIG_B_FRAME_IDENTIFIER_SHIFT)
        | (channel_mask & AUDIO_PACKET_CONFIG_CEA_MASK_STEREO)
        | AUDIO_PACKET_CONFIG_ZERO_DATA_ON_SAMPLE_FLAT
        | AUDIO_PACKET_CONFIG_ZERO_DATA_ON_INACTIVE_CHANNELS;
    let mai_config_val: u32 = MAI_CONFIG_BIT_REVERSE
        | MAI_CONFIG_FORMAT_REVERSE
        | (channel_mask & MAI_CONFIG_CHANNEL_MASK_STEREO);
    let mai_fmt_val: u32 = (MAI_FORMAT_PCM << MAI_FMT_AUDIO_FORMAT_SHIFT)
        | (mai_sample_rate_code() << MAI_FMT_SAMPLE_RATE_SHIFT);
    // Raspberry Pi Linux vc4 gen3 thresholds (the `vc4->gen < VC4_GEN_5`
    // path taken on BCM2835/2710/2837): PANICHIGH=0x08, PANICLOW=0x08,
    // DREQHIGH=0x06, DREQLOW=0x08. (Circle's 0x1010_1010 was the other
    // candidate; the gen3 values are the ones that matched our SoC.)
    let mai_thr_val: u32 = 0x0808_0608;

    // Linear sequence below mirrors vc4_hdmi_audio_startup +
    // vc4_hdmi_audio_prepare (vc4_hdmi.c).
    //
    // SAFETY: MMIO writes in the Device-nGnRE window mapped by mmu::init.
    unsafe {
        // Linux/Circle leave MAI_CTL.PAREN clear, so we do too.
        let playback_ctl =
            (2u32 << MAI_CTL_CHNUM_SHIFT) | MAI_CTL_WHOLSMP | MAI_CTL_CHALIGN | MAI_CTL_ENABLE;

        // `vc4_hdmi_audio_startup` — RESET + FLUSH + DLATE + error
        // masking. DLATE belongs here, not per-frame: both Linux
        // (vc4_hdmi.c:2505) and Circle (hdmisoundbasedevice.cpp:388)
        // include it in this write.
        write_volatile(
            HDMI_MAI_CTL as *mut u32,
            MAI_CTL_RESET | MAI_CTL_FLUSH | MAI_CTL_DLATE | MAI_CTL_ERRORE | MAI_CTL_ERRORF,
        );

        enable_hdmi_phy_rng();

        // `vc4_hdmi_audio_set_mai_clock`.
        write_volatile(HDMI_MAI_SMP as *mut u32, smp_val);

        // `vc4_hdmi_audio_prepare` — CTL with channels + WHOLSMP +
        // CHALIGN + ENABLE. (Linux's working VC4 path enables MAI here,
        // during prepare; deferring it until after the Audio InfoFrame
        // write was tried and made no difference, so we follow Linux.)
        write_volatile(HDMI_MAI_CTL as *mut u32, playback_ctl);

        // `vc4_hdmi_audio_prepare` — MAI_FMT.
        write_volatile(HDMI_MAI_FMT as *mut u32, mai_fmt_val);

        // FIFO thresholds (gen3 values, see `mai_thr_val` above).
        write_volatile(HDMI_MAI_THR as *mut u32, mai_thr_val);

        // `vc4_hdmi_audio_prepare` — MAI_CONFIG.
        write_volatile(HDMI_MAI_CONFIG as *mut u32, mai_config_val);

        // `vc4_hdmi_audio_prepare` — channel_map.
        write_volatile(HDMI_MAI_CHANNEL_MAP as *mut u32, channel_map);

        // `vc4_hdmi_audio_prepare` — AUDIO_PACKET_CONFIG.
        write_volatile(HDMI_AUDIO_PACKET_CONFIG as *mut u32, audio_packet_config);

        // `vc4_hdmi_set_n_cts` — CRP_CFG + CTS_0/1.
        //
        // EXTERNAL_CTS_EN matters for audible quality: tested on real
        // hardware, clearing it produced a buzzy tone (the hardware's
        // auto-computed CTS doesn't lock with our N value at the
        // panel's pixel clock). So we set it and supply CTS from our
        // measured pixel_clock. The receiver re-syncs every ~1 sec
        // either way, so CTS drift isn't the cause of those resets.
        write_volatile(
            HDMI_CRP_CFG as *mut u32,
            CRP_CFG_EXTERNAL_CTS_EN | ((n & CRP_CFG_N_MASK) << CRP_CFG_N_SHIFT),
        );
        write_volatile(HDMI_CTS_0 as *mut u32, cts);
        write_volatile(HDMI_CTS_1 as *mut u32, cts);

        // Ensure all of the above MMIO writes have committed before
        // anyone (including subsequent reads in this function or in
        // start_output) observes the new state.
        dsb_sy();
    }
    // SCHEDULER_CONTROL is owned by the firmware's modeset. We verify
    // HDMI mode is active rather than writing it ourselves: writing
    // here without the rest of the modeset state risks a re-arm of
    // the encoder which observably shifts the display geometry.
    // SAFETY: MMIO read.
    let sched = unsafe { read_volatile(HDMI_SCHEDULER_CONTROL as *const u32) };
    if sched & SCHEDULER_CONTROL_MODE_HDMI == 0 {
        kprintln!(
            "audio_pi_hdmi: ERROR — SCHEDULER_CONTROL={:#x} reports HDMI mode off; \
             audio packets would be discarded at the encoder. Audio disabled.",
            sched
        );
        return false;
    }
    // Write the Audio InfoFrame ONCE at bringup. Our stream format is
    // fixed (PCM stereo 16-bit 44.1 kHz) and never changes between
    // clips, so re-arming the RAM_PACKET_CONFIG slot per StartOutput
    // would only perturb the firmware-managed HDMI schedule (visible
    // as a display re-modeset) without any semantic benefit. Linux
    // re-arms in trigger(START) because it supports rate changes; we
    // don't.
    set_audio_info_frame();
    true
}

/// Compose a CEA-861 "Audio InfoFrame" and write it into the HDMI
/// block's RAM packet area, then ask the scheduler to transmit it.
/// CEA-861-F §6.6: 10-byte payload for an audio info frame, plus a
/// 4-byte header (packet type 0x84, version 1, length 10).
///
/// Called exactly once, from `bringup_mai`. Our stream format is fixed
/// (PCM stereo 16-bit 44.1 kHz) and never changes between clips, so the
/// InfoFrame is written once at bring-up rather than re-armed per
/// `StartOutput`: re-arming the RAM_PACKET_CONFIG slot would only
/// perturb the firmware-managed HDMI schedule (visible as a display
/// re-modeset). Linux re-arms in trigger(START) because it supports
/// rate changes; we don't. See `start_output`'s comments.
///
/// Byte stream laid out per `vc4_hdmi_write_infoframe` (vc4_hdmi.c):
/// the on-wire packet is `[type, ver, len, checksum, PB1..PBn]` and
/// the hardware writes 14 bytes packed 3+4 per dword pair:
///
/// ```text
///   word 0 = buffer[0] | buffer[1]<<8 | buffer[2]<<16            (high byte zero)
///   word 1 = buffer[3] | buffer[4]<<8 | buffer[5]<<16 | buffer[6]<<24
///   word 2 = buffer[7] | buffer[8]<<8 | buffer[9]<<16 | buffer[10]<<24
///   word 3 = buffer[11] | buffer[12]<<8 | buffer[13]<<16
/// ```
///
/// `buffer[0..3]` = `{type, version, length}`, `buffer[3]` = checksum,
/// `buffer[4..]` = PB1..PB10. Note the checksum lands in the *low* byte
/// of word 1, not the high byte of word 0 — packing it into word 0
/// shifts the receiver's view of the payload by one byte and reliably
/// trashes the InfoFrame. Derived from the upstream loop:
///
/// ```c
///   writel(buffer[i] | buffer[i+1]<<8 | buffer[i+2]<<16, …);
///   writel(buffer[i+3] | buffer[i+4]<<8 | buffer[i+5]<<16 | buffer[i+6]<<24, …);
/// ```
fn set_audio_info_frame() {
    // Build the 14-byte packet stream.
    //   buffer[0..3] = header: type=0x84, ver=1, len=10.
    //   buffer[3]    = checksum (computed below).
    //   buffer[4..14] = PB1..PB10.
    //
    // PB1 = (CT<<4) | (CC&0x7), with CT=0 (refer-to-stream) and
    //       CC = channel_count - 1 = 1 (stereo).
    //   → PB1 = 0x01.
    // PB2 = 0x00, matching Linux hdmi-codec and Circle: sample
    // size/frequency are "refer to stream".
    // PB3..PB10 = 0.
    let mut buffer = [0u8; 14];
    buffer[0] = 0x84;
    buffer[1] = 0x01;
    buffer[2] = 0x0A;
    buffer[4] = 0x01;
    // PB2=0: sample size/frequency are "refer to stream header" rather
    // than duplicated in the InfoFrame (matches Linux hdmi-codec/Circle).
    buffer[5] = 0x00;
    // Checksum = -sum(bytes) over the full packet (header + payload),
    // with the checksum slot itself counted as 0.
    let mut sum: u32 = 0;
    for &b in &buffer {
        sum = sum.wrapping_add(b as u32);
    }
    buffer[3] = (0u32.wrapping_sub(sum) & 0xFF) as u8;

    // Pack as Linux's `vc4_hdmi_write_infoframe` does: 7 payload bytes
    // per 8-byte sub-block, where the first word holds 3 bytes (with
    // the high byte zero) and the second word holds 4 bytes. For a
    // 14-byte audio InfoFrame this is two sub-blocks (bytes [0..7]
    // and [7..14]):
    //
    //   for (i = 0; i < len; i += 7) {
    //     writel(buffer[i+0] | buffer[i+1]<<8 | buffer[i+2]<<16,
    //            base + packet_reg);            packet_reg += 4;
    //     writel(buffer[i+3] | buffer[i+4]<<8 | buffer[i+5]<<16 |
    //            buffer[i+6]<<24,
    //            base + packet_reg);            packet_reg += 4;
    //   }
    //
    // The 3+4 split repeats for the second pair. A 4+3 split
    // (buffer[7..11] into word2, buffer[11..14] into word3) would
    // shift buffer[10..13] by one byte slot relative to what the
    // hardware expects. Bytes 10..13 are PB7..PB10 of the Audio
    // InfoFrame, which are zero in our stream, so that is benign for
    // audio but corrupts any InfoFrame with nonzero high PB bytes
    // (AVI, SPD, etc.).
    let word0 = (buffer[0] as u32) | ((buffer[1] as u32) << 8) | ((buffer[2] as u32) << 16);
    let word1 = (buffer[3] as u32)
        | ((buffer[4] as u32) << 8)
        | ((buffer[5] as u32) << 16)
        | ((buffer[6] as u32) << 24);
    let word2 = (buffer[7] as u32) | ((buffer[8] as u32) << 8) | ((buffer[9] as u32) << 16);
    let word3 = (buffer[10] as u32)
        | ((buffer[11] as u32) << 8)
        | ((buffer[12] as u32) << 16)
        | ((buffer[13] as u32) << 24);

    // Linux's `vc4_hdmi_stop_packet` clears the slot's enable bit,
    // then polls RAM_PACKET_STATUS for the slot to read back as 0.
    // `vc4_hdmi_write_infoframe` then writes the bytes and polls
    // RAM_PACKET_STATUS for the slot to read back as 1. We match
    // that sequence exactly.
    let slot_bit: u32 = 1u32 << RAM_PACKET_AUDIO_SLOT;
    // SAFETY: HDMI RAM packet area is part of the same Device-nGnRE
    // MMIO window mapped by mmu::init.
    unsafe {
        // 1. Clear the slot enable.
        let cur = read_volatile(HDMI_RAM_PACKET_CONFIG as *const u32);
        write_volatile(HDMI_RAM_PACKET_CONFIG as *mut u32, cur & !slot_bit);
        // 2. Wait for hardware to acknowledge by clearing STATUS.
        //    Linux uses a 100 ms timeout for the same poll; we use
        //    an iteration cap calibrated against the device frame
        //    rate (worst case the AVI info-frame is mid-transmit,
        //    which takes <1 ms at 60 Hz). The cap is loud-failure:
        //    if it expires the receiver may get a torn packet, but
        //    we don't block the trap handler forever.
        if !wait_for_ram_packet_status(slot_bit, false) {
            kprintln!(
                "audio_pi_hdmi: WARNING — RAM_PACKET_STATUS slot {} did not clear",
                RAM_PACKET_AUDIO_SLOT
            );
        }

        // 3. Write the 14 packet bytes (packed 3+4 per dword pair)
        //    plus zero-fill the rest of the 36-byte slot so we don't
        //    leak whatever the firmware left behind past byte 14.
        let base = HDMI_RAM_PACKET_START + (RAM_PACKET_AUDIO_SLOT as usize) * 0x24;
        write_volatile(base as *mut u32, word0);
        write_volatile((base + 4) as *mut u32, word1);
        write_volatile((base + 8) as *mut u32, word2);
        write_volatile((base + 12) as *mut u32, word3);
        for off in (16..0x24).step_by(4) {
            write_volatile((base + off) as *mut u32, 0);
        }

        // 4. Re-enable the slot and the master packet-transmit gate.
        //    We match Linux's working RAM-packet schedule on this panel
        //    — slots 2, 3, and 4 enabled (`0x1001c`) — rather than
        //    preserving the firmware's slots 0, 2, 4 (`0x10015`), which
        //    did not transmit our audio InfoFrame reliably.
        let next = RAM_PACKET_ENABLE | (1u32 << 2) | (1u32 << 3) | slot_bit;
        write_volatile(HDMI_RAM_PACKET_CONFIG as *mut u32, next);
        // 5. Wait for the hardware to acknowledge by setting STATUS.
        if !wait_for_ram_packet_status(slot_bit, true) {
            kprintln!(
                "audio_pi_hdmi: WARNING — RAM_PACKET_STATUS slot {} did not set",
                RAM_PACKET_AUDIO_SLOT
            );
        }

        // Order: make sure the enable write is visible to the HDMI
        // block before the caller proceeds (e.g., to write MAI_CTL).
        dsb_sy();
    }
}

/// Spin until `(STATUS & mask) == expected_mask` or `expected_mask==0`
/// match holds, with a bounded iteration count. Each iteration is one
/// MMIO read, ~50–100 ns on Device-nGnRE. The cap of 200k iterations
/// is ~10–20 ms wall time, which is well above the AVI-info-frame
/// vsync period (16.6 ms at 60 Hz) and matches Linux's 100 ms poll
/// budget. Returns true on success, false on timeout.
fn wait_for_ram_packet_status(mask: u32, set: bool) -> bool {
    let target = if set { mask } else { 0 };
    for _ in 0..200_000 {
        // SAFETY: MMIO read in Device-nGnRE window.
        let status = unsafe { read_volatile(HDMI_RAM_PACKET_STATUS as *const u32) };
        if status & mask == target {
            return true;
        }
    }
    false
}

/// Stein-Brocot rational approximation of `num/denom`, bounded by
/// `max_num` and `max_denom`. Returns `(n, m)` with `n/m ≈ num/denom`
/// and `n ≤ max_num`, `m ≤ max_denom`. Mirrors Linux's
/// `rational_best_approximation` in `lib/math/rational.c`.
fn rational_best_approximation(num: u32, denom: u32, max_num: u64, max_denom: u64) -> (u32, u32) {
    let mut n = num as u64;
    let mut d = denom as u64;
    let mut a = 0u64;
    let mut b = 1u64;
    let mut c = 1u64;
    let mut d_out = 0u64;
    loop {
        if d == 0 {
            break;
        }
        let t = n / d;
        let na = a + t * c;
        let nb = b + t * d_out;
        if na > max_num || nb > max_denom {
            // Use the prior convergent.
            return (c as u32, d_out as u32);
        }
        a = c;
        b = d_out;
        c = na;
        d_out = nb;
        let nr = n - t * d;
        n = d;
        d = nr;
    }
    (c as u32, d_out as u32)
}

fn ring_overrun_log(want: usize, have: usize) {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    kprintln!(
        "audio_pi_hdmi: ring producer overrun #{} (want={} have={})",
        n + 1,
        want,
        have
    );
}
