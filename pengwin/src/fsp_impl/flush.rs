use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::FileInfo;
use winfsp::Result;
use windows::Win32::Foundation::STATUS_INTERNAL_ERROR;

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    pub fn flush_cb(
        &self,
        context: Option<&FileHandle>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // Flush pending journal state (no-op in current single-txn-per-op model).
        {
            let mut journal = self.journal.lock();
            journal.flush_pending(&self.dev)
                .map_err(|_| STATUS_INTERNAL_ERROR)?;
        }

        // Write barrier: flush to underlying storage.
        self.dev.flush().map_err(|_| STATUS_INTERNAL_ERROR)?;

        if let Some(handle) = context {
            let (inode, inode_num) = match handle {
                FileHandle::File { inode, inode_num }
                | FileHandle::Directory { inode, inode_num, .. }
                | FileHandle::Symlink { inode, inode_num, .. } => (inode, *inode_num),
            };
            *file_info = file_info_from_inode(inode, inode_num);
        }

        Ok(())
    }
}
