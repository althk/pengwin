/// Task 03 — Crash Simulation Tests
///
/// A `FaultInjectDevice` wraps `ImageFileDevice` and aborts writes after a
/// configurable number of sectors, simulating a power-loss crash mid-transaction.
/// Each test verifies that journal replay restores a consistent filesystem.
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use ext4_core::block_device::{BlockDevice, BlockDeviceError, image_file::ImageFileDevice};
use ext4_core::journal::mount::Ext4FsRw;
use ext4_core::journal::check_and_recover;
use ext4_core::superblock;
use ext4_core::group_desc::GroupDescTable;
use ext4_core::alloc::Allocator;
use ext4_core::alloc::inode_alloc;
use ext4_core::dir_write::{dir_add_entry, dir_file_type};

// ---------------------------------------------------------------------------
// FaultInjectDevice
// ---------------------------------------------------------------------------

struct FaultInjectDevice {
    inner: ImageFileDevice,
    fault_on_write: Arc<AtomicU32>,
    write_count: Arc<AtomicU32>,
}

impl FaultInjectDevice {
    fn new(inner: ImageFileDevice, fault_after: u32) -> Self {
        FaultInjectDevice {
            inner,
            fault_on_write: Arc::new(AtomicU32::new(fault_after)),
            write_count: Arc::new(AtomicU32::new(0)),
        }
    }

    #[allow(dead_code)]
    fn reset_count(&self) {
        self.write_count.store(0, Ordering::SeqCst);
    }
}

impl BlockDevice for FaultInjectDevice {
    fn read_sector(&self, idx: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
        self.inner.read_sector(idx, buf)
    }

    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }

    fn write_sector(&self, idx: u64, buf: &[u8; 512]) -> Result<(), BlockDeviceError> {
        let count = self.write_count.fetch_add(1, Ordering::SeqCst);
        if count >= self.fault_on_write.load(Ordering::SeqCst) {
            return Err(BlockDeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected fault",
            )));
        }
        self.inner.write_sector(idx, buf)
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Helpers
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

/// Run `e2fsck -fn` on the image.
/// On Windows, translates the path to a WSL mount path and invokes via `wsl`.
fn fsck(path: &Path) -> bool {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("e2fsck")
            .args(["-fn", path.to_str().expect("path")])
            .output()
            .expect("e2fsck not found — install e2fsprogs");
        return output.status.code() == Some(0);
    }
    #[cfg(windows)]
    {
        let wsl_path = windows_path_to_wsl(path.to_str().expect("path"));
        let output = std::process::Command::new("wsl")
            .args(["e2fsck", "-fn", &wsl_path])
            .output()
            .expect("wsl not found — enable WSL and install e2fsprogs inside it");
        return output.status.code() == Some(0);
    }
    #[allow(unreachable_code)]
    false
}

#[cfg(windows)]
fn windows_path_to_wsl(win_path: &str) -> String {
    let p = win_path.replace('\\', "/");
    if p.len() >= 2 && p.as_bytes()[1] == b':' {
        let drive = p[..1].to_lowercase();
        let rest = &p[2..];
        format!("/mnt/{drive}{rest}")
    } else {
        p
    }
}

/// Replay the journal on the image and verify no hard error is left behind.
fn remount_and_check(path: &Path) {
    let dev = ImageFileDevice::open_rw(path).expect("open for recovery");
    let sb = superblock::parse(&dev).expect("parse superblock");
    let gdt = GroupDescTable::load(&dev, &sb).expect("load gdt");
    check_and_recover(&dev, &sb, &gdt).expect("check_and_recover");
    // Verify filesystem is not permanently errored after replay.
    let sb2 = superblock::parse(&dev).expect("re-parse superblock");
    assert!(!sb2.state_has_error(), "ERROR_FS set after journal replay");
    assert!(fsck(path), "e2fsck failed after journal replay");
}

/// Attempt to create a file, ignoring I/O errors (expected from fault injection).
fn try_create_file(fs: &mut Ext4FsRw<FaultInjectDevice>, name: &str) {
    let _ = (|| -> Result<(), Box<dyn std::error::Error>> {
        let parent_inode_num = 2u32;
        let mut txn = fs.journal.begin_transaction();

        let new_inum = {
            let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
            alloc.alloc_inode(&mut txn, false, 0)?
        };
        inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, new_inum, 0o100644, 0, 0)?;
        {
            let gdt_snap = fs.gdt.clone();
            let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
            dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
                parent_inode_num, name, new_inum, dir_file_type::REG_FILE)?;
        }
        fs.journal.commit(&fs.dev, txn)?;
        Ok(())
    })();
}

// ---------------------------------------------------------------------------
// Task 03 tests
// ---------------------------------------------------------------------------

