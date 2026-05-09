use super::{BlockDevice, BlockDeviceError};

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::FileExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    };

    /// A BlockDevice backed by a raw Windows disk or partition.
    ///
    /// Path format: `\\.\PhysicalDriveN` or `\\.\HarddiskVolumeN`
    pub struct RawDiskDevice {
        file: std::fs::File,
        sector_count: u64,
    }

    impl RawDiskDevice {
        pub fn open(path: &std::path::Path) -> Result<Self, BlockDeviceError> {
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_NO_BUFFERING,
                    0 as HANDLE,
                )
            };

            if handle == INVALID_HANDLE_VALUE {
                return Err(BlockDeviceError::Io(std::io::Error::last_os_error()));
            }

            let file = unsafe { std::fs::File::from_raw_handle(handle as *mut _) };

            let sector_count = query_sector_count(handle)?;

            Ok(Self { file, sector_count })
        }
    }

    fn query_sector_count(handle: HANDLE) -> Result<u64, BlockDeviceError> {
        let mut geometry = unsafe { std::mem::zeroed::<DISK_GEOMETRY_EX>() };
        let mut bytes_returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                std::ptr::null(),
                0,
                &mut geometry as *mut _ as *mut _,
                std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(BlockDeviceError::Io(std::io::Error::last_os_error()));
        }
        Ok(geometry.DiskSize as u64 / 512)
    }

    impl BlockDevice for RawDiskDevice {
        fn read_sector(&self, sector_index: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            if sector_index >= self.sector_count {
                return Err(BlockDeviceError::OutOfRange(sector_index, self.sector_count));
            }
            let offset = sector_index
                .checked_mul(512)
                .ok_or(BlockDeviceError::OutOfRange(sector_index, self.sector_count))?;
            self.file.seek_read(buf, offset)?;
            Ok(())
        }

        fn sector_count(&self) -> u64 {
            self.sector_count
        }

        fn write_sector(&self, sector_index: u64, buf: &[u8; 512]) -> Result<(), BlockDeviceError> {
            if sector_index >= self.sector_count {
                return Err(BlockDeviceError::OutOfRange(sector_index, self.sector_count));
            }
            let offset = sector_index
                .checked_mul(512)
                .ok_or(BlockDeviceError::OutOfRange(sector_index, self.sector_count))?;
            self.file.seek_write(buf, offset)?;
            Ok(())
        }

        fn flush(&self) -> Result<(), BlockDeviceError> {
            use std::os::windows::io::AsRawHandle;
            if unsafe { FlushFileBuffers(self.file.as_raw_handle() as _) } == 0 {
                return Err(BlockDeviceError::Io(std::io::Error::last_os_error()));
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use windows_impl::RawDiskDevice;

#[cfg(not(windows))]
pub struct RawDiskDevice;

#[cfg(not(windows))]
impl RawDiskDevice {
    pub fn open(_path: &std::path::Path) -> Result<Self, BlockDeviceError> {
        Err(BlockDeviceError::NotSupported("RawDiskDevice is Windows-only"))
    }
}

#[cfg(test)]
mod tests {
    use crate::block_device::{BlockDevice, BlockDeviceError};

    struct MemoryDevice(std::sync::Mutex<Vec<u8>>);

    impl BlockDevice for MemoryDevice {
        fn read_sector(&self, sector_index: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            let data = self.0.lock().unwrap();
            let total = data.len() as u64 / 512;
            if sector_index >= total {
                return Err(BlockDeviceError::OutOfRange(sector_index, total));
            }
            let offset = sector_index as usize * 512;
            buf.copy_from_slice(&data[offset..offset + 512]);
            Ok(())
        }

        fn sector_count(&self) -> u64 {
            self.0.lock().unwrap().len() as u64 / 512
        }

        fn write_sector(&self, sector_index: u64, buf: &[u8; 512]) -> Result<(), BlockDeviceError> {
            let mut data = self.0.lock().unwrap();
            let total = data.len() as u64 / 512;
            if sector_index >= total {
                return Err(BlockDeviceError::OutOfRange(sector_index, total));
            }
            let offset = sector_index as usize * 512;
            data[offset..offset + 512].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&self) -> Result<(), BlockDeviceError> { Ok(()) }
    }

    #[test]
    fn memory_device_bounds_check() {
        let data = vec![0xABu8; 1024]; // 2 sectors
        let dev = MemoryDevice(std::sync::Mutex::new(data));
        let mut buf = [0u8; 512];
        dev.read_sector(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);

        let err = dev.read_sector(2, &mut buf).unwrap_err();
        assert!(matches!(err, BlockDeviceError::OutOfRange(2, 2)));
    }

    #[test]
    #[ignore = "requires admin and a physical disk"]
    fn open_physical_drive() {
        use super::RawDiskDevice;
        let path = std::path::Path::new(r"\\.\PhysicalDrive0");
        let dev = RawDiskDevice::open(path).unwrap();
        assert!(dev.sector_count() > 0);
    }
}
