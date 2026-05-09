use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use super::AllocError;

pub struct BlockBitmap {
    data: Vec<u8>,
    #[allow(dead_code)]
    group_index: usize,
}

impl BlockBitmap {
    pub fn load(
        dev: &dyn BlockDevice,
        sb: &Superblock,
        gdt: &GroupDescTable,
        group: usize,
    ) -> Result<Self, AllocError> {
        let desc = gdt.get(group)?;
        let sectors_per_block = sb.block_size as u64 / 512;
        let start_sector = desc.block_bitmap * sectors_per_block;
        let data = read_sectors(dev, start_sector, sectors_per_block)?;
        Ok(BlockBitmap { data, group_index: group })
    }

    pub fn is_allocated(&self, block_in_group: u32) -> bool {
        let byte = (block_in_group / 8) as usize;
        let bit = block_in_group % 8;
        self.data[byte] & (1 << bit) != 0
    }

    /// Mark block as allocated. Returns false if already allocated.
    pub fn allocate(&mut self, block_in_group: u32) -> bool {
        if self.is_allocated(block_in_group) { return false; }
        self.data[block_in_group as usize / 8] |= 1 << (block_in_group % 8);
        true
    }

    /// Mark block as free. Returns false if already free.
    pub fn free(&mut self, block_in_group: u32) -> bool {
        if !self.is_allocated(block_in_group) { return false; }
        self.data[block_in_group as usize / 8] &= !(1 << (block_in_group % 8));
        true
    }

    /// Find the first free block in this group. Returns None if full.
    pub fn first_free(&self) -> Option<u32> {
        for (i, &byte) in self.data.iter().enumerate() {
            if byte != 0xFF {
                let bit = byte.trailing_ones();
                return Some(i as u32 * 8 + bit);
            }
        }
        None
    }

    /// Find `count` contiguous free bits starting at or after `start`. Returns None if not found.
    pub fn first_free_run(&self, count: u32, bits_in_group: u32) -> Option<u32> {
        if count == 0 { return Some(0); }
        let mut run_start = 0u32;
        let mut run_len = 0u32;
        let limit = bits_in_group.min(self.data.len() as u32 * 8);
        for bit in 0..limit {
            if !self.is_allocated(bit) {
                if run_len == 0 { run_start = bit; }
                run_len += 1;
                if run_len >= count { return Some(run_start); }
            } else {
                run_len = 0;
            }
        }
        None
    }

    pub fn as_bytes(&self) -> &[u8] { &self.data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bitmap(data: Vec<u8>) -> BlockBitmap {
        BlockBitmap { data, group_index: 0 }
    }

    #[test]
    fn is_allocated_basic() {
        let bm = make_bitmap(vec![0b00000001, 0xFF]);
        assert!(bm.is_allocated(0));
        assert!(!bm.is_allocated(1));
        assert!(bm.is_allocated(8));
        assert!(bm.is_allocated(15));
    }

    #[test]
    fn allocate_and_free() {
        let mut bm = make_bitmap(vec![0u8; 4]);
        assert!(bm.allocate(3));
        assert!(bm.is_allocated(3));
        assert!(!bm.allocate(3), "double-allocate should return false");
        assert!(bm.free(3));
        assert!(!bm.is_allocated(3));
        assert!(!bm.free(3), "double-free should return false");
    }

    #[test]
    fn first_free_all_zero() {
        let bm = make_bitmap(vec![0u8; 4]);
        assert_eq!(bm.first_free(), Some(0));
    }

    #[test]
    fn first_free_full() {
        let bm = make_bitmap(vec![0xFF; 4]);
        assert_eq!(bm.first_free(), None);
    }

    #[test]
    fn first_free_partial() {
        // 0xFF: all bits set. 0b11111110: bit 0 is clear → first free is index 8.
        let bm = make_bitmap(vec![0xFF, 0b11111110, 0x00]);
        assert_eq!(bm.first_free(), Some(8));
    }

    #[test]
    fn first_free_run_contiguous() {
        let mut bm = make_bitmap(vec![0u8; 16]);
        // Allocate bits 0-3 so first run of 4 starts at 4.
        bm.allocate(0);
        bm.allocate(1);
        bm.allocate(2);
        bm.allocate(3);
        assert_eq!(bm.first_free_run(4, 128), Some(4));
    }
}
