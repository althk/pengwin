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

pub fn read_sectors(
    dev: &dyn BlockDevice,
    start: u64,
    count: u64,
) -> Result<Vec<u8>, BlockDeviceError> {
    let mut out = vec![0u8; (count * 512) as usize];
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

    pub(super) struct MemoryDevice(pub(super) Vec<u8>);

    impl BlockDevice for MemoryDevice {
        fn read_sector(&self, sector_index: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            let total = self.sector_count();
            if sector_index >= total {
                return Err(BlockDeviceError::OutOfRange(sector_index, total));
            }
            let offset = sector_index as usize * 512;
            buf.copy_from_slice(&self.0[offset..offset + 512]);
            Ok(())
        }

        fn sector_count(&self) -> u64 {
            self.0.len() as u64 / 512
        }
    }

    fn make_device(sectors: u64) -> MemoryDevice {
        let mut data = vec![0u8; (sectors * 512) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        MemoryDevice(data)
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
    fn read_sectors_multi() {
        let dev = make_device(4);
        let data = read_sectors(&dev, 1, 3).unwrap();
        assert_eq!(data.len(), 3 * 512);
        assert_eq!(data[0], (512 % 256) as u8);
        assert_eq!(data[512], (1024 % 256) as u8);
        assert_eq!(data[1024], (1536 % 256) as u8);
    }
}
