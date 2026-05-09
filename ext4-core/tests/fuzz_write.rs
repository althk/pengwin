/// Task 04 — Fuzz Write Operations
///
/// Random sequences of write operations verified by e2fsck (Unix) every 50 ops.
/// Run with: cargo test -p ext4-core --features fixtures -- --ignored fuzz_write
use std::path::Path;

use ext4_core::block_device::image_file::ImageFileDevice;
use ext4_core::journal::mount::Ext4FsRw;
use ext4_core::alloc::Allocator;
use ext4_core::alloc::inode_alloc;
use ext4_core::dir_write::{dir_add_entry, dir_remove_entry, dir_rename, dir_file_type};
use ext4_core::file_write::write_file_data;
use ext4_core::inode_write::{update_inode, InodeUpdate};

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

// ---------------------------------------------------------------------------
// Fuzz state: a list of inode numbers of files in root we can operate on
// ---------------------------------------------------------------------------

struct FuzzState {
    /// (inode_num, name, size_bytes)
    files: Vec<(u32, String, u64)>,
    /// (inode_num, name)
    dirs: Vec<(u32, String)>,
    counter: u32,
}

impl FuzzState {
    fn new() -> Self {
        FuzzState { files: Vec::new(), dirs: Vec::new(), counter: 0 }
    }

    fn next_name(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}_{}", self.counter)
    }
}

fn create_random_file(fs: &mut Ext4FsRw<ImageFileDevice>, state: &mut FuzzState) {
    let name = state.next_name("f");
    let mut txn = fs.journal.begin_transaction();
    let new_inum = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        match alloc.alloc_inode(&mut txn, false, 0) {
            Ok(n) => n,
            Err(_) => return,
        }
    };
    if inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, new_inum, 0o100644, 0, 0)
        .is_err()
    {
        return;
    }
    let ok = {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            2, &name, new_inum, dir_file_type::REG_FILE)
            .is_ok()
    };
    if ok {
        if fs.journal.commit(&fs.dev, txn).is_ok() {
            state.files.push((new_inum, name, 0));
        }
    }
}

fn write_random_data(
    fs: &mut Ext4FsRw<ImageFileDevice>,
    state: &mut FuzzState,
    rng: &mut impl FuzzRng,
) {
    if state.files.is_empty() { return; }
    let idx = rng.next_u32() as usize % state.files.len();
    let (inum, _, size) = &mut state.files[idx];
    let inum = *inum;

    let data_len = (rng.next_u32() as usize % 4096) + 1;
    let data = vec![0x42u8; data_len];
    let inode = match ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, inum) {
        Ok(i) => i,
        Err(_) => return,
    };
    let mut txn = fs.journal.begin_transaction();
    let ok = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        write_file_data(&fs.dev, &fs.sb, &mut alloc, &mut txn, inum, &inode, 0, &data).is_ok()
    };
    if ok {
        let _ = update_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, inum,
            InodeUpdate::default().with_size(data.len() as u64));
        if fs.journal.commit(&fs.dev, txn).is_ok() {
            *size = data.len() as u64;
        }
    }
}

fn delete_random_file(fs: &mut Ext4FsRw<ImageFileDevice>, state: &mut FuzzState, rng: &mut impl FuzzRng) {
    if state.files.is_empty() { return; }
    let idx = rng.next_u32() as usize % state.files.len();
    let (inum, name, _) = state.files[idx].clone();

    let mut txn = fs.journal.begin_transaction();
    if dir_remove_entry(&fs.dev, &fs.sb, &fs.gdt, &mut txn, 2, &name).is_ok() {
        let inode = ext4_core::inode::read_inode(&fs.dev, &fs.sb, &fs.gdt, inum).ok();
        if let Some(inode) = inode {
            let new_links = inode.links_count.saturating_sub(1);
            let _ = update_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, inum,
                InodeUpdate::default().with_links_count(new_links));
            if new_links == 0 {
                let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
                let _ = alloc.free_inode(&mut txn, inum);
            }
        }
        if fs.journal.commit(&fs.dev, txn).is_ok() {
            state.files.remove(idx);
        }
    }
}

