use ext4_core::block_device::BlockDevice;
use ext4_core::alloc::Allocator;
use ext4_core::extent_write::extent_truncate;
use ext4_core::inode_write::{update_inode, InodeUpdate};
use winfsp::filesystem::FileInfo;
use winfsp::Result;
use windows::Win32::Foundation::{
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INTERNAL_ERROR,
};

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;
use crate::fsp_impl::now;

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    pub fn set_file_size_cb(
        &self,
        context: &FileHandle,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        let inode_num = match context {
            FileHandle::File { inode_num, .. } => *inode_num,
            _ => return Err(STATUS_INVALID_DEVICE_REQUEST.into()),
        };

        let inode = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;

        let mut journal = self.journal.lock();
        let mut txn = journal.begin_transaction();

        {
            let mut gdt = self.gdt.lock();

            if new_size < inode.size {
                let block_size = self.sb.block_size as u64;
                let keep_from = new_size.div_ceil(block_size) as u32;
                let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
                extent_truncate(&self.dev, &self.sb, &mut alloc, &mut txn, inode_num, keep_from)
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;
                update_inode(&self.dev, &self.sb, alloc.gdt_ref(), &mut txn, inode_num,
                    InodeUpdate::default().with_size(new_size).with_mtime(now()).with_ctime(now()))
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;
            } else {
                // Extend: update size only; blocks allocated lazily on write.
                update_inode(&self.dev, &self.sb, &gdt, &mut txn, inode_num,
                    InodeUpdate::default().with_size(new_size).with_mtime(now()).with_ctime(now()))
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;
            }
        }

        journal.commit(&self.dev, txn).map_err(|_| STATUS_INTERNAL_ERROR)?;
        drop(journal);

        let updated = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
        *file_info = file_info_from_inode(&updated, inode_num);
        Ok(())
    }
}
