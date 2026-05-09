pub mod image_file;
pub mod raw_disk;

/// Abstraction over any block-addressable storage (disk image, raw partition, etc.)
///
/// Block size is fixed at 512 bytes at the device level (sector size).
/// The ext4 layer handles logical block sizes (1K–64K) on top of this.
pub trait BlockDevice: Send + Sync {
    /// Read exactly 512 bytes from sector `sector_index` into `buf`.
    ///
    /// # Errors
    /// Returns `BlockDeviceError` on I/O failure or out-of-bounds access.
    fn read_sector(&self, sector_index: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError>;

    /// Total number of 512-byte sectors on this device.
    fn sector_count(&self) -> u64;

    /// Write exactly 512 bytes to sector `sector_index`.
    ///
    /// Implementors must guarantee the write reaches the underlying medium
    /// before returning Ok — no internal buffering without explicit flush.
    fn write_sector(&self, sector_index: u64, buf: &[u8; 512]) -> Result<(), BlockDeviceError>;

    /// Flush any OS-level write buffers to physical media.
    /// Called after every journal commit.
    fn flush(&self) -> Result<(), BlockDeviceError>;
}

#[derive(Debug, thiserror::Error)]
pub enum BlockDeviceError {
    #[error("sector {0} is out of range (device has {1} sectors)")]
    OutOfRange(u64, u64),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("file size {0} is not a multiple of 512 bytes")]
    InvalidGeometry(u64),

    #[error("{0}")]
    NotSupported(&'static str),
}

pub fn write_sectors(
    dev: &dyn BlockDevice,
    start: u64,
    data: &[u8],
) -> Result<(), BlockDeviceError> {
    assert_eq!(data.len() % 512, 0, "data must be sector-aligned");
    for (i, chunk) in data.chunks_exact(512).enumerate() {
        dev.write_sector(start + i as u64, chunk.try_into().unwrap())?;
    }
    Ok(())
}

pub fn read_sectors(
    dev: &dyn BlockDevice,
    start: u64,
    count: u64,
) -> Result<Vec<u8>, BlockDeviceError> {
    let total_bytes = count
        .checked_mul(512)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(BlockDeviceError::InvalidGeometry(count))?;
    let mut out = vec![0u8; total_bytes];
    for i in 0..count {
        let offset = i as usize * 512;
        let buf: &mut [u8; 512] = (&mut out[offset..offset + 512])
            .try_into()
            .unwrap_or_else(|_| unreachable!("slice is always exactly 512 bytes"));
        dev.read_sector(start + i, buf)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) struct MemoryDevice(pub(super) std::sync::Mutex<Vec<u8>>);

    impl MemoryDevice {
        pub(super) fn new(data: Vec<u8>) -> Self { Self(std::sync::Mutex::new(data)) }
        #[allow(dead_code)]
        pub(super) fn data_snapshot(&self) -> Vec<u8> { self.0.lock().unwrap().clone() }
    }

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

    fn make_device(sectors: u64) -> MemoryDevice {
        let mut data = vec![0u8; (sectors * 512) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        MemoryDevice::new(data)
    }

    #[test]
    fn read_valid_sector() {
        let dev = make_device(4);
        let mut buf = [0u8; 512];
        dev.read_sector(1, &mut buf).unwrap();
        assert_eq!(buf[0], (512 % 256) as u8);
        assert_eq!(buf[1], (513 % 256) as u8);
    }

    #[test]
    fn read_out_of_bounds() {
        let dev = make_device(2);
        let mut buf = [0u8; 512];
        let err = dev.read_sector(2, &mut buf).unwrap_err();
        assert!(matches!(err, BlockDeviceError::OutOfRange(2, 2)));
    }

    #[test]
    fn read_sectors_count_overflow() {
        let dev = make_device(4);
        let err = read_sectors(&dev, 0, u64::MAX).unwrap_err();
        assert!(matches!(err, BlockDeviceError::InvalidGeometry(_)));
    }

    #[test]
    fn read_sectors_multi() {
        let dev = make_device(4);
        let data = read_sectors(&dev, 1, 3).unwrap();
        assert_eq!(data.len(), 3 * 512);
        assert_eq!(data[0], (512 % 256) as u8);
        assert_eq!(data[512], (1024 % 256) as u8);
        assert_eq!(data[1024], (1536 % 256) as u8);
    }

    #[test]
    fn write_and_read_back() {
        let dev = make_device(4);
        let mut pattern = [0u8; 512];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = (i ^ 0xAA) as u8;
        }
        dev.write_sector(1, &pattern).unwrap();
        let mut buf = [0u8; 512];
        dev.read_sector(1, &mut buf).unwrap();
        assert_eq!(buf, pattern);
    }

    #[test]
    fn write_out_of_bounds() {
        let dev = make_device(2);
        let buf = [0u8; 512];
        let err = dev.write_sector(2, &buf).unwrap_err();
        assert!(matches!(err, BlockDeviceError::OutOfRange(2, 2)));
    }

    #[test]
    fn flush_succeeds() {
        let dev = make_device(2);
        dev.flush().unwrap();
    }

    #[test]
    fn write_sectors_helper() {
        let dev = make_device(4);
        let data = vec![0xBBu8; 1024];
        write_sectors(&dev, 1, &data).unwrap();
        let mut buf = [0u8; 512];
        dev.read_sector(1, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));
        dev.read_sector(2, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));
    }
}
