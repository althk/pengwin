use ext4_core::{
    block_device::BlockDevice, group_desc::GroupDescTable, journal::writer::JournalWriter,
    superblock::Superblock,
};
use parking_lot::Mutex;

pub struct Ext4Fs<D: BlockDevice> {
    pub(crate) dev: D,
    pub(crate) sb: Superblock,
    pub(crate) gdt: Mutex<GroupDescTable>,
    pub(crate) journal: Mutex<JournalWriter>,
    pub(crate) read_only: bool,
}

impl<D: BlockDevice + Send + Sync + 'static> Ext4Fs<D> {
    /// Open the filesystem read-only. Refuses to mount if the journal is dirty
    /// (would require replay, which writes to the device).
    ///
    /// Use [`open_rw`](Self::open_rw) for write access, or [`open_ro_force`](Self::open_ro_force)
    /// to mount RO without replay even if the journal is dirty.
    pub fn open(dev: D) -> Result<Self, Ext4FsError> {
        Self::open_inner(dev, OpenMode::ReadOnly)
    }

    /// Open the filesystem read-write. Replays the journal if needed.
    pub fn open_rw(dev: D) -> Result<Self, Ext4FsError> {
        Self::open_inner(dev, OpenMode::ReadWrite)
    }

    /// Open read-only, skipping journal replay even when the journal is dirty.
    /// The on-disk view will reflect the last cleanly committed state; uncommitted
    /// journal contents are not visible. Intended as an escape hatch when
    /// e2fsck is undesirable (e.g. read-only physical media).
    pub fn open_ro_force(dev: D) -> Result<Self, Ext4FsError> {
        Self::open_inner(dev, OpenMode::ReadOnlyForce)
    }

    fn open_inner(dev: D, mode: OpenMode) -> Result<Self, Ext4FsError> {
        let sb = ext4_core::superblock::parse(&dev)?;
        let gdt = GroupDescTable::load(&dev, &sb)?;

        let journal = match mode {
            OpenMode::ReadWrite => ext4_core::journal::check_and_recover(&dev, &sb, &gdt)
                .map_err(Ext4FsError::Journal)?,
            OpenMode::ReadOnly => {
                let j = ext4_core::journal::Journal::load(&dev, &sb, &gdt)
                    .map_err(Ext4FsError::Journal)?;
                if j.sb.errno != 0 {
                    return Err(Ext4FsError::Journal(
                        ext4_core::journal::JournalError::JournalErrno(j.sb.errno),
                    ));
                }
                if j.sb.needs_recovery() {
                    return Err(Ext4FsError::DirtyJournalReadOnly);
                }
                j
            }
            OpenMode::ReadOnlyForce => {
                let j = ext4_core::journal::Journal::load(&dev, &sb, &gdt)
                    .map_err(Ext4FsError::Journal)?;
                if j.sb.needs_recovery() {
                    tracing::warn!(
                        "mounting read-only with dirty journal (--force); \
                         uncommitted journal contents will not be visible"
                    );
                }
                j
            }
        };

        let writer = JournalWriter::new(&journal);
        let read_only = !matches!(mode, OpenMode::ReadWrite);
        tracing::info!(
            volume_name = %sb.volume_name,
            block_size = sb.block_size,
            blocks = sb.blocks_count,
            read_only,
            "ext4 filesystem opened"
        );
        Ok(Self {
            dev,
            sb,
            gdt: Mutex::new(gdt),
            journal: Mutex::new(writer),
            read_only,
        })
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Look up an inode by number.
    pub fn inode(&self, num: u32) -> Result<ext4_core::inode::Inode, Ext4FsError> {
        let gdt = self.gdt.lock();
        ext4_core::inode::read_inode(&self.dev, &self.sb, &gdt, num).map_err(Ext4FsError::from)
    }

    /// Read directory entries for a directory inode.
    pub fn read_dir(
        &self,
        dir_inode: &ext4_core::inode::Inode,
    ) -> Result<Vec<ext4_core::dir::DirEntry>, Ext4FsError> {
        let gdt = self.gdt.lock();
        ext4_core::dir::read_dir(&self.dev, &self.sb, &gdt, dir_inode).map_err(Ext4FsError::from)
    }

    pub fn dev(&self) -> &D {
        &self.dev
    }
    pub fn sb(&self) -> &ext4_core::superblock::Superblock {
        &self.sb
    }

    /// Read the target of a symlink inode as a String.
    pub fn read_symlink_target(
        &self,
        inode: &ext4_core::inode::Inode,
    ) -> Result<String, Ext4FsError> {
        match ext4_core::file::read_symlink(inode) {
            Ok(s) => return Ok(s),
            Err(ext4_core::file::FileError::SlowSymlink) => {}
            Err(e) => return Err(Ext4FsError::File(e)),
        }
        use std::io::Read;
        let mut reader = ext4_core::file::FileReader::new(&self.dev, &self.sb, inode)
            .map_err(Ext4FsError::File)?;
        let mut s = String::new();
        reader.read_to_string(&mut s).map_err(|e| {
            Ext4FsError::File(ext4_core::file::FileError::BlockDevice(
                ext4_core::block_device::BlockDeviceError::Io(e),
            ))
        })?;
        Ok(s)
    }

    /// Look up a name in a directory inode, returning the inode number if found.
    pub fn lookup(
        &self,
        dir_inode: &ext4_core::inode::Inode,
        name: &str,
    ) -> Result<Option<u32>, Ext4FsError> {
        let gdt = self.gdt.lock();
        ext4_core::dir::lookup(&self.dev, &self.sb, &gdt, dir_inode, name)
            .map_err(Ext4FsError::from)
    }

    /// Resolve an absolute path like "/subdir/file.txt" to an inode number.
    pub fn resolve_path(&self, path: &str) -> Result<u32, Ext4FsError> {
        self.resolve_path_with_hops(path, 0)
    }

    fn resolve_path_with_hops(&self, path: &str, hops: u32) -> Result<u32, Ext4FsError> {
        if hops > MAX_SYMLINK_HOPS {
            return Err(Ext4FsError::SymlinkLoop);
        }

        let mut current = 2u32; // root inode
        for component in path.split('/').filter(|s| !s.is_empty()) {
            if component == ".." {
                return Err(Ext4FsError::DotDotNotAllowed);
            }
            if component == "." {
                continue;
            }

            let inode = self.inode(current)?;
            if !inode.is_dir() {
                return Err(Ext4FsError::NotADirectory(component.to_string()));
            }
            match self.lookup(&inode, component)? {
                Some(n) => current = n,
                None => return Err(Ext4FsError::NotFound(path.to_string())),
            }
        }
        Ok(current)
    }
}

