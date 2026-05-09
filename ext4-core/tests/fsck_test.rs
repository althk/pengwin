// Task 02 — e2fsck Integration Tests
//
// Each test performs one write operation, cleanly unmounts, then runs
// `e2fsck -fn` to verify filesystem integrity.
//
// On Linux/macOS: calls e2fsck directly.
// On Windows: translates the path to a WSL path and calls e2fsck via `wsl`.
// Tests are skipped if neither e2fsck nor wsl+e2fsck is available.

use std::path::Path;

use ext4_core::block_device::image_file::ImageFileDevice;
use ext4_core::journal::mount::Ext4FsRw;
use ext4_core::alloc::Allocator;
use ext4_core::alloc::inode_alloc;
use ext4_core::dir_write::{dir_add_entry, dir_remove_entry, dir_rename, hard_link, dir_file_type};
use ext4_core::extent_write::extent_truncate;
use ext4_core::file_write::write_file_data;
use ext4_core::inode_write::{update_inode, InodeUpdate};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

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

/// Run `e2fsck -fn <path>`, return true if exit code is 0 (no errors).
/// On Windows, translates the path to a WSL mount path and invokes via `wsl e2fsck`.
fn fsck(path: &Path) -> bool {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("e2fsck")
            .args(["-fn", path.to_str().expect("path to str")])
            .output()
            .expect("e2fsck not found — install e2fsprogs");
        output.status.code() == Some(0)
    }
    #[cfg(windows)]
    {
        // Convert Windows path (e.g. C:\foo\bar.img) to WSL path (/mnt/c/foo/bar.img).
        let win_path = path.to_str().expect("path to str");
        let wsl_path = windows_path_to_wsl(win_path);
        let output = std::process::Command::new("wsl")
            .args(["e2fsck", "-fn", &wsl_path])
            .output()
            .expect("wsl not found — enable WSL and install e2fsprogs inside it");
        output.status.code() == Some(0)
    }
}

/// Convert a Windows absolute path to its WSL /mnt/<drive>/... equivalent.
#[cfg(windows)]
fn windows_path_to_wsl(win_path: &str) -> String {
    // Handle both C:\foo\bar and C:/foo/bar
    let p = win_path.replace('\\', "/");
    if p.len() >= 2 && p.as_bytes()[1] == b':' {
        let drive = p[..1].to_lowercase();
        let rest = &p[2..]; // strip the colon
        format!("/mnt/{drive}{rest}")
    } else {
        p
    }
}

// ---------------------------------------------------------------------------
// Low-level write helpers
// ---------------------------------------------------------------------------

fn create_file(fs: &mut Ext4FsRw<ImageFileDevice>, parent_inum: u32, name: &str) -> u32 {
    let mut txn = fs.journal.begin_transaction();

    let new_inum = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        alloc.alloc_inode(&mut txn, false, 0).expect("alloc inode")
    };
    inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, new_inum, 0o100644, 0, 0)
        .expect("init inode");
    {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            parent_inum, name, new_inum, dir_file_type::REG_FILE)
            .expect("dir_add_entry");
    }
    fs.journal.commit(&fs.dev, txn).expect("commit");
    new_inum
}

fn create_dir(fs: &mut Ext4FsRw<ImageFileDevice>, parent_inum: u32, name: &str) -> u32 {
    let mut txn = fs.journal.begin_transaction();

    let new_inum = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        alloc.alloc_inode(&mut txn, true, 0).expect("alloc inode for dir")
    };
    inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, new_inum, 0o040755, 0, 0)
        .expect("init dir inode");

    for (dir_inum, entry_name) in [
        (parent_inum, name),
        (new_inum, "."),
    ] {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            dir_inum, entry_name, new_inum, dir_file_type::DIR)
            .expect("dir_add_entry");
    }
    {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            new_inum, "..", parent_inum, dir_file_type::DIR)
            .expect("dir_add_entry ..");
    }
    fs.journal.commit(&fs.dev, txn).expect("commit");
    new_inum
}

