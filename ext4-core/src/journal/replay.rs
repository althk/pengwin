use std::collections::HashSet;
use zerocopy::{FromBytes, FromZeroes, AsBytes};
use zerocopy::big_endian::{U16, U32};
use crate::block_device::{BlockDevice, write_sectors};
use crate::superblock::Superblock;
use super::{Journal, JournalError, JOURNAL_MAGIC};
use super::superblock::RawJournalSuperblock;

pub mod block_type {
    pub const DESCRIPTOR:    u32 = 1;
    pub const COMMIT:        u32 = 2;
    pub const SUPERBLOCK_V1: u32 = 3;
    pub const SUPERBLOCK_V2: u32 = 4;
    pub const REVOKE:        u32 = 5;
}

pub mod tag_flags {
    pub const SAME_UUID: u16 = 0x1;
    pub const DELETED:   u16 = 0x2;
    pub const ESCAPE:    u16 = 0x4;
    pub const LAST_TAG:  u16 = 0x8;
}

#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct JournalBlockHeader {
    pub h_magic:     U32,
    pub h_blocktype: U32,
    pub h_sequence:  U32,
}

/// Descriptor block tag (16 bytes: 12-byte base + 4-byte checksum).
/// When the 64BIT feature is active, t_blocknr_high holds the high 32 bits.
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct JournalBlockTag {
    pub t_blocknr:      U32,
    pub t_flags:        U16,
    pub t_blocknr_high: U16,
    pub t_checksum:     U32,
}

const _: () = assert!(core::mem::size_of::<JournalBlockHeader>() == 12);
const _: () = assert!(core::mem::size_of::<JournalBlockTag>() == 12);

/// Replay uncommitted journal transactions onto the filesystem.
///
/// Must be called before any write if `journal.sb.needs_recovery()`.
pub fn replay(
    dev: &dyn BlockDevice,
    fs_sb: &Superblock,
    journal: &Journal,
) -> Result<(), JournalError> {
    if journal.sb.is_clean() {
        tracing::debug!("journal is clean, no replay needed");
        return Ok(());
    }

    tracing::info!(
        start_seq = journal.sb.start_sequence,
        start_block = journal.sb.start_block,
        "replaying journal"
    );

    let mut sequence = journal.sb.start_sequence;
    let mut jblock = journal.sb.start_block;
    let mut revoke_set: HashSet<u64> = HashSet::new();

    loop {
        let mut buf = Vec::new();
        journal.read_block(dev, fs_sb, jblock, &mut buf)?;

        if buf.len() < 12 {
            break;
        }
        let header = match JournalBlockHeader::read_from(&buf[..12]) {
            Some(h) => h,
            None => break,
        };

        if header.h_magic.get() != JOURNAL_MAGIC {
            break;
        }
        if header.h_sequence.get() != sequence {
            break;
        }

        match header.h_blocktype.get() {
            block_type::DESCRIPTOR => {
                // Parse the descriptor and collect pending writes, but only apply
                // them if a commit block follows (partial transactions must not be applied).
                let (next_jblock, pending) = parse_descriptor_block(
                    dev, fs_sb, journal, jblock, &revoke_set,
                )?;
                jblock = next_jblock;

                // Scan ahead for commit or end-of-log.
                let mut commit_buf = Vec::new();
                journal.read_block(dev, fs_sb, jblock, &mut commit_buf)?;
                let is_commit = commit_buf.len() >= 12
                    && JournalBlockHeader::read_from(&commit_buf[..12])
                        .map(|h| {
                            h.h_magic.get() == JOURNAL_MAGIC
                                && h.h_sequence.get() == sequence
                                && h.h_blocktype.get() == block_type::COMMIT
                        })
                        .unwrap_or(false);

                if !is_commit {
                    // Partial transaction — stop, do not apply.
                    break;
                }

                // Commit confirmed — apply the writes.
                for (fs_block, data) in pending {
                    let start_sector = fs_block * (fs_sb.block_size as u64 / 512);
                    write_sectors(dev, start_sector, &data)?;
                }

                tracing::debug!(sequence, "committed transaction replayed");
                sequence += 1;
                jblock = advance_jblock(jblock, journal);
            }
            block_type::COMMIT => {
                // Empty transaction (no descriptor) — still counts as committed.
                tracing::debug!(sequence, "empty committed transaction replayed");
                sequence += 1;
                jblock = advance_jblock(jblock, journal);
            }
            block_type::REVOKE => {
                collect_revoked_blocks(fs_sb, &buf, &mut revoke_set);
                jblock = advance_jblock(jblock, journal);
            }
            _ => break,
        }
    }

    mark_journal_clean(dev, fs_sb, journal, sequence)?;
    tracing::info!("journal replay complete");
    Ok(())
}

