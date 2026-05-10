use std::io::{self, Read, Seek, SeekFrom};
use crate::block_device::{BlockDevice, read_sectors};
use crate::superblock::Superblock;
use crate::inode::Inode;
use crate::extent::{ExtentLeaf, collect_leaves_pub};

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("inode is not a regular file or symlink")]
    NotAFile,

    #[error("symlink target is not inline (use FileReader)")]
    SlowSymlink,

    #[error("extent error: {0}")]
    Extent(#[from] crate::extent::ExtentError),

    #[error("block device error: {0}")]
    BlockDevice(#[from] crate::block_device::BlockDeviceError),
}

pub struct FileReader<'a> {
    dev:      &'a dyn BlockDevice,
    sb:       &'a Superblock,
    inode:    &'a Inode,
    position: u64,
    /// Sorted extent leaf cache — built once at construction, used for O(log n) block lookups.
    leaves:   Vec<ExtentLeaf>,
}

impl<'a> std::fmt::Debug for FileReader<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileReader").field("position", &self.position).finish_non_exhaustive()
    }
}

impl<'a> FileReader<'a> {
    pub fn new(dev: &'a dyn BlockDevice, sb: &'a Superblock, inode: &'a Inode) -> Result<Self, FileError> {
        if !inode.is_file() && !inode.is_symlink() {
            return Err(FileError::NotAFile);
        }
        let mut leaves = Vec::new();
        collect_leaves_pub(dev, sb, inode, &mut leaves)?;
        leaves.sort_by_key(|l| l.ee_block.get());
        Ok(Self { dev, sb, inode, position: 0, leaves })
    }

    pub fn size(&self) -> u64 { self.inode.size }

    /// Resolve a logical block number to a physical block, using the cached leaf list.
    fn resolve_lblock(&self, lblock: u64) -> Option<u64> {
        // Binary search for the last leaf whose ee_block <= lblock.
        let idx = self.leaves.partition_point(|l| l.ee_block.get() as u64 <= lblock);
        if idx == 0 {
            return None;
        }
        let leaf = &self.leaves[idx - 1];
        let first = leaf.ee_block.get() as u64;
        let len_raw = leaf.ee_len.get();
        // ee_len > 32768 means uninitialized extent — treat as hole.
        if len_raw > 32768 {
            return None;
        }
        let count = len_raw as u64;
        if lblock < first + count {
            let phys_start = (leaf.ee_start_hi.get() as u64) << 32
                | leaf.ee_start_lo.get() as u64;
            Some(phys_start + (lblock - first))
        } else {
            None
        }
    }
}

impl<'a> Read for FileReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.inode.size {
            return Ok(0);
        }

        let remaining = self.inode.size - self.position;
        let to_read = buf.len().min(remaining as usize);
        let block_size = self.sb.block_size as u64;

        let mut written = 0usize;
        while written < to_read {
            let file_off = self.position + written as u64;
            let lblock = file_off / block_size;
            let block_off = (file_off % block_size) as usize;
            let can_take = ((block_size as usize) - block_off).min(to_read - written);

            match self.resolve_lblock(lblock) {
                None => {
                    // Sparse hole or uninitialized extent — fill with zeros.
                    buf[written..written + can_take].fill(0);
                }
                Some(block_num) => {
                    let sectors_per_block = block_size / 512;
                    let start_sector = block_num
                        .checked_mul(sectors_per_block)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "block number overflow"))?;
                    let block_data = read_sectors(self.dev, start_sector, sectors_per_block)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                    buf[written..written + can_take]
                        .copy_from_slice(&block_data[block_off..block_off + can_take]);
                }
            }

            written += can_take;
        }

        self.position += written as u64;
        Ok(written)
    }
}

impl<'a> Seek for FileReader<'a> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos: i64 = match pos {
            SeekFrom::Start(n) => {
                i64::try_from(n)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek offset too large"))?
            }
            SeekFrom::End(n) => {
                (self.inode.size as i64)
                    .checked_add(n)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek offset out of range"))?
            }
            SeekFrom::Current(n) => {
                (self.position as i64)
                    .checked_add(n)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek offset out of range"))?
            }
        };
        if new_pos < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start of file"));
        }
        self.position = (new_pos as u64).min(self.inode.size);
        Ok(self.position)
    }
}

