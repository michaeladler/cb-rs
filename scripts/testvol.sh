#!/usr/bin/env bash
# Create and mount the loopback volume the filesystem tests need.
#
#   scripts/testvol.sh <btrfs|xfs|ext4|zfs> [mountpoint]
#
# Idempotent: an already-mounted volume of the same type is left alone. The
# mount does not survive a reboot, so re-run this before the tests that want it.
# Every volume lands on mount-<fs>, which is also where tests/reflink.rs and
# scripts/bench.py look when CB_TESTVOL_DIR is unset.
#
# The volume is not only for btrfs: XFS with reflink=1 is the other CoW
# filesystem the copy ladder claims to use, and it reaches FICLONE through a
# different kernel path, ZFS reaches it through a third one, and a plain ext4
# volume is a second device to move across. Pass the mountpoint as
# CB_TESTVOL_DIR to the tests.
set -euo pipefail

fs=${1:-}
if [ -z "$fs" ]; then
    echo "usage: $(basename "$0") <btrfs|xfs|ext4|zfs> [mountpoint]" >&2
    exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
image="$root/${fs}_disk.img"

case $fs in
    btrfs | xfs | ext4 | zfs) ;;
    *)
        echo "unknown filesystem: $fs" >&2
        exit 2
        ;;
esac

if [ $# -ge 2 ]; then
    mountpoint=$2
else
    mountpoint="$root/mount-$fs"
fi

mkfs_volume() {
    case $fs in
        btrfs) mkfs.btrfs -q "$image" ;;
        # reflink=1 is the point: without it mkfs.xfs makes a filesystem that
        # answers EOPNOTSUPP and every reflink test would skip.
        xfs) mkfs.xfs -q -m reflink=1 "$image" ;;
        ext4) mkfs.ext4 -q -F "$image" ;;
        # zpool create makes the filesystem and the labels in one step, so there
        # is nothing to format here.
        zfs) : ;;
    esac
}

if findmnt -n -T "$mountpoint" -o FSTYPE | grep -qx "$fs"; then
    echo "$fs already mounted at $mountpoint"
    exit 0
fi

if [ ! -f "$image" ]; then
    truncate -s 10G "$image"
    mkfs_volume
fi

mkdir -p "$mountpoint"
if [ "$fs" = zfs ]; then
    # ashift=12 is the 4 KiB sector size the loop file pretends to have; the
    # default for a file vdev is 512-byte sectors and a 512-byte recordsize.
    # mountpoint= overrides the default /<pool> so the same $mountpoint path
    # works for every filesystem. -f replaces a pool left over from an earlier
    # run, which is what makes the already-mounted check above the only guard
    # needed.
    # mountpoint is a dataset property, so it needs -O (root dataset), not -o
    # (pool/vdev). ashift is a vdev property and stays on -o.
    sudo zpool create -f -o ashift=12 -O mountpoint="$mountpoint" cbtest "$image"
else
    sudo mount -o loop "$image" "$mountpoint"
fi

# mkfs leaves the root inode owned by root. Left that way, every probe in the
# tests fails with EACCES for the unprivileged user and the suite reports a
# clean pass having skipped the parts that matter.
sudo chown "$(id -u):$(id -g)" "$mountpoint"
probe="$mountpoint/.cb-write-probe-$$"
if ! : >"$probe"; then
    echo "mounted $image at $mountpoint but $mountpoint is not writable by $(id -un)" >&2
    exit 1
fi
rm -f "$probe"

echo "mounted $image at $mountpoint"