fn advance_jblock(jblock: u32, journal: &Journal) -> u32 {
    let next = jblock + 1;
    if next >= journal.sb.total_blocks {
        journal.sb.first_usable_block
    } else {
        next
    }
}

/// Parse a descriptor block and collect the pending (fs_block, data) pairs.
/// Does NOT write to the filesystem — only writes on COMMIT confirmation.
fn parse_descriptor_block(
    dev: &dyn BlockDevice,
    fs_sb: &Superblock,
    journal: &Journal,
    desc_jblock: u32,
    revoke_set: &HashSet<u64>,
) -> Result<(u32, Vec<(u64, Vec<u8>)>), JournalError> {
    let mut buf = Vec::new();
    journal.read_block(dev, fs_sb, desc_jblock, &mut buf)?;

    let block_size = fs_sb.block_size as usize;
    let tag_size = core::mem::size_of::<JournalBlockTag>();
    let uuid_size: usize = 16;

    let mut offset = 12usize; // skip block header
    let mut jblock = advance_jblock(desc_jblock, journal);
    let mut pending: Vec<(u64, Vec<u8>)> = Vec::new();

    loop {
        if offset + tag_size > buf.len() {
            break;
        }
        let tag = match JournalBlockTag::read_from(&buf[offset..offset + tag_size]) {
            Some(t) => t,
            None => break,
        };

        let flags = tag.t_flags.get();
        let fs_block_lo = tag.t_blocknr.get() as u64;
        let fs_block_hi = tag.t_blocknr_high.get() as u64;
        let fs_block = if journal.sb.has_64bit() {
            (fs_block_hi << 32) | fs_block_lo
        } else {
            fs_block_lo
        };

        offset += tag_size;
        if flags & tag_flags::SAME_UUID == 0 {
            offset += uuid_size;
        }

        if !revoke_set.contains(&fs_block) && flags & tag_flags::DELETED == 0 {
            let mut data_buf = Vec::new();
            journal.read_block(dev, fs_sb, jblock, &mut data_buf)?;

            if flags & tag_flags::ESCAPE != 0 {
                if data_buf.len() >= 4 {
                    data_buf[0] = 0xC0;
                    data_buf[1] = 0x3B;
                    data_buf[2] = 0x39;
                    data_buf[3] = 0x98;
                }
            }

            data_buf.resize(block_size, 0);
            pending.push((fs_block, data_buf));
        }

        jblock = advance_jblock(jblock, journal);

        if flags & tag_flags::LAST_TAG != 0 {
            break;
        }
    }

    Ok((jblock, pending))
}

fn collect_revoked_blocks(
    fs_sb: &Superblock,
    buf: &[u8],
    revoke_set: &mut HashSet<u64>,
) {
    // Revoke block layout: 12-byte header, then 4-byte or 8-byte block numbers
    // depending on 64BIT feature. We parse conservatively as 4-byte entries.
    let block_size = fs_sb.block_size;
    // Header is 12 bytes; after that come the revoked block numbers (4 bytes each).
    let count_field_size = 4usize;
    if buf.len() < 12 + count_field_size {
        return;
    }
    // s_count is at offset 12 (first word after the block header).
    let s_count = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;
    let entry_size = 4usize;
    let mut off = 16usize;
    for _ in 0..s_count {
        if off + entry_size > buf.len() {
            break;
        }
        let blk = u32::from_be_bytes([buf[off], buf[off+1], buf[off+2], buf[off+3]]) as u64;
        revoke_set.insert(blk);
        off += entry_size;
    }
    let _ = block_size;
}

