//! FAT-specific volume support.

use crate::{
    debug,
    fat::{
        Bpb, Fat16Info, Fat32Info, FatSpecificInfo, FatType, InfoSector, OnDiskDirEntry,
        RESERVED_ENTRIES,
    },
    filesystem::FilenameError,
    trace, warn, Attributes, Block, BlockCache, BlockCount, BlockDevice, BlockIdx, ClusterId,
    DirEntry, DirectoryInfo, Error, LfnBuffer, ShortFileName, TimeSource, VolumeType,
};
use byteorder::{ByteOrder, LittleEndian};
use core::convert::TryFrom;

/// An MS-DOS 11 character volume label.
///
/// ISO-8859-1 encoding is assumed. Trailing spaces are trimmed. Reserved
/// characters are not allowed. There is no file extension, unlike with a
/// filename.
///
/// Volume labels can be found in the BIOS Parameter Block, and in a root
/// directory entry with the 'Volume Label' bit set. Both places should have the
/// same contents, but they can get out of sync.
///
/// MS-DOS FDISK would show you the one in the BPB, but DIR would show you the
/// one in the root directory.
#[cfg_attr(feature = "defmt-log", derive(defmt::Format))]
#[derive(PartialEq, Eq, Clone)]
pub struct VolumeName {
    pub(crate) contents: [u8; Self::TOTAL_LEN],
}

impl VolumeName {
    const TOTAL_LEN: usize = 11;

    /// Get name
    pub fn name(&self) -> &[u8] {
        let mut bytes = &self.contents[..];
        while let [rest @ .., last] = bytes {
            if last.is_ascii_whitespace() {
                bytes = rest;
            } else {
                break;
            }
        }
        bytes
    }

    /// Create a new MS-DOS volume label.
    pub fn create_from_str(name: &str) -> Result<VolumeName, FilenameError> {
        let mut sfn = VolumeName {
            contents: [b' '; Self::TOTAL_LEN],
        };

        let mut idx = 0;
        for ch in name.chars() {
            match ch {
                // Microsoft say these are the invalid characters
                '\u{0000}'..='\u{001F}'
                | '"'
                | '*'
                | '+'
                | ','
                | '/'
                | ':'
                | ';'
                | '<'
                | '='
                | '>'
                | '?'
                | '['
                | '\\'
                | ']'
                | '.'
                | '|' => {
                    return Err(FilenameError::InvalidCharacter);
                }
                x if x > '\u{00FF}' => {
                    // We only handle ISO-8859-1 which is Unicode Code Points
                    // \U+0000 to \U+00FF. This is above that.
                    return Err(FilenameError::InvalidCharacter);
                }
                _ => {
                    let b = ch as u8;
                    if idx < Self::TOTAL_LEN {
                        sfn.contents[idx] = b;
                    } else {
                        return Err(FilenameError::NameTooLong);
                    }
                    idx += 1;
                }
            }
        }
        if idx == 0 {
            return Err(FilenameError::FilenameEmpty);
        }
        Ok(sfn)
    }

    /// Convert to a Short File Name
    ///
    /// # Safety
    ///
    /// Volume Labels can contain things that Short File Names cannot, so only
    /// do this conversion if you are creating the name of a directory entry
    /// with the 'Volume Label' attribute.
    pub unsafe fn to_short_filename(self) -> ShortFileName {
        ShortFileName {
            contents: self.contents,
        }
    }
}

impl core::fmt::Display for VolumeName {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let mut printed = 0;
        for &c in self.name().iter() {
            // converting a byte to a codepoint means you are assuming
            // ISO-8859-1 encoding, because that's how Unicode was designed.
            write!(f, "{}", c as char)?;
            printed += 1;
        }
        if let Some(mut width) = f.width() {
            if width > printed {
                width -= printed;
                for _ in 0..width {
                    write!(f, "{}", f.fill())?;
                }
            }
        }
        Ok(())
    }
}

impl core::fmt::Debug for VolumeName {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "VolumeName(\"{}\")", self)
    }
}

/// Identifies a FAT16 or FAT32 Volume on the disk.
#[cfg_attr(feature = "defmt-log", derive(defmt::Format))]
#[derive(Debug, PartialEq, Eq)]
pub struct FatVolume {
    /// The block number of the start of the partition. All other BlockIdx values are relative to this.
    pub(crate) lba_start: BlockIdx,
    /// The number of blocks in this volume
    pub(crate) num_blocks: BlockCount,
    /// The name of this volume
    pub(crate) name: VolumeName,
    /// Number of 512 byte blocks (or Blocks) in a cluster
    pub(crate) blocks_per_cluster: u8,
    /// The block the data starts in. Relative to start of partition (so add
    /// `self.lba_offset` before passing to volume manager)
    pub(crate) first_data_block: BlockCount,
    /// The block the FAT starts in. Relative to start of partition (so add
    /// `self.lba_offset` before passing to volume manager)
    pub(crate) fat_start: BlockCount,
    /// The block the second FAT starts in. Relative to start of partition (so add
    /// `self.lba_offset` before passing to volume manager)
    pub(crate) second_fat_start: Option<BlockCount>,
    /// Expected number of free clusters
    pub(crate) free_clusters_count: Option<u32>,
    /// Number of the next expected free cluster
    pub(crate) next_free_cluster: Option<ClusterId>,
    /// Total number of clusters
    pub(crate) cluster_count: u32,
    /// Type of FAT
    pub(crate) fat_specific_info: FatSpecificInfo,
}

