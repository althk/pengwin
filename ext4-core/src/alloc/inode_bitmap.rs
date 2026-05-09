use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use super::AllocError;

pub struct InodeBitmap {
    data: Vec<u8>,
}

impl InodeBitmap {
    pub fn load(
        dev: &dyn BlockDevice,
        sb: &Superblock,
        gdt: &GroupDescTable,
        group: usize,
    ) -> Result<Self, AllocError> {
        let desc = gdt.get(group)?;
        let sectors_per_block = sb.block_size as u64 / 512;
        let start_sector = desc.inode_bitmap * sectors_per_block;
        let data = read_sectors(dev, start_sector, sectors_per_block)?;
        Ok(InodeBitmap { data })
    }

    /// `inode_in_group` is 0-based (inode_num - 1 - group * inodes_per_group).
    pub fn is_allocated(&self, inode_in_group: u32) -> bool {
        let byte = (inode_in_group / 8) as usize;
        let bit = inode_in_group % 8;
        self.data[byte] & (1 << bit) != 0
    }

    /// Returns false if already allocated.
    pub fn allocate(&mut self, inode_in_group: u32) -> bool {
        if self.is_allocated(inode_in_group) { return false; }
        self.data[inode_in_group as usize / 8] |= 1 << (inode_in_group % 8);
        true
    }

    /// Returns false if already free.
    pub fn free(&mut self, inode_in_group: u32) -> bool {
        if !self.is_allocated(inode_in_group) { return false; }
        self.data[inode_in_group as usize / 8] &= !(1 << (inode_in_group % 8));
        true
    }

    /// First free inode slot (0-based index within group). None if group is full.
    pub fn first_free(&self) -> Option<u32> {
        for (i, &byte) in self.data.iter().enumerate() {
            if byte != 0xFF {
                let bit = byte.trailing_ones();
                return Some(i as u32 * 8 + bit);
            }
        }
        None
    }

    pub fn as_bytes(&self) -> &[u8] { &self.data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bitmap(data: Vec<u8>) -> InodeBitmap {
        InodeBitmap { data }
    }

    #[test]
    fn allocate_and_free_inode() {
        let mut bm = make_bitmap(vec![0u8; 4]);
        assert!(bm.allocate(5));
        assert!(bm.is_allocated(5));
        assert!(!bm.allocate(5));
        assert!(bm.free(5));
        assert!(!bm.is_allocated(5));
        assert!(!bm.free(5));
    }

    #[test]
    fn first_free_inode() {
        let bm = make_bitmap(vec![0xFF, 0b00000001, 0x00]);
        // Byte 0 full. Byte 1 has bit 0 set; first free is bit 9 (index 1*8+1).
        assert_eq!(bm.first_free(), Some(9));
    }
}
