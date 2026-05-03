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
        let inode = match context {
            FileHandle::File { inode, .. } => inode,
            FileHandle::Directory { .. } | FileHandle::Symlink { .. } => {
                return Err(STATUS_INVALID_DEVICE_REQUEST.into())
            }
        };

        let mut reader = ext4_core::file::FileReader::new(self.dev(), self.sb(), inode)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;

        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|_| STATUS_INTERNAL_ERROR)?;

        let n = reader.read(buffer).map_err(|_| STATUS_INTERNAL_ERROR)?;
        Ok(n as u32)
    }
}
