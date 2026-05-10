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
/// Caller is responsible for updating inode size via `update_inode` after this call.
///
/// Allocation strategy: unallocated logical blocks in the write range are grouped
/// into runs and allocated in batches via `alloc_blocks(count, hint)`. The batch
/// returns a contiguous physical range, which then coalesces into the trailing
/// extent in `extent_append`. Capping the per-call batch at `EXTENT_BATCH_MAX`
/// keeps each allocator pass within a single block group's worst-case bitmap
/// scan and within `ee_len`'s 32768-block extent ceiling.
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

    // Phase 1: walk the lblock range and provision physical blocks for every
    // logical block. Runs of unallocated logical blocks are batched into a
    // single alloc_blocks(count) call so the resulting extent stays contiguous.
    let total_lblocks = (last_lblock - first_lblock + 1) as usize;
    let mut phys_map = vec![0u64; total_lblocks];
    let mut lblock = first_lblock;
    while lblock <= last_lblock {
        let phys_opt = lookup_block(dev, sb, inode, lblock)?;
        if let Some(p) = phys_opt {
            phys_map[(lblock - first_lblock) as usize] = p;
            lblock += 1;
            continue;
        }

        // Walk forward to find the end of this unallocated run.
        let run_start = lblock;
        let mut run_end = lblock;
        while run_end <= last_lblock {
            if lookup_block(dev, sb, inode, run_end)?.is_some() { break; }
            run_end += 1;
            if run_end - run_start >= EXTENT_BATCH_MAX as u64 { break; }
        }
        let mut remaining = (run_end - run_start) as u32;
        let hint = (inode.size / block_size).max(run_start);

        // alloc_blocks may not return the full requested run if the largest
        // contiguous free region is smaller — loop until the whole run is filled,
        // halving the request size on failure to handle fragmented free space.
        let mut cursor = run_start;
        while remaining > 0 {
            let mut want = remaining.min(EXTENT_BATCH_MAX);
            let phys_start = loop {
                match alloc.alloc_blocks(txn, want, hint) {
                    Ok(p) => break p,
                    Err(e) if want == 1 => return Err(e.into()),
                    Err(_) => want /= 2,
                }
            };
            let got = want;

            // Zero the freshly-allocated blocks once on disk, then record + extent_append.
            let zeros = vec![0u8; (got as usize) * sb.block_size as usize];
            write_sectors(dev, phys_start * spb, &zeros)?;
            extent_append(dev, sb, txn, inode_num, phys_start, got as u16, alloc)?;

            for i in 0..got as u64 {
                phys_map[(cursor + i - first_lblock) as usize] = phys_start + i;
            }
            cursor += got as u64;
            remaining -= got;
        }
        lblock = run_end;
    }

    // Phase 2: write the user's data into the (now-fully-allocated) blocks.
    for lb in first_lblock..=last_lblock {
        let block_start = lb * block_size;
        let intra_off = if lb == first_lblock {
            (offset - block_start) as usize
        } else { 0 };
        let data_start = if lb == first_lblock {
            0
        } else {
            (lb * block_size - offset) as usize
        };
        let data_end = ((lb + 1) * block_size - offset).min(data.len() as u64) as usize;
        let slice = &data[data_start..data_end];

        let phys = phys_map[(lb - first_lblock) as usize];
        let start_sector = phys * spb;
        // Partial-block writes require read-modify-write; full-block writes can skip the read.
        let mut block_data = if intra_off == 0 && slice.len() == sb.block_size as usize {
            vec![0u8; sb.block_size as usize]
        } else {
            read_sectors(dev, start_sector, spb)?
        };
        block_data[intra_off..intra_off + slice.len()].copy_from_slice(slice);
        write_sectors(dev, start_sector, &block_data)?;
        txn.pin_block(phys, block_data)?;
    }

    Ok(())
}

/// Cap on a single `alloc_blocks` request. 32768 = `ee_len` ceiling for one
/// initialized extent, and also the typical blocks-per-group. Going larger
/// would split into multiple extents anyway.
const EXTENT_BATCH_MAX: u32 = 32768;
