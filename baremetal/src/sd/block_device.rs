//! `embedded_sdmmc::BlockDevice` impl for the BCM2835 SDHOST driver.
//!
//! The crate already knows how to parse MBR and FAT — we just need to
//! give it a way to read and write 512-byte sectors and a hint about
//! how many sectors the card has. Both come straight from
//! [`super::sdhost::SdHost`].

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, TimeSource, Timestamp};

use super::sdhost::{CmdError, SdHost};

impl BlockDevice for SdHost {
    type Error = CmdError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter_mut().enumerate() {
            let lba = start.0 + i as u32;
            self.read_block(lba, &mut block.contents)?;
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter().enumerate() {
            let lba = start.0 + i as u32;
            self.write_block(lba, &block.contents)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        // We don't decode CSD yet (CMD9 result is currently ignored),
        // so report the largest count embedded-sdmmc tolerates.
        // The crate's bounds checks fire against actual partition
        // sizes from the MBR, which is what we care about; the raw
        // card capacity only matters for whole-disk operations.
        Ok(BlockCount(u32::MAX))
    }
}

/// `TimeSource` that always reports a fixed epoch — we don't have a
/// real-time clock and FAT timestamps don't affect correctness of
/// read or write operations. Picks 2026-05-12 to match the rough
/// real-world date of bring-up. Constructed only by FAT consumers
/// (flash-persist-sd, sd-probe); allow on the default-build path.
#[allow(dead_code)]
pub struct NullTime;

impl TimeSource for NullTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56, // 2026
            zero_indexed_month: 4, // May
            zero_indexed_day: 11, // 12th
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}