impl FatVolume {
    /// Write a new entry in the FAT
    pub fn update_info_sector<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => {
                // FAT16 volumes don't have an info sector
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                if self.free_clusters_count.is_none() && self.next_free_cluster.is_none() {
                    return Ok(());
                }
                trace!("Reading info sector");
                let block = block_cache
                    .read_mut(fat32_info.info_location)
                    .map_err(Error::DeviceError)?;
                if let Some(count) = self.free_clusters_count {
                    block[488..492].copy_from_slice(&count.to_le_bytes());
                }
                if let Some(next_free_cluster) = self.next_free_cluster {
                    block[492..496].copy_from_slice(&next_free_cluster.0.to_le_bytes());
                }
                trace!("Writing info sector");
                block_cache.write_back()?;
            }
        }
        Ok(())
    }

    /// Get the type of FAT this volume is
    pub(crate) fn get_fat_type(&self) -> FatType {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => FatType::Fat16,
            FatSpecificInfo::Fat32(_) => FatType::Fat32,
        }
    }

    /// Write a new entry in the FAT
    fn update_fat<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
        new_value: ClusterId,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        let mut second_fat_block_num = None;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                let fat_offset = cluster.0 * 2;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                if let Some(second_fat_start) = self.second_fat_start {
                    second_fat_block_num =
                        Some(self.lba_start + second_fat_start.offset_bytes(fat_offset));
                }
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Reading FAT for update");
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .map_err(Error::DeviceError)?;
                // See <https://en.wikipedia.org/wiki/Design_of_the_FAT_file_system>
                let entry = match new_value {
                    ClusterId::INVALID => 0xFFF6,
                    ClusterId::BAD => 0xFFF7,
                    ClusterId::EMPTY => 0x0000,
                    ClusterId::END_OF_FILE => 0xFFFF,
                    _ => new_value.0 as u16,
                };
                LittleEndian::write_u16(
                    &mut block[this_fat_ent_offset..=this_fat_ent_offset + 1],
                    entry,
                );
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                // FAT32 => 4 bytes per entry
                let fat_offset = cluster.0 * 4;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                if let Some(second_fat_start) = self.second_fat_start {
                    second_fat_block_num =
                        Some(self.lba_start + second_fat_start.offset_bytes(fat_offset));
                }
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Reading FAT for update");
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .map_err(Error::DeviceError)?;
                let entry = match new_value {
                    ClusterId::INVALID => 0x0FFF_FFF6,
                    ClusterId::BAD => 0x0FFF_FFF7,
                    ClusterId::EMPTY => 0x0000_0000,
                    _ => new_value.0,
                };
                let existing =
                    LittleEndian::read_u32(&block[this_fat_ent_offset..=this_fat_ent_offset + 3]);
                let new = (existing & 0xF000_0000) | (entry & 0x0FFF_FFFF);
                LittleEndian::write_u32(
                    &mut block[this_fat_ent_offset..=this_fat_ent_offset + 3],
                    new,
                );
            }
        }
        trace!("Updating FAT");
        if let Some(duplicate) = second_fat_block_num {
            block_cache.write_back_with_duplicate(duplicate)?;
        } else {
            block_cache.write_back()?;
        }
        Ok(())
    }

    /// Look in the FAT to see which cluster comes next.
    pub(crate) fn next_cluster<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        if cluster.0 > (u32::MAX / 4) {
            panic!("next_cluster called on invalid cluster {:x?}", cluster);
        }
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                let fat_offset = cluster.0 * 2;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Walking FAT");
                let block = block_cache.read(this_fat_block_num)?;
                let fat_entry =
                    LittleEndian::read_u16(&block[this_fat_ent_offset..=this_fat_ent_offset + 1]);
                match fat_entry {
                    0xFFF7 => {
                        // Bad cluster
                        Err(Error::BadCluster)
                    }
                    0xFFF8..=0xFFFF => {
                        // There is no next cluster
                        Err(Error::EndOfFile)
                    }
                    f => {
                        // Seems legit
                        Ok(ClusterId(u32::from(f)))
                    }
                }
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                let fat_offset = cluster.0 * 4;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Walking FAT");
                let block = block_cache.read(this_fat_block_num)?;
                let fat_entry =
                    LittleEndian::read_u32(&block[this_fat_ent_offset..=this_fat_ent_offset + 3])
                        & 0x0FFF_FFFF;
                match fat_entry {
                    0x0000_0000 => {
                        // Jumped to free space
                        Err(Error::UnterminatedFatChain)
                    }
                    0x0FFF_FFF7 => {
                        // Bad cluster
                        Err(Error::BadCluster)
                    }
                    0x0000_0001 | 0x0FFF_FFF8..=0x0FFF_FFFF => {
                        // There is no next cluster
                        Err(Error::EndOfFile)
                    }
                    f => {
                        // Seems legit
                        Ok(ClusterId(f))
                    }
                }
            }
        }
    }

    /// Number of bytes in a cluster.
    pub(crate) fn bytes_per_cluster(&self) -> u32 {
        u32::from(self.blocks_per_cluster) * Block::LEN_U32
    }

    /// Converts a cluster number (or `Cluster`) to a block number (or
    /// `BlockIdx`). Gives an absolute `BlockIdx` you can pass to the
    /// volume manager.
    pub(crate) fn cluster_to_block(&self, cluster: ClusterId) -> BlockIdx {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                let block_num = match cluster {
                    ClusterId::ROOT_DIR => fat16_info.first_root_dir_block,
                    ClusterId(c) => {
                        // FirstSectorofCluster = ((N – 2) * BPB_SecPerClus) + FirstDataSector;
                        let first_block_of_cluster =
                            BlockCount((c - 2) * u32::from(self.blocks_per_cluster));
                        self.first_data_block + first_block_of_cluster
                    }
                };
                self.lba_start + block_num
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                let cluster_num = match cluster {
                    ClusterId::ROOT_DIR => fat32_info.first_root_dir_cluster.0,
                    c => c.0,
                };
                // FirstSectorofCluster = ((N – 2) * BPB_SecPerClus) + FirstDataSector;
                let first_block_of_cluster =
                    BlockCount((cluster_num - 2) * u32::from(self.blocks_per_cluster));
                self.lba_start + self.first_data_block + first_block_of_cluster
            }
        }
    }

    /// Finds a empty entry space and writes the new entry to it, allocates a new cluster if it's
    /// needed
    pub(crate) fn write_new_directory_entry<D, T>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        time_source: &T,
        dir_cluster: ClusterId,
        name: ShortFileName,
        attributes: Attributes,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                // Root directories on FAT16 have a fixed size, because they use
                // a specially reserved space on disk (see
                // `first_root_dir_block`). Other directories can have any size
                // as they are made of regular clusters.
                let mut current_cluster = Some(dir_cluster);
                let mut first_dir_block_num = match dir_cluster {
                    ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
                    _ => self.cluster_to_block(dir_cluster),
                };
                let dir_size = match dir_cluster {
                    ClusterId::ROOT_DIR => {
                        let len_bytes =
                            u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                        BlockCount::from_bytes(len_bytes)
                    }
                    _ => BlockCount(u32::from(self.blocks_per_cluster)),
                };

                // Walk the directory
                while let Some(cluster) = current_cluster {
                    for block_idx in first_dir_block_num.range(dir_size) {
                        trace!("Reading directory");
                        let block = block_cache
                            .read_mut(block_idx)
                            .map_err(Error::DeviceError)?;
                        for (i, dir_entry_bytes) in
                            block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate()
                        {
                            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                            // 0x00 or 0xE5 represents a free entry
                            if !dir_entry.is_valid() {
                                let ctime = time_source.get_timestamp();
                                let entry = DirEntry::new(
                                    name,
                                    attributes,
                                    ClusterId::EMPTY,
                                    ctime,
                                    block_idx,
                                    (i * OnDiskDirEntry::LEN) as u32,
                                );
                                dir_entry_bytes
                                    .copy_from_slice(&entry.serialize(FatType::Fat16)[..]);
                                trace!("Updating directory");
                                block_cache.write_back()?;
                                return Ok(entry);
                            }
                        }
                    }
                    if cluster != ClusterId::ROOT_DIR {
                        current_cluster = match self.next_cluster(block_cache, cluster) {
                            Ok(n) => {
                                first_dir_block_num = self.cluster_to_block(n);
                                Some(n)
                            }
                            Err(Error::EndOfFile) => {
                                let c = self.alloc_cluster(block_cache, Some(cluster), true)?;
                                first_dir_block_num = self.cluster_to_block(c);
                                Some(c)
                            }
                            _ => None,
                        };
                    } else {
                        current_cluster = None;
                    }
                }
                Err(Error::NotEnoughSpace)
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                // All directories on FAT32 have a cluster chain but the root
                // dir starts in a specified cluster.
                let mut current_cluster = match dir_cluster {
                    ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
                    _ => Some(dir_cluster),
                };
                let mut first_dir_block_num = self.cluster_to_block(dir_cluster);

                let dir_size = BlockCount(u32::from(self.blocks_per_cluster));
                // Walk the cluster chain until we run out of clusters
                while let Some(cluster) = current_cluster {
                    // Loop through the blocks in the cluster
                    for block_idx in first_dir_block_num.range(dir_size) {
                        // Read a block of directory entries
                        trace!("Reading directory");
                        let block = block_cache
                            .read_mut(block_idx)
                            .map_err(Error::DeviceError)?;
                        // Are any entries in the block we just loaded blank? If so
                        // we can use them.
                        for (i, dir_entry_bytes) in
                            block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate()
                        {
                            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                            // 0x00 or 0xE5 represents a free entry
                            if !dir_entry.is_valid() {
                                let ctime = time_source.get_timestamp();
                                let entry = DirEntry::new(
                                    name,
                                    attributes,
                                    ClusterId(0),
                                    ctime,
                                    block_idx,
                                    (i * OnDiskDirEntry::LEN) as u32,
                                );
                                dir_entry_bytes
                                    .copy_from_slice(&entry.serialize(FatType::Fat32)[..]);
                                trace!("Updating directory");
                                block_cache.write_back()?;
                                return Ok(entry);
                            }
                        }
                    }
                    // Well none of the blocks in that cluster had any space in
                    // them, let's fetch another one.
                    current_cluster = match self.next_cluster(block_cache, cluster) {
                        Ok(n) => {
                            first_dir_block_num = self.cluster_to_block(n);
                            Some(n)
                        }
                        Err(Error::EndOfFile) => {
                            let c = self.alloc_cluster(block_cache, Some(cluster), true)?;
                            first_dir_block_num = self.cluster_to_block(c);
                            Some(c)
                        }
                        _ => None,
                    };
                }
                // We ran out of clusters in the chain, and apparently we weren't
                // able to make the chain longer, so the disk must be full.
                Err(Error::NotEnoughSpace)
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory.
    /// Useful for performing directory listings.
    pub(crate) fn iterate_dir<D, F>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry),
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                self.iterate_fat16(dir_info, fat16_info, block_cache, |de, _| func(de))
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                self.iterate_fat32(dir_info, fat32_info, block_cache, |de, _| func(de))
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory,
    /// including the Long File Name.
    ///
    /// Useful for performing directory listings.
    pub(crate) fn iterate_dir_lfn<D, F>(
        &self,
        block_cache: &mut BlockCache<D>,
        lfn_buffer: &mut LfnBuffer<'_>,
        dir_info: &DirectoryInfo,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry, Option<&str>),
        D: BlockDevice,
    {
        #[derive(Clone, Copy)]
        enum SeqState {
            Waiting,
            Remaining { csum: u8, next: u8 },
            Complete { csum: u8 },
        }

        impl SeqState {
            fn update(
                self,
                lfn_buffer: &mut LfnBuffer<'_>,
                start: bool,
                sequence: u8,
                csum: u8,
                buffer: [u16; 13],
            ) -> Self {
                #[cfg(feature = "log")]
                debug!("LFN Contents {start} {sequence} {csum:02x} {buffer:04x?}");
                #[cfg(feature = "defmt-log")]
                debug!(
                    "LFN Contents {=bool} {=u8} {=u8:02x} {=[?; 13]:#04x}",
                    start, sequence, csum, buffer
                );
                match (start, sequence, self) {
                    (true, 0x01, _) => {
                        lfn_buffer.clear();
                        lfn_buffer.push(&buffer);
                        SeqState::Complete { csum }
                    }
                    (true, sequence, _) if sequence >= 0x02 && sequence < 0x14 => {
                        lfn_buffer.clear();
                        lfn_buffer.push(&buffer);
                        SeqState::Remaining {
                            csum,
                            next: sequence - 1,
                        }
                    }
                    (false, 0x01, SeqState::Remaining { csum, next }) if next == sequence => {
                        lfn_buffer.push(&buffer);
                        SeqState::Complete { csum }
                    }
                    (false, sequence, SeqState::Remaining { csum, next })
                        if sequence >= 0x01 && sequence < 0x13 && next == sequence =>
                    {
                        lfn_buffer.push(&buffer);
                        SeqState::Remaining {
                            csum,
                            next: sequence - 1,
                        }
                    }
                    _ => {
                        // this seems wrong
                        lfn_buffer.clear();
                        SeqState::Waiting
                    }
                }
            }
        }

        let mut seq_state = SeqState::Waiting;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                self.iterate_fat16(dir_info, fat16_info, block_cache, |de, odde| {
                    if let Some((start, this_seqno, csum, buffer)) = odde.lfn_contents() {
                        seq_state = seq_state.update(lfn_buffer, start, this_seqno, csum, buffer);
                    } else if let SeqState::Complete { csum } = seq_state {
                        if csum == de.name.csum() {
                            // Checksum is good, and all the pieces are there
                            func(de, Some(lfn_buffer.as_str()))
                        } else {
                            // Checksum was bad
                            func(de, None)
                        }
                    } else {
                        func(de, None)
                    }
                })
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                self.iterate_fat32(dir_info, fat32_info, block_cache, |de, odde| {
                    if let Some((start, this_seqno, csum, buffer)) = odde.lfn_contents() {
                        seq_state = seq_state.update(lfn_buffer, start, this_seqno, csum, buffer);
                    } else if let SeqState::Complete { csum } = seq_state {
                        if csum == de.name.csum() {
                            // Checksum is good, and all the pieces are there
                            func(de, Some(lfn_buffer.as_str()))
                        } else {
                            // Checksum was bad
                            func(de, None)
                        }
                    } else {
                        func(de, None)
                    }
                })
            }
        }
    }

    fn iterate_fat16<D, F>(
        &self,
        dir_info: &DirectoryInfo,
        fat16_info: &Fat16Info,
        block_cache: &mut BlockCache<D>,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: for<'odde> FnMut(&DirEntry, &OnDiskDirEntry<'odde>),
        D: BlockDevice,
    {
        // Root directories on FAT16 have a fixed size, because they use
        // a specially reserved space on disk (see
        // `first_root_dir_block`). Other directories can have any size
        // as they are made of regular clusters.
        let mut current_cluster = Some(dir_info.cluster);
        let mut first_dir_block_num = match dir_info.cluster {
            ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
            _ => self.cluster_to_block(dir_info.cluster),
        };
        let dir_size = match dir_info.cluster {
            ClusterId::ROOT_DIR => {
                let len_bytes = u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                BlockCount::from_bytes(len_bytes)
            }
            _ => BlockCount(u32::from(self.blocks_per_cluster)),
        };

        while let Some(cluster) = current_cluster {
            for block_idx in first_dir_block_num.range(dir_size) {
                trace!("Reading FAT");
                let block = block_cache.read(block_idx)?;
                for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                    if dir_entry.is_end() {
                        // Can quit early
                        return Ok(());
                    } else if dir_entry.is_valid() {
                        // Safe, since Block::LEN always fits on a u32
                        let start = (i * OnDiskDirEntry::LEN) as u32;
                        let entry = dir_entry.get_entry(FatType::Fat16, block_idx, start);
                        func(&entry, &dir_entry);
                    }
                }
            }
            if cluster != ClusterId::ROOT_DIR {
                current_cluster = match self.next_cluster(block_cache, cluster) {
                    Ok(n) => {
                        first_dir_block_num = self.cluster_to_block(n);
                        Some(n)
                    }
                    _ => None,
                };
            } else {
                current_cluster = None;
            }
        }
        Ok(())
    }

    fn iterate_fat32<D, F>(
        &self,
        dir_info: &DirectoryInfo,
        fat32_info: &Fat32Info,
        block_cache: &mut BlockCache<D>,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: for<'odde> FnMut(&DirEntry, &OnDiskDirEntry<'odde>),
        D: BlockDevice,
    {
        // All directories on FAT32 have a cluster chain but the root
        // dir starts in a specified cluster.
        let mut current_cluster = match dir_info.cluster {
            ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
            _ => Some(dir_info.cluster),
        };
        while let Some(cluster) = current_cluster {
            let start_block_idx = self.cluster_to_block(cluster);
            for block_idx in start_block_idx.range(BlockCount(u32::from(self.blocks_per_cluster))) {
                trace!("Reading FAT");
                let block = block_cache.read(block_idx).map_err(Error::DeviceError)?;
                for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                    if dir_entry.is_end() {
                        // Can quit early
                        return Ok(());
                    } else if dir_entry.is_valid() {
                        // Safe, since Block::LEN always fits on a u32
                        let start = (i * OnDiskDirEntry::LEN) as u32;
                        let entry = dir_entry.get_entry(FatType::Fat32, block_idx, start);
                        func(&entry, &dir_entry);
                    }
                }
            }
            current_cluster = match self.next_cluster(block_cache, cluster) {
                Ok(n) => Some(n),
                _ => None,
            };
        }
        Ok(())
    }

    /// Get an entry from the given directory
    pub(crate) fn find_directory_entry<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                // Root directories on FAT16 have a fixed size, because they use
                // a specially reserved space on disk (see
                // `first_root_dir_block`). Other directories can have any size
                // as they are made of regular clusters.
                let mut current_cluster = Some(dir_info.cluster);
                let mut first_dir_block_num = match dir_info.cluster {
                    ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
                    _ => self.cluster_to_block(dir_info.cluster),
                };
                let dir_size = match dir_info.cluster {
                    ClusterId::ROOT_DIR => {
                        let len_bytes =
                            u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                        BlockCount::from_bytes(len_bytes)
                    }
                    _ => BlockCount(u32::from(self.blocks_per_cluster)),
                };

                while let Some(cluster) = current_cluster {
                    for block in first_dir_block_num.range(dir_size) {
                        match self.find_entry_in_block(
                            block_cache,
                            FatType::Fat16,
                            match_name,
                            block,
                        ) {
                            Err(Error::NotFound) => continue,
                            x => return x,
                        }
                    }
                    if cluster != ClusterId::ROOT_DIR {
                        current_cluster = match self.next_cluster(block_cache, cluster) {
                            Ok(n) => {
                                first_dir_block_num = self.cluster_to_block(n);
                                Some(n)
                            }
                            _ => None,
                        };
                    } else {
                        current_cluster = None;
                    }
                }
                Err(Error::NotFound)
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                let mut current_cluster = match dir_info.cluster {
                    ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
                    _ => Some(dir_info.cluster),
                };
                while let Some(cluster) = current_cluster {
                    let block_idx = self.cluster_to_block(cluster);
                    for block in block_idx.range(BlockCount(u32::from(self.blocks_per_cluster))) {
                        match self.find_entry_in_block(
                            block_cache,
                            FatType::Fat32,
                            match_name,
                            block,
                        ) {
                            Err(Error::NotFound) => continue,
                            x => return x,
                        }
                    }
                    current_cluster = match self.next_cluster(block_cache, cluster) {
                        Ok(n) => Some(n),
                        _ => None,
                    }
                }
                Err(Error::NotFound)
            }
        }
    }

    /// Finds an entry in a given block of directory entries.
    fn find_entry_in_block<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        fat_type: FatType,
        match_name: &ShortFileName,
        block_idx: BlockIdx,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        trace!("Reading directory");
        let block = block_cache.read(block_idx).map_err(Error::DeviceError)?;
        for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
            if dir_entry.is_end() {
                // Can quit early
                break;
            } else if dir_entry.matches(match_name) {
                // Found it
                // Block::LEN always fits on a u32
                let start = (i * OnDiskDirEntry::LEN) as u32;
                return Ok(dir_entry.get_entry(fat_type, block_idx, start));
            }
        }
        Err(Error::NotFound)
    }

    /// Delete an entry from the given directory
    pub(crate) fn delete_directory_entry<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                // Root directories on FAT16 have a fixed size, because they use
                // a specially reserved space on disk (see
                // `first_root_dir_block`). Other directories can have any size
                // as they are made of regular clusters.
                let mut current_cluster = Some(dir_info.cluster);
                let mut first_dir_block_num = match dir_info.cluster {
                    ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
                    _ => self.cluster_to_block(dir_info.cluster),
                };
                let dir_size = match dir_info.cluster {
                    ClusterId::ROOT_DIR => {
                        let len_bytes =
                            u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                        BlockCount::from_bytes(len_bytes)
                    }
                    _ => BlockCount(u32::from(self.blocks_per_cluster)),
                };

                // Walk the directory
                while let Some(cluster) = current_cluster {
                    // Scan the cluster / root dir a block at a time
                    for block_idx in first_dir_block_num.range(dir_size) {
                        match self.delete_entry_in_block(block_cache, match_name, block_idx) {
                            Err(Error::NotFound) => {
                                // Carry on
                            }
                            x => {
                                // Either we deleted it OK, or there was some
                                // catastrophic error reading/writing the disk.
                                return x;
                            }
                        }
                    }
                    // if it's not the root dir, find the next cluster so we can keep looking
                    if cluster != ClusterId::ROOT_DIR {
                        current_cluster = match self.next_cluster(block_cache, cluster) {
                            Ok(n) => {
                                first_dir_block_num = self.cluster_to_block(n);
                                Some(n)
                            }
                            _ => None,
                        };
                    } else {
                        current_cluster = None;
                    }
                }
                // Ok, give up
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                // Root directories on FAT32 start at a specified cluster, but
                // they can have any length.
                let mut current_cluster = match dir_info.cluster {
                    ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
                    _ => Some(dir_info.cluster),
                };
                // Walk the directory
                while let Some(cluster) = current_cluster {
                    // Scan the cluster a block at a time
                    let start_block_idx = self.cluster_to_block(cluster);
                    for block_idx in
                        start_block_idx.range(BlockCount(u32::from(self.blocks_per_cluster)))
                    {
                        match self.delete_entry_in_block(block_cache, match_name, block_idx) {
                            Err(Error::NotFound) => {
                                // Carry on
                                continue;
                            }
                            x => {
                                // Either we deleted it OK, or there was some
                                // catastrophic error reading/writing the disk.
                                return x;
                            }
                        }
                    }
                    // Find the next cluster
                    current_cluster = match self.next_cluster(block_cache, cluster) {
                        Ok(n) => Some(n),
                        _ => None,
                    }
                }
                // Ok, give up
            }
        }
        // If we get here we never found the right entry in any of the
        // blocks that made up the directory
        Err(Error::NotFound)
    }

    /// Deletes a directory entry from a block of directory entries.
    ///
    /// Entries are marked as deleted by setting the first byte of the file name
    /// to a special value.
    fn delete_entry_in_block<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        match_name: &ShortFileName,
        block_idx: BlockIdx,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        trace!("Reading directory");
        let block = block_cache
            .read_mut(block_idx)
            .map_err(Error::DeviceError)?;
        for (i, dir_entry_bytes) in block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate() {
            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
            if dir_entry.is_end() {
                // Can quit early
                break;
            } else if dir_entry.matches(match_name) {
                let start = i * OnDiskDirEntry::LEN;
                // set first byte to the 'unused' marker
                block[start] = 0xE5;
                trace!("Updating directory");
                return block_cache.write_back().map_err(Error::DeviceError);
            }
        }
        Err(Error::NotFound)
    }

    /// Finds the next free cluster after the start_cluster and before end_cluster
    pub(crate) fn find_next_free_cluster<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        start_cluster: ClusterId,
        end_cluster: ClusterId,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let mut current_cluster = start_cluster;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                while current_cluster.0 < end_cluster.0 {
                    trace!(
                        "current_cluster={:?}, end_cluster={:?}",
                        current_cluster,
                        end_cluster
                    );
                    let fat_offset = current_cluster.0 * 2;
                    trace!("fat_offset = {:?}", fat_offset);
                    let this_fat_block_num =
                        self.lba_start + self.fat_start.offset_bytes(fat_offset);
                    trace!("this_fat_block_num = {:?}", this_fat_block_num);
                    let mut this_fat_ent_offset = usize::try_from(fat_offset % Block::LEN_U32)
                        .map_err(|_| Error::ConversionError)?;
                    trace!("Reading block {:?}", this_fat_block_num);
                    let block = block_cache
                        .read(this_fat_block_num)
                        .map_err(Error::DeviceError)?;
                    while this_fat_ent_offset <= Block::LEN - 2 {
                        let fat_entry = LittleEndian::read_u16(
                            &block[this_fat_ent_offset..=this_fat_ent_offset + 1],
                        );
                        if fat_entry == 0 {
                            return Ok(current_cluster);
                        }
                        this_fat_ent_offset += 2;
                        current_cluster += 1;
                    }
                }
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                while current_cluster.0 < end_cluster.0 {
                    trace!(
                        "current_cluster={:?}, end_cluster={:?}",
                        current_cluster,
                        end_cluster
                    );
                    let fat_offset = current_cluster.0 * 4;
                    trace!("fat_offset = {:?}", fat_offset);
                    let this_fat_block_num =
                        self.lba_start + self.fat_start.offset_bytes(fat_offset);
                    trace!("this_fat_block_num = {:?}", this_fat_block_num);
                    let mut this_fat_ent_offset = usize::try_from(fat_offset % Block::LEN_U32)
                        .map_err(|_| Error::ConversionError)?;
                    trace!("Reading block {:?}", this_fat_block_num);
                    let block = block_cache
                        .read(this_fat_block_num)
                        .map_err(Error::DeviceError)?;
                    while this_fat_ent_offset <= Block::LEN - 4 {
                        let fat_entry = LittleEndian::read_u32(
                            &block[this_fat_ent_offset..=this_fat_ent_offset + 3],
                        ) & 0x0FFF_FFFF;
                        if fat_entry == 0 {
                            return Ok(current_cluster);
                        }
                        this_fat_ent_offset += 4;
                        current_cluster += 1;
                    }
                }
            }
        }
        warn!("Out of space...");
        Err(Error::NotEnoughSpace)
    }

    /// Tries to allocate a cluster
    pub(crate) fn alloc_cluster<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        prev_cluster: Option<ClusterId>,
        zero: bool,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        debug!("Allocating new cluster, prev_cluster={:?}", prev_cluster);
        let end_cluster = ClusterId(self.cluster_count + RESERVED_ENTRIES);
        let start_cluster = match self.next_free_cluster {
            Some(cluster) if cluster.0 < end_cluster.0 => cluster,
            _ => ClusterId(RESERVED_ENTRIES),
        };
        trace!(
            "Finding next free between {:?}..={:?}",
            start_cluster,
            end_cluster
        );
        let new_cluster = match self.find_next_free_cluster(block_cache, start_cluster, end_cluster)
        {
            Ok(cluster) => cluster,
            Err(_) if start_cluster.0 > RESERVED_ENTRIES => {
                debug!(
                    "Retrying, finding next free between {:?}..={:?}",
                    ClusterId(RESERVED_ENTRIES),
                    end_cluster
                );
                self.find_next_free_cluster(block_cache, ClusterId(RESERVED_ENTRIES), end_cluster)?
            }
            Err(e) => return Err(e),
        };
        // This new cluster is the end of the file's chain
        self.update_fat(block_cache, new_cluster, ClusterId::END_OF_FILE)?;
        // If there's something before this new one, update the FAT to point it at us
        if let Some(cluster) = prev_cluster {
            trace!(
                "Updating old cluster {:?} to {:?} in FAT",
                cluster,
                new_cluster
            );
            self.update_fat(block_cache, cluster, new_cluster)?;
        }
        trace!(
            "Finding next free between {:?}..={:?}",
            new_cluster,
            end_cluster
        );
        self.next_free_cluster =
            match self.find_next_free_cluster(block_cache, new_cluster, end_cluster) {
                Ok(cluster) => Some(cluster),
                Err(_) if new_cluster.0 > RESERVED_ENTRIES => {
                    match self.find_next_free_cluster(
                        block_cache,
                        ClusterId(RESERVED_ENTRIES),
                        end_cluster,
                    ) {
                        Ok(cluster) => Some(cluster),
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            };
        debug!("Next free cluster is {:?}", self.next_free_cluster);
        // Record that we've allocated a cluster
        if let Some(ref mut number_free_cluster) = self.free_clusters_count {
            *number_free_cluster -= 1;
        };
        if zero {
            let start_block_idx = self.cluster_to_block(new_cluster);
            let num_blocks = BlockCount(u32::from(self.blocks_per_cluster));
            for block_idx in start_block_idx.range(num_blocks) {
                trace!("Zeroing cluster {:?}", block_idx);
                let _block = block_cache.blank_mut(block_idx);
                block_cache.write_back()?;
            }
        }
        debug!("All done, returning {:?}", new_cluster);
        Ok(new_cluster)
    }

    /// LOCAL ADDITION (vendored — see VENDOR.md). Allocate a whole
    /// cluster chain in one pass over the FAT, instead of one
    /// `alloc_cluster` call (with its per-cluster rescans and
    /// per-entry FAT writes) per cluster.
    ///
    /// `head` is an optional existing single-cluster, EOF-terminated
    /// chain to extend (the cluster `Mode::ReadWriteTruncate`
    /// retains); `None` allocates a fresh chain. `total_clusters` is
    /// the length the whole chain must have, *including* `head`.
    /// Returns the chain's head cluster.
    ///
    /// Each touched FAT sector is read and written once (duplicated to
    /// the second FAT like `update_fat`), plus one re-patch per sector
    /// transition for the link that crosses it. Claimed entries are
    /// written END_OF_FILE before anything links to them, so the
    /// on-disk chain reachable from `head` is EOF-terminated at every
    /// intermediate state — a power cut mid-call can orphan a few
    /// claimed clusters but never leaves the file's chain pointing at
    /// a free entry.
    ///
    /// On exhaustion (a full scan plus one wrap finds too few free
    /// entries) the newly claimed clusters are freed again and `head`
    /// re-terminated before returning `Err(NotEnoughSpace)`.
    pub(crate) fn alloc_cluster_chain<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        head: Option<ClusterId>,
        total_clusters: u32,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let needed = total_clusters - u32::from(head.is_some());
        if needed == 0 {
            // Nothing to allocate: only valid when extending a head to
            // a 1-cluster chain (total_clusters == 0 is a caller bug).
            return head.ok_or(Error::NotEnoughSpace);
        }
        // Early-out on the FSInfo free count. It's a hint, so this can
        // refuse where a scan would succeed (stale undercount) — the
        // caller's fallback path still works in that case.
        if let Some(free) = self.free_clusters_count {
            if free < needed {
                return Err(Error::NotEnoughSpace);
            }
        }
        let fat32 = matches!(self.fat_specific_info, FatSpecificInfo::Fat32(_));
        let es: u32 = if fat32 { 4 } else { 2 };
        let end = self.cluster_count + RESERVED_ENTRIES;
        let mut cur = match self.next_free_cluster {
            Some(c) if c.0 < end => c,
            _ => ClusterId(RESERVED_ENTRIES),
        };
        let mut wrapped = false;
        let mut prev = head;
        let mut first_new: Option<ClusterId> = None;
        let mut last = ClusterId(RESERVED_ENTRIES);
        let mut allocated = 0u32;

        while allocated < needed {
            if cur.0 >= end {
                if wrapped {
                    // Exhausted after a full wrap: free what we
                    // claimed and re-terminate the head.
                    if let Some(mut c) = first_new {
                        loop {
                            let nxt = match self.next_cluster(block_cache, c) {
                                Ok(n) => Some(n),
                                Err(Error::EndOfFile) => None,
                                Err(e) => return Err(e),
                            };
                            self.update_fat(block_cache, c, ClusterId::EMPTY)?;
                            match nxt {
                                Some(n) => c = n,
                                None => break,
                            }
                        }
                    }
                    if let Some(h) = head {
                        self.update_fat(block_cache, h, ClusterId::END_OF_FILE)?;
                    }
                    warn!("Out of space...");
                    return Err(Error::NotEnoughSpace);
                }
                wrapped = true;
                cur = ClusterId(RESERVED_ENTRIES);
            }
            // Process the FAT sector containing `cur`, claiming as
            // many free entries as we still need.
            let sector_fat_offset = cur.0 * es;
            let this_fat_block_num =
                self.lba_start + self.fat_start.offset_bytes(sector_fat_offset);
            // The cross-sector link discovered while scanning this
            // sector (at most one: previous sector's last claim → this
            // sector's first claim, or the `head` link). Patched after
            // this sector is written back, so claimed entries hit the
            // disk as END_OF_FILE before anything points at them.
            let mut pending_link: Option<(ClusterId, ClusterId)> = None;
            let mut modified = false;
            {
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .map_err(Error::DeviceError)?;
                let mut ent = (sector_fat_offset % Block::LEN_U32) as usize;
                while ent + es as usize <= Block::LEN && cur.0 < end && allocated < needed {
                    let free = if fat32 {
                        LittleEndian::read_u32(&block[ent..ent + 4]) & 0x0FFF_FFFF == 0
                    } else {
                        LittleEndian::read_u16(&block[ent..ent + 2]) == 0
                    };
                    if free {
                        // Claim as END_OF_FILE (FAT32: preserve the
                        // reserved top nibble, as update_fat does).
                        if fat32 {
                            let existing = LittleEndian::read_u32(&block[ent..ent + 4]);
                            LittleEndian::write_u32(
                                &mut block[ent..ent + 4],
                                (existing & 0xF000_0000) | 0x0FFF_FFFF,
                            );
                        } else {
                            LittleEndian::write_u16(&mut block[ent..ent + 2], 0xFFFF);
                        }
                        modified = true;
                        if first_new.is_none() {
                            first_new = Some(cur);
                        }
                        if let Some(p) = prev {
                            let p_offset = p.0 * es;
                            let p_block_num =
                                self.lba_start + self.fat_start.offset_bytes(p_offset);
                            if p_block_num == this_fat_block_num {
                                let p_ent = (p_offset % Block::LEN_U32) as usize;
                                if fat32 {
                                    let existing =
                                        LittleEndian::read_u32(&block[p_ent..p_ent + 4]);
                                    LittleEndian::write_u32(
                                        &mut block[p_ent..p_ent + 4],
                                        (existing & 0xF000_0000) | (cur.0 & 0x0FFF_FFFF),
                                    );
                                } else {
                                    LittleEndian::write_u16(
                                        &mut block[p_ent..p_ent + 2],
                                        cur.0 as u16,
                                    );
                                }
                            } else {
                                pending_link = Some((p, cur));
                            }
                        }
                        prev = Some(cur);
                        last = cur;
                        allocated += 1;
                    }
                    cur += 1;
                    ent += es as usize;
                }
            }
            if modified {
                if let Some(second_fat_start) = self.second_fat_start {
                    let duplicate =
                        self.lba_start + second_fat_start.offset_bytes(sector_fat_offset);
                    block_cache.write_back_with_duplicate(duplicate)?;
                } else {
                    block_cache.write_back()?;
                }
            }
            if let Some((p, c)) = pending_link {
                self.update_fat(block_cache, p, c)?;
            }
        }

        if let Some(ref mut free) = self.free_clusters_count {
            *free -= needed;
        }
        // Search hint only; alloc_cluster / the next call here clamp
        // out-of-range values back to RESERVED_ENTRIES.
        self.next_free_cluster = Some(ClusterId(last.0 + 1));
        debug!(
            "Allocated {} cluster(s), head {:?}, last {:?}",
            allocated,
            head.or(first_new),
            last
        );
        head.or(first_new).ok_or(Error::NotEnoughSpace)
    }

    /// Marks the input cluster as an EOF and all the subsequent clusters in the chain as free
    pub(crate) fn truncate_cluster_chain<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        if cluster.0 < RESERVED_ENTRIES {
            // file doesn't have any valid cluster allocated, there is nothing to do
            return Ok(());
        }
        let mut next = {
            match self.next_cluster(block_cache, cluster) {
                Ok(n) => n,
                Err(Error::EndOfFile) => return Ok(()),
                Err(e) => return Err(e),
            }
        };
        if let Some(ref mut next_free_cluster) = self.next_free_cluster {
            if next_free_cluster.0 > next.0 {
                *next_free_cluster = next;
            }
        } else {
            self.next_free_cluster = Some(next);
        }
        self.update_fat(block_cache, cluster, ClusterId::END_OF_FILE)?;
        loop {
            match self.next_cluster(block_cache, next) {
                Ok(n) => {
                    self.update_fat(block_cache, next, ClusterId::EMPTY)?;
                    next = n;
                }
                Err(Error::EndOfFile) => {
                    self.update_fat(block_cache, next, ClusterId::EMPTY)?;
                    break;
                }
                Err(e) => return Err(e),
            }
            if let Some(ref mut number_free_cluster) = self.free_clusters_count {
                *number_free_cluster += 1;
            };
        }
        Ok(())
    }

    /// Writes a Directory Entry to the disk
    pub(crate) fn write_entry_to_disk<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        entry: &DirEntry,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        let fat_type = match self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => FatType::Fat16,
            FatSpecificInfo::Fat32(_) => FatType::Fat32,
        };
        trace!("Reading directory for update");
        let block = block_cache
            .read_mut(entry.entry_block)
            .map_err(Error::DeviceError)?;

        let start = usize::try_from(entry.entry_offset).map_err(|_| Error::ConversionError)?;
        block[start..start + 32].copy_from_slice(&entry.serialize(fat_type)[..]);

        trace!("Updating directory");
        block_cache.write_back().map_err(Error::DeviceError)?;
        Ok(())
    }

    /// Create a new directory.
    ///
    /// 1) Creates the directory entry in the parent
    /// 2) Allocates a new cluster to hold the new directory
    /// 3) Writes out the `.` and `..` entries in the new directory
    pub(crate) fn make_dir<D, T>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        time_source: &T,
        parent: ClusterId,
        sfn: ShortFileName,
        att: Attributes,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        let mut new_dir_entry_in_parent =
            self.write_new_directory_entry(block_cache, time_source, parent, sfn, att)?;
        if new_dir_entry_in_parent.cluster == ClusterId::EMPTY {
            new_dir_entry_in_parent.cluster = self.alloc_cluster(block_cache, None, false)?;
            // update the parent dir with the cluster of the new dir
            self.write_entry_to_disk(block_cache, &new_dir_entry_in_parent)?;
        }
        let new_dir_start_block = self.cluster_to_block(new_dir_entry_in_parent.cluster);
        debug!("Made new dir entry {:?}", new_dir_entry_in_parent);
        let now = time_source.get_timestamp();
        let fat_type = self.get_fat_type();
        // A blank block
        let block = block_cache.blank_mut(new_dir_start_block);
        // make the "." entry
        let dot_entry_in_child = DirEntry {
            name: crate::ShortFileName::this_dir(),
            mtime: now,
            ctime: now,
            attributes: att,
            // point at ourselves
            cluster: new_dir_entry_in_parent.cluster,
            size: 0,
            entry_block: new_dir_start_block,
            entry_offset: 0,
        };
        debug!("New dir has {:?}", dot_entry_in_child);
        let mut offset = 0;
        block[offset..offset + OnDiskDirEntry::LEN]
            .copy_from_slice(&dot_entry_in_child.serialize(fat_type)[..]);
        offset += OnDiskDirEntry::LEN;
        // make the ".." entry
        let dot_dot_entry_in_child = DirEntry {
            name: crate::ShortFileName::parent_dir(),
            mtime: now,
            ctime: now,
            attributes: att,
            // point at our parent
            cluster: if parent == ClusterId::ROOT_DIR {
                // indicate parent is root using Cluster(0)
                ClusterId::EMPTY
            } else {
                parent
            },
            size: 0,
            entry_block: new_dir_start_block,
            entry_offset: OnDiskDirEntry::LEN_U32,
        };
        debug!("New dir has {:?}", dot_dot_entry_in_child);
        block[offset..offset + OnDiskDirEntry::LEN]
            .copy_from_slice(&dot_dot_entry_in_child.serialize(fat_type)[..]);

        block_cache.write_back()?;

        for block_idx in new_dir_start_block
            .range(BlockCount(u32::from(self.blocks_per_cluster)))
            .skip(1)
        {
            let _block = block_cache.blank_mut(block_idx);
            block_cache.write_back()?;
        }

        Ok(())
    }
}

