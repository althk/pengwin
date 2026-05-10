use zerocopy::FromBytes;
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::journal::writer::Transaction;
use crate::inode::RawInode;

#[derive(Debug, thiserror::Error)]
pub enum InodeWriteError {
    #[error("inode {0} is out of range (max {1})")]
    OutOfRange(u32, u32),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] crate::group_desc::GroupDescError),

    #[error("inode buffer has wrong size")]
    InvalidBuffer,

    #[error("journal error: {0}")]
    Journal(#[from] crate::journal::JournalError),
}

/// Describes which fields of an inode to update.
#[derive(Default, Clone)]
pub struct InodeUpdate {
    pub size:        Option<u64>,
    pub mode:        Option<u16>,
    pub uid:         Option<u32>,
    pub gid:         Option<u32>,
    pub atime:       Option<u32>,
    pub mtime:       Option<u32>,
    pub ctime:       Option<u32>,
    pub links_count: Option<u16>,
    pub flags:       Option<u32>,
}

impl InodeUpdate {
    pub fn with_size(mut self, v: u64) -> Self        { self.size = Some(v); self }
    pub fn with_mode(mut self, v: u16) -> Self        { self.mode = Some(v); self }
    pub fn with_uid(mut self, v: u32) -> Self         { self.uid = Some(v); self }
    pub fn with_gid(mut self, v: u32) -> Self         { self.gid = Some(v); self }
    pub fn with_atime(mut self, v: u32) -> Self       { self.atime = Some(v); self }
    pub fn with_mtime(mut self, v: u32) -> Self       { self.mtime = Some(v); self }
    pub fn with_ctime(mut self, v: u32) -> Self       { self.ctime = Some(v); self }
    pub fn with_links_count(mut self, v: u16) -> Self { self.links_count = Some(v); self }
    pub fn with_flags(mut self, v: u32) -> Self       { self.flags = Some(v); self }
}

/// Compute the (inode_table_block, byte_offset_within_block) for inode_num.
pub(crate) fn inode_location(
    sb: &Superblock,
    gdt: &GroupDescTable,
    inode_num: u32,
) -> Result<(u64, usize), InodeWriteError> {
    if inode_num == 0 || inode_num > sb.inodes_count {
        return Err(InodeWriteError::OutOfRange(inode_num, sb.inodes_count));
    }
    let group = (inode_num - 1) / sb.inodes_per_group;
    let index_in_group = (inode_num - 1) % sb.inodes_per_group;
    let desc = gdt.get(group as usize)?;
    let inode_table_block = desc.inode_table;
    let offset_in_table = index_in_group as usize * sb.inode_size as usize;
    Ok((inode_table_block, offset_in_table))
}


