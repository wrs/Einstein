//! Writing an uploaded container to `HYPERV.IMG` on the boot
//! partition, so the next power-on (where the firmware loads that
//! file to `IMAGE_ADDR`) boots what was just uploaded.
//!
//! The file is a fixed [`FILE_SIZE`] so it can be rewritten *in place*:
//! with the FAT chain already allocated, every 512-byte sector of the
//! container has a known LBA (`VolumeManager::file_cluster_lbas`, the
//! vendored addition) and is written straight through the block
//! device, bypassing the FAT layer's per-write bookkeeping. Only the
//! sectors that differ from the firmware-loaded copy are written — a
//! typical rebuild changes a small fraction of the 10 MiB image — and
//! the header sector goes last, so a power cut mid-write leaves a
//! container whose CRC fails (nhboot then waits for a re-upload)
//! rather than one that claims a half-written payload is valid.
//!
//! The slow path — file missing or not [`FILE_SIZE`] — creates it
//! through the FAT API (`ReadWriteCreateOrTruncate` + `write`, which
//! allocates the chain cluster by cluster), then still writes the
//! header sector last via the LBA map. That path is a fallback:
//! `scripts/build-sd.sh` puts a correctly sized file on a fresh card.

use core::cell::UnsafeCell;

use embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};

use crate::image::{FILE_SIZE, HDR_SIZE};
use crate::println;
use crate::sd::block_device::NullTime;
use crate::sd::sdhost::{CmdError, SdHost};
use crate::time::{elapsed_us, now_us};

/// The container file, at the FAT32 root (8.3 name).
pub const FILE_NAME: &str = "HYPERV.IMG";

const SECTOR: usize = 512;
/// Sized for the smallest possible FAT cluster (one sector), so any
/// formatter's choice fits: 32 Ki entries = 128 KiB of .bss, which
/// costs nothing in the binary. (A 64 MB test volume from macOS
/// `hdiutil` really does use 512-byte clusters.)
const MAX_CLUSTERS: usize = FILE_SIZE / SECTOR;
/// Progress line cadence, in sectors written.
const PROGRESS_EVERY: u32 = 2048;
/// FAT-path write granularity for the create fallback (also the size
/// of the zero buffer in .rodata — keep it small).
const CREATE_CHUNK: usize = 4096;

type Vm = VolumeManager<SdHost, NullTime, 4, 4, 1>;

#[derive(Debug)]
pub enum PersistError {
    Sd(CmdError),
    Fat(embedded_sdmmc::Error<CmdError>),
    /// `file_cluster_lbas` declined (zero-length file or no chain).
    NoLbaMap,
    /// The file's cluster chain has more clusters than our map holds
    /// (cannot happen with a ≥512-byte cluster; kept as a trip-wire).
    TooManyClusters,
    /// The chain covers fewer bytes than the file claims.
    ShortChain,
}

impl core::fmt::Display for PersistError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PersistError::Sd(e) => write!(f, "sd: {:?}", e),
            PersistError::Fat(e) => write!(f, "fat: {:?}", e),
            PersistError::NoLbaMap => write!(f, "no cluster map for {}", FILE_NAME),
            PersistError::TooManyClusters => write!(f, "{} has more clusters than the map", FILE_NAME),
            PersistError::ShortChain => write!(f, "{}'s cluster chain is shorter than its size", FILE_NAME),
        }
    }
}

impl From<embedded_sdmmc::Error<CmdError>> for PersistError {
    fn from(e: embedded_sdmmc::Error<CmdError>) -> Self {
        PersistError::Fat(e)
    }
}

impl From<CmdError> for PersistError {
    fn from(e: CmdError) -> Self {
        PersistError::Sd(e)
    }
}

pub struct Stats {
    pub sectors_written: u32,
    pub sectors_total: u32,
    pub created: bool,
    pub ms: u64,
}

/// Per-cluster start LBAs of the container file. Far too big for the
/// 16 KiB stack, so it lives in .bss. `UnsafeCell` instead of
/// `static mut` for the `static_mut_refs` lint; single core, and only
/// `persist` touches it.
struct LbaMap(UnsafeCell<[u32; MAX_CLUSTERS]>);
// SAFETY: single-core bootloader, no concurrent access.
unsafe impl Sync for LbaMap {}
static LBAS: LbaMap = LbaMap(UnsafeCell::new([0; MAX_CLUSTERS]));

