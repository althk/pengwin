use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::inode::{read_inode, FileType};
use crate::extent::{lookup_block, ExtentError};
use crate::alloc::Allocator;
use crate::journal::writer::Transaction;
use crate::inode_write::{update_inode, InodeUpdate, InodeWriteError};
use crate::extent_write::{extent_append, ExtentWriteError};

pub mod dir_file_type {
    pub use crate::dir::dir_file_type::*;
}

#[derive(Debug, thiserror::Error)]
pub enum DirWriteError {
    #[error("name '{0}' already exists in directory")]
    AlreadyExists(String),

    #[error("name '{0}' not found in directory")]
    NotFound(String),

    #[error("directory is not empty")]
    NotEmpty,

    #[error("hard link to directory is not allowed")]
    HardLinkToDirectory,

    #[error("hard link count would exceed maximum (65000)")]
    TooManyLinks,

    #[error("name too long (max 255 bytes)")]
    NameTooLong,

    #[error("inode write error: {0}")]
    InodeWrite(#[from] InodeWriteError),

    #[error("allocator error: {0}")]
    Alloc(#[from] crate::alloc::AllocError),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),

    #[error("journal error: {0}")]
    Journal(#[from] crate::journal::JournalError),

    #[error("inode error: {0}")]
    Inode(#[from] crate::inode::InodeError),

    #[error("extent error: {0}")]
    Extent(#[from] ExtentError),

    #[error("extent write error: {0}")]
    ExtentWrite(#[from] ExtentWriteError),

    #[error("directory block number overflows address space")]
    BlockNumberOverflow,

    #[error("directory size is too large (possible corruption)")]
    SizeTooLarge,

    #[error("corrupt directory: invalid record length {0}")]
    InvalidRecLen(u16),
}

const MAX_DIR_BLOCKS: u64 = 65536;

/// Minimum record length for a name of `name_len` bytes (4-byte aligned, 8-byte header).
fn min_rec_len(name_len: usize) -> u16 {
    ((8 + name_len + 3) & !3) as u16
}

fn read_block(dev: &dyn BlockDevice, sb: &Superblock, block: u64) -> Result<Vec<u8>, DirWriteError> {
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = block
        .checked_mul(sectors_per_block)
        .ok_or(DirWriteError::BlockNumberOverflow)?;
    let data = read_sectors(dev, start_sector, sectors_per_block)?;
    Ok(data)
}


/// Write a directory entry header+name into `buf` at `offset` with the given `rec_len`.
fn write_dirent(buf: &mut [u8], offset: usize, inode_num: u32, name: &str, file_type: u8, rec_len: u16) {
    let bytes = name.as_bytes();
    assert!(
        offset + 8 + bytes.len() <= buf.len(),
        "write_dirent: offset {} + entry len {} exceeds buffer len {}",
        offset, 8 + bytes.len(), buf.len()
    );
    buf[offset..offset + 4].copy_from_slice(&inode_num.to_le_bytes());
    buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
    buf[offset + 6] = bytes.len() as u8;
    buf[offset + 7] = file_type;
    buf[offset + 8..offset + 8 + bytes.len()].copy_from_slice(bytes);
}

/// Walk directory blocks of `dir_inode_num` calling `f` for each block.
/// `f` receives `(phys_block, block_data)` and returns `Ok(Some(T))` to stop and return a value,
/// `Ok(None)` to continue, or `Err(e)`.
fn walk_dir_blocks<T, F>(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    dir_inode_num: u32,
    mut f: F,
) -> Result<Option<T>, DirWriteError>
where
    F: FnMut(u64, Vec<u8>) -> Result<Option<T>, DirWriteError>,
{
    let inode = read_inode(dev, sb, gdt, dir_inode_num)?;
    let block_count = inode.size.div_ceil(sb.block_size as u64);
    if block_count > MAX_DIR_BLOCKS {
        return Err(DirWriteError::SizeTooLarge);
    }
    for lblock in 0..block_count {
        let phys = lookup_block(dev, sb, &inode, lblock)?;
        if let Some(phys_block) = phys {
            let data = read_block(dev, sb, phys_block)?;
            if let Some(result) = f(phys_block, data)? {
                return Ok(Some(result));
            }
        }
    }
    Ok(None)
}

/// Try to insert a new entry into an existing directory block.
/// Returns `Some(phys_block)` if insertion succeeded (caller must pin the block in txn).
fn try_insert_in_block(
    block_data: &mut Vec<u8>,
    name: &str,
    child_inode_num: u32,
    file_type: u8,
) -> Result<bool, DirWriteError> {
    let block_size = block_data.len();
    let needed = min_rec_len(name.len());
    let mut pos = 0usize;

    while pos < block_size {
        if pos + 8 > block_size {
            break;
        }
        let inode_num = u32::from_le_bytes([
            block_data[pos], block_data[pos + 1],
            block_data[pos + 2], block_data[pos + 3],
        ]);
        let rec_len = u16::from_le_bytes([block_data[pos + 4], block_data[pos + 5]]);
        let name_len = block_data[pos + 6] as usize;

        if rec_len < 8 {
            return Err(DirWriteError::InvalidRecLen(rec_len));
        }

        let is_last = pos + rec_len as usize >= block_size;

        if inode_num == 0 {
            // Deleted slot — reuse if big enough.
            if rec_len >= needed {
                write_dirent(block_data, pos, child_inode_num, name, file_type, rec_len);
                return Ok(true);
            }
        } else {
            // Live entry — check if there's slack at the end.
            let actual = min_rec_len(name_len);
            let slack = rec_len.saturating_sub(actual);
            if is_last && slack >= needed {
                // Shrink this entry's rec_len to actual, write new entry in the gap.
                let new_rec_len = rec_len - actual;
                block_data[pos + 4..pos + 6].copy_from_slice(&actual.to_le_bytes());
                let new_off = pos + actual as usize;
                write_dirent(block_data, new_off, child_inode_num, name, file_type, new_rec_len);
                return Ok(true);
            }
        }

        pos += rec_len as usize;
    }
    Ok(false)
}

/// Insert a new entry without checking for duplicates. Used internally by `dir_rename`
/// after the conflicting destination has already been removed (but only in the txn).
fn dir_insert_entry(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    dir_inode_num: u32,
    name: &str,
    child_inode_num: u32,
    file_type: u8,
) -> Result<(), DirWriteError> {
    // Try to fit in an existing block (read from disk — txn may have modified blocks,
    // but pin_block deduplicates so re-pinning with a modified copy is correct).
    let inode = read_inode(dev, sb, gdt, dir_inode_num)?;
    let block_count = inode.size.div_ceil(sb.block_size as u64);
    if block_count > MAX_DIR_BLOCKS {
        return Err(DirWriteError::SizeTooLarge);
    }

    for lblock in 0..block_count {
        let phys = lookup_block(dev, sb, &inode, lblock)?;
        if let Some(phys_block) = phys {
            // Prefer the already-pinned version of this block if available.
            let mut block_data = txn.pinned_blocks()
                .iter()
                .rev()
                .find(|(b, _)| *b == phys_block)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| read_block(dev, sb, phys_block).unwrap_or_default());
            if block_data.is_empty() {
                block_data = read_block(dev, sb, phys_block)?;
            }
            if try_insert_in_block(&mut block_data, name, child_inode_num, file_type)? {
                txn.pin_block(phys_block, block_data)?;
                return Ok(());
            }
        }
    }

    // No room — allocate a new block.
    let hint = if block_count > 0 {
        let last_lblock = block_count - 1;
        lookup_block(dev, sb, &inode, last_lblock)?.unwrap_or(0)
    } else {
        0
    };
    let new_phys_block = alloc.alloc_blocks(txn, 1, hint)?;

    let mut new_block = vec![0u8; sb.block_size as usize];
    write_dirent(&mut new_block, 0, child_inode_num, name, file_type, sb.block_size as u16);
    txn.pin_block(new_phys_block, new_block)?;

    extent_append(dev, sb, txn, dir_inode_num, new_phys_block, 1, alloc)?;

    let new_size = inode.size + sb.block_size as u64;
    update_inode(dev, sb, gdt, txn, dir_inode_num,
        InodeUpdate::default().with_size(new_size))?;

    Ok(())
}

/// Add a new directory entry (`name → child_inode_num`) to a directory inode.
pub fn dir_add_entry(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    dir_inode_num: u32,
    name: &str,
    child_inode_num: u32,
    file_type: u8,
) -> Result<(), DirWriteError> {
    if name.len() > 255 {
        return Err(DirWriteError::NameTooLong);
    }

    // Check for existing entry (reads from disk — appropriate for user-facing API).
    let existing = walk_dir_blocks(dev, sb, gdt, dir_inode_num, |_phys, block_data| {
        let mut pos = 0usize;
        while pos + 8 <= block_data.len() {
            let inum = u32::from_le_bytes([
                block_data[pos], block_data[pos + 1],
                block_data[pos + 2], block_data[pos + 3],
            ]);
            let rec_len = u16::from_le_bytes([block_data[pos + 4], block_data[pos + 5]]) as usize;
            if rec_len < 8 { break; }
            let name_len = block_data[pos + 6] as usize;
            if inum != 0 && name_len == name.len() {
                let entry_name = &block_data[pos + 8..pos + 8 + name_len.min(rec_len.saturating_sub(8))];
                if entry_name == name.as_bytes() {
                    return Ok(Some(()));
                }
            }
            pos += rec_len;
        }
        Ok(None)
    })?;
    if existing.is_some() {
        return Err(DirWriteError::AlreadyExists(name.to_owned()));
    }

    dir_insert_entry(dev, sb, gdt, alloc, txn, dir_inode_num, name, child_inode_num, file_type)
}

/// Remove a directory entry by name.
/// Returns the removed entry's inode number (caller may need it to decrement links_count).
pub fn dir_remove_entry(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    txn: &mut Transaction,
    dir_inode_num: u32,
    name: &str,
) -> Result<u32, DirWriteError> {
    let inode = read_inode(dev, sb, gdt, dir_inode_num)?;
    let block_count = inode.size.div_ceil(sb.block_size as u64);
    if block_count > MAX_DIR_BLOCKS {
        return Err(DirWriteError::SizeTooLarge);
    }

    for lblock in 0..block_count {
        let phys = lookup_block(dev, sb, &inode, lblock)?;
        let Some(phys_block) = phys else { continue };
        // Prefer the already-pinned version so we operate on the txn's view of the block.
        let mut block_data = txn.pinned_blocks()
            .iter()
            .rev()
            .find(|(b, _)| *b == phys_block)
            .map(|(_, d)| d.clone())
            .unwrap_or(read_block(dev, sb, phys_block)?);
        let block_size = block_data.len();

        let mut pos = 0usize;
        let mut prev_end: Option<usize> = None; // byte where previous live entry's rec_len field is

        while pos + 8 <= block_size {
            let inum = u32::from_le_bytes([
                block_data[pos], block_data[pos + 1],
                block_data[pos + 2], block_data[pos + 3],
            ]);
            let rec_len = u16::from_le_bytes([block_data[pos + 4], block_data[pos + 5]]);
            if rec_len < 8 {
                return Err(DirWriteError::InvalidRecLen(rec_len));
            }
            let name_len = block_data[pos + 6] as usize;

            let is_match = inum != 0
                && name_len == name.len()
                && &block_data[pos + 8..pos + 8 + name_len] == name.as_bytes();

            if is_match {
                if let Some(prev_pos) = prev_end {
                    // Extend previous entry's rec_len to absorb this entry.
                    let prev_rec_len = u16::from_le_bytes([
                        block_data[prev_pos + 4],
                        block_data[prev_pos + 5],
                    ]);
                    let new_rec_len = prev_rec_len + rec_len;
                    block_data[prev_pos + 4..prev_pos + 6]
                        .copy_from_slice(&new_rec_len.to_le_bytes());
                } else {
                    // First entry in block — zero the inode number (mark deleted).
                    block_data[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes());
                }
                txn.pin_block(phys_block, block_data)?;
                return Ok(inum);
            }

            if inum != 0 {
                prev_end = Some(pos);
            }
            pos += rec_len as usize;
        }
    }

    Err(DirWriteError::NotFound(name.to_owned()))
}

/// Look up a directory entry by name, returning `(inode_num, file_type)`.
fn dir_lookup_entry(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    dir_inode_num: u32,
    name: &str,
) -> Result<Option<(u32, u8)>, DirWriteError> {
    let result = walk_dir_blocks(dev, sb, gdt, dir_inode_num, |_phys, block_data| {
        let block_size = block_data.len();
        let mut pos = 0usize;
        while pos + 8 <= block_size {
            let inum = u32::from_le_bytes([
                block_data[pos], block_data[pos + 1],
                block_data[pos + 2], block_data[pos + 3],
            ]);
            let rec_len = u16::from_le_bytes([block_data[pos + 4], block_data[pos + 5]]);
            if rec_len < 8 { break; }
            let name_len = block_data[pos + 6] as usize;
            let file_type = block_data[pos + 7];

            if inum != 0 && name_len == name.len()
                && &block_data[pos + 8..pos + 8 + name_len] == name.as_bytes()
            {
                return Ok(Some((inum, file_type)));
            }
            pos += rec_len as usize;
        }
        Ok(None)
    })?;
    Ok(result)
}

/// Check whether a directory is empty (contains only "." and "..").
fn dir_is_empty(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    dir_inode_num: u32,
) -> Result<bool, DirWriteError> {
    let non_dot = walk_dir_blocks(dev, sb, gdt, dir_inode_num, |_phys, block_data| {
        let block_size = block_data.len();
        let mut pos = 0usize;
        while pos + 8 <= block_size {
            let inum = u32::from_le_bytes([
                block_data[pos], block_data[pos + 1],
                block_data[pos + 2], block_data[pos + 3],
            ]);
            let rec_len = u16::from_le_bytes([block_data[pos + 4], block_data[pos + 5]]);
            if rec_len < 8 { break; }
            let name_len = block_data[pos + 6] as usize;
            if inum != 0 {
                let entry_name = &block_data[pos + 8..pos + 8 + name_len];
                if entry_name != b"." && entry_name != b".." {
                    return Ok(Some(()));
                }
            }
            pos += rec_len as usize;
        }
        Ok(None)
    })?;
    Ok(non_dot.is_none())
}

/// Update the ".." entry in a directory to point to a new parent.
fn update_dotdot(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    txn: &mut Transaction,
    dir_inode_num: u32,
    new_parent_inode: u32,
) -> Result<(), DirWriteError> {
    let inode = read_inode(dev, sb, gdt, dir_inode_num)?;
    let block_count = inode.size.div_ceil(sb.block_size as u64);

    for lblock in 0..block_count.min(MAX_DIR_BLOCKS) {
        let phys = lookup_block(dev, sb, &inode, lblock)?;
        let Some(phys_block) = phys else { continue };
        let mut block_data = read_block(dev, sb, phys_block)?;

        let mut pos = 0usize;
        while pos + 8 <= block_data.len() {
            let inum = u32::from_le_bytes([
                block_data[pos], block_data[pos + 1],
                block_data[pos + 2], block_data[pos + 3],
            ]);
            let rec_len = u16::from_le_bytes([block_data[pos + 4], block_data[pos + 5]]);
            if rec_len < 8 { break; }
            let name_len = block_data[pos + 6] as usize;
            if inum != 0 && name_len == 2 && &block_data[pos + 8..pos + 10] == b".." {
                block_data[pos..pos + 4].copy_from_slice(&new_parent_inode.to_le_bytes());
                txn.pin_block(phys_block, block_data)?;
                return Ok(());
            }
            pos += rec_len as usize;
        }
    }
    // ".." not found — not a critical error for the caller, silently succeed.
    Ok(())
}

/// Atomic rename within or across directories.
pub fn dir_rename(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    src_dir_inode: u32,
    src_name: &str,
    dst_dir_inode: u32,
    dst_name: &str,
) -> Result<(), DirWriteError> {
    // 1. Look up src_name.
    let (src_inode_num, src_file_type) = dir_lookup_entry(dev, sb, gdt, src_dir_inode, src_name)?
        .ok_or_else(|| DirWriteError::NotFound(src_name.to_owned()))?;

    let src_inode = read_inode(dev, sb, gdt, src_inode_num)?;
    let src_is_dir = src_inode.file_type() == FileType::Directory;

    // 2. Handle existing destination.
    if let Some((dst_inode_num, _)) = dir_lookup_entry(dev, sb, gdt, dst_dir_inode, dst_name)? {
        let dst_inode = read_inode(dev, sb, gdt, dst_inode_num)?;
        if dst_inode.file_type() == FileType::Directory {
            if !dir_is_empty(dev, sb, gdt, dst_inode_num)? {
                return Err(DirWriteError::NotEmpty);
            }
            dir_remove_entry(dev, sb, gdt, txn, dst_dir_inode, dst_name)?;
        } else {
            let old_links = dst_inode.links_count;
            dir_remove_entry(dev, sb, gdt, txn, dst_dir_inode, dst_name)?;
            if old_links > 1 {
                update_inode(dev, sb, gdt, txn, dst_inode_num,
                    InodeUpdate::default().with_links_count(old_links - 1))?;
            }
        }
    }

    // 3. Add new entry in dst_dir (use unchecked insert — dst already removed above).
    dir_insert_entry(dev, sb, gdt, alloc, txn, dst_dir_inode, dst_name, src_inode_num, src_file_type)?;

    // 4. Remove old entry from src_dir.
    dir_remove_entry(dev, sb, gdt, txn, src_dir_inode, src_name)?;

    // 5. If source is a directory and moved to a different parent, update "..".
    if src_is_dir && src_dir_inode != dst_dir_inode {
        update_dotdot(dev, sb, gdt, txn, src_inode_num, dst_dir_inode)?;
    }

    Ok(())
}

/// Create a hard link: add a new directory entry pointing to an existing inode.
pub fn hard_link(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    target_inode_num: u32,
    dst_dir_inode: u32,
    link_name: &str,
) -> Result<(), DirWriteError> {
    if link_name.len() > 255 {
        return Err(DirWriteError::NameTooLong);
    }

    let target = read_inode(dev, sb, gdt, target_inode_num)?;

    if target.file_type() == FileType::Directory {
        return Err(DirWriteError::HardLinkToDirectory);
    }

    if target.links_count >= 65000 {
        return Err(DirWriteError::TooManyLinks);
    }

    let file_type = match target.file_type() {
        FileType::Regular  => crate::dir::dir_file_type::REG_FILE,
        FileType::Symlink  => crate::dir::dir_file_type::SYMLINK,
        _                  => crate::dir::dir_file_type::UNKNOWN,
    };

    dir_add_entry(dev, sb, gdt, alloc, txn, dst_dir_inode, link_name, target_inode_num, file_type)?;

    update_inode(dev, sb, gdt, txn, target_inode_num,
        InodeUpdate::default().with_links_count(target.links_count + 1))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zerocopy::{AsBytes, FromBytes, FromZeroes};
    use crate::block_device::{BlockDeviceError, write_sectors};
    use crate::group_desc::{GroupDesc, GroupDescTable};
    use crate::superblock::Superblock;
    use crate::inode::{RawInode, mode};
    use crate::extent::{ExtentHeader, ExtentLeaf, EXTENT_MAGIC};
    use crate::journal::writer::Transaction;
    use zerocopy::little_endian::{U16, U32};

    fn write_block(dev: &MemDev, sb: &Superblock, block: u64, data: &[u8]) {
        let sectors_per_block = sb.block_size as u64 / 512;
        let start_sector = block * sectors_per_block;
        let mut padded = data.to_vec();
        padded.resize(sb.block_size as usize, 0);
        write_sectors(dev, start_sector, &padded).unwrap();
    }

    // ── MemDev ──────────────────────────────────────────────────────────────────

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

    // ── helpers ─────────────────────────────────────────────────────────────────

    const BLOCK_SIZE: usize = 4096;
    const DEVICE_BLOCKS: usize = 64;

    fn make_sb() -> Superblock {
        Superblock {
            block_size:        BLOCK_SIZE as u32,
            blocks_count:      DEVICE_BLOCKS as u64,
            inodes_count:      64,
            inodes_per_group:  64,
            blocks_per_group:  DEVICE_BLOCKS as u32,
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size:        256,
            state:             0x0001,
        }
    }

    fn make_gdt() -> GroupDescTable {
        GroupDescTable::from_groups(vec![GroupDesc {
            block_bitmap:      0,
            inode_bitmap:      1,
            inode_table:       2,
            free_blocks_count: (DEVICE_BLOCKS - 8) as u32,
            free_inodes_count: 60,
            itable_unused:     0,
        }])
    }

    /// Layout: block 0=block bitmap, 1=inode bitmap, 2-5=inode table, 6+= data.
    /// Block bitmap marks blocks 0-7 as used.
    fn make_device() -> (MemDev, Superblock) {
        let sb = make_sb();
        let mut data = vec![0u8; DEVICE_BLOCKS * BLOCK_SIZE];
        data[0] = 0xFF; // blocks 0-7 used in block bitmap
        (MemDev(Mutex::new(data)), sb)
    }

    fn write_inode(dev: &MemDev, sb: &Superblock, inode_num: u32, raw: &RawInode) {
        let inode_size = sb.inode_size as usize;
        let off = 2 * BLOCK_SIZE + (inode_num as usize - 1) * inode_size;
        let mut data = dev.0.lock().unwrap();
        data[off..off + 128].copy_from_slice(raw.as_bytes());
    }

    fn read_inode_raw(dev: &MemDev, sb: &Superblock, inode_num: u32) -> RawInode {
        let inode_size = sb.inode_size as usize;
        let off = 2 * BLOCK_SIZE + (inode_num as usize - 1) * inode_size;
        let data = dev.0.lock().unwrap();
        RawInode::read_from(&data[off..off + 128]).unwrap()
    }

    fn make_extent_root_one(phys_block: u64) -> [u8; 60] {
        let mut buf = [0u8; 60];
        let mut hdr = ExtentHeader::new_zeroed();
        hdr.eh_magic   = U16::new(EXTENT_MAGIC);
        hdr.eh_entries = U16::new(1);
        hdr.eh_max     = U16::new(4);
        hdr.eh_depth   = U16::new(0);
        buf[..12].copy_from_slice(hdr.as_bytes());

        let mut leaf = ExtentLeaf::new_zeroed();
        leaf.ee_block    = U32::new(0);
        leaf.ee_len      = U16::new(1);
        leaf.ee_start_hi = U16::new((phys_block >> 32) as u16);
        leaf.ee_start_lo = U32::new(phys_block as u32);
        buf[12..24].copy_from_slice(leaf.as_bytes());
        buf
    }

    fn make_raw_dir_inode(phys_block: u64, size: u64, links: u16) -> RawInode {
        let mut raw = RawInode::new_zeroed();
        raw.i_mode        = U16::new(mode::S_IFDIR | 0o755);
        raw.i_size_lo     = U32::new(size as u32);
        raw.i_size_hi     = U32::new((size >> 32) as u32);
        raw.i_flags       = U32::new(0x80000);
        raw.i_links_count = U16::new(links);
        raw.i_block       = make_extent_root_one(phys_block);
        raw
    }

    fn make_raw_file_inode(links: u16) -> RawInode {
        let mut raw = RawInode::new_zeroed();
        raw.i_mode        = U16::new(mode::S_IFREG | 0o644);
        raw.i_size_lo     = U32::new(42);
        raw.i_flags       = U32::new(0x80000);
        raw.i_links_count = U16::new(links);
        raw
    }

    fn write_dir_block(dev: &MemDev, phys_block: u64, entries: &[(u32, &str, u8)]) {
        let mut block = vec![0u8; BLOCK_SIZE];
        let mut pos = 0usize;
        for (i, (inum, name, ftype)) in entries.iter().enumerate() {
            let is_last = i + 1 == entries.len();
            let rec_len = if is_last {
                (BLOCK_SIZE - pos) as u16
            } else {
                min_rec_len(name.len())
            };
            write_dirent(&mut block, pos, *inum, name, *ftype, rec_len);
            pos += rec_len as usize;
        }
        let start = phys_block as usize * BLOCK_SIZE;
        let mut data = dev.0.lock().unwrap();
        data[start..start + BLOCK_SIZE].copy_from_slice(&block);
    }

    fn read_dir_block(dev: &MemDev, phys_block: u64) -> Vec<u8> {
        let start = phys_block as usize * BLOCK_SIZE;
        dev.0.lock().unwrap()[start..start + BLOCK_SIZE].to_vec()
    }

    fn parse_entries(block: &[u8]) -> Vec<(u32, String, u8)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= block.len() {
            let inum = u32::from_le_bytes([block[pos], block[pos+1], block[pos+2], block[pos+3]]);
            let rec_len = u16::from_le_bytes([block[pos+4], block[pos+5]]) as usize;
            let name_len = block[pos+6] as usize;
            let ftype = block[pos+7];
            if rec_len < 8 { break; }
            if inum != 0 {
                let name = String::from_utf8_lossy(&block[pos+8..pos+8+name_len]).into_owned();
                out.push((inum, name, ftype));
            }
            pos += rec_len;
        }
        out
    }

    // Apply pinned blocks back to the device for inspection.
    fn apply_txn(dev: &MemDev, sb: &Superblock, txn: &Transaction) {
        for (blk, data) in txn.pinned_blocks() {
            write_block(dev, sb, *blk, data);
        }
    }

    // ── tests ────────────────────────────────────────────────────────────────────

    #[test]
    fn add_entry_with_space() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        let mut gdt_alloc = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[
            (2, ".", dir_file_type::DIR),
            (2, "..", dir_file_type::DIR),
        ]);

        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt_alloc);
        dir_add_entry(&dev, &sb, &gdt, &mut alloc, &mut txn, 2, "hello", 7, dir_file_type::REG_FILE).unwrap();

        let pinned: Vec<_> = txn.pinned_blocks().iter().filter(|(b, _)| *b == 8).collect();
        assert_eq!(pinned.len(), 1);
        let entries = parse_entries(&pinned[0].1);
        assert!(entries.iter().any(|(_, n, _)| n == "hello"), "hello should be present");
    }

    #[test]
    fn add_entry_already_exists() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        let mut gdt_alloc = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[(2, ".", dir_file_type::DIR), (5, "foo", dir_file_type::REG_FILE)]);

        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt_alloc);
        let err = dir_add_entry(&dev, &sb, &gdt, &mut alloc, &mut txn, 2, "foo", 9, dir_file_type::REG_FILE).unwrap_err();
        assert!(matches!(err, DirWriteError::AlreadyExists(_)));
    }

    #[test]
    fn remove_entry_merges_rec_len() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[
            (2, ".", dir_file_type::DIR),
            (2, "..", dir_file_type::DIR),
            (5, "target", dir_file_type::REG_FILE),
        ]);

        let mut txn = Transaction::new(1);
        let removed = dir_remove_entry(&dev, &sb, &gdt, &mut txn, 2, "target").unwrap();
        assert_eq!(removed, 5);

        let pinned = &txn.pinned_blocks()[0].1;
        let entries = parse_entries(pinned);
        assert!(!entries.iter().any(|(_, n, _)| n == "target"));
    }

    #[test]
    fn remove_entry_first_in_block() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[(5, "only", dir_file_type::REG_FILE)]);

        let mut txn = Transaction::new(1);
        let removed = dir_remove_entry(&dev, &sb, &gdt, &mut txn, 2, "only").unwrap();
        assert_eq!(removed, 5);

        let pinned = &txn.pinned_blocks()[0].1;
        let inum = u32::from_le_bytes([pinned[0], pinned[1], pinned[2], pinned[3]]);
        assert_eq!(inum, 0, "first entry inode_num should be zeroed");
    }

    #[test]
    fn remove_entry_not_found() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[(2, ".", dir_file_type::DIR)]);

        let mut txn = Transaction::new(1);
        let err = dir_remove_entry(&dev, &sb, &gdt, &mut txn, 2, "nosuch").unwrap_err();
        assert!(matches!(err, DirWriteError::NotFound(_)));
    }

    #[test]
    fn rename_same_dir() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        let mut gdt_alloc = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[
            (2, ".", dir_file_type::DIR),
            (2, "..", dir_file_type::DIR),
            (5, "foo", dir_file_type::REG_FILE),
        ]);

        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt_alloc);
        dir_rename(&dev, &sb, &gdt, &mut alloc, &mut txn, 2, "foo", 2, "bar").unwrap();

        apply_txn(&dev, &sb, &txn);

        let block = read_dir_block(&dev, 8);
        let entries = parse_entries(&block);
        assert!(!entries.iter().any(|(_, n, _)| n == "foo"), "foo should be gone");
        assert!(entries.iter().any(|(_, n, _)| n == "bar"), "bar should be present");
        assert_eq!(entries.iter().find(|(_, n, _)| n == "bar").unwrap().0, 5);
    }

    #[test]
    fn rename_overwrites_existing() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        let mut gdt_alloc = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_inode(&dev, &sb, 5, &make_raw_file_inode(2));
        write_dir_block(&dev, 8, &[
            (2, ".", dir_file_type::DIR),
            (2, "..", dir_file_type::DIR),
            (3, "src", dir_file_type::REG_FILE),
            (5, "dst", dir_file_type::REG_FILE),
        ]);

        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt_alloc);
        dir_rename(&dev, &sb, &gdt, &mut alloc, &mut txn, 2, "src", 2, "dst").unwrap();

        apply_txn(&dev, &sb, &txn);

        let block = read_dir_block(&dev, 8);
        let entries = parse_entries(&block);
        assert!(!entries.iter().any(|(_, n, _)| n == "src"), "src should be gone");
        let dst_entry = entries.iter().find(|(_, n, _)| n == "dst");
        assert!(dst_entry.is_some(), "dst should exist");
        assert_eq!(dst_entry.unwrap().0, 3, "dst should point to src's inode");
    }

    #[test]
    fn hard_link_increments_count() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        let mut gdt_alloc = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[(2, ".", dir_file_type::DIR), (2, "..", dir_file_type::DIR)]);
        write_inode(&dev, &sb, 3, &make_raw_file_inode(1));

        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt_alloc);
        hard_link(&dev, &sb, &gdt, &mut alloc, &mut txn, 3, 2, "linkname").unwrap();

        apply_txn(&dev, &sb, &txn);

        let raw = read_inode_raw(&dev, &sb, 3);
        assert_eq!(raw.i_links_count.get(), 2u16, "links_count should be 2 after hard link");
    }

    #[test]
    fn hard_link_to_dir_rejected() {
        let (dev, sb) = make_device();
        let gdt = make_gdt();
        let mut gdt_alloc = make_gdt();
        write_inode(&dev, &sb, 2, &make_raw_dir_inode(8, BLOCK_SIZE as u64, 2));
        write_dir_block(&dev, 8, &[(2, ".", dir_file_type::DIR)]);
        write_inode(&dev, &sb, 4, &make_raw_dir_inode(9, BLOCK_SIZE as u64, 2));

        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt_alloc);
        let err = hard_link(&dev, &sb, &gdt, &mut alloc, &mut txn, 4, 2, "badlink").unwrap_err();
        assert!(matches!(err, DirWriteError::HardLinkToDirectory));
    }
}
