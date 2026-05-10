pub mod block_bitmap;
pub mod inode_bitmap;
pub mod block_alloc;
pub mod inode_alloc;

use crate::block_device::BlockDevice;
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::journal::writer::Transaction;

#[derive(Debug, thiserror::Error)]
pub enum AllocError {
    #[error("filesystem is full — no free blocks")]
    NoFreeBlocks,

    #[error("no free inodes")]
    NoFreeInodes,

    #[error("block {0} is already free")]
    DoubleFreed(u64),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] crate::group_desc::GroupDescError),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),
}

pub struct Allocator<'a> {
    dev: &'a dyn BlockDevice,
    sb: &'a Superblock,
    gdt: &'a mut GroupDescTable,
}

impl<'a> Allocator<'a> {
    pub fn new(dev: &'a dyn BlockDevice, sb: &'a Superblock, gdt: &'a mut GroupDescTable) -> Self {
        Self { dev, sb, gdt }
    }

    pub fn alloc_blocks(&mut self, txn: &mut Transaction, count: u32, hint: u64)
        -> Result<u64, AllocError>
    {
        block_alloc::alloc_blocks(self.dev, self.sb, self.gdt, txn, count, hint)
    }

    pub fn free_block(&mut self, txn: &mut Transaction, block: u64)
        -> Result<(), AllocError>
    {
        block_alloc::free_block(self.dev, self.sb, self.gdt, txn, block)
    }

    pub fn alloc_inode(&mut self, txn: &mut Transaction, is_dir: bool, hint_group: usize)
        -> Result<u32, AllocError>
    {
        inode_alloc::alloc_inode(self.dev, self.sb, self.gdt, txn, is_dir, hint_group)
    }

    pub fn free_inode(&mut self, txn: &mut Transaction, inode_num: u32)
        -> Result<(), AllocError>
    {
        inode_alloc::free_inode(self.dev, self.sb, self.gdt, txn, inode_num)
    }

    pub fn gdt_ref(&self) -> &GroupDescTable { self.gdt }
    pub fn sb_ref(&self) -> &Superblock { self.sb }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDeviceError;
    use crate::group_desc::{GroupDesc, GroupDescTable};
    use crate::journal::writer::Transaction;
    use std::sync::Mutex;
    use zerocopy::FromBytes;
    use crate::inode::RawInode;

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

    // Blocks per group = 128, inode_size = 128.
    // Layout per group:
    //   block 0 = block_bitmap (1 block = 128 bits → covers 128 blocks)
    //   block 1 = inode_bitmap
    //   block 2 = inode_table (128 inodes × 128 bytes = 16 KiB = 4 blocks, but 1 block enough for tests)
    //   blocks 3..127 = data
    // Two groups so device = 256 × 4096 = 1 MiB.
    fn make_sb(blocks_per_group: u32, inodes_per_group: u32) -> Superblock {
        Superblock {
            block_size:       4096,
            blocks_count:     (blocks_per_group as u64) * 2,
            inodes_count:     inodes_per_group * 2,
            inodes_per_group,
            blocks_per_group,
            first_data_block: 0,
            uuid:             [0u8; 16],
            volume_name:      String::new(),
            desc_size:        64,
            feature_incompat: 0,
            feature_ro_compat: 0,
            inode_size:       128,
            state:            0x0001,
        }
    }

    fn make_gdt_and_dev(
        blocks_per_group: u32,
        inodes_per_group: u32,
        group0_bitmap_block: u64,
        group1_bitmap_block: u64,
        group0_inode_bitmap: u64,
        group1_inode_bitmap: u64,
        group0_inode_table: u64,
        group1_inode_table: u64,
    ) -> (MemDev, GroupDescTable) {
        let dev_size = (blocks_per_group as u64) * 2 * 4096;
        let raw = vec![0u8; dev_size as usize];
        let dev = MemDev(Mutex::new(raw));
        let groups = vec![
            GroupDesc {
                block_bitmap:      group0_bitmap_block,
                inode_bitmap:      group0_inode_bitmap,
                inode_table:       group0_inode_table,
                free_blocks_count: blocks_per_group,
                free_inodes_count: inodes_per_group,
                itable_unused:    0,
            },
            GroupDesc {
                block_bitmap:      group1_bitmap_block,
                inode_bitmap:      group1_inode_bitmap,
                inode_table:       group1_inode_table,
                free_blocks_count: blocks_per_group,
                free_inodes_count: inodes_per_group,
                itable_unused:    0,
            },
        ];
        (dev, GroupDescTable::from_groups(groups))
    }

