#!/usr/bin/env bash
# Run once on Linux/WSL to generate test fixture images.
# Output: ext4-core/tests/fixtures/minimal.img and ext4_1k.img
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="$SCRIPT_DIR/../tests/fixtures"
MOUNT=/tmp/ext4-pengwin-mount

mkdir -p "$FIXTURES"

# Minimal ext4 image: 16MB, 4K blocks
dd if=/dev/zero of="$FIXTURES/minimal.img" bs=1M count=16
mkfs.ext4 -F -b 4096 -L "test-vol" "$FIXTURES/minimal.img"

mkdir -p "$MOUNT"
sudo mount -o loop "$FIXTURES/minimal.img" "$MOUNT"

echo "hello world"  | sudo tee "$MOUNT/hello.txt"        > /dev/null
sudo mkdir -p "$MOUNT/subdir"
echo "nested file"  | sudo tee "$MOUNT/subdir/nested.txt" > /dev/null

# Sparse file: 1MB real data then truncate to 4MB (creates a 3MB hole)
sudo dd if=/dev/zero of="$MOUNT/sparse.bin" bs=1M count=1
sudo truncate -s 4M "$MOUNT/sparse.bin"

# Large sparse file: 8GB, takes no real disk space — tests 64-bit offset arithmetic
sudo truncate -s 8G "$MOUNT/large_sparse.bin"

# Fast symlink (target < 60 bytes → stored inline in inode)
sudo ln -s /hello.txt "$MOUNT/link.txt"

sudo umount "$MOUNT"

# 1K block image for compatibility testing
dd if=/dev/zero of="$FIXTURES/ext4_1k.img" bs=1M count=8
mkfs.ext4 -F -b 1024 -L "1k-blocks" "$FIXTURES/ext4_1k.img"

# Record checksums for CI verification
sha256sum "$FIXTURES/minimal.img" "$FIXTURES/ext4_1k.img" > "$FIXTURES/checksums.sha256"

echo "Fixtures written to $FIXTURES"
echo "Run: cargo test --features fixtures -- --include-ignored"
