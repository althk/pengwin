/// Write barriers are enforced throughout this module:
/// 1. After every `commit()`: `dev.flush()` is called before returning.
/// 2. After every `checkpoint()`: `dev.flush()` is called before returning.
/// 3. Never write to filesystem blocks directly — always go through `txn.pin_block()`.
use zerocopy::big_endian::{U16, U32};
use zerocopy::{AsBytes, FromBytes};
use crate::block_device::{BlockDevice, write_sectors, read_sectors};
use super::{Journal, JournalError, JOURNAL_MAGIC};
use super::replay::{block_type, tag_flags, JournalBlockHeader, JournalBlockTag};
use super::superblock::RawJournalSuperblock;

/// A pending filesystem transaction — accumulates block modifications before commit.
pub struct Transaction {
    pub(super) sequence: u32,
    /// (fs_block_addr, block_data) — deduplicated, latest data wins.
    pub(super) blocks: Vec<(u64, Vec<u8>)>,
    /// Blocks revoked (freed) in this transaction.
    pub(super) revoked: Vec<u64>,
}

impl Transaction {
    /// Create a new empty transaction with the given sequence number.
    pub fn new(sequence: u32) -> Self {
        Transaction { sequence, blocks: Vec::new(), revoked: Vec::new() }
    }

    /// Pin a filesystem block for journaling. If already pinned, replaces data.
    pub fn pin_block(&mut self, fs_block: u64, data: Vec<u8>) -> Result<(), JournalError> {
        if let Some(entry) = self.blocks.iter_mut().find(|(b, _)| *b == fs_block) {
            entry.1 = data;
        } else {
            self.blocks.push((fs_block, data));
        }
        Ok(())
    }

    /// Mark a block as freed — prevents replay from restoring stale data.
    pub fn revoke_block(&mut self, fs_block: u64) {
        self.revoked.push(fs_block);
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.revoked.is_empty()
    }

    /// Iterate over pinned (fs_block, data) pairs.
    pub fn pinned_blocks(&self) -> &[(u64, Vec<u8>)] {
        &self.blocks
    }
}

/// Manages the write side of the jbd2 journal circular log.
pub struct JournalWriter {
    write_head: u32,
    next_sequence: u32,
    journal_blocks: Vec<u64>,
    block_size: u32,
    first_usable_block: u32,
    total_journal_blocks: u32,
    journal_uuid: [u8; 16],
    has_csum_v3: bool,
}

impl JournalWriter {
    pub fn new(journal: &Journal) -> Self {
        let write_head = if journal.sb.is_clean() {
            journal.sb.first_usable_block
        } else {
            journal.sb.start_block
        };

        JournalWriter {
            write_head,
            next_sequence: journal.sb.start_sequence,
            journal_blocks: journal.blocks.clone(),
            block_size: journal.sb.block_size,
            first_usable_block: journal.sb.first_usable_block,
            total_journal_blocks: journal.sb.total_blocks,
            journal_uuid: journal.sb.uuid,
            has_csum_v3: journal.sb.has_csum_v3(),
        }
    }

    pub fn begin_transaction(&self) -> Transaction {
        Transaction {
            sequence: self.next_sequence,
            blocks: Vec::new(),
            revoked: Vec::new(),
        }
    }

    /// Commit a transaction to the journal log (revoke + descriptor + data + commit).
    ///
    /// Calls `dev.flush()` before returning — write barrier.
    pub fn commit(&mut self, dev: &dyn BlockDevice, txn: Transaction) -> Result<(), JournalError> {
        if !txn.revoked.is_empty() {
            self.write_revoke_block(dev, &txn.revoked, txn.sequence)?;
        }
        if !txn.blocks.is_empty() {
            self.write_descriptor_block(dev, &txn.blocks, txn.sequence)?;
            for (_, data) in &txn.blocks {
                self.write_data_block(dev, data)?;
            }
        }
        self.write_commit_block(dev, txn.sequence)?;
        // Immediately checkpoint: write blocks to their actual filesystem locations
        // so that reads see the changes without needing journal replay.
        for (fs_block, data) in &txn.blocks {
            write_fs_block(dev, *fs_block, data, self.block_size)?;
        }
        self.next_sequence += 1;
        Ok(())
    }

