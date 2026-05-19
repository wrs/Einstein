# Linux VC4 HDMI Audio Driver — BCM2835/BCM2710/BCM2837 (Pi Zero 2 W)

Survey captured 2026-05-20 from `raspberrypi/linux@rpi-6.6.y`. Source
references in this document use the line numbers from that branch:

- `drivers/gpu/drm/vc4/vc4_hdmi.c` (4114 lines)
- `drivers/gpu/drm/vc4/vc4_hdmi.h`
- `drivers/gpu/drm/vc4/vc4_hdmi_regs.h`
- `drivers/gpu/drm/vc4/vc4_regs.h`
- `drivers/gpu/drm/vc4/vc4_hdmi_phy.c`

The Pi Zero 2 W uses the same VideoCore IV / VC4 HDMI block as the Pi 3
and Pi 2 — `vc4_hdmi.c:3955` picks the `bcm2835_variant`, which is the
"gen3" (`VC4_GEN < VC4_GEN_5`) path everywhere `gen` is checked.

## 1. Audio path overview — ALSA → MAI → HDMI

The driver does not implement an ASoC platform-driver "trigger" itself.
It registers as the CPU DAI plus a `hdmi-codec` instance and lets ALSA's
`snd-dmaengine-pcm` shuttle bytes from the userspace ring buffer into a
slave-DMA channel whose destination address is the MMIO-mapped
`HDMI_MAI_DATA` register.

Bringup chain (`vc4_hdmi.c:2830-2965 vc4_hdmi_audio_init`):

- `devm_snd_dmaengine_pcm_register(dev, &pcm_conf, 0)` registers the
  generic ALSA PCM/dmaengine front-end, with `pcm_conf` (line 2760):
  ```c
  static const struct snd_dmaengine_pcm_config pcm_conf = {
      .chan_names[SNDRV_PCM_STREAM_PLAYBACK] = "audio-rx",
      .prepare_slave_config = snd_dmaengine_pcm_prepare_slave_config,
  };
  ```
  At runtime ALSA calls `dma_request_chan(dev, "audio-rx")` which goes
  to the DT and resolves the `dmas`/`dma-names` property; on BCM2835
  this binds to one of the bcm2835-dma channels. The slave config drives
  `dmaengine_prep_dma_cyclic`, set up by
  `snd_dmaengine_pcm_prepare_slave_config` from the ALSA period/buffer
  parameters.
- The CPU DAI is `vc4-hdmi-cpu-dai`, declared at `vc4_hdmi.c:2745`:
  ```c
  .playback = { .channels_min = 1, .channels_max = 8,
                .rates = 32000|44100|48000|88200|96000|176400|192000,
                .formats = SNDRV_PCM_FMTBIT_IEC958_SUBFRAME_LE };
  ```
  `vc4_hdmi_audio_cpu_dai_probe` (2732) does
  `snd_soc_dai_init_dma_data(dai, &audio.dma_data, NULL)` — the DMA
  slave address is the only piece the CPU DAI publishes.
- The codec side is `hdmi-codec`
  (`sound/soc/codecs/hdmi-codec.c`). The codec instance is registered
  with `vc4_hdmi_codec_ops` (line 2792), which has only `prepare`,
  `audio_startup`, `audio_shutdown`, `get_eld`, `hook_plugged_cb`. There
  is **no `audio_trigger`** in this driver. hdmi-codec.c itself
  implements `SNDRV_PCM_TRIGGER_START/STOP` via the dmaengine PCM
  (cyclic DMA simply starts running) — the VC4 driver doesn't see PCM
  triggers directly.

DMA flow on Pi Zero 2 W: bcm2835-dma channel pulls 32-bit words from a
cyclic ring in CMA memory and pushes them, with burst = 2 (8 bytes), to
the physical address of `HDMI_MAI_DATA`. The MAI FIFO drains samples
into the HDMI controller, which packs them into Audio Sample Packets
between video blanking intervals along with the audio infoframe and the
N/CTS regeneration packet.

The DMA slave address is captured in `vc4_hdmi_audio_init` (2885):

```c
vc4_hdmi->audio.dma_data.addr = iomem->start + mai_data->offset;
vc4_hdmi->audio.dma_data.addr_width = DMA_SLAVE_BUSWIDTH_4_BYTES;
vc4_hdmi->audio.dma_data.maxburst = 2;
```

