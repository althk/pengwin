use zerocopy::{FromBytes, FromZeroes, AsBytes};
use zerocopy::little_endian::{U16, U32};
use crate::block_device::{BlockDevice, BlockDeviceError, read_sectors};

// Offsets verified against Linux fs/ext4/ext4.h struct ext4_super_block.
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct RawSuperblock {
    pub s_inodes_count:         U32,    // 0
    pub s_blocks_count_lo:      U32,    // 4
    pub s_r_blocks_count_lo:    U32,    // 8
    pub s_free_blocks_count_lo: U32,    // 12
    pub s_free_inodes_count:    U32,    // 16
    pub s_first_data_block:     U32,    // 20
    pub s_log_block_size:       U32,    // 24
    _pad1: [u8; 4],                     // 28: s_log_cluster_size
    pub s_blocks_per_group:     U32,    // 32
    _pad2: [u8; 4],                     // 36: s_clusters_per_group
    pub s_inodes_per_group:     U32,    // 40
    _pad3: [u8; 12],                    // 44: s_mtime, s_wtime, s_mnt_count, s_max_mnt_count
    pub s_magic:                U16,    // 56
    pub s_state:                U16,    // 58
    _pad4: [u8; 4],                     // 60: s_errors, s_minor_rev_level
    pub s_rev_level:            U32,    // 64
    _pad5a: [u8; 20],                   // 68: s_lastcheck..s_def_resuid/resgid
    pub s_inode_size:           U16,    // 88
    _pad5b: [u8; 2],                    // 90: s_block_group_nr
    pub s_feature_compat:       U32,    // 92
    pub s_feature_incompat:     U32,    // 96
    pub s_feature_ro_compat:    U32,    // 100
    pub s_uuid:                 [u8; 16], // 104
    pub s_volume_name:          [u8; 16], // 120
    _pad6: [u8; 118],                   // 136..253: last_mounted, algo_usage_bitmap, etc.
    pub s_desc_size:            U16,    // 254
    _pad7: [u8; 80],                    // 256..335: s_default_mount_opts..s_jnl_blocks
    pub s_blocks_count_hi:      U32,    // 336
    pub s_r_blocks_count_hi:    U32,    // 340
    pub s_free_blocks_count_hi: U32,    // 344
    _pad8: [u8; 676],                   // 348..1023: remainder → total = 1024
}

const _: () = assert!(core::mem::size_of::<RawSuperblock>() == 1024);

pub mod incompat {
    pub const FILETYPE:         u32 = 0x0002;
    pub const RECOVER:          u32 = 0x0004;
    pub const EXTENTS:          u32 = 0x0040;
    pub const FLEX_BG:          u32 = 0x0200;
    pub const INLINE_DATA:      u32 = 0x8000;
    pub const ENCRYPT:          u32 = 0x10000;

    // INLINE_DATA stores file data directly in the inode block field; we do not
    // implement it, and silently treating such inodes as extent-based would
    // corrupt reads. Reject any image that uses it.
    // RECOVER is handled by journal replay (task 03); it does not block mounting.
    pub const UNSUPPORTED_MASK: u32 = ENCRYPT | INLINE_DATA;
}

pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x0001;
    pub const LARGE_FILE:   u32 = 0x0002;
    pub const HUGE_FILE:    u32 = 0x0008;
    pub const GDT_CSUM:     u32 = 0x0010;
    pub const DIR_NLINK:    u32 = 0x0020;
    pub const EXTRA_ISIZE:  u32 = 0x0040;
    pub const METADATA_CSUM:u32 = 0x0400;
}

pub mod state {
    pub const VALID_FS: u16 = 0x0001;
    pub const ERROR_FS: u16 = 0x0002;
}

#[derive(Debug, Clone)]
pub struct Superblock {
    pub block_size:        u32,
    pub blocks_count:      u64,
    pub inodes_count:      u32,
    pub inodes_per_group:  u32,
    pub blocks_per_group:  u32,
    pub first_data_block:  u32,
    pub uuid:              [u8; 16],
    pub volume_name:       String,
    pub desc_size:         u16,
    pub feature_incompat:  u32,
    pub feature_ro_compat: u32,
    pub inode_size:        u16,
    pub state:             u16,
}