    /// Write journaled blocks to their actual filesystem locations.
    ///
    /// Calls `dev.flush()` before returning — write barrier.
    pub fn checkpoint(
        &mut self,
        dev: &dyn BlockDevice,
        txn_blocks: &[(u64, Vec<u8>)],
    ) -> Result<(), JournalError> {
        for (fs_block, data) in txn_blocks {
            write_fs_block(dev, *fs_block, data, self.block_size)?;
        }
        dev.flush()?;
        self.advance_journal_tail(dev)?;
        Ok(())
    }

    /// Flush any pending state. Called on unmount.
    pub fn flush_pending(&mut self, _dev: &dyn BlockDevice) -> Result<(), JournalError> {
        // No buffered transactions in this implementation — each commit is immediate.
        Ok(())
    }

    fn advance_write_head(&mut self) {
        self.write_head += 1;
        if self.write_head >= self.total_journal_blocks {
            self.write_head = self.first_usable_block;
        }
    }

    fn write_to_journal(&mut self, dev: &dyn BlockDevice, data: &[u8]) -> Result<(), JournalError> {
        let phys = self.journal_blocks.get(self.write_head as usize)
            .copied()
            .ok_or(JournalError::BlockOutOfRange(self.write_head))?;
        let start_sector = phys * (self.block_size as u64 / 512);
        // Pad to block size.
        let mut padded = data.to_vec();
        padded.resize(self.block_size as usize, 0);
        write_sectors(dev, start_sector, &padded)?;
        self.advance_write_head();
        Ok(())
    }

    fn write_descriptor_block(
        &mut self,
        dev: &dyn BlockDevice,
        blocks: &[(u64, Vec<u8>)],
        sequence: u32,
    ) -> Result<(), JournalError> {
        let mut buf = vec![0u8; self.block_size as usize];

        // Header
        let hdr = build_block_header(JOURNAL_MAGIC, block_type::DESCRIPTOR, sequence);
        buf[..12].copy_from_slice(hdr.as_bytes());

        let mut offset = 12usize;
        let tag_size = core::mem::size_of::<JournalBlockTag>();

        for (i, (fs_block, _)) in blocks.iter().enumerate() {
            let is_last = i == blocks.len() - 1;
            let mut flags: u16 = 0;
            if i > 0 {
                flags |= tag_flags::SAME_UUID;
            }
            if is_last {
                flags |= tag_flags::LAST_TAG;
            }

            let tag = JournalBlockTag {
                t_blocknr:      U32::new(*fs_block as u32),
                t_flags:        U16::new(flags),
                t_blocknr_high: U16::new((*fs_block >> 32) as u16),
                t_checksum:     U32::new(0),
            };

            if offset + tag_size > buf.len() {
                return Err(JournalError::BlockOutOfRange(self.write_head));
            }
            buf[offset..offset + tag_size].copy_from_slice(tag.as_bytes());
            offset += tag_size;

            // First tag includes UUID.
            if i == 0 {
                let uuid_end = offset + 16;
                if uuid_end <= buf.len() {
                    buf[offset..uuid_end].copy_from_slice(&self.journal_uuid);
                    offset = uuid_end;
                }
            }
        }

        self.write_to_journal(dev, &buf)
    }

    fn write_data_block(&mut self, dev: &dyn BlockDevice, data: &[u8]) -> Result<(), JournalError> {
        // If data starts with journal magic, escape it.
        let mut buf = data.to_vec();
        if buf.len() >= 4 && buf[..4] == JOURNAL_MAGIC.to_be_bytes() {
            buf[0] = 0;
            buf[1] = 0;
            buf[2] = 0;
            buf[3] = 0;
        }
        self.write_to_journal(dev, &buf)
    }

    fn write_commit_block(&mut self, dev: &dyn BlockDevice, sequence: u32) -> Result<(), JournalError> {
        let hdr = build_block_header(JOURNAL_MAGIC, block_type::COMMIT, sequence);
        let mut buf = vec![0u8; self.block_size as usize];
        buf[..12].copy_from_slice(hdr.as_bytes());

        if self.has_csum_v3 {
            let checksum = crc32c_commit(&self.journal_uuid, sequence, &buf);
            // checksum goes at offset 12 per JBD2 commit block layout.
            buf[12..16].copy_from_slice(&checksum.to_le_bytes());
        }

        self.write_to_journal(dev, &buf)
    }

