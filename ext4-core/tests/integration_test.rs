use ext4_core::{
    block_device::image_file::ImageFileDevice,
    superblock,
    group_desc::GroupDescTable,
    inode,
    dir,
    file::{self, FileReader},
};
use std::io::{Read, Seek, SeekFrom};

fn open_fixture(name: &str) -> ImageFileDevice {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    ImageFileDevice::open(&path).expect("fixture not found — run scripts/make_fixtures.sh")
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_superblock_minimal() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    assert_eq!(sb.block_size, 4096);
    assert_eq!(sb.volume_name, "test-vol");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_superblock_1k_blocks() {
    let dev = open_fixture("ext4_1k.img");
    let sb = superblock::parse(&dev).unwrap();
    assert_eq!(sb.block_size, 1024);
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_read_root_dir() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();
    let entries = dir::read_dir(&dev, &sb, &gdt, &root).unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"."));
    assert!(names.contains(&".."));
    assert!(names.contains(&"hello.txt"));
    assert!(names.contains(&"subdir"));
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_read_file_content() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();
    let entry_inum = dir::lookup(&dev, &sb, &gdt, &root, "hello.txt")
        .unwrap()
        .expect("hello.txt not found");
    let file_inode = inode::read_inode(&dev, &sb, &gdt, entry_inum).unwrap();
    let mut reader = FileReader::new(&dev, &sb, &file_inode).unwrap();
    let mut content = String::new();
    reader.read_to_string(&mut content).unwrap();
    assert_eq!(content.trim(), "hello world");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_nested_directory() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();

    let subdir_inum = dir::lookup(&dev, &sb, &gdt, &root, "subdir")
        .unwrap()
        .expect("subdir not found");
    let subdir_inode = inode::read_inode(&dev, &sb, &gdt, subdir_inum).unwrap();
    assert!(subdir_inode.is_dir());

    let nested_inum = dir::lookup(&dev, &sb, &gdt, &subdir_inode, "nested.txt")
        .unwrap()
        .expect("nested.txt not found");
    let nested_inode = inode::read_inode(&dev, &sb, &gdt, nested_inum).unwrap();
    let mut reader = FileReader::new(&dev, &sb, &nested_inode).unwrap();
    let mut content = String::new();
    reader.read_to_string(&mut content).unwrap();
    assert_eq!(content.trim(), "nested file");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_symlink() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();

    let link_inum = dir::lookup(&dev, &sb, &gdt, &root, "link.txt")
        .unwrap()
        .expect("link.txt not found");
    let link_inode = inode::read_inode(&dev, &sb, &gdt, link_inum).unwrap();
    assert!(link_inode.is_symlink());

    let target = file::read_symlink(&link_inode).unwrap();
    assert_eq!(target, "/hello.txt");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_lookup_missing_returns_none() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();
    let result = dir::lookup(&dev, &sb, &gdt, &root, "does_not_exist.xyz").unwrap();
    assert!(result.is_none());
}

// Task 02: large file — verify 64-bit size and that seeking to a high offset returns zeros.
#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_large_sparse_file_size() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();

    let inum = dir::lookup(&dev, &sb, &gdt, &root, "large_sparse.bin")
        .unwrap()
        .expect("large_sparse.bin not found");
    let file_inode = inode::read_inode(&dev, &sb, &gdt, inum).unwrap();

    let expected_size: u64 = 8u64 * 1024 * 1024 * 1024; // 8 GiB
    assert_eq!(file_inode.size, expected_size, "large_sparse.bin size mismatch");

    // Read 512 bytes at a 4GiB offset — all must be zero (sparse hole).
    let mut reader = FileReader::new(&dev, &sb, &file_inode).unwrap();
    reader.seek(SeekFrom::Start(4u64 * 1024 * 1024 * 1024)).unwrap();
    let mut buf = [0xFFu8; 512];
    reader.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "expected zero bytes in sparse region");
}

// Task 03: sparse file — verify that the 3MB hole (bytes 1MB..4MB) returns zeros.
#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn test_sparse_file_hole_is_zero() {
    let dev = open_fixture("minimal.img");
    let sb = superblock::parse(&dev).unwrap();
    let gdt = GroupDescTable::load(&dev, &sb).unwrap();
    let root = inode::read_inode(&dev, &sb, &gdt, 2).unwrap();

    let inum = dir::lookup(&dev, &sb, &gdt, &root, "sparse.bin")
        .unwrap()
        .expect("sparse.bin not found");
    let file_inode = inode::read_inode(&dev, &sb, &gdt, inum).unwrap();

    assert_eq!(file_inode.size, 4 * 1024 * 1024, "sparse.bin should be 4MB");

    // The hole starts at 1MB. Read 512 bytes from 2MB offset — must all be zero.
    let mut reader = FileReader::new(&dev, &sb, &file_inode).unwrap();
    reader.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
    let mut buf = [0xFFu8; 512];
    reader.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "expected zero bytes in hole region");
}
