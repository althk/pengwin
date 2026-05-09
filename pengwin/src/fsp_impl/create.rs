use ext4_core::block_device::BlockDevice;
use ext4_core::alloc::{Allocator, inode_alloc};
use ext4_core::dir_write::{dir_add_entry, dir_file_type};
use ext4_core::inode_write::{update_inode, InodeUpdate};
use winfsp::filesystem::OpenFileInfo;
use winfsp::Result;
use winfsp::U16CStr;
use windows::Win32::Foundation::{
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_OBJECT_NAME_INVALID, STATUS_NOT_A_DIRECTORY,
    STATUS_OBJECT_NAME_COLLISION, STATUS_DISK_FULL, STATUS_INTERNAL_ERROR,
};

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;
use crate::fsp_impl::now;

/// Split "/parent/dir/name" into ("/parent/dir", "name").
pub(crate) fn split_path(path: &str) -> Option<(&str, &str)> {
    let path = path.trim_end_matches('/');
    let pos = path.rfind('/')?;
    let parent = if pos == 0 { "/" } else { &path[..pos] };
    let name = &path[pos + 1..];
    if name.is_empty() { return None; }
    Some((parent, name))
}

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    #[allow(clippy::too_many_arguments)]
    pub fn create_handle(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<FileHandle> {
        const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;

        let path = file_name.to_string_lossy();
        let path = path.replace('\\', "/");

        let (parent_path, name) = split_path(&path)
            .ok_or(STATUS_OBJECT_NAME_INVALID)?;

        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let mode: u16 = if is_dir { 0o040755 } else { 0o100644 };

        let parent_inode_num = self.resolve_path(parent_path)
            .map_err(|_| STATUS_OBJECT_PATH_NOT_FOUND)?;
        let parent_inode = self.inode(parent_inode_num)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        if !parent_inode.is_dir() {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }

        if self.lookup(&parent_inode, name).map_err(|_| STATUS_INTERNAL_ERROR)?.is_some() {
            return Err(STATUS_OBJECT_NAME_COLLISION.into());
        }

        let mut journal = self.journal.lock();
        let mut txn = journal.begin_transaction();

        let new_inode_num = {
            let mut gdt = self.gdt.lock();
            let parent_group = ((parent_inode_num - 1) / self.sb.inodes_per_group) as usize;

            // Allocate inode (alloc mutably borrows gdt, then is dropped to release it).
            let new_inode_num = {
                let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
                alloc.alloc_inode(&mut txn, is_dir, parent_group)
                    .map_err(|_| STATUS_DISK_FULL)?
            };

            inode_alloc::init_inode(&self.dev, &self.sb, &gdt, &mut txn,
                new_inode_num, mode, 0, 0)
                .map_err(|_| STATUS_INTERNAL_ERROR)?;

            let ft = if is_dir { dir_file_type::DIR } else { dir_file_type::REG_FILE };

            // dir_add_entry takes both `gdt: &GDT` and `alloc: &mut Alloc` where alloc holds
            // `&mut gdt`. Clone a snapshot for the read-only param to satisfy the borrow checker.
            {
                let gdt_snap = gdt.clone();
                let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
                dir_add_entry(&self.dev, &self.sb, &gdt_snap, &mut alloc, &mut txn,
                    parent_inode_num, name, new_inode_num, ft)
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;
            }

            if is_dir {
                {
                    let gdt_snap = gdt.clone();
                    let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
                    dir_add_entry(&self.dev, &self.sb, &gdt_snap, &mut alloc, &mut txn,
                        new_inode_num, ".", new_inode_num, dir_file_type::DIR)
                        .map_err(|_| STATUS_INTERNAL_ERROR)?;
                }
                {
                    let gdt_snap = gdt.clone();
                    let mut alloc = Allocator::new(&self.dev, &self.sb, &mut gdt);
                    dir_add_entry(&self.dev, &self.sb, &gdt_snap, &mut alloc, &mut txn,
                        new_inode_num, "..", parent_inode_num, dir_file_type::DIR)
                        .map_err(|_| STATUS_INTERNAL_ERROR)?;
                }
                let new_links = parent_inode.links_count + 1;
                update_inode(&self.dev, &self.sb, &gdt, &mut txn, parent_inode_num,
                    InodeUpdate::default().with_links_count(new_links).with_ctime(now()))
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;
            }

            new_inode_num
        };

        journal.commit(&self.dev, txn).map_err(|_| STATUS_INTERNAL_ERROR)?;
        drop(journal);

        let new_inode = self.inode(new_inode_num).map_err(|_| STATUS_INTERNAL_ERROR)?;
        *file_info.as_mut() = file_info_from_inode(&new_inode, new_inode_num);

        let handle = if is_dir {
            FileHandle::Directory {
                inode_num: new_inode_num,
                inode: new_inode,
                dir_buffer: winfsp::filesystem::DirBuffer::new(),
            }
        } else {
            FileHandle::File { inode_num: new_inode_num, inode: new_inode }
        };
        Ok(handle)
    }
}
