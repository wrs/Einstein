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
//!         ↓ pump: SPDIF-encode (24-bit shift + parity)
//!   IEC 60958 subframes → MAI_DATA register
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
//! ## Polled, not DMA
//!
//! Circle's `hdmisoundbasedevice.cpp` uses a cyclic DMA channel into
//! MAI_DATA. We're polled: each `pump()` call writes up to
//! PUMP_MAX_FRAMES stereo frames while the FIFO reports not-full.
//! The trap-IRQ tail fires on every Newton timer match (~16 ms
//! cadence), and sync traps fire at multiples-of-kHz rates during
//! normal boot, so the aggregate pump cadence is comfortable for a
//! 44.1 kHz stereo feed. If a clip plays through a quiet stretch of
//! the guest where neither sync traps nor timer IRQs fire often
//! enough, we'll hear underruns and the right answer is to switch
//! to DMA.
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
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::{dprintln, kprintln, peripherals::vic};

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
//   A2W_PLLD_CTRL            +0x1140 PLLD integer NDIV (bits 0..9)
//   A2W_PLLD_FRAC            +0x1240 PLLD fractional NDIV (bits 0..19)
//   A2W_PLLD_PER             +0x1540 PLLD per-output divider (bits 0..7)
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
const A2W_PLLC_CTRL: usize = CM_BASE + 0x1120;
const A2W_PLLC_FRAC: usize = CM_BASE + 0x1220;
const A2W_PLLC_PER: usize = CM_BASE + 0x1520;
const A2W_PLLD_CTRL: usize = CM_BASE + 0x1140;
const A2W_PLLD_FRAC: usize = CM_BASE + 0x1240;
const A2W_PLLD_PER: usize = CM_BASE + 0x1540;

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
/// Parity enable. Linux's vc4_hdmi.c does not set this bit.
const MAI_CTL_PAREN: u32 = 1 << 8;
const MAI_CTL_FLUSH: u32 = 1 << 9;
const MAI_CTL_EMPTY: u32 = 1 << 10; // RO; FIFO drained-to-empty indicator.
const MAI_CTL_FULL: u32 = 1 << 11; // RO; pump bails when set.
const MAI_CTL_WHOLSMP: u32 = 1 << 12;
const MAI_CTL_CHALIGN: u32 = 1 << 13;
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
const AUDIO_PACKET_CONFIG_FORCE_SAMPLE_PRESENT: u32 = 1 << 19;
const AUDIO_PACKET_CONFIG_FORCE_B_FRAME: u32 = 1 << 18;
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
/// `enum VC4_HDMI_MAI_SAMPLE_RATE_48000 = 9` in `vc4_regs.h`.
const MAI_SAMPLE_RATE_CODE_48_KHZ: u32 = 9;

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
// Two known-good conventions exist; the pair must match:
//   - Linux/alsa-lib: 0x8 / 0x8
//   - Circle:         0xF / 0xF
//
// On real Pi Zero 2 W hardware we observed `0x8` causing intermittent
// boot hangs (~5/6) where `0xF` reliably boots; the failure mode is
// in the kernel's polling loop after StartOutput, suggesting that
// the Linux convention's more aggressive block-boundary detection
// interacts badly with the firmware-managed HDMI block we share.
// The active value is selected by `IEC_DIAGNOSTIC_MODE` below so the
// preamble and AUDIO_PACKET_CONFIG B_FRAME_IDENTIFIER stay paired.
const IEC958_B_FRAME_PREAMBLE_CIRCLE: u32 = 0xF;
const IEC958_B_FRAME_PREAMBLE_ALSA: u32 = 0x8;

/// Newton source audio parameters (Einstein PulseAudio backend,
/// TPulseAudioSoundManager.cpp).
#[allow(dead_code)] // referenced in comments; kept for future resampler work.
const NEWTON_RATE_HZ: u32 = 22050;

// ---- HDMI audio configuration ---------------------------------------------
//
// Defaults here follow the working Linux/Circle path unless the name says it
// is a tone-test probe.

/// 48 kHz cadence probe. Diagnostic-only: Newton's current resampler emits
/// 44.1 kHz, so leave this false for normal guest audio.
const TONE_TEST_48_KHZ: bool = false;
/// HDMI output audio rate for the current diagnostic build.
const HDMI_RATE_HZ: u32 = if TONE_TEST_48_KHZ { 48_000 } else { 44_100 };

/// Linux/Circle-style Audio InfoFrame: PB2=0 means sample size/frequency are
/// taken from the stream header rather than duplicated in the InfoFrame.
const AUDIO_INFOFRAME_REFER_TO_STREAM: bool = true;
/// One-shot startup register dump for the tone test.
const TONE_TEST_STARTUP_REG_LOG: bool = true;
/// Periodic logging from the tone-test feeder underruns MAI: the FIFO holds
/// less than 1 ms of stereo audio, while UART output can stall for many ms.
/// Keep per-second diagnostics off for listening tests.
const TONE_TEST_HEARTBEAT_LOG: bool = false;
/// Use Raspberry Pi Linux VC4 gen4 FIFO thresholds instead of Circle's 0x10s.
const USE_LINUX_GEN4_MAI_THRESHOLDS: bool = true;
/// Linux's working VC4 path enables MAI during prepare, before the Audio
/// InfoFrame helper returns.
const ENABLE_MAI_AFTER_INFOFRAME: bool = false;
/// Linux/Circle set the packetizer's zero-data flags.
const USE_AUDIO_PACKET_ZERO_FLAGS: bool = true;
/// Linux and Circle leave MAI_CTL.PAREN clear.
const USE_MAI_CTL_PAREN: bool = false;
/// Linux/Circle do not force every CEA channel sample as present.
const FORCE_AUDIO_SAMPLE_PRESENT: bool = false;
/// Linux/Circle do not force every IEC block boundary as a B frame.
const FORCE_AUDIO_B_FRAME: bool = false;
/// Insert a 10 ms silent notch at exactly our one-second sample boundary.
const TONE_TEST_ONE_SECOND_SILENCE_NOTCH: bool = false;
/// Match Linux/Circle by powering the HDMI TX PHY RNG before audio starts.
const ENABLE_HDMI_PHY_RNG: bool = true;
/// Match the ACR values observed from Linux on the same panel. Linux programs
/// the legacy VC4 N value and a CTS derived from its active HDMI pixel clock,
/// not the 85.5 MHz PLLH pixel rate our firmware mailbox reports.
const USE_LINUX_OBSERVED_ACR: bool = true;
/// Match Linux's working RAM packet schedule on this panel: AVI/SPD/Audio
/// slots 2, 3, and 4 enabled. Firmware leaves us with slots 0, 2, and 4.
const USE_LINUX_RAM_PACKET_CONFIG: bool = true;

