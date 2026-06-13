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
- `src/fat/volume.rs`: added `FatVolume::alloc_cluster_chain` —
  allocates a file's whole cluster chain for a known final size in one
  pass over the FAT (each touched FAT sector read + written once,
  duplicated to the second FAT), instead of one `alloc_cluster` call
  (with its rescans and per-entry writes) per cluster. Claimed entries
  hit the disk EOF-terminated before anything links to them, and
  exhaustion rolls the new chain back before `NotEnoughSpace`.
- `src/volume_mgr.rs`: added `VolumeManager::file_preallocate` — bulk-
  allocates an open zero-length file's chain via `alloc_cluster_chain`,
  persists the dir entry's cluster (on-disk size stays 0 until the next
  `flush_file`, so an interrupted data transfer is never presented as a
  valid file) and sets the in-memory size for `file_cluster_lbas`.
- `src/fat/volume.rs`: added `alloc_chain_tests`, host-side unit tests
  for `alloc_cluster_chain` (fresh/fragmented/extend/rollback, FAT16 +
  FAT32). Run from this directory with an explicit host target, because
  the repo-level `.cargo/config.toml` pins the bare-metal target:
  `cargo test --target aarch64-apple-darwin`.
- `Cargo.toml`: re-added the `hex-literal` dev-dependency — upstream's
  in-library `#[cfg(test)]` fixtures need it under `cargo test`.

To re-vendor a newer upstream: re-copy `src/`, reapply the additions
above, and diff this file's change list.
