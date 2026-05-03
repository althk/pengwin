use std::ffi::c_void;

use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::{DirMarker, FileSecurity, FileInfo, FileSystemContext, OpenFileInfo, VolumeInfo};
use winfsp::{Result, U16CStr};
use winfsp_sys::FILE_ACCESS_RIGHTS;

use crate::fs_context::{Ext4Fs, FileHandle};

impl<D: BlockDevice + Send + Sync + 'static> FileSystemContext for Ext4Fs<D> {
    type FileContext = FileHandle;

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
        self.fill_volume_info(out_volume_info)
    }

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        self.security_by_name(file_name, security_descriptor)
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        self.open_handle(file_name, create_options, granted_access, file_info)
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        self.cleanup_handle(context, file_name, flags);
    }

    fn close(&self, context: Self::FileContext) {
        self.close_handle(context);
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        self.file_info_for_handle(context, file_info)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        self.read_dir_entries(context, pattern, marker, buffer)
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        self.read_file_data(context, buffer, offset)
    }

    fn get_reparse_point(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> Result<u64> {
        self.get_reparse_point_data(context, buffer)
    }

    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: &mut [u8],
    ) -> Result<u64> {
        self.get_reparse_point_data_by_name(file_name, buffer)
    }
}
