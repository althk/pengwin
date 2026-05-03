use super::{BlockDevice, BlockDeviceError};

/// A BlockDevice backed by a flat disk image file (.img, .raw).
#[derive(Debug)]
pub struct ImageFileDevice {
    file: std::fs::File,
    sector_count: u64,
}

impl ImageFileDevice {
    /// Open an image file read-only.
    ///
    /// # Errors
    /// - File not found
    /// - File size is not a multiple of 512
    pub fn open(path: &std::path::Path) -> Result<Self, BlockDeviceError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(false)
            .open(path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        if size % 512 != 0 {
            return Err(BlockDeviceError::InvalidGeometry(size));
        }
        Ok(Self { file, sector_count: size / 512 })
    }
}

#[cfg(unix)]
fn read_at_offset(file: &std::fs::File, buf: &mut [u8; 512], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_at_offset(file: &std::fs::File, buf: &mut [u8; 512], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)?;
    Ok(())
}

impl BlockDevice for ImageFileDevice {
    fn read_sector(&self, sector_index: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
        if sector_index >= self.sector_count {
            return Err(BlockDeviceError::OutOfRange(sector_index, self.sector_count));
        }
        let offset = sector_index * 512;
        read_at_offset(&self.file, buf, offset)?;
        Ok(())
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_temp_image(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn open_valid_image() {
        let data = vec![0u8; 2048]; // 4 sectors
        let tmp = make_temp_image(&data);
        let dev = ImageFileDevice::open(tmp.path()).unwrap();
        assert_eq!(dev.sector_count(), 4);
    }

    #[test]
    fn open_non_multiple() {
        let data = vec![0u8; 513];
        let tmp = make_temp_image(&data);
        let err = ImageFileDevice::open(tmp.path()).unwrap_err();
        assert!(matches!(err, BlockDeviceError::InvalidGeometry(513)));
    }

    #[test]
    fn read_first_sector() {
        let mut data = vec![0u8; 2048];
        for (i, b) in data[..512].iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        let tmp = make_temp_image(&data);
        let dev = ImageFileDevice::open(tmp.path()).unwrap();
        let mut buf = [0u8; 512];
        dev.read_sector(0, &mut buf).unwrap();
        for i in 0..512 {
            assert_eq!(buf[i], (i % 256) as u8, "mismatch at byte {i}");
        }
    }

    #[test]
    fn read_last_sector() {
        let mut data = vec![0u8; 2048];
        for (i, b) in data[1536..].iter_mut().enumerate() {
            *b = ((i + 1) % 256) as u8;
        }
        let tmp = make_temp_image(&data);
        let dev = ImageFileDevice::open(tmp.path()).unwrap();
        let mut buf = [0u8; 512];
        dev.read_sector(3, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
        assert_eq!(buf[1], 2);
    }

    #[test]
    fn read_out_of_bounds() {
        let data = vec![0u8; 2048]; // 4 sectors
        let tmp = make_temp_image(&data);
        let dev = ImageFileDevice::open(tmp.path()).unwrap();
        let mut buf = [0u8; 512];
        let err = dev.read_sector(4, &mut buf).unwrap_err();
        assert!(matches!(err, BlockDeviceError::OutOfRange(4, 4)));
    }
}
