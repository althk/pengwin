use zerocopy::little_endian::U16;
use zerocopy::FromBytes;
use crate::block_device::{BlockDevice, BlockDeviceError, read_sectors, write_sectors};
use crate::superblock::{Superblock, SuperblockError, RawSuperblock, state};
use crate::group_desc::{GroupDescTable, GroupDescError};
use super::{JournalError, check_and_recover};
use super::writer::JournalWriter;

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("filesystem error flag is set: {0}")]
    FilesystemError(String),

    #[error("journal error: {0}")]
    Journal(#[from] JournalError),

    #[error("superblock error: {0}")]
    Superblock(#[from] SuperblockError),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] GroupDescError),

    #[error("block device error: {0}")]
    BlockDevice(#[from] BlockDeviceError),

    #[error("superblock buffer is too small")]
    InvalidBuffer,
}

/// A read-write mounted ext4 filesystem.
pub struct Ext4FsRw<D: BlockDevice> {
    pub dev: D,
    pub sb: Superblock,
    pub gdt: GroupDescTable,
    pub journal: JournalWriter,
}

impl<D: BlockDevice + 'static> Ext4FsRw<D> {
    /// Mount the filesystem read-write, replaying the journal if needed.
    ///
    /// Safety sequence (enforced in order):
    /// 1. Parse superblock.
    /// 2. Refuse if ERROR_FS bit is set.
    /// 3. Load group descriptor table.
    /// 4. Check and replay journal.
    /// 5. Set dirty flag (clear VALID_FS).
    pub fn open_rw(dev: D) -> Result<Self, MountError> {
        let sb = crate::superblock::parse(&dev)?;

        if sb.state_has_error() {
            return Err(MountError::FilesystemError(
                "filesystem has errors — run e2fsck before mounting read-write".into(),
            ));
        }

        let gdt = GroupDescTable::load(&dev, &sb)?;
        let journal = check_and_recover(&dev, &sb, &gdt)?;

        set_dirty_flag(&dev, &sb)?;
        dev.flush()?;

        tracing::info!("filesystem mounted read-write");
        Ok(Ext4FsRw {
            dev,
            sb,
            gdt,
            journal: JournalWriter::new(&journal),
        })
    }

    /// Cleanly unmount: flush pending journal state, set VALID_FS, flush device.
    ///
    /// Final write barrier is applied before returning.
    pub fn unmount(mut self) -> Result<(), MountError> {
        self.journal.flush_pending(&self.dev)?;
        clear_dirty_flag(&self.dev, &self.sb)?;
        self.dev.flush()?;
        tracing::info!("filesystem cleanly unmounted");
        Ok(())
    }
}

// Superblock lives at byte 1024 = sectors 2–3 (two 512-byte sectors).
// s_state is at byte offset 58 within the superblock = device byte 1024 + 58.

fn read_raw_superblock_sectors(dev: &dyn BlockDevice) -> Result<Vec<u8>, MountError> {
    Ok(read_sectors(dev, 2, 2)?)
}

/// Clear VALID_FS in s_state — marks filesystem as "mounted / dirty".
pub fn set_dirty_flag(dev: &dyn BlockDevice, _sb: &Superblock) -> Result<(), MountError> {
    let mut data = read_raw_superblock_sectors(dev)?;
    let raw = RawSuperblock::mut_from(&mut data[..])
        .ok_or(MountError::InvalidBuffer)?;
    let current = raw.s_state.get();
    raw.s_state = U16::new(current & !state::VALID_FS);
    write_sectors(dev, 2, &data)?;
    Ok(())
}

/// Set VALID_FS in s_state — marks filesystem as cleanly unmounted.
pub fn clear_dirty_flag(dev: &dyn BlockDevice, _sb: &Superblock) -> Result<(), MountError> {
    let mut data = read_raw_superblock_sectors(dev)?;
    let raw = RawSuperblock::mut_from(&mut data[..])
        .ok_or(MountError::InvalidBuffer)?;
    let current = raw.s_state.get();
    raw.s_state = U16::new(current | state::VALID_FS);
    write_sectors(dev, 2, &data)?;
    dev.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDeviceError;
    use crate::superblock::{RawSuperblock, incompat};
    use zerocopy::{AsBytes, FromZeroes, little_endian::{U16, U32}};

    struct MemDev(std::sync::Mutex<Vec<u8>>);

    impl BlockDevice for MemDev {
        fn read_sector(&self, idx: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            let data = self.0.lock().unwrap();
            let total = data.len() as u64 / 512;
            if idx >= total { return Err(BlockDeviceError::OutOfRange(idx, total)); }
            let off = idx as usize * 512;
            buf.copy_from_slice(&data[off..off + 512]);
            Ok(())
        }
        fn sector_count(&self) -> u64 { self.0.lock().unwrap().len() as u64 / 512 }
        fn write_sector(&self, idx: u64, buf: &[u8; 512]) -> Result<(), BlockDeviceError> {
            let mut data = self.0.lock().unwrap();
            let total = data.len() as u64 / 512;
            if idx >= total { return Err(BlockDeviceError::OutOfRange(idx, total)); }
            let off = idx as usize * 512;
            data[off..off + 512].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> Result<(), BlockDeviceError> { Ok(()) }
    }

    fn valid_raw_sb(state_val: u16) -> RawSuperblock {
        let mut sb = RawSuperblock::new_zeroed();
        sb.s_magic            = U16::new(0xEF53);
        sb.s_log_block_size   = U32::new(2); // 4096
        sb.s_inodes_count     = U32::new(1024);
        sb.s_inodes_per_group = U32::new(256);
        sb.s_blocks_per_group = U32::new(32768);
        sb.s_blocks_count_lo  = U32::new(8192);
        sb.s_first_data_block = U32::new(0);
        sb.s_desc_size        = U16::new(64);
        sb.s_feature_incompat = U32::new(incompat::EXTENTS | incompat::FILETYPE);
        sb.s_state            = U16::new(state_val);
        sb
    }

    /// Build a minimal device: 2 KiB boot + 1 KiB superblock padded to 2 sectors.
    fn make_dev_with_sb(state_val: u16) -> MemDev {
        let raw_sb = valid_raw_sb(state_val);
        // Device needs at least sectors 0..4 (2 KiB) for superblock at byte 1024.
        let mut data = vec![0u8; 4 * 512];
        data[1024..2048].copy_from_slice(raw_sb.as_bytes());
        MemDev(std::sync::Mutex::new(data))
    }

    #[test]
    fn set_dirty_flag_clears_valid_fs() {
        let dev = make_dev_with_sb(state::VALID_FS);
        let sb = crate::superblock::parse(&dev).unwrap();
        assert!(sb.was_cleanly_unmounted());

        set_dirty_flag(&dev, &sb).unwrap();

        let sb2 = crate::superblock::parse(&dev).unwrap();
        assert!(!sb2.was_cleanly_unmounted(), "VALID_FS should be cleared after set_dirty_flag");
    }

    #[test]
    fn clear_dirty_flag_sets_valid_fs() {
        let dev = make_dev_with_sb(0); // no VALID_FS
        let sb = crate::superblock::parse(&dev).unwrap();
        assert!(!sb.was_cleanly_unmounted());

        clear_dirty_flag(&dev, &sb).unwrap();

        let sb2 = crate::superblock::parse(&dev).unwrap();
        assert!(sb2.was_cleanly_unmounted(), "VALID_FS should be set after clear_dirty_flag");
    }

    #[test]
    fn error_fs_state_detected() {
        let dev = make_dev_with_sb(state::ERROR_FS);
        let sb = crate::superblock::parse(&dev).unwrap();
        assert!(sb.state_has_error());
        assert!(!sb.was_cleanly_unmounted());
    }

    #[test]
    fn state_has_error_false_for_valid_fs() {
        let dev = make_dev_with_sb(state::VALID_FS);
        let sb = crate::superblock::parse(&dev).unwrap();
        assert!(!sb.state_has_error());
    }
}
