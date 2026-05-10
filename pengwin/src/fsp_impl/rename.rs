use ext4_core::block_device::BlockDevice;
use ext4_core::alloc::Allocator;
use ext4_core::dir_write::dir_rename;
use winfsp::Result;
use winfsp::U16CStr;
use windows::Win32::Foundation::{
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_OBJECT_NAME_INVALID, STATUS_INTERNAL_ERROR,
    STATUS_OBJECT_NAME_COLLISION,
};

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::create::split_path;
use crate::fsp_impl::STATUS_MEDIA_WRITE_PROTECTED;

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    pub fn rename_handle(
        &self,
        _context: &FileHandle,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<()> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED.into());
        }
        let src_str = file_name.to_string_lossy();
        let src = src_str.replace('\\', "/");
        let dst_str = new_file_name.to_string_lossy();
        let dst = dst_str.replace('\\', "/");

        let (src_parent, src_name) = split_path(&src).ok_or(STATUS_OBJECT_NAME_INVALID)?;
        let (dst_parent, dst_name) = split_path(&dst).ok_or(STATUS_OBJECT_NAME_INVALID)?;

        let dst_parent_inode_num = self.resolve_path(dst_parent)
            .map_err(|e| { tracing::error!(dst_parent, "rename: resolve dst_parent: {e}"); STATUS_OBJECT_PATH_NOT_FOUND })?;
        let dst_parent_inode = self.inode(dst_parent_inode_num)
            .map_err(|e| { tracing::error!(dst_parent_inode_num, "rename: read dst_parent inode: {e}"); STATUS_INTERNAL_ERROR })?;

        if self.lookup(&dst_parent_inode, dst_name)
            .map_err(|e| { tracing::error!(dst_name, "rename: lookup dst: {e}"); STATUS_INTERNAL_ERROR })?
            .is_some()
            && !replace_if_exists
        {
            return Err(STATUS_OBJECT_NAME_COLLISION.into());
        }

        let src_parent_inode_num = self.resolve_path(src_parent)
            .map_err(|e| { tracing::error!(src_parent, "rename: resolve src_parent: {e}"); STATUS_OBJECT_PATH_NOT_FOUND })?;

        let mut journal = self.journal.lock();
        let mut txn = journal.begin_transaction();

        {
            let mut gdt = self.gdt.lock();
            // Clone snapshot for the read-only gdt param (alloc holds &mut gdt).
            let gdt_snap = gdt.clone();
            let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
            dir_rename(&self.dev, &self.sb, &gdt_snap, &mut alloc, &mut txn,
                src_parent_inode_num, src_name,
                dst_parent_inode_num, dst_name)
                .map_err(|e| { tracing::error!(src_parent_inode_num, src_name, dst_parent_inode_num, dst_name, "rename: dir_rename: {e}"); STATUS_INTERNAL_ERROR })?;
        }

        journal.commit(&self.dev, txn).map_err(|e| { tracing::error!("rename: journal.commit: {e}"); STATUS_INTERNAL_ERROR })?;
        Ok(())
    }
}