// Unavoidable non-Linux infrastructure still called out explicitly:
// - MAI_DATA is CPU-fed, not cyclic DMA/DREQ.
// - HSM is inherited from the firmware-owned HDMI modeset. Directly poking
//   CM_HSMCTL/CM_HSMDIV while the firmware encoder is live is not equivalent
//   to Linux's KMS + common-clock-framework path and produced quiet hiss.
// - Normal Newton playback writes the Audio InfoFrame once at bringup, not on
//   every ALSA prepare/start.

const IEC_MODE_SUPPRESS_ALL: u8 = 0;
const IEC_MODE_ALSA_B_ONLY: u8 = 1;
const IEC_MODE_ALSA_B_AND_CS_BYTE3: u8 = 2;
const IEC_MODE_ALSA_B_AND_CS_BYTE4: u8 = 3;
const IEC_MODE_ALSA_B_AND_ALL_CS: u8 = 4;
/// IEC bisection mode. Default now matches Linux's ALSA IEC958 plugin:
/// X/Y/Z preamble nibbles plus full channel-status bytes.
const IEC_DIAGNOSTIC_MODE: u8 = IEC_MODE_ALSA_B_AND_ALL_CS;

/// IEC 60958 block size — 192 frames. The B-frame preamble marks the
/// start of each block; subsequent frames use M/W preambles which the
/// hardware inserts for us (we just set frame-counter % 192 == 0 → set
/// the B-preamble bits in our subframe).
const IEC958_BLOCK_FRAMES: u32 = 192;

const fn mai_sample_rate_code() -> u32 {
    if TONE_TEST_48_KHZ {
        MAI_SAMPLE_RATE_CODE_48_KHZ
    } else {
        MAI_SAMPLE_RATE_CODE_44_1_KHZ
    }
}

const fn hdmi_acr_n() -> u32 {
    if USE_LINUX_OBSERVED_ACR && !TONE_TEST_48_KHZ {
        5644
    } else if TONE_TEST_48_KHZ {
        6144
    } else {
        6272
    }
}

const fn iec_b_frame_preamble() -> u32 {
    if IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_ONLY
        || IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_AND_CS_BYTE3
        || IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_AND_CS_BYTE4
        || IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_AND_ALL_CS
    {
        IEC958_B_FRAME_PREAMBLE_ALSA
    } else {
        IEC958_B_FRAME_PREAMBLE_CIRCLE
    }
}

const fn use_alsa_iec_preambles() -> bool {
    IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_ONLY
        || IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_AND_CS_BYTE3
        || IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_AND_CS_BYTE4
        || IEC_DIAGNOSTIC_MODE == IEC_MODE_ALSA_B_AND_ALL_CS
}

/// Ring capacity in stereo frames. Newton ping-pongs two 1872-sample
/// buffers — at the 2× upsample to 44.1 kHz that's 7488 frames total
/// queued before our pump has drained the first half. 8192 frames
/// (~186 ms) gives enough headroom for the full ping-pong without
/// overrun. Power-of-two for cheap modulo with `& MASK`. (16384 was
/// tried earlier and produced intermittent boot hangs that we
/// suspected were BSS-layout-related; 8192 is the minimum that fits
/// the ping-pong cleanly.)
const RING_FRAMES: usize = 8192;
const RING_MASK: usize = RING_FRAMES - 1;

// ---------- State ---------------------------------------------------------

/// One stereo frame in the ring: lower 16 bits = left, upper = right.
/// Stored as LE-S16 already byte-swapped from Newton's BE-S16; the
/// SPDIF encoder in `pump` shifts each channel up into the subframe
/// payload position.
#[repr(transparent)]
#[derive(Copy, Clone, Default)]
struct StereoFrame(u32);

#[repr(align(64))]
struct RingState {
    /// Producer index (frames written, monotonic).
    head: AtomicU32,
    /// Consumer index (frames played, monotonic).
    tail: AtomicU32,
    /// `schedule_output` writes encoded stereo frames here; `pump`
    /// pulls them out and pushes to MAI_DATA.
    frames: [StereoFrame; RING_FRAMES],
}

#[allow(static_mut_refs)]
fn ring_state() -> &'static RingState {
    static mut STATE: RingState = RingState {
        head: AtomicU32::new(0),
        tail: AtomicU32::new(0),
        frames: [StereoFrame(0); RING_FRAMES],
    };
    // SAFETY: single-core EL2; all interior fields are atomics or
    // accessed only via the producer / consumer indices' ordering.
    unsafe { &STATE }
}

static INIT_DONE: AtomicBool = AtomicBool::new(false);
static OUTPUT_RUNNING: AtomicBool = AtomicBool::new(false);
static OUTPUT_INT_MASK: AtomicU32 = AtomicU32::new(0);
static INPUT_INT_MASK: AtomicU32 = AtomicU32::new(0);
static SCHED_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
static START_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
static STOP_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
static IRQ_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
static PUMP_TICK_LOG: AtomicU32 = AtomicU32::new(0);
/// Edge-trigger for the consumer-side "ask for more" IRQ in `pump`.
/// Set true when ring fill first crosses below the low-watermark;
/// reset by `schedule_output` once fresh samples have arrived, so
/// the next crossing can raise the IRQ again.
static WATERMARK_CROSSED: AtomicBool = AtomicBool::new(false);
/// Last volume passed to `output_volume_set`; reported back by
/// `output_volume_get`. Default = `kOutputVolume_Max = 0`.
static OUTPUT_VOLUME: AtomicU32 = AtomicU32::new(0);
/// The two ping-pong buffer addresses passed to subfn 0x05.
static BUF1_ADDR: AtomicU32 = AtomicU32::new(0);
static BUF2_ADDR: AtomicU32 = AtomicU32::new(0);
/// IEC 60958 frame counter (mod 192). Used to set the B-frame
/// preamble bits on the first subframe of each block.
static IEC_FRAME_CTR: AtomicU32 = AtomicU32::new(0);
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