impl Superblock {
    pub fn state_has_error(&self) -> bool { self.state & state::ERROR_FS != 0 }
    pub fn was_cleanly_unmounted(&self) -> bool { self.state & state::VALID_FS != 0 }
}

#[derive(Debug, thiserror::Error)]
pub enum SuperblockError {
    #[error("not an ext2/3/4 filesystem (magic = {0:#x})")]
    BadMagic(u16),

    #[error("unsupported incompatible features: {0:#010x}")]
    UnsupportedFeatures(u32),

    #[error("block device error: {0}")]
    BlockDevice(#[from] BlockDeviceError),

    #[error("superblock buffer has wrong size")]
    InvalidBuffer,

    #[error("s_log_block_size {0} is out of range (max 6, i.e. 64 KiB blocks)")]
    InvalidBlockSize(u32),

    #[error("inode size {0} is invalid (must be 128–1024 and a power of two)")]
    InvalidInodeSize(u16),

    #[error("filesystem geometry is invalid (inodes_per_group or blocks_per_group is zero)")]
    InvalidGeometry,
}

/// Read and validate the ext4 superblock from `dev`.
///
/// The superblock is always located at byte offset 1024 (sectors 2–3).
pub fn parse(dev: &dyn BlockDevice) -> Result<Superblock, SuperblockError> {
    // Superblock lives at byte 1024 = sectors 2 and 3 (two 512-byte sectors).
    let raw_bytes = read_sectors(dev, 2, 2)?;
    let raw = RawSuperblock::read_from(raw_bytes.as_slice())
        .ok_or(SuperblockError::InvalidBuffer)?;

    let magic = raw.s_magic.get();
    if magic != 0xEF53 {
        return Err(SuperblockError::BadMagic(magic));
    }

    let feature_incompat = raw.s_feature_incompat.get();
    let unsupported = feature_incompat & incompat::UNSUPPORTED_MASK;
    if unsupported != 0 {
        return Err(SuperblockError::UnsupportedFeatures(unsupported));
    }

    let log_block_size = raw.s_log_block_size.get();
    if log_block_size > 6 {
        return Err(SuperblockError::InvalidBlockSize(log_block_size));
    }
    let block_size = 1024u32 << log_block_size;
    let blocks_lo = raw.s_blocks_count_lo.get() as u64;
    let blocks_hi = raw.s_blocks_count_hi.get() as u64;
    let blocks_count = (blocks_hi << 32) | blocks_lo;

    let name_bytes = &raw.s_volume_name;
    let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
    let volume_name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();

    let mut desc_size = raw.s_desc_size.get();
    if desc_size < 32 {
        // ext2/3 or old ext4 without 64-bit feature: descriptors are 32 bytes
        desc_size = 32;
    }

    let inode_size = match raw.s_rev_level.get() {
        0 => 128,
        _ => {
            let s = raw.s_inode_size.get();
            if s < 128 {
                128
            } else {
                if s > 1024 || !s.is_power_of_two() {
                    return Err(SuperblockError::InvalidInodeSize(s));
                }
                s
            }
        }
    };

    let inodes_per_group = raw.s_inodes_per_group.get();
    let blocks_per_group = raw.s_blocks_per_group.get();
    if inodes_per_group == 0 || blocks_per_group == 0 {
        return Err(SuperblockError::InvalidGeometry);
    }

    Ok(Superblock {
        block_size,
        blocks_count,
        inodes_count:     raw.s_inodes_count.get(),
        inodes_per_group,
        blocks_per_group,
        first_data_block: raw.s_first_data_block.get(),
        uuid:             raw.s_uuid,
        volume_name,
        desc_size,
        feature_incompat,
        feature_ro_compat: raw.s_feature_ro_compat.get(),
        inode_size,
        state:            raw.s_state.get(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::AsBytes;

    /// Minimal in-memory block device.
    struct MemDevice(Vec<u8>);

    impl BlockDevice for MemDevice {
        fn read_sector(&self, idx: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
            let total = self.0.len() as u64 / 512;
            if idx >= total {
                return Err(BlockDeviceError::OutOfRange(idx, total));
            }
            let off = idx as usize * 512;
            buf.copy_from_slice(&self.0[off..off + 512]);
            Ok(())
        }
        fn sector_count(&self) -> u64 { self.0.len() as u64 / 512 }
        fn write_sector(&self, _: u64, _: &[u8; 512]) -> Result<(), BlockDeviceError> {
            Err(BlockDeviceError::NotSupported("read-only test device"))
        }
        fn flush(&self) -> Result<(), BlockDeviceError> { Ok(()) }
    }

    /// Build a minimal 2048-byte device with `sb` installed at byte 1024.
    fn make_device(sb: &RawSuperblock) -> MemDevice {
        let mut data = vec![0u8; 2048];
        data[1024..2048].copy_from_slice(sb.as_bytes());
        MemDevice(data)
    }

    fn valid_sb() -> RawSuperblock {
        let mut sb = RawSuperblock::new_zeroed();
        sb.s_magic           = U16::new(0xEF53);
        sb.s_log_block_size  = U32::new(2);         // block_size = 1024 << 2 = 4096
        sb.s_inodes_count    = U32::new(1024);
        sb.s_inodes_per_group = U32::new(256);
        sb.s_blocks_per_group = U32::new(32768);
        sb.s_blocks_count_lo = U32::new(8192);
        sb.s_first_data_block = U32::new(0);
        sb.s_desc_size        = U16::new(64);
        sb.s_feature_incompat = U32::new(incompat::EXTENTS | incompat::FILETYPE);
        let name = b"testvol\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        sb.s_volume_name.copy_from_slice(&name[..16]);
        sb
    }

    #[test]
    fn parse_valid_ext4() {
        let sb = valid_sb();
        let dev = make_device(&sb);
        let parsed = parse(&dev).unwrap();
        assert_eq!(parsed.block_size, 4096);
        assert_eq!(parsed.inodes_count, 1024);
        assert_eq!(parsed.volume_name, "testvol");
    }

    #[test]
    fn bad_magic() {
        let mut sb = valid_sb();
        sb.s_magic = U16::new(0x1234);
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::BadMagic(0x1234)));
    }

    #[test]
    fn unsupported_features() {
        let mut sb = valid_sb();
        sb.s_feature_incompat = U32::new(
            incompat::EXTENTS | incompat::FILETYPE | incompat::ENCRYPT,
        );
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::UnsupportedFeatures(f) if f & incompat::ENCRYPT != 0));
    }

