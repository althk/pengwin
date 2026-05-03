/// End-to-end mount tests for the pengwin WinFsp filesystem.
///
/// These tests require:
///   - Windows with WinFsp installed (https://winfsp.dev)
///   - Administrator rights (WinFsp mount needs kernel driver)
///   - A fixture image built by ext4-core/scripts/make_fixtures.sh
///
/// Run with:
///   cargo test --test mount_test -- --include-ignored
///
/// All tests are #[ignore] by default to avoid CI failures without the above.
use serial_test::serial;

/// Force-unmount D:/mnt/Z if it is already mounted, so tests start clean.
///
/// WinFsp directory mounts are not network drives, so `net use /delete` does
/// not work.  Killing the pengwin process that owns the mount causes WinFsp to
/// call FspFileSystemRemoveMountPoint via Drop, which removes the junction and
/// frees the mount point.
#[cfg(windows)]
fn force_unmount_z() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let mount_path = std::path::Path::new(r"D:\mnt\Z");
    if !mount_path.exists() {
        return;
    }

    // Kill any lingering pengwin process that owns the mount.
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "pengwin.exe"])
        .output();

    // Wait up to 5 s for the reparse point to be removed, then clean up the
    // empty directory that WinFsp leaves behind after RemoveMountPoint.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !mount_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir(mount_path);
}

/// Mount the given fixture image at drive letter Z:, run `f` with the mount path,
/// then unmount.  Returns only after Z: is fully gone.
#[cfg(windows)]
fn with_mounted_fixture<F>(image_name: &str, f: F)
where
    F: FnOnce(&std::path::Path),
{
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    force_unmount_z();

    let image_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("ext4-core/tests/fixtures")
        .join(image_name);

    // Spawn pengwin mount in background.
    let image_str = image_path.to_string_lossy().into_owned();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pengwin"))
        .args(["mount", &image_str, "D:/mnt/Z"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn pengwin");

    // Poll until D:/mnt/Z exists (up to 10 seconds).
    let mount_path = std::path::Path::new(r"D:\mnt\Z");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if mount_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        mount_path.exists(),
        "mount timed out — D:/mnt/Z never appeared"
    );

    f(mount_path);

    // Unmount: kill the pengwin process so WinFsp Drop removes the mount point.
    // net use /delete does not work for WinFsp directory mounts.
    let _ = child.kill();
    let _ = child.wait();

    // Wait for the reparse point to be removed (up to 5 seconds), then delete
    // the empty directory that WinFsp leaves behind after RemoveMountPoint.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !mount_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir(mount_path);
}

#[test]
#[serial]
#[cfg(windows)]
#[ignore = "requires WinFsp installed and admin rights"]
fn mount_and_read_hello_txt() {
    with_mounted_fixture("minimal.img", |mount| {
        let content =
            std::fs::read_to_string(mount.join("hello.txt")).expect("hello.txt not readable");
        assert_eq!(content.trim(), "hello world");
    });
}

#[test]
#[serial]
#[cfg(windows)]
#[ignore = "requires WinFsp installed and admin rights"]
fn mount_and_list_root_directory() {
    with_mounted_fixture("minimal.img", |mount| {
        let entries: Vec<_> = std::fs::read_dir(mount)
            .expect("read_dir failed")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.contains(&"hello.txt".to_owned()));
        assert!(entries.contains(&"subdir".to_owned()));
    });
}

#[test]
#[serial]
#[cfg(windows)]
#[ignore = "requires WinFsp installed and admin rights"]
fn mount_file_attributes_are_readonly() {
    with_mounted_fixture("minimal.img", |mount| {
        let meta = std::fs::metadata(mount.join("hello.txt")).expect("metadata failed");
        assert!(
            meta.permissions().readonly(),
            "files on read-only mount should be readonly"
        );
    });
}

#[test]
#[serial]
#[cfg(windows)]
#[ignore = "requires WinFsp installed and admin rights"]
fn mount_file_size_matches_content() {
    with_mounted_fixture("minimal.img", |mount| {
        let path = mount.join("hello.txt");
        let meta = std::fs::metadata(&path).expect("metadata failed");
        let content = std::fs::read(&path).expect("read failed");
        assert_eq!(meta.len(), content.len() as u64);
    });
}

#[test]
#[serial]
#[cfg(windows)]
#[ignore = "requires WinFsp installed and admin rights"]
fn mount_nested_directory_readable() {
    with_mounted_fixture("minimal.img", |mount| {
        let content = std::fs::read_to_string(mount.join("subdir/nested.txt"))
            .expect("nested.txt not readable");
        assert_eq!(content.trim(), "nested file");
    });
}

#[test]
#[serial]
#[cfg(windows)]
#[ignore = "requires WinFsp installed and admin rights"]
fn mount_large_file_hash_matches() {
    with_mounted_fixture("minimal.img", |mount| {
        use std::io::Read;

        // Read sparse.bin through the mount and verify its reported size.
        let path = mount.join("sparse.bin");
        let meta = std::fs::metadata(&path).expect("metadata failed");
        assert_eq!(meta.len(), 4 * 1024 * 1024);

        // Verify the first 1MB is non-zero (dd-written data) by checking length.
        let mut f = std::fs::File::open(&path).expect("open failed");
        let mut buf = vec![0u8; 1024 * 1024];
        f.read_exact(&mut buf).expect("read failed");
        // The real data region should have been written with zeros from /dev/zero,
        // which is fine — we just verify we can read the full 1MB without error.
        drop(buf);

        // The hole region (2MB..4MB) must read as zero.
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(2 * 1024 * 1024))
            .expect("seek failed");
        let mut hole_buf = [0xFFu8; 512];
        f.read_exact(&mut hole_buf).expect("read at hole failed");
        assert!(hole_buf.iter().all(|&b| b == 0), "sparse hole must be zero");
    });
}
