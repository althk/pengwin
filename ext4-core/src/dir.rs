use crate::block_device::BlockDevice;
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::inode::Inode;
use crate::extent::{lookup_block, ExtentError};

pub mod dir_file_type {
    pub const UNKNOWN:  u8 = 0;
    pub const REG_FILE: u8 = 1;
    pub const DIR:      u8 = 2;
    pub const CHRDEV:   u8 = 3;
    pub const BLKDEV:   u8 = 4;
    pub const FIFO:     u8 = 5;
    pub const SOCK:     u8 = 6;
    pub const SYMLINK:  u8 = 7;
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub inode_num: u32,
    pub name:      String,
    pub file_type: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum DirError {
    #[error("inode is not a directory")]
    NotADirectory,

    #[error("directory entry record length is invalid: {0}")]
    InvalidRecLen(u16),

    #[error("extent error: {0}")]
    Extent(#[from] ExtentError),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),
}

/// Parse one directory entry from `buf` at `offset`.
/// Returns `(entry_or_none, bytes_consumed)`.
/// `entry_or_none` is None when inode_num == 0 (deleted entry).
fn parse_dirent(buf: &[u8], offset: usize) -> Result<Option<(DirEntry, usize)>, DirError> {
    let remaining = &buf[offset..];
    if remaining.len() < 8 {
        return Ok(None);
    }

    let inode_num = u32::from_le_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]);
    let rec_len   = u16::from_le_bytes([remaining[4], remaining[5]]);
    let name_len  = remaining[6] as usize;
    let file_type = remaining[7];

    if rec_len < 8 {
        return Err(DirError::InvalidRecLen(rec_len));
    }
    let rec = rec_len as usize;
    if offset + rec > buf.len() {
        return Err(DirError::InvalidRecLen(rec_len));
    }

    if inode_num == 0 {
        return Ok(Some((
            DirEntry { inode_num: 0, name: String::new(), file_type },
            rec,
        )));
    }

    let name_end = 8 + name_len.min(rec.saturating_sub(8));
    let name = String::from_utf8_lossy(&remaining[8..name_end]).into_owned();

    Ok(Some((DirEntry { inode_num, name, file_type }, rec)))
}

/// Read one block worth of directory entries, appending non-deleted entries into `out`.
fn parse_block_entries(block: &[u8], out: &mut Vec<DirEntry>) -> Result<(), DirError> {
    let mut pos = 0usize;
    while pos < block.len() {
        match parse_dirent(block, pos)? {
            None => break,
            Some((entry, consumed)) => {
                if entry.inode_num != 0 {
                    out.push(entry);
                }
                pos += consumed;
            }
        }
    }
    Ok(())
}

fn read_block(dev: &dyn BlockDevice, sb: &Superblock, block: u64) -> Result<Vec<u8>, DirError> {
    use crate::block_device::read_sectors;
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = block * sectors_per_block;
    let data = read_sectors(dev, start_sector, sectors_per_block)?;
    Ok(data)
}

/// Iterate all directory entries in a directory inode.
/// Skips deleted entries (inode_num == 0).
/// Includes "." and ".." — caller can filter.
pub fn read_dir(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    _gdt: &GroupDescTable,
    dir_inode: &Inode,
) -> Result<Vec<DirEntry>, DirError> {
    if !dir_inode.is_dir() {
        return Err(DirError::NotADirectory);
    }

    let block_count = dir_inode.size.div_ceil(sb.block_size as u64);
    let mut entries = Vec::new();

    for lblock in 0..block_count {
        let phys = lookup_block(dev, sb, dir_inode, lblock)?;
        if let Some(block_num) = phys {
            let block_data = read_block(dev, sb, block_num)?;
            parse_block_entries(&block_data, &mut entries)?;
        }
        // Hole blocks in a directory contribute nothing.
    }

    Ok(entries)
}