/// Load the boot parameter block from the start of the given partition and
/// determine if the partition contains a valid FAT16 or FAT32 file system.
pub fn parse_volume<D>(
    block_cache: &mut BlockCache<D>,
    lba_start: BlockIdx,
    num_blocks: BlockCount,
) -> Result<VolumeType, Error<D::Error>>
where
    D: BlockDevice,
    D::Error: core::fmt::Debug,
{
    trace!("Reading BPB");
    let block = block_cache.read(lba_start).map_err(Error::DeviceError)?;
    let bpb = Bpb::create_from_bytes(block).map_err(Error::FormatError)?;
    let fat_start = BlockCount(u32::from(bpb.reserved_block_count()));
    let second_fat_start = if bpb.num_fats() == 2 {
        Some(fat_start + BlockCount(bpb.fat_size()))
    } else {
        None
    };
    match bpb.fat_type {
        FatType::Fat16 => {
            if bpb.bytes_per_block() as usize != Block::LEN {
                return Err(Error::BadBlockSize(bpb.bytes_per_block()));
            }
            // FirstDataSector = BPB_ResvdSecCnt + (BPB_NumFATs * FATSz) + RootDirSectors;
            let root_dir_blocks = ((u32::from(bpb.root_entries_count()) * OnDiskDirEntry::LEN_U32)
                + (Block::LEN_U32 - 1))
                / Block::LEN_U32;
            let first_root_dir_block =
                fat_start + BlockCount(u32::from(bpb.num_fats()) * bpb.fat_size());
            let first_data_block = first_root_dir_block + BlockCount(root_dir_blocks);
            let volume = FatVolume {
                lba_start,
                num_blocks,
                name: VolumeName {
                    contents: bpb.volume_label(),
                },
                blocks_per_cluster: bpb.blocks_per_cluster(),
                first_data_block,
                fat_start,
                second_fat_start,
                free_clusters_count: None,
                next_free_cluster: None,
                cluster_count: bpb.total_clusters(),
                fat_specific_info: FatSpecificInfo::Fat16(Fat16Info {
                    root_entries_count: bpb.root_entries_count(),
                    first_root_dir_block,
                }),
            };
            Ok(VolumeType::Fat(volume))
        }
        FatType::Fat32 => {
            // FirstDataSector = BPB_ResvdSecCnt + (BPB_NumFATs * FATSz);
            let first_data_block =
                fat_start + BlockCount(u32::from(bpb.num_fats()) * bpb.fat_size());
            // Safe to unwrap since this is a Fat32 Type
            let info_location = bpb.fs_info_block().unwrap();
            let mut volume = FatVolume {
                lba_start,
                num_blocks,
                name: VolumeName {
                    contents: bpb.volume_label(),
                },
                blocks_per_cluster: bpb.blocks_per_cluster(),
                first_data_block,
                fat_start,
                second_fat_start,
                free_clusters_count: None,
                next_free_cluster: None,
                cluster_count: bpb.total_clusters(),
                fat_specific_info: FatSpecificInfo::Fat32(Fat32Info {
                    info_location: lba_start + info_location,
                    first_root_dir_cluster: ClusterId(bpb.first_root_dir_cluster()),
                }),
            };

            // Now we don't need the BPB, update the volume with data from the info sector
            trace!("Reading info block");
            let info_block = block_cache
                .read(lba_start + info_location)
                .map_err(Error::DeviceError)?;
            let info_sector =
                InfoSector::create_from_bytes(info_block).map_err(Error::FormatError)?;
            volume.free_clusters_count = info_sector.free_clusters_count();
            volume.next_free_cluster = info_sector.next_free_cluster();

            Ok(VolumeType::Fat(volume))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_name() {
        let sfn = VolumeName {
            contents: *b"Hello \xA399  ",
        };
        assert_eq!(sfn, VolumeName::create_from_str("Hello £99").unwrap())
    }
}

// ****************************************************************************
//
// Unit Tests
//
// ****************************************************************************

/// LOCAL ADDITION (vendored — see VENDOR.md): host-side tests for
/// `alloc_cluster_chain`. Run from the crate directory with an explicit
/// host target (the repo-level `.cargo/config.toml` pins a bare-metal
/// target), e.g. `cargo test --target aarch64-apple-darwin`.
#[cfg(test)]
mod alloc_chain_tests {
    use super::*;
    use std::cell::RefCell;
    use std::vec::Vec;

    /// In-memory block device, enough to host a hand-built FAT.
    struct MemDevice(RefCell<Vec<Block>>);

    impl MemDevice {
        fn new(num_blocks: usize) -> Self {
            MemDevice(RefCell::new(vec![Block::new(); num_blocks]))
        }
    }

    #[derive(Debug)]
    struct MemError;

    impl BlockDevice for MemDevice {
        type Error = MemError;
        fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), MemError> {
            let store = self.0.borrow();
            for (i, b) in blocks.iter_mut().enumerate() {
                *b = store[start.0 as usize + i].clone();
            }
            Ok(())
        }
        fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), MemError> {
            let mut store = self.0.borrow_mut();
            for (i, b) in blocks.iter().enumerate() {
                store[start.0 as usize + i] = b.clone();
            }
            Ok(())
        }
        fn num_blocks(&self) -> Result<BlockCount, MemError> {
            Ok(BlockCount(self.0.borrow().len() as u32))
        }
    }

    /// Test layout: FAT 1 at blocks 1..=2, FAT 2 at blocks 3..=4,
    /// data area from block 5. One block per cluster.
    const FAT_BLOCKS: u32 = 2;

    fn test_volume(fat32: bool, cluster_count: u32, free: Option<u32>) -> FatVolume {
        FatVolume {
            lba_start: BlockIdx(0),
            num_blocks: BlockCount(64),
            name: VolumeName {
                contents: *b"UNITTEST   ",
            },
            blocks_per_cluster: 1,
            first_data_block: BlockCount(1 + 2 * FAT_BLOCKS),
            fat_start: BlockCount(1),
            second_fat_start: Some(BlockCount(1 + FAT_BLOCKS)),
            free_clusters_count: free,
            next_free_cluster: None,
            cluster_count,
            fat_specific_info: if fat32 {
                FatSpecificInfo::Fat32(Fat32Info {
                    first_root_dir_cluster: ClusterId(2),
                    info_location: BlockIdx(0),
                })
            } else {
                FatSpecificInfo::Fat16(Fat16Info {
                    first_root_dir_block: BlockCount(0),
                    root_entries_count: 0,
                })
            },
        }
    }

    const F32_EOF: u32 = 0x0FFF_FFFF;
    const F16_EOF: u32 = 0xFFFF;

    /// Read a raw FAT entry from the first FAT copy.
    fn fat_entry(dev: &MemDevice, cluster: u32, fat32: bool) -> u32 {
        let es = if fat32 { 4 } else { 2 };
        let off = cluster as usize * es;
        let store = dev.0.borrow();
        let block = &store[1 + off / Block::LEN];
        let ent = off % Block::LEN;
        if fat32 {
            LittleEndian::read_u32(&block[ent..ent + 4]) & 0x0FFF_FFFF
        } else {
            u32::from(LittleEndian::read_u16(&block[ent..ent + 2]))
        }
    }

    /// Write a raw FAT entry to both FAT copies (pre-seeding helper).
    fn set_fat_entry(dev: &MemDevice, cluster: u32, value: u32, fat32: bool) {
        let es = if fat32 { 4 } else { 2 };
        let off = cluster as usize * es;
        let ent = off % Block::LEN;
        let mut store = dev.0.borrow_mut();
        for base in [1usize, 1 + FAT_BLOCKS as usize] {
            let block = &mut store[base + off / Block::LEN];
            if fat32 {
                LittleEndian::write_u32(&mut block[ent..ent + 4], value);
            } else {
                LittleEndian::write_u16(&mut block[ent..ent + 2], value as u16);
            }
        }
    }

    fn assert_fats_match(dev: &MemDevice) {
        let store = dev.0.borrow();
        for i in 0..FAT_BLOCKS as usize {
            assert_eq!(
                store[1 + i].contents,
                store[1 + FAT_BLOCKS as usize + i].contents,
                "FAT copies differ at sector {}",
                i
            );
        }
    }

    #[test]
    fn fat32_fresh_chain() {
        let dev = MemDevice::new(16);
        let mut cache = BlockCache::new(dev);
        let mut vol = test_volume(true, 200, Some(200));
        let head = vol
            .alloc_cluster_chain(&mut cache, None, 10)
            .expect("alloc");
        assert_eq!(head, ClusterId(2));
        let dev = cache.free();
        for c in 2..11u32 {
            assert_eq!(fat_entry(&dev, c, true), c + 1, "link {} broken", c);
        }
        assert_eq!(fat_entry(&dev, 11, true), F32_EOF);
        assert_eq!(fat_entry(&dev, 12, true), 0);
        assert_fats_match(&dev);
        assert_eq!(vol.free_clusters_count, Some(190));
        assert_eq!(vol.next_free_cluster, Some(ClusterId(12)));
    }

    #[test]
    fn fat32_fragmented_cross_sector() {
        let dev = MemDevice::new(16);
        // Use up clusters 2..=125 and 130. A FAT32 sector holds 128
        // entries, so the chain must cross the sector-0/1 boundary
        // (exercising the deferred cross-sector link) and skip 130.
        for c in 2..=125u32 {
            set_fat_entry(&dev, c, F32_EOF, true);
        }
        set_fat_entry(&dev, 130, F32_EOF, true);
        let mut cache = BlockCache::new(dev);
        let mut vol = test_volume(true, 200, None);
        let head = vol
            .alloc_cluster_chain(&mut cache, None, 6)
            .expect("alloc");
        assert_eq!(head, ClusterId(126));
        let dev = cache.free();
        assert_eq!(fat_entry(&dev, 126, true), 127);
        assert_eq!(fat_entry(&dev, 127, true), 128); // cross-sector link
        assert_eq!(fat_entry(&dev, 128, true), 129);
        assert_eq!(fat_entry(&dev, 129, true), 131); // skips used 130
        assert_eq!(fat_entry(&dev, 131, true), 132);
        assert_eq!(fat_entry(&dev, 132, true), F32_EOF);
        assert_eq!(fat_entry(&dev, 130, true), F32_EOF); // pre-used, untouched
        assert_fats_match(&dev);
    }

    #[test]
    fn fat32_extend_from_truncated_head() {
        let dev = MemDevice::new(16);
        // The head cluster a ReadWriteTruncate retains: EOF-terminated.
        set_fat_entry(&dev, 5, F32_EOF, true);
        let mut cache = BlockCache::new(dev);
        let mut vol = test_volume(true, 200, Some(199));
        let head = vol
            .alloc_cluster_chain(&mut cache, Some(ClusterId(5)), 4)
            .expect("alloc");
        assert_eq!(head, ClusterId(5));
        let dev = cache.free();
        assert_eq!(fat_entry(&dev, 5, true), 2); // head → first new
        assert_eq!(fat_entry(&dev, 2, true), 3);
        assert_eq!(fat_entry(&dev, 3, true), 4);
        assert_eq!(fat_entry(&dev, 4, true), F32_EOF);
        assert_fats_match(&dev);
        assert_eq!(vol.free_clusters_count, Some(196));
    }

    #[test]
    fn fat32_not_enough_space_rolls_back() {
        let dev = MemDevice::new(16);
        // 10 clusters (ids 2..=11); 3,5,7,9 pre-used; 11 is the head.
        // Only 2,4,6,8,10 are free but 7 are needed → exhaustion after
        // the wrap, and everything claimed must be released again.
        for c in [3u32, 5, 7, 9] {
            set_fat_entry(&dev, c, F32_EOF, true);
        }
        set_fat_entry(&dev, 11, F32_EOF, true);
        let mut cache = BlockCache::new(dev);
        let mut vol = test_volume(true, 10, None);
        let err = vol
            .alloc_cluster_chain(&mut cache, Some(ClusterId(11)), 8)
            .expect_err("must run out of space");
        assert!(matches!(err, Error::NotEnoughSpace));
        let dev = cache.free();
        for c in [2u32, 4, 6, 8, 10] {
            assert_eq!(fat_entry(&dev, c, true), 0, "cluster {} not freed", c);
        }
        for c in [3u32, 5, 7, 9] {
            assert_eq!(fat_entry(&dev, c, true), F32_EOF);
        }
        assert_eq!(fat_entry(&dev, 11, true), F32_EOF); // head re-terminated
        assert_fats_match(&dev);
    }

    #[test]
    fn fat16_fresh_chain_cross_sector() {
        let dev = MemDevice::new(16);
        let mut cache = BlockCache::new(dev);
        let mut vol = test_volume(false, 300, Some(300));
        // 260 clusters crosses the 256-entries-per-sector boundary.
        let head = vol
            .alloc_cluster_chain(&mut cache, None, 260)
            .expect("alloc");
        assert_eq!(head, ClusterId(2));
        let dev = cache.free();
        for c in 2..261u32 {
            assert_eq!(fat_entry(&dev, c, false), c + 1, "link {} broken", c);
        }
        assert_eq!(fat_entry(&dev, 261, false), F16_EOF);
        assert_eq!(fat_entry(&dev, 262, false), 0);
        assert_fats_match(&dev);
        assert_eq!(vol.free_clusters_count, Some(40));
        assert_eq!(vol.next_free_cluster, Some(ClusterId(262)));
    }
}

// ****************************************************************************
//
// End Of File
//
// ****************************************************************************
