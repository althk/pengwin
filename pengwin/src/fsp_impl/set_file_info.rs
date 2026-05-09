use ext4_core::block_device::BlockDevice;
use ext4_core::inode_write::{update_inode, InodeUpdate};
use winfsp::filesystem::FileInfo;
use winfsp::Result;
use windows::Win32::Foundation::STATUS_INTERNAL_ERROR;

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;

fn inode_num_from_handle(handle: &FileHandle) -> u32 {
    match handle {
        FileHandle::File { inode_num, .. }
        | FileHandle::Directory { inode_num, .. }
        | FileHandle::Symlink { inode_num, .. } => *inode_num,
    }
}

/// Convert a Windows FILETIME (100-ns intervals since 1601-01-01) to a Unix timestamp.
fn from_filetime(ft: u64) -> u32 {
    if ft == 0 { return 0; }
    ((ft / 10_000_000).saturating_sub(11_644_473_600)) as u32
}

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    #[allow(clippy::too_many_arguments)]
    pub fn set_basic_info_cb(
        &self,
        context: &FileHandle,
        _file_attributes: u32,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        let inode_num = inode_num_from_handle(context);

        let mut upd = InodeUpdate::default();
        if last_access_time != 0 { upd = upd.with_atime(from_filetime(last_access_time)); }
        if last_write_time  != 0 { upd = upd.with_mtime(from_filetime(last_write_time)); }
        if last_change_time != 0 { upd = upd.with_ctime(from_filetime(last_change_time)); }

        let mut journal = self.journal.lock();
        let mut txn = journal.begin_transaction();

        {
            let gdt = self.gdt.lock();
            update_inode(&self.dev, &self.sb, &gdt, &mut txn, inode_num, upd)
                .map_err(|_| STATUS_INTERNAL_ERROR)?;
        }

        journal.commit(&self.dev, txn).map_err(|_| STATUS_INTERNAL_ERROR)?;
        drop(journal);

        let updated = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
        *file_info = file_info_from_inode(&updated, inode_num);
        Ok(())
    }
}
