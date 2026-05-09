/// Task 01 — Dirty Flag & Clean Unmount Audit
///
/// After every write operation that calls unmount(), verify that:
///   - s_state & VALID_FS != 0  (filesystem was cleanly unmounted)
///   - s_state & ERROR_FS == 0  (no error flag set)
use std::path::Path;

use ext4_core::block_device::image_file::ImageFileDevice;
use ext4_core::journal::mount::Ext4FsRw;
use ext4_core::superblock::{self, state};
use ext4_core::alloc::Allocator;
use ext4_core::inode_write::{update_inode, InodeUpdate};
use ext4_core::dir_write::{dir_add_entry, dir_file_type};
use ext4_core::alloc::inode_alloc;

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copy `minimal.img` into a temporary file and return a handle to it.
fn make_rw_image() -> tempfile::NamedTempFile {
    let src = fixture_path("minimal.img");
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::io::copy(
        &mut std::fs::File::open(&src).expect("fixture not found — run scripts/make_fixtures.sh"),
        tmp.as_file_mut(),
    )
    .expect("copy fixture");
    tmp.as_file().sync_all().expect("sync");
    tmp
}

/// Assert that the image at `path` was cleanly unmounted.
fn assert_cleanly_unmounted(path: &Path) {
    let dev = ImageFileDevice::open(path).expect("open for state check");
    let sb = superblock::parse(&dev).expect("parse superblock");
    assert!(
        sb.state & state::VALID_FS != 0,
        "VALID_FS not set — filesystem was not cleanly unmounted (s_state={:#06x})",
        sb.state
    );
    assert!(
        sb.state & state::ERROR_FS == 0,
        "ERROR_FS set — filesystem has errors (s_state={:#06x})",
        sb.state
    );
}

// ---------------------------------------------------------------------------
// Write-operation helpers (thin wrappers used by both Task 01 and Task 02)
// ---------------------------------------------------------------------------

/// Create a regular file named `name` in the root directory.
fn create_file_in_root(fs: &mut Ext4FsRw<ImageFileDevice>, name: &str) {
    let mut journal = fs.journal.begin_transaction();
    // We drive the low-level API directly, matching the pattern in pengwin/create.rs.
    let parent_inode_num = 2u32; // root
    let _parent_inode = ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, parent_inode_num)
        .expect("read root inode");

    let new_inum = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        alloc
            .alloc_inode(&mut journal, false, 0)
            .expect("alloc inode")
    };

    inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut journal, new_inum, 0o100644, 0, 0)
        .expect("init inode");

    {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(
            &fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut journal,
            parent_inode_num, name, new_inum, dir_file_type::REG_FILE,
        )
        .expect("dir_add_entry");
    }

    // Update parent mtime/ctime
    update_inode(
        &fs.dev, &fs.sb, &fs.gdt, &mut journal, parent_inode_num,
        InodeUpdate::default().with_mtime(0).with_ctime(0),
    )
    .expect("update parent inode");

    fs.journal.commit(&fs.dev, journal).expect("commit");
}

/// Create a directory named `name` in the root directory.
fn create_dir_in_root(fs: &mut Ext4FsRw<ImageFileDevice>, name: &str) {
    let mut journal = fs.journal.begin_transaction();
    let parent_inode_num = 2u32;

    let new_inum = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        alloc
            .alloc_inode(&mut journal, true, 0)
            .expect("alloc inode for dir")
    };

    inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut journal, new_inum, 0o040755, 0, 0)
        .expect("init dir inode");

    {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(
            &fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut journal,
            parent_inode_num, name, new_inum, dir_file_type::DIR,
        )
        .expect("dir_add_entry parent");
    }
    {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(
            &fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut journal,
            new_inum, ".", new_inum, dir_file_type::DIR,
        )
        .expect("dir_add_entry dot");
    }
    {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(
            &fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut journal,
            new_inum, "..", parent_inode_num, dir_file_type::DIR,
        )
        .expect("dir_add_entry dotdot");
    }

    fs.journal.commit(&fs.dev, journal).expect("commit");
}

// ---------------------------------------------------------------------------
// Task 01 tests
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn dirty_flag_cleared_after_create_file() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    create_file_in_root(&mut fs, "task01_testfile.txt");
    fs.unmount().expect("unmount");
    assert_cleanly_unmounted(tmp.path());
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn dirty_flag_cleared_after_create_dir() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    create_dir_in_root(&mut fs, "task01_testdir");
    fs.unmount().expect("unmount");
    assert_cleanly_unmounted(tmp.path());
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn dirty_flag_cleared_after_write_data() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");

    let file_name = "task01_write.txt";
    create_file_in_root(&mut fs, file_name);

    // Resolve inode and write a small payload.
    let root = ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, 2).expect("root inode");
    let file_inum = ext4_core::dir::lookup(&fs.dev, &fs.sb, &fs.gdt, &root, file_name)
        .expect("lookup")
        .expect("file not found");
    let file_inode = ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, file_inum)
        .expect("file inode");

    let payload = b"hello dirty-flag test";
    {
        let mut journal = fs.journal.begin_transaction();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        ext4_core::file_write::write_file_data(
            &fs.dev, &fs.sb, &mut alloc, &mut journal,
            file_inum, &file_inode, 0, payload,
        )
        .expect("write_file_data");
        update_inode(
            &fs.dev, &fs.sb, &fs.gdt, &mut journal, file_inum,
            InodeUpdate::default().with_size(payload.len() as u64),
        )
        .expect("update inode size");
        fs.journal.commit(&fs.dev, journal).expect("commit");
    }

    fs.unmount().expect("unmount");
    assert_cleanly_unmounted(tmp.path());
}
