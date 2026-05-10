use std::io::{Read, Seek, SeekFrom};

use ext4_core::block_device::BlockDevice;
use winfsp::Result;
use windows::Win32::Foundation::{STATUS_INTERNAL_ERROR, STATUS_INVALID_DEVICE_REQUEST};

use crate::fs_context::{Ext4Fs, FileHandle};

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn read_file_data(
        &self,
        context: &FileHandle,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<u32> {
        let inode_num = match context {
            FileHandle::File { inode_num, .. } => *inode_num,
            FileHandle::Directory { .. } | FileHandle::Symlink { .. } => {
                return Err(STATUS_INVALID_DEVICE_REQUEST.into())
            }
        };

        // Always re-read the inode from disk: writes update on-disk state but the
        // inode embedded in the FileHandle remains the snapshot taken at open time.
        let inode = self.inode(inode_num).map_err(|e| { tracing::error!(inode_num, "read: inode: {e}"); STATUS_INTERNAL_ERROR })?;
        let mut reader = ext4_core::file::FileReader::new(self.dev(), self.sb(), &inode)
            .map_err(|e| { tracing::error!(inode_num, "read: FileReader::new: {e}"); STATUS_INTERNAL_ERROR })?;

        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| { tracing::error!(offset, "read: seek: {e}"); STATUS_INTERNAL_ERROR })?;

        let n = reader.read(buffer).map_err(|e| { tracing::error!(offset, len = buffer.len(), "read: read: {e}"); STATUS_INTERNAL_ERROR })?;
        tracing::debug!(target: "pengwin::read", "read inode={inode_num} off={offset} req={} got={n} size={}", buffer.len(), inode.size);
        Ok(n as u32)
    }
}
