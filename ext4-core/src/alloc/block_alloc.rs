use crate::block_device::{BlockDevice, read_sectors, write_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::journal::writer::Transaction;
use super::{AllocError, block_bitmap::BlockBitmap};

/// Allocate `count` contiguous blocks. Returns starting absolute block address.
/// Tries to allocate near `hint_block` for locality.
pub fn alloc_blocks(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    count: u32,
    hint_block: u64,
) -> Result<u64, AllocError> {
    let num_groups = gdt.group_count();
    let hint_group = (hint_block / sb.blocks_per_group as u64) as usize;

    // Try hint group first, then walk all groups.
    let group_order = (0..num_groups)
        .map(|i| (hint_group + i) % num_groups);

    for group in group_order {
        let free_count = gdt.get(group)?.free_blocks_count;
        if free_count < count { continue; }

        let mut bm = BlockBitmap::load(dev, sb, gdt, group)?;
        let start_bit = match bm.first_free_run(count, sb.blocks_per_group) {
            Some(b) => b,
            None => continue,
        };

        for bit in start_bit..start_bit + count {
            bm.allocate(bit);
        }

        let bitmap_block = gdt.get(group)?.block_bitmap;
        flush_bitmap(dev, sb, txn, bitmap_block, bm.as_bytes())?;
        update_gdt_free_blocks(dev, sb, gdt, txn, group, -(count as i64))?;
        let abs_block = group as u64 * sb.blocks_per_group as u64 + start_bit as u64;
        return Ok(abs_block);
    }

    Err(AllocError::NoFreeBlocks)
}

/// Free a single block.
pub fn free_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    block: u64,
) -> Result<(), AllocError> {
    let group = (block / sb.blocks_per_group as u64) as usize;
    let bit = (block % sb.blocks_per_group as u64) as u32;

    let mut bm = BlockBitmap::load(dev, sb, gdt, group)?;
    if !bm.free(bit) {
        return Err(AllocError::DoubleFreed(block));
    }

    let bitmap_block = gdt.get(group)?.block_bitmap;
    flush_bitmap(dev, sb, txn, bitmap_block, bm.as_bytes())?;
    txn.revoke_block(block);
    update_gdt_free_blocks(dev, sb, gdt, txn, group, 1)?;
    Ok(())
}

fn flush_bitmap(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    bitmap_block: u64,
    data: &[u8],
) -> Result<(), AllocError> {
    // Write through to disk immediately so subsequent loads see the update.
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = bitmap_block * sectors_per_block;
    let mut padded = data.to_vec();
    padded.resize(sb.block_size as usize, 0);
    write_sectors(dev, start_sector, &padded)?;
    txn.pin_block(bitmap_block, padded).ok();
    Ok(())
}

/// Update `free_blocks_count` in the in-memory GDT and pin the affected GDT block to `txn`.
fn update_gdt_free_blocks(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    gdt: &mut GroupDescTable,
    txn: &mut Transaction,
    group: usize,
    delta: i64,
) -> Result<(), AllocError> {
    gdt.adjust_free_blocks(group, delta)?;

    // Pin GDT block containing this descriptor to the transaction.
    let gdt_block = sb.first_data_block as u64 + 1;
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = gdt_block * sectors_per_block;
    let sector_count = sectors_per_block;
    let gdt_data = read_sectors(dev, start_sector, sector_count)?;
    txn.pin_block(gdt_block, gdt_data).ok();
    Ok(())
}
