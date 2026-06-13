# Non-blocking SD flash autosave (background DMA)

The flash-persist autosave writes dirty blocks of `NEWTON.BIN` to the
SD card via DMA, advanced cluster-by-cluster from the DMA completion
IRQ, so the guest keeps running during a save. A synchronous FAT
write path remains as the fallback for first-time/full saves and for
error recovery.

## Why DMA

A synchronous PIO save busy-waits in EL2 for the duration of the
write (~186 ms for a typical incremental save at the card's
~680 KB/s program rate). The guest is frozen for that window, so
`TSoundServer` can't queue the next audio buffer and the HDMI MAI
ring (~46 ms cushion) underruns → audible dropout. DMA doesn't make
the card faster; it frees the CPU/guest during the transfer. Same
architectural direction as the rest of the interrupt-driven
peripherals: move polled/blocking work onto the general-purpose
same-EL ISR (`trap::irq_from_el2` / `host_dma::on_completion`).

## Pieces / where things live

- **DMA channel:** `SD_TX_CHANNEL = 6`, `DREQ_SDHOST = 13` in
  `src/host/host_dma.rs`. Helpers there: `init_sd_tx`,
  `arm_sd_tx`, `sd_tx_active`, `sd_tx_error`, `sd_tx_abort`. Bare-DREQ
  CS flags (SD yields to MAI's higher AXI priority — intended).
- **SDHOST DMA writes** in `src/host/sd/sdhost.rs`:
  - `write_block_dma(lba, &[u8;512])` — single block, polled.
  - `write_sectors_dma(lba, &[u8])` — N sectors via `CMD25` +
    DREQ-paced DMA + `CMD12`, polled. Kept for isolated bring-up.
  - `start_sectors_dma(lba, &[u8])` / `finish_sectors_dma()` — the
    async pair the autosave uses. `start` does `prepare_data`, arms
    the channel **with `TI_INTEN`** (GPU IRQ 16+`SD_TX_CHANNEL` on
    completion), issues `CMD25`, and returns immediately; the source
    buffer is cache-flushed (`dc_civac`) by `arm_sd_dma` and must
    stay stable until completion. `finish` runs from the completion
    IRQ: check CS.ERROR, settle the write FSM, `CMD12` + busy,
    restore idle `SDHCFG`.
  - The CB-build/arm and completion poll are shared module helpers
    (`arm_sd_dma(buf_pa, len, inten)`, `poll_sd_dma_done`).
- **Per-cluster LBA map:** the vendored embedded-sdmmc exposes
  `VolumeManager::file_cluster_lbas(file, out: &mut [u32]) ->
  Option<(num_clusters, blocks_per_cluster)>`
  (`vendor/embedded-sdmmc/src/volume_mgr.rs`; local changes listed in
  `vendor/.../VENDOR.md`). Resolved + verified at load time in
  `flash_persist/sd.rs::try_load`, cached in:
  - `FLASH_CLUSTER_LBAS` — `UnsafeCell<[u32; MAX_FLASH_CLUSTERS]>`,
    `MAX_FLASH_CLUSTERS = flash::SIZE / 4096 = 2048`. `[i]` = start LBA
    of file cluster `i`.
  - `FLASH_NUM_CLUSTERS: AtomicUsize` — 0 = unresolved → use the
    synchronous FAT save.
  - `FLASH_BLOCKS_PER_CLUSTER: AtomicU32`.

  The map is per-cluster, not a single extent, because the file can
  be fragmented. Do NOT assume dirty block == cluster: the dirty unit
  `BLOCK_SIZE = 64 KiB` (`src/host/flash_persist/sd.rs`) typically spans
  2+ clusters (e.g. 32 KiB clusters on the bench card), and clusters
  can also be larger than `BLOCK_SIZE`.
- **Save trigger:** `snapshot::maybe_autosave` →
  `maybe_flash_autosave` (`src/hv/snapshot.rs`, every
  `AUTOSAVE_INTERVAL_MS = 2000`) →
  `with_irqs_unmasked(flash_persist::maybe_save)`.
- **Completion IRQ plumbing:** `host_dma::on_completion(ch)`
  dispatched from `trap::irq_from_el2` / `irq_from_guest` for
  channels 4 (MAI), 5 (UART), and 6 (SD). Mind the slim-ISR contract
  doc in `trap.rs` (list any new state it touches).

## The save state machine

In `src/host/flash_persist/sd.rs` (`SAVE_ACTIVE` / `SAVE_CLUSTER` /
`SAVE_PENDING_CL` bitmap / `SAVE_SNAPSHOT`):

- `maybe_save` tick: when `valid && FLASH_NUM_CLUSTERS != 0`,
  `try_start_dma_save` swaps out the dirty bitmap (guest can keep
  dirtying), builds the dirty-**cluster** set (each dirty 64 KiB
  block → the clusters its byte range overlaps), and `advance_save`
  starts the first cluster's DMA, then returns to the guest. If a
  save is still in flight it re-marks this tick's blocks for the next
  pass.
- Completion IRQ (`host_dma::on_completion(SD_TX_CHANNEL)` →
  `flash_persist::on_sd_dma_done` → `sd::on_dma_completion`): finish
  the cluster just written, then `advance_save` starts the next,
  until the set drains (`finish_save` → `Idle`).
- Each cluster: DMA `GUEST_FLASH[ci*cluster_bytes .. +cluster_bytes]`
  → `FLASH_CLUSTER_LBAS[ci]` (per-cluster, so fragmentation is fine).
- On any DMA/finish error: `abort_save` tears down the channel,
  re-marks the snapshot dirty, and clears `FLASH_NUM_CLUSTERS` so the
  next tick falls back to the synchronous FAT writes (rather than
  retrying a latched-error channel forever).

**Design note — CMD12 busy is handled inline, not as a polled
`WaitBusy` state.** The completion handler runs `finish_sectors_dma`
(which busy-waits out the card program time after `CMD12`) inside a
`cpu::with_irqs_unmasked` window, so the audio MAI feed / CNTHP rearm
stay serviced through the wait; the nested IRQs take the slim
`irq_from_el2` path, which never starts a save, so the SD controller
is never re-entered. The guest runs during each cluster's DMA **data
phase** (the part DMA offloads) but is still paused during each
cluster's card-program busy. Fully overlapping the busy with guest
execution — a true polled `WaitBusy` advanced from the timer tick —
is the one remaining optional refinement; it needs the BCM2835-SDHOST
busy-completion semantics cross-checked against the datasheet / Linux
`bcm2835-sdhost` and a measured per-cluster busy time to justify the
added hot-path complexity.

## Initial full save (fresh card / wrong-size NEWTON.BIN)

The first save has no file to write into, so the background machine
above can't run. Historically it grew `NEWTON.BIN` through the FAT
layer's generic write path — one single-block CMD24 (each with card
program/busy time) per 512 bytes, ~16 k commands for 8 MiB, plus one
`alloc_cluster` FAT scan-and-update round trip per cluster: minutes of
frozen guest on a fresh card.

