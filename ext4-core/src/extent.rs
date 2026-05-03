use zerocopy::{FromBytes, FromZeroes, AsBytes};
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::inode::Inode;

pub const EXTENT_MAGIC: u16 = 0xF30A;
const MAX_DEPTH: u16 = 5;

/// Extent tree header — appears at the start of each node (root or internal).
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct ExtentHeader {
    pub eh_magic:      U16,
    pub eh_entries:    U16,
    pub eh_max:        U16,
    pub eh_depth:      U16,
    pub eh_generation: U32,
}

/// Internal (index) node entry — points to a child block.
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct ExtentIdx {
    pub ei_block:   U32,
    pub ei_leaf_lo: U32,
    pub ei_leaf_hi: U16,
    _unused:        U16,
}

/// Leaf node entry — maps logical blocks to physical blocks.
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct ExtentLeaf {
    pub ee_block:    U32,
    pub ee_len:      U16,
    pub ee_start_hi: U16,
    pub ee_start_lo: U32,
}

const _: () = assert!(core::mem::size_of::<ExtentHeader>() == 12);
const _: () = assert!(core::mem::size_of::<ExtentIdx>() == 12);
const _: () = assert!(core::mem::size_of::<ExtentLeaf>() == 12);

#[derive(Debug, thiserror::Error)]
pub enum ExtentError {
    #[error("invalid extent magic: {0:#x}")]
    BadMagic(u16),

    #[error("old-style indirect block maps are not supported")]
    UnsupportedBlockMap,

    #[error("extent tree depth {0} exceeds maximum (5)")]
    DepthExceeded(u16),

    #[error("extent node buffer too small for declared entry count")]
    TruncatedNode,

    #[error("block number overflows sector address space")]
    BlockNumberOverflow,

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),
}

/// Resolve logical block `lblock` to a physical block number.
///
/// Returns `None` if the block is a hole (sparse file or uninitialized extent).
pub fn lookup_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    inode: &Inode,
    lblock: u64,
) -> Result<Option<u64>, ExtentError> {
    if !inode.uses_extents() {
        return Err(ExtentError::UnsupportedBlockMap);
    }
    lookup_in_node(dev, sb, &inode.block_data, lblock)
}

fn lookup_in_node(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    node_data: &[u8],
    lblock: u64,
) -> Result<Option<u64>, ExtentError> {
    if node_data.len() < 12 {
        return Err(ExtentError::TruncatedNode);
    }
    let header = ExtentHeader::read_from(&node_data[..12])
        .ok_or(ExtentError::TruncatedNode)?;

    let magic = header.eh_magic.get();
    if magic != EXTENT_MAGIC {
        return Err(ExtentError::BadMagic(magic));
    }

    let depth = header.eh_depth.get();
    let entries = header.eh_entries.get() as usize;

    if depth > MAX_DEPTH {
        return Err(ExtentError::DepthExceeded(depth));
    }

    // Verify the node buffer is large enough for all declared entries.
    let required = 12usize.saturating_add(entries.saturating_mul(12));
    if node_data.len() < required {
        return Err(ExtentError::TruncatedNode);
    }

    if depth == 0 {
        // Leaf node: search for the extent covering lblock.
        for i in 0..entries {
            let off = 12 + i * 12;
            let leaf = ExtentLeaf::read_from(&node_data[off..off + 12])
                .ok_or(ExtentError::TruncatedNode)?;
            let first = leaf.ee_block.get() as u64;
            let len = leaf.ee_len.get();
            // ee_len > 32768 means uninitialized extent — treat as hole.
            if len > 32768 {
                continue;
            }
            let count = len as u64;
            if lblock >= first && lblock < first + count {
                let phys_start = (leaf.ee_start_hi.get() as u64) << 32
                    | leaf.ee_start_lo.get() as u64;
                return Ok(Some(phys_start + (lblock - first)));
            }
        }
        return Ok(None);
    }

    // Internal node: find the index entry whose subtree covers lblock.
    // The correct child is the last index entry with ei_block <= lblock.
    let mut best: Option<u64> = None;
    for i in 0..entries {
        let off = 12 + i * 12;
        let idx = ExtentIdx::read_from(&node_data[off..off + 12])
            .ok_or(ExtentError::TruncatedNode)?;
        if idx.ei_block.get() as u64 <= lblock {
            let child_phys = (idx.ei_leaf_hi.get() as u64) << 32
                | idx.ei_leaf_lo.get() as u64;
            best = Some(child_phys);
        } else {
            break;
        }
    }

    let child_block = match best {
        Some(b) => b,
        None    => return Ok(None),
    };

    let child_data = read_block(dev, sb, child_block)?;
    lookup_in_node(dev, sb, &child_data, lblock)
}

