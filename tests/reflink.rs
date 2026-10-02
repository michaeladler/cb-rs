//! The reflink rung needs a CoW filesystem to be observable at all. Set
//! `CB_BTRFS_DIR` to a btrfs (or XFS with `reflink=1`) directory; the tests skip
//! without one so the rest of the suite still runs on a plain filesystem.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use cb_rs::copy::{Method, clone_file};
use cb_rs::walk;
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags, fstat, open};
use rustix::ioctl::{Opcode, Updater, opcode};

const FICLONE: Opcode = opcode::write::<i32>(0x94, 9);
/// `_IOWR('f', 11, struct fiemap)`: the size in the opcode is the 32-byte
/// header, not the whole buffer we pass.
const FIEMAP: Opcode = opcode::from_components(rustix::ioctl::Direction::ReadWrite, b'f', 11, 32);
const MAX_EXTENTS: usize = 8;

static PROBE: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Default)]
struct Fiemap {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
    fm_extents: [FiemapExtent; MAX_EXTENTS],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
}

/// Physical offset of the first extent, the thing a reflink shares and a byte
/// copy never can.
fn first_extent_physical(path: &Path) -> u64 {
    let file = open(path, OFlags::RDONLY, Mode::empty()).unwrap();
    let mut map = Fiemap {
        fm_length: u64::MAX,
        fm_extent_count: MAX_EXTENTS as u32,
        ..Default::default()
    };
    // SAFETY: the opcode is FS_IOC_FIEMAP and `map` is a correctly laid out
    // `struct fiemap` with room for the extents the kernel is told about.
    let result = unsafe { rustix::ioctl::ioctl(&file, Updater::<FIEMAP, _>::new(&mut map)) };
    assert!(matches!(result, Ok(())), "FIEMAP failed on {path:?}");
    assert!(map.fm_mapped_extents > 0, "no extents mapped for {path:?}");
    map.fm_extents[0].fe_physical
}

fn shared_with_source(src: &Path, dst: &Path) -> bool {
    first_extent_physical(src) == first_extent_physical(dst)
}

/// The CoW test volume, or `None` when this machine has none. The `CB_BTRFS_DIR`
/// override exists because the loop-mounted image lives outside any fixed path.
///
/// The probe asks the ladder whether it can reflink instead of checking
/// `f_type`: XFS only does FICLONE when formatted `mkfs.xfs -m reflink=1`, and a
/// magic-number allowlist would pass that volume here and then fail every
/// `Method::Reflink` assert instead of skipping.
fn cow_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("CB_BTRFS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mount"),
    };
    // Per-call names: the tests run in parallel and a shared probe path would be
    // truncated out from under a neighbour mid-clone.
    let tag = format!("{}-{}", std::process::id(), PROBE.fetch_add(1, Relaxed));
    let probe = dir.join(format!(".cb-cow-probe-{tag}"));
    let clone = dir.join(format!(".cb-cow-probe-{tag}-clone"));
    fs::write(&probe, b"probe").ok()?;
    let src_fd = open(&probe, OFlags::RDONLY, Mode::empty()).ok()?;
    let src_st = fstat(&src_fd).ok()?;
    let dst_fd = open(
        &clone,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
        Mode::RUSR | Mode::WUSR,
    )
    .ok()?;
    let verdict = clone_file(&src_fd, &dst_fd, &src_st).ok();
    drop(src_fd);
    drop(dst_fd);
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(&clone);
    (verdict == Some(Method::Reflink)).then_some(dir)
}

struct Case {
    dir: PathBuf,
}