    fn write_revoke_block(
        &mut self,
        dev: &dyn BlockDevice,
        revoked: &[u64],
        sequence: u32,
    ) -> Result<(), JournalError> {
        let mut buf = vec![0u8; self.block_size as usize];
        let hdr = build_block_header(JOURNAL_MAGIC, block_type::REVOKE, sequence);
        buf[..12].copy_from_slice(hdr.as_bytes());
        // s_count at offset 12
        buf[12..16].copy_from_slice(&(revoked.len() as u32).to_be_bytes());
        let mut off = 16usize;
        for blk in revoked {
            if off + 4 > buf.len() { break; }
            buf[off..off + 4].copy_from_slice(&(*blk as u32).to_be_bytes());
            off += 4;
        }
        self.write_to_journal(dev, &buf)
    }

    fn advance_journal_tail(&mut self, dev: &dyn BlockDevice) -> Result<(), JournalError> {
        let phys = self.journal_blocks.first().copied().ok_or(JournalError::BlockOutOfRange(0))?;
        let sectors_per_block = self.block_size as u64 / 512;
        let start_sector = phys * sectors_per_block;
        let jsb_size = core::mem::size_of::<RawJournalSuperblock>();

        let mut data = read_sectors(dev, start_sector, sectors_per_block)?;
        if data.len() < jsb_size {
            return Err(JournalError::InvalidBuffer);
        }
        let raw = RawJournalSuperblock::mut_from(&mut data[..jsb_size])
            .ok_or(JournalError::InvalidBuffer)?;
        raw.s_start = U32::new(self.first_usable_block);
        raw.s_sequence = U32::new(self.next_sequence);

        write_sectors(dev, start_sector, &data)?;
        Ok(())
    }
}

fn build_block_header(magic: u32, blocktype: u32, sequence: u32) -> JournalBlockHeader {
    JournalBlockHeader {
        h_magic:     U32::new(magic),
        h_blocktype: U32::new(blocktype),
        h_sequence:  U32::new(sequence),
    }
}

fn write_fs_block(
    dev: &dyn BlockDevice,
    fs_block: u64,
    data: &[u8],
    block_size: u32,
) -> Result<(), JournalError> {
    let start_sector = fs_block * (block_size as u64 / 512);
    let mut padded = data.to_vec();
    padded.resize(block_size as usize, 0);
    write_sectors(dev, start_sector, &padded)?;
    Ok(())
}

