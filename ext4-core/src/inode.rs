use zerocopy::{FromBytes, FromZeroes, AsBytes};
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::journal::writer::Transaction;

pub mod mode {
    pub const S_IFMT:   u16 = 0xF000;
    pub const S_IFREG:  u16 = 0x8000;
    pub const S_IFDIR:  u16 = 0x4000;
    pub const S_IFLNK:  u16 = 0xA000;
    pub const S_IFBLK:  u16 = 0x6000;
    pub const S_IFCHR:  u16 = 0x2000;
    pub const S_IFIFO:  u16 = 0x1000;
    pub const S_IFSOCK: u16 = 0xC000;
}

#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct RawInode {
    pub i_mode:        U16,
    pub i_uid:         U16,
    pub i_size_lo:     U32,
    pub i_atime:       U32,
    pub i_ctime:       U32,
    pub i_mtime:       U32,
    pub i_dtime:       U32,
    pub i_gid:         U16,
    pub i_links_count: U16,
    pub i_blocks_lo:   U32,
    pub i_flags:       U32,
    pub _osd1:         [u8; 4],
    pub i_block:       [u8; 60],
    pub i_generation:  U32,
    pub i_file_acl_lo: U32,
    pub i_size_hi:     U32,
    pub i_obso_faddr:  U32,
    pub _osd2:         [u8; 12],
}

const _: () = assert!(core::mem::size_of::<RawInode>() == 128);

#[derive(Debug, Clone, PartialEq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct Inode {
    pub mode:        u16,
    pub uid:         u32,
    pub gid:         u32,
    pub size:        u64,
    pub atime:       u32,
    pub mtime:       u32,
    pub ctime:       u32,
    pub links_count: u16,
    pub flags:       u32,
    pub block_data:  [u8; 60],
}

impl Inode {
    pub fn file_type(&self) -> FileType {
        match self.mode & mode::S_IFMT {
            mode::S_IFREG  => FileType::Regular,
            mode::S_IFDIR  => FileType::Directory,
            mode::S_IFLNK  => FileType::Symlink,
            _              => FileType::Other,
        }
    }

    pub fn is_dir(&self) -> bool { self.file_type() == FileType::Directory }
    pub fn is_file(&self) -> bool { self.file_type() == FileType::Regular }
    pub fn is_symlink(&self) -> bool { self.file_type() == FileType::Symlink }
    pub fn uses_extents(&self) -> bool { self.flags & 0x80000 != 0 }
}

#[derive(Debug, thiserror::Error)]
pub enum InodeError {
    #[error("inode {0} is out of range (max {1})")]
    OutOfRange(u32, u32),

    #[error("inode {0} is unallocated (dtime != 0)")]
    Deleted(u32),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] crate::group_desc::GroupDescError),

    #[error("inode buffer has wrong size")]
    InvalidBuffer,
}

/// Load inode `inode_num` (1-based) from the filesystem.
pub fn read_inode(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    inode_num: u32,
) -> Result<Inode, InodeError> {
    if inode_num == 0 || inode_num > sb.inodes_count {
        return Err(InodeError::OutOfRange(inode_num, sb.inodes_count));
    }

    let group = (inode_num - 1) / sb.inodes_per_group;
    let index_in_group = (inode_num - 1) % sb.inodes_per_group;

    let desc = gdt.get(group as usize)?;
    let inode_table_block = desc.inode_table;

    let inode_size = sb.inode_size as u64;
    let offset_in_table = index_in_group as u64 * inode_size;

    let block_byte_offset = inode_table_block * sb.block_size as u64;
    let byte_offset = block_byte_offset + offset_in_table;

    let start_sector = byte_offset / 512;
    let offset_in_first_sector = (byte_offset % 512) as usize;
    let sectors_needed = (offset_in_first_sector as u64 + inode_size).div_ceil(512);

    let raw_bytes = read_sectors(dev, start_sector, sectors_needed)?;
    let end = offset_in_first_sector
        .checked_add(128)
        .filter(|&e| e <= raw_bytes.len())
        .ok_or(InodeError::InvalidBuffer)?;
    let inode_slice = &raw_bytes[offset_in_first_sector..end];

    let raw = RawInode::read_from(inode_slice).ok_or(InodeError::InvalidBuffer)?;

    if raw.i_dtime.get() != 0 {
        return Err(InodeError::Deleted(inode_num));
    }

    parse_raw(&raw, inode_num)
}

/// Read `inode_num` from the filesystem, but if the inode-table block is currently
/// pinned in `txn`, decode the inode from the pinned bytes instead of disk. Use
/// this whenever the caller is in the middle of a transaction that has already
/// modified the inode (e.g. between two `dir_add_entry` calls on the same dir).
pub fn read_inode_with_txn(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    txn: &Transaction,
    inode_num: u32,
) -> Result<Inode, InodeError> {
    if inode_num == 0 || inode_num > sb.inodes_count {
        return Err(InodeError::OutOfRange(inode_num, sb.inodes_count));
    }

    let group = (inode_num - 1) / sb.inodes_per_group;
    let index_in_group = (inode_num - 1) % sb.inodes_per_group;
    let desc = gdt.get(group as usize)?;
    let inode_size = sb.inode_size as usize;
    let inodes_per_block = sb.block_size as usize / inode_size;
    let block_index_in_table = (index_in_group as usize) / inodes_per_block;
    let phys_block = desc.inode_table + block_index_in_table as u64;
    let offset_in_block = ((index_in_group as usize) % inodes_per_block) * inode_size;

    if let Some((_, pinned)) = txn.pinned_blocks().iter().rev().find(|(b, _)| *b == phys_block) {
        let raw = RawInode::read_from(&pinned[offset_in_block..offset_in_block + 128])
            .ok_or(InodeError::InvalidBuffer)?;
        if raw.i_dtime.get() != 0 {
            return Err(InodeError::Deleted(inode_num));
        }
        return parse_raw(&raw, inode_num);
    }

    read_inode(dev, sb, gdt, inode_num)
}

