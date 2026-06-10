# Vendored: embedded-sdmmc 0.9.0

Source: https://github.com/rust-embedded-community/embedded-sdmmc-rs
(crates.io `embedded-sdmmc` v0.9.0, MIT OR Apache-2.0).

## Why vendored

The flash-persist background-DMA autosave needs to write the
`NEWTON.BIN` flash image's sectors directly (so the write can be
DMA-driven and interleave with the guest, instead of freezing it inside
the synchronous `BlockDevice::write` loop). That requires the file's
on-disk LBA extent, which the upstream public API does not expose — but
the cluster→block math (`FatVolume::cluster_to_block`) and FAT-chain
traversal (`next_cluster`) already exist internally. Vendoring lets us
expose them through one small method rather than maintaining a second,
parallel FAT32 implementation in the hypervisor.

## Local changes vs. upstream 0.9.0

- `Cargo.toml`: trimmed to the library target (dropped the upstream
  `[[example]]`/`[[test]]` stanzas and dev-dependencies; we don't build
  them as a path dependency).
- `src/volume_mgr.rs`: added `VolumeManager::file_cluster_lbas` — fills
  a caller buffer with the starting LBA of each of an open file's data
  clusters (file order), returning `(num_clusters, blocks_per_cluster)`.
  Lets a caller DMA-write each cluster independently regardless of
  fragmentation. Reuses the existing internal `cluster_to_block` /
  `next_cluster`. No upstream behaviour changed.
- `src/volume_mgr.rs`: `device()` made generic over the closure return
  type `R` (was hardcoded `-> T`, the TimeSource type — an upstream
  wart that made the accessor unusable for returning a read result).

To re-vendor a newer upstream: re-copy `src/`, reapply the
`file_contiguous_extent` addition, and diff this file's change list.
