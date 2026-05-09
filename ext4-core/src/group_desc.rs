use zerocopy::{FromBytes, FromZeroes, AsBytes};
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, BlockDeviceError, read_sectors};
use crate::superblock::Superblock;

// 64-byte group descriptor (ext4 with s_desc_size == 64).
// For 32-byte descriptors the hi fields are absent on disk; we zero-extend.
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct RawGroupDesc {
    pub bg_block_bitmap_lo:      U32,   // 0
    pub bg_inode_bitmap_lo:      U32,   // 4
    pub bg_inode_table_lo:       U32,   // 8
    pub bg_free_blocks_count_lo: U16,   // 12
    pub bg_free_inodes_count_lo: U16,   // 14
    pub bg_used_dirs_count_lo:   U16,   // 16
    pub bg_flags:                U16,   // 18
    pub bg_exclude_bitmap_lo:    U32,   // 20
    pub bg_block_bitmap_csum_lo: U16,   // 24
    pub bg_inode_bitmap_csum_lo: U16,   // 26
    pub bg_itable_unused_lo:     U16,   // 28
    pub bg_checksum:             U16,   // 30
    // 64-byte extension (zero when s_desc_size == 32)
    pub bg_block_bitmap_hi:      U32,   // 32
    pub bg_inode_bitmap_hi:      U32,   // 36
    pub bg_inode_table_hi:       U32,   // 40
    pub bg_free_blocks_count_hi: U16,   // 44
    pub bg_free_inodes_count_hi: U16,   // 46
    pub bg_used_dirs_count_hi:   U16,   // 48
    pub bg_itable_unused_hi:     U16,   // 50
    pub bg_exclude_bitmap_hi:    U32,   // 52
    pub bg_block_bitmap_csum_hi: U16,   // 56
    pub bg_inode_bitmap_csum_hi: U16,   // 58
    _reserved:                   U32,   // 60
}

const _: () = assert!(core::mem::size_of::<RawGroupDesc>() == 64);

#[derive(Debug, Clone)]
pub struct GroupDesc {
    pub block_bitmap:     u64,
    pub inode_bitmap:     u64,
    pub inode_table:      u64,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
}