impl Case {
    /// Skips the test body when no CoW volume is available.
    fn new(name: &str) -> Option<Self> {
        let dir = cow_dir()?.join(format!("cb-reflink-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Some(Self { dir })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    /// Copy through the ladder and report which rung paid off.
    fn clone(&self, src: &Path, dst_name: &str) -> Method {
        let src_fd = open(src, OFlags::RDONLY, Mode::empty()).unwrap();
        let src_st = fstat(&src_fd).unwrap();
        let dst_fd: OwnedFd = open(
            self.path(dst_name),
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        clone_file(&src_fd, &dst_fd, &src_st).unwrap()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

macro_rules! cow_case {
    ($name:ident, $case:ident) => {
        let Some($case) = Case::new(stringify!($name)) else {
            eprintln!(
                "skipping {}: no CoW volume (set CB_BTRFS_DIR)",
                stringify!($name)
            );
            return;
        };
    };
}

#[test]
fn reflink_rung_fires_on_a_cow_filesystem() {
    cow_case!(reflink_rung_fires_on_a_cow_filesystem, case);

    let data = vec![0x5au8; 1024 * 1024];
    let src = case.write("src.bin", &data);
    let dst = case.path("dst.bin");

    let method = case.clone(&src, "dst.bin");

    assert_eq!(
        method,
        Method::Reflink,
        "FICLONE must win on a btrfs volume, not fall through to a byte copy"
    );
    assert_eq!(fs::read(&dst).unwrap(), data);
    assert!(
        shared_with_source(&src, &dst),
        "extents must be physically shared, not merely equal in content"
    );
}

#[test]
fn reflink_of_an_empty_file_succeeds() {
    cow_case!(reflink_of_an_empty_file_succeeds, case);

    let src = case.write("empty.bin", b"");
    let method = case.clone(&src, "empty-copy.bin");

    assert_eq!(method, Method::Reflink);
    assert_eq!(fs::metadata(case.path("empty-copy.bin")).unwrap().len(), 0);
}

#[test]
fn writing_to_a_clone_leaves_the_original_untouched() {
    cow_case!(writing_to_a_clone_leaves_the_original_untouched, case);

    let original: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    let src = case.write("cow-src.bin", &original);
    let dst = case.path("cow-dst.bin");
    assert_eq!(case.clone(&src, "cow-dst.bin"), Method::Reflink);

    // Copy-on-write is only correct if the two files diverge on write.
    let mut with_marker = original.clone();
    with_marker[0] = b'X';
    fs::write(&dst, &with_marker).unwrap();
    assert_eq!(fs::read(&src).unwrap(), original);

    // And the source's extents survive the clone's write: the clone broke its
    // own CoW dependency, not the source's.
    assert!(
        !shared_with_source(&src, &dst),
        "rewriting the clone must detach it from the source extents"
    );
}

#[test]
fn a_reflinked_copy_keeps_the_source_readable_and_separate() {
    cow_case!(
        a_reflinked_copy_keeps_the_source_readable_and_separate,
        case
    );

    let src = case.write("inode-src.bin", &vec![1u8; 32 * 1024]);
    let dst = case.path("inode-dst.bin");
    assert_eq!(case.clone(&src, "inode-dst.bin"), Method::Reflink);

    assert_ne!(
        fs::metadata(&src).unwrap().ino(),
        fs::metadata(&dst).unwrap().ino(),
        "a reflink is a new inode, not a hardlink to the user's file"
    );
    assert_eq!(fs::read(&src).unwrap(), fs::read(&dst).unwrap());
}

/// The fallback chain has to stay honest: on a filesystem without reflinks the
/// ladder must still produce a correct copy by some other rung.
#[test]
fn a_filesystem_without_reflinks_falls_through_to_a_correct_copy() {
    let dir = std::env::temp_dir().join(format!("cb-nocow-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.bin");
    let dst = dir.join("dst.bin");
    let data = vec![0x11u8; 512 * 1024];
    fs::write(&src, &data).unwrap();

    let method = {
        let src_fd = open(&src, OFlags::RDONLY, Mode::empty()).unwrap();
        let src_st = fstat(&src_fd).unwrap();
        let dst_fd = open(
            &dst,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        clone_file(&src_fd, &dst_fd, &src_st).unwrap()
    };

    assert_ne!(method, Method::Reflink);
    assert_eq!(fs::read(&dst).unwrap(), data);
    let _ = fs::remove_dir_all(&dir);
}

/// The rung as the walker reaches it, not just as the helper: a reflink that
/// only works when called directly is not wired up.
#[test]
fn the_walker_copy_also_reflinks() {
    let Some(case) = Case::new("walker") else {
        eprintln!("skipping: no CoW volume (set CB_BTRFS_DIR)");
        return;
    };
    let data = vec![0x77u8; 256 * 1024];
    let src = case.write("walk-src.bin", &data);
    let dst = case.path("walk-dst.bin");

    let failures = walk::copy_any(&src, &dst);

    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::read(&dst).unwrap(), data);
    assert!(shared_with_source(&src, &dst));
}

/// A "this rung does not work here" answer is remembered per device *pair*, and
/// two ways of getting that wrong are invisible until they are not: a plain
/// flag lets a tmpfs file disable the rung for a btrfs destination in the same
/// process, and a destination-only key does the same to the btrfs destination
/// of a copy whose source sits on another mount.
#[test]
fn a_refused_reflink_does_not_leak_to_another_device() {
    let plain = std::env::temp_dir().join(format!("cb-devkey-{}", std::process::id()));
    fs::create_dir_all(&plain).unwrap();
    let data = vec![0x33u8; 64 * 1024];
    let src = plain.join("src.bin");
    fs::write(&src, &data).unwrap();

    let same_device_refusal = {
        let src_fd = open(&src, OFlags::RDONLY, Mode::empty()).unwrap();
        let src_st = fstat(&src_fd).unwrap();
        let dst_fd = open(
            &plain.join("first.bin"),
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        clone_file(&src_fd, &dst_fd, &src_st).unwrap()
    };
    assert_ne!(
        same_device_refusal,
        Method::Reflink,
        "the temp dir cannot reflink, and that has to be learned"
    );
    assert_eq!(fs::read(plain.join("first.bin")).unwrap(), data);

    let Some(case) = Case::new("devkey") else {
        eprintln!("skipping: no CoW volume (set CB_BTRFS_DIR)");
        let _ = fs::remove_dir_all(&plain);
        return;
    };
    // Source and destination both on the CoW volume, so `EXDEV` is not the
    // answer here and the rung must still be reached.
    let cow_src = case.write("cow-src.bin", &data);
    assert_eq!(
        case.clone(&cow_src, "second.bin"),
        Method::Reflink,
        "a rung refused for one device pair must still be tried for another"
    );
    let _ = fs::remove_dir_all(&plain);
}

/// Guards the opcode the rung depends on. A wrong value here compiles, runs,
/// and silently reports `EOPNOTSUPP` on every filesystem.
#[test]
fn ficlone_opcode_matches_the_kernel_constant() {
    assert_eq!(FICLONE, 0x4004_9409, "_IOW(0x94, 9, int)");
}
