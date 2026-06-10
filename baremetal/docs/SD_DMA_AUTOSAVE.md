# Non-blocking SD flash autosave (background DMA) — plan + status

## Problem

The flash-persist incremental autosave (every 2 s) writes dirty 64 KiB
blocks of `NEWTON.BIN` to the SD card **synchronously via SDHOST PIO**,
busy-waiting in EL2 for ~186 ms. During that window the **guest is
frozen**, so `TSoundServer` can't queue the next audio buffer; the
HDMI MAI ring (~46 ms cushion) underruns and replays stale fill →
**audible dropout**. (The audio *engine* stays fed — it's DMA-driven
and serviced by the slim ISR during the save's unmasked window — the
dropout is the guest being unable to produce new samples.) Evidence:
guest `sound` subfn calls show a ~200 ms gap bracketing each
`flash_persist_sd: incremental save ... done (... 186 ms ...)` line.

**Goal:** make the save non-blocking — DMA-driven, advanced by the
channel completion IRQ — so the guest keeps running during it. DMA
does *not* make the card faster (the ~680 KB/s is the card's program
time); it frees the CPU/guest during the transfer.

Same architectural direction as the other interrupt-driven work this
cycle (audio MAI DMA; the USB tablet, see the `input-mtouch`
interrupt-driven commit): move polled/blocking peripherals onto the
general-purpose same-EL ISR (`trap::irq_from_el2` / `host_dma::on_completion`).

## Milestones

| # | What | Status |
|---|------|--------|
| 1 | Confirm SDHOST DMA DREQ = **13** (Linux DT `sdhost { dmas = <&dma 13> }`; Circle TDREQ gap between UARTTX=12 / UARTRX=14) | done |
| 2 | Isolated **single-block** (512 B) DMA write, polled; probe verifies write→read-back | validated (`dma-write: PASS`) |
| — | Vendor embedded-sdmmc 0.9.0 (path dep) to expose the file's on-disk LBAs | done |
| 3 | Resolve `NEWTON.BIN`'s **per-cluster LBA map**, verify cluster 0 raw vs loaded image | validated |
| 4a | **Multi-block** (cluster = 64-sector / 32 KiB) DMA write (`CMD25` + DMA + `CMD12` + busy) | implemented — HW validation pending |
| 4b | Background per-cluster save state machine on the completion IRQ | implemented — HW validation pending |

## Key facts / where things live