fn parse_raw(raw: &RawInode, _inode_num: u32) -> Result<Inode, InodeError> {
    let uid_lo = raw.i_uid.get() as u32;
    let gid_lo = raw.i_gid.get() as u32;
    let uid_hi = u16::from_le_bytes([raw._osd2[4], raw._osd2[5]]) as u32;
    let gid_hi = u16::from_le_bytes([raw._osd2[6], raw._osd2[7]]) as u32;
    Ok(Inode {
        mode:        raw.i_mode.get(),
        uid:         uid_lo | (uid_hi << 16),
        gid:         gid_lo | (gid_hi << 16),
        size:        raw.i_size_lo.get() as u64 | ((raw.i_size_hi.get() as u64) << 32),
        atime:       raw.i_atime.get(),
        mtime:       raw.i_mtime.get(),
        ctime:       raw.i_ctime.get(),
        links_count: raw.i_links_count.get(),
        flags:       raw.i_flags.get(),
        block_data:  raw.i_block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::AsBytes;
    use crate::block_device::BlockDeviceError;
    use crate::group_desc::{GroupDesc, GroupDescTable};

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

    fn make_sb(inode_size: u16) -> Superblock {
        Superblock {
            block_size:        4096,
            blocks_count:      8192,
            inodes_count:      256,
            inodes_per_group:  256,
            blocks_per_group:  8192,
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size,
            state:             0x0001,
        }
    }

    /// Build a device with the inode table at block 5.
    fn make_device_with_inodes(
        inodes: &[(u32, RawInode)],
        inode_size: u16,
    ) -> (MemDevice, GroupDescTable) {
        let inode_table_block: u64 = 5;
        let mut data = vec![0u8; 40960]; // 10 × 4096 bytes
        for (idx, raw_inode) in inodes {
            let off = inode_table_block as usize * 4096 + *idx as usize * inode_size as usize;
            data[off..off + 128].copy_from_slice(raw_inode.as_bytes());
        }
        let dev = MemDevice(data);
        let groups = vec![GroupDesc {
            block_bitmap:      3,
            inode_bitmap:      4,
            inode_table:       inode_table_block,
            free_blocks_count: 0,
            free_inodes_count: 0,
            itable_unused:     0,
        }];
        let gdt = GroupDescTable::from_groups(groups);
        (dev, gdt)
    }

    fn make_raw_inode(mode: u16, size: u64, flags: u32, dtime: u32) -> RawInode {
        let mut r = RawInode::new_zeroed();
        r.i_mode        = U16::new(mode);
        r.i_size_lo     = U32::new(size as u32);
        r.i_size_hi     = U32::new((size >> 32) as u32);
        r.i_flags       = U32::new(flags);
        r.i_dtime       = U32::new(dtime);
        r.i_links_count = U16::new(1);
        r
    }

    #[test]
    fn read_root_inode() {
        // inode 2 (0-based index 1) is always root dir
        let raw = make_raw_inode(mode::S_IFDIR | 0o755, 4096, 0x80000, 0);
        let (dev, gdt) = make_device_with_inodes(&[(1, raw)], 256);
        let sb = make_sb(256);
        let inode = read_inode(&dev, &sb, &gdt, 2).unwrap();
        assert!(inode.is_dir());
        assert_eq!(inode.size, 4096);
    }

    #[test]
    fn read_regular_file_inode() {
        let size: u64 = (5u64 << 32) | 0xDEAD_BEEF;
        let raw = make_raw_inode(mode::S_IFREG | 0o644, size, 0x80000, 0);
        let (dev, gdt) = make_device_with_inodes(&[(0, raw)], 256);
        let sb = make_sb(256);
        let inode = read_inode(&dev, &sb, &gdt, 1).unwrap();
        assert!(inode.is_file());
        assert_eq!(inode.size, size);
    }

    #[test]
    fn inode_zero_invalid() {
        let (dev, gdt) = make_device_with_inodes(&[], 256);
        let sb = make_sb(256);
        let err = read_inode(&dev, &sb, &gdt, 0).unwrap_err();
        assert!(matches!(err, InodeError::OutOfRange(0, _)));
    }

    #[test]
    fn uses_extents_flag() {
        let raw = make_raw_inode(mode::S_IFREG, 0, 0x80000, 0);
        let (dev, gdt) = make_device_with_inodes(&[(0, raw)], 256);
        let sb = make_sb(256);
        let inode = read_inode(&dev, &sb, &gdt, 1).unwrap();
        assert!(inode.uses_extents());

        let raw2 = make_raw_inode(mode::S_IFREG, 0, 0, 0);
        let (dev2, gdt2) = make_device_with_inodes(&[(0, raw2)], 256);
        let inode2 = read_inode(&dev2, &sb, &gdt2, 1).unwrap();
        assert!(!inode2.uses_extents());
    }
}
