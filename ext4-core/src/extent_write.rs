use zerocopy::{AsBytes, FromBytes, FromZeroes};
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, read_sectors, write_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::inode::{self, RawInode};
use crate::extent::{ExtentHeader, ExtentIdx, ExtentLeaf, EXTENT_MAGIC};
use crate::alloc::Allocator;
use crate::journal::writer::Transaction;

const MAX_DEPTH: u16 = 5;
/// Number of leaf entries that fit in the inode's 60-byte block_data area.
const ROOT_MAX_ENTRIES: u16 = 4;
/// Number of entries that fit in a single block (after 12-byte header).
fn leaf_max_entries(block_size: u32) -> u16 {
    ((block_size - 12) / 12) as u16
}

#[derive(Debug, thiserror::Error)]
pub enum ExtentWriteError {
    #[error("inode does not use extents (old-style block map not supported for write)")]
    NotExtentBased,

    #[error("extent tree depth limit (5) exceeded")]
    DepthLimitExceeded,

    #[error("extent tree is corrupt at block {0}")]
    CorruptTree(u64),

    #[error("allocator error: {0}")]
    Alloc(#[from] crate::alloc::AllocError),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),

    #[error("inode error: {0}")]
    Inode(#[from] crate::inode::InodeError),

    #[error("journal error: {0}")]
    Journal(#[from] crate::journal::JournalError),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] crate::group_desc::GroupDescError),
}

// ─── helpers ──────────────────────────────────────────────────────────────────

fn read_block(dev: &dyn BlockDevice, sb: &Superblock, block: u64) -> Result<Vec<u8>, ExtentWriteError> {
    let spb = sb.block_size as u64 / 512;
    let data = read_sectors(dev, block * spb, spb)?;
    Ok(data)
}

fn write_block(dev: &dyn BlockDevice, sb: &Superblock, txn: &mut Transaction, block: u64, data: Vec<u8>) -> Result<(), ExtentWriteError> {
    let spb = sb.block_size as u64 / 512;
    write_sectors(dev, block * spb, &data)?;
    txn.pin_block(block, data)?;
    Ok(())
}

fn parse_header(data: &[u8], location: u64) -> Result<ExtentHeader, ExtentWriteError> {
    if data.len() < 12 {
        return Err(ExtentWriteError::CorruptTree(location));
    }
    let hdr = ExtentHeader::read_from(&data[..12]).ok_or(ExtentWriteError::CorruptTree(location))?;
    if hdr.eh_magic.get() != EXTENT_MAGIC {
        return Err(ExtentWriteError::CorruptTree(location));
    }
    Ok(hdr)
}

/// Returns (inode_table_block_for_this_inode, byte_offset_within_that_block).
fn inode_location(sb: &Superblock, gdt: &GroupDescTable, inode_num: u32) -> Result<(u64, usize), ExtentWriteError> {
    let group = ((inode_num - 1) / sb.inodes_per_group) as usize;
    let idx_in_group = ((inode_num - 1) % sb.inodes_per_group) as usize;
    let desc = gdt.get(group)?;
    let inode_size = sb.inode_size as usize;
    let inodes_per_block = sb.block_size as usize / inode_size;
    let block_index = idx_in_group / inodes_per_block;
    let offset_in_block = (idx_in_group % inodes_per_block) * inode_size;
    Ok((desc.inode_table + block_index as u64, offset_in_block))
}

/// Read a raw inode from `table_block` at byte `off`, preferring the latest
/// version pinned in `txn` so prior in-txn changes to the block are not lost
/// by a read-modify-write that hits the disk version.
fn read_raw_inode_at_with_txn(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &Transaction,
    table_block: u64,
    off: usize,
) -> Result<RawInode, ExtentWriteError> {
    let data = match txn.pinned_blocks().iter().rev().find(|(b, _)| *b == table_block) {
        Some((_, pinned)) => pinned.clone(),
        None => read_block(dev, sb, table_block)?,
    };
    let raw = RawInode::read_from(&data[off..off + 128]).ok_or(ExtentWriteError::CorruptTree(0))?;
    Ok(raw)
}

fn flush_raw_inode(dev: &dyn BlockDevice, sb: &Superblock, txn: &mut Transaction, table_block: u64, off: usize, raw: &RawInode) -> Result<(), ExtentWriteError> {
    // Use the latest pinned copy if present so concurrent updates to other inodes
    // in the same table block are preserved.
    let mut data = match txn.pinned_blocks().iter().rev().find(|(b, _)| *b == table_block) {
        Some((_, pinned)) => pinned.clone(),
        None => read_block(dev, sb, table_block)?,
    };
    data[off..off + 128].copy_from_slice(raw.as_bytes());
    write_block(dev, sb, txn, table_block, data)?;
    Ok(())
}

fn make_header(entries: u16, max: u16, depth: u16) -> ExtentHeader {
    let mut h = ExtentHeader::new_zeroed();
    h.eh_magic   = U16::new(EXTENT_MAGIC);
    h.eh_entries = U16::new(entries);
    h.eh_max     = U16::new(max);
    h.eh_depth   = U16::new(depth);
    h
}

fn make_leaf_entry(ee_block: u32, ee_len: u16, phys: u64) -> ExtentLeaf {
    let mut l = ExtentLeaf::new_zeroed();
    l.ee_block    = U32::new(ee_block);
    l.ee_len      = U16::new(ee_len);
    l.ee_start_hi = U16::new((phys >> 32) as u16);
    l.ee_start_lo = U32::new(phys as u32);
    l
}

fn make_idx_entry(ei_block: u32, child_phys: u64) -> ExtentIdx {
    let mut i = ExtentIdx::new_zeroed();
    i.ei_block   = U32::new(ei_block);
    i.ei_leaf_lo = U32::new(child_phys as u32);
    i.ei_leaf_hi = U16::new((child_phys >> 32) as u16);
    i
}

/// Compute next logical block number from a root/leaf node buffer.
fn next_logical_block(node_data: &[u8]) -> u32 {
    let hdr = match ExtentHeader::read_from(&node_data[..12]) { Some(h) => h, None => return 0 };
    let entries = hdr.eh_entries.get() as usize;
    if entries == 0 { return 0; }
    let off = 12 + (entries - 1) * 12;
    let leaf = match ExtentLeaf::read_from(&node_data[off..off + 12]) { Some(l) => l, None => return 0 };
    let raw_len = leaf.ee_len.get();
    let len = if raw_len > 32768 { raw_len - 32768 } else { raw_len };
    leaf.ee_block.get().saturating_add(len as u32)
}

fn update_iblocks(raw: &mut RawInode, sb: &Superblock, added: u16) {
    let sectors = added as u32 * (sb.block_size / 512);
    raw.i_blocks_lo = U32::new(raw.i_blocks_lo.get().saturating_add(sectors));
}

// ─── Task 01: extent_append ────────────────────────────────────────────────────

/// Append `block_count` newly-allocated blocks starting at `phys_start`
/// to the extent tree of `inode_num`. Updates the inode on disk.
///
/// Coalesces with the rightmost extent when possible: if the new range is
/// contiguous in both logical and physical space and the resulting extent
/// would still fit in `ee_len`'s 15-bit max (32768 blocks), the existing
/// rightmost leaf entry's `ee_len` is bumped instead of allocating a new
/// leaf entry. This is what keeps a sequential write of N blocks producing
/// a single extent rather than N leaves (and overflowing the depth limit).
pub fn extent_append(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    inode_num: u32,
    phys_start: u64,
    block_count: u16,
    alloc: &mut Allocator,
) -> Result<(), ExtentWriteError> {
    let (uses_extents, table_block, inode_off) = {
        let gdt = alloc.gdt_ref();
        let inode = inode::read_inode(dev, sb, gdt, inode_num)?;
        let (tb, off) = inode_location(sb, gdt, inode_num)?;
        (inode.uses_extents(), tb, off)
    };
    if !uses_extents {
        return Err(ExtentWriteError::NotExtentBased);
    }
    let mut raw = read_raw_inode_at_with_txn(dev, sb, txn, table_block, inode_off)?;
    let hdr = parse_header(&raw.i_block, 0)?;

    if hdr.eh_depth.get() == 0 {
        // Try to extend the rightmost leaf entry in-place before allocating a new one.
        if try_coalesce_root_leaf(&mut raw.i_block, phys_start, block_count) {
            update_iblocks(&mut raw, sb, block_count);
            flush_raw_inode(dev, sb, txn, table_block, inode_off, &raw)?;
            return Ok(());
        }

        let entries = hdr.eh_entries.get();
        let max = hdr.eh_max.get();
        let logical = next_logical_block(&raw.i_block);
        let new_leaf = make_leaf_entry(logical, block_count, phys_start);

        if entries < max {
            let off = 12 + entries as usize * 12;
            raw.i_block[off..off + 12].copy_from_slice(new_leaf.as_bytes());
            let new_hdr = make_header(entries + 1, max, 0);
            raw.i_block[..12].copy_from_slice(new_hdr.as_bytes());
        } else {
            // Root full — grow tree, then append to the new rightmost leaf.
            extent_tree_grow(dev, sb, txn, inode_num, table_block, inode_off, &mut raw, alloc)?;

            // After grow, root is depth=1 with one index entry.
            let hdr2 = parse_header(&raw.i_block, 0)?;
            debug_assert_eq!(hdr2.eh_depth.get(), 1);
            let n = hdr2.eh_entries.get() as usize;
            let last_idx_off = 12 + (n - 1) * 12;
            let idx = ExtentIdx::read_from(&raw.i_block[last_idx_off..last_idx_off + 12])
                .ok_or(ExtentWriteError::CorruptTree(0))?;
            let child_phys = (idx.ei_leaf_hi.get() as u64) << 32 | idx.ei_leaf_lo.get() as u64;

            append_to_leaf_block(dev, sb, txn, inode_num as u64, child_phys, new_leaf, alloc, &mut raw.i_block)?;
        }
    } else {
        let depth = hdr.eh_depth.get();
        let rightmost_leaf_phys = find_rightmost_leaf(dev, sb, &raw.i_block, depth)?;

        // Try to coalesce into the rightmost leaf block before adding a new entry.
        if try_coalesce_leaf_block(dev, sb, txn, rightmost_leaf_phys, phys_start, block_count)? {
            update_iblocks(&mut raw, sb, block_count);
            flush_raw_inode(dev, sb, txn, table_block, inode_off, &raw)?;
            return Ok(());
        }

        let logical = logical_next_in_leaf(dev, sb, rightmost_leaf_phys)?;
        let new_leaf = make_leaf_entry(logical, block_count, phys_start);
        append_to_leaf_block(dev, sb, txn, inode_num as u64, rightmost_leaf_phys, new_leaf, alloc, &mut raw.i_block)?;
    }

    update_iblocks(&mut raw, sb, block_count);
    flush_raw_inode(dev, sb, txn, table_block, inode_off, &raw)?;
    Ok(())
}

/// Maximum length of a single (initialized) extent — `ee_len` is u16 with
/// values >32768 reserved for the "uninitialized" encoding.
const MAX_EXTENT_LEN: u32 = 32768;

/// If the rightmost leaf of the inline (depth=0) extent tree is contiguous in
/// both logical and physical space with `(phys_start, block_count)`, extend its
/// `ee_len` in place and return true. Otherwise return false.
fn try_coalesce_root_leaf(root: &mut [u8; 60], phys_start: u64, block_count: u16) -> bool {
    let hdr = match ExtentHeader::read_from(&root[..12]) { Some(h) => h, None => return false };
    if hdr.eh_depth.get() != 0 { return false; }
    let entries = hdr.eh_entries.get() as usize;
    if entries == 0 { return false; }
    let off = 12 + (entries - 1) * 12;
    let leaf = match ExtentLeaf::read_from(&root[off..off + 12]) { Some(l) => l, None => return false };
    coalesce_leaf_in_buf(root, off, &leaf, phys_start, block_count)
}

/// Same as `try_coalesce_root_leaf` but for a separate leaf block on disk.
/// Reads the leaf via the txn's pinned view if available.
fn try_coalesce_leaf_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    leaf_phys: u64,
    phys_start: u64,
    block_count: u16,
) -> Result<bool, ExtentWriteError> {
    let mut leaf_data = match txn.pinned_blocks().iter().rev().find(|(b, _)| *b == leaf_phys) {
        Some((_, d)) => d.clone(),
        None => read_block(dev, sb, leaf_phys)?,
    };
    let hdr = parse_header(&leaf_data, leaf_phys)?;
    let entries = hdr.eh_entries.get() as usize;
    if entries == 0 { return Ok(false); }
    let off = 12 + (entries - 1) * 12;
    let leaf = ExtentLeaf::read_from(&leaf_data[off..off + 12])
        .ok_or(ExtentWriteError::CorruptTree(leaf_phys))?;

    if coalesce_leaf_in_buf(&mut leaf_data, off, &leaf, phys_start, block_count) {
        write_block(dev, sb, txn, leaf_phys, leaf_data)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// If the trailing extent at byte `off` in `buf` is physically contiguous with
/// `(phys_start, block_count)` and the combined length still fits in a single
/// initialized extent, rewrite its `ee_len` in place and return true.
///
/// `extent_append` always places new blocks at `next_logical_block(tree)`, which
/// equals the rightmost leaf's `ee_block + ee_len`, so logical contiguity is
/// implied — only physical contiguity needs to be checked here.
fn coalesce_leaf_in_buf(
    buf: &mut [u8],
    off: usize,
    leaf: &ExtentLeaf,
    phys_start: u64,
    block_count: u16,
) -> bool {
    let raw_len = leaf.ee_len.get();
    // Skip uninitialized extents (ee_len > 32768 is the uninit encoding).
    if raw_len == 0 || raw_len > MAX_EXTENT_LEN as u16 { return false; }
    let len = raw_len as u32;
    let phys_end = ((leaf.ee_start_hi.get() as u64) << 32 | leaf.ee_start_lo.get() as u64)
        + len as u64;
    if phys_end != phys_start { return false; }

    let combined = len + block_count as u32;
    if combined > MAX_EXTENT_LEN { return false; }

    // ee_len lives at byte offset 4 within the 12-byte leaf entry.
    buf[off + 4..off + 6].copy_from_slice(&(combined as u16).to_le_bytes());
    true
}

fn find_rightmost_leaf(dev: &dyn BlockDevice, sb: &Superblock, node_data: &[u8], depth: u16) -> Result<u64, ExtentWriteError> {
    let hdr = parse_header(node_data, 0)?;
    let n = hdr.eh_entries.get() as usize;
    if n == 0 { return Err(ExtentWriteError::CorruptTree(0)); }
    let last_off = 12 + (n - 1) * 12;
    let idx = ExtentIdx::read_from(&node_data[last_off..last_off + 12])
        .ok_or(ExtentWriteError::CorruptTree(0))?;
    let child_phys = (idx.ei_leaf_hi.get() as u64) << 32 | idx.ei_leaf_lo.get() as u64;
    if depth == 1 { return Ok(child_phys); }
    let child_data = read_block(dev, sb, child_phys)?;
    find_rightmost_leaf(dev, sb, &child_data, depth - 1)
}

fn logical_next_in_leaf(dev: &dyn BlockDevice, sb: &Superblock, leaf_phys: u64) -> Result<u32, ExtentWriteError> {
    let data = read_block(dev, sb, leaf_phys)?;
    Ok(next_logical_block(&data))
}

fn append_to_leaf_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    inode_hint: u64,
    leaf_phys: u64,
    new_leaf: ExtentLeaf,
    alloc: &mut Allocator,
    root_data: &mut [u8; 60],
) -> Result<(), ExtentWriteError> {
    let mut leaf_data = read_block(dev, sb, leaf_phys)?;
    let hdr = parse_header(&leaf_data, leaf_phys)?;
    let entries = hdr.eh_entries.get();
    let max = hdr.eh_max.get();

    if entries < max {
        let off = 12 + entries as usize * 12;
        leaf_data[off..off + 12].copy_from_slice(new_leaf.as_bytes());
        let new_hdr = make_header(entries + 1, max, 0);
        leaf_data[..12].copy_from_slice(new_hdr.as_bytes());
        write_block(dev, sb, txn, leaf_phys, leaf_data)?;
    } else {
        let new_block = extent_leaf_split(dev, sb, alloc, txn, leaf_phys, leaf_phys, inode_hint, root_data)?;
        let mut new_leaf_data = read_block(dev, sb, new_block)?;
        let hdr2 = parse_header(&new_leaf_data, new_block)?;
        let e2 = hdr2.eh_entries.get();
        let m2 = hdr2.eh_max.get();
        if e2 < m2 {
            let off = 12 + e2 as usize * 12;
            new_leaf_data[off..off + 12].copy_from_slice(new_leaf.as_bytes());
            let hdr3 = make_header(e2 + 1, m2, 0);
            new_leaf_data[..12].copy_from_slice(hdr3.as_bytes());
            write_block(dev, sb, txn, new_block, new_leaf_data)?;
        }
    }
    Ok(())
}

// ─── Task 02: extent_leaf_split ───────────────────────────────────────────────

fn extent_leaf_split(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    _parent_block: u64,
    leaf_block: u64,
    inode_hint: u64,
    root_data: &mut [u8; 60],
) -> Result<u64, ExtentWriteError> {
    let mut old_data = read_block(dev, sb, leaf_block)?;
    let hdr = parse_header(&old_data, leaf_block)?;
    let entries = hdr.eh_entries.get() as usize;
    let max = hdr.eh_max.get();

    let new_block = alloc.alloc_blocks(txn, 1, inode_hint)?;

    let split_at = entries / 2;
    let new_entries = entries - split_at;
    let mut new_data = vec![0u8; sb.block_size as usize];
    let new_hdr = make_header(new_entries as u16, max, 0);
    new_data[..12].copy_from_slice(new_hdr.as_bytes());
    new_data[12..12 + new_entries * 12]
        .copy_from_slice(&old_data[12 + split_at * 12..12 + entries * 12]);

    let old_hdr = make_header(split_at as u16, max, 0);
    old_data[..12].copy_from_slice(old_hdr.as_bytes());

    write_block(dev, sb, txn, leaf_block, old_data)?;
    write_block(dev, sb, txn, new_block, new_data.clone())?;

    let first_logical = ExtentLeaf::read_from(&new_data[12..24])
        .map(|l| l.ee_block.get())
        .unwrap_or(0);
    add_idx_to_root(root_data, first_logical, new_block)?;

    Ok(new_block)
}

fn add_idx_to_root(root_data: &mut [u8; 60], first_logical: u32, child_phys: u64) -> Result<(), ExtentWriteError> {
    let hdr = parse_header(root_data, 0)?;
    let entries = hdr.eh_entries.get();
    let max = hdr.eh_max.get();
    if entries >= max {
        return Err(ExtentWriteError::DepthLimitExceeded);
    }
    let off = 12 + entries as usize * 12;
    root_data[off..off + 12].copy_from_slice(make_idx_entry(first_logical, child_phys).as_bytes());
    let new_hdr = make_header(entries + 1, max, hdr.eh_depth.get());
    root_data[..12].copy_from_slice(new_hdr.as_bytes());
    Ok(())
}

// ─── Task 03: extent_tree_grow ────────────────────────────────────────────────

fn extent_tree_grow(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    txn: &mut Transaction,
    inode_num: u32,
    table_block: u64,
    inode_off: usize,
    raw: &mut RawInode,
    alloc: &mut Allocator,
) -> Result<(), ExtentWriteError> {
    let hdr = parse_header(&raw.i_block, 0)?;
    let depth = hdr.eh_depth.get();
    assert!(depth < MAX_DEPTH, "extent tree depth limit exceeded");

    let new_block = alloc.alloc_blocks(txn, 1, inode_num as u64)?;

    let entries = hdr.eh_entries.get();
    let new_max = leaf_max_entries(sb.block_size);
    let mut child_data = vec![0u8; sb.block_size as usize];
    let child_hdr = make_header(entries, new_max, depth);
    child_data[..12].copy_from_slice(child_hdr.as_bytes());
    let entries_bytes = entries as usize * 12;
    child_data[12..12 + entries_bytes].copy_from_slice(&raw.i_block[12..12 + entries_bytes]);
    write_block(dev, sb, txn, new_block, child_data)?;

    let root_hdr = make_header(1, ROOT_MAX_ENTRIES, depth + 1);
    let mut new_root = [0u8; 60];
    new_root[..12].copy_from_slice(root_hdr.as_bytes());
    new_root[12..24].copy_from_slice(make_idx_entry(0, new_block).as_bytes());
    raw.i_block = new_root;

    flush_raw_inode(dev, sb, txn, table_block, inode_off, raw)?;
    Ok(())
}

// ─── Task 04: extent_truncate ─────────────────────────────────────────────────

/// Free all extent blocks beyond logical block `keep_from`.
/// If `keep_from == 0`, frees the entire extent tree including internal nodes.
pub fn extent_truncate(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    inode_num: u32,
    keep_from: u32,
) -> Result<(), ExtentWriteError> {
    let (uses_extents, table_block, inode_off) = {
        let gdt = alloc.gdt_ref();
        let inode = inode::read_inode(dev, sb, gdt, inode_num)?;
        let (tb, off) = inode_location(sb, gdt, inode_num)?;
        (inode.uses_extents(), tb, off)
    };
    if !uses_extents {
        return Err(ExtentWriteError::NotExtentBased);
    }
    let mut raw = read_raw_inode_at_with_txn(dev, sb, txn, table_block, inode_off)?;

    let hdr = parse_header(&raw.i_block, 0)?;
    let depth = hdr.eh_depth.get();

    // Work on a mutable slice of the root.
    let mut root_buf = raw.i_block;
    truncate_node(dev, sb, alloc, txn, &mut root_buf[..], depth, keep_from, true)?;

    raw.i_block = root_buf;
    raw.i_blocks_lo = U32::new(count_iblocks(&raw.i_block, sb.block_size));
    flush_raw_inode(dev, sb, txn, table_block, inode_off, &raw)?;
    Ok(())
}

fn truncate_node(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    node: &mut [u8],
    depth: u16,
    keep_from: u32,
    is_root: bool,
) -> Result<bool, ExtentWriteError> {
    let hdr = parse_header(node, 0)?;
    let mut entries = hdr.eh_entries.get() as usize;

    if depth == 0 {
        let mut i = 0usize;
        while i < entries {
            let off = 12 + i * 12;
            let leaf = ExtentLeaf::read_from(&node[off..off + 12])
                .ok_or(ExtentWriteError::CorruptTree(0))?;
            let first = leaf.ee_block.get();
            let raw_len = leaf.ee_len.get();
            let len = if raw_len > 32768 { raw_len - 32768 } else { raw_len } as u32;
            let last = first.saturating_add(len);

            if first >= keep_from {
                let phys = (leaf.ee_start_hi.get() as u64) << 32 | leaf.ee_start_lo.get() as u64;
                for b in 0..len as u64 {
                    alloc.free_block(txn, phys + b)?;
                    txn.revoke_block(phys + b);
                }
                let remaining = entries - i - 1;
                if remaining > 0 {
                    node.copy_within(off + 12..off + 12 + remaining * 12, off);
                }
                entries -= 1;
            } else if last > keep_from {
                let keep_len = (keep_from - first) as u16;
                let phys = (leaf.ee_start_hi.get() as u64) << 32 | leaf.ee_start_lo.get() as u64;
                let free_start = phys + keep_len as u64;
                let free_len = (last - keep_from) as u64;
                for b in 0..free_len {
                    alloc.free_block(txn, free_start + b)?;
                    txn.revoke_block(free_start + b);
                }
                let actual_keep = if raw_len > 32768 { keep_len + 32768 } else { keep_len };
                node[off + 4..off + 6].copy_from_slice(&actual_keep.to_le_bytes());
                i += 1;
            } else {
                i += 1;
            }
        }
        let new_hdr = make_header(entries as u16, hdr.eh_max.get(), 0);
        node[..12].copy_from_slice(new_hdr.as_bytes());
        return Ok(entries == 0 && keep_from == 0);
    }

    let mut i = 0usize;
    while i < entries {
        let off = 12 + i * 12;
        let idx = ExtentIdx::read_from(&node[off..off + 12])
            .ok_or(ExtentWriteError::CorruptTree(0))?;
        let ei_block = idx.ei_block.get();
        let child_phys = (idx.ei_leaf_hi.get() as u64) << 32 | idx.ei_leaf_lo.get() as u64;

        if ei_block >= keep_from && keep_from > 0 {
            free_subtree(dev, sb, alloc, txn, child_phys, depth - 1)?;
            let remaining = entries - i - 1;
            if remaining > 0 {
                node.copy_within(off + 12..off + 12 + remaining * 12, off);
            }
            entries -= 1;
        } else {
            let mut child_data = read_block(dev, sb, child_phys)?;
            let child_empty = truncate_node(dev, sb, alloc, txn, &mut child_data, depth - 1, keep_from, false)?;
            if child_empty {
                alloc.free_block(txn, child_phys)?;
                txn.revoke_block(child_phys);
                let remaining = entries - i - 1;
                if remaining > 0 {
                    node.copy_within(off + 12..off + 12 + remaining * 12, off);
                }
                entries -= 1;
            } else {
                write_block(dev, sb, txn, child_phys, child_data)?;
                i += 1;
            }
        }
    }

    let new_hdr = make_header(entries as u16, hdr.eh_max.get(), depth);
    node[..12].copy_from_slice(new_hdr.as_bytes());
    Ok(entries == 0 && !is_root)
}

fn free_subtree(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    alloc: &mut Allocator,
    txn: &mut Transaction,
    block_phys: u64,
    depth: u16,
) -> Result<(), ExtentWriteError> {
    let data = read_block(dev, sb, block_phys)?;
    let hdr = parse_header(&data, block_phys)?;
    let entries = hdr.eh_entries.get() as usize;

    if depth == 0 {
        for i in 0..entries {
            let off = 12 + i * 12;
            let leaf = ExtentLeaf::read_from(&data[off..off + 12])
                .ok_or(ExtentWriteError::CorruptTree(block_phys))?;
            let raw_len = leaf.ee_len.get();
            let len = if raw_len > 32768 { raw_len - 32768 } else { raw_len } as u64;
            let phys = (leaf.ee_start_hi.get() as u64) << 32 | leaf.ee_start_lo.get() as u64;
            for b in 0..len {
                alloc.free_block(txn, phys + b)?;
                txn.revoke_block(phys + b);
            }
        }
    } else {
        for i in 0..entries {
            let off = 12 + i * 12;
            let idx = ExtentIdx::read_from(&data[off..off + 12])
                .ok_or(ExtentWriteError::CorruptTree(block_phys))?;
            let child_phys = (idx.ei_leaf_hi.get() as u64) << 32 | idx.ei_leaf_lo.get() as u64;
            free_subtree(dev, sb, alloc, txn, child_phys, depth - 1)?;
            alloc.free_block(txn, child_phys)?;
            txn.revoke_block(child_phys);
        }
    }

    alloc.free_block(txn, block_phys)?;
    txn.revoke_block(block_phys);
    Ok(())
}

fn count_iblocks(root_data: &[u8], block_size: u32) -> u32 {
    if root_data.len() < 12 { return 0; }
    let hdr = match ExtentHeader::read_from(&root_data[..12]) { Some(h) => h, None => return 0 };
    if hdr.eh_depth.get() != 0 { return 0; }
    let entries = hdr.eh_entries.get() as usize;
    let mut total: u32 = 0;
    for i in 0..entries {
        let off = 12 + i * 12;
        if off + 12 > root_data.len() { break; }
        let leaf = match ExtentLeaf::read_from(&root_data[off..off + 12]) { Some(l) => l, None => break };
        let raw_len = leaf.ee_len.get();
        let len = if raw_len > 32768 { raw_len - 32768 } else { raw_len } as u32;
        total = total.saturating_add(len * (block_size / 512));
    }
    total
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDeviceError;
    use crate::group_desc::{GroupDesc, GroupDescTable};
    use crate::alloc::Allocator;
    use zerocopy::AsBytes;
    use std::sync::Mutex;

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

    /// Device layout (4096-byte blocks, 128 blocks/group, 32 inodes/group, 2 groups):
    ///   group 0: block_bitmap=0, inode_bitmap=1, inode_table=2, data=3..127
    ///   group 1: block_bitmap=128, inode_bitmap=129, inode_table=130, data=131..255
    /// Metadata blocks (0-2 and 128-130) are pre-marked as allocated in the bitmaps.
    fn make_env() -> (MemDev, Superblock, GroupDescTable) {
        let block_size: u32 = 4096;
        let bpg: u32 = 128;
        let ipg: u32 = 32;
        let mut raw = vec![0u8; (bpg * 2) as usize * block_size as usize];

        // Group 0 bitmap at block 0 (byte offset 0): mark bits 0,1,2 as used.
        raw[0] |= 0b0000_0111; // bits 0,1,2

        // Group 1 bitmap at block 128 (byte offset 128*4096): mark bits 0,1,2 as used.
        let g1_bm_off = 128 * block_size as usize;
        raw[g1_bm_off] |= 0b0000_0111;

        let dev = MemDev(Mutex::new(raw));
        let sb = Superblock {
            block_size,
            blocks_count: (bpg * 2) as u64,
            inodes_count: ipg * 2,
            inodes_per_group: ipg,
            blocks_per_group: bpg,
            first_data_block: 0,
            uuid: [0u8; 16],
            volume_name: String::new(),
            desc_size: 64,
            feature_incompat: 0,
            feature_ro_compat: 0,
            inode_size: 128,
            state: 0x0001,
        };
        // free_blocks_count reflects that 3 blocks are pre-used in each group.
        let gdt = GroupDescTable::from_groups(vec![
            GroupDesc { block_bitmap: 0, inode_bitmap: 1, inode_table: 2, free_blocks_count: bpg - 3, free_inodes_count: ipg, itable_unused: 0 },
            GroupDesc { block_bitmap: 128, inode_bitmap: 129, inode_table: 130, free_blocks_count: bpg - 3, free_inodes_count: ipg, itable_unused: 0 },
        ]);
        (dev, sb, gdt)
    }

    fn write_test_inode(dev: &MemDev, sb: &Superblock, gdt: &GroupDescTable, inode_num: u32, block_data: [u8; 60]) {
        let group = ((inode_num - 1) / sb.inodes_per_group) as usize;
        let idx_in_group = ((inode_num - 1) % sb.inodes_per_group) as usize;
        let desc = gdt.get(group).unwrap();
        let off = desc.inode_table as usize * sb.block_size as usize + idx_in_group * 128;
        let mut raw = RawInode::new_zeroed();
        raw.i_mode        = U16::new(0o100644);
        raw.i_links_count = U16::new(1);
        raw.i_flags       = U32::new(0x80000);
        raw.i_block       = block_data;
        let mut data = dev.0.lock().unwrap();
        data[off..off + 128].copy_from_slice(raw.as_bytes());
    }

    fn empty_root() -> [u8; 60] {
        let mut d = [0u8; 60];
        d[..12].copy_from_slice(make_header(0, ROOT_MAX_ENTRIES, 0).as_bytes());
        d
    }

    #[allow(dead_code)]
    fn root_with_leaves(leaves: &[ExtentLeaf]) -> [u8; 60] {
        let mut d = [0u8; 60];
        d[..12].copy_from_slice(make_header(leaves.len() as u16, ROOT_MAX_ENTRIES, 0).as_bytes());
        for (i, l) in leaves.iter().enumerate() {
            d[12 + i * 12..24 + i * 12].copy_from_slice(l.as_bytes());
        }
        d
    }

    fn read_root(dev: &MemDev, sb: &Superblock, gdt: &GroupDescTable, inode_num: u32) -> [u8; 60] {
        let group = ((inode_num - 1) / sb.inodes_per_group) as usize;
        let idx_in_group = ((inode_num - 1) % sb.inodes_per_group) as usize;
        let desc = gdt.get(group).unwrap();
        let off = desc.inode_table as usize * sb.block_size as usize + idx_in_group * 128;
        let data = dev.0.lock().unwrap();
        RawInode::read_from(&data[off..off + 128]).unwrap().i_block
    }

    // ── tests ──────────────────────────────────────────────────────────────────

    #[test]
    fn append_to_empty_file() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
        extent_append(&dev, &sb, &mut txn, 1, 500, 1, &mut alloc).unwrap();

        // Read inode back — need gdt after alloc is dropped.
        drop(alloc);
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_entries.get(), 1);
        let leaf = ExtentLeaf::read_from(&bd[12..24]).unwrap();
        assert_eq!(leaf.ee_block.get(), 0);
        assert_eq!(leaf.ee_len.get(), 1);
        let phys = (leaf.ee_start_hi.get() as u64) << 32 | leaf.ee_start_lo.get() as u64;
        assert_eq!(phys, 500);
    }

    /// Sequential physically-contiguous appends must coalesce into one extent.
    /// This is the property that prevents large sequential writes from blowing
    /// past the 5-level extent tree depth limit.
    #[test]
    fn append_contiguous_coalesces_into_one_extent() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);

