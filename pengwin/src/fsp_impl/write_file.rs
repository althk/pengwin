use ext4_core::block_device::BlockDevice;
use ext4_core::file_write::write_file_data;
use ext4_core::inode_write::{update_inode, InodeUpdate};
use ext4_core::alloc::Allocator;
use winfsp::filesystem::FileInfo;
use winfsp::Result;
use windows::Win32::Foundation::{
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INTERNAL_ERROR, STATUS_DISK_FULL,
};

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;
use crate::fsp_impl::now;

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    pub fn write_file_data_cb(
        &self,
        context: &FileHandle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        let inode_num = match context {
            FileHandle::File { inode_num, .. } => *inode_num,
            _ => return Err(STATUS_INVALID_DEVICE_REQUEST.into()),
        };

        if buffer.is_empty() {
            let inode = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
            *file_info = file_info_from_inode(&inode, inode_num);
            return Ok(0);
        }

        let mut journal = self.journal.lock();
        let mut txn = journal.begin_transaction();

        {
            // Re-read inode inside the journal lock so write_offset and new_size
            // are derived from the same snapshot used by the write and inode update.
            let inode = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
            let write_offset = if write_to_eof { inode.size } else { offset };

            if constrained_io && write_offset + buffer.len() as u64 > inode.size {
                *file_info = file_info_from_inode(&inode, inode_num);
                return Ok(0);
            }

            let new_size = (write_offset + buffer.len() as u64).max(inode.size);

            let mut gdt = self.gdt.lock();
            let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
            write_file_data(&self.dev, &self.sb, &mut alloc, &mut txn,
                inode_num, &inode, write_offset, buffer)
                .map_err(|e| { tracing::error!(inode_num, write_offset, len = buffer.len(), "write_file_data: {e}"); STATUS_DISK_FULL })?;
            update_inode(&self.dev, &self.sb, alloc.gdt_ref(), &mut txn, inode_num,
                InodeUpdate::default().with_size(new_size).with_mtime(now()).with_ctime(now()))
                .map_err(|e| { tracing::error!(inode_num, "update_inode: {e}"); STATUS_INTERNAL_ERROR })?;

            // Persist superblock changes (free blocks/inodes count). Reuse the gdt
            // lock acquired above — parking_lot::Mutex is not reentrant.
            let free_blocks: u64 = (0..gdt.group_count())
                .filter_map(|i| gdt.get(i).ok())
                .map(|g| g.free_blocks_count as u64)
                .sum();
            let free_inodes: u32 = (0..gdt.group_count())
                .filter_map(|i| gdt.get(i).ok())
                .map(|g| g.free_inodes_count)
                .sum();

            ext4_core::superblock_write::update_superblock(&self.dev, &self.sb, &mut txn,
                free_blocks, free_inodes, now())
                .map_err(|e| { tracing::error!("update_superblock: {e}"); STATUS_INTERNAL_ERROR })?;
        }

        journal.commit(&self.dev, txn).map_err(|e| { tracing::error!(inode_num, "journal.commit: {e}"); STATUS_INTERNAL_ERROR })?;
        drop(journal);

        let updated = self.inode(inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
        *file_info = file_info_from_inode(&updated, inode_num);
        tracing::debug!(target: "pengwin::write",
            "write_file_data_cb returning n={} size={} alloc={}",
            buffer.len(), file_info.file_size, file_info.allocation_size);
        Ok(buffer.len() as u32)
    }
}
