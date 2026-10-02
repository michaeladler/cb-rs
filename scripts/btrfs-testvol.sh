#!/usr/bin/env bash
# Create and mount the loopback btrfs volume the reflink tests need.
# Idempotent: an already-mounted ./mount is left alone. The mount does not
# survive a reboot, so re-run this before `cargo test --test reflink`.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
image="$root/btrfs_disk.img"
mountpoint="$root/mount"

if findmnt -n -T "$mountpoint" -o FSTYPE | grep -q btrfs; then
    echo "btrfs already mounted at $mountpoint"
    exit 0
fi

if [ ! -f "$image" ]; then
    truncate -s 10G "$image"
    mkfs.btrfs -q "$image"
fi

mkdir -p "$mountpoint"
sudo mount -o loop,compress=zstd "$image" "$mountpoint"
echo "mounted $image at $mountpoint"