        // 10 single-block appends, each starting at the previous extent's end.
        for i in 0..10u64 {
            extent_append(&dev, &sb, &mut txn, 1, 500 + i, 1, &mut alloc).unwrap();
        }
        drop(alloc);
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_depth.get(), 0, "should not have grown");
        assert_eq!(hdr.eh_entries.get(), 1, "all 10 blocks should fold into one extent");
        let leaf = ExtentLeaf::read_from(&bd[12..24]).unwrap();
        assert_eq!(leaf.ee_block.get(), 0);
        assert_eq!(leaf.ee_len.get(), 10);
        let phys = (leaf.ee_start_hi.get() as u64) << 32 | leaf.ee_start_lo.get() as u64;
        assert_eq!(phys, 500);
    }

    /// Coalescing must stop at the per-extent length ceiling (32768 blocks):
    /// the next append starts a new extent rather than overflowing `ee_len`.
    #[test]
    fn append_stops_coalescing_at_max_extent_len() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);

        // Seed an extent already at the max length; the next contiguous append
        // must NOT bump it past 32768.
        extent_append(&dev, &sb, &mut txn, 1, 1000, 32768u16, &mut alloc).unwrap();
        // ...and immediately another contiguous block.
        extent_append(&dev, &sb, &mut txn, 1, 1000 + 32768, 1, &mut alloc).unwrap();
        drop(alloc);
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_entries.get(), 2,
            "second append should start a fresh leaf entry once the first hits ee_len max");
        let leaf0 = ExtentLeaf::read_from(&bd[12..24]).unwrap();
        let leaf1 = ExtentLeaf::read_from(&bd[24..36]).unwrap();
        assert_eq!(leaf0.ee_len.get(), 32768);
        assert_eq!(leaf1.ee_len.get(), 1);
        assert_eq!(leaf1.ee_block.get(), 32768);
    }

    #[test]
    fn append_fills_root() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
        for i in 0..4u16 {
            extent_append(&dev, &sb, &mut txn, 1, 500 + i as u64 * 10, 1, &mut alloc).unwrap();
        }
        drop(alloc);
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_entries.get(), 4);
        for i in 0..4usize {
            let leaf = ExtentLeaf::read_from(&bd[12 + i * 12..24 + i * 12]).unwrap();
            assert_eq!(leaf.ee_block.get(), i as u32);
        }
    }

    #[test]
    fn append_triggers_grow() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
        for i in 0..5u64 {
            extent_append(&dev, &sb, &mut txn, 1, 500 + i * 10, 1, &mut alloc).unwrap();
        }
        drop(alloc);
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_depth.get(), 1, "tree should have grown to depth 1");
    }

    #[test]
    fn append_after_grow() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        let mut txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
        for i in 0..6u64 {
            extent_append(&dev, &sb, &mut txn, 1, 500 + i * 10, 1, &mut alloc).unwrap();
        }
        drop(alloc);
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_depth.get(), 1);
        // Verify child leaf has entries.
        let idx_entry = ExtentIdx::read_from(&bd[12..24]).unwrap();
        let child_phys = (idx_entry.ei_leaf_hi.get() as u64) << 32 | idx_entry.ei_leaf_lo.get() as u64;
        let child_data = {
            let data = dev.0.lock().unwrap();
            data[child_phys as usize * 4096..child_phys as usize * 4096 + 4096].to_vec()
        };
        let child_hdr = ExtentHeader::read_from(&child_data[..12]).unwrap();
        assert!(child_hdr.eh_entries.get() > 0);
    }

    #[test]
    fn split_full_leaf() {
        let (dev, sb, mut gdt) = make_env();
        let max = leaf_max_entries(sb.block_size);
        let mut setup_txn = Transaction::new(1);
        let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
        let leaf_phys = alloc.alloc_blocks(&mut setup_txn, 1, 0).unwrap();

        // Fill the leaf block.
        let mut leaf_data = vec![0u8; sb.block_size as usize];
        leaf_data[..12].copy_from_slice(make_header(max, max, 0).as_bytes());
        for i in 0..max as usize {
            leaf_data[12 + i * 12..24 + i * 12]
                .copy_from_slice(make_leaf_entry(i as u32, 1, 1000 + i as u64).as_bytes());
        }
        {
            let mut data = dev.0.lock().unwrap();
            let off = leaf_phys as usize * 4096;
            data[off..off + 4096].copy_from_slice(&leaf_data);
        }

        let mut root_data = [0u8; 60];
        root_data[..12].copy_from_slice(make_header(1, ROOT_MAX_ENTRIES, 1).as_bytes());
        root_data[12..24].copy_from_slice(make_idx_entry(0, leaf_phys).as_bytes());

        let mut txn = Transaction::new(2);
        let new_block = extent_leaf_split(&dev, &sb, &mut alloc, &mut txn, 0, leaf_phys, leaf_phys, &mut root_data).unwrap();

        let old_entries = {
            let data = dev.0.lock().unwrap();
            ExtentHeader::read_from(&data[leaf_phys as usize * 4096..leaf_phys as usize * 4096 + 12]).unwrap().eh_entries.get()
        };
        let new_entries = {
            let data = dev.0.lock().unwrap();
            ExtentHeader::read_from(&data[new_block as usize * 4096..new_block as usize * 4096 + 12]).unwrap().eh_entries.get()
        };
        assert!(old_entries > 0);
        assert!(new_entries > 0);
        assert_eq!(old_entries + new_entries, max);
    }

    #[test]
    fn truncate_partial() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        // Append 10 blocks (each 1-block extent) — allocator assigns real blocks.
        let mut txn = Transaction::new(1);
        {
            let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
            for _ in 0..10 {
                // alloc a data block, then append it
                let phys = alloc.alloc_blocks(&mut txn, 1, 0).unwrap();
                extent_append(&dev, &sb, &mut txn, 1, phys, 1, &mut alloc).unwrap();
            }
        }
        // Truncate to keep only first 5 logical blocks.
        let mut txn2 = Transaction::new(2);
        {
            let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
            extent_truncate(&dev, &sb, &mut alloc, &mut txn2, 1, 5).unwrap();
        }
        // Verify the inode tree: root entries should cover ≤ 5 blocks total.
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        if hdr.eh_depth.get() == 0 {
            let total: u32 = (0..hdr.eh_entries.get() as usize).map(|i| {
                let off = 12 + i * 12;
                let l = ExtentLeaf::read_from(&bd[off..off + 12]).unwrap();
                let raw_len = l.ee_len.get();
                if raw_len > 32768 { (raw_len - 32768) as u32 } else { raw_len as u32 }
            }).sum();
            assert!(total <= 5, "should have at most 5 blocks, got {total}");
        }
        // depth>0 means some internal nodes remain — just verify it didn't panic.
    }

    #[test]
    fn truncate_full() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        // Append 4 blocks using the allocator (blocks are properly tracked in bitmap).
        let mut txn = Transaction::new(1);
        {
            let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
            for _ in 0..4 {
                let phys = alloc.alloc_blocks(&mut txn, 1, 0).unwrap();
                extent_append(&dev, &sb, &mut txn, 1, phys, 1, &mut alloc).unwrap();
            }
        }
        // Truncate to 0.
        let mut txn2 = Transaction::new(2);
        {
            let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
            extent_truncate(&dev, &sb, &mut alloc, &mut txn2, 1, 0).unwrap();
        }
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_entries.get(), 0);
    }

    #[test]
    fn truncate_across_internal_nodes() {
        let (dev, sb, mut gdt) = make_env();
        write_test_inode(&dev, &sb, &gdt, 1, empty_root());
        // Append 5 blocks, triggering tree grow (depth 1 with internal nodes).
        let mut txn = Transaction::new(1);
        {
            let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
            for _ in 0..5 {
                let phys = alloc.alloc_blocks(&mut txn, 1, 0).unwrap();
                extent_append(&dev, &sb, &mut txn, 1, phys, 1, &mut alloc).unwrap();
            }
        }
        // Truncate to 0 — must free internal node blocks too.
        let mut txn2 = Transaction::new(2);
        {
            let mut alloc = Allocator::new(&dev, &sb, &mut gdt);
            extent_truncate(&dev, &sb, &mut alloc, &mut txn2, 1, 0).unwrap();
        }
        let bd = read_root(&dev, &sb, &gdt, 1);
        let hdr = ExtentHeader::read_from(&bd[..12]).unwrap();
        assert_eq!(hdr.eh_entries.get(), 0);
    }
}