/// Diagnostic toggle: when `true`, `init` brings up MAI and then
/// hijacks the CPU to play a 200 Hz triangle wave forever via the
/// HDMI MAI block, bypassing all Newton-kernel sound integration.
///
/// If you hear a clean continuous tone, the entire HDMI/MAI path is
/// good — clocks, FIFO, IEC subframe encoding, AUDIO_PACKET_CONFIG,
/// CRP/CTS, the receiver. If you hear noise, distortion, or silence,
/// the audio path itself is broken and the kernel-side issues are
/// downstream of that. Either way it isolates the problem.
const TONE_TEST: bool = false;

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
    INIT_DONE.store(true, Ordering::Release);

    if TONE_TEST {
        // Self-test: take over the CPU and play a pure tone forever.
        // The Newton kernel never runs. Diagnostic-only.
        play_test_tone();
    }

    // Post-init register dump. Useful for verifying we hit the
    // intended bit patterns when something goes wrong on real
    // hardware. The log volume is one-shot, not per-buffer.
    // SAFETY: MMIO reads in the Device-nGnRE window mapped by mmu::init.
    let (ctl, fmt, cfg, thr, smap, mcfg, apc, crp, cts, rpc, sched) = unsafe {
        (
            read_volatile(HDMI_MAI_CTL as *const u32),
            read_volatile(HDMI_MAI_FMT as *const u32),
            read_volatile(HDMI_MAI_CONFIG as *const u32),
            read_volatile(HDMI_MAI_THR as *const u32),
            read_volatile(HDMI_MAI_SMP as *const u32),
            read_volatile(HDMI_MAI_CHANNEL_MAP as *const u32),
            read_volatile(HDMI_AUDIO_PACKET_CONFIG as *const u32),
            read_volatile(HDMI_CRP_CFG as *const u32),
            read_volatile(HDMI_CTS_0 as *const u32),
            read_volatile(HDMI_RAM_PACKET_CONFIG as *const u32),
            read_volatile(HDMI_SCHEDULER_CONTROL as *const u32),
        )
    };
    kprintln!(
        "audio_pi_hdmi: MAI initialised, output {} Hz stereo PCM",
        HDMI_RATE_HZ
    );
    kprintln!(
        "audio_pi_hdmi: post-init regs CTL={:#x} FMT={:#x} CONFIG={:#x} THR={:#x}",
        ctl,
        fmt,
        cfg,
        thr
    );
    kprintln!(
        "audio_pi_hdmi: post-init regs SMP={:#x} CHMAP={:#x} APC={:#x} CRP={:#x}",
        smap,
        mcfg,
        apc,
        crp
    );
    kprintln!(
        "audio_pi_hdmi: post-init regs CTS_0={} RAM_PACKET_CONFIG={:#x} SCHEDULER={:#x}",
        cts,
        rpc,
        sched
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
    // The Audio InfoFrame is written ONCE in `bringup_mai`, not on
    // every start_output — re-arming RAM_PACKET_CONFIG forces a
    // panel re-modeset on each clip.
    mai_ctl_enable_playback();
    let n = START_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 4 {
        // SAFETY: MMIO read in the Device-nGnRE window mapped by mmu::init.
        let ctl = unsafe { read_volatile(HDMI_MAI_CTL as *const u32) };
        let ring = ring_state();
        let head = ring.head.load(Ordering::Relaxed);
        let tail = ring.tail.load(Ordering::Relaxed);
        kprintln!(
            "audio_pi_hdmi: start_output #{} MAI_CTL={:#x} ring head={} tail={}",
            n + 1,
            ctl,
            head,
            tail
        );
    }
}

pub fn stop_output() {
    OUTPUT_RUNNING.store(false, Ordering::Release);
    mai_ctl_shutdown();
    // Drop any in-flight samples — start fresh on the next clip.
    let ring = ring_state();
    let head = ring.head.load(Ordering::Acquire);
    ring.tail.store(head, Ordering::Release);
    let n = STOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 4 {
        kprintln!("audio_pi_hdmi: stop_output #{}", n + 1);
    }
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
    let n = SCHED_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        kprintln!(
            "audio_pi_hdmi: schedule_output #{} which={} bytes={:#x} base={:#x}",
            n + 1,
            which,
            byte_count,
            base
        );
    }
    // Per TCoreAudioSoundManager::ScheduleOutput
    // (TCoreAudioSoundManager.cpp:261-273): queue the samples and
    // return. The IRQ is NOT raised here (note Einstein's explicit
    // commented-out `// RaiseOutputInterrupt();` at line 271) — IRQ
    // generation is the consumer's job, fired from the playback
    // side (our `pump`) when the buffer is running low.
    if byte_count == 0 || base == 0 {
        return;
    }
    // Fresh samples are arriving — clear the edge-trigger so the next
    // dip below the watermark can fire the IRQ again.
    WATERMARK_CROSSED.store(false, Ordering::Release);
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
        let s_be = match crate::guest_endian::guest_read_u16_va(va) {
            Some(v) => v as i16,
            None => 0,
        };
        let frame = encode_stereo_frame(s_be);
        let slot = (head as usize) & RING_MASK;
        // SAFETY: head is our exclusive producer index; the
        // consumer never reads slots beyond `tail < head`.
        unsafe {
            let p = ring.frames.as_ptr().add(slot) as *mut StereoFrame;
            *p = frame;
            let slot2 = (head.wrapping_add(1) as usize) & RING_MASK;
            let p2 = ring.frames.as_ptr().add(slot2) as *mut StereoFrame;
            *p2 = frame;
        }
        head = head.wrapping_add(2);
        input_idx += 1;
    }
    ring.head.store(head, Ordering::Release);
}

