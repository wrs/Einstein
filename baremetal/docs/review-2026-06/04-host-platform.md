# Newton Hypervisor — platform/peripheral subsystem review

> Review agent report, 2026-06-11, at working copy `somv 8b564c93`.
> Scope: `src/platform/`, `src/sd/`, `src/usb/`, `src/input/`, `src/audio/`,
> `src/host_io/`, `src/display/fb.rs`, `src/flash_persist/`, `src/mailbox.rs`,
> `src/uart.rs`, `src/timer.rs`, `src/main.rs`, `boot.s`, `vectors.s`,
> `build.rs`, `Cargo.toml`, both linker scripts. All files read in full; two
> suspected feature-matrix holes verified by `cargo check`.

## High

### H1. Mailbox DMA buffer is not cache-line aligned/padded — post-response `dc civac` can corrupt the firmware's reply
`src/mailbox.rs:157-166`, `src/cpu.rs:115-122`

`Buffer` is `#[repr(C, align(16))]` and lives on the stack. The protocol is: clean the buffer, ring the doorbell, the VC writes the response into RAM through the uncached alias, then `dc_civac_range` again before reading (`mailbox.rs:183,211`). Because the buffer is only 16-byte aligned, its first/last 64-byte cache lines can be shared with adjacent stack data (the callee frame of `mailbox_call` sits directly below `buf`). If the CPU dirties such a shared line between the doorbell and the post-response maintenance (frame saves, spilled poll-loop temporaries), the post-response **CIVAC writes the dirty line back over the VC's freshly-written response bytes** before invalidating — intermittent response corruption. The comment on `dc_civac_range` ("adjacent data … then the clean is a no-op") is only true for the outbound direction; it is wrong for buffers a device writes into. The same reasoning that forced `#[repr(align(64))]` on `MaiTxRing` and the UART `Ring` applies here. Fix: `#[repr(C, align(64))] struct Buffer { words: [u32; 64] }` (size is already a 64-byte multiple), and correct the `dc_civac_range` doc to say inbound DMA buffers must be line-aligned and line-padded.

## Medium

### M1. Feature matrix has verified non-building combinations; cross-axis constraints aren't enforced in build.rs
`build.rs:249-344`, `Cargo.toml:38-258`

build.rs rigorously enforces the platform axis (panics on 0 or 2 platforms) but enforces nothing across axes. Two combinations confirmed broken:

- `cargo check --release --features sd-probe` (i.e. sd-probe without `no-semihost`) → **E0599**: `src/sd/probe.rs:99` calls `write_block_dma`, which is gated `#[cfg(all(feature = "no-semihost", feature = "platform-raspi3b"))]` (`src/sd/sdhost.rs:390`).
- `--no-default-features --features "platform-raspi3b no-semihost flash-persist-sd input-mtouch"` (mtouch without `host-io-pi-fb`) → **E0432**: `src/input/calibrate.rs:13` unconditionally imports `crate::host_io::pi_fb`, which only exists under `nh_host_io_pi_fb`.

Symmetric, unverified-but-structural holes: `host-io-pi-fb`, `flash-persist-sd`, or `audio-pi-hdmi` combined with `platform-fvp-base` cannot build, because `mod display`, `mod sd`, and `mod mailbox` are all gated `#[cfg(feature = "platform-raspi3b")]` (`src/main.rs:11,22,31`) while the backends reference them unconditionally. The aggregates (`pi-bare-metal-*`) paper over this, but the prompt-level question "does every feature combination build?" is currently *no*. Fix: add a `validate_feature_matrix()` step in build.rs that panics with actionable messages (`input-mtouch requires host-io-pi-fb`, `sd-probe requires no-semihost`, `host-io-pi-fb/flash-persist-sd/audio-pi-hdmi require platform-raspi3b`) — same pattern as `select_platform_linker_script`.

### M2. `read_block` / `write_block` error paths don't restore SDHCFG
`src/sd/sdhost.rs:341, 362`

In `read_block`, `prepare_data` sets `SDHCFG = hcfg_base | DATA_IRPT_EN`, then `resp?;` at line 341 early-returns on a CMD17 failure **before** the `write_reg(SDHCFG, self.hcfg_base)` at line 350. `write_block` has the same shape at line 362. The DMA variants (`write_block_dma`, `write_sectors_dma`, `start_sectors_dma`) all carefully restore SDHCFG on their error paths, so this is both a leak and an inconsistency: after a failed read, `DATA_IRPT_EN` stays set across subsequent non-data commands. Given the file's own doc that this bit "gates the FSM's data-movement path", a stale value across an error→retry sequence is a plausible source of hard-to-reproduce CRC/FSM weirdness. Fix: hoist the restore into a small RAII guard or restructure both functions to flow through a single exit that writes `hcfg_base`.

