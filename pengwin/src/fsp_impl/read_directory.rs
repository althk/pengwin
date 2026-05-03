use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::{DirInfo, DirMarker, WideNameInfo};
use winfsp::{Result, U16CStr};
use windows::Win32::Foundation::{STATUS_INTERNAL_ERROR, STATUS_NOT_A_DIRECTORY};

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn read_dir_entries(
        &self,
        context: &FileHandle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        let (inode, dir_buffer) = match context {
            FileHandle::Directory { inode, dir_buffer, .. } => (inode, dir_buffer),
            FileHandle::File { .. } | FileHandle::Symlink { .. } => {
                return Err(STATUS_NOT_A_DIRECTORY.into())
            }
        };

        // On first call (no marker), populate the DirBuffer from ext4.
        // On subsequent calls WinFsp re-uses the cached buffer with an updated marker.
        let lock = dir_buffer.acquire(marker.is_none(), None)?;

        if marker.is_none() {
            let entries = self
                .read_dir(inode)
                .map_err(|_| STATUS_INTERNAL_ERROR)?;

            for entry in &entries {
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                let entry_inode = self
                    .inode(entry.inode_num)
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;

                let mut dir_info = DirInfo::<255>::new();
                *dir_info.file_info_mut() = file_info_from_inode(&entry_inode, entry.inode_num);
                dir_info
                    .set_name(entry.name.as_str())
                    .map_err(|_| STATUS_INTERNAL_ERROR)?;

                lock.write(&mut dir_info).map_err(|_| STATUS_INTERNAL_ERROR)?;
            }
        }

        // Release the lock before reading.
        drop(lock);

        Ok(dir_buffer.read(marker, buffer))
    }
}