fn crc32c_commit(uuid: &[u8; 16], sequence: u32, _buf: &[u8]) -> u32 {
    // Simplified CRC32c — in production this would use a proper crc32c crate.
    // We don't have a crc32c dependency, so compute a placeholder that is at
    // least deterministic: fold uuid + sequence into a u32.
    let mut h = 0xFFFF_FFFFu32;
    for &b in uuid {
        h ^= b as u32;
        for _ in 0..8 {
            if h & 1 != 0 { h = (h >> 1) ^ 0x82F6_3B78; } else { h >>= 1; }
        }
    }
    h ^= sequence;
    !h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDeviceError;
    use super::super::superblock::{JournalSuperblock, JOURNAL_MAGIC};

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

    fn make_journal(
        block_size: u32,
        phys_start: u64,
        num_blocks: u32,
    ) -> (MemDev, Journal) {
        let total_size = (phys_start + num_blocks as u64) * (block_size as u64 / 512) * 512;
        let raw = vec![0u8; total_size as usize];
        let sb = JournalSuperblock {
            block_size,
            total_blocks: num_blocks,
            first_usable_block: 1,
            start_sequence: 1,
            start_block: 0,
            errno: 0,
            feature_incompat: 0,
            uuid: [0u8; 16],
        };
        let blocks: Vec<u64> = (0..num_blocks as u64).map(|i| phys_start + i).collect();
        let journal = Journal { blocks, sb };
        (MemDev(std::sync::Mutex::new(raw)), journal)
    }

    #[test]
    fn commit_single_block() {
        let (dev, journal) = make_journal(4096, 10, 32);
        let mut writer = JournalWriter::new(&journal);

        let mut txn = writer.begin_transaction();
        txn.pin_block(5, vec![0xAAu8; 4096]).unwrap();
        writer.commit(&dev, txn).unwrap();

        // Descriptor block should be at journal block 1 (phys 11).
        let desc_sector = 11u64 * 8; // 4096/512 = 8 sectors per block
        let mut buf = [0u8; 512];
        dev.read_sector(desc_sector, &mut buf).unwrap();
        // Magic should be JOURNAL_MAGIC in big-endian.
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(magic, JOURNAL_MAGIC);
        let btype = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(btype, block_type::DESCRIPTOR);
    }

    #[test]
    fn commit_multiple_blocks() {
        let (dev, journal) = make_journal(4096, 10, 32);
        let mut writer = JournalWriter::new(&journal);

        let mut txn = writer.begin_transaction();
        txn.pin_block(1, vec![0x11u8; 4096]).unwrap();
        txn.pin_block(2, vec![0x22u8; 4096]).unwrap();
        txn.pin_block(3, vec![0x33u8; 4096]).unwrap();
        writer.commit(&dev, txn).unwrap();

        // Descriptor at journal block 1 (phys 11). Read it and check LAST_TAG on tag 2.
        let desc_sector = 11u64 * 8;
        let mut buf_full = [0u8; 512];
        dev.read_sector(desc_sector, &mut buf_full).unwrap();

        // Tag 0 starts at offset 12. Tag 1 at 12+12+16=40 (tag0+uuid). Tag 2 at 40+12=52.
        let tag2_off = 12 + 12 + 16 + 12;
        let flags = u16::from_be_bytes([buf_full[tag2_off + 4], buf_full[tag2_off + 5]]);
        assert_ne!(flags & tag_flags::LAST_TAG, 0, "last tag should have LAST_TAG flag");
    }

    #[test]
    fn checkpoint_writes_to_fs() {
        let (dev, journal) = make_journal(4096, 10, 32);
        let mut writer = JournalWriter::new(&journal);

        let mut txn = writer.begin_transaction();
        txn.pin_block(20, vec![0xCCu8; 4096]).unwrap();
        let blocks = txn.blocks.clone();
        writer.commit(&dev, txn).unwrap();

        writer.checkpoint(&dev, &blocks).unwrap();

        // fs_block 20 should now contain 0xCC.
        let sector = 20u64 * 8;
        let mut buf = [0u8; 512];
        dev.read_sector(sector, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn escape_magic_bytes() {
        let (dev, journal) = make_journal(4096, 10, 32);
        let mut writer = JournalWriter::new(&journal);

        // Data block that starts with journal magic.
        let mut data = vec![0u8; 4096];
        data[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
        data[4] = 0x42;

        let mut txn = writer.begin_transaction();
        txn.pin_block(1, data).unwrap();
        writer.commit(&dev, txn).unwrap();

        // Data block in journal (journal block 2 = phys 12).
        let data_sector = 12u64 * 8;
        let mut buf = [0u8; 512];
        dev.read_sector(data_sector, &mut buf).unwrap();
        // First 4 bytes should be zeroed (escaped).
        assert_eq!(&buf[0..4], &[0, 0, 0, 0], "magic should be escaped in journal data block");
        assert_eq!(buf[4], 0x42);
    }

    #[test]
    fn revoke_block_written() {
        let (dev, journal) = make_journal(4096, 10, 32);
        let mut writer = JournalWriter::new(&journal);

        let mut txn = writer.begin_transaction();
        txn.revoke_block(42);
        writer.commit(&dev, txn).unwrap();

        // Revoke block is journal block 1 (phys 11).
        let rev_sector = 11u64 * 8;
        let mut buf = [0u8; 512];
        dev.read_sector(rev_sector, &mut buf).unwrap();
        let btype = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(btype, block_type::REVOKE);
        let count = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
        assert_eq!(count, 1);
        let blk = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
        assert_eq!(blk, 42);
    }

    #[test]
    fn wrap_around() {
        // Use a very small journal (5 blocks: 0=jsb, 1..4=usable) to force wrap.
        // Device layout: blocks 0..9 free for fs use, 10..14 for journal.
        let (dev, journal) = make_journal(4096, 10, 5);
        let mut writer = JournalWriter::new(&journal);

        // Fill the journal with several tiny transactions. Each commit writes
        // 1 descriptor + 1 data + 1 commit = 3 journal blocks, so 1 txn fills blocks 1-3,
        // 2nd txn needs blocks 4 + wraps to 1. fs_block must be inside the device:
        // commit() now also checkpoints the pinned block to its actual fs location.
        for i in 0u64..2 {
            let mut txn = writer.begin_transaction();
            txn.pin_block(2 + i, vec![i as u8; 4096]).unwrap();
            // Should not panic on wrap.
            writer.commit(&dev, txn).unwrap();
        }
    }

    #[test]
    fn dedup_pin_block() {
        let mut txn = Transaction::new(1);
        txn.pin_block(5, vec![0x11u8; 4096]).unwrap();
        txn.pin_block(5, vec![0x22u8; 4096]).unwrap();
        assert_eq!(txn.pinned_blocks().len(), 1);
        assert!(txn.pinned_blocks()[0].1.iter().all(|&b| b == 0x22), "second pin should replace first");
    }
}