### M3. `pi_hdmi` stereo ring mutates through a shared reference without `UnsafeCell`
`src/audio/pi_hdmi.rs:441-461, 795-801, 914-917`

`RingState.frames` is a plain array; `ring_state()` hands out `&'static RingState`, and `schedule_output` writes frames via `ring.frames.as_ptr().add(slot) as *mut StereoFrame` (line 796-800). Writing through a pointer derived from a shared reference to non-`UnsafeCell` data is undefined behavior under Rust's aliasing rules, regardless of the single-core argument. The file already does this correctly twenty lines later for `MAI_TX_RING` (`UnsafeCell<[u32; …]>` + `Sync` wrapper), and `mtouch.rs`/`sd.rs` use the same `StateCell` pattern. Fix: wrap `frames` in `UnsafeCell` (head/tail stay atomics) and route writes through `.get()` — mechanical, no behavior change, removes the one aliasing-UB instance in the audio path.

### M4. CTS is hard-coded to one panel's pixel clock; the "refuse to come up" pixel-clock gate now protects an unused value
`src/audio/pi_hdmi.rs:1614-1664`

`bringup_mai` refuses to initialize audio when the mailbox pixel-clock query fails ("we refuse to fabricate a value", lines 1615-1627), but the queried value is then discarded (`let _ = pixel_clock_hz;` line 1664) and CTS is computed from `const PANEL_PIXEL_CLOCK_HZ: u64 = 51_200_000` — an empirically back-solved constant for the one shipped touchscreen panel. On any other monitor/mode the regenerated audio clock will be wrong (the "crunchy" symptom the module's own header warns about), while the boot log misleadingly prints the measured pixel clock next to the CTS as if they were related. For stabilization: either (a) derive CTS from the mailbox value when it's sane and fall back to the panel constant only for the known-bad 85.5 MHz reading, or at minimum (b) demote the unused query (stop failing bring-up on it) and rename/log the constant as a panel-specific override so the limitation is visible.

### M5. Audio driver still carries its bring-up bisection scaffolding
`src/audio/pi_hdmi.rs:317-417, 1446-1496, 1820-1824`

For a stabilization pass, `pi_hdmi.rs` retains a full diagnostic matrix that is now constant-folded to one configuration: `TONE_TEST_48_KHZ`, the five-mode `IEC_DIAGNOSTIC_MODE` machinery (`IEC_MODE_SUPPRESS_ALL` … `IEC_MODE_ALSA_B_AND_ALL_CS`, plus `iec_b_frame_preamble`/`use_alsa_iec_preambles` const-fns and the dead Circle-preamble branch in `encode_iec958_pair`), `ENABLE_MAI_AFTER_INFOFRAME`, `USE_MAI_CTL_PAREN`, `FORCE_AUDIO_SAMPLE_PRESENT`, `FORCE_AUDIO_B_FRAME`, `SKIP_AUDIO_INFOFRAME` (line 1820, with a comment narrating a since-finished experiment), and three `#[allow(dead_code)]` `mai_ctl_*` helpers kept "for symmetry". This is exactly the leftover-probe category the recent commits pruned elsewhere (dead USB code, Phase-B scaffolding). Suggest: collapse to the shipped configuration (ALSA preambles + full CS bytes + Linux gen3 thresholds), delete the dead branches, and keep one paragraph of prose recording *why* the alternatives lost (the hard-won knowledge is the comment content, not the dead `if`s).

### M6. Contradictory / stale documentation cluster around the FVP EL3 story and SD driver state
Several comments now contradict the code or each other; in a codebase this comment-dense, these actively mislead:

- `src/platform/fvp_base.rs:4-5` says "has_el3=0" while lines 77-80 in the same file say "has_el3=1 (our chosen config — see … the EL3 stub in boot.s)". `src/platform/gicv3.rs:1-9` is written entirely from the has_el3=0 premise ("there is no secure firmware"), yet `gicv3.rs:365-367` describes the boot.s EL3 stub clearing ProcessorSleep from Secure. boot.s (lines 6-11, 35-44) is the ground truth; the fvp_base/gicv3 headers should be rewritten against it.
- `src/timer.rs:13`: "fvp-base — GICv3 (TODO; currently a no-op — see platform::fvp_base)" — implemented long since.
- `src/sd/sdhost.rs:124-125`: "Untested on real hardware. See the module-level 'Bring-up status' note" — the note no longer exists and the driver is the production path on the Pi Zero 2 W.
- `src/sd/sdhost.rs:676-680`: claims "We don't currently issue any [R1b] … the only one we send is CMD7" — the DMA paths issue CMD12 (`write_sectors_dma`, `finish_sectors_dma`), and the very next line special-cases `CMD_STOP_TRANSMISSION`.
- `Cargo.toml:261-270`: the `trace` feature doc describes the retired UDF-`#index`/first-touch mechanism; the current implementation is the 5-word trampoline, every-call design (per `src/tracer.rs` and CLAUDE.md). `build.rs:6` similarly claims the tables are built "When the `trace` cargo feature is on", but `build_trace_tables()` runs unconditionally (`build.rs:96`).
- `src/audio/pi_hdmi.rs:1841-1845`: `set_audio_info_frame` doc says "Called from `start_output` on each StartOutput … the slot needs re-arming each time"; it is called exactly once from `bringup_mai` (line 1832), and `start_output`'s comments explain why it must *not* re-arm.
- `src/flash_persist/mod.rs:81`: `#[cfg_attr(feature = "no-semihost", allow(dead_code))]` on `maybe_save` is stale — it *is* live under `no-semihost` via `snapshot::maybe_flash_autosave` (`src/snapshot.rs:338`).

### M7. `all(feature = "no-semihost", feature = "platform-raspi3b")` is a repeated proxy for "real hardware with host_dma"
`src/sd/sdhost.rs:372,390,426,462,495,521`, `src/input/mod.rs:82`, `src/audio/mod.rs:153`, `src/flash_persist/mod.rs:94`, `src/uart.rs:42,100,254,534`

"`no-semihost` AND raspi3b" semantically means "the BCM2835 DMA engine and real peripherals exist", but that's an inference, not a declaration — and it's spelled out in ~10 places. It's also why M1's `sd-probe` hole exists (the probe assumes the DMA half of the driver is present, but the gate is keyed off an unrelated feature). Since build.rs already owns backend resolution, it could emit a single `cfg(nh_real_hw)` (or `nh_host_dma`) and the source could consume that. One definition, one place to change when (say) a Pi 5 platform appears, and the cross-axis validation from M1 falls out naturally.

## Low

### L1. `host-io-pico` resolves to a cfg with no implementation — silently builds a do-nothing backend
`build.rs:267`, `src/host_io/mod.rs:27-33`. `flash-persist-pico` is explicitly mapped onto the null backend (`src/flash_persist/mod.rs:29-30,60-61`), but `nh_host_io_pico` matches no module and no dispatch arm in `host_io::init/push_blit/pump_input` — the cfg-dispatch functions just compile to empty bodies. Either map it to null explicitly like flash-persist does, or make the resolver panic ("host-io-pico is reserved, not implemented").

### L2. Mailbox response timeout is reported as `FirmwareError`, not `Timeout`
`src/mailbox.rs:200-216`. The read-poll loop can exhaust its 10M iterations without a channel-8 reply; the code falls through, invalidates, and then fails the `words[1] != RESPONSE_SUCCESS` check — masking a wedged VC as a firmware NAK. Detect loop exhaustion and return `MailboxError::Timeout`. (Also: `BUS_UNCACHED` doc at line 92-94 says "ANDing this in"; it's OR'd.)

### L3. `delay_us` runs ~20× fast on real silicon
`src/sd/sdhost.rs:981-990`. A `nop` loop at 50 iterations/µs on a ~1 GHz A53 yields ≈50 ns per nominal µs, so the "10 ms" power-up/reset settles are really ~0.5 ms. The comment admits it's a placeholder, and the hardware demonstrably tolerates it, but `cpu::delay_ms` (CNTPCT-based, `src/cpu.rs:153`) already exists — a CNTPCT-based `delay_us` would make the named delays true and remove a latent marginal-card risk. Relatedly, Linux's bcm2835-sdhost exempts CRC7 errors on `SEND_OP_COND` (R3 carries a 0xFF CRC field); `send_cmd_kind` maps any `SDHSTS_CRC7_ERROR` to a hard error — worth a one-time cross-check against the Linux source before calling init "stable".

### L4. `num_blocks` reports `u32::MAX`
`src/sd/block_device.rs:31-38`. Documented and deliberate (CSD undecoded), but it disables every whole-device bounds check in embedded-sdmmc — a corrupt MBR/FAT could direct raw block writes anywhere on the card. Decoding CSD (CMD9's response is already fetched and discarded, `sdhost.rs:230`) is cheap insurance for a subsystem whose write path is now DMA-driven.

### L5. mtouch attach failure messages conflate "wrong device" with "controller not ready"
`src/input/mtouch.rs:148-151, 159-166`. `attach` returns `UsbError::NotReady` for a VID/PID mismatch, and `init` prints "DWC2 not ready; pen input disabled" for that error — so plugging in the wrong USB device logs a controller fault (the ignoring-device line is also printed, but the summary line lies). A distinct error variant or message would save a future bring-up session some confusion.

### L6. 16 KiB EL2 stack with nested-IRQ frames and no guard
`linker.ld:33-35`, `linker-fvp.ld:38-40`, `src/cpu.rs:226`. `with_irqs_unmasked` allows one level of IRQ nesting on the same stack (256 B context + handler frames + `kprintln` formatting), and `pi_fb::push_blit`'s bilinear loop plus embedded-sdmmc frames run there too. The stack sits directly above `__bss_end` with no guard, so overflow silently corrupts whatever lands at the top of .bss. Cheap mitigations: a canary word checked in `trap_irq`, or placing the stack below the image with a faulting page.

## What looked solid

- DMA discipline elsewhere is consistent and correct: `arm_with_cs` flushes every CB before arming (`host_dma.rs:332-344`); `arm_sd_dma`, the MAI ring refill, the UART TX ring, and `pi_fb` all clean exactly the spans the engine will read; the MAI ring's one-period-ahead write fence plus the CONBLK_AD resync in `on_mai_dma_done` is a genuinely careful treatment of IRQ-coalescing drift.
- `gicv3.rs` ordering (wake RD → SRE_EL2 → distributor with RWP waits → EL1 interface → per-PPI config) is right, bounded-spins panic loudly, and the v3/v4 stride probe in the RD walk is correct.
- The SPSC pen queue's monotonic-cursor free-space math is correct (including the full-at-`QSIZE` case), and the semihost flash backend's swap-bitmap/remark-on-failure save protocol is sound, mirrored faithfully by the SD backend; the embedded-sdmmc seek+write-in-place semantics the incremental save depends on were verified in the vendored crate.
- `boot.s`'s EL3 stub and `vectors.s` (out-of-line resume tail to respect the 128-byte slot budget) are clean and well-justified.

## Shape of this subsystem

The platform/peripheral layer is in good shape for code that crossed three hosts during active bring-up: the platform seam (`platform::imp` + constants) is genuinely thin, backend axes resolve through one mechanism in build.rs, and hardware sequences are annotated against their Linux/Circle oracles line-by-line — the review found one latent coherency bug (mailbox buffer alignment) and zero logic errors in the IRQ/DMA state machines, which is a strong result. The weak spots are exactly what you'd expect post-bring-up: feature axes that compose syntactically but not semantically, an audio driver still wearing its diagnostic harness, and a comment layer that has drifted from the code in the places that changed most (FVP EL3 story, SD driver maturity, tracer mechanism).

Top three refactors, in order of payoff: **(1)** add cross-axis validation to build.rs and replace the scattered `all(no-semihost, platform-raspi3b)` gates with a single resolver-emitted `nh_real_hw`/`nh_host_dma` cfg — this fixes the two confirmed broken builds, prevents the FVP×real-hw-backend class, and makes the matrix self-documenting; **(2)** strip `pi_hdmi.rs`'s constant-folded diagnostic matrix down to the shipped configuration (and fix the `RingState` aliasing while in there) — the file drops from ~2050 lines to something a future maintainer can hold in their head; **(3)** do a one-pass doc reconciliation against the as-built system (fvp_base/gicv3/boot.s EL3 narrative, sdhost bring-up status, Cargo.toml `trace`), since this codebase leans on its comments as the primary design record and the stale ones currently point debugging effort in wrong directions.