pub fn pump() {
    if !INIT_DONE.load(Ordering::Acquire) {
        return;
    }
    if !OUTPUT_RUNNING.load(Ordering::Acquire) {
        return;
    }
    let ring = ring_state();
    let head = ring.head.load(Ordering::Acquire);
    let mut tail = ring.tail.load(Ordering::Acquire);

    // Drain while the ring has frames. The MAI FIFO is 64
    // 32-bit entries (per `clk-bcm2835.c` / vc4_hdmi.c — 32 stereo
    // frames = 0.73 ms of audio at 44.1 kHz), so each pump call
    // writes at most a FIFO worth before it naturally stalls. FULL is
    // per-word: do not write L+R after a single FULL check.
    while tail != head {
        // SAFETY: tail < head was the invariant when we entered the
        // loop; the slot we read here is the consumer's exclusive
        // domain until we advance `tail` at the end of the iteration.
        let frame = unsafe {
            let slot = (tail as usize) & RING_MASK;
            let p = ring.frames.as_ptr().add(slot);
            (*p).0
        };
        let left = (frame & 0xFFFF) as i16;
        let right = ((frame >> 16) & 0xFFFF) as i16;
        let frame_idx_in_block = IEC_FRAME_CTR.load(Ordering::Relaxed);
        let (sf_l, sf_r) = encode_iec958_pair(left, right, frame_idx_in_block);
        write_mai_data_wait(sf_l);
        write_mai_data_wait(sf_r);
        IEC_FRAME_CTR.store(
            (frame_idx_in_block + 1) % IEC958_BLOCK_FRAMES,
            Ordering::Relaxed,
        );
        tail = tail.wrapping_add(1);
    }
    ring.tail.store(tail, Ordering::Release);

    // Consumer-side "ask for more" IRQ, matching
    // TCoreAudioSoundManager::RenderCallback
    // (TCoreAudioSoundManager.cpp:212-255). Edge-triggered: we raise
    // ONCE per crossing-of-watermark, not every pump call. CoreAudio
    // is naturally rate-limited (~86 callbacks per second at 512-frame
    // render slots); our pump fires from every trap-handler tail
    // (sub-millisecond cadence), so without an edge-gate we'd flood
    // the kernel with hundreds of identical "feed me" IRQs while it
    // was still handling the first one.
    //
    // WATERMARK_CROSSED is reset by `schedule_output` when fresh
    // samples arrive (so the next crossing can raise again).
    const LOW_WATERMARK_FRAMES: u32 = 2000;
    if OUTPUT_RUNNING.load(Ordering::Acquire) {
        let queued = head.wrapping_sub(tail);
        if queued < LOW_WATERMARK_FRAMES && !WATERMARK_CROSSED.swap(true, Ordering::AcqRel) {
            let output_mask = OUTPUT_INT_MASK.load(Ordering::Relaxed);
            if output_mask != 0 {
                vic::raise(output_mask);
                let n = IRQ_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    dprintln!(
                        "audio_pi_hdmi: low-watermark IRQ #{} mask={:#x} queued={}",
                        n + 1,
                        output_mask,
                        queued
                    );
                }
            }
        }
    }

    // Periodic pump status — one line per power-of-two pump entry at
    // tick >= 1024 with the ring fill level and MAI_CTL value, so a
    // stuck FIFO or empty ring shows up in the log without spamming.
    // dprintln (not kprintln) because this runs in the trap tail and
    // must not block on UART.
    let tick = PUMP_TICK_LOG.fetch_add(1, Ordering::Relaxed);
    if tick.is_power_of_two() && tick >= 1024 {
        // SAFETY: MMIO read.
        let ctl = unsafe { read_volatile(HDMI_MAI_CTL as *const u32) };
        let queued = head.wrapping_sub(tail);
        dprintln!(
            "audio_pi_hdmi: pump tick={} queued={} MAI_CTL={:#x}",
            tick,
            queued,
            ctl
        );
    }
}

// ---------- Internals -----------------------------------------------------

fn encode_stereo_frame(mono_be_sample: i16) -> StereoFrame {
    // Newton is mono — duplicate to both channels.
    let lo = (mono_be_sample as u16) as u32;
    StereoFrame(lo | (lo << 16))
}

/// Encode a (left, right) 16-bit pair into two IEC 60958 subframes.
/// `frame_idx_in_block` is the position within the 192-frame block;
/// when it's 0, the left subframe carries the B-frame preamble bits
/// that mark the start of a block.
fn encode_iec958_pair(left: i16, right: i16, frame_idx_in_block: u32) -> (u32, u32) {
    // Diagnostic mode 0 forces every subframe with the same sample to be
    // byte-identical: no C-bit variation and no B-frame preamble flip.
    if IEC_DIAGNOSTIC_MODE == IEC_MODE_SUPPRESS_ALL {
        let sf_l = build_iec958_subframe(left, 0);
        let sf_r = build_iec958_subframe(right, 0);
        return (sf_l, sf_r);
    }

    let c_l = channel_status_bit(frame_idx_in_block);
    let c_r = c_l;
    let mut sf_l = build_iec958_subframe(left, c_l);
    let mut sf_r = build_iec958_subframe(right, c_r);
    if use_alsa_iec_preambles() {
        // ALSA's IEC958 plugin supplies all software preamble nibbles:
        // Z=0x8 on left block-start, X=0x2 on other left subframes,
        // Y=0x4 on every right subframe.
        let left_preamble = if frame_idx_in_block == 0 { 0x8 } else { 0x2 };
        sf_l = (sf_l & !0xF) | left_preamble;
        sf_r = (sf_r & !0xF) | 0x4;
    } else if frame_idx_in_block == 0 {
        // Circle sets only the block-start marker, on both subframes.
        sf_l = (sf_l & !0xF) | iec_b_frame_preamble();
        sf_r = (sf_r & !0xF) | iec_b_frame_preamble();
    }
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
    let byte_idx = (frame_idx_in_block / 8) as usize;
    let bit_idx = (frame_idx_in_block % 8) as u32;
    if byte_idx >= CHANNEL_STATUS_BYTES.len() {
        return 0;
    }
    match IEC_DIAGNOSTIC_MODE {
        IEC_MODE_ALSA_B_ONLY => return 0,
        IEC_MODE_ALSA_B_AND_CS_BYTE3 if byte_idx != 3 => return 0,
        IEC_MODE_ALSA_B_AND_CS_BYTE4 if byte_idx != 4 => return 0,
        IEC_MODE_ALSA_B_AND_ALL_CS => {}
        _ => return 0,
    }
    ((CHANNEL_STATUS_BYTES[byte_idx] >> bit_idx) & 1) as u32
}

