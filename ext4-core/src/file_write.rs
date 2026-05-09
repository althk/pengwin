use crate::block_device::{BlockDevice, read_sectors, write_sectors};
use crate::superblock::Superblock;
use crate::inode::Inode;
use crate::extent::{lookup_block, ExtentError};
use crate::extent_write::{extent_append, ExtentWriteError};
use crate::alloc::Allocator;
use crate::journal::writer::Transaction;

#[derive(Debug, thiserror::Error)]
pub enum FileWriteError {
    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),

    #[error("extent error: {0}")]
    Extent(#[from] ExtentError),

    #[error("extent write error: {0}")]
    ExtentWrite(#[from] ExtentWriteError),

    #[error("journal error: {0}")]
    Journal(#[from] crate::journal::JournalError),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] crate::group_desc::GroupDescError),

    #[error("alloc error: {0}")]
    Alloc(#[from] crate::alloc::AllocError),
}

/// Write `data` at byte `offset` within a file, allocating new blocks as needed.
///
/// Caller is responsible for updating inode size via update_inode after this call.
pub fn write_file_data(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    inode_num: u32,
    inode: &Inode,
    offset: u64,
    data: &[u8],
) -> Result<(), FileWriteError> {
    if data.is_empty() {
        return Ok(());
    }

    let block_size = sb.block_size as u64;
    let spb = block_size / 512;

    let first_lblock = offset / block_size;
    let last_lblock = (offset + data.len() as u64 - 1) / block_size;

    for lblock in first_lblock..=last_lblock {
        let block_start = lblock * block_size;
        let intra_off = if lblock == first_lblock {
            (offset - block_start) as usize
        } else {
            0
        };
        let data_start = if lblock == first_lblock {
            0
        } else {
            (lblock * block_size - offset) as usize
        };
        let data_end = ((lblock + 1) * block_size - offset).min(data.len() as u64) as usize;
        let slice = &data[data_start..data_end];

        // Lookup physical block — borrows alloc.gdt_ref() briefly.
        let phys_opt = lookup_block(dev, sb, inode, lblock)?;

        let phys = match phys_opt {
            Some(p) => p,
            None => {
                let hint = inode.size / block_size;
                let phys = alloc.alloc_blocks(txn, 1, hint)?;
                let zeros = vec![0u8; sb.block_size as usize];
                write_sectors(dev, phys * spb, &zeros)?;
                extent_append(dev, sb, txn, inode_num, phys, 1, alloc)?;
                phys
            }
        };

        let start_sector = phys * spb;
        let mut block_data = read_sectors(dev, start_sector, spb)?;
        block_data[intra_off..intra_off + slice.len()].copy_from_slice(slice);
        write_sectors(dev, start_sector, &block_data)?;
        txn.pin_block(phys, block_data)?;
    }

    Ok(())
}