fn rename_random_file(fs: &mut Ext4FsRw<ImageFileDevice>, state: &mut FuzzState, rng: &mut impl FuzzRng) {
    if state.files.is_empty() { return; }
    let idx = rng.next_u32() as usize % state.files.len();
    let (inum, old_name, size) = state.files[idx].clone();
    let new_name = state.next_name("r");

    let mut txn = fs.journal.begin_transaction();
    let gdt_snap = fs.gdt.clone();
    let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
    if dir_rename(
        &fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
        2, &old_name, 2, &new_name,
    ).is_ok() && fs.journal.commit(&fs.dev, txn).is_ok() {
        state.files[idx] = (inum, new_name, size);
    }
}

fn create_random_dir(fs: &mut Ext4FsRw<ImageFileDevice>, state: &mut FuzzState) {
    let name = state.next_name("d");
    let mut txn = fs.journal.begin_transaction();
    let new_inum = {
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        match alloc.alloc_inode(&mut txn, true, 0) {
            Ok(n) => n,
            Err(_) => return,
        }
    };
    if inode_alloc::init_inode(&fs.dev, &fs.sb, &fs.gdt, &mut txn, new_inum, 0o040755, 0, 0)
        .is_err()
    {
        return;
    }
    let mut ok = true;
    for (dir_inum, entry_name) in [(2u32, name.as_str()), (new_inum, ".")] {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        if dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            dir_inum, entry_name, new_inum, dir_file_type::DIR).is_err()
        {
            ok = false;
            break;
        }
    }
    if ok {
        let gdt_snap = fs.gdt.clone();
        let mut alloc = Allocator::new(&fs.dev, &fs.sb, &mut fs.gdt);
        if dir_add_entry(&fs.dev, &fs.sb, &gdt_snap, &mut alloc, &mut txn,
            new_inum, "..", 2, dir_file_type::DIR).is_err()
        {
            ok = false;
        }
    }
    if ok && fs.journal.commit(&fs.dev, txn).is_ok() {
        state.dirs.push((new_inum, name));
    }
}

// ---------------------------------------------------------------------------
// Minimal deterministic RNG (xoshiro32-like, avoids pulling in rand crate)
// ---------------------------------------------------------------------------

trait FuzzRng {
    fn next_u32(&mut self) -> u32;
}

struct SimpleRng(u64);

impl SimpleRng {
    fn seed(seed: u64) -> Self { SimpleRng(seed) }
}

impl FuzzRng for SimpleRng {
    fn next_u32(&mut self) -> u32 {
        // xorshift64 adapted to u32 output
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as u32
    }
}

// ---------------------------------------------------------------------------
// Fuzz test
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow — run manually: cargo test -p ext4-core --features fixtures -- --ignored fuzz_write_operations"]
fn fuzz_write_operations() {
    let tmp = make_rw_image();
    let mut rng = SimpleRng::seed(42);
    let mut state = FuzzState::new();

    for _i in 0..1000u32 {
        let dev = ImageFileDevice::open_rw(tmp.path()).expect("open_rw in fuzz loop");
        let mut fs = Ext4FsRw::open_rw(dev).expect("mount in fuzz loop");

        let op = rng.next_u32() % 5;
        match op {
            0 => create_random_file(&mut fs, &mut state),
            1 => write_random_data(&mut fs, &mut state, &mut rng),
            2 => delete_random_file(&mut fs, &mut state, &mut rng),
            3 => rename_random_file(&mut fs, &mut state, &mut rng),
            4 => create_random_dir(&mut fs, &mut state),
            _ => unreachable!(),
        }

        fs.unmount().expect("unmount in fuzz loop");

        if _i % 50 == 0 {
            assert!(fsck(tmp.path()), "fsck failed after op {_i}");
        }
    }

    assert!(fsck(tmp.path()), "fsck failed at end of fuzz run");
}