fn mai_fifo_full() -> bool {
    // SAFETY: MMIO read in the Device-nGnRE window mapped by mmu::init.
    let ctl = unsafe { read_volatile(HDMI_MAI_CTL as *const u32) };
    (ctl & MAI_CTL_FULL) != 0
}

fn write_mai_data_wait(word: u32) {
    // FULL is a per-32-bit-word FIFO status, not a per-stereo-frame
    // status. Linux uses DMA DREQ pacing and Circle's polling path writes
    // one subframe per writable check; do the same to avoid FIFO-full
    // errors from writing L+R after only one slot became available.
    while mai_fifo_full() {}
    // SAFETY: HDMI_MAI_DATA is MMIO in the Device-nGnRE window.
    unsafe {
        write_volatile(HDMI_MAI_DATA as *mut u32, word);
    }
}

fn log_packet_scheduler_regs(context: &str, seconds: u64) {
    // SAFETY: MMIO reads in the Device-nGnRE window mapped by mmu::init.
    let (rpc, rps, apc, crp, cts0, cts1, sched, thr, fmt, smp) = unsafe {
        (
            read_volatile(HDMI_RAM_PACKET_CONFIG as *const u32),
            read_volatile(HDMI_RAM_PACKET_STATUS as *const u32),
            read_volatile(HDMI_AUDIO_PACKET_CONFIG as *const u32),
            read_volatile(HDMI_CRP_CFG as *const u32),
            read_volatile(HDMI_CTS_0 as *const u32),
            read_volatile(HDMI_CTS_1 as *const u32),
            read_volatile(HDMI_SCHEDULER_CONTROL as *const u32),
            read_volatile(HDMI_MAI_THR as *const u32),
            read_volatile(HDMI_MAI_FMT as *const u32),
            read_volatile(HDMI_MAI_SMP as *const u32),
        )
    };
    kprintln!(
        "audio_pi_hdmi: {} t={}s RPC={:#x} RPS={:#x} APC={:#x} CRP={:#x} CTS0={} CTS1={} SCHED={:#x}",
        context, seconds, rpc, rps, apc, crp, cts0, cts1, sched
    );
    kprintln!(
        "audio_pi_hdmi: {} t={}s THR={:#x} FMT={:#x} SMP={:#x}",
        context,
        seconds,
        thr,
        fmt,
        smp
    );
}

fn enable_hdmi_phy_rng() {
    if !ENABLE_HDMI_PHY_RNG {
        return;
    }

    // Circle clears TxPhyControl0.RngPowerDown on RPi <= 3 before
    // enabling MAI; Linux does the same through vc4_hdmi->phy_rng_enable.
    // The firmware modeset may leave this powered down, so make it explicit.
    // SAFETY: HDMI_TX_PHY_CTL0 is MMIO in the Device-nGnRE window.
    let (before, after) = unsafe {
        let before = read_volatile(HDMI_TX_PHY_CTL0 as *const u32);
        let after = before & !TX_PHY_CTL0_RNG_POWER_DOWN;
        write_volatile(HDMI_TX_PHY_CTL0 as *mut u32, after);
        dsb_sy();
        (before, read_volatile(HDMI_TX_PHY_CTL0 as *const u32))
    };
    kprintln!(
        "audio_pi_hdmi: HDMI TX PHY RNG enable CTL0 {:#x} -> {:#x}",
        before,
        after
    );
}

/// Write the "playing" MAI_CTL bit pattern, mirroring
/// `vc4_hdmi_audio_trigger(START)` in vc4_hdmi.c:
///
/// ```c
/// HDMI_WRITE(HDMI_MAI_CTL,
///     VC4_SET_FIELD(channels, VC4_HD_MAI_CTL_CHNUM) |
///     VC4_HD_MAI_CTL_WHOLSMP | VC4_HD_MAI_CTL_CHALIGN |
///     VC4_HD_MAI_CTL_ENABLE);
/// ```
fn mai_ctl_enable_playback() {
    let mut ctl =
        (2u32 << MAI_CTL_CHNUM_SHIFT) | MAI_CTL_WHOLSMP | MAI_CTL_CHALIGN | MAI_CTL_ENABLE;
    if USE_MAI_CTL_PAREN {
        ctl |= MAI_CTL_PAREN;
    }
    // SAFETY: MMIO write in the Device-nGnRE window.
    unsafe {
        write_volatile(HDMI_MAI_CTL as *mut u32, ctl);
    }
}

/// Write the "stopped" MAI_CTL bit pattern, matching
/// `vc4_hdmi_audio_shutdown` in vc4_hdmi.c:
///
/// ```c
/// HDMI_WRITE(HDMI_MAI_CTL,
///     VC4_HD_MAI_CTL_RESET |
///     VC4_HD_MAI_CTL_ERRORF |
///     VC4_HD_MAI_CTL_ERRORE |
///     VC4_HD_MAI_CTL_DLATE);
/// ```
///
/// (Previously this wrote zero, which is wrong: zero doesn't assert
/// RESET, so the engine remains in a half-enabled state between
/// playbacks and the next `start_output` doesn't get a clean FIFO.)
fn mai_ctl_shutdown() {
    // SAFETY: MMIO write in the Device-nGnRE window.
    unsafe {
        write_volatile(
            HDMI_MAI_CTL as *mut u32,
            MAI_CTL_RESET | MAI_CTL_ERRORF | MAI_CTL_ERRORE | MAI_CTL_DLATE,
        );
    }
}

/// Resolve the live pixel clock in Hz, preferring the measured rate
/// over the firmware's "configured" rate. Returns `None` if both
/// mailbox queries fail or report 0 — the caller is then responsible
/// for refusing to come up rather than fabricating a CTS value.
fn pixel_clock_hz() -> Option<(u32, &'static str)> {
    if let Ok(hz) = crate::mailbox::get_clock_rate_measured(crate::mailbox::CLOCK_ID_PIXEL) {
        if hz != 0 {
            return Some((hz, "measured"));
        }
    }
    if let Ok(hz) = crate::mailbox::get_clock_rate(crate::mailbox::CLOCK_ID_PIXEL) {
        if hz != 0 {
            return Some((hz, "configured"));
        }
    }
    None
}

