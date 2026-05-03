use ext4_core::block_device::BlockDevice;
use ext4_core::inode::mode;
use winfsp::filesystem::OpenFileInfo;
use winfsp::Result;
use winfsp::U16CStr;
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_NOT_A_REPARSE_POINT,
    STATUS_BUFFER_TOO_SMALL, STATUS_NAME_TOO_LONG,
};
use winfsp_sys::FILE_ACCESS_RIGHTS;

use crate::fs_context::{Ext4Fs, FileHandle};
use crate::fsp_impl::file_info::file_info_from_inode;

// IO_REPARSE_TAG_SYMLINK (Windows SDK)
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

// SYMLINK_FLAG_RELATIVE: set when the symlink target is relative
const SYMLINK_FLAG_RELATIVE: u32 = 0x0000_0001;

/// Encode a Unix symlink target into a Windows `REPARSE_DATA_BUFFER` for `IO_REPARSE_TAG_SYMLINK`.
///
/// Layout (little-endian):
/// - u32 ReparseTag
/// - u16 ReparseDataLength
/// - u16 Reserved (0)
/// - u16 SubstituteNameOffset
/// - u16 SubstituteNameLength
/// - u16 PrintNameOffset
/// - u16 PrintNameLength
/// - u32 Flags
/// - [u8]  PathBuffer (SubstituteName immediately followed by PrintName, UTF-16LE, no NUL)
/// Maximum UTF-16 code units in a symlink target that can fit in a REPARSE_DATA_BUFFER.
/// The buffer's ReparseDataLength field is u16; the max payload is 0xFFFF bytes.
/// Payload = 12 (fixed symlink fields) + 2 * name_bytes.
/// So name_bytes ≤ (0xFFFF - 12) / 2 = 32761 bytes = 16380 UTF-16 code units.
const MAX_SYMLINK_UTF16_LEN: usize = 16380;