fn delete_file(fs: &mut Ext4FsRw<ImageFileDevice>, parent_inum: u32, name: &str) {
    let mut txn = fs.journal.begin_transaction();
    let removed_inum = dir_remove_entry(&fs.dev, &fs.sb, &fs.gdt, &mut txn, parent_inum, name)
        .expect("dir_remove_entry");
    let inode = ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, removed_inum)
        .expect("read inode");
    let new_links = inode.links_count.saturating_sub(1);
    update_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, removed_inum,
        InodeUpdate::default().with_links_count(new_links))
        .expect("update links");
    if new_links == 0 {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        alloc.free_inode(&mut txn, removed_inum).expect("free_inode");
    }
    fs.journal.commit(&fs.dev, txn).expect("commit");
}

fn write_data(fs: &mut Ext4FsRw<ImageFileDevice>, inode_num: u32, data: &[u8]) {
    let inode = ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, inode_num)
        .expect("read inode");
    let mut txn = fs.journal.begin_transaction();
    {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        write_file_data(&fs.dev, &fs.sb, &mut alloc, &mut txn, inode_num, &inode, 0, data)
            .expect("write_file_data");
    }
    update_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, inode_num,
        InodeUpdate::default().with_size(data.len() as u64))
        .expect("update size");
    fs.journal.commit(&fs.dev, txn).expect("commit");
}

// ---------------------------------------------------------------------------
// Task 02 tests — one per write operation
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn create_file_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    create_file(&mut fs, 2, "fsck_test_file.txt");
    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after create_file");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn create_directory_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    create_dir(&mut fs, 2, "fsck_test_dir");
    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after create_directory");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn delete_file_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    create_file(&mut fs, 2, "to_delete.txt");
    delete_file(&mut fs, 2, "to_delete.txt");
    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after delete_file");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn rename_file_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    create_file(&mut fs, 2, "before_rename.txt");

    {
        let mut txn = fs.journal.begin_transaction();
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_rename(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            2, "before_rename.txt", 2, "after_rename.txt")
            .expect("dir_rename");
        fs.journal.commit(&fs.dev, txn).expect("commit");
    }

    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after rename_file");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn write_data_small_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    let inum = create_file(&mut fs, 2, "small_write.txt");
    write_data(&mut fs, inum, b"small payload");
    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after write_data (small)");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn write_data_multi_block_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    let inum = create_file(&mut fs, 2, "multi_block_write.bin");
    // 3 × 4096 bytes to force multi-block allocation.
    let payload = vec![0xABu8; 3 * 4096];
    write_data(&mut fs, inum, &payload);
    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after write_data (multi-block)");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn hard_link_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    let target_inum = create_file(&mut fs, 2, "link_target.txt");

    {
        let mut txn = fs.journal.begin_transaction();
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        hard_link(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            target_inum, 2, "link_alias.txt")
            .expect("hard_link");
        fs.journal.commit(&fs.dev, txn).expect("commit");
    }

    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after hard_link");
}

#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn truncate_passes_fsck() {
    let tmp = make_rw_image();
    let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw");
    let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
    let inum = create_file(&mut fs, 2, "truncate_me.bin");

    // Write 3 blocks then truncate to 1 block.
    let payload = vec![0xCDu8; 3 * 4096];
    write_data(&mut fs, inum, &payload);

    {
        let mut txn = fs.journal.begin_transaction();
        {
            let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
            // keep_from = 1 means keep only block 0, free blocks 1+
            extent_truncate(&fs.dev, &fs.sb, &mut alloc, &mut txn, inum, 1)
                .expect("extent_truncate");
        }
        update_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, inum,
            InodeUpdate::default().with_size(4096))
            .expect("update size after truncate");
        fs.journal.commit(&fs.dev, txn).expect("commit");
    }

    fs.unmount().expect("unmount");
    assert!(fsck(tmp.path()), "e2fsck failed after truncate");
}
