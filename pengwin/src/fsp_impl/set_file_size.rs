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
use crate::fsp_impl::{now, STATUS_MEDIA_WRITE_PROTECTED};

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    pub fn set_file_size_cb(
        &self,
        context: &FileHandle,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED.into());
        }

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

        // Persist superblock free-block/inode counts (truncate frees blocks).
        {
            let gdt = self.gdt.lock();
            let free_blocks: u64 = (0..gdt.group_count())
                .filter_map(|i| gdt.get(i).ok())
                .map(|g| g.free_blocks_count as u64)
                .sum();
            let free_inodes: u32 = (0..gdt.group_count())
                .filter_map(|i| gdt.get(i).ok())
                .map(|g| g.free_inodes_count)
                .sum();
            drop(gdt);
            ext4_core::superblock_write::update_superblock(&self.dev, &self.sb, &mut txn,
                free_blocks, free_inodes, now())
                .map_err(|_| STATUS_INTERNAL_ERROR)?;
        }

        journal.commit(&self.dev, txn).map_err(|_| STATUS_INTERNAL_ERROR)?;
        drop(journal);

        let updated = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
        *file_info = file_info_from_inode(&updated, inode_num);
        Ok(())
    }
}