pub fn encode_symlink_reparse_buffer(unix_target: &str) -> winfsp::Result<Vec<u8>> {
    // Convert to Windows path: replace '/' with '\\'.
    // Absolute Unix paths get a leading '\' which Windows treats as relative to the
    // current drive root — acceptable for a read-only cross-platform mount.
    let win_target: String = unix_target.replace('/', "\\");

    // UTF-16LE encode, no null terminator.
    let target_utf16: Vec<u16> = win_target.encode_utf16().collect();
    if target_utf16.len() > MAX_SYMLINK_UTF16_LEN {
        return Err(STATUS_NAME_TOO_LONG.into());
    }

    let target_bytes: Vec<u8> = target_utf16
        .iter()
        .flat_map(|c| c.to_le_bytes())
        .collect();

    // SubstituteName and PrintName are identical; they share the same bytes.
    // SubstituteName: offset 0, length = target_bytes.len()
    // PrintName:      offset = target_bytes.len(), length = target_bytes.len()
    let name_len = target_bytes.len() as u16; // safe: target_bytes.len() ≤ 32761*2 = 65522
    let path_buffer_len = 2 * target_bytes.len(); // substitute + print

    // ReparseDataLength covers everything after the 8-byte common header.
    // That is: 4×u16 + u32 (Flags) + PathBuffer = 12 + path_buffer_len bytes.
    // safe: 12 + 65522*2 = 131056+12 — wait, path_buffer_len = 2*target_bytes.len() = 4*utf16_len
    // max path_buffer_len = 4 * 16380 = 65520; 12 + 65520 = 65532 ≤ 0xFFFF ✓
    let reparse_data_len = (12usize + path_buffer_len) as u16;

    let flags: u32 = if unix_target.starts_with('/') {
        // Absolute paths: no SYMLINK_FLAG_RELATIVE
        0
    } else {
        SYMLINK_FLAG_RELATIVE
    };

    let mut buf = Vec::with_capacity(8 + reparse_data_len as usize);
    buf.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes()); // ReparseTag
    buf.extend_from_slice(&reparse_data_len.to_le_bytes());       // ReparseDataLength
    buf.extend_from_slice(&0u16.to_le_bytes());                   // Reserved
    // SymbolicLinkReparseBuffer:
    buf.extend_from_slice(&0u16.to_le_bytes());                   // SubstituteNameOffset
    buf.extend_from_slice(&name_len.to_le_bytes());               // SubstituteNameLength
    buf.extend_from_slice(&name_len.to_le_bytes());               // PrintNameOffset
    buf.extend_from_slice(&name_len.to_le_bytes());               // PrintNameLength
    buf.extend_from_slice(&flags.to_le_bytes());                  // Flags
    buf.extend_from_slice(&target_bytes);                         // SubstituteName (UTF-16LE)
    buf.extend_from_slice(&target_bytes);                         // PrintName      (UTF-16LE)
    Ok(buf)
}

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    pub fn open_handle(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> Result<FileHandle> {
        let path = file_name.to_string_lossy();
        let path = path.replace('\\', "/");

        let inode_num = self.resolve_path(&path)
            .map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
        let inode = self.inode(inode_num)
            .map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;

        // Reject special files (device nodes, FIFOs, sockets) — not representable on Windows.
        let itype = inode.mode & mode::S_IFMT;
        if matches!(itype, mode::S_IFCHR | mode::S_IFBLK | mode::S_IFIFO | mode::S_IFSOCK) {
            tracing::warn!(
                path = %path,
                inode = inode_num,
                mode = format!("{:#06x}", inode.mode),
                "access denied: special file type not supported on Windows"
            );
            return Err(STATUS_ACCESS_DENIED.into());
        }



        *file_info.as_mut() = file_info_from_inode(&inode, inode_num);

        if inode.is_symlink() {
            let target = self.read_symlink_target(&inode)
                .map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
            tracing::debug!(path = %path, target = %target, "opened symlink");
            return Ok(FileHandle::Symlink { inode_num, inode, target });
        }

        if inode.is_dir() {
            Ok(FileHandle::Directory {
                inode_num,
                inode,
                dir_buffer: winfsp::filesystem::DirBuffer::new(),
            })
        } else {
            Ok(FileHandle::File { inode_num, inode })
        }
    }

    /// Write the `REPARSE_DATA_BUFFER` for a symlink handle into `buffer`.
    /// Returns the number of bytes written.
    pub fn get_reparse_point_data(
        &self,
        context: &FileHandle,
        buffer: &mut [u8],
    ) -> Result<u64> {
        let target = match context {
            FileHandle::Symlink { target, .. } => target,
            _ => return Err(STATUS_NOT_A_REPARSE_POINT.into()),
        };

        let encoded = encode_symlink_reparse_buffer(target)?;
        if buffer.len() < encoded.len() {
            return Err(STATUS_BUFFER_TOO_SMALL.into());
        }
        buffer[..encoded.len()].copy_from_slice(&encoded);
        Ok(encoded.len() as u64)
    }

    /// Write the `REPARSE_DATA_BUFFER` for a symlink looked up by path.
    /// Called by WinFsp during path resolution when following symlinks.
    pub fn get_reparse_point_data_by_name(
        &self,
        file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> Result<u64> {
        let path = file_name.to_string_lossy();
        let path = path.replace('\\', "/");

        let inode_num = self.resolve_path(&path)
            .map_err(|_| STATUS_NOT_A_REPARSE_POINT)?;
        let inode = self.inode(inode_num)
            .map_err(|_| STATUS_NOT_A_REPARSE_POINT)?;

        if !inode.is_symlink() {
            return Err(STATUS_NOT_A_REPARSE_POINT.into());
        }

        let target = self.read_symlink_target(&inode)
            .map_err(|_| STATUS_NOT_A_REPARSE_POINT)?;

        let encoded = encode_symlink_reparse_buffer(&target)?;
        if buffer.len() < encoded.len() {
            return Err(STATUS_BUFFER_TOO_SMALL.into());
        }
        buffer[..encoded.len()].copy_from_slice(&encoded);
        Ok(encoded.len() as u64)
    }

    pub fn cleanup_handle(
        &self,
        _context: &FileHandle,
        _file_name: Option<&U16CStr>,
        _flags: u32,
    ) {
    }

    pub fn close_handle(&self, _context: FileHandle) {}
}

#[cfg(test)]
mod tests {
    use super::encode_symlink_reparse_buffer;

    fn parse_reparse_header(buf: &[u8]) -> (u32, u16, u16) {
        let tag    = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let dlen   = u16::from_le_bytes(buf[4..6].try_into().unwrap());
        let _rsrvd = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        (tag, dlen, _rsrvd)
    }

    fn parse_symlink_fields(buf: &[u8]) -> (u16, u16, u16, u16, u32) {
        // After 8-byte common header
        let sub_off  = u16::from_le_bytes(buf[ 8..10].try_into().unwrap());
        let sub_len  = u16::from_le_bytes(buf[10..12].try_into().unwrap());
        let prn_off  = u16::from_le_bytes(buf[12..14].try_into().unwrap());
        let prn_len  = u16::from_le_bytes(buf[14..16].try_into().unwrap());
        let flags    = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        (sub_off, sub_len, prn_off, prn_len, flags)
    }

    fn decode_utf16(buf: &[u8]) -> String {
        let u16s: Vec<u16> = buf.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s).to_owned()
    }

    #[test]
    fn absolute_symlink_flag_zero() {
        let buf = encode_symlink_reparse_buffer("/hello.txt").unwrap();
        let (_tag, _dlen, _) = parse_reparse_header(&buf);
        let (_so, _sl, _po, _pl, flags) = parse_symlink_fields(&buf);
        assert_eq!(flags, 0, "absolute symlink must not set SYMLINK_FLAG_RELATIVE");
    }

    #[test]
    fn relative_symlink_flag_set() {
        let buf = encode_symlink_reparse_buffer("../sibling.txt").unwrap();
        let (_so, _sl, _po, _pl, flags) = parse_symlink_fields(&buf);
        assert_eq!(flags, 1, "relative symlink must set SYMLINK_FLAG_RELATIVE");
    }

    #[test]
    fn reparse_tag_correct() {
        let buf = encode_symlink_reparse_buffer("/foo").unwrap();
        let (tag, _, _) = parse_reparse_header(&buf);
        assert_eq!(tag, 0xA000_000C);
    }

    #[test]
    fn substitute_name_round_trips() {
        let target = "/hello/world.txt";
        let buf = encode_symlink_reparse_buffer(target).unwrap();
        let (sub_off, sub_len, _po, _pl, _flags) = parse_symlink_fields(&buf);
        let path_start = 20usize; // 8 header + 12 symlink fields
        let sub_bytes = &buf[path_start + sub_off as usize..path_start + sub_off as usize + sub_len as usize];
        let decoded = decode_utf16(sub_bytes);
        // '/' → '\\'
        assert_eq!(decoded, target.replace('/', "\\"));
    }

    #[test]
    fn buffer_size_consistent() {
        let buf = encode_symlink_reparse_buffer("/a/b/c").unwrap();
        let (_tag, dlen, _) = parse_reparse_header(&buf);
        // Total buf = 8 (common header) + dlen
        assert_eq!(buf.len(), 8 + dlen as usize);
    }

    #[test]
    fn too_long_target_returns_error() {
        // 16381 ASCII chars → 16381 UTF-16 code units > MAX_SYMLINK_UTF16_LEN (16380)
        let long_target: String = "a".repeat(16381);
        let result = encode_symlink_reparse_buffer(&long_target);
        assert!(result.is_err(), "expected error for oversized symlink target");
    }
}
