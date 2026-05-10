use ext4_core::block_device::BlockDevice;
use ext4_core::inode::Inode;
use winfsp::filesystem::FileInfo;
use winfsp::Result;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT,
};

use crate::fs_context::{Ext4Fs, FileHandle};

// IO_REPARSE_TAG_SYMLINK (Windows SDK constant)
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

pub fn file_info_from_inode(inode: &Inode, inode_num: u32) -> FileInfo {
    // Map ext4 mode bits to NTFS-style attributes. Owner-write missing => READONLY.
    let mut attrs = if inode.mode & 0o200 == 0 {
        FILE_ATTRIBUTE_READONLY.0
    } else {
        FILE_ATTRIBUTE_NORMAL.0
    };
    if inode.is_dir() {
        // Directory bit is non-exclusive with NORMAL; clear NORMAL when adding dir bit.
        attrs = (attrs & !FILE_ATTRIBUTE_NORMAL.0) | FILE_ATTRIBUTE_DIRECTORY.0;
    }
    let reparse_tag = if inode.is_symlink() {
        attrs |= FILE_ATTRIBUTE_REPARSE_POINT.0;
        IO_REPARSE_TAG_SYMLINK
    } else {
        0
    };

    let to_filetime = |unix_secs: u32| -> u64 {
        (unix_secs as u64 + 11_644_473_600) * 10_000_000
    };

    FileInfo {
        file_attributes: attrs,
        reparse_tag,
        // Round to the volume cluster size (sectors_per_allocation_unit * sector_size = 8 * 512 = 4096).
        // Reporting a 512-aligned value smaller than the cluster makes the kernel cache manager
        // refuse to flush full pages, hanging WriteFile/Close on newly created files.
        allocation_size: (inode.size + 4095) & !4095,
        file_size: inode.size,
        creation_time: to_filetime(inode.ctime),
        last_access_time: to_filetime(inode.atime),
        last_write_time: to_filetime(inode.mtime),
        change_time: to_filetime(inode.mtime),
        index_number: inode_num as u64,
        hard_links: inode.links_count as u32,
        ea_size: 0,
    }
}


impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn file_info_for_handle(
        &self,
        context: &FileHandle,
        out: &mut FileInfo,
    ) -> Result<()> {
        let inode_num = match context {
            FileHandle::File { inode_num, .. }
            | FileHandle::Directory { inode_num, .. }
            | FileHandle::Symlink { inode_num, .. } => *inode_num,
        };
        // Re-read from disk: the inode in FileHandle is stale after writes.
        let inode = self.inode(inode_num)
            .map_err(|_| windows::Win32::Foundation::STATUS_INTERNAL_ERROR)?;
        *out = file_info_from_inode(&inode, inode_num);
        Ok(())
    }

}
