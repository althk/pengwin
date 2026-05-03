use std::ffi::c_void;

use ext4_core::block_device::BlockDevice;
use winfsp::filesystem::FileSecurity;
use winfsp::Result;
use winfsp::U16CStr;
use windows::Win32::Foundation::STATUS_OBJECT_NAME_NOT_FOUND;

use crate::fs_context::Ext4Fs;

/// Pre-computed DACL: O:S-1-1-0G:S-1-1-0D:P(A;;0x120089;;;WD)
/// This is a self-relative security descriptor with:
/// - Owner: S-1-1-0 (Everyone)
/// - Group: S-1-1-0 (Everyone)
/// - DACL: Present, Protected, one ACE: Allow FILE_GENERIC_READ for Everyone (S-1-1-0)
const FIXED_SD: &[u8] = &[
    0x01, 0x00, 0x04, 0x80, // Revision 1, Padding 0, Control 0x8004 (SE_SELF_RELATIVE | SE_DACL_PRESENT)
    0x30, 0x00, 0x00, 0x00, // Owner offset 0x30 (48)
    0x3C, 0x00, 0x00, 0x00, // Group offset 0x3C (60)
    0x00, 0x00, 0x00, 0x00, // SACL offset 0
    0x14, 0x00, 0x00, 0x00, // DACL offset 0x14 (20)

    // DACL Header (offset 0x14)
    0x02, 0x00, 0x1C, 0x00, // AclRevision 2, Sbz1 0, AclSize 28, AceCount 1
    0x01, 0x00, 0x00, 0x00, // Sbz2 0 (AceCount 1 is in low 2 bytes)

    // ACE (offset 0x1C)
    0x00, 0x00, 0x14, 0x00, // AceType 0 (ACCESS_ALLOWED), AceFlags 0, AceSize 20
    0x89, 0x00, 0x12, 0x00, // Mask 0x00120089 (FILE_GENERIC_READ)
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // SID S-1-1-0 (Everyone)

    // Owner SID (offset 0x30)
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // SID S-1-1-0 (Everyone)

    // Group SID (offset 0x3C)
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // SID S-1-1-0 (Everyone)
];

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
    ) -> Result<FileSecurity> {
        let path = file_name.to_string_lossy();
        let path = path.replace('\\', "/");

        let inode_num = self.resolve_path(&path)
            .map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
        let inode = self.inode(inode_num)
            .map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;


        if let Some(buf) = security_descriptor {
            let buf_bytes: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len())
            };
            let copy_len = buf_bytes.len().min(FIXED_SD.len());
            buf_bytes[..copy_len].copy_from_slice(&FIXED_SD[..copy_len]);
        }

        let attributes = inode_to_file_attributes(&inode);

        Ok(FileSecurity {
            reparse: inode.is_symlink(),
            sz_security_descriptor: FIXED_SD.len() as u64,
            attributes,
        })
    }
}

pub(crate) fn inode_to_file_attributes(inode: &ext4_core::inode::Inode) -> u32 {
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let mut attrs = FILE_ATTRIBUTE_READONLY.0;
    if inode.is_dir() {
        attrs |= FILE_ATTRIBUTE_DIRECTORY.0;
    }
    if inode.is_symlink() {
        attrs |= FILE_ATTRIBUTE_REPARSE_POINT.0;
    }
    attrs
}