fn combine32(lo: u32, hi: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

impl GroupDesc {
    fn from_raw(raw: &RawGroupDesc, desc_size: u16) -> Self {
        let (bitmap_hi, inode_bm_hi, table_hi, free_blocks_hi, free_inodes_hi) =
            if desc_size >= 64 {
                (
                    raw.bg_block_bitmap_hi.get(),
                    raw.bg_inode_bitmap_hi.get(),
                    raw.bg_inode_table_hi.get(),
                    raw.bg_free_blocks_count_hi.get() as u32,
                    raw.bg_free_inodes_count_hi.get() as u32,
                )
            } else {
                (0, 0, 0, 0, 0)
            };

        GroupDesc {
            block_bitmap:      combine32(raw.bg_block_bitmap_lo.get(), bitmap_hi),
            inode_bitmap:      combine32(raw.bg_inode_bitmap_lo.get(), inode_bm_hi),
            inode_table:       combine32(raw.bg_inode_table_lo.get(), table_hi),
            free_blocks_count: raw.bg_free_blocks_count_lo.get() as u32
                               | (free_blocks_hi << 16),
            free_inodes_count: raw.bg_free_inodes_count_lo.get() as u32
                               | (free_inodes_hi << 16),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GroupDescError {
    #[error("group index {0} out of range (have {1} groups)")]
    OutOfRange(usize, usize),

    #[error("inode number {0} is out of range")]
    InvalidInode(u32),

    #[error("block device error: {0}")]
    BlockDevice(#[from] BlockDeviceError),

    #[error("group descriptor buffer has wrong size")]
    InvalidBuffer,

    #[error("group count {0} exceeds maximum (filesystem is corrupt or too large)")]
    TooManyGroups(u64),

    #[error("group descriptor table size overflows address space")]
    TableSizeOverflow,
}

#[derive(Debug, Clone)]
pub struct GroupDescTable {
    groups: Vec<GroupDesc>,
}

impl GroupDescTable {
    /// Load all group descriptors from the device using the parsed superblock.
    pub fn load(dev: &dyn BlockDevice, sb: &Superblock) -> Result<Self, GroupDescError> {
        // blocks_per_group == 0 is validated in superblock::parse, but guard here too.
        let group_count_u64 = groups_count(sb)?;
        let group_count = usize::try_from(group_count_u64)
            .map_err(|_| GroupDescError::TooManyGroups(group_count_u64))?;

        // GDT starts at the block immediately after the superblock block.
        // s_first_data_block is 0 for 4K blocks, 1 for 1K blocks.
        let gdt_block = sb.first_data_block as u64 + 1;

        let desc_size = sb.desc_size as u64;
        let total_bytes = group_count_u64
            .checked_mul(desc_size)
            .ok_or(GroupDescError::TableSizeOverflow)?;

        // Convert block address to sector address.
        let sectors_per_block = sb.block_size as u64 / 512;
        let start_sector = gdt_block * sectors_per_block;
        let sector_count = total_bytes.div_ceil(512);

        let raw_bytes = read_sectors(dev, start_sector, sector_count)?;

        let mut groups = Vec::with_capacity(group_count);
        for i in 0..group_count {
            let off = i
                .checked_mul(sb.desc_size as usize)
                .ok_or(GroupDescError::TableSizeOverflow)?;
            // Always parse as 64-byte RawGroupDesc; bytes beyond desc_size remain zero.
            let mut buf = [0u8; 64];
            let len = (sb.desc_size as usize).min(64);
            let src_end = off.checked_add(len).ok_or(GroupDescError::TableSizeOverflow)?;
            if src_end > raw_bytes.len() {
                return Err(GroupDescError::InvalidBuffer);
            }
            buf[..len].copy_from_slice(&raw_bytes[off..src_end]);
            let raw = RawGroupDesc::read_from(buf.as_slice())
                .ok_or(GroupDescError::InvalidBuffer)?;
            groups.push(GroupDesc::from_raw(&raw, sb.desc_size));
        }

        Ok(GroupDescTable { groups })
    }

    /// Descriptor for the group containing `inode_num` (1-based).
    pub fn group_for_inode(&self, inode_num: u32, sb: &Superblock) -> Result<&GroupDesc, GroupDescError> {
        if inode_num == 0 {
            return Err(GroupDescError::InvalidInode(0));
        }
        let idx = (inode_num - 1) as usize / sb.inodes_per_group as usize;
        self.get(idx)
    }

    /// Descriptor by zero-based group index.
    pub fn get(&self, index: usize) -> Result<&GroupDesc, GroupDescError> {
        self.groups.get(index).ok_or(GroupDescError::OutOfRange(index, self.groups.len()))
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Construct a table directly from a pre-built list of descriptors (test / internal use).
    pub fn from_groups(groups: Vec<GroupDesc>) -> Self {
        GroupDescTable { groups }
    }

    /// Adjust `free_blocks_count` for a group by `delta` (positive = more free, negative = fewer).
    pub fn adjust_free_blocks(&mut self, group: usize, delta: i64) -> Result<(), GroupDescError> {
        let n = self.groups.len();
        let desc = self.groups.get_mut(group)
            .ok_or(GroupDescError::OutOfRange(group, n))?;
        desc.free_blocks_count = (desc.free_blocks_count as i64 + delta).max(0) as u32;
        Ok(())
    }

    /// Adjust `free_inodes_count` for a group by `delta`.
    pub fn adjust_free_inodes(&mut self, group: usize, delta: i64) -> Result<(), GroupDescError> {
        let n = self.groups.len();
        let desc = self.groups.get_mut(group)
            .ok_or(GroupDescError::OutOfRange(group, n))?;
        desc.free_inodes_count = (desc.free_inodes_count as i64 + delta).max(0) as u32;
        Ok(())
    }
}

fn groups_count(sb: &Superblock) -> Result<u64, GroupDescError> {
    // blocks_per_group == 0 is validated in superblock::parse; guard defensively here.
    if sb.blocks_per_group == 0 {
        return Err(GroupDescError::TooManyGroups(0));
    }
    let count = sb.blocks_count.div_ceil(sb.blocks_per_group as u64);
    // Ext4 supports at most 2^32 block groups; cap at a safe practical limit.
    if count > (1 << 21) {
        return Err(GroupDescError::TooManyGroups(count));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::AsBytes;
    use crate::block_device::BlockDeviceError;

    struct MemDevice(Vec<u8>);

    impl BlockDevice for MemDevice {
        fn read_sector(&self, idx: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            let total = self.0.len() as u64 / 512;
            if idx >= total {
                return Err(BlockDeviceError::OutOfRange(idx, total));
            }
            let off = idx as usize * 512;
            buf.copy_from_slice(&self.0[off..off + 512]);
            Ok(())
        }
        fn sector_count(&self) -> u64 { self.0.len() as u64 / 512 }
        fn write_sector(&self, _: u64, _: &[u8; 512]) -> Result<(), BlockDeviceError> {
            Err(BlockDeviceError::NotSupported("read-only test device"))
        }
        fn flush(&self) -> Result<(), BlockDeviceError> { Ok(()) }
    }

    /// Superblock for a 4K-block filesystem with one group.
    fn make_sb(blocks_per_group: u32, total_blocks: u64, inodes_per_group: u32) -> Superblock {
        Superblock {
            block_size:       4096,
            blocks_count:     total_blocks,
            inodes_count:     inodes_per_group,
            inodes_per_group,
            blocks_per_group,
            first_data_block: 0,   // 4K blocks → first_data_block = 0
            uuid:             [0u8; 16],
            volume_name:      String::new(),
            desc_size:        64,
            feature_incompat: 0,
            feature_ro_compat: 0,
            inode_size:       256,
            state:            0x0001,
        }
    }

    /// Device large enough to hold block 0 (superblock) and block 1 (GDT).
    /// GDT starts at sector 8 (block 1, with 4K block size = 8 sectors/block).
    fn make_device_with_gdt(descs: &[RawGroupDesc]) -> MemDevice {
        // Need at least 2 full 4K blocks = 8192 bytes = 16 sectors.
        let mut data = vec![0u8; 8192];
        // GDT at byte offset 4096 (block 1).
        let mut off = 4096usize;
        for d in descs {
            data[off..off + 64].copy_from_slice(d.as_bytes());
            off += 64;
        }
        MemDevice(data)
    }

    fn make_desc(block_bitmap: u32, inode_bitmap: u32, inode_table: u32,
                 free_blocks: u16, free_inodes: u16) -> RawGroupDesc {
        let mut d = RawGroupDesc::new_zeroed();
        d.bg_block_bitmap_lo      = U32::new(block_bitmap);
        d.bg_inode_bitmap_lo      = U32::new(inode_bitmap);
        d.bg_inode_table_lo       = U32::new(inode_table);
        d.bg_free_blocks_count_lo = U16::new(free_blocks);
        d.bg_free_inodes_count_lo = U16::new(free_inodes);
        d
    }

    #[test]
    fn load_single_group() {
        let raw = make_desc(10, 11, 12, 500, 200);
        let dev = make_device_with_gdt(&[raw]);
        let sb = make_sb(32768, 32768, 256);
        let gdt = GroupDescTable::load(&dev, &sb).unwrap();
        assert_eq!(gdt.group_count(), 1);
        let g = gdt.get(0).unwrap();
        assert_eq!(g.block_bitmap, 10);
        assert_eq!(g.inode_bitmap, 11);
        assert_eq!(g.inode_table, 12);
        assert_eq!(g.free_blocks_count, 500);
        assert_eq!(g.free_inodes_count, 200);
    }

    #[test]
    fn group_for_inode_first() {
        let raw = make_desc(10, 11, 12, 0, 0);
        let dev = make_device_with_gdt(&[raw]);
        let sb = make_sb(32768, 32768, 256);
        let gdt = GroupDescTable::load(&dev, &sb).unwrap();
        let g = gdt.group_for_inode(1, &sb).unwrap();
        assert_eq!(g.block_bitmap, 10);
    }

    #[test]
    fn group_for_inode_boundary() {
        // Two groups, 256 inodes each.
        let raw0 = make_desc(10, 11, 12, 0, 0);
        let raw1 = make_desc(20, 21, 22, 0, 0);
        let mut data = vec![0u8; 8192];
        data[4096..4160].copy_from_slice(raw0.as_bytes());
        data[4160..4224].copy_from_slice(raw1.as_bytes());
        let dev = MemDevice(data);
        let sb = make_sb(32768, 65536, 256);
        let gdt = GroupDescTable::load(&dev, &sb).unwrap();
        // Inode 256 is last inode of group 0 (0-based index 255 → group 0).
        let g0 = gdt.group_for_inode(256, &sb).unwrap();
        assert_eq!(g0.block_bitmap, 10);
        // Inode 257 is first inode of group 1.
        let g1 = gdt.group_for_inode(257, &sb).unwrap();
        assert_eq!(g1.block_bitmap, 20);
    }

    #[test]
    fn group_for_inode_invalid() {
        let raw = make_desc(10, 11, 12, 0, 0);
        let dev = make_device_with_gdt(&[raw]);
        let sb = make_sb(32768, 32768, 256);
        let gdt = GroupDescTable::load(&dev, &sb).unwrap();
        let err = gdt.group_for_inode(0, &sb).unwrap_err();
        assert!(matches!(err, GroupDescError::InvalidInode(0)));
    }

    #[test]
    fn zero_blocks_per_group_returns_error() {
        // blocks_per_group == 0 must not panic; groups_count returns an error.
        let sb = Superblock {
            block_size:        4096,
            blocks_count:      1024,
            inodes_count:      256,
            inodes_per_group:  256,
            blocks_per_group:  0, // invalid
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size:        256,
            state:             0x0001,
        };
        let dev = MemDevice(vec![0u8; 8192]);
        let err = GroupDescTable::load(&dev, &sb).unwrap_err();
        assert!(matches!(err, GroupDescError::TooManyGroups(_)));
    }

    #[test]
    fn out_of_range() {
        let raw = make_desc(10, 11, 12, 0, 0);
        let dev = make_device_with_gdt(&[raw]);
        let sb = make_sb(32768, 32768, 256);
        let gdt = GroupDescTable::load(&dev, &sb).unwrap();
        let err = gdt.get(1).unwrap_err();
        assert!(matches!(err, GroupDescError::OutOfRange(1, 1)));
    }
}