`iomem` is the second `reg` range from DT (the "HD" block) and
`mai_data->offset` is `0x0020` on BCM2835
(`VC4_HD_REG(HDMI_MAI_DATA, 0x0020)` at `vc4_hdmi_regs.h:187`).

## 2. MAI bringup — `vc4_hdmi_audio_startup` and `vc4_hdmi_audio_prepare`

### `vc4_hdmi_audio_startup` (`vc4_hdmi.c:2482`, runs at PCM open)

The startup just resets the MAI FIFO and turns on the PHY noise
generator. Single MMIO write to `HDMI_MAI_CTL`:

```c
HDMI_WRITE(HDMI_MAI_CTL,
    VC4_HD_MAI_CTL_RESET |    // BIT(0)
    VC4_HD_MAI_CTL_FLUSH |    // BIT(9)
    VC4_HD_MAI_CTL_DLATE |    // BIT(15)
    VC4_HD_MAI_CTL_ERRORE |   // BIT(2)
    VC4_HD_MAI_CTL_ERRORF);   // BIT(1)
```

Effective value: `0x0000_8207` (BIT15|BIT9|BIT2|BIT1|BIT0). Then
`vc4_hdmi->variant->phy_rng_enable(vc4_hdmi)` is called (only present on
the BCM2835 variant for the gen3 path; vc5 has its own).

### `vc4_hdmi_audio_prepare` (`vc4_hdmi.c:2619`, runs at hw_params/prepare)

This is where the format-dependent register programming happens.
Argument: a `hdmi_codec_params` carrying `sample_rate`, `channels`,
`sample_width`, and the IEC958 channel-status bytes that came from
userspace.

Step 1 — set the MAI sample-rate divider (described in §3 below):

```c
vc4_hdmi_audio_set_mai_clock(vc4_hdmi, sample_rate);   // writes HDMI_MAI_SMP
```

Step 2 — enable the MAI block with channel count (2654):

```c
HDMI_WRITE(HDMI_MAI_CTL,
    VC4_SET_FIELD(channels, VC4_HD_MAI_CTL_CHNUM) |   // bits 7:4
    VC4_HD_MAI_CTL_WHOLSMP |                          // BIT(12)
    VC4_HD_MAI_CTL_CHALIGN |                          // BIT(13)
    VC4_HD_MAI_CTL_ENABLE);                           // BIT(3)
```

For stereo: `channels=2`, so value = `0x0000_3028`.

Step 3 — write MAI_FMT (2661-2671):

```c
mai_sample_rate = sample_rate_to_mai_fmt(sample_rate);   // 48000 → 9
if (iec958.status[0] & IEC958_AES0_NONAUDIO && channels == 8)
    mai_audio_format = VC4_HDMI_MAI_FORMAT_HBR;   // 200
else
    mai_audio_format = VC4_HDMI_MAI_FORMAT_PCM;   // 2
HDMI_WRITE(HDMI_MAI_FMT,
    VC4_SET_FIELD(mai_sample_rate, VC4_HDMI_MAI_FORMAT_SAMPLE_RATE) |  // bits 15:8
    VC4_SET_FIELD(mai_audio_format, VC4_HDMI_MAI_FORMAT_AUDIO_FORMAT)); // bits 23:16
```

For 48 kHz PCM: `mai_sample_rate=9, mai_audio_format=2` → `0x0002_0900`.
Sample-rate codes enum at `vc4_regs.h:830-846` (8 kHz=1 .. 192 kHz=15).

Step 4 — build `audio_packet_config` (2673-2681):

```c
audio_packet_config =
    VC4_HDMI_AUDIO_PACKET_ZERO_DATA_ON_SAMPLE_FLAT |          // BIT(29)
    VC4_HDMI_AUDIO_PACKET_ZERO_DATA_ON_INACTIVE_CHANNELS |    // BIT(24)
    VC4_SET_FIELD(0x8, VC4_HDMI_AUDIO_PACKET_B_FRAME_IDENTIFIER); // bits 13:10 = 8
channel_mask = GENMASK(channels - 1, 0);                       // stereo: 0x03
audio_packet_config |= VC4_SET_FIELD(channel_mask, VC4_HDMI_AUDIO_PACKET_CEA_MASK);
```

For stereo: `0x2100_2003`.

Step 5 — MAI_THR (2683-2701), threshold/DREQ:

```c
// gen3 / BCM2835 / Pi Zero 2 W path:
HDMI_WRITE(HDMI_MAI_THR,
    VC4_SET_FIELD(0x8, VC4_HD_MAI_THR_PANICHIGH) |   // bits 29:24
    VC4_SET_FIELD(0x8, VC4_HD_MAI_THR_PANICLOW)  |   // bits 21:16
    VC4_SET_FIELD(0x6, VC4_HD_MAI_THR_DREQHIGH)  |   // bits 13:8
    VC4_SET_FIELD(0x8, VC4_HD_MAI_THR_DREQLOW));     // bits  5:0
```

Bit-field encoding from `vc4_regs.h:1033-1040`. For BCM2835 the value is
`(8<<24)|(8<<16)|(6<<8)|8 = 0x0808_0608`. gen5 (BCM2711) uses
`0x10, 0x10, 0x1c, 0x1c` in the same field layout; gen5 step-D0 uses the
wider `VC4_D0_*` field shifts (7-bit fields).

Step 6 — MAI_CONFIG (2703-2706):

```c
HDMI_WRITE(HDMI_MAI_CONFIG,
    VC4_HDMI_MAI_CONFIG_BIT_REVERSE |       // BIT(26)
    VC4_HDMI_MAI_CONFIG_FORMAT_REVERSE |    // BIT(27)
    VC4_SET_FIELD(channel_mask, VC4_HDMI_MAI_CHANNEL_MASK));
```

For stereo: `0x0C00_0003`.

Step 7 — channel map and audio packet config (2708-2710):

```c
channel_map = vc4_hdmi->variant->channel_map(vc4_hdmi, channel_mask);
HDMI_WRITE(HDMI_MAI_CHANNEL_MAP, channel_map);
HDMI_WRITE(HDMI_AUDIO_PACKET_CONFIG, audio_packet_config);
```

`vc4_hdmi_channel_map` (gen3, line 2359) uses a 3-bit-per-channel slot
stride: for stereo (mask 0x03) → bit 0 → slot 0 contributes `0<<0=0`,
bit 1 → slot 1 contributes `1<<3=8`, so `channel_map = 0x08`. (vc5 uses
4-bit stride.)

Step 8 — N/CTS (described in §4) via
`vc4_hdmi_set_n_cts(vc4_hdmi, sample_rate)`.

Step 9 — audio infoframe (described in §6):

```c
memcpy(&vc4_hdmi->audio.infoframe, &params->cea, sizeof(params->cea));
vc4_hdmi_set_audio_infoframe(encoder);
```

## 3. MAI sample-rate clock — `vc4_hdmi_audio_set_mai_clock` (`vc4_hdmi.c:2403`)

```c
hsm_clock = clk_get_rate(vc4_hdmi->audio_clock);
rational_best_approximation(hsm_clock, samplerate,
    VC4_HD_MAI_SMP_N_MASK >> VC4_HD_MAI_SMP_N_SHIFT,        // (1<<24) - 1
    (VC4_HD_MAI_SMP_M_MASK >> VC4_HD_MAI_SMP_M_SHIFT) + 1,  //  1<<8
    &n, &m);
HDMI_WRITE(HDMI_MAI_SMP,
    VC4_SET_FIELD(n, VC4_HD_MAI_SMP_N) |    // bits 31:8 (24 bits)
    VC4_SET_FIELD(m - 1, VC4_HD_MAI_SMP_M));// bits  7:0 (8 bits, "minus one")
```

`audio_clock` is the HSM clock fed to the audio block (BCM2835 DT
assigns it to one of the HSM clock outputs). `rational_best_approximation`
(`lib/math/rational.c`) finds rational n/m ≤ the max-field widths such
that `(hsm_clock × m) / n ≈ samplerate`. The MAI block divides
hsm_clock down by m to produce the sample-rate clock; n is the
numerator and m-1 is what gets written (so writing 0 means dividing by 1).

Field masks (`vc4_regs.h:1054-1057`):
- N: bits 31:8 (24-bit numerator)
- M: bits 7:0 (8-bit denominator, written as `m-1`)

## 4. N/CTS — `vc4_hdmi_set_n_cts` (`vc4_hdmi.c:2432`)

Linux does **not** use the HDMI spec's hard-coded N tables. It computes
both N and CTS at runtime:

```c
n = 128 * samplerate / 1000;
tmp = (u64)(mode->clock * 1000) * n;
do_div(tmp, 128 * samplerate);
cts = tmp;

HDMI_WRITE(HDMI_CRP_CFG,
    VC4_HDMI_CRP_CFG_EXTERNAL_CTS_EN |          // BIT(24)
    VC4_SET_FIELD(n, VC4_HDMI_CRP_CFG_N));      // bits 19:0

HDMI_WRITE(HDMI_CTS_0, cts);
HDMI_WRITE(HDMI_CTS_1, cts);
```

For 48 kHz: `n = 128*48000/1000 = 6144` — matches HDMI spec's
recommended N for 48 kHz. For 44.1 kHz: `128 * 44100 / 1000 = 5644` —
slightly off from the spec's 6272, but Linux trusts the formula.

`mode->clock` is the TMDS pixel clock in kHz from the chosen DRM mode.
With `EXTERNAL_CTS_EN`, the hardware uses the manually-programmed
CTS_0/CTS_1 rather than measuring it from the TMDS-vs-MAI clock
divergence; both CTS_0 and CTS_1 are written to the same value (the
driver comment at 2452-2454 notes that "we could get slightly more
accurate clocks by providing a CTS_1 value. The two CTS values are
alternated based on the period fields" — i.e., dithering CTS for
non-integer ratios is a future enhancement, not done today).

`VC4_HDMI_CRP_CFG_EXTERNAL_CTS_EN = BIT(24)` (`vc4_regs.h:859`),
`VC4_HDMI_CRP_CFG_N` occupies bits 19:0.

## 5. DMA setup — cyclic, audio-rx, burst 2

Recap of §1:

- Channel name: `"audio-rx"` (the "rx" naming is historic — it's the VC
  RX from the CPU's POV, not the audio direction).
- `addr_width = 4 bytes`, `maxburst = 2` → AXI transfer length 2 beats
  × 4 bytes = 8 bytes per DMA cycle.
- Cyclic mode is selected by `snd_dmaengine_pcm` based on the ALSA
  stream type (playback PCM uses `dmaengine_prep_dma_cyclic`).
- Period/buffer sizes come from userspace (`hw_params`); the driver
  doesn't constrain them in the DAI definition besides rate/format/
  channels.
- Slave config (built by `snd_dmaengine_pcm_prepare_slave_config`)
  writes `dst_addr = MAI_DATA phys`, `direction = DMA_MEM_TO_DEV`, with
  the 4-byte width and burst-2 we set above.
- DMA starts at `SNDRV_PCM_TRIGGER_START` handled inside the dmaengine
  PCM layer — VC4 driver gets no callback for this.

## 6. Audio InfoFrame — `vc4_hdmi_write_infoframe` (`vc4_hdmi.c:868`)

`vc4_hdmi_set_audio_infoframe` (2716) copies the codec-supplied
`params->cea` into `vc4_hdmi->audio.infoframe` and calls
`vc4_hdmi_write_infoframe` if the packet RAM has been enabled
(`packet_ram_enabled` — set at `vc4_hdmi.c:1975` when transitioning to
HDMI mode).

The packet ID is derived from the HDMI type byte:
`packet_id = frame->any.type - 0x80`. For `HDMI_INFOFRAME_TYPE_AUDIO`
(0x84) → packet_id = 4. There are 8 slots in the packet RAM (one per
BIT in RAM_PACKET_CONFIG).

```c
len = hdmi_infoframe_pack(frame, buffer, sizeof(buffer));   // common HDMI packer
vc4_hdmi_stop_packet(encoder, frame->any.type, true);       // disables slot, polls
```

`vc4_hdmi_stop_packet` (840) clears the per-slot bit in
`HDMI_RAM_PACKET_CONFIG` and polls (with `poll=true`) on the
corresponding bit in `HDMI_RAM_PACKET_STATUS` going low (100 ms
timeout).

Then the actual packing into packet RAM (lines 904-919):

```c
for (i = 0; i < len; i += 7) {
    writel(buffer[i+0] | buffer[i+1] << 8 | buffer[i+2] << 16,
           base + packet_reg);
    packet_reg += 4;
    writel(buffer[i+3] | buffer[i+4] << 8 | buffer[i+5] << 16 | buffer[i+6] << 24,
           base + packet_reg);
    packet_reg += 4;
}
```

The HDMI packet RAM is laid out as 8-byte sub-blocks: word 0 holds 3
payload bytes, word 1 holds 4 payload bytes, then the next 8-byte block.
Linux iterates 7 payload bytes at a time. `VC4_HDMI_PACKET_STRIDE = 0x24`
(36 bytes) per slot (`vc4_hdmi_regs.h:8`) — large enough for header +
28 byte payload. Remainder bytes are zeroed (922-926):

```c
for (; packet_reg < packet_reg_next; packet_reg += 4)
    writel(0, base + packet_reg);   // avoid checksum errors on analysers
```

Re-enable the slot:

```c
HDMI_WRITE(HDMI_RAM_PACKET_CONFIG,
    HDMI_READ(HDMI_RAM_PACKET_CONFIG) | BIT(packet_id));
```

Poll until `HDMI_RAM_PACKET_STATUS` shows the slot active (line 933):

```c
ret = wait_for((HDMI_READ(HDMI_RAM_PACKET_STATUS) & BIT(packet_id)), 100);
```

On BCM2835 the register offsets (`vc4_hdmi_regs.h:208-209, 242`):

- `HDMI_RAM_PACKET_CONFIG = 0x00a0`
- `HDMI_RAM_PACKET_STATUS = 0x00a4`
- `HDMI_RAM_PACKET_START  = 0x0400` (slot 0 base; slot n at
  `0x400 + 0x24*n`).

So the audio infoframe (slot 4) writes to `0x0400 + 4*0x24 = 0x0490`
for 9 words (36 bytes).

The global RAM-packet enable bit (BIT(16) of RAM_PACKET_CONFIG,
`VC4_HDMI_RAM_PACKET_ENABLE`) is set once when the encoder transitions
to HDMI mode (`vc4_hdmi.c:1975-1976`):

```c
HDMI_WRITE(HDMI_RAM_PACKET_CONFIG, VC4_HDMI_RAM_PACKET_ENABLE);
vc4_hdmi->packet_ram_enabled = true;
```

## 7. Trigger START/STOP

There's **no vc4-specific PCM trigger**. The trigger sequence is
implemented inside `sound/soc/codecs/hdmi-codec.c` and
`snd-dmaengine-pcm`:

- `SNDRV_PCM_TRIGGER_START`: snd-dmaengine-pcm calls
  `dmaengine_terminate_async` cleanup if needed, prepares the cyclic
  descriptor (already done at hw_params), then `dma_async_issue_pending`
  — DMA hardware starts pushing words to `HDMI_MAI_DATA`. The MAI block
  was already enabled by `vc4_hdmi_audio_prepare` writing
  `VC4_HD_MAI_CTL_ENABLE` to MAI_CTL.
- `SNDRV_PCM_TRIGGER_STOP`: dmaengine PCM calls
  `dmaengine_terminate_async` to halt the DMA; the VC4 MAI block stays
  enabled. When ALSA later closes the stream, hdmi-codec calls
  `audio_shutdown`.

`vc4_hdmi_audio_shutdown` (`vc4_hdmi.c:2547`):

```c
HDMI_WRITE(HDMI_MAI_CTL,
    VC4_HD_MAI_CTL_DLATE |   // BIT(15)
    VC4_HD_MAI_CTL_ERRORE |  // BIT(2)
    VC4_HD_MAI_CTL_ERRORF);  // BIT(1)
// Value: 0x0000_8006 — note: VC4_HD_MAI_CTL_ENABLE is *not* set.
phy_rng_disable();
vc4_hdmi_audio_reset(vc4_hdmi);
```

`vc4_hdmi_audio_reset` (2524):

```c
vc4_hdmi_stop_packet(encoder, HDMI_INFOFRAME_TYPE_AUDIO, false);
HDMI_WRITE(HDMI_MAI_CTL, VC4_HD_MAI_CTL_RESET);   // bit 0 only
HDMI_WRITE(HDMI_MAI_CTL, VC4_HD_MAI_CTL_ERRORF);  // bit 1 only — clear underflow err
HDMI_WRITE(HDMI_MAI_CTL, VC4_HD_MAI_CTL_FLUSH);   // bit 9 only — drain FIFO
```

Three single-bit pulses, not OR'd — each bit is asserted alone.

## 8. IEC 60958 / SPDIF — raw IEC subframes from userspace

The DAI declares `formats = SNDRV_PCM_FMTBIT_IEC958_SUBFRAME_LE` (line
2756). That format is **pre-encoded IEC 60958 subframes** — userspace
(or alsa-lib's iec958 plug) hands the driver fully formatted 32-bit IEC
subframes: preamble + 24 audio bits + V/U/C/P bits. The hardware does
*not* synthesise IEC subframes from raw PCM; it just streams these
32-bit words into the HDMI Audio Sample Packets.

That's why the channel-status bytes (`params->iec.status[0]`) are passed
into prepare — at line 2662 the driver checks
`params->iec.status[0] & IEC958_AES0_NONAUDIO` and switches MAI_FMT
audio_format from PCM (2) to HBR (200) for 8-channel non-audio. The
hardware respects the IEC channel-status bits for sample rate /
non-audio that travel inside the subframes.

The `B_FRAME_IDENTIFIER = 0x8` written in `audio_packet_config` (line
2677) is the IEC frame "B" block marker — every 192 frames userspace
sets the preamble to "B" and the value 8 here tells the hardware which
mask value to use.

## 9. PHY RNG enable — `vc4_hdmi_phy_rng_enable` (`vc4_hdmi_phy.c:199`)

Called from `vc4_hdmi_audio_startup` (line 2513-2514) via the variant
pointer. For BCM2835:

```c
void vc4_hdmi_phy_rng_enable(struct vc4_hdmi *vc4_hdmi)
{
    unsigned long flags;
    spin_lock_irqsave(&vc4_hdmi->hw_lock, flags);
    HDMI_WRITE(HDMI_TX_PHY_CTL_0,
               HDMI_READ(HDMI_TX_PHY_CTL_0) &
               ~VC4_HDMI_TX_PHY_RNG_PWRDN);   // clear BIT(25)
    spin_unlock_irqrestore(&vc4_hdmi->hw_lock, flags);
}
```

`VC4_HDMI_TX_PHY_RNG_PWRDN = BIT(25)` (`vc4_regs.h:995`). Power-down bit
for the PHY's random-noise generator. *Cleared* to enable RNG.
`vc4_hdmi_phy_rng_disable` (line 210) sets it back.

VC5/VC6 path has the equivalent in `HDMI_TX_PHY_POWERDOWN_CTL` bit 4 —
different register, same idea.

## 10. MAI_THR threshold values

From `vc4_hdmi.c:2683-2701`:

- **Gen3 (BCM2835/BCM2710/BCM2837 — Pi Zero 2 W)**:
  `panic_high=0x08, panic_low=0x08, dreq_high=0x06, dreq_low=0x08`
  packed via 6-bit field shifts (`VC4_HD_MAI_THR_*` from
  `vc4_regs.h:1033`). Raw value: `0x0808_0608`.
- **Gen5 (BCM2711, non-step-D0)**: `0x10, 0x10, 0x1c, 0x1c` packed via
  the same 6-bit field shifts. Raw: `0x1010_1c1c`.
- **Gen5 step-D0 (BCM2711 D0 silicon)**: same numeric values
  `0x10, 0x10, 0x1c, 0x1c` but packed with the wider
  `VC4_D0_HD_MAI_THR_*` 7-bit fields. Raw: `0x0808_8E1C`.

No gen4 — Linux uses `vc4->gen >= VC4_GEN_5` as the threshold check.

## Side notes for the comparison

- `vc4->gen` for the Pi Zero 2 W is VC4_GEN_3 (or older "vc4" — the
  gen3 path); the practical check is `vc4->gen < VC4_GEN_5`.
- Register offsets for BCM2835 HDMI block (the "non-HD" part):
  - `HDMI_MAI_CHANNEL_MAP = 0x090`, `HDMI_MAI_CONFIG = 0x094`,
    `HDMI_MAI_FORMAT = 0x098`
  - `HDMI_AUDIO_PACKET_CONFIG = 0x09c`
  - `HDMI_RAM_PACKET_CONFIG = 0x0a0`,
    `HDMI_RAM_PACKET_STATUS = 0x0a4`
  - `HDMI_CRP_CFG = 0x0a8`, `HDMI_CTS_0 = 0x0ac`, `HDMI_CTS_1 = 0x0b0`
  - `HDMI_RAM_PACKET_START = 0x400`, packet stride `0x24`
- HD block (BCM2835):
  - `HDMI_MAI_CTL = 0x014`, `HDMI_MAI_THR = 0x018`,
    `HDMI_MAI_FMT = 0x01c`, `HDMI_MAI_DATA = 0x020`,
    `HDMI_MAI_SMP = 0x02c`
- The driver runs **no** explicit "write user data" operation — IEC
  subframes carry their own C/U/V bits.
- The "iec958 codec" channel-status bytes come from the userspace ALSA
  control "IEC958 Playback Default" handled inside hdmi-codec.c.