fn read_block(dev: &dyn BlockDevice, sb: &Superblock, block: u64) -> Result<Vec<u8>, ExtentError> {
    let sectors_per_block = sb.block_size as u64 / 512;
    let start_sector = block
        .checked_mul(sectors_per_block)
        .ok_or(ExtentError::BlockNumberOverflow)?;
    let data = read_sectors(dev, start_sector, sectors_per_block)?;
    Ok(data)
}

/// Yields `(logical_block, Option<physical_block>)` for every logical block in the file.
/// `None` physical block means a sparse hole or uninitialized extent.
#[allow(dead_code)]
pub struct ExtentIter<'a> {
    dev:             &'a dyn BlockDevice,
    sb:              &'a Superblock,
    inode:           &'a Inode,
    leaves:          Vec<ExtentLeaf>,
    leaf_idx:        usize,
    block_in_extent: u64,
    next_lblock:     u64,
    total_lblocks:   u64,
    done:            bool,
}

impl<'a> ExtentIter<'a> {
    pub fn new(dev: &'a dyn BlockDevice, sb: &'a Superblock, inode: &'a Inode) -> Result<Self, ExtentError> {
        if !inode.uses_extents() {
            return Err(ExtentError::UnsupportedBlockMap);
        }
        let mut leaves = Vec::new();
        collect_leaves(dev, sb, &inode.block_data, &mut leaves)?;
        leaves.sort_by_key(|l| l.ee_block.get());

        let total_lblocks = inode.size.div_ceil(sb.block_size as u64);

        Ok(ExtentIter {
            dev,
            sb,
            inode,
            leaves,
            leaf_idx: 0,
            block_in_extent: 0,
            next_lblock: 0,
            total_lblocks,
            done: false,
        })
    }
}

fn collect_leaves(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    node_data: &[u8],
    out: &mut Vec<ExtentLeaf>,
) -> Result<(), ExtentError> {
    if node_data.len() < 12 {
        return Err(ExtentError::TruncatedNode);
    }
    let header = ExtentHeader::read_from(&node_data[..12])
        .ok_or(ExtentError::TruncatedNode)?;
    let magic = header.eh_magic.get();
    if magic != EXTENT_MAGIC {
        return Err(ExtentError::BadMagic(magic));
    }
    let depth = header.eh_depth.get();
    let entries = header.eh_entries.get() as usize;
    if depth > MAX_DEPTH {
        return Err(ExtentError::DepthExceeded(depth));
    }

    let required = 12usize.saturating_add(entries.saturating_mul(12));
    if node_data.len() < required {
        return Err(ExtentError::TruncatedNode);
    }

    if depth == 0 {
        for i in 0..entries {
            let off = 12 + i * 12;
            let leaf = ExtentLeaf::read_from(&node_data[off..off + 12])
                .ok_or(ExtentError::TruncatedNode)?;
            out.push(leaf);
        }
    } else {
        for i in 0..entries {
            let off = 12 + i * 12;
            let idx = ExtentIdx::read_from(&node_data[off..off + 12])
                .ok_or(ExtentError::TruncatedNode)?;
            let child_phys = (idx.ei_leaf_hi.get() as u64) << 32
                | idx.ei_leaf_lo.get() as u64;
            let child_data = read_block(dev, sb, child_phys)?;
            collect_leaves(dev, sb, &child_data, out)?;
        }
    }
    Ok(())
}

