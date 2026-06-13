# Non-blocking SD flash autosave (background DMA)

The flash-persist autosave writes dirty blocks of `NEWTON.BIN` to the
SD card via DMA, advanced cluster-by-cluster from the DMA completion
IRQ, so the guest keeps running during a save. The background save is
always on — there is no feature gate or toggle. A synchronous FAT
write path remains as the fallback for error recovery and for cards
whose per-cluster map cannot be resolved.

(Historical note: a long intermittent-corruption hunt once implicated
this save path; the root cause turned out to be hypervisor TLB
maintenance around the kernel's MMU-off windows, not the save —
`HIGHLEVEL.md` §4.4, `docs/project-history.md` §9. The save's design
is corruption-safe as described below.)

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
  - `start_sectors_dma(lba, &[u8])` /
    `begin_finish_sectors_dma()` / `poll_finish_sectors_dma()` — the
    async triple the autosave uses. `start` does `prepare_data`, arms
    the channel **with `TI_INTEN`** (GPU IRQ 16+`SD_TX_CHANNEL` on
    completion), issues `CMD25`, and returns immediately; the source
    buffer must stay stable until completion. `begin_finish` runs
    from the completion IRQ: check CS.ERROR, settle the write FSM,
    and *issue* `CMD12` — no wait. `poll_finish` is a non-blocking
    `SDCMD.NEW_FLAG` check that restores the idle `SDHCFG` once the
    card's program time has elapsed.
  - The CB-build/arm and completion poll are shared module helpers
    (`arm_sd_dma(buf_pa, len, inten)`, `poll_sd_dma_done`).
    `arm_sd_dma` cleans the source range to RAM with `dc_cvac_range`
    — clean-only, **not** clean+invalidate: the DMA only reads the
    buffer, and invalidating GUEST_FLASH on every autosave would
    evict the guest's live store working set (observed as a general
    UI slowdown).
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
  doc in `hv/trap/mod.rs` (list any new state it touches). The same
  guest-path timer tick also drives `poll_dma_save` (the `WaitBusy`
  poll, below).

## The save state machine

In `src/host/flash_persist/sd.rs` (`SAVE_ACTIVE` / `SAVE_WAIT_BUSY` /
`SAVE_CLUSTER` / `SAVE_PENDING_CL` bitmap / `SAVE_SNAPSHOT` /
`SAVE_STAGING`):

- `maybe_save` tick: when `valid && FLASH_NUM_CLUSTERS != 0`,
  `try_start_dma_save` swaps out the dirty bitmap (guest can keep
  dirtying), builds the dirty-**cluster** set (each dirty 64 KiB
  block → the clusters its byte range overlaps), **stages** those
  clusters (below), and `advance_save` starts the first cluster's
  DMA, then returns to the guest. If a save is still in flight it
  re-marks this tick's blocks for the next pass.
- **Save staging.** `stage_pending_clusters` memcpys the dirty
  clusters from GUEST_FLASH into an 8 MiB staging mirror
  (`SAVE_STAGING`) while the guest is still paused in the autosave
  IRQ, and the DMA reads from staging, never from live GUEST_FLASH.
  This makes the persisted image one atomic, consistent instant of
  the store. Without it, the multi-cluster DMA reads the live store
  over ~100 ms+ while the guest mutates it: the saved image is
  byte-faithful but stitched from different instants — cluster N's
  updated pointer with cluster M's not-yet-updated target — and the
  ROM's `TFlashStore` reader crashes on it at the next load. Blocks
  the guest dirties during a save stay set in `DIRTY` and are
  re-staged on the next pass.
- Completion IRQ (`host_dma::on_completion(SD_TX_CHANNEL)` →
  `flash_persist::on_sd_dma_done` → `sd::on_dma_completion`):
  `begin_finish_sectors_dma` settles the FSM and *issues* `CMD12`,
  then the handler sets the `WaitBusy` sub-state and returns — no
  busy-wait, no `with_irqs_unmasked`, no nested-IRQ re-entry.
- `WaitBusy` (`poll_dma_save`, called from every timer tick in
  `hv::trap`): `poll_finish_sectors_dma` checks whether the card's
  program time has elapsed; when it has, `advance_save` starts the
  next cluster, until the set drains (`finish_save` → idle). The
  guest runs through both the DMA data phase *and* the card-program
  busy of every cluster. The wait is bounded by
  `WAIT_BUSY_TIMEOUT_MS` (2 s; the SD spec caps a write's program
  time at 250 ms) — a card still busy past that is declared wedged
  and the save aborts.
- Each cluster: DMA `staging[ci*cluster_bytes .. +cluster_bytes]`
  → `FLASH_CLUSTER_LBAS[ci]` (per-cluster, so fragmentation is fine).
- On any DMA/finish error or `WaitBusy` timeout: `abort_save` tears
  down the channel, re-marks the snapshot dirty, and clears
  `FLASH_NUM_CLUSTERS` so the next tick falls back to the synchronous
  FAT writes (rather than retrying a latched-error channel forever).

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
