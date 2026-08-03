# HDMI audio / DMA: hypervisor vs Linux

Side-by-side review of the hypervisor's HDMI audio + BCM2835-DMA path
against the Linux drivers that run on the same SoC (BCM2837 / Pi Zero
2 W). Companion notes:

- [`LINUX_VC4_HDMI_AUDIO.md`](LINUX_VC4_HDMI_AUDIO.md) — VC4 HDMI audio
  driver survey.
- [`LINUX_BCM2835_DMA.md`](LINUX_BCM2835_DMA.md) — BCM2835 DMA driver
  survey.

Files reviewed on our side:

- `src/host/audio/mod.rs` — backend selection + the sound-driver contract.
- `src/host/audio/pi_hdmi.rs` (~2000 lines) — VC4 HDMI/MAI driver, IEC
  encoder, cyclic-DMA chain, audio infoframe.
- `src/host/host_dma.rs` — BCM2835 DMA driver shared with the
  PL011 TX path.
- `src/host/platform/` — owns the BCM2835 pending-register IRQ dispatch
  (`bcm2835_irq_pending_1` → DMA channel N's `on_completion`); the
  generic IRQ path in `src/hv/trap/mod.rs` calls into `platform::`
  rather than carrying platform cfg blocks.

## 1. Linux architecture (the reference)

```
userspace (ALSA app)
   │  16/24-bit PCM, optionally pre-encoded into IEC958 subframes
   ▼
alsa-lib  (iec958 plug encodes 32-bit IEC subframes for the hdmi-codec)
   │  SNDRV_PCM_FMTBIT_IEC958_SUBFRAME_LE
   ▼
snd-dmaengine-pcm  (sound/core/pcm_dmaengine.c)
   │  dmaengine_prep_dma_cyclic(buf, period_len, MEM_TO_DEV)
   ▼
bcm2835-dma  (drivers/dma/bcm2835-dma.c)
   │  one CB per ALSA period, ring closed, INT_EN on period-boundary CB
   │  ARM phys → bus alias via phys_to_dma() (DT dma-ranges)
   ▼
VC4 HDMI MAI FIFO  (HDMI_MAI_DATA @ 0x3F808020)
   │  configured by drivers/gpu/drm/vc4/vc4_hdmi.c
   │   - MAI_SMP from rational_best_approximation(audio_clock, fs)
   │   - MAI_THR = 0x08080608   (gen3 thresholds)
   │   - MAI_CTL = CHNUM|WHOLSMP|CHALIGN|ENABLE
   │   - MAI_FMT = (fmt<<16) | (rate_code<<8)
   │   - MAI_CONFIG = BIT_REVERSE|FORMAT_REVERSE|channel_mask
   │   - MAI_CHANNEL_MAP = 0x8 stereo
   │   - AUDIO_PACKET_CONFIG = ZERO_DATA_*flags | B_FRAME_ID(8)<<10 | mask
   │   - CRP_CFG = EXTERNAL_CTS_EN | N(=128*fs/1000)
   │   - CTS_0 = CTS_1 = (pixel_kHz * 1000 * N) / (128 * fs)
   │   - Audio InfoFrame written via stop_packet → pack 7 bytes/8-word →
   │     reenable slot 4, polled on RAM_PACKET_STATUS
   ▼
HDMI Audio Sample Packets in video blanking → TMDS link → receiver
```

Key control-flow points:

- **No driver-side IEC encoding.** Userspace hands the driver fully
  formatted 32-bit IEC subframes (preamble + 24 audio bits + V/U/C/P).
- **trigger(START/STOP)** is handled inside `snd-dmaengine-pcm` and
  `sound/soc/codecs/hdmi-codec.c`; VC4 driver sees `prepare`,
  `audio_startup`, `audio_shutdown` only.
- **MAI_CTL.ENABLE** is set at `prepare` time and stays asserted until
  `audio_shutdown`. DMA stop is purely a dmaengine action.
- **Audio InfoFrame** is rewritten on every `prepare` (sample rate /
  channel count may change between opens).
- **PHY RNG** (`HDMI_TX_PHY_CTL_0` BIT(25) cleared) toggled at
  startup/shutdown.

BCM2835-DMA cyclic chain:

- One control block per ALSA period; `cb_list[N-1].next =
  cb_list[0].paddr`.
- `INT_EN` (`BIT(0)` of TI) on period-boundary CBs only.
- TI for MEM_TO_DEV peripheral-paced TX:
  `PER_MAP(dreq) | WAIT_RESP | D_DREQ | S_INC | BURST_LENGTH(3)`
  + `INT_EN` on the boundary CB. (`BURST_LENGTH(3)` is the only value
  the driver ever writes for "bursting enabled" — it ignores the
  consumer's `maxburst` value at runtime.)
- Arm: `CS = BIT(31)` (RESET) → `CONBLK_AD = first_cb_paddr` → `CS =
  ACTIVE | CS_FLAGS(dreq)`. Three writes.
- IRQ ACK: single write `CS = INT | ACTIVE | CS_FLAGS(dreq)` — clears
  the W1C `INT` bit and re-asserts ACTIVE in the same store.

## 2. Hypervisor architecture (what we built)

```
Newton guest (BE-S16 mono PCM @ 22.05 kHz, ping-pong 1872-frame buffers)
   │  HVC subfn 0x07 (ScheduleOutput)
   ▼
audio::pi_hdmi::schedule_output  (src/host/audio/pi_hdmi.rs)
   │  read guest buffer via guest_read_u16_va, 2× upsample (S&H),
   │  duplicate mono→stereo, write into stereo RING (8192 frames)
   │  → immediately call refill_mai_dma_ring()
   ▼
audio::pi_hdmi::refill_mai_dma_ring
   │  drain stereo ring → IEC subframe encoding (build_iec958_subframe)
   │   - 16-bit sample sign-extended into 24-bit IEC payload
   │   - channel-status bit per IEC 60958-3 consumer mode 0 (44.1 kHz)
   │   - ALSA-style preamble nibbles (Z=8 / X=2 / Y=4)
   │   - even parity over bits 4..30
   │  → write encoded subframes into MAI_TX_RING (16384 × u32)
   │  → cache-flush the just-written slots (dc civac, RAM aliased to
   │    bus 0xC000_0000 is the DMA's view)
   │  → fall through to silence-padding to keep the ring TARGET_AHEAD
   │    of the consumer
   ▼
BCM2835 DMA channel 4  (src/host/host_dma.rs)
   │  cyclic CB chain, N_PERIODS=4 × PERIOD_SLOTS=4096 subframes
   │  TI = (DREQ_HDMI=17 << 16) | (2 << 12 burst) | SRC_INC | DEST_DREQ
   │       | WAIT_RESP | INTEN     (INTEN on EVERY CB, since each CB is
   │                                 itself one period)
   │  cb[i].next = cb[(i+1)%N].paddr  (ring closed at build time)
   │  Arm: write CONBLK_AD = bus_addr_ram(&cb[0])
   │       write CS = ACTIVE         (channel was already reset in
   │                                  init_channel at bringup)
   ▼
HDMI MAI block @ 0x3F908000 / 0x3F808000   — bringup_mai()
   │  programmed once at init from MMU-mapped Device-nGnRE window
   │  same register sequence Linux's vc4_hdmi_audio_prepare uses
   ▼
HDMI Audio Sample Packets → TMDS → receiver
```

IRQ path:

- `trap_irq` checks `bcm2835_irq_pending_1` (GPU IRQs 32..63) and
  forwards DMA-channel completions to `host_dma::on_completion(ch)`.
- `on_completion(MAI_TX_CHANNEL)` ACKs the channel CS, then dispatches
  to `audio::on_mai_dma_done()`.
- `on_mai_dma_done` bumps `MAI_PERIODS_DONE`, refills the next period,
  and rate-limits a "give us more" IRQ back to the guest via
  `vic::raise(OUTPUT_INT_MASK)` if the stereo ring is below the low
  watermark. This is the analog of Linux's
  `vchan_cyclic_callback` → `snd_pcm_period_elapsed`.
- `schedule_output` also calls `refill_mai_dma_ring` directly so newly
  queued PCM reaches the wire without waiting one period (~46 ms).

Newton sound-driver subfn mapping (in `audio::mod.rs`):

| Subfn | Function                | Our handler                     |
|-------|-------------------------|---------------------------------|
| 0x05  | SetOutputBuffers        | `set_output_buffers(b1, b2)`    |
| 0x07  | ScheduleOutputBuffer    | `schedule_output(which, bytes)` |
| 0x0D  | StartOutput             | `start_output()`                |
| 0x0F  | StopOutput              | `stop_output()`                 |
| 0x13  | OutputIsRunning         | `output_is_running()`           |
| 0x17  | SetOutputVolume         | `output_volume_set(v)`          |
| 0x18  | GetOutputVolume         | `output_volume_get()`           |
| 0x1F  | SetInterruptMask        | `set_interrupt_mask(in,out)`    |

The guest never sees the HDMI MAI block. It thinks it's talking to
Newton's modeled sound hardware, which Einstein backed with PulseAudio
/ CoreAudio. We back the same contract with the VC4 HDMI MAI block.

## 3. Side-by-side comparison

### 3.1 MAI register programming (`bringup_mai` ↔ `vc4_hdmi_audio_prepare`)

| Register                    | Linux gen3 value                                                  | Hypervisor value                                                  | Match     |
|-----------------------------|-------------------------------------------------------------------|-------------------------------------------------------------------|-----------|
| `MAI_CTL` (initial reset)   | `RESET\|FLUSH\|DLATE\|ERRORE\|ERRORF` = `0x8207`                  | same OR-pattern                                                   | ✓         |
| `MAI_SMP`                   | rational_best_approximation(clk_get_rate(audio_clock), fs)        | `rational_best_approximation(read_audio_clock_hz(), fs, …)`       | ✓         |
| `MAI_CTL` (playback enable) | `(2<<4)\|WHOLSMP\|CHALIGN\|ENABLE` = `0x3028`                     | same (PAREN off, matches Linux)                                   | ✓         |
| `MAI_FMT`                   | `(PCM=2)<<16 \| (rate_code=8 for 44.1)<<8` = `0x00020800`         | same                                                              | ✓         |
| `MAI_THR` (gen3)            | `0x08080608`                                                      | same (`USE_LINUX_GEN4_MAI_THRESHOLDS=true`)                       | ✓         |
| `MAI_CONFIG`                | `BIT_REVERSE\|FORMAT_REVERSE\|mask=3` = `0x0C000003`              | same                                                              | ✓         |
| `MAI_CHANNEL_MAP` (stereo)  | `0x8`                                                             | `0x8`                                                             | ✓         |
| `AUDIO_PACKET_CONFIG`       | `ZERO_DATA_ON_SAMPLE_FLAT \| ZERO_DATA_ON_INACTIVE_CHANNELS \| B_FRAME_ID(8)<<10 \| mask` = `0x21002003` | same when `USE_AUDIO_PACKET_ZERO_FLAGS=true` (the default)        | ✓         |
| `CRP_CFG`                   | `EXTERNAL_CTS_EN \| N`                                            | same                                                              | ✓         |
| `N` (44.1 kHz)              | `128 * 44100 / 1000 = 5644`                                       | `5644` (`USE_LINUX_OBSERVED_ACR=true`)                            | ✓         |
| `CTS_0 = CTS_1` (44.1 kHz)  | `(mode->clock_kHz * 1000 * N) / (128 * fs)`                       | `0xC7F8 = 51192` (hard-coded "observed on this panel")            | ⚠ see §4.A |
| `TX_PHY_CTL_0 RNG_PWRDN`    | cleared (BIT(25) `&= ~`)                                          | same                                                              | ✓         |
| Audio InfoFrame             | `vc4_hdmi_write_infoframe(HDMI_INFOFRAME_TYPE_AUDIO)`, packed 7-bytes-per-8-bytes, RAM_PACKET_STATUS polled on stop and start, 100 ms timeout | `set_audio_info_frame()` builds the 14-byte buffer manually, packs 7-bytes-per-8-bytes identically, polls RAM_PACKET_STATUS both directions with ~200k iter (~10-20 ms) cap | ✓ (functionally) |
| `RAM_PACKET_CONFIG`         | preserve other slots, OR in audio slot bit (slot 4)               | optionally write Linux's observed `0x1001C` (slots 2,3,4) when `USE_LINUX_RAM_PACKET_CONFIG=true` | ✓ (with that flag set) |

Effective conclusion: the MAI bringup writes are byte-for-byte the
same as Linux's `vc4_hdmi_audio_prepare`, with one exception — CTS
(see §4.A).

### 3.2 BCM2835 DMA layer

| Aspect                          | Linux `bcm2835-dma.c`                                                                                  | Hypervisor `host_dma.rs` + `pi_hdmi.rs`                                                                  | Match     |
|---------------------------------|--------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------|-----------|
| Control block struct            | 6 active words + 2 pads, `repr(C, align(32))`                                                          | same layout                                                                                              | ✓         |
| Cyclic ring closure              | `cb_list[N-1].next = cb_list[0].paddr` after building chain                                            | `cb.nextconbk = bus_addr_ram(&MAI_TX_CBS[(i+1)%N])`, last loops to first                                 | ✓         |
| `TI.INT_EN` placement            | Only on period-boundary CBs (one per ALSA period)                                                      | On **every** CB (but each CB == one period, so net IRQ rate is the same)                                 | ✓ in effect |
| `TI.PER_MAP` (DREQ)              | from `c->dreq` (set by DT)                                                                             | `DREQ_HDMI = 17` for BCM2837 (Pi 4 would need 10; we don't target Pi 4)                                  | ✓         |
| `TI.BURST_LENGTH`                | `3` when burst flag passed via dreq cookie, else `0`                                                   | `2` (literal field value = 3-beat burst)                                                                 | ⚠ minor — §4.B |
| `TI` mode bits (TX path)         | `WAIT_RESP \| D_DREQ \| S_INC`                                                                         | same                                                                                                     | ✓         |
| Bus-address translation          | `phys_to_dma(dev, addr)` via DT `dma-ranges` (yields `\| 0xC000_0000` for RAM)                         | explicit `bus_addr_ram(arm_phys)` (RAM) / `bus_addr_periph(arm_phys)` (peripheral) helpers               | ✓ (different mechanism, same result) |
| Arm sequence                     | `CS = BIT(31)` (RESET) → `CONBLK_AD` → `CS = ACTIVE \| CS_FLAGS`                                       | one-time `init_channel`: `CS = RESET` then `CS = INT\|END`; arm: `CONBLK_AD` then `CS = ACTIVE`          | ✓ (per-arm RESET pulse is implicit because we never re-arm) |
| `CS_FLAGS` priority bits         | Whatever the consumer encoded into dreq cookie (typically zero for HDMI audio path)                    | None (`CS_ACTIVE` only) — explicitly chose against `WAIT_FOR_OUTSTANDING_WRITES` per comment             | ✓         |
| IRQ ACK                          | `writel(INT \| ACTIVE \| CS_FLAGS, CS)` — clears INT (W1C) and re-asserts ACTIVE in one store          | `cs = readl(CS); writel(cs, CS)` — same net effect (the read value has INT=1 and ACTIVE=1)               | ✓         |
| Per-channel IRQ wiring           | GPU IRQ `(16 + ch)` from BCM2835 IRQ controller `IRQ_PEND_1`                                           | same (`platform::enable_bcm2835_irq(16 + ch)`)                                                           | ✓         |
| Cache/coherency                  | `dma_pool` provides coherent memory; no `dma_sync_*` in driver                                          | `dc civac` the CB chain at arm + each refilled ring region; data ring is in normal cacheable RAM        | ✓ (different mechanism, same result) |
| Channel selection                | DT `dma-channel-mask = 0x7F35` (channels 1,3,6,7,15 reserved). HDMI audio uses whatever DT assigns      | `MAI_TX_CHANNEL = 4`, verified at `init_channel` by reading the global ENABLE register                   | ✓         |
| Stop / abort                     | `bcm2835_dma_terminate_all`: NEXTCONBK=0 → CS\|=ABORT\|ACTIVE → poll → clear ACTIVE → CS=RESET          | **none — we never stop the cyclic chain.** MAI keeps running forever; clip start/stop is producer-side  | acceptable — §4.C |

### 3.3 Linux constructs that we replace, not match

These are real Linux structures we don't have, because we don't have a
userspace and we don't have an OS sitting between Newton and the
hardware. Each one names what we use in its place.

| Linux                                  | Hypervisor analog                                                                       |
|----------------------------------------|-----------------------------------------------------------------------------------------|
| `dmaengine_prep_dma_cyclic` (one call) | `mai_dma_init_cyclic` — we build the CB chain manually at init                          |
| `snd-dmaengine-pcm` period IRQ chain   | `host_dma::on_completion` → `audio::on_mai_dma_done` → `refill_mai_dma_ring`            |
| `vchan_cyclic_callback`                | the `on_mai_dma_done` body (refill + watermark IRQ)                                     |
| `snd_pcm_period_elapsed`               | `maybe_raise_watermark_irq` raising the kernel's `OUTPUT_INT_MASK` via `vic::raise`     |
| ALSA `SNDRV_PCM_FMTBIT_IEC958_SUBFRAME_LE` (userspace encodes) | `encode_iec958_pair` / `build_iec958_subframe` (we encode in EL2, because Newton hands us raw PCM) |
| `clk_get_rate(audio_clock)` (CCF)      | `read_audio_clock_hz` walks `CM_HSMCTL`/`CM_HSMDIV`/`A2W_PLLx_*` directly               |
| `mode->clock` from KMS modeset         | `pixel_clock_hz()` via mailbox `TAG_GET_CLOCK_RATE_MEASURED` / `_CLOCK_RATE`            |
| `phys_to_dma` via DT `dma-ranges`      | hard-coded `bus_addr_ram` / `bus_addr_periph` helpers                                   |

### 3.4 Trigger START/STOP — the bigger semantic shift

Linux model:

- `prepare` enables MAI_CTL.ENABLE and writes InfoFrame.
- `trigger(START)`: dmaengine starts cyclic DMA; MAI_CTL untouched.
- `trigger(STOP)`: dmaengine terminates DMA; MAI_CTL untouched.
- `audio_shutdown`: clears MAI_CTL.ENABLE, three single-bit reset
  pulses (RESET, ERRORF, FLUSH), PHY RNG off.

Hypervisor model:

- `bringup_mai` (= Linux `prepare`) is called once at boot.
- DMA armed once in `mai_dma_init_cyclic`; never stopped.
- `start_output` only flips `OUTPUT_RUNNING` (producer gate) and
  re-arms the watermark-IRQ rate limiter. MAI_CTL.ENABLE is **not**
  toggled.
- `stop_output` flips `OUTPUT_RUNNING` off and drains the stereo ring.
  MAI_CTL.ENABLE still untouched. Silence frames continue feeding the
  wire.
- `mai_ctl_shutdown` (`RESET\|ERRORF\|ERRORE\|DLATE` OR'd into one
  write) exists but is unreferenced in normal operation — kept for
  diagnostic/teardown.

The reason for the divergence is documented in `pi_hdmi.rs::stop_output`:
toggling MAI_CTL.ENABLE between clips made the touchscreen-integrated
HDMI panel renegotiate the audio capability of the link, which on this
panel manifests as a full panel reboot (and its USB-attached
touchscreen with it). Keeping the wire continuously fed with valid
silence subframes between clips keeps the link stable.

This is the one place we **must** diverge from Linux. Linux uses a
panel that tolerates audio renegotiation; we don't.

## 4. Real divergences (numbered for follow-up)

### 4.A. Hard-coded CTS at 44.1 kHz

- Linux: `cts = (pixel_kHz * 1000 * N) / (128 * fs)` at every prepare.
- Hypervisor: when `USE_LINUX_OBSERVED_ACR=true && !TONE_TEST_48_KHZ`
  we write `0xC7F8 = 51192` literally — the value observed on this
  specific panel under Linux. Otherwise we compute `(pixel_clock_hz *
  N) / (128 * fs)`.

Why: the firmware mailbox reports the PLLH pixel-clock rate
(85.5 MHz), which doesn't match the rate Linux derives from its DRM
modeline at the same panel. Computing CTS from the firmware rate gave
a "buzzy" tone on real hardware. The hard-coded value is a workaround
for the firmware/KMS modeset asymmetry, not a real semantic
difference.

Fix: ideally read the live HDMI block's pixel-clock counter (not the
firmware's "configured" rate). The block exposes one via the HDMI HD
control registers, but we haven't reverse-engineered the offset yet.
Until then the hard-coded value is acceptable for the specific panel.

### 4.B. `TI.BURST_LENGTH = 2` (we write field 2, Linux writes field 3)

We write `(2 << 12)` in the TI field. Linux writes
`BCM2835_DMA_BURST_LENGTH(3) = (3 << 12)` whenever the consumer asks
for bursting. The field encodes "extra beats beyond the first," so we
get 3-beat bursts (12 bytes), Linux gets 4-beat bursts (16 bytes).

The original justification in `pi_hdmi.rs:1182` reads "matches Linux
vc4_hdmi maxburst = 2" — that comment refers to the `maxburst = 2`
value in the DMA slave config (vc4_hdmi.c:2887), which is the number
of beats the *consumer* requests. But bcm2835-dma's TI field is
binary: burst on (field = 3) or burst off (field = 0). The runtime
maxburst value is ignored by the DMA driver itself.

Fix: change `(2u32 << TI_BURST_LENGTH_SHIFT)` to `(3u32 <<
TI_BURST_LENGTH_SHIFT)` in `pi_hdmi.rs::mai_dma_init_cyclic`. Larger
bursts reduce DMA overhead, which only matters in marginal cases — but
it brings us bit-for-bit identical to Linux's TI word.

### 4.C. We never call the abort/teardown sequence

No `bcm2835_dma_terminate_all` analog. The hypervisor doesn't need
one in normal operation (MAI runs for the lifetime of the SoC, by
design), but a `host_dma::abort_channel` helper <!-- doc-symbols: proposed --> following the Linux
`NEXTCONBK=0 → ABORT|ACTIVE → poll → ~ACTIVE → RESET` recipe would be
worth adding the day we have a use case (e.g., reseating the HDMI link
intentionally during a guest power-cycle).

### 4.D. IEC subframe encoding location

We encode IEC subframes in EL2 because Newton produces raw 16-bit PCM
and there's no userspace to pre-encode them. Linux relies on
alsa-lib's `iec958` plug to do the encoding before any byte hits the
kernel. The on-wire bits are identical — the difference is purely
where the encoder runs.

The encoder is in `encode_iec958_pair` / `build_iec958_subframe`:

- 16-bit signed sample → sign-extended into the 24-bit IEC payload at
  bits 4..27.
- channel-status bit per IEC 60958-3 consumer mode 0 — bytes:
  `[0x00, 0x82, 0x00, 0x00, 0x02, 0..0]` (consumer + PCM + 44.1 kHz +
  16-bit). Matches `snd_pcm_iec958_default_status`'s hwparams-fixed
  values for stereo 16-bit PCM at 44.1 kHz.
- ALSA-style preamble nibbles: Z=`0x8` on left-block-start, X=`0x2`
  otherwise, Y=`0x4` on right. (Selected by
  `IEC_DIAGNOSTIC_MODE = ALSA_B_AND_ALL_CS` — the default matches
  Linux's plug.)
- Even parity over bits 4..30 in the parity bit (bit 31).

This is the right place to do it in our architecture; no fix needed.

### 4.E. `INT_EN` set on every CB (vs Linux: period boundary only)

Linux sets `TI.INT_EN` only on the CB that closes each ALSA period; if
a period spans multiple CBs (lite channels with very large periods),
the intermediate CBs run silently.

Our chain has N_PERIODS CBs and each CB *is* a full period (4096
subframes = ~46 ms). Setting INT_EN on every CB therefore produces
exactly the same IRQ cadence Linux's pattern would (one per period).
This is functional, not different.

If we ever sub-divide periods (e.g., to reduce ring-refill latency at
low buffer depths), we should switch to Linux's per-boundary pattern.

### 4.F. Per-arm RESET pulse

Linux's `bcm2835_dma_start_desc` writes `CS = BIT(31)` (RESET) before
every CONBLK_AD/ACTIVE pair. We RESET in `init_channel` once, then
arm with just `CONBLK_AD = ...; CS = ACTIVE` because we never re-arm.

If we ever introduce a stop/restart path, we should mirror Linux's
three-write sequence on every arm.

## 5. Conclusion

The HDMI MAI register sequence is byte-for-byte equivalent to Linux's
gen3 path when the conservative defaults in `pi_hdmi.rs` are set
(`USE_LINUX_GEN4_MAI_THRESHOLDS`, `USE_AUDIO_PACKET_ZERO_FLAGS`,
`USE_LINUX_OBSERVED_ACR`, `USE_LINUX_RAM_PACKET_CONFIG`,
`ENABLE_HDMI_PHY_RNG`, `IEC_DIAGNOSTIC_MODE = ALSA_B_AND_ALL_CS`,
`USE_MAI_CTL_PAREN = false`, `FORCE_AUDIO_SAMPLE_PRESENT = false`,
`FORCE_AUDIO_B_FRAME = false`) — and those are the live defaults today.

The BCM2835-DMA cyclic chain shape is equivalent to Linux's: same CB
layout, same loop closure, same DREQ-paced TI configuration, same
period-IRQ cadence. Two small mechanical bit-cleanups worth doing:

1. `TI.BURST_LENGTH` field: `2` → `3` (§4.B).
2. Add a `host_dma::abort_channel` helper <!-- doc-symbols: proposed --> for future stop/restart
   needs (§4.C).

Two architectural divergences are intentional and must stay:

1. We encode IEC subframes in EL2 — Newton has no userspace
   alsa-lib (§4.D).
2. We don't toggle `MAI_CTL.ENABLE` between clips — the panel
   re-negotiates audio capability on toggle and reboots (§3.4).

One workaround is brittle:

1. `CTS` is hard-coded at 44.1 kHz because the firmware mailbox
   reports a different pixel rate than the active HDMI block uses
   (§4.A). Reading the live block's pixel-clock counter would let us
   match Linux's runtime formula and drop the constant.
