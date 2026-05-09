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
    // The superblock is always at byte 1024 = sectors 2-3, regardless of block size.
    // We read exactly 1024 bytes (2 sectors).
    let raw_bytes = read_sectors(dev, 2, 2)?;

    if raw_bytes.len() < core::mem::size_of::<RawSuperblock>() {
        return Err(SuperblockWriteError::InvalidBuffer);
    }

    let mut buf = raw_bytes;
    {
        let raw = RawSuperblock::mut_from(&mut buf[..core::mem::size_of::<RawSuperblock>()])
            .ok_or(SuperblockWriteError::InvalidBuffer)?;

        raw.s_free_blocks_count_lo.set(free_blocks as u32);
        raw.s_free_blocks_count_hi.set((free_blocks >> 32) as u32);
        raw.s_free_inodes_count.set(free_inodes);
    }

    // s_wtime is at byte offset 48 within the superblock (mtime=44, wtime=48).
    // Write directly into buf since _pad3 is private in RawSuperblock.
    buf[48..52].copy_from_slice(&wtime.to_le_bytes());

    // Which filesystem block holds the superblock?
    let sb_fs_block: u64 = if sb.block_size <= 1024 { 1 } else { 0 };
    txn.pin_block(sb_fs_block, buf)?;
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
        let mut data = vec![0u8; 8 * 512]; // 4 KiB
        // Write a minimal RawSuperblock at sectors 2-3 (byte 1024).
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

        let raw = RawSuperblock::read_from(&data[..core::mem::size_of::<RawSuperblock>()]).unwrap();
        assert_eq!(raw.s_free_blocks_count_lo.get(), 500u32);
        assert_eq!(raw.s_free_inodes_count.get(), 100u32);
    }
}
