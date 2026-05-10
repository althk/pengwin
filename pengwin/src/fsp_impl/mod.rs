pub mod file_info;
pub mod open_close;
pub mod read_directory;
pub mod read_file;
pub mod security;
pub mod volume_info;
pub mod write_file;
pub mod set_file_size;
pub mod create;
pub mod delete;
pub mod rename;
pub mod set_file_info;
pub mod flush;

/// NTSTATUS returned for write attempts on a read-only mount.
/// Maps to the "media is write-protected" error in Win32.
pub(crate) const STATUS_MEDIA_WRITE_PROTECTED: windows::Win32::Foundation::NTSTATUS =
    windows::Win32::Foundation::NTSTATUS(0xC000_00A2u32 as i32);

/// Current time as a Unix timestamp (seconds since epoch).
pub(crate) fn now() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}
