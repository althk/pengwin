use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::VolumeInfo;
use winfsp::Result;

use crate::fs_context::Ext4Fs;

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn fill_volume_info(&self, out: &mut VolumeInfo) -> Result<()> {
        let sb = self.superblock();
        out.total_size = sb.blocks_count * sb.block_size as u64;
        out.free_size = 0;
        out.set_volume_label(&sb.volume_name);
        Ok(())
    }
}