/// Update inode fields on disk inside a journal transaction.
pub fn update_inode(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    txn: &mut Transaction,
    inode_num: u32,
    update: InodeUpdate,
) -> Result<(), InodeWriteError> {
    let (inode_table_block, offset_in_table) = inode_location(sb, gdt, inode_num)?;

    let block_index_in_table = offset_in_table / sb.block_size as usize;
    let offset_within_block = offset_in_table % sb.block_size as usize;
    let phys_block = inode_table_block + block_index_in_table as u64;

    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = phys_block * sectors_per_block;

    // Prefer the txn's latest pin of this block (e.g. from a prior init_inode
    // or update_inode call in the same transaction); fall back to reading from
    // disk. Reading from disk while the block is pinned would silently revert
    // earlier in-txn modifications (e.g. a freshly init'd sibling inode).
    let mut block_data = match txn.pinned_blocks().iter().rev().find(|(b, _)| *b == phys_block) {
        Some((_, data)) => data.clone(),
        None => read_sectors(dev, start_sector, sectors_per_block)?,
    };

    let end = offset_within_block
        .checked_add(128)
        .filter(|&e| e <= block_data.len())
        .ok_or(InodeWriteError::InvalidBuffer)?;
    let raw = RawInode::mut_from(&mut block_data[offset_within_block..end])
        .ok_or(InodeWriteError::InvalidBuffer)?;

    if let Some(v) = update.size {
        raw.i_size_lo = U32::new(v as u32);
        raw.i_size_hi = U32::new((v >> 32) as u32);
    }
    if let Some(v) = update.mode        { raw.i_mode        = U16::new(v); }
    if let Some(v) = update.atime       { raw.i_atime       = U32::new(v); }
    if let Some(v) = update.mtime       { raw.i_mtime       = U32::new(v); }
    if let Some(v) = update.ctime       { raw.i_ctime       = U32::new(v); }
    if let Some(v) = update.links_count { raw.i_links_count = U16::new(v); }
    if let Some(v) = update.flags       { raw.i_flags       = U32::new(v); }
    if let Some(v) = update.uid {
        raw.i_uid = U16::new(v as u16);
        raw._osd2[4] = ((v >> 16) & 0xFF) as u8;
        raw._osd2[5] = ((v >> 24) & 0xFF) as u8;
    }
    if let Some(v) = update.gid {
        raw.i_gid = U16::new(v as u16);
        raw._osd2[6] = ((v >> 16) & 0xFF) as u8;
        raw._osd2[7] = ((v >> 24) & 0xFF) as u8;
    }

    txn.pin_block(phys_block, block_data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zerocopy::{AsBytes, FromZeroes};
    use zerocopy::little_endian::{U16, U32};
    use crate::block_device::BlockDeviceError;
    use crate::group_desc::{GroupDesc, GroupDescTable};
    use crate::superblock::Superblock;
    use crate::journal::writer::Transaction;

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

    fn make_env() -> (MemDev, Superblock, GroupDescTable) {
        // inode table at block 2, device is 10 blocks of 4096 bytes
        let mut data = vec![0u8; 10 * 4096];

        // Write a basic RawInode at inode_table_block 2, slot 0 (inode 1)
        let inode_table_off = 2 * 4096;
        let mut raw = RawInode::new_zeroed();
        raw.i_mode        = U16::new(0o100644);
        raw.i_size_lo     = U32::new(1024);
        raw.i_links_count = U16::new(1);
        raw.i_flags       = U32::new(0x80000);
        data[inode_table_off..inode_table_off + 128].copy_from_slice(raw.as_bytes());

        let groups = vec![GroupDesc {
            block_bitmap: 0,
            inode_bitmap: 1,
            inode_table: 2,
            free_blocks_count: 0,
            free_inodes_count: 0,
            itable_unused: 0,
        }];
        let sb = Superblock {
            block_size:        4096,
            blocks_count:      80,
            inodes_count:      256,
            inodes_per_group:  256,
            blocks_per_group:  8192,
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size:        256,
            state:             0x0001,
        };
        (MemDev(Mutex::new(data)), sb, GroupDescTable::from_groups(groups))
    }

    #[test]
    fn update_inode_size() {
        let (dev, sb, gdt) = make_env();
        let mut txn = Transaction::new(1);

        update_inode(
            &dev, &sb, &gdt, &mut txn,
            1,
            InodeUpdate::default().with_size(8192).with_mtime(12345),
        ).unwrap();

        // The pinned block should contain the updated inode.
        assert_eq!(txn.pinned_blocks().len(), 1);
        let (phys_block, block_data) = &txn.pinned_blocks()[0];
        assert_eq!(*phys_block, 2); // inode_table_block = 2

        // offset_within_block for inode 1 (index 0 in group) with inode_size=256 → 0
        let raw = RawInode::read_from(&block_data[0..128]).unwrap();
        assert_eq!(raw.i_size_lo.get(), 8192u32);
        assert_eq!(raw.i_mtime.get(), 12345u32);
    }

    #[test]
    fn update_inode_links_count() {
        let (dev, sb, gdt) = make_env();
        let mut txn = Transaction::new(1);

        update_inode(
            &dev, &sb, &gdt, &mut txn,
            1,
            InodeUpdate::default().with_links_count(3),
        ).unwrap();

        let (_, block_data) = &txn.pinned_blocks()[0];
        let raw = RawInode::read_from(&block_data[0..128]).unwrap();
        assert_eq!(raw.i_links_count.get(), 3u16);
    }

    /// Two updates in the same txn, targeting different inodes in the same table
    /// block, must both be visible after commit. Previously the second update
    /// re-read from disk, clobbering the first.
    #[test]
    fn updates_to_two_inodes_in_same_block_are_preserved() {
        let (dev, sb, gdt) = make_env();
        let mut txn = Transaction::new(1);

        // inode 1 lives at offset 0; pre-seed inode 2 at offset 256 (inode_size=256).
        {
            let inode_table_off = 2 * 4096 + 256;
            let mut data = dev.0.lock().unwrap();
            let mut raw = RawInode::new_zeroed();
            raw.i_mode = U16::new(0o100644);
            raw.i_links_count = U16::new(1);
            data[inode_table_off..inode_table_off + 128].copy_from_slice(raw.as_bytes());
        }

        update_inode(&dev, &sb, &gdt, &mut txn, 1,
            InodeUpdate::default().with_ctime(0xAAAA_AAAA)).unwrap();
        update_inode(&dev, &sb, &gdt, &mut txn, 2,
            InodeUpdate::default().with_ctime(0xBBBB_BBBB)).unwrap();

        // Both inodes should reflect their updates in the latest pinned copy.
        let (_, latest) = txn.pinned_blocks().last().unwrap();
        let raw1 = RawInode::read_from(&latest[0..128]).unwrap();
        let raw2 = RawInode::read_from(&latest[256..256 + 128]).unwrap();
        assert_eq!(raw1.i_ctime.get(), 0xAAAA_AAAA);
        assert_eq!(raw2.i_ctime.get(), 0xBBBB_BBBB);
    }
}
