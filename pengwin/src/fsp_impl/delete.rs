use ext4_core::block_device::BlockDevice;
use ext4_core::alloc::Allocator;
use ext4_core::extent_write::extent_truncate;
use ext4_core::inode_write::{update_inode, InodeUpdate};
use ext4_core::dir_write::dir_remove_entry;
use winfsp::Result;
use winfsp::U16CStr;
use windows::Win32::Foundation::{
    STATUS_INTERNAL_ERROR, STATUS_DIRECTORY_NOT_EMPTY,
};

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::create::split_path;
use crate::fsp_impl::now;

// FspCleanupDelete flag value from WinFsp.
const FSP_CLEANUP_DELETE: u32 = 0x01;

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    pub fn set_delete_cb(
        &self,
        context: &FileHandle,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        if !delete_file {
            return Ok(());
        }

        let inode = match context {
            FileHandle::File { inode, .. }
            | FileHandle::Directory { inode, .. }
            | FileHandle::Symlink { inode, .. } => inode,
        };

        if inode.is_dir() {
            let entries = self.read_dir(inode).map_err(|e| { tracing::error!("set_delete: read_dir: {e}"); STATUS_INTERNAL_ERROR })?;
            let non_dot = entries.iter().filter(|e| e.name != "." && e.name != "..").count();
            if non_dot > 0 {
                return Err(STATUS_DIRECTORY_NOT_EMPTY.into());
            }
        }

        Ok(())
    }

    pub fn cleanup_handle_write(
        &self,
        context: &FileHandle,
        file_name: Option<&U16CStr>,
        flags: u32,
    ) {
        tracing::debug!(target: "pengwin::delete", "cleanup_handle_write flags=0x{flags:x} has_name={}", file_name.is_some());
        if flags & FSP_CLEANUP_DELETE == 0 {
            return;
        }

        let path_str = match file_name {
            Some(n) => n.to_string_lossy(),
            None => { tracing::warn!("delete cleanup: no file_name supplied"); return; },
        };
        let path = path_str.replace('\\', "/");
        let (parent_path, name) = match split_path(&path) {
            Some(p) => p,
            None => { tracing::warn!(path, "delete cleanup: bad path"); return; },
        };

        if let Err(e) = self.do_delete(parent_path, name, context) {
            tracing::error!(parent_path, name, "delete cleanup: do_delete: {e:?}");
        }
    }

    fn do_delete(&self, parent_path: &str, name: &str, handle: &FileHandle) -> Result<()> {
        let parent_inode_num = self.resolve_path(parent_path)
            .map_err(|e| { tracing::error!(parent_path, "do_delete: resolve_path: {e}"); STATUS_INTERNAL_ERROR })?;
        let (inode_num, is_dir) = match handle {
            FileHandle::File { inode_num, .. } => (*inode_num, false),
            FileHandle::Directory { inode_num, .. } => (*inode_num, true),
            FileHandle::Symlink { inode_num, .. } => (*inode_num, false),
        };
        let inode = self.inode(inode_num).map_err(|e| { tracing::error!(inode_num, "do_delete: read inode: {e}"); STATUS_INTERNAL_ERROR })?;

        let mut journal = self.journal.lock();
        let mut txn = journal.begin_transaction();

        {
            let mut gdt = self.gdt.lock();

            // Remove directory entry — no alloc needed.
            dir_remove_entry(&self.dev, &self.sb, &gdt, &mut txn,
                parent_inode_num, name)
                .map_err(|e| { tracing::error!(parent_inode_num, name, "do_delete: dir_remove_entry: {e}"); STATUS_INTERNAL_ERROR })?;

            let new_links = inode.links_count.saturating_sub(1);
            if new_links == 0 {
                let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
                extent_truncate(&self.dev, &self.sb, &mut alloc, &mut txn, inode_num, 0)
                    .map_err(|e| { tracing::error!(inode_num, "do_delete: extent_truncate: {e}"); STATUS_INTERNAL_ERROR })?;
                alloc.free_inode(&mut txn, inode_num)
                    .map_err(|e| { tracing::error!(inode_num, "do_delete: free_inode: {e}"); STATUS_INTERNAL_ERROR })?;
            } else {
                update_inode(&self.dev, &self.sb, &gdt, &mut txn, inode_num,
                    InodeUpdate::default().with_links_count(new_links).with_ctime(now()))
                    .map_err(|e| { tracing::error!(inode_num, "do_delete: update_inode (links): {e}"); STATUS_INTERNAL_ERROR })?;
            }

            if is_dir {
                let parent_inode = self.inode(parent_inode_num)
                    .map_err(|e| { tracing::error!(parent_inode_num, "do_delete: read parent inode: {e}"); STATUS_INTERNAL_ERROR })?;
                let new_parent_links = parent_inode.links_count.saturating_sub(1);
                update_inode(&self.dev, &self.sb, &gdt, &mut txn, parent_inode_num,
                    InodeUpdate::default().with_links_count(new_parent_links).with_ctime(now()))
                    .map_err(|e| { tracing::error!(parent_inode_num, "do_delete: update_inode (parent links): {e}"); STATUS_INTERNAL_ERROR })?;
            }
        }

        journal.commit(&self.dev, txn).map_err(|e| { tracing::error!("do_delete: journal.commit: {e}"); STATUS_INTERNAL_ERROR })?;
        Ok(())
    }
}
