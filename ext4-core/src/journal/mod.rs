pub mod superblock;
pub mod replay;
pub mod writer;
pub mod mount;

use crate::block_device::{BlockDevice, BlockDeviceError, read_sectors};
use crate::superblock::Superblock;
use crate::group_desc::GroupDescTable;
use crate::inode;
use crate::extent::{ExtentLeaf, collect_leaves_pub};
pub use superblock::{JournalSuperblock, RawJournalSuperblock, JOURNAL_MAGIC};
use zerocopy::FromBytes;

pub const EXT4_JOURNAL_INO: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("invalid journal magic: {0:#010x}")]
    BadMagic(u32),

    #[error("journal block {0} is out of range")]
    BlockOutOfRange(u32),

    #[error("journal reports errno {0} — filesystem error, run e2fsck")]
    JournalErrno(i32),

    #[error("unsupported journal feature flags: {0:#010x}")]
    UnsupportedFeatures(u32),

    #[error("block device error: {0}")]
    BlockDevice(#[from] BlockDeviceError),

    #[error("inode error: {0}")]
    Inode(#[from] inode::InodeError),

    #[error("extent error: {0}")]
    Extent(#[from] crate::extent::ExtentError),

    #[error("journal superblock buffer is too small")]
    InvalidBuffer,

    #[error("journal geometry is invalid (total_blocks={0}, first_usable={1})")]
    InvalidGeometry(u32, u32),
}

/// Loaded journal — block map and parsed superblock.
pub struct Journal {
    /// Physical block addresses of all journal blocks (index = journal-relative block number).
    pub blocks: Vec<u64>,
    pub sb: JournalSuperblock,
}

impl Journal {
    /// Load journal from inode 8.
    pub fn load(
        dev: &dyn BlockDevice,
        fs_sb: &Superblock,
        gdt: &GroupDescTable,
    ) -> Result<Self, JournalError> {
        let inode = inode::read_inode(dev, fs_sb, gdt, EXT4_JOURNAL_INO)?;

        let mut leaves: Vec<ExtentLeaf> = Vec::new();
        collect_leaves_pub(dev, fs_sb, &inode, &mut leaves)?;
        leaves.sort_by_key(|l| l.ee_block.get());

        let total_lblocks = inode.size.div_ceil(fs_sb.block_size as u64);
        let mut blocks: Vec<u64> = Vec::with_capacity(total_lblocks as usize);

        for lblock in 0..total_lblocks {
            let phys = find_phys(&leaves, lblock);
            blocks.push(phys.unwrap_or(0));
        }

        let sb = read_journal_superblock(dev, fs_sb, &blocks)?;

        Ok(Journal { blocks, sb })
    }

    /// Read journal block by journal-relative index into `buf` (resized to fs block size).
    pub fn read_block(
        &self,
        dev: &dyn BlockDevice,
        fs_sb: &Superblock,
        jblock: u32,
        buf: &mut Vec<u8>,
    ) -> Result<(), JournalError> {
        let phys = self.blocks.get(jblock as usize)
            .copied()
            .ok_or(JournalError::BlockOutOfRange(jblock))?;
        read_phys_block(dev, fs_sb, phys, buf)?;
        Ok(())
    }
}

fn find_phys(leaves: &[ExtentLeaf], lblock: u64) -> Option<u64> {
    for leaf in leaves {
        let first = leaf.ee_block.get() as u64;
        let len = leaf.ee_len.get();
        if len > 32768 { continue; }
        let count = len as u64;
        if lblock >= first && lblock < first + count {
            let phys_start = (leaf.ee_start_hi.get() as u64) << 32
                | leaf.ee_start_lo.get() as u64;
            return phys_start.checked_add(lblock - first);
        }
    }
    None
}

fn read_phys_block(
    dev: &dyn BlockDevice,
    fs_sb: &Superblock,
    phys: u64,
    buf: &mut Vec<u8>,
) -> Result<(), JournalError> {
    let sectors_per_block = fs_sb.block_size as u64 / 512;
    let start_sector = phys * sectors_per_block;
    let data = read_sectors(dev, start_sector, sectors_per_block)?;
    buf.clear();
    buf.extend_from_slice(&data);
    Ok(())
}

fn read_journal_superblock(
    dev: &dyn BlockDevice,
    fs_sb: &Superblock,
    blocks: &[u64],
) -> Result<JournalSuperblock, JournalError> {
    let phys = blocks.first().copied().ok_or(JournalError::BlockOutOfRange(0))?;
    let sectors_per_block = fs_sb.block_size as u64 / 512;
    let start_sector = phys * sectors_per_block;
    let data = read_sectors(dev, start_sector, sectors_per_block)?;
    if data.len() < core::mem::size_of::<RawJournalSuperblock>() {
        return Err(JournalError::InvalidBuffer);
    }
    let raw = RawJournalSuperblock::read_from(&data[..core::mem::size_of::<RawJournalSuperblock>()])
        .ok_or(JournalError::InvalidBuffer)?;
    superblock::parse(&raw)
}

/// Mount-time helper: load journal and replay if dirty.
pub fn check_and_recover(
    dev: &dyn BlockDevice,
    fs_sb: &Superblock,
    gdt: &GroupDescTable,
) -> Result<Journal, JournalError> {
    let journal = Journal::load(dev, fs_sb, gdt)?;
    if journal.sb.errno != 0 {
        return Err(JournalError::JournalErrno(journal.sb.errno));
    }
    replay::replay(dev, fs_sb, &journal)?;
    Ok(journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDeviceError;

    #[allow(dead_code)]
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

    fn make_raw_jsb(magic: u32, start: u32) -> RawJournalSuperblock {
        use zerocopy::big_endian::U32;
        use zerocopy::FromZeroes;
        let mut jsb = RawJournalSuperblock::new_zeroed();
        jsb.h_magic = U32::new(magic);
        jsb.h_blocktype = U32::new(superblock::SUPERBLOCK_V2);
        jsb.s_blocksize = U32::new(4096);
        jsb.s_maxlen = U32::new(1024);
        jsb.s_first = U32::new(1);
        jsb.s_sequence = U32::new(1);
        jsb.s_start = U32::new(start);
        jsb
    }

    #[test]
    fn bad_magic() {
        let jsb = make_raw_jsb(0xDEADBEEF, 0);
        let err = superblock::parse(&jsb).unwrap_err();
        assert!(matches!(err, JournalError::BadMagic(0xDEADBEEF)));
    }

    #[test]
    fn clean_journal_superblock() {
        let jsb = make_raw_jsb(JOURNAL_MAGIC, 0);
        let parsed = superblock::parse(&jsb).unwrap();
        assert!(parsed.is_clean());
        assert!(!parsed.needs_recovery());
    }

    #[test]
    fn dirty_journal_superblock() {
        let jsb = make_raw_jsb(JOURNAL_MAGIC, 5);
        let parsed = superblock::parse(&jsb).unwrap();
        assert!(parsed.needs_recovery());
        assert!(!parsed.is_clean());
    }
}