fn mark_journal_clean(
    dev: &dyn BlockDevice,
    fs_sb: &Superblock,
    journal: &Journal,
    next_seq: u32,
) -> Result<(), JournalError> {
    let phys = journal.blocks.first().copied().ok_or(JournalError::BlockOutOfRange(0))?;
    let sectors_per_block = fs_sb.block_size as u64 / 512;
    let start_sector = phys * sectors_per_block;

    let jsb_size = core::mem::size_of::<RawJournalSuperblock>();
    let mut data = crate::block_device::read_sectors(dev, start_sector, sectors_per_block)?;

    if data.len() < jsb_size {
        return Err(JournalError::InvalidBuffer);
    }

    let raw = RawJournalSuperblock::mut_from(&mut data[..jsb_size])
        .ok_or(JournalError::InvalidBuffer)?;

    raw.s_start = U32::new(0);
    raw.s_sequence = U32::new(next_seq);

    write_sectors(dev, start_sector, &data)?;
    dev.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDeviceError;
    use crate::superblock::Superblock;
    use super::super::superblock::{JournalSuperblock, JOURNAL_MAGIC};
    use zerocopy::AsBytes;

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

    fn make_fs_sb(block_size: u32) -> Superblock {
        Superblock {
            block_size,
            blocks_count: 4096,
            inodes_count: 1024,
            inodes_per_group: 256,
            blocks_per_group: 32768,
            first_data_block: 0,
            uuid: [0u8; 16],
            volume_name: String::new(),
            desc_size: 64,
            feature_incompat: 0x40, // EXTENTS
            feature_ro_compat: 0,
            inode_size: 256,
            state: 0x0001,
        }
    }

    fn make_journal_sb(start: u32, seq: u32, total: u32, first: u32, block_size: u32) -> JournalSuperblock {
        JournalSuperblock {
            block_size,
            total_blocks: total,
            first_usable_block: first,
            start_sequence: seq,
            start_block: start,
            errno: 0,
            feature_incompat: 0,
            uuid: [0u8; 16],
        }
    }

    /// Build a device where journal blocks start at physical block `journal_start`.
    /// Journal layout: block 0 = jsb, block 1.. = content
    fn make_journal_device(
        block_size: usize,
        journal_phys_start: u64,
        journal_block_count: u32,
        patch: impl FnOnce(&mut Vec<u8>),
    ) -> (MemDev, Journal, Superblock) {
        let fs_sb = make_fs_sb(block_size as u32);
        let total_sectors = (journal_phys_start + journal_block_count as u64) * (block_size as u64 / 512);
        let mut raw = vec![0u8; total_sectors as usize * 512];

        // Write journal superblock into block 0 of the journal (physical block journal_phys_start).
        let jsb_offset = journal_phys_start as usize * block_size;
        let mut raw_jsb = RawJournalSuperblock::new_zeroed();
        raw_jsb.h_magic = U32::new(JOURNAL_MAGIC);
        raw_jsb.h_blocktype = U32::new(4);
        raw_jsb.s_blocksize = U32::new(block_size as u32);
        raw_jsb.s_maxlen = U32::new(journal_block_count);
        raw_jsb.s_first = U32::new(1);
        raw_jsb.s_sequence = U32::new(1);
        raw_jsb.s_start = U32::new(0); // clean by default
        raw[jsb_offset..jsb_offset + core::mem::size_of::<RawJournalSuperblock>()]
            .copy_from_slice(raw_jsb.as_bytes());

        patch(&mut raw);

        let jsb = make_journal_sb(0, 1, journal_block_count, 1, block_size as u32);
        let blocks: Vec<u64> = (0..journal_block_count as u64)
            .map(|i| journal_phys_start + i)
            .collect();
        let journal = Journal { blocks, sb: jsb };

        (MemDev(std::sync::Mutex::new(raw)), journal, fs_sb)
    }

    #[test]
    fn replay_noop_on_clean() {
        let (dev, journal, fs_sb) = make_journal_device(4096, 10, 16, |_| {});
        // Journal is clean — replay should do nothing.
        replay(&dev, &fs_sb, &journal).unwrap();
    }

    #[test]
    fn replay_single_transaction() {
        // Journal layout:
        //   block 0: jsb (phys 10)
        //   block 1: descriptor (phys 11) — one tag pointing to fs_block 5
        //   block 2: data block (phys 12) — 4096 bytes of 0xAA
        //   block 3: commit block (phys 13)
        let block_size = 4096usize;
        let phys_start = 10u64;

        let (dev, mut journal, fs_sb) = make_journal_device(block_size, phys_start, 32, |raw| {
            let off = |jb: u64| -> usize { (phys_start + jb) as usize * block_size };

            // Descriptor block at journal block 1
            let desc_off = off(1);
            let mut hdr = [0u8; 12];
            hdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            hdr[4..8].copy_from_slice(&1u32.to_be_bytes()); // DESCRIPTOR
            hdr[8..12].copy_from_slice(&1u32.to_be_bytes()); // sequence 1
            raw[desc_off..desc_off + 12].copy_from_slice(&hdr);

            // One tag: fs_block = 5, flags = LAST_TAG | SAME_UUID
            let tag_off = desc_off + 12;
            let mut tag = [0u8; 12];
            tag[0..4].copy_from_slice(&5u32.to_be_bytes()); // t_blocknr
            let flags: u16 = tag_flags::LAST_TAG | tag_flags::SAME_UUID;
            tag[4..6].copy_from_slice(&flags.to_be_bytes());
            raw[tag_off..tag_off + 12].copy_from_slice(&tag);

            // Data block at journal block 2
            let data_off = off(2);
            raw[data_off..data_off + block_size].fill(0xAA);

            // Commit block at journal block 3
            let commit_off = off(3);
            let mut chdr = [0u8; 12];
            chdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            chdr[4..8].copy_from_slice(&2u32.to_be_bytes()); // COMMIT
            chdr[8..12].copy_from_slice(&1u32.to_be_bytes()); // sequence 1
            raw[commit_off..commit_off + 12].copy_from_slice(&chdr);
        });

        // Mark journal dirty — start at block 1, sequence 1.
        journal.sb.start_block = 1;
        journal.sb.start_sequence = 1;

        replay(&dev, &fs_sb, &journal).unwrap();

        // fs_block 5 should now have 0xAA written.
        let sector = 5u64 * (block_size as u64 / 512);
        let mut buf = [0u8; 512];
        dev.read_sector(sector, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA), "fs block 5 should be 0xAA after replay");
    }

    #[test]
    fn replay_with_revoke() {
        let block_size = 4096usize;
        let phys_start = 10u64;

        let (dev, mut journal, fs_sb) = make_journal_device(block_size, phys_start, 32, |raw| {
            let off = |jb: u64| -> usize { (phys_start + jb) as usize * block_size };

            // Revoke block at journal block 1, revoking fs_block 7
            let rev_off = off(1);
            let mut rhdr = [0u8; 12];
            rhdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            rhdr[4..8].copy_from_slice(&5u32.to_be_bytes()); // REVOKE
            rhdr[8..12].copy_from_slice(&1u32.to_be_bytes());
            raw[rev_off..rev_off + 12].copy_from_slice(&rhdr);
            // s_count = 1 at offset 12
            raw[rev_off + 12..rev_off + 16].copy_from_slice(&1u32.to_be_bytes());
            // block 7 at offset 16
            raw[rev_off + 16..rev_off + 20].copy_from_slice(&7u32.to_be_bytes());

            // Descriptor at journal block 2
            let desc_off = off(2);
            let mut hdr = [0u8; 12];
            hdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            hdr[4..8].copy_from_slice(&1u32.to_be_bytes()); // DESCRIPTOR
            hdr[8..12].copy_from_slice(&1u32.to_be_bytes());
            raw[desc_off..desc_off + 12].copy_from_slice(&hdr);
            // Tag: fs_block 7, LAST_TAG | SAME_UUID
            let tag_off = desc_off + 12;
            let mut tag = [0u8; 12];
            tag[0..4].copy_from_slice(&7u32.to_be_bytes());
            let flags: u16 = tag_flags::LAST_TAG | tag_flags::SAME_UUID;
            tag[4..6].copy_from_slice(&flags.to_be_bytes());
            raw[tag_off..tag_off + 12].copy_from_slice(&tag);

            // Data block at journal block 3 — 0xBB (should NOT be replayed)
            let data_off = off(3);
            raw[data_off..data_off + block_size].fill(0xBB);

            // Commit at journal block 4
            let commit_off = off(4);
            let mut chdr = [0u8; 12];
            chdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            chdr[4..8].copy_from_slice(&2u32.to_be_bytes()); // COMMIT
            chdr[8..12].copy_from_slice(&1u32.to_be_bytes());
            raw[commit_off..commit_off + 12].copy_from_slice(&chdr);
        });

        journal.sb.start_block = 1;
        journal.sb.start_sequence = 1;

        replay(&dev, &fs_sb, &journal).unwrap();

        // fs_block 7 should NOT have been written (it was revoked).
        let sector = 7u64 * (block_size as u64 / 512);
        let mut buf = [0u8; 512];
        dev.read_sector(sector, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x00), "revoked block should not be replayed");
    }

    #[test]
    fn replay_partial_transaction_not_applied() {
        // Descriptor present but no commit — should not be replayed.
        let block_size = 4096usize;
        let phys_start = 10u64;

        let (dev, mut journal, fs_sb) = make_journal_device(block_size, phys_start, 32, |raw| {
            let off = |jb: u64| -> usize { (phys_start + jb) as usize * block_size };

            // Descriptor at journal block 1
            let desc_off = off(1);
            let mut hdr = [0u8; 12];
            hdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            hdr[4..8].copy_from_slice(&1u32.to_be_bytes()); // DESCRIPTOR
            hdr[8..12].copy_from_slice(&1u32.to_be_bytes());
            raw[desc_off..desc_off + 12].copy_from_slice(&hdr);
            let tag_off = desc_off + 12;
            let mut tag = [0u8; 12];
            tag[0..4].copy_from_slice(&3u32.to_be_bytes()); // fs_block 3
            let flags: u16 = tag_flags::LAST_TAG | tag_flags::SAME_UUID;
            tag[4..6].copy_from_slice(&flags.to_be_bytes());
            raw[tag_off..tag_off + 12].copy_from_slice(&tag);

            // Data block at journal block 2
            let data_off = off(2);
            raw[data_off..data_off + block_size].fill(0xFF);

            // No commit block — this is a partial/crashed transaction.
        });

        journal.sb.start_block = 1;
        journal.sb.start_sequence = 1;

        // replay should succeed (partial txn just stops at the missing commit).
        replay(&dev, &fs_sb, &journal).unwrap();

        // fs_block 3 should be zero (not replayed).
        let sector = 3u64 * (block_size as u64 / 512);
        let mut buf = [0u8; 512];
        dev.read_sector(sector, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x00), "partial txn should not be applied");
    }

    #[test]
    fn mark_clean_after_replay() {
        let block_size = 4096usize;
        let phys_start = 10u64;

        let (dev, mut journal, fs_sb) = make_journal_device(block_size, phys_start, 16, |raw| {
            let off = |jb: u64| -> usize { (phys_start + jb) as usize * block_size };

            // Single commit block at journal block 1 (empty transaction).
            let commit_off = off(1);
            let mut chdr = [0u8; 12];
            chdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            chdr[4..8].copy_from_slice(&2u32.to_be_bytes()); // COMMIT
            chdr[8..12].copy_from_slice(&1u32.to_be_bytes());
            raw[commit_off..commit_off + 12].copy_from_slice(&chdr);
        });

        journal.sb.start_block = 1;
        journal.sb.start_sequence = 1;

        replay(&dev, &fs_sb, &journal).unwrap();

        // Verify journal superblock s_start is now 0.
        let jsb_sector = phys_start * (block_size as u64 / 512);
        let jsb_size = core::mem::size_of::<RawJournalSuperblock>();
        let data = crate::block_device::read_sectors(&dev, jsb_sector, block_size as u64 / 512).unwrap();
        let raw_jsb = RawJournalSuperblock::read_from(&data[..jsb_size]).unwrap();
        assert_eq!(raw_jsb.s_start.get(), 0, "journal should be marked clean after replay");
    }
}
