use std::ffi::c_void;

use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::{DirMarker, FileSecurity, FileInfo, FileSystemContext, OpenFileInfo, VolumeInfo};
use winfsp::{Result, U16CStr};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

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

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: FILE_ACCESS_RIGHTS,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        security_descriptor: Option<&[c_void]>,
        allocation_size: u64,
        extra_buffer: Option<&[u8]>,
        extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        self.create_handle(
            file_name,
            create_options,
            granted_access,
            file_attributes,
            security_descriptor,
            allocation_size,
            extra_buffer,
            extra_buffer_is_reparse_point,
            file_info,
        )
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
        tracing::debug!(target: "pengwin::dispatch", "cleanup ENTRY flags=0x{flags:x}");
        self.cleanup_handle(context, file_name, flags);
        tracing::debug!(target: "pengwin::dispatch", "cleanup EXIT");
    }

    fn close(&self, context: Self::FileContext) {
        tracing::debug!(target: "pengwin::dispatch", "close ENTRY");
        self.close_handle(context);
        tracing::debug!(target: "pengwin::dispatch", "close EXIT");
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        tracing::debug!(target: "pengwin::dispatch", "get_file_info ENTRY");
        let r = self.file_info_for_handle(context, file_info);
        tracing::debug!(target: "pengwin::dispatch", "get_file_info EXIT ok={}", r.is_ok());
        r
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

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        tracing::debug!(target: "pengwin::dispatch", "write ENTRY len={} off={offset} eof={write_to_eof} cstr={constrained_io}", buffer.len());
        let r = self.write_file_data_cb(context, buffer, offset, write_to_eof, constrained_io, file_info);
        tracing::debug!(target: "pengwin::dispatch", "write EXIT ok={}", r.is_ok());
        r
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        tracing::debug!(target: "pengwin::dispatch", "set_file_size ENTRY new_size={new_size} set_alloc={set_allocation_size}");
        let r = self.set_file_size_cb(context, new_size, set_allocation_size, file_info);
        tracing::debug!(target: "pengwin::dispatch", "set_file_size EXIT ok={}", r.is_ok());
        r
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        tracing::debug!(target: "pengwin::dispatch", "set_basic_info ENTRY");
        let r = self.set_basic_info_cb(
            context,
            file_attributes,
            creation_time,
            last_access_time,
            last_write_time,
            last_change_time,
            file_info,
        );
        tracing::debug!(target: "pengwin::dispatch", "set_basic_info EXIT ok={}", r.is_ok());
        r
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        tracing::debug!(target: "pengwin::dispatch", "set_delete ENTRY delete={delete_file}");
        let r = self.set_delete_cb(context, file_name, delete_file);
        tracing::debug!(target: "pengwin::dispatch", "set_delete EXIT ok={}", r.is_ok());
        r
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<()> {
        tracing::debug!(target: "pengwin::dispatch", "rename ENTRY");
        let r = self.rename_handle(context, file_name, new_file_name, replace_if_exists);
        tracing::debug!(target: "pengwin::dispatch", "rename EXIT ok={}", r.is_ok());
        r
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        tracing::debug!(target: "pengwin::dispatch", "flush ENTRY");
        let r = self.flush_cb(context, file_info);
        tracing::debug!(target: "pengwin::dispatch", "flush EXIT ok={}", r.is_ok());
        r
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