impl<'a> Iterator for ExtentIter<'a> {
    type Item = Result<(u64, Option<u64>), ExtentError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next_lblock >= self.total_lblocks {
            return None;
        }

        let lblock = self.next_lblock;

        // Find the current leaf that covers lblock (if any).
        loop {
            if self.leaf_idx >= self.leaves.len() {
                // No more extents — rest is a hole.
                self.next_lblock += 1;
                return Some(Ok((lblock, None)));
            }

            let leaf = &self.leaves[self.leaf_idx];
            let first = leaf.ee_block.get() as u64;
            let len_raw = leaf.ee_len.get();
            let is_uninit = len_raw > 32768;
            let count = if is_uninit { (len_raw - 32768) as u64 } else { len_raw as u64 };

            if lblock < first {
                // We're in a hole before this extent.
                self.next_lblock += 1;
                return Some(Ok((lblock, None)));
            }

            if lblock < first + count {
                self.next_lblock += 1;
                let phys = if is_uninit {
                    None
                } else {
                    let phys_start = (leaf.ee_start_hi.get() as u64) << 32
                        | leaf.ee_start_lo.get() as u64;
                    Some(phys_start + (lblock - first))
                };
                return Some(Ok((lblock, phys)));
            }

            // lblock is past this extent; advance to the next.
            self.leaf_idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::AsBytes;
    use crate::block_device::BlockDeviceError;
    use crate::inode::{Inode, mode};
    use crate::superblock::Superblock;

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

    fn make_sb() -> Superblock {
        Superblock {
            block_size:        4096,
            blocks_count:      1024,
            inodes_count:      256,
            inodes_per_group:  256,
            blocks_per_group:  1024,
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size:        256,
        }
    }

    fn make_inode_with_block_data(block_data: [u8; 60], uses_extents: bool, size: u64) -> Inode {
        Inode {
            mode:        mode::S_IFREG,
            uid:         0,
            gid:         0,
            size,
            atime:       0,
            mtime:       0,
            ctime:       0,
            links_count: 1,
            flags:       if uses_extents { 0x80000 } else { 0 },
            block_data,
        }
    }

    /// Build the 60-byte block_data for a root node with the given leaf entries.
    fn make_root_with_leaves(leaves: &[ExtentLeaf]) -> [u8; 60] {
        let mut data = [0u8; 60];
        let mut hdr = ExtentHeader::new_zeroed();
        hdr.eh_magic   = U16::new(EXTENT_MAGIC);
        hdr.eh_entries = U16::new(leaves.len() as u16);
        hdr.eh_max     = U16::new(4);
        hdr.eh_depth   = U16::new(0);
        data[..12].copy_from_slice(hdr.as_bytes());
        for (i, leaf) in leaves.iter().enumerate() {
            let off = 12 + i * 12;
            data[off..off + 12].copy_from_slice(leaf.as_bytes());
        }
        data
    }

    fn make_leaf(ee_block: u32, ee_len: u16, phys_start: u64) -> ExtentLeaf {
        let mut l = ExtentLeaf::new_zeroed();
        l.ee_block    = U32::new(ee_block);
        l.ee_len      = U16::new(ee_len);
        l.ee_start_hi = U16::new((phys_start >> 32) as u16);
        l.ee_start_lo = U32::new(phys_start as u32);
        l
    }

    #[test]
    fn lookup_single_extent() {
        let leaf = make_leaf(0, 8, 100);
        let block_data = make_root_with_leaves(&[leaf]);
        let inode = make_inode_with_block_data(block_data, true, 8 * 4096);
        let dev = MemDevice(vec![0u8; 512]);
        let sb = make_sb();
        assert_eq!(lookup_block(&dev, &sb, &inode, 3).unwrap(), Some(103));
        assert_eq!(lookup_block(&dev, &sb, &inode, 0).unwrap(), Some(100));
        assert_eq!(lookup_block(&dev, &sb, &inode, 7).unwrap(), Some(107));
    }

    #[test]
    fn lookup_multi_extent() {
        let leaf0 = make_leaf(0, 4, 50);
        let leaf1 = make_leaf(10, 4, 200);
        let block_data = make_root_with_leaves(&[leaf0, leaf1]);
        let inode = make_inode_with_block_data(block_data, true, 14 * 4096);
        let dev = MemDevice(vec![0u8; 512]);
        let sb = make_sb();
        assert_eq!(lookup_block(&dev, &sb, &inode, 2).unwrap(), Some(52));
        assert_eq!(lookup_block(&dev, &sb, &inode, 11).unwrap(), Some(201));
    }

    #[test]
    fn lookup_out_of_range() {
        let leaf = make_leaf(0, 4, 50);
        let block_data = make_root_with_leaves(&[leaf]);
        let inode = make_inode_with_block_data(block_data, true, 4 * 4096);
        let dev = MemDevice(vec![0u8; 512]);
        let sb = make_sb();
        // Block 10 is beyond any extent → hole.
        assert_eq!(lookup_block(&dev, &sb, &inode, 10).unwrap(), None);
    }

    #[test]
    fn depth_one_tree() {
        // Build a depth-1 tree: root has one index entry pointing to a child block at block 2.
        let sb = make_sb();
        // The child block is at physical block 2 → sector 2*8 = 16.
        let child_block: u64 = 2;
        let sectors = 32usize; // need at least child_block's sectors
        let mut device_data = vec![0u8; sectors * 512];

        // Build child leaf node at block 2 (byte offset 2*4096).
        let leaf = make_leaf(0, 8, 100);
        let mut child_node = [0u8; 4096];
        let mut hdr = ExtentHeader::new_zeroed();
        hdr.eh_magic   = U16::new(EXTENT_MAGIC);
        hdr.eh_entries = U16::new(1);
        hdr.eh_max     = U16::new(340);
        hdr.eh_depth   = U16::new(0);
        child_node[..12].copy_from_slice(hdr.as_bytes());
        child_node[12..24].copy_from_slice(leaf.as_bytes());
        let child_byte = child_block as usize * 4096;
        device_data[child_byte..child_byte + 4096].copy_from_slice(&child_node);

        // Build root node: depth=1, one index entry pointing to child_block.
        let mut root_data = [0u8; 60];
        let mut root_hdr = ExtentHeader::new_zeroed();
        root_hdr.eh_magic   = U16::new(EXTENT_MAGIC);
        root_hdr.eh_entries = U16::new(1);
        root_hdr.eh_max     = U16::new(4);
        root_hdr.eh_depth   = U16::new(1);
        root_data[..12].copy_from_slice(root_hdr.as_bytes());
        let mut idx = ExtentIdx::new_zeroed();
        idx.ei_block   = U32::new(0);
        idx.ei_leaf_lo = U32::new(child_block as u32);
        idx.ei_leaf_hi = U16::new((child_block >> 32) as u16);
        root_data[12..24].copy_from_slice(idx.as_bytes());

        let inode = make_inode_with_block_data(root_data, true, 8 * 4096);
        let dev = MemDevice(device_data);
        assert_eq!(lookup_block(&dev, &sb, &inode, 5).unwrap(), Some(105));
    }

    #[test]
    fn uninitialized_extent() {
        // ee_len > 32768 → uninitialized, treat as hole.
        let leaf = make_leaf(0, 33000, 100);
        let block_data = make_root_with_leaves(&[leaf]);
        let inode = make_inode_with_block_data(block_data, true, 8 * 4096);
        let dev = MemDevice(vec![0u8; 512]);
        let sb = make_sb();
        assert_eq!(lookup_block(&dev, &sb, &inode, 0).unwrap(), None);
    }
}