- **DMA channel:** `SD_TX_CHANNEL = 6`, `DREQ_SDHOST = 13` in
  `src/peripherals/host_dma.rs`. Helpers there: `init_sd_tx`,
  `arm_sd_tx`, `sd_tx_active`, `sd_tx_error`, `sd_tx_abort`. Bare-DREQ
  CS flags (SD yields to MAI's higher AXI priority — intended).
- **Single-block DMA write (m2, validated):**
  `SdHost::write_block_dma(lba, &[u8;512])` in `src/sd/sdhost.rs` —
  DREQ-paced CB (RAM→`SDDATA`, 32-bit beats, `SRC_INC`+`DEST_DREQ`),
  armed before `CMD24`, polled to completion. `CmdError::DmaError`
  added. m4a generalises this to N sectors via `CMD25`.
- **Per-cluster map (m3, validated):** vendored
  `VolumeManager::file_cluster_lbas(file, out: &mut [u32]) ->
  Option<(num_clusters, blocks_per_cluster)>` (in
  `vendor/embedded-sdmmc/src/volume_mgr.rs`; also made `device()`
  generic over its closure return — see `vendor/.../VENDOR.md`).
  Resolved + verified in `flash_persist/sd.rs::try_load`, cached in:
  - `FLASH_CLUSTER_LBAS` — `UnsafeCell<[u32; MAX_FLASH_CLUSTERS]>`,
    `MAX_FLASH_CLUSTERS = flash::SIZE / 4096 = 2048`. `[i]` = start LBA
    of file cluster `i`.
  - `FLASH_NUM_CLUSTERS: AtomicUsize` — 0 = unresolved → use FAT save.
  - `FLASH_BLOCKS_PER_CLUSTER: AtomicU32`.
- **This card's geometry (Pi Zero 2 W test card):** 32 KiB clusters
  (`64 blocks/cluster`), **256 clusters** for the 8 MiB image,
  `lba[0] = 139382`. The dirty unit `BLOCK_SIZE = 64 KiB`
  (`src/flash_persist/sd.rs`), so **each dirty 64 KiB block = 2
  clusters** (possibly non-adjacent — the file is fragmented; that's
  why the map is per-cluster, not a single extent). Do NOT assume
  block == cluster: clusters can be smaller (handle 2+ per block) or
  larger than `BLOCK_SIZE`.
- **Save trigger:** `snapshot::maybe_autosave` -> `maybe_flash_autosave`
  (`src/snapshot.rs`, every `AUTOSAVE_INTERVAL_MS = 2000`) ->
  `with_irqs_unmasked(flash_persist::maybe_save)`. Current
  `SdBackend::maybe_save` (`flash_persist/sd.rs`) is the synchronous
  FAT seek+write loop over dirty 64 KiB blocks — m4b replaces its hot
  path when the map is resolved.
- **Completion IRQ plumbing:** `host_dma::on_completion(ch)` dispatched
  from `trap::irq_from_el2` / `irq_from_guest` for channels 4 (MAI) and
  5 (UART). m4b adds channel 6 dispatch -> an SD-save `on_completion`.
  Mind the slim-ISR contract doc in `trap.rs` (list any new state it
  touches).

## Milestone 4a — multi-block DMA write (implemented)

`SdHost::write_sectors_dma(lba, &[u8])` in `src/sd/sdhost.rs` (len a
multiple of 512): `prepare_data(hcfg, 512, n_blocks)`, CB
`txfr_len = n*512`, `CMD25` (WRITE_MULTIPLE_BLOCK), DMA the whole
buffer DREQ-paced, poll to completion, settle `finish_data_phase`, then
`CMD12` (STOP_TRANSMISSION, R1b — `send_cmd_kind` adds `SDCMD_BUSYWAIT`
automatically). This is the **polled** form, kept for isolated
bring-up; m4b uses the async pair below instead.

The CB-build/arm and the completion poll are factored into module
helpers (`arm_sd_dma(buf_pa, len, inten)` and `poll_sd_dma_done`), now
shared by `write_block_dma` (single 512 B) and `write_sectors_dma`.

**Still TODO (HW validation):** the polled `write_sectors_dma` exists
but no probe drives it yet. Before relying on the live save, extend the
dma-write probe to write a 32 KiB pattern to a scratch region and read
it back via PIO — a sequencing bug here would corrupt `NEWTON.BIN`.

## Milestone 4b — background per-cluster save (implemented)

Async primitives in `src/sd/sdhost.rs`:
- `start_sectors_dma(lba, &[u8])` — `prepare_data` + arm the channel
  **with `TI_INTEN`** (raises GPU IRQ 16+`SD_TX_CHANNEL` on completion)
  + `CMD25`, then return immediately. Source must be cache-flushed
  (`arm_sd_dma` does `dc_civac` it) and stable until completion.
- `finish_sectors_dma()` — from the completion IRQ: check CS.ERROR,
  settle the write FSM, `CMD12` + busy, restore idle `SDHCFG`.

State machine in `src/flash_persist/sd.rs` (`SAVE_ACTIVE` / `SAVE_CLUSTER`
/ `SAVE_PENDING_CL` bitmap / `SAVE_SNAPSHOT`):
- `maybe_save` tick: when `valid && FLASH_NUM_CLUSTERS != 0`,
  `try_start_dma_save` swaps out the dirty bitmap (guest can keep
  dirtying), builds the dirty-**cluster** set (each dirty 64 KiB block →
  the clusters its byte range overlaps), and `advance_save` starts the
  first cluster's DMA, then returns to the guest. If a save is still in
  flight it re-marks this tick's blocks for the next pass.
- Completion IRQ (`host_dma::on_completion(SD_TX_CHANNEL)` →
  `flash_persist::on_sd_dma_done` → `sd::on_dma_completion`): finish the
  cluster just written, then `advance_save` starts the next, until the
  set drains (`finish_save` → `Idle`).
- Each cluster: DMA `GUEST_FLASH[ci*cluster_bytes .. +cluster_bytes]` →
  `FLASH_CLUSTER_LBAS[ci]` (per-cluster, so fragmentation is fine).
- On any DMA/finish error: `abort_save` tears down the channel,
  re-marks the snapshot dirty, and clears `FLASH_NUM_CLUSTERS` so the
  next tick falls back to the proven synchronous FAT writes (rather than
  retrying a latched-error channel forever).

**Design note — CMD12 busy is handled inline, not as a polled
`WaitBusy` state.** The completion handler runs `finish_sectors_dma`
(which busy-waits out the card program time after `CMD12`) inside a
`cpu::with_irqs_unmasked` window, so the audio MAI feed / CNTHP rearm
stay serviced through the wait; the nested IRQs take the slim
`irq_from_el2` path, which never starts a save, so the SD controller is
never re-entered. The guest runs during each cluster's DMA **data
phase** (the part DMA offloads) but is still paused during each
cluster's card-program busy. Fully overlapping the busy with guest
execution — a true polled `WaitBusy` advanced from the timer tick —
is the remaining optional refinement; it needs the BCM2835-SDHOST
busy-completion semantics cross-checked against the datasheet / Linux
`bcm2835-sdhost` and a measured per-cluster busy time to justify the
added hot-path complexity.

## Build-gate fix (incidental)

The m2 commit (`write_block_dma` + `SD_TX_CB`) referenced
`peripherals::host_dma` — which is gated `all(no-semihost,
platform-raspi3b)` — without matching cfg gates, so the default
(semihost) build, including the QEMU guest-test build, failed to
compile. All `host_dma`-dependent items in `sdhost.rs` (the DMA write
methods, `cmd_arg`, `SD_TX_CB`, `arm_sd_dma`, `poll_sd_dma_done`) now
carry the same cfg as `host_dma`. `guest-tests/scripts/run-all.sh` is
green again (35/35).

## How to validate (input build)

```bash
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest> [sd-mount]
```
At boot, `try_load` logs the resolved map:
`flash_persist_sd: extent N clusters, M blocks/cluster, lba[0]=X — verified (DMA save eligible)`.
For m4b, the win shows as the guest `sound` subfn gap around each save
shrinking from ~200 ms toward ~0 (and no audible dropout).

## Side note: sd-probe FAT mount hangs

The `sd-probe` build wedges at `fat: handing off to embedded-sdmmc...`
(no further output) — a **pre-existing** issue (the milestone-2 log,
before any vendoring, stopped at the same line). The *same* mount path
works fine in the input build's flash-persist (proven by successful
saves + the m3 extent verify), so it's a stale probe-tool quirk, not a
product bug. Don't validate FAT-dependent things via the probe; use the
input build.