    fn make_txn() -> Transaction {
        Transaction::new(1)
    }

    // ---------- block bitmap tests ----------

    #[test]
    fn alloc_and_free_block() {
        let (dev, mut gdt) = make_gdt_and_dev(128, 32, 0, 128, 1, 129, 2, 130);
        let sb = make_sb(128, 32);
        let mut txn = make_txn();

        let block = block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap();
        let group = (block / sb.blocks_per_group as u64) as usize;
        let bit = (block % sb.blocks_per_group as u64) as u32;
        let bm = block_bitmap::BlockBitmap::load(&dev, &sb, &gdt, group).unwrap();
        assert!(bm.is_allocated(bit));

        block_alloc::free_block(&dev, &sb, &mut gdt, &mut txn, block).unwrap();
        let bm2 = block_bitmap::BlockBitmap::load(&dev, &sb, &gdt, group).unwrap();
        assert!(!bm2.is_allocated(bit));
    }

    #[test]
    fn alloc_fills_group() {
        let blocks_per_group = 64u32;
        let (dev, mut gdt) = make_gdt_and_dev(blocks_per_group, 32, 0, 64, 1, 65, 2, 66);
        let sb = make_sb(blocks_per_group, 32);

        for _ in 0..blocks_per_group {
            let mut txn = make_txn();
            block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap();
        }
        // Group 0 full — next alloc should use group 1 or return NoFreeBlocks if group 1 is also full.
        // Since group 1 is free, it should succeed.
        let mut txn = make_txn();
        let result = block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0);
        assert!(result.is_ok(), "should fall back to group 1");
    }

    #[test]
    fn alloc_falls_back_to_next_group() {
        let blocks_per_group = 32u32;
        let (dev, mut gdt) = make_gdt_and_dev(blocks_per_group, 16, 0, 32, 1, 33, 2, 34);
        let sb = make_sb(blocks_per_group, 16);

        // Fill group 0 completely.
        for _ in 0..blocks_per_group {
            let mut txn = make_txn();
            block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap();
        }

        // Next alloc must come from group 1.
        let mut txn = make_txn();
        let block = block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap();
        assert!(block >= blocks_per_group as u64, "block should be in group 1");
    }

    #[test]
    fn double_free_block() {
        let (dev, mut gdt) = make_gdt_and_dev(128, 32, 0, 128, 1, 129, 2, 130);
        let sb = make_sb(128, 32);

        let mut txn = make_txn();
        let block = block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap();
        block_alloc::free_block(&dev, &sb, &mut gdt, &mut txn, block).unwrap();
        let err = block_alloc::free_block(&dev, &sb, &mut gdt, &mut txn, block).unwrap_err();
        assert!(matches!(err, AllocError::DoubleFreed(_)));
    }

    // ---------- inode tests ----------

    #[test]
    fn alloc_inode_sequential() {
        let (dev, mut gdt) = make_gdt_and_dev(128, 32, 0, 128, 1, 129, 2, 130);
        let sb = make_sb(128, 32);

        let mut txn = make_txn();
        let i1 = inode_alloc::alloc_inode(&dev, &sb, &mut gdt, &mut txn, false, 0).unwrap();
        let i2 = inode_alloc::alloc_inode(&dev, &sb, &mut gdt, &mut txn, false, 0).unwrap();
        let i3 = inode_alloc::alloc_inode(&dev, &sb, &mut gdt, &mut txn, false, 0).unwrap();

        assert!(i1 >= 1);
        assert_ne!(i1, i2);
        assert_ne!(i2, i3);
        // They should be sequential since we start from 0.
        assert_eq!(i2, i1 + 1);
        assert_eq!(i3, i1 + 2);
    }

    #[test]
    fn init_inode_fields() {
        let (dev, mut gdt) = make_gdt_and_dev(128, 32, 0, 128, 1, 129, 2, 130);
        let sb = make_sb(128, 32);

        let mut txn = make_txn();
        let inum = inode_alloc::alloc_inode(&dev, &sb, &mut gdt, &mut txn, false, 0).unwrap();
        inode_alloc::init_inode(&dev, &sb, &gdt, &mut txn, inum, 0o100644, 1000, 1000).unwrap();

        // Find the inode in the txn pinned blocks and verify fields.
        assert!(!txn.pinned_blocks().is_empty(), "init_inode must pin a block");
        // Read the inode back from txn block data.
        let group = ((inum - 1) / sb.inodes_per_group) as usize;
        let desc = gdt.get(group).unwrap();
        let inode_table_block = desc.inode_table;
        let pinned = txn.pinned_blocks().iter().find(|(b, _)| *b == inode_table_block).unwrap();
        let idx_in_group = ((inum - 1) % sb.inodes_per_group) as usize;
        let off = idx_in_group * sb.inode_size as usize;
        let raw = RawInode::read_from(&pinned.1[off..off + 128] as &[u8]).unwrap();
        assert_eq!(raw.i_mode.get(), 0o100644);
        assert_eq!(raw.i_uid.get(), 1000);
        assert_eq!(raw.i_links_count.get(), 1);
        assert_ne!(raw.i_ctime.get(), 0);
        // EXT4_EXTENTS_FL = 0x80000
        assert_ne!(raw.i_flags.get() & 0x80000, 0);
    }

    #[test]
    fn free_inode_sets_dtime() {
        let (dev, mut gdt) = make_gdt_and_dev(128, 32, 0, 128, 1, 129, 2, 130);
        let sb = make_sb(128, 32);

        let mut txn = make_txn();
        let inum = inode_alloc::alloc_inode(&dev, &sb, &mut gdt, &mut txn, false, 0).unwrap();
        inode_alloc::init_inode(&dev, &sb, &gdt, &mut txn, inum, 0o100644, 0, 0).unwrap();

        inode_alloc::free_inode(&dev, &sb, &mut gdt, &mut txn, inum).unwrap();

        let group = ((inum - 1) / sb.inodes_per_group) as usize;
        let desc = gdt.get(group).unwrap();
        let inode_table_block = desc.inode_table;
        // Find the most recent pin for this block (free_inode re-pins it).
        let pinned = txn.pinned_blocks().iter().rev().find(|(b, _)| *b == inode_table_block).unwrap();
        let idx_in_group = ((inum - 1) % sb.inodes_per_group) as usize;
        let off = idx_in_group * sb.inode_size as usize;
        let raw = RawInode::read_from(&pinned.1[off..off + 128] as &[u8]).unwrap();
        assert_ne!(raw.i_dtime.get(), 0, "i_dtime should be set after free_inode");
    }

    #[test]
    fn alloc_fails_when_disk_full() {
        let blocks_per_group = 32u32;
        let (dev, mut gdt) = make_gdt_and_dev(blocks_per_group, 16, 0, 32, 1, 33, 2, 34);
        let sb = make_sb(blocks_per_group, 16);

        // Fill every block in both groups.
        for _ in 0..blocks_per_group * 2 {
            let mut txn = make_txn();
            let _ = block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0);
        }

        // Next alloc must fail with NoFreeBlocks.
        let mut txn = make_txn();
        let err = block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap_err();
        assert!(matches!(err, AllocError::NoFreeBlocks), "expected NoFreeBlocks, got {err}");
    }

    #[test]
    fn alloc_journaled() {
        let (dev, mut gdt) = make_gdt_and_dev(128, 32, 0, 128, 1, 129, 2, 130);
        let sb = make_sb(128, 32);
        let mut txn = make_txn();

        block_alloc::alloc_blocks(&dev, &sb, &mut gdt, &mut txn, 1, 0).unwrap();
        assert!(!txn.pinned_blocks().is_empty(), "alloc_blocks must pin bitmap block into transaction");
    }
}
