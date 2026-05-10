use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::VolumeInfo;
use winfsp::Result;

use crate::fs_context::Ext4Fs;

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn fill_volume_info(&self, out: &mut VolumeInfo) -> Result<()> {
        let sb = self.superblock();
        let block_size = sb.block_size as u64;
        out.total_size = sb.blocks_count * block_size;
        let gdt = self.gdt.lock();
        let free_blocks: u64 = (0..gdt.group_count())
            .filter_map(|i| gdt.get(i).ok())
            .map(|g| g.free_blocks_count as u64)
            .sum();
        out.free_size = free_blocks * block_size;
        tracing::debug!(total = out.total_size, free = out.free_size, "volume_info");
        out.set_volume_label(&sb.volume_name);
        Ok(())
    }
}
