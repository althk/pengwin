# 🐧 Pengwin

**Pengwin** is a fast ext4 filesystem driver for Windows. Built with Rust and powered by WinFsp, it mounts Linux partitions and disk images straight into Windows Explorer.

No more copying files over the network or rebooting into Linux to grab that one config file. Read-only by default — because the worst thing a filesystem driver can do is be too eager to help. Opt into writes with `--rw` when you mean it.

---

## 🚀 Overview

Pengwin consists of two main parts:

- **`ext4-core`**: A pure-Rust, dependency-light implementation of the ext4 filesystem parser.
- **`pengwin`**: The CLI tool that bridges ext4 with Windows using [WinFsp](https://winfsp.dev/), providing a seamless native drive experience.

### Key Features

- **Native Experience**: Mounts as a real drive letter (e.g., `Z:`) or a directory junction.
- **Blazing Fast**: Uses a custom LRU sector cache to overcome raw disk latency.
- **Safe by Default**: Mounts read-only unless you pass `--rw`. The OS can't even *try* to write to the underlying image in RO mode.
- **Write Support**: Full RW mounts with journal replay, allocator, and cleanup-on-unmount — for when you actually want to change something.
- **Raw Disk Support**: Direct access to physical partitions (e.g., `\\.\Harddisk0Partition5`).

---

## 📦 Installation (Pre-built Binary)

No Rust required — just two steps:

1. **Install [WinFsp](https://winfsp.dev/)** — download and run the installer (Core component is sufficient).
2. **Download `pengwin.exe`** from the [latest release](../../releases/latest) and place it anywhere on your `PATH`.

Then run as Administrator — see the usage examples below.

> **Note:** Administrator rights are required to access physical disk partitions. Mounting image files works without elevation.

---

## 🛠️ Prerequisites (Building from Source)

Before you start flying with Pengwin, make sure you have:

1. **Windows 10/11** (64-bit).
2. **[WinFsp](https://winfsp.dev/)**: The Windows File System Proxy (Core and Developer components).
3. **Rust**: The [Rust toolchain](https://rustup.rs/) (stable channel).
4. **Administrator Rights**: Required for mounting physical disks and creating directory junctions.
5. **WSL 2** *(for contributors)*: Needed to generate integration test fixtures (the repo has all the required ones pre-generated, but this is recommended). Any distro works — Ubuntu is fine.

---

## 🏁 How to Run Locally

### 1. Clone and Build

```powershell
git clone https://github.com/your-repo/pengwin
cd pengwin
cargo build --release
```

### 2. Mount an Image File (read-only)

```powershell
./target/release/pengwin mount C:\path\to\linux.img Z:
```

### 3. Mount Read-Write

```powershell
./target/release/pengwin mount --rw C:\path\to\linux.img Z:
```

`--rw` replays the journal if needed and lets you create, edit, delete, and rename files. Without it, every write call returns `STATUS_MEDIA_WRITE_PROTECTED` — Explorer will tell you the disk is write-protected, which is exactly the point.

### 4. Mount a Physical Partition

Identify your partition (e.g., Partition 5 on Disk 0) and run as Administrator:

```powershell
./target/release/pengwin mount \\.\Harddisk0Partition5 Z:
```

### 5. Dirty Journal? `--force`

If the filesystem wasn't cleanly unmounted, RO mode refuses to mount (replay would require writing). Three ways out:

- `--rw` — replay the journal and mount RW.
- Run `e2fsck` from Linux/WSL — fix it properly.
- `--force` — RO mount without replay. Uncommitted journal data won't be visible, but your physical drive stays untouched. Useful when e2fsck would itself be a write.

### 6. Unmount

Press `Ctrl+C` in the terminal where Pengwin is running. It cleanly unmounts the drive, flushes the journal (if RW), and removes any mount points.

---

## 🐞 Debugging with `--verbose`

When something looks wrong — a file won't open, a directory shows up empty, writes return errors — re-run with `-v` (or `--verbose`):

```powershell
./target/release/pengwin mount -v C:\path\to\linux.img Z:
./target/release/pengwin mount --rw -v \\.\Harddisk0Partition5 Z:
```

This switches log filtering from `pengwin=info` to `pengwin=debug,ext4_core=debug`, which gets you:

- Every WinFsp dispatch call (`create`, `read`, `write`, `set_delete`, `rename`, …) with arguments and return status.
- Inode numbers, offsets, and block allocations on the write path.
- Journal transaction begin/commit boundaries.
- Path resolution and symlink expansion in the read path.

Logs go to **stderr**, so you can capture them without polluting the mount status line:

```powershell
./target/release/pengwin mount -v C:\path\to\linux.img Z: 2> pengwin.log
```

Tip: `--verbose` is verbose. Reproduce the smallest failing operation, then grep the log — `pengwin::dispatch` shows the FSP boundary, `pengwin::write` and `pengwin::delete` show the write paths, and `ext4_core::*` shows what the parser is doing underneath.

---

## 🧪 Development Setup

### Running Unit Tests

Unit tests run entirely on Windows with no extra setup:

```powershell
cargo test
```

### Integration Tests (requires WSL)

Integration tests validate the parser against real ext4 images. The fixture images are **not** checked in — you generate them once using WSL.

**Step 1 — generate the fixtures (run once):**

```bash
# From a WSL terminal, in the repo root:
bash ext4-core/scripts/make_fixtures.sh
```

This script creates two images under `ext4-core/tests/fixtures/`:

| Image | Size | Description |
| --- | --- | --- |
| `minimal.img` | 16 MB | 4K-block ext4 with files, a symlink, sparse data, and an 8 GB sparse hole |
| `ext4_1k.img` | 8 MB | 1K-block ext4 for block-size compatibility testing |

It also writes `checksums.sha256` so CI can verify the images are intact.

**Step 2 — run the integration tests:**

```powershell
cargo test --features fixtures -- --include-ignored
```

> **Why WSL?** The script uses `mkfs.ext4`, `mount`, and Linux-specific `truncate` semantics — none of which are available natively on Windows. WSL gives you a real Linux kernel right next to your Windows checkout.

---

## 🤖 AI Note

This project was built through a collaborative pair-programming journey with **Claude** and **Gemini**. It's a testament to what humans and AI can build together when they share a love for filesystems and rusty things.

Rust turns out to be an excellent fit for AI-assisted systems programming. The compiler enforces memory safety and eliminates whole classes of bugs — use-after-free, buffer overflows, data races — at compile time, before any code ever runs. That means AI-generated code that *compiles* is already far less likely to harbor the subtle memory errors that plague equivalent C/C++ code. The borrow checker is, in effect, a second reviewer that never sleeps.

---

## ⚖️ License

Distributed under the MIT License. See `LICENSE` for more information.
