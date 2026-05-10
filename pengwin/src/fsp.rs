use winfsp::filesystem::FileSystemContext;
use winfsp::host::{FileSystemHost, VolumeParams};

#[derive(Debug, thiserror::Error)]
pub enum FspError {
    #[error("failed to enable SeCreateSymbolicLinkPrivilege: {0}")]
    PrivilegeError(String),
    #[error("WinFsp is not installed. Download from https://winfsp.dev\nDetail: {0}")]
    WinFspNotInstalled(String),

    #[error("failed to create filesystem host: {0}")]
    HostCreation(String),

    #[error("mount failed at {0}: {1}")]
    MountFailed(String, String),
}

/// Call once at startup. Returns Err with a user-friendly message if WinFsp DLL is absent.
pub fn check_winfsp_installed() -> Result<winfsp::FspInit, FspError> {
    winfsp::winfsp_init().map_err(|e| FspError::WinFspNotInstalled(e.to_string()))
}

pub struct Ext4Host<T: FileSystemContext> {
    host: FileSystemHost<T>,
}

impl<T: FileSystemContext + 'static> Ext4Host<T> {
    pub fn new(fs: T, read_only: bool) -> Result<Self, FspError> {
        let mut params = VolumeParams::default();
        params
            .sector_size(512)
            .sectors_per_allocation_unit(8) // 4 KiB clusters
            .file_info_timeout(1000)
            .volume_info_timeout(1000)
            .case_sensitive_search(true)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .read_only_volume(read_only)
            .post_cleanup_when_modified_only(true)
            .flush_and_purge_on_cleanup(false);

        let host =
            FileSystemHost::new(params, fs).map_err(|e| FspError::HostCreation(e.to_string()))?;
        Ok(Self { host })
    }

    pub fn mount(&mut self, mountpoint: &str) -> Result<(), FspError> {
        // Drive letters go through DefineDosDevice and need no special privilege.
        // Directory mount points use a reparse point; attempt to acquire
        // SeCreateSymbolicLinkPrivilege but don't hard-fail — on Developer Mode
        // builds WinFSP can create the mount without it.
        let is_dir_mount = std::path::Path::new(mountpoint).components().count() > 1;
        if is_dir_mount {
            if let Err(e) = enable_symlink_privilege() {
                tracing::debug!("SeCreateSymbolicLinkPrivilege not acquired ({}); proceeding anyway", e);
            }
        }
        self.host
            .mount(mountpoint)
            .map_err(|e| FspError::MountFailed(mountpoint.to_string(), e.to_string()))
    }

    pub fn unmount(&mut self) {
        self.host.unmount();
    }

    /// Start dispatcher threads and block until stopped.
    pub fn start(&mut self) -> Result<(), FspError> {
        self.host
            .start()
            .map_err(|e| FspError::HostCreation(e.to_string()))
    }

    pub fn stop(&mut self) {
        self.host.stop();
    }
}

/// Enable SeCreateSymbolicLinkPrivilege in the current process token so that
/// WinFsp can create a directory junction mount point.
#[cfg(windows)]
fn enable_symlink_privilege() -> Result<(), FspError> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| FspError::PrivilegeError(e.to_string()))?;

        let mut luid = windows::Win32::Foundation::LUID::default();
        LookupPrivilegeValueW(
            None,
            windows::core::w!("SeCreateSymbolicLinkPrivilege"),
            &mut luid,
        )
        .map_err(|e| FspError::PrivilegeError(e.to_string()))?;

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None)
            .map_err(|e| FspError::PrivilegeError(e.to_string()))?;

        if windows::Win32::Foundation::GetLastError() == windows::Win32::Foundation::ERROR_NOT_ALL_ASSIGNED {
            return Err(FspError::PrivilegeError(
                "SeCreateSymbolicLinkPrivilege not held by process; run as Administrator or enable Developer Mode".to_string()
            ));
        }


        windows::Win32::Foundation::CloseHandle(token)
            .map_err(|e| FspError::PrivilegeError(e.to_string()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn enable_symlink_privilege() -> Result<(), FspError> {
    Ok(())
}