/// Abort before the commit block is written.
/// The transaction is incomplete, so after remount the file must NOT exist.
#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn crash_before_commit_block() {
    let tmp = make_rw_image();

    // Allow only a few writes — not enough to reach the commit block.
    {
        let dev = FaultInjectDevice::new(
            ImageFileDevice::open_rw(tmp.path()).expect("open_rw"),
            4,
        );
        // open_rw itself writes the dirty flag (2 sectors); subsequent alloc/dir writes
        // will trip the fault before the commit block.
        if let Ok(mut fs) = Ext4FsRw::open_rw(dev) {
            try_create_file(&mut fs, "crash_pre_commit.txt");
            // unmount may also fail — that's fine; we care about on-disk state.
            let _ = fs.unmount();
        }
    }

    remount_and_check(tmp.path());

    // The file should NOT be present because the commit block was never written.
    let dev = ImageFileDevice::open(tmp.path()).expect("open ro");
    let sb = superblock::parse(&dev).expect("parse sb");
    let gdt = GroupDescTable::load(&dev, &sb).expect("load gdt");
    let root = ext4_core::inode::read_inode(&dev, &sb, &gdt, 2).expect("root inode");
    let found = ext4_core::dir::lookup(&dev, &sb, &gdt, &root, "crash_pre_commit.txt")
        .expect("lookup");
    assert!(found.is_none(), "file should not exist: transaction never committed");
}

/// Abort after the commit block is written but before checkpoint.
/// After remount + replay the file MUST exist.
#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn crash_after_commit_block() {
    let tmp = make_rw_image();

    // Allow enough writes to complete the commit, then abort during checkpoint.
    {
        let dev = FaultInjectDevice::new(
            ImageFileDevice::open_rw(tmp.path()).expect("open_rw"),
            u32::MAX, // let everything through for the create
        );
        let mut fs = Ext4FsRw::open_rw(dev).expect("mount");

        // Do the create (commit completes successfully).
        try_create_file(&mut fs, "crash_post_commit.txt");

        // Now re-arm the fault so that subsequent writes (checkpoint) will fail.
        fs.dev.fault_on_write.store(0, Ordering::SeqCst);
        fs.dev.write_count.store(0, Ordering::SeqCst);

        // unmount triggers checkpoint — will fail.
        let _ = fs.unmount();
    }

    // Journal replay must recover the committed create.
    remount_and_check(tmp.path());

    let dev_ro = ImageFileDevice::open(tmp.path()).expect("open ro");
    let sb = superblock::parse(&dev_ro).expect("parse sb");
    let gdt = GroupDescTable::load(&dev_ro, &sb).expect("load gdt");
    let root = ext4_core::inode::read_inode(&dev_ro, &sb, &gdt, 2).expect("root inode");
    let found = ext4_core::dir::lookup(&dev_ro, &sb, &gdt, &root, "crash_post_commit.txt")
        .expect("lookup");
    assert!(found.is_some(), "file must exist after journal replay of committed transaction");
}

/// Abort during checkpoint (writes partially flushed).
/// Replay must leave the filesystem in a consistent, non-errored state.
#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn crash_during_checkpoint() {
    let tmp = make_rw_image();

    {
        let dev = FaultInjectDevice::new(
            ImageFileDevice::open_rw(tmp.path()).expect("open_rw"),
            u32::MAX,
        );
        let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
        try_create_file(&mut fs, "crash_checkpoint.txt");

        // Fault mid-checkpoint (allow 1 write then abort).
        fs.dev.fault_on_write.store(1, Ordering::SeqCst);
        fs.dev.write_count.store(0, Ordering::SeqCst);
        let _ = fs.unmount();
    }

    remount_and_check(tmp.path());
}

/// Verify that remounting with the dirty flag still set (no clean unmount) triggers
/// replay and leaves the filesystem consistent.
#[test]
#[cfg_attr(not(feature = "fixtures"), ignore)]
fn crash_before_dirty_flag_cleared() {
    let tmp = make_rw_image();

    // Mount and write a file, but abort before the dirty-flag is cleared.
    {
        let dev = FaultInjectDevice::new(
            ImageFileDevice::open_rw(tmp.path()).expect("open_rw"),
            u32::MAX,
        );
        let mut fs = Ext4FsRw::open_rw(dev).expect("mount");
        try_create_file(&mut fs, "dirty_flag_crash.txt");

        // Block the very last writes (clear_dirty_flag writes 2 sectors + flush).
        // The journal commit itself already completed above, so the file is safe.
        fs.dev.fault_on_write.store(0, Ordering::SeqCst);
        fs.dev.write_count.store(0, Ordering::SeqCst);
        let _ = fs.unmount();
    }

    // Superblock still has dirty flag (VALID_FS cleared). remount must replay + recover.
    remount_and_check(tmp.path());

    // File must be present: the crash happened after the commit, before dirty-flag clear.
    let dev = ImageFileDevice::open(tmp.path()).expect("open ro");
    let sb = superblock::parse(&dev).expect("parse sb");
    let gdt = GroupDescTable::load(&dev, &sb).expect("load gdt");
    let root = ext4_core::inode::read_inode(&dev, &sb, &gdt, 2).expect("root inode");
    let found = ext4_core::dir::lookup(&dev, &sb, &gdt, &root, "dirty_flag_crash.txt")
        .expect("lookup");
    assert!(found.is_some(), "committed file must survive crash before dirty-flag clear");
}
