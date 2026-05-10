use std::time::{SystemTime, UNIX_EPOCH};
use zerocopy::{FromBytes, FromZeroes};
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, read_sectors, write_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::inode::RawInode;
use crate::journal::writer::Transaction;
use super::{AllocError, inode_bitmap::InodeBitmap};

const EXTENTS_FL: u32 = 0x0008_0000;

/// Inline extent tree header: magic=0xF30A, entries=0, max=4, depth=0, generation=0.
const EXTENT_HEADER: [u8; 12] = [
    0x0A, 0xF3, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

pub fn alloc_inode(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    _is_dir: bool,
    hint_group: usize,
) -> Result<u32, AllocError> {
    let num_groups = gdt.group_count();

    for i in 0..num_groups {
        let group = (hint_group + i) % num_groups;
        if gdt.get(group)?.free_inodes_count == 0 { continue; }

        let mut bm = InodeBitmap::load(dev, sb, gdt, group)?;
        let slot = match bm.first_free() {
            Some(s) => s,
            None => continue,
        };
        bm.allocate(slot);

        let bitmap_block = gdt.get(group)?.inode_bitmap;
        flush_inode_bitmap(dev, sb, txn, bitmap_block, bm.as_bytes())?;
        update_gdt_free_inodes(dev, sb, gdt, txn, group, -1)?;
        gdt.update_itable_unused(group, slot, sb)?;
        update_gdt_itable_unused(dev, sb, gdt, txn, group)?;

        return Ok(group as u32 * sb.inodes_per_group + slot + 1);
    }

    Err(AllocError::NoFreeInodes)
}

pub fn init_inode(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    txn: &mut Transaction,
    inode_num: u32,
    mode: u16,
    uid: u32,
    gid: u32,
) -> Result<(), AllocError> {
    let now = unix_now();
    let (mut table_block, offset, inode_table_block) =
        load_inode_table_block(dev, sb, gdt, inode_num)?;

    let raw = RawInode::mut_from(&mut table_block[offset..offset + 128])
        .expect("inode slice must be 128 bytes");

    *raw = RawInode::new_zeroed();
    raw.i_mode        = U16::new(mode);
    raw.i_uid         = U16::new(uid as u16);
    raw.i_gid         = U16::new(gid as u16);
    raw.i_links_count = U16::new(1);
    raw.i_atime       = U32::new(now);
    raw.i_ctime       = U32::new(now);
    raw.i_mtime       = U32::new(now);
    raw.i_flags       = U32::new(EXTENTS_FL);
    raw._osd2[4]      = (uid >> 16) as u8;
    raw._osd2[5]      = (uid >> 24) as u8;
    raw._osd2[6]      = (gid >> 16) as u8;
    raw._osd2[7]      = (gid >> 24) as u8;
    raw.i_block[..12].copy_from_slice(&EXTENT_HEADER);

    flush_inode_table(dev, sb, txn, inode_table_block, &table_block)?;
    Ok(())
}

pub fn free_inode(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    inode_num: u32,
) -> Result<(), AllocError> {
    let group = ((inode_num - 1) / sb.inodes_per_group) as usize;
    let slot  = (inode_num - 1) % sb.inodes_per_group;

    let mut bm = InodeBitmap::load(dev, sb, gdt, group)?;
    bm.free(slot);
    let bitmap_block = gdt.get(group)?.inode_bitmap;
    flush_inode_bitmap(dev, sb, txn, bitmap_block, bm.as_bytes())?;

    let now = unix_now();
    let (mut table_block, offset, inode_table_block) =
        load_inode_table_block(dev, sb, gdt, inode_num)?;
    let raw = RawInode::mut_from(&mut table_block[offset..offset + 128])
        .expect("inode slice must be 128 bytes");
    raw.i_dtime = U32::new(now);
    flush_inode_table(dev, sb, txn, inode_table_block, &table_block)?;

    update_gdt_itable_unused(dev, sb, gdt, txn, group)?;
    update_gdt_free_inodes(dev, sb, gdt, txn, group, 1)?;
    Ok(())
}

fn unix_now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(1)
}

/// Returns (block_data, byte_offset_of_inode_in_block, inode_table_block_addr).
fn load_inode_table_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    inode_num: u32,
) -> Result<(Vec<u8>, usize, u64), AllocError> {
    let group = ((inode_num - 1) / sb.inodes_per_group) as usize;
    let index_in_group = ((inode_num - 1) % sb.inodes_per_group) as usize;
    let desc = gdt.get(group)?;
    let inode_table_block = desc.inode_table;

    let inode_size = sb.inode_size as usize;
    let inodes_per_block = sb.block_size as usize / inode_size;
    let block_index_in_table = index_in_group / inodes_per_block;
    let index_in_block = index_in_group % inodes_per_block;
    let offset = index_in_block * inode_size;
    let target_block = inode_table_block + block_index_in_table as u64;

    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = target_block * sectors_per_block;
    let data = read_sectors(dev, start_sector, sectors_per_block)?;
    Ok((data, offset, target_block))
}

fn flush_inode_bitmap(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    bitmap_block: u64,
    data: &[u8],
) -> Result<(), AllocError> {
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = bitmap_block * sectors_per_block;
    let mut padded = data.to_vec();
    padded.resize(sb.block_size as usize, 0);
    write_sectors(dev, start_sector, &padded)?;
    txn.pin_block(bitmap_block, padded).ok();
    Ok(())
}

fn flush_inode_table(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    inode_table_block: u64,
    data: &[u8],
) -> Result<(), AllocError> {
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = inode_table_block * sectors_per_block;
    let mut padded = data.to_vec();
    padded.resize(sb.block_size as usize, 0);
    write_sectors(dev, start_sector, &padded)?;
    txn.pin_block(inode_table_block, padded).ok();
    Ok(())
}

fn update_gdt_free_inodes(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    group: usize,
    delta: i64,
) -> Result<(), AllocError> {
    gdt.adjust_free_inodes(group, delta)?;
    sync_gdt_block(dev, sb, gdt, txn, group)
}

fn update_gdt_itable_unused(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    group: usize,
) -> Result<(), AllocError> {
    sync_gdt_block(dev, sb, gdt, txn, group)
}

fn sync_gdt_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &GroupDescTable,
    txn: &mut Transaction,
    group: usize,
) -> Result<(), AllocError> {
    let gdt_block = sb.first_data_block as u64 + 1;
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = gdt_block * sectors_per_block;
    let mut gdt_data = read_sectors(dev, start_sector, sectors_per_block)?;

    let desc_size = sb.desc_size as usize;
    let offset = group * desc_size;
    if offset + 32 <= gdt_data.len() {
        let desc = gdt.get(group)?;
        // free_inodes_count_lo @ offset 14
        gdt_data[offset + 14..offset + 16].copy_from_slice(&(desc.free_inodes_count as u16).to_le_bytes());
        // itable_unused_lo @ offset 28
        gdt_data[offset + 28..offset + 30].copy_from_slice(&(desc.itable_unused as u16).to_le_bytes());

        if desc_size >= 64 {
            // free_inodes_count_hi @ offset 46
            gdt_data[offset + 46..offset + 48].copy_from_slice(&((desc.free_inodes_count >> 16) as u16).to_le_bytes());
            // itable_unused_hi @ offset 50
            gdt_data[offset + 50..offset + 52].copy_from_slice(&((desc.itable_unused >> 16) as u16).to_le_bytes());
        }
    }

    txn.pin_block(gdt_block, gdt_data).ok();
    Ok(())
}