/// Write the container at `new_base` (header + `payload_len` payload
/// bytes) to `HYPERV.IMG`. `old` is the firmware-loaded container if
/// it validated — the exact bytes on the card, so sectors equal to it
/// are skipped.
pub fn persist(new_base: usize, payload_len: u32, old: Option<usize>) -> Result<Stats, PersistError> {
    let t0 = now_us();
    let sd = SdHost::init()?;
    let mgr: Vm = VolumeManager::new(sd, NullTime);
    let volume = mgr.open_volume(VolumeIdx(0))?;
    let root = volume.open_root_dir()?;

    // Fast path: the file exists at the fixed size.
    let mut created = false;
    let file = match root.open_file_in_dir(FILE_NAME, Mode::ReadWriteAppend) {
        Ok(f) if f.length() as usize == FILE_SIZE => f,
        Ok(f) => {
            println!("persist: {} is {} bytes, not {}; recreating", FILE_NAME, f.length(), FILE_SIZE);
            f.close()?;
            created = true;
            create_full_file(&root, new_base, payload_len)?
        }
        Err(embedded_sdmmc::Error::NotFound) => {
            println!("persist: {} not found; creating", FILE_NAME);
            created = true;
            create_full_file(&root, new_base, payload_len)?
        }
        Err(e) => return Err(e.into()),
    };

    // Resolve sector → LBA through the cluster chain.
    let raw = file.to_raw_file();
    // SAFETY: see `LbaMap`.
    let lbas: &mut [u32; MAX_CLUSTERS] = unsafe { &mut *LBAS.0.get() };
    let (num_clusters, bpc) = match mgr.file_cluster_lbas(raw, lbas) {
        Ok(Some(v)) => v,
        Ok(None) => {
            // Either no chain or more clusters than the map holds;
            // distinguish by what the map could have held.
            let _ = mgr.close_file(raw);
            return Err(PersistError::NoLbaMap);
        }
        Err(e) => {
            let _ = mgr.close_file(raw);
            return Err(e.into());
        }
    };
    if num_clusters > MAX_CLUSTERS {
        let _ = mgr.close_file(raw);
        return Err(PersistError::TooManyClusters);
    }
    let total_sectors = (HDR_SIZE + payload_len as usize).div_ceil(SECTOR) as u32;
    if (num_clusters as u64) * (bpc as u64) < total_sectors as u64 {
        let _ = mgr.close_file(raw);
        return Err(PersistError::ShortChain);
    }
    let lba_of = |sector: u32| -> u32 { lbas[(sector / bpc) as usize] + sector % bpc };

    // Which sectors need writing. The create path just wrote every
    // payload sector through the FAT API, so only the header is left.
    let hdr_sectors = (HDR_SIZE / SECTOR) as u32;
    let mut written: u32 = 0;
    let result = (|| -> Result<(), PersistError> {
        if !created {
            for s in hdr_sectors..total_sectors {
                if sector_unchanged(new_base, old, s) {
                    continue;
                }
                mgr.device(|d| d.write_block(lba_of(s), sector_of(new_base, s)))?;
                written += 1;
                if written.is_multiple_of(PROGRESS_EVERY) {
                    println!("persist: {}/{} sectors", written, total_sectors);
                }
            }
        }
        // Header last (sector 0 carries the magic/len/CRC; 1..7 are
        // zero and rarely differ).
        for s in (0..hdr_sectors).rev() {
            if !created && sector_unchanged(new_base, old, s) {
                continue;
            }
            mgr.device(|d| d.write_block(lba_of(s), sector_of(new_base, s)))?;
            written += 1;
        }
        Ok(())
    })();
    let _ = mgr.close_file(raw);
    result?;
    Ok(Stats {
        sectors_written: written,
        sectors_total: total_sectors,
        created,
        ms: elapsed_us(t0) / 1000,
    })
}

/// Create `FILE_NAME` at `FILE_SIZE` through the FAT layer: a zero
/// header (rewritten last by the caller), the payload, zero pad.
fn create_full_file<'a>(
    root: &'a embedded_sdmmc::Directory<'a, SdHost, NullTime, 4, 4, 1>,
    new_base: usize,
    payload_len: u32,
) -> Result<embedded_sdmmc::File<'a, SdHost, NullTime, 4, 4, 1>, PersistError> {
    static ZEROS: [u8; CREATE_CHUNK] = [0; CREATE_CHUNK];
    let file = root.open_file_in_dir(FILE_NAME, Mode::ReadWriteCreateOrTruncate)?;
    let t0 = now_us();
    file.write(&ZEROS[..HDR_SIZE])?;
    // SAFETY: the new container's payload, filled by xfer.rs.
    let payload: &[u8] = unsafe {
        core::slice::from_raw_parts((new_base + HDR_SIZE) as *const u8, payload_len as usize)
    };
    let mut off = 0;
    while off < payload.len() {
        let end = (off + CREATE_CHUNK).min(payload.len());
        file.write(&payload[off..end])?;
        off = end;
        if off % (1 << 20) == 0 {
            println!("persist: created {} KiB", (HDR_SIZE + off) >> 10);
        }
    }
    let mut remaining = FILE_SIZE - HDR_SIZE - payload.len();
    while remaining > 0 {
        let n = remaining.min(CREATE_CHUNK);
        file.write(&ZEROS[..n])?;
        remaining -= n;
    }
    file.flush()?;
    println!(
        "persist: created {} ({} bytes) in {} ms",
        FILE_NAME,
        FILE_SIZE,
        elapsed_us(t0) / 1000
    );
    Ok(file)
}

fn sector_of(base: usize, s: u32) -> &'static [u8; SECTOR] {
    // SAFETY: inside a container area (16 MiB at a fixed address),
    // read-only, and `s` is below `total_sectors`.
    unsafe { &*((base + s as usize * SECTOR) as *const [u8; SECTOR]) }
}

fn sector_unchanged(new_base: usize, old: Option<usize>, s: u32) -> bool {
    match old {
        Some(old_base) => sector_of(new_base, s) == sector_of(old_base, s),
        None => false,
    }
}
