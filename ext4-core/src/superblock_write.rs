use zerocopy::FromBytes;
use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::{Superblock, RawSuperblock};
use crate::journal::writer::Transaction;

#[derive(Debug, thiserror::Error)]
pub enum SuperblockWriteError {
    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),

    #[error("superblock buffer has wrong size")]
    InvalidBuffer,

    #[error("journal error: {0}")]
    Journal(#[from] crate::journal::JournalError),
}

/// Persist superblock changes (free block/inode counts, write time) to disk via the journal.
///
/// The superblock lives at byte offset 1024 (sectors 2-3). For journaling we
/// treat it as belonging to the filesystem block that contains it: block 0 for
/// block_size >= 2048, block 1 for 1 KiB blocks.
pub fn update_superblock(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    free_blocks: u64,
    free_inodes: u32,
    wtime: u32,
) -> Result<(), SuperblockWriteError> {
    // Which filesystem block holds the superblock, and where within it?
    // - 1 KiB blocks: superblock is in block 1 (byte offset 1024..2048), at offset 0 within block.
    // - >=2 KiB blocks: superblock is in block 0 (byte offset 0..block_size), at offset 1024 within block.
    let (sb_fs_block, sb_offset_in_block): (u64, usize) = if sb.block_size <= 1024 {
        (1, 0)
    } else {
        (0, 1024)
    };

    // Read the entire filesystem block so we don't clobber adjacent data.
    let sectors_per_block = sb.block_size as u64 / 512;
    let block_start_sector = sb_fs_block * sectors_per_block;
    let mut block_buf = read_sectors(dev, block_start_sector, sectors_per_block)?;

    let sb_end = sb_offset_in_block + core::mem::size_of::<RawSuperblock>();
    if block_buf.len() < sb_end {
        return Err(SuperblockWriteError::InvalidBuffer);
    }

    {
        let raw = RawSuperblock::mut_from(&mut block_buf[sb_offset_in_block..sb_end])
            .ok_or(SuperblockWriteError::InvalidBuffer)?;

        raw.s_free_blocks_count_lo.set(free_blocks as u32);
        raw.s_free_blocks_count_hi.set((free_blocks >> 32) as u32);
        raw.s_free_inodes_count.set(free_inodes);
    }

    // s_wtime is at byte offset 48 within the superblock (mtime=44, wtime=48).
    block_buf[sb_offset_in_block + 48..sb_offset_in_block + 52]
        .copy_from_slice(&wtime.to_le_bytes());

    txn.pin_block(sb_fs_block, block_buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::block_device::BlockDeviceError;
    use crate::journal::writer::Transaction;
    use zerocopy::{AsBytes, FromZeroes};
    use zerocopy::little_endian::{U16, U32};

    struct MemDev(Mutex<Vec<u8>>);

    impl BlockDevice for MemDev {
        fn read_sector(&self, idx: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            let data = self.0.lock().unwrap();
            let total = data.len() as u64 / 512;
            if idx >= total { return Err(BlockDeviceError::OutOfRange(idx, total)); }
            let off = idx as usize * 512;
            buf.copy_from_slice(&data[off..off + 512]);
            Ok(())
        }
        fn sector_count(&self) -> u64 { self.0.lock().unwrap().len() as u64 / 512 }
        fn write_sector(&self, idx: u64, buf: &[u8; 512]) -> Result<(), BlockDeviceError> {
            let mut data = self.0.lock().unwrap();
            let total = data.len() as u64 / 512;
            if idx >= total { return Err(BlockDeviceError::OutOfRange(idx, total)); }
            let off = idx as usize * 512;
            data[off..off + 512].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> Result<(), BlockDeviceError> { Ok(()) }
    }

    fn make_env() -> (MemDev, Superblock) {
        // Device must be large enough for block 0 (4 KiB = 8 sectors).
        let mut data = vec![0u8; 8 * 512];
        // Superblock lives at byte offset 1024 within block 0 for 4 KiB block size.
        let mut raw = RawSuperblock::new_zeroed();
        raw.s_magic = U16::new(0xEF53);
        raw.s_free_blocks_count_lo = U32::new(1000);
        raw.s_free_inodes_count = U32::new(200);
        let raw_bytes = raw.as_bytes();
        data[1024..1024 + raw_bytes.len()].copy_from_slice(raw_bytes);

        let sb = Superblock {
            block_size: 4096,
            blocks_count: 8192,
            inodes_count: 256,
            inodes_per_group: 256,
            blocks_per_group: 8192,
            first_data_block: 0,
            uuid: [0u8; 16],
            volume_name: String::new(),
            desc_size: 64,
            feature_incompat: 0,
            feature_ro_compat: 0,
            inode_size: 256,
            state: 0x0001,
        };
        (MemDev(Mutex::new(data)), sb)
    }

    #[test]
    fn update_superblock_pins_block() {
        let (dev, sb) = make_env();
        let mut txn = Transaction::new(1);
        update_superblock(&dev, &sb, &mut txn, 500, 100, 99999).unwrap();

        assert_eq!(txn.pinned_blocks().len(), 1);
        let (blk, data) = &txn.pinned_blocks()[0];
        assert_eq!(*blk, 0); // 4 KiB block size → superblock is in block 0

        // For block_size >= 2048, superblock sits at byte 1024 within the pinned block.
        let sb_offset = 1024usize;
        let sb_end = sb_offset + core::mem::size_of::<RawSuperblock>();
        let raw = RawSuperblock::read_from(&data[sb_offset..sb_end]).unwrap();
        assert_eq!(raw.s_free_blocks_count_lo.get(), 500u32);
        assert_eq!(raw.s_free_inodes_count.get(), 100u32);
    }
}