`fast_full_save` (`src/host/flash_persist/sd.rs`) replaces that:

1. `VolumeManager::file_preallocate` (vendored LOCAL ADDITION) bulk-
   allocates the whole cluster chain in one FAT pass
   (`FatVolume::alloc_cluster_chain` — each touched FAT sector written
   once) and persists the dir entry's **cluster only**; the on-disk
   size stays 0.
2. `file_cluster_lbas` fills `FLASH_CLUSTER_LBAS` *without* publishing
   it (`FLASH_NUM_CLUSTERS` was zeroed before the truncate, so the
   background path can't read a stale map).
3. The data streams out as coalesced multi-block CMD25 transfers
   (`write_sectors_dma`): contiguous cluster runs — on a fresh card,
   usually one run for the whole file — chunked at 256 KiB so progress
   dots tick and each trailing CMD12 busy stays bounded.
4. Only after the data is on disk does `flush_file` write FSInfo and
   the dir entry with the real size, and `resolve_extent_map` verify +
   publish the map (background DMA eligible from the next tick, no
   reboot needed).

Crash-safety ordering: the FAT chain on disk is EOF-terminated at every
intermediate step, and the dir entry says size 0 until the final flush —
a power cut mid-save leaves a file `try_load` rejects (`len != SIZE` →
cold boot) and the next save's truncate reclaims, never a chain pointing
at free clusters.

Fallback: any `fast_full_save` failure (cluster size < 4 KiB makes the
map exceed `MAX_FLASH_CLUSTERS`, DMA init/CMD25 error, disk full) logs
`fast full save unavailable (…) — FAT-path fallback` and reruns the old
chunked `file.write` loop on the same open file. The log line tells the
paths apart: `full save done (DMA, …)` vs `full save done (FAT, …)`.

## Build / observe

```bash
PI_CARGO_FEATURES=pi-bare-metal-input scripts/build-sd.sh <dest> [sd-mount]
```

At boot, `try_load` logs the resolved map:
`flash_persist_sd: extent N clusters, M blocks/cluster, lba[0]=X — verified (DMA save eligible)`.

All `host_dma`-dependent items in `sdhost.rs` carry the same
`all(no-semihost, platform-raspi3b)` cfg as `host_dma` itself, so the
default semihost/QEMU build (including guest-tests) compiles without
them.

## Side note: sd-probe FAT mount hangs

The `sd-probe` build wedges at `fat: handing off to embedded-sdmmc...`
(no further output). The *same* mount path works fine in the input
build's flash-persist, so it's a stale probe-tool quirk, not a product
bug. Don't validate FAT-dependent things via the probe; use the input
build.