/// Resolve the HSM ("HDMI State Machine") clock rate in Hz. This is
/// the `audio_clock` vc4_hdmi.c divides down with MAI_SMP to produce
/// the 44.1 kHz sample edge. Reading it correctly is the only way to
/// avoid pitch drift in the output stream.
///
/// Path: `CM_HSMCTL.SRC` selects a PLL output (PLLA-per / PLLC-per /
/// PLLD-per / oscillator). `CM_HSMDIV` divides that down with a
/// 12.12 integer.fractional divider. Each per-output PLL is itself
/// `OSC * (NDIV + FRAC / 2^20) / PER_DIV`, with NDIV/FRAC/PER_DIV
/// living in the A2W_PLLx_* registers.
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

    // Source PLL output rate. The A2W_PLLx_CTRL register's bottom 10
    // bits are the integer NDIV; A2W_PLLx_FRAC's bottom 20 bits are
    // the fractional NDIV. A2W_PLLx_PER's bottom 8 bits are the
    // per-output divider that takes the VCO down to the *_PER lane.
    fn read_pll_per_hz(ctrl_reg: usize, frac_reg: usize, per_reg: usize) -> Option<u32> {
        let ctrl = unsafe { read_volatile(ctrl_reg as *const u32) };
        let frac = unsafe { read_volatile(frac_reg as *const u32) };
        let per = unsafe { read_volatile(per_reg as *const u32) };
        let ndiv = (ctrl & 0x3FF) as u64;
        let frac20 = (frac & 0xFFFFF) as u64;
        let per_div = (per & 0xFF) as u64;
        if ndiv == 0 || per_div == 0 {
            return None;
        }
        // VCO = OSC * (NDIV + FRAC / 2^20)
        //     = (OSC * NDIV * 2^20 + OSC * FRAC) / 2^20
        // PER = VCO / PER_DIV
        let osc = BCM283X_OSC_HZ as u64;
        let vco = osc * ndiv + (osc * frac20) / (1u64 << 20);
        Some((vco / per_div) as u32)
    }
    let src_hz = match src {
        1 => BCM283X_OSC_HZ,
        4 => read_pll_per_hz(A2W_PLLA_CTRL, A2W_PLLA_FRAC, A2W_PLLA_PER)?,
        5 => read_pll_per_hz(A2W_PLLC_CTRL, A2W_PLLC_FRAC, A2W_PLLC_PER)?,
        6 => read_pll_per_hz(A2W_PLLD_CTRL, A2W_PLLD_FRAC, A2W_PLLD_PER)?,
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
/// Diagnostic self-test: feed HDMI MAI forever, bypassing all
/// Newton-kernel sound integration. Three modes, chosen by these
/// const flags:
///
/// - `TONE_TEST_SILENCE = true`: all-zero samples.
/// - `TONE_TEST_SILENCE = false`, `TONE_TEST_DC = true`: constant
///   non-zero `TONE_TEST_DC_VALUE` samples (no modulation).
/// - both false: 200 Hz triangle wave.
///
/// Comparing zero vs constant-non-zero vs triangle isolates whether
/// the panel/encoder reacts to "non-zero payload anywhere" or to
/// "changing samples".
const TONE_TEST_SILENCE: bool = false;
const TONE_TEST_DC: bool = false;
const TONE_TEST_DC_VALUE: i16 = 0x4000;

fn play_test_tone() -> ! {
    if ENABLE_MAI_AFTER_INFOFRAME {
        mai_ctl_enable_playback();
    }

    if TONE_TEST_SILENCE {
        kprintln!("audio_pi_hdmi: TONE_TEST — writing zero samples forever (DC silence)");
    } else if TONE_TEST_DC {
        kprintln!("audio_pi_hdmi: TONE_TEST — writing constant non-zero ({:#x}) samples forever (DC offset)",
                  TONE_TEST_DC_VALUE);
    } else {
        kprintln!("audio_pi_hdmi: TONE_TEST — playing 200 Hz triangle wave forever");
    }
    kprintln!(
        "audio_pi_hdmi: TONE_TEST config rate={}Hz infoframe_stream={} linux_thr={} late_enable={} zero_flags={} paren={} phy_rng={} linux_acr={} linux_rpc={} iec_mode={} notch={} heartbeat_log={}",
        HDMI_RATE_HZ,
        AUDIO_INFOFRAME_REFER_TO_STREAM,
        USE_LINUX_GEN4_MAI_THRESHOLDS,
        ENABLE_MAI_AFTER_INFOFRAME,
        USE_AUDIO_PACKET_ZERO_FLAGS,
        USE_MAI_CTL_PAREN,
        ENABLE_HDMI_PHY_RNG,
        USE_LINUX_OBSERVED_ACR,
        USE_LINUX_RAM_PACKET_CONFIG,
        IEC_DIAGNOSTIC_MODE,
        TONE_TEST_ONE_SECOND_SILENCE_NOTCH,
        TONE_TEST_HEARTBEAT_LOG
    );

    // Triangle wave state.
    const AMPLITUDE: i32 = 0x4000;
    const HALF_PERIOD: i32 = (HDMI_RATE_HZ / 400) as i32;
    const STEP: i32 = AMPLITUDE * 2 / HALF_PERIOD;
    const NOTCH_FRAMES: u64 = (HDMI_RATE_HZ as u64) / 100;

    let mut value: i32 = -AMPLITUDE;
    let mut direction: i32 = STEP;
    let mut frame_idx_in_block: u32 = 0;

    // MAI_CTL monitoring state. FULL is masked out because it toggles every
    // frame as a side effect of FIFO drain. Periodic heartbeat logging stays
    // disabled for listening tests because UART output starves the FIFO.
    const CTL_DIFF_MASK: u32 = !MAI_CTL_FULL;
    let mut last_logged_ctl: u32 = 0xFFFF_FFFF; // forces first read to log.
    let mut frame_count: u64 = 0;

    loop {
        let sample: i16 = if TONE_TEST_SILENCE {
            0
        } else if TONE_TEST_DC {
            TONE_TEST_DC_VALUE
        } else if TONE_TEST_ONE_SECOND_SILENCE_NOTCH
            && frame_count % (HDMI_RATE_HZ as u64) < NOTCH_FRAMES
        {
            0
        } else {
            value as i16
        };
        let (sf_l, sf_r) = encode_iec958_pair(sample, sample, frame_idx_in_block);
        write_mai_data_wait(sf_l);
        write_mai_data_wait(sf_r);

        // Snapshot MAI_CTL after the writes. Log on any non-FULL flip
        // and (separately) on a 1 Hz heartbeat. The busy-poll above
        // rate-limits the loop to the FIFO drain rate (~44.1 kHz), so
        // `frame_count` is a faithful wall-clock proxy.
        // SAFETY: MMIO read in Device-nGnRE window.
        let ctl = unsafe { read_volatile(HDMI_MAI_CTL as *const u32) };
        if (ctl & CTL_DIFF_MASK) != (last_logged_ctl & CTL_DIFF_MASK) {
            let flips = (ctl ^ last_logged_ctl) & CTL_DIFF_MASK;
            let ms = frame_count.saturating_mul(1000) / (HDMI_RATE_HZ as u64);
            kprintln!(
                "audio_pi_hdmi: TONE_TEST t={}ms MAI_CTL {:#010x} -> {:#010x} flips={:#010x}{}{}{}{}{}{}{}{}",
                ms, last_logged_ctl, ctl, flips,
                if flips & MAI_CTL_RESET  != 0 { " RESET"  } else { "" },
                if flips & MAI_CTL_ERRORF != 0 { " ERRORF" } else { "" },
                if flips & MAI_CTL_ERRORE != 0 { " ERRORE" } else { "" },
                if flips & MAI_CTL_ENABLE != 0 { " ENABLE" } else { "" },
                if flips & MAI_CTL_PAREN  != 0 { " PAREN"  } else { "" },
                if flips & MAI_CTL_FLUSH  != 0 { " FLUSH"  } else { "" },
                if flips & MAI_CTL_EMPTY  != 0 { " EMPTY"  } else { "" },
                if flips & MAI_CTL_BUSY   != 0 { " BUSY"   } else { "" },
            );
            last_logged_ctl = ctl;
        }
        if TONE_TEST_HEARTBEAT_LOG && frame_count > 0 && frame_count % (HDMI_RATE_HZ as u64) == 0 {
            let seconds = frame_count / HDMI_RATE_HZ as u64;
            kprintln!(
                "audio_pi_hdmi: TONE_TEST heartbeat t={}s MAI_CTL={:#010x}",
                seconds,
                ctl
            );
            log_packet_scheduler_regs("TONE_TEST heartbeat", seconds);
        }
        frame_count = frame_count.wrapping_add(1);

        // Advance triangle wave state (harmless when silence mode is on).
        value += direction;
        if value >= AMPLITUDE {
            value = AMPLITUDE;
            direction = -STEP;
        } else if value <= -AMPLITUDE {
            value = -AMPLITUDE;
            direction = STEP;
        }

        // Advance IEC 60958 frame counter (mod 192-frame block).
        frame_idx_in_block = (frame_idx_in_block + 1) % IEC958_BLOCK_FRAMES;
    }
}

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

    // N value: HDMI 1.4a Table 7-1 recommends 6272 for 44.1 kHz, but
    // Linux's working VC4 stream on this exact panel programs the legacy
    // VC4 value 5644 plus CTS=0xc7f8, so we use the observed working pair.
    //
    //   CTS = (pixel_clock * N) / (128 * sample_rate)
    //       = pixel_clock / 900    at fs=44100, N=6272
    //       = pixel_clock / 1000   at fs=48000, N=6144
    let n: u32 = hdmi_acr_n();
    let cts: u32 = if USE_LINUX_OBSERVED_ACR && !TONE_TEST_48_KHZ {
        0x0000_c7f8
    } else {
        ((pixel_clock_hz as u64 * n as u64) / (128 * HDMI_RATE_HZ as u64)) as u32
    };

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
        "audio_pi_hdmi: pixel_clock={} Hz (src={}), audio_clock={} Hz, \
         N={}, CTS={}, MAI_SMP n={} m={} ({:#x})",
        pixel_clock_hz,
        pixel_clock_src,
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
    // of each IEC subframe for to detect block starts. Must match
    // the software preamble selected by `IEC_DIAGNOSTIC_MODE`.
    let mut audio_packet_config: u32 = (iec_b_frame_preamble()
        << AUDIO_PACKET_CONFIG_B_FRAME_IDENTIFIER_SHIFT)
        | (channel_mask & AUDIO_PACKET_CONFIG_CEA_MASK_STEREO);
    if USE_AUDIO_PACKET_ZERO_FLAGS {
        audio_packet_config |= AUDIO_PACKET_CONFIG_ZERO_DATA_ON_SAMPLE_FLAT
            | AUDIO_PACKET_CONFIG_ZERO_DATA_ON_INACTIVE_CHANNELS;
    }
    if FORCE_AUDIO_SAMPLE_PRESENT {
        audio_packet_config |= AUDIO_PACKET_CONFIG_FORCE_SAMPLE_PRESENT;
    }
    if FORCE_AUDIO_B_FRAME {
        audio_packet_config |= AUDIO_PACKET_CONFIG_FORCE_B_FRAME;
    }
    let mai_config_val: u32 = MAI_CONFIG_BIT_REVERSE
        | MAI_CONFIG_FORMAT_REVERSE
        | (channel_mask & MAI_CONFIG_CHANNEL_MASK_STEREO);
    let mai_fmt_val: u32 = (MAI_FORMAT_PCM << MAI_FMT_AUDIO_FORMAT_SHIFT)
        | (mai_sample_rate_code() << MAI_FMT_SAMPLE_RATE_SHIFT);
    let mai_thr_val: u32 = if USE_LINUX_GEN4_MAI_THRESHOLDS {
        // Raspberry Pi Linux vc4 gen4 thresholds:
        // PANICHIGH=0x08, PANICLOW=0x08, DREQHIGH=0x06, DREQLOW=0x08.
        0x0808_0608
    } else {
        // Circle's non-RPi5 value.
        0x1010_1010
    };

    // Linear sequence below mirrors vc4_hdmi_audio_startup +
    // vc4_hdmi_audio_prepare (vc4_hdmi.c).
    //
    // SAFETY: MMIO writes in the Device-nGnRE window mapped by mmu::init.
    unsafe {
        let playback_ctl =
            (2u32 << MAI_CTL_CHNUM_SHIFT) | MAI_CTL_WHOLSMP | MAI_CTL_CHALIGN | MAI_CTL_ENABLE;
        let playback_ctl = if USE_MAI_CTL_PAREN {
            playback_ctl | MAI_CTL_PAREN
        } else {
            playback_ctl
        };

        // `vc4_hdmi_audio_startup` — RESET + FLUSH + DLATE + error
        // masking. Both Linux (vc4_hdmi.c:2505) and Circle
        // (hdmisoundbasedevice.cpp:388) include DLATE here; previous
        // comment claimed it was per-frame-only, which was wrong.
        write_volatile(
            HDMI_MAI_CTL as *mut u32,
            MAI_CTL_RESET | MAI_CTL_FLUSH | MAI_CTL_DLATE | MAI_CTL_ERRORE | MAI_CTL_ERRORF,
        );

        enable_hdmi_phy_rng();

        // `vc4_hdmi_audio_set_mai_clock`.
        write_volatile(HDMI_MAI_SMP as *mut u32, smp_val);

        if !ENABLE_MAI_AFTER_INFOFRAME {
            // `vc4_hdmi_audio_prepare` — CTL with channels + WHOLSMP +
            // CHALIGN + ENABLE.
            write_volatile(HDMI_MAI_CTL as *mut u32, playback_ctl);
        }

        // `vc4_hdmi_audio_prepare` — MAI_FMT.
        write_volatile(HDMI_MAI_FMT as *mut u32, mai_fmt_val);

        // FIFO thresholds. Linux picks generation-specific values;
        // Circle uses 0x10 in each field on non-RPi5.
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
    // Diagnostic: skip the Audio InfoFrame write to test whether the
    // ~1 second receiver re-sync cycle is being triggered by the
    // InfoFrame contents being mis-parsed. Previous result with
    // SKIP=true AND SUPPRESS_FRAME_VARIATION=true (CS bits all zero
    // on wire) was "no effect on the click cadence". This time we're
    // turning it back on (SKIP=false) while keeping CS bits empty,
    // to test whether the click is the receiver periodically failing
    // to recognise audio format in the absence of *both* CS content
    // and a CEA-861 Audio InfoFrame.
    const SKIP_AUDIO_INFOFRAME: bool = false;
    if SKIP_AUDIO_INFOFRAME {
        kprintln!("audio_pi_hdmi: SKIP_AUDIO_INFOFRAME — not writing the Audio InfoFrame");
        return true;
    }
    // Write the Audio InfoFrame ONCE at bringup. Our stream format is
    // fixed (PCM stereo 16-bit 44.1 kHz) and never changes between
    // clips, so re-arming the RAM_PACKET_CONFIG slot per StartOutput
    // would only perturb the firmware-managed HDMI schedule (visible
    // as a display re-modeset) without any semantic benefit. Linux
    // re-arms in trigger(START) because it supports rate changes; we
    // don't.
    set_audio_info_frame();
    if TONE_TEST_STARTUP_REG_LOG {
        log_packet_scheduler_regs("post-infoframe", 0);
    }
    true
}

/// Compose a CEA-861 "Audio InfoFrame" and write it into the HDMI
/// block's RAM packet area, then ask the scheduler to transmit it.
/// CEA-861-F §6.6: 10-byte payload for an audio info frame, plus a
/// 4-byte header (packet type 0x84, version 1, length 10).
///
/// Called from `start_output` on each `StartOutput` subfn, matching
/// vc4_hdmi.c's trigger(START) sequence — receivers expect the
/// InfoFrame around the time playback begins, and the slot needs
/// re-arming each time because firmware may have rotated the RAM
/// packet schedule between starts.
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
/// `buffer[4..]` = PB1..PB10. The previous implementation packed the
/// checksum into the *high* byte of word 0 (i.e. as if it were
/// `buffer[3]` *of word 0*), which shifts the receiver's view of the
/// payload by one byte and reliably trashes the InfoFrame. Re-derived
/// from the upstream loop:
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
    buffer[5] = if AUDIO_INFOFRAME_REFER_TO_STREAM {
        0x00
    } else {
        0x09
    };
    // Checksum = -sum(bytes) over the full packet (header + payload),
    // with the checksum slot itself counted as 0.
    let mut sum: u32 = 0;
    for &b in &buffer {
        sum = sum.wrapping_add(b as u32);
    }
    buffer[3] = (0u32.wrapping_sub(sum) & 0xFF) as u8;

    let word0 = (buffer[0] as u32) | ((buffer[1] as u32) << 8) | ((buffer[2] as u32) << 16);
    let word1 = (buffer[3] as u32)
        | ((buffer[4] as u32) << 8)
        | ((buffer[5] as u32) << 16)
        | ((buffer[6] as u32) << 24);
    let word2 = (buffer[7] as u32)
        | ((buffer[8] as u32) << 8)
        | ((buffer[9] as u32) << 16)
        | ((buffer[10] as u32) << 24);
    let word3 = (buffer[11] as u32) | ((buffer[12] as u32) << 8) | ((buffer[13] as u32) << 16);

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
        //    By default preserve every other bit the firmware had set.
        //    Diagnostic override: Linux's working state on this panel
        //    enables slots 2, 3, and 4 (`0x1001c`), while firmware leaves
        //    us with slots 0, 2, and 4 (`0x10015`).
        let cur = read_volatile(HDMI_RAM_PACKET_CONFIG as *const u32);
        let next = if USE_LINUX_RAM_PACKET_CONFIG {
            RAM_PACKET_ENABLE | (1u32 << 2) | (1u32 << 3) | slot_bit
        } else {
            cur | RAM_PACKET_ENABLE | slot_bit
        };
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
    if n < 4 {
        kprintln!(
            "audio_pi_hdmi: ring producer overrun #{} (want={} have={})",
            n + 1,
            want,
            have
        );
    }
}
