use zerocopy::{FromBytes, FromZeroes, AsBytes};
use zerocopy::big_endian::{U32, I32};

pub const JOURNAL_MAGIC: u32 = 0xC03B3998;

/// JBD2 superblock block type.
pub const SUPERBLOCK_V2: u32 = 4;

#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct RawJournalSuperblock {
    pub h_magic:            U32,
    pub h_blocktype:        U32,
    pub h_sequence:         U32,

    pub s_blocksize:        U32,
    pub s_maxlen:           U32,
    pub s_first:            U32,
    pub s_sequence:         U32,
    pub s_start:            U32,
    pub s_errno:            I32,

    pub s_feature_compat:   U32,
    pub s_feature_incompat: U32,
    pub s_feature_ro_compat:U32,
    pub s_uuid:             [u8; 16],
    pub s_nr_users:         U32,
    pub s_dynsuper:         U32,
    pub s_max_transaction:  U32,
    pub s_max_trans_data:   U32,
    pub s_checksum_type:    u8,
    _pad:                   [u8; 3],
    _pad2:                  [u8; 168],
    pub s_checksum:         U32,
    pub s_users:            [u8; 768],
}

const _: () = assert!(core::mem::size_of::<RawJournalSuperblock>() == 1024);

/// JBD2 incompat feature: revoke records in the journal.
pub const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x001;
/// JBD2 incompat feature: 64-bit block numbers in tags.
pub const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x002;
/// JBD2 incompat feature: async commit (write ordering only, safe for read-only replay).
pub const JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x004;
/// JBD2 incompat feature: checksums v2 on metadata blocks (CRC32c, superseded by v3).
pub const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x008;
/// JBD2 incompat feature: checksums v3 on commit blocks (CRC32c).
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x010;

/// Supported incompat mask — refuse if any other bits are set.
const SUPPORTED_INCOMPAT: u32 =
    JBD2_FEATURE_INCOMPAT_REVOKE
    | JBD2_FEATURE_INCOMPAT_64BIT
    | JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT
    | JBD2_FEATURE_INCOMPAT_CSUM_V2
    | JBD2_FEATURE_INCOMPAT_CSUM_V3;

#[derive(Debug, Clone)]
pub struct JournalSuperblock {
    pub block_size:         u32,
    pub total_blocks:       u32,
    pub first_usable_block: u32,
    pub start_sequence:     u32,
    /// 0 means journal is clean.
    pub start_block:        u32,
    pub errno:              i32,
    pub feature_incompat:   u32,
    pub uuid:               [u8; 16],
}

impl JournalSuperblock {
    pub fn is_clean(&self) -> bool { self.start_block == 0 }
    pub fn needs_recovery(&self) -> bool { self.start_block != 0 }
    pub fn has_64bit(&self) -> bool { self.feature_incompat & JBD2_FEATURE_INCOMPAT_64BIT != 0 }
    pub fn has_csum_v3(&self) -> bool { self.feature_incompat & JBD2_FEATURE_INCOMPAT_CSUM_V3 != 0 }
}

pub fn parse(raw: &RawJournalSuperblock) -> Result<JournalSuperblock, super::JournalError> {
    let magic = raw.h_magic.get();
    if magic != JOURNAL_MAGIC {
        return Err(super::JournalError::BadMagic(magic));
    }

    let incompat = raw.s_feature_incompat.get();
    let unsupported = incompat & !SUPPORTED_INCOMPAT;
    if unsupported != 0 {
        return Err(super::JournalError::UnsupportedFeatures(unsupported));
    }

    let total_blocks = raw.s_maxlen.get();
    let first_usable_block = raw.s_first.get();
    // first_usable_block must be at least 1 (block 0 is the journal superblock itself)
    // and there must be at least one usable block after it.
    if first_usable_block == 0 || first_usable_block >= total_blocks {
        return Err(super::JournalError::InvalidGeometry(total_blocks, first_usable_block));
    }

    Ok(JournalSuperblock {
        block_size:         raw.s_blocksize.get(),
        total_blocks,
        first_usable_block,
        start_sequence:     raw.s_sequence.get(),
        start_block:        raw.s_start.get(),
        errno:              raw.s_errno.get(),
        feature_incompat:   incompat,
        uuid:               raw.s_uuid,
    })
}
