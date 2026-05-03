# 🐧 Pengwin

**Pengwin** is a high-performance, read-only ext4 filesystem driver for Windows. Built with Rust and powered by WinFsp, it allows you to mount Linux partitions (currently ext4 only) and disk images directly into Windows Explorer with ease.

No more copying files over the network or rebooting into Linux just to grab that one config file. Pengwin brings your Linux data to your Windows fingertips.

---

## 🚀 Overview

Pengwin consists of two main parts:

- **`ext4-core`**: A pure-Rust, dependency-light implementation of the ext4 filesystem parser.
- **`pengwin`**: The CLI tool that bridges ext4 with Windows using [WinFsp](https://winfsp.dev/), providing a seamless native drive experience.

### Key Features

- **Native Experience**: Mounts as a real drive letter (e.g., `Z:`) or a directory junction.
- **Blazing Fast**: Uses a custom LRU sector cache to overcome raw disk latency.
- **Safety First**: Mounts are strictly **read-only**, ensuring your Linux data remains untainted.
- **Raw Disk Support**: Direct access to physical partitions (e.g., `\\.\Harddisk0Partition5`).

---

## 🛠️ Prerequisites

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

### 2. Mount an Image File

```powershell
./target/release/pengwin mount C:\path\to\linux.img Z:
```

### 3. Mount a Physical Partition

Identify your partition (e.g., Partition 5 on Disk 0) and run as Administrator:

```powershell
./target/release/pengwin mount \\.\Harddisk0Partition5 Z:
```

### 4. Unmount

Simply press `Ctrl+C` in the terminal where Pengwin is running. It will cleanly unmount the drive and clean up any mount points.

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