/// Find a directory entry by name. Returns inode number.
pub fn lookup(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    dir_inode: &Inode,
    name: &str,
) -> Result<Option<u32>, DirError> {
    let entries = read_dir(dev, sb, gdt, dir_inode)?;
    Ok(entries.into_iter().find(|e| e.name == name).map(|e| e.inode_num))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::little_endian::{U16, U32};
    use zerocopy::{AsBytes, FromZeroes};
    use crate::block_device::BlockDeviceError;
    use crate::inode::{Inode, mode};
    use crate::superblock::Superblock;
    use crate::group_desc::{GroupDesc, GroupDescTable};
    use crate::extent::{ExtentHeader, ExtentLeaf, EXTENT_MAGIC};

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
    }

    const BLOCK_SIZE: usize = 4096;

    fn make_sb() -> Superblock {
        Superblock {
            block_size:        BLOCK_SIZE as u32,
            blocks_count:      256,
            inodes_count:      256,
            inodes_per_group:  256,
            blocks_per_group:  256,
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size:        256,
        }
    }

    /// Build a 60-byte extent root with a single leaf mapping lblock 0 → phys_block.
    fn make_extent_root(phys_block: u64) -> [u8; 60] {
        let mut data = [0u8; 60];
        let mut hdr = ExtentHeader::new_zeroed();
        hdr.eh_magic   = U16::new(EXTENT_MAGIC);
        hdr.eh_entries = U16::new(1);
        hdr.eh_max     = U16::new(4);
        hdr.eh_depth   = U16::new(0);
        data[..12].copy_from_slice(hdr.as_bytes());

        let mut leaf = ExtentLeaf::new_zeroed();
        leaf.ee_block    = U32::new(0);
        leaf.ee_len      = U16::new(1);
        leaf.ee_start_hi = U16::new((phys_block >> 32) as u16);
        leaf.ee_start_lo = U32::new(phys_block as u32);
        data[12..24].copy_from_slice(leaf.as_bytes());
        data
    }

    fn make_dir_inode(block_data: [u8; 60], size: u64) -> Inode {
        Inode {
            mode:        mode::S_IFDIR,
            uid:         0, gid: 0,
            size,
            atime: 0, mtime: 0, ctime: 0,
            links_count: 2,
            flags:       0x80000, // extents
            block_data,
        }
    }

    fn make_file_inode(block_data: [u8; 60], size: u64) -> Inode {
        Inode {
            mode:        mode::S_IFREG,
            uid:         0, gid: 0,
            size,
            atime: 0, mtime: 0, ctime: 0,
            links_count: 1,
            flags:       0x80000,
            block_data,
        }
    }

    /// Write a dirent into `block` at `offset`. Returns bytes written.
    fn write_dirent(block: &mut [u8], offset: usize, inode: u32, name: &str, ftype: u8, rec_len: u16) -> usize {
        let bytes = name.as_bytes();
        block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
        block[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        block[offset + 6] = bytes.len() as u8;
        block[offset + 7] = ftype;
        block[offset + 8..offset + 8 + bytes.len()].copy_from_slice(bytes);
        rec_len as usize
    }

    /// Build a device with one directory block at physical block 1 containing the given entries.
    /// Each entry except the last has rec_len = round_up(8+name, 4); the last fills the block.
    fn make_device_with_dir(entries: &[(u32, &str, u8)]) -> (MemDevice, Superblock, GroupDescTable, Inode) {
        let sb = make_sb();
        let phys_block: u64 = 1;

        // Build the block data.
        let mut block = vec![0u8; BLOCK_SIZE];
        let mut pos = 0usize;
        for (i, (inum, name, ftype)) in entries.iter().enumerate() {
            let name_len = name.len();
            let rec_len = if i + 1 == entries.len() {
                (BLOCK_SIZE - pos) as u16
            } else {
                ((8 + name_len + 3) & !3) as u16
            };
            pos += write_dirent(&mut block, pos, *inum, name, *ftype, rec_len);
        }

        // Build device: needs at least phys_block+1 blocks.
        let mut device_data = vec![0u8; (phys_block as usize + 1) * BLOCK_SIZE];
        let start = phys_block as usize * BLOCK_SIZE;
        device_data[start..start + BLOCK_SIZE].copy_from_slice(&block);

        let dev = MemDevice(device_data);
        let groups = vec![GroupDesc { block_bitmap: 0, inode_bitmap: 0, inode_table: 0, free_blocks_count: 0, free_inodes_count: 0 }];
        let gdt = GroupDescTable::from_groups(groups);

        let block_data = make_extent_root(phys_block);
        let inode = make_dir_inode(block_data, BLOCK_SIZE as u64);

        (dev, sb, gdt, inode)
    }

    #[test]
    fn read_root_dir() {
        let entries = &[(2, ".", dir_file_type::DIR), (2, "..", dir_file_type::DIR), (11, "lost+found", dir_file_type::DIR)];
        let (dev, sb, gdt, inode) = make_device_with_dir(entries);
        let result = read_dir(&dev, &sb, &gdt, &inode).unwrap();
        let names: Vec<_> = result.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."));
        assert!(names.contains(&".."));
        assert!(names.contains(&"lost+found"));
    }

    #[test]
    fn lookup_existing() {
        let entries = &[(2, ".", dir_file_type::DIR), (2, "..", dir_file_type::DIR), (42, "readme.txt", dir_file_type::REG_FILE)];
        let (dev, sb, gdt, inode) = make_device_with_dir(entries);
        let inum = lookup(&dev, &sb, &gdt, &inode, "readme.txt").unwrap();
        assert_eq!(inum, Some(42));
    }

    #[test]
    fn lookup_missing() {
        let entries = &[(2, ".", dir_file_type::DIR), (2, "..", dir_file_type::DIR)];
        let (dev, sb, gdt, inode) = make_device_with_dir(entries);
        let result = lookup(&dev, &sb, &gdt, &inode, "nosuchfile").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn skip_deleted() {
        // inode_num == 0 means deleted
        let entries = &[(0, "ghost", dir_file_type::REG_FILE), (7, "alive", dir_file_type::REG_FILE)];
        let (dev, sb, gdt, inode) = make_device_with_dir(entries);
        let result = read_dir(&dev, &sb, &gdt, &inode).unwrap();
        assert!(result.iter().all(|e| e.inode_num != 0));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "alive");
    }

    #[test]
    fn not_a_directory() {
        let sb = make_sb();
        let groups = vec![GroupDesc { block_bitmap: 0, inode_bitmap: 0, inode_table: 0, free_blocks_count: 0, free_inodes_count: 0 }];
        let gdt = GroupDescTable::from_groups(groups);
        let inode = make_file_inode([0u8; 60], 0);
        let dev = MemDevice(vec![0u8; 512]);
        let err = read_dir(&dev, &sb, &gdt, &inode).unwrap_err();
        assert!(matches!(err, DirError::NotADirectory));
    }
}