/// Maximum number of symlink hops allowed during path resolution.
const MAX_SYMLINK_HOPS: u32 = 40;

#[derive(Copy, Clone, Debug)]
enum OpenMode {
    ReadOnly,
    ReadOnlyForce,
    ReadWrite,
}

/// Per-handle context for an open file or directory.
pub enum FileHandle {
    File {
        inode_num: u32,
        inode: ext4_core::inode::Inode,
    },
    Directory {
        inode_num: u32,
        inode: ext4_core::inode::Inode,
        dir_buffer: winfsp::filesystem::DirBuffer,
    },
    Symlink {
        inode_num: u32,
        inode: ext4_core::inode::Inode,
        target: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Ext4FsError {
    #[error("superblock error: {0}")]
    Superblock(#[from] ext4_core::superblock::SuperblockError),

    #[error("group descriptor error: {0}")]
    GroupDesc(#[from] ext4_core::group_desc::GroupDescError),

    #[error("inode error: {0}")]
    Inode(#[from] ext4_core::inode::InodeError),

    #[error("directory error: {0}")]
    Dir(#[from] ext4_core::dir::DirError),

    #[error("file error: {0}")]
    File(#[from] ext4_core::file::FileError),

    #[error("journal error: {0}")]
    Journal(ext4_core::journal::JournalError),

    #[error("path not found: {0}")]
    NotFound(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),

    #[error("'..' components are not permitted in paths")]
    DotDotNotAllowed,

    #[error("symlink loop detected (too many hops)")]
    SymlinkLoop,

    #[error(
        "journal is dirty — refusing read-only mount; \
             use --rw to replay, run e2fsck, or pass --force to mount RO without replay"
    )]
    DirtyJournalReadOnly,
}