/// Read symlink target. Returns the path as a String.
/// Only handles fast (inline) symlinks; slow symlinks return `Err(FileError::SlowSymlink)`.
pub fn read_symlink(inode: &Inode) -> Result<String, FileError> {
    if inode.size < 60 {
        let bytes = &inode.block_data[..inode.size as usize];
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    Err(FileError::SlowSymlink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use zerocopy::little_endian::{U16, U32};
    use zerocopy::{AsBytes, FromZeroes};
    use crate::block_device::BlockDeviceError;
    use crate::inode::{Inode, mode};
    use crate::superblock::Superblock;
    use crate::extent::{ExtentHeader, ExtentLeaf, EXTENT_MAGIC};

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

    const BLOCK_SIZE: usize = 4096;

    fn make_sb() -> Superblock {
        Superblock {
            block_size:        BLOCK_SIZE as u32,
            blocks_count:      256,
            inodes_count:      256,
            inodes_per_group:  256,
            blocks_per_group:  256,
            first_data_block:  0,
            uuid:              [0u8; 16],
            volume_name:       String::new(),
            desc_size:         64,
            feature_incompat:  0,
            feature_ro_compat: 0,
            inode_size:        256,
            state:             0x0001,
        }
    }

    fn make_extent_root_leaves(leaves: &[(u32, u16, u64)]) -> [u8; 60] {
        let mut data = [0u8; 60];
        let mut hdr = ExtentHeader::new_zeroed();
        hdr.eh_magic   = U16::new(EXTENT_MAGIC);
        hdr.eh_entries = U16::new(leaves.len() as u16);
        hdr.eh_max     = U16::new(4);
        hdr.eh_depth   = U16::new(0);
        data[..12].copy_from_slice(hdr.as_bytes());
        for (i, (ee_block, ee_len, phys)) in leaves.iter().enumerate() {
            let mut leaf = ExtentLeaf::new_zeroed();
            leaf.ee_block    = U32::new(*ee_block);
            leaf.ee_len      = U16::new(*ee_len);
            leaf.ee_start_hi = U16::new((*phys >> 32) as u16);
            leaf.ee_start_lo = U32::new(*phys as u32);
            let off = 12 + i * 12;
            data[off..off + 12].copy_from_slice(leaf.as_bytes());
        }
        data
    }

    fn make_file_inode(block_data: [u8; 60], size: u64) -> Inode {
        Inode {
            mode:        mode::S_IFREG,
            uid:         0, gid: 0,
            size,
            atime: 0, mtime: 0, ctime: 0,
            links_count: 1,
            flags:       0x80000,
            block_data,
        }
    }

    fn make_symlink_inode(block_data: [u8; 60], size: u64) -> Inode {
        Inode {
            mode:        mode::S_IFLNK,
            uid:         0, gid: 0,
            size,
            atime: 0, mtime: 0, ctime: 0,
            links_count: 1,
            flags:       0x80000,
            block_data,
        }
    }

    /// Build a device with content blocks at the given physical block numbers.
    fn make_device_with_blocks(blocks: &[(u64, Vec<u8>)]) -> MemDevice {
        let max_block = blocks.iter().map(|(b, _)| *b).max().unwrap_or(0);
        let mut data = vec![0u8; (max_block as usize + 1) * BLOCK_SIZE];
        for (phys, content) in blocks {
            let start = *phys as usize * BLOCK_SIZE;
            let len = content.len().min(BLOCK_SIZE);
            data[start..start + len].copy_from_slice(&content[..len]);
        }
        MemDevice(data)
    }

    #[test]
    fn read_small_file() {
        let content = b"hello file\n";
        let mut block_content = vec![0u8; BLOCK_SIZE];
        block_content[..content.len()].copy_from_slice(content);

        let block_data = make_extent_root_leaves(&[(0, 1, 2)]);
        let inode = make_file_inode(block_data, content.len() as u64);
        let dev = make_device_with_blocks(&[(2, block_content)]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn read_multi_block_file() {
        let block_a = vec![0xAAu8; BLOCK_SIZE];
        let block_b = vec![0xBBu8; BLOCK_SIZE];
        let block_c = vec![0xCCu8; BLOCK_SIZE];

        let block_data = make_extent_root_leaves(&[(0, 3, 5)]);
        let inode = make_file_inode(block_data, 3 * BLOCK_SIZE as u64);
        let dev = make_device_with_blocks(&[
            (5, block_a.clone()),
            (6, block_b.clone()),
            (7, block_c.clone()),
        ]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut out = vec![0u8; 3 * BLOCK_SIZE];
        reader.read_exact(&mut out).unwrap();
        assert!(out[..BLOCK_SIZE].iter().all(|&b| b == 0xAA));
        assert!(out[BLOCK_SIZE..2 * BLOCK_SIZE].iter().all(|&b| b == 0xBB));
        assert!(out[2 * BLOCK_SIZE..].iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn read_with_offset() {
        let mut content = vec![0u8; BLOCK_SIZE];
        for (i, b) in content.iter_mut().enumerate() { *b = (i % 256) as u8; }

        let block_data = make_extent_root_leaves(&[(0, 1, 3)]);
        let inode = make_file_inode(block_data, BLOCK_SIZE as u64);
        let dev = make_device_with_blocks(&[(3, content.clone())]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        reader.seek(SeekFrom::Start(100)).unwrap();
        let mut out = [0u8; 50];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out[..], &content[100..150]);
    }

    /// Mirrors the Windows read pattern: a single read with a 4 KiB buffer for a
    /// file far smaller than that. Returns `n == file_size` and a follow-up read
    /// at EOF returns 0.
    #[test]
    fn read_small_file_with_oversized_buffer() {
        let content = b"hello world\n";
        let mut block_content = vec![0u8; BLOCK_SIZE];
        block_content[..content.len()].copy_from_slice(content);

        let block_data = make_extent_root_leaves(&[(0, 1, 2)]);
        let inode = make_file_inode(block_data, content.len() as u64);
        let dev = make_device_with_blocks(&[(2, block_content)]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut out = vec![0xCDu8; 4096];
        let n = reader.read(&mut out).unwrap();
        assert_eq!(n, content.len(), "should report bytes_read == file_size");
        assert_eq!(&out[..n], content);

        let n2 = reader.read(&mut out).unwrap();
        assert_eq!(n2, 0);
    }

    /// Two FileReader instances built from the same inode must return byte-identical
    /// content. Catches non-determinism in extent lookup or block-device caching.
    #[test]
    fn repeated_reads_are_deterministic() {
        let content = b"hello world\n";
        let mut block_content = vec![0u8; BLOCK_SIZE];
        block_content[..content.len()].copy_from_slice(content);

        let block_data = make_extent_root_leaves(&[(0, 1, 2)]);
        let inode = make_file_inode(block_data, content.len() as u64);
        let dev = make_device_with_blocks(&[(2, block_content)]);
        let sb = make_sb();

        let mut a = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut b = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut buf_a = vec![0u8; 4096];
        let mut buf_b = vec![0u8; 4096];
        let na = a.read(&mut buf_a).unwrap();
        let nb = b.read(&mut buf_b).unwrap();
        assert_eq!(na, nb);
        assert_eq!(&buf_a[..na], &buf_b[..nb]);
    }

    #[test]
    fn read_past_eof() {
        let content = b"short";
        let mut block_content = vec![0u8; BLOCK_SIZE];
        block_content[..content.len()].copy_from_slice(content);

        let block_data = make_extent_root_leaves(&[(0, 1, 1)]);
        let inode = make_file_inode(block_data, content.len() as u64);
        let dev = make_device_with_blocks(&[(1, block_content)]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut out = vec![0u8; 1000];
        let n = reader.read(&mut out).unwrap();
        assert_eq!(n, content.len());
        assert_eq!(&out[..n], content);
    }

    #[test]
    fn read_sparse_hole() {
        // Two extents: blocks 0 and 2; block 1 is a hole.
        let block_a = vec![0xAAu8; BLOCK_SIZE];
        let block_c = vec![0xCCu8; BLOCK_SIZE];

        let block_data = make_extent_root_leaves(&[(0, 1, 5), (2, 1, 7)]);
        let inode = make_file_inode(block_data, 3 * BLOCK_SIZE as u64);
        let dev = make_device_with_blocks(&[(5, block_a), (7, block_c)]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let mut out = vec![0u8; 3 * BLOCK_SIZE];
        reader.read_exact(&mut out).unwrap();

        assert!(out[..BLOCK_SIZE].iter().all(|&b| b == 0xAA));
        assert!(out[BLOCK_SIZE..2 * BLOCK_SIZE].iter().all(|&b| b == 0));
        assert!(out[2 * BLOCK_SIZE..].iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn seek_from_end() {
        let content = b"abcdef";
        let mut block_content = vec![0u8; BLOCK_SIZE];
        block_content[..content.len()].copy_from_slice(content);

        let block_data = make_extent_root_leaves(&[(0, 1, 2)]);
        let inode = make_file_inode(block_data, content.len() as u64);
        let dev = make_device_with_blocks(&[(2, block_content)]);
        let sb = make_sb();

        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let pos = reader.seek(SeekFrom::End(-1)).unwrap();
        assert_eq!(pos, content.len() as u64 - 1);
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], b'f');
    }

    #[test]
    fn seek_before_start_is_error() {
        let content = b"abcdef";
        let mut block_content = vec![0u8; BLOCK_SIZE];
        block_content[..content.len()].copy_from_slice(content);
        let block_data = make_extent_root_leaves(&[(0, 1, 2)]);
        let inode = make_file_inode(block_data, content.len() as u64);
        let dev = make_device_with_blocks(&[(2, block_content)]);
        let sb = make_sb();
        let mut reader = FileReader::new(&dev, &sb, &inode).unwrap();
        let err = reader.seek(SeekFrom::End(-1000)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn fast_symlink() {
        let target = "/hello.txt";
        let mut block_data = [0u8; 60];
        block_data[..target.len()].copy_from_slice(target.as_bytes());
        let inode = make_symlink_inode(block_data, target.len() as u64);
        let result = read_symlink(&inode).unwrap();
        assert_eq!(result, target);
    }

    #[test]
    fn not_a_file_returns_error() {
        let inode = Inode {
            mode:        crate::inode::mode::S_IFDIR,
            uid: 0, gid: 0, size: 0,
            atime: 0, mtime: 0, ctime: 0,
            links_count: 2,
            flags: 0x80000,
            block_data: [0u8; 60],
        };
        let dev = MemDevice(vec![0u8; 512]);
        let sb = make_sb();
        match FileReader::new(&dev, &sb, &inode) {
            Err(FileError::NotAFile) => {}
            other => panic!("expected NotAFile, got {:?}", other),
        }
    }
}
