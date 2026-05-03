use ext4_core::{
    block_device::BlockDevice,
    superblock::Superblock,
    group_desc::GroupDescTable,
};

pub struct Ext4Fs<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    gdt: GroupDescTable,
}

impl<D: BlockDevice + 'static> Ext4Fs<D> {
    /// Open an ext4 filesystem on the given block device.
    pub fn open(dev: D) -> Result<Self, Ext4FsError> {
        let sb = ext4_core::superblock::parse(&dev)?;
        let gdt = GroupDescTable::load(&dev, &sb)?;
        tracing::info!(
            volume_name = %sb.volume_name,
            block_size = sb.block_size,
            blocks = sb.blocks_count,
            "ext4 filesystem opened"
        );
        Ok(Self { dev, sb, gdt })
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Look up an inode by number.
    pub fn inode(&self, num: u32) -> Result<ext4_core::inode::Inode, Ext4FsError> {
        ext4_core::inode::read_inode(&self.dev, &self.sb, &self.gdt, num)
            .map_err(Ext4FsError::from)
    }

    /// Read directory entries for a directory inode.
    pub fn read_dir(
        &self,
        dir_inode: &ext4_core::inode::Inode,
    ) -> Result<Vec<ext4_core::dir::DirEntry>, Ext4FsError> {
        ext4_core::dir::read_dir(&self.dev, &self.sb, &self.gdt, dir_inode)
            .map_err(Ext4FsError::from)
    }

    pub fn dev(&self) -> &D { &self.dev }
    pub fn sb(&self) -> &ext4_core::superblock::Superblock { &self.sb }

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
        // Slow symlink: target is in the data blocks.
        use std::io::Read;
        let mut reader = ext4_core::file::FileReader::new(&self.dev, &self.sb, inode)
            .map_err(Ext4FsError::File)?;
        let mut s = String::new();
        reader.read_to_string(&mut s)
            .map_err(|e| Ext4FsError::File(ext4_core::file::FileError::BlockDevice(
                ext4_core::block_device::BlockDeviceError::Io(e),
            )))?;
        Ok(s)
    }

    /// Look up a name in a directory inode, returning the inode number if found.
    pub fn lookup(
        &self,
        dir_inode: &ext4_core::inode::Inode,
        name: &str,
    ) -> Result<Option<u32>, Ext4FsError> {
        ext4_core::dir::lookup(&self.dev, &self.sb, &self.gdt, dir_inode, name)
            .map_err(Ext4FsError::from)
    }

    /// Resolve an absolute path like "/subdir/file.txt" to an inode number.
    /// Path separator is always '/' (WinFsp normalizes backslashes).
    pub fn resolve_path(&self, path: &str) -> Result<u32, Ext4FsError> {
        let mut current = 2u32; // root inode
        for component in path.split('/').filter(|s| !s.is_empty()) {
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
        /// Resolved symlink target (Unix path, may be absolute).
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

    #[error("path not found: {0}")]
    NotFound(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),
}