    #[test]
    fn volume_name_trimmed() {
        let mut sb = valid_sb();
        let padded = b"myfs\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        sb.s_volume_name.copy_from_slice(padded);
        let dev = make_device(&sb);
        let parsed = parse(&dev).unwrap();
        assert_eq!(parsed.volume_name, "myfs");
    }

    #[test]
    fn invalid_log_block_size() {
        let mut sb = valid_sb();
        sb.s_log_block_size = U32::new(7); // > 6 → invalid
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::InvalidBlockSize(7)));
    }

    #[test]
    fn invalid_inode_size_too_large() {
        let mut sb = valid_sb();
        sb.s_rev_level = U32::new(1);
        sb.s_inode_size = U16::new(2048); // > 1024 → invalid
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::InvalidInodeSize(2048)));
    }

    #[test]
    fn invalid_inode_size_not_power_of_two() {
        let mut sb = valid_sb();
        sb.s_rev_level = U32::new(1);
        sb.s_inode_size = U16::new(300); // not a power of two → invalid
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::InvalidInodeSize(300)));
    }

    #[test]
    fn invalid_geometry_zero_inodes_per_group() {
        let mut sb = valid_sb();
        sb.s_inodes_per_group = U32::new(0);
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::InvalidGeometry));
    }

    #[test]
    fn invalid_geometry_zero_blocks_per_group() {
        let mut sb = valid_sb();
        sb.s_blocks_per_group = U32::new(0);
        let dev = make_device(&sb);
        let err = parse(&dev).unwrap_err();
        assert!(matches!(err, SuperblockError::InvalidGeometry));
    }

    #[test]
    fn blocks_count_hi_combined() {
        let mut sb = valid_sb();
        sb.s_blocks_count_lo = U32::new(0x0000_0001);
        sb.s_blocks_count_hi = U32::new(0x0000_0001);
        let dev = make_device(&sb);
        let parsed = parse(&dev).unwrap();
        assert_eq!(parsed.blocks_count, 0x0000_0001_0000_0001u64);
    }
}
