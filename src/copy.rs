#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[cfg(target_os = "linux")]
use rustix::fd::AsFd;
use rustix::fd::OwnedFd;
#[cfg(target_os = "linux")]
use rustix::fs::copy_file_range;
use rustix::fs::{Mode, Stat, fchmod, fsync};
#[cfg(target_os = "linux")]
use rustix::io::Errno;
use rustix::io::Result;
#[cfg(target_os = "linux")]
use rustix::ioctl::{IntegerSetter, ioctl, opcode};

/// `_IOW(0x94, 9, int)`: clone the open file into the destination inode.
#[cfg(target_os = "linux")]
const FICLONE: rustix::ioctl::Opcode = opcode::write::<i32>(0x94, 9);

const CHUNK: usize = 1 << 20;
/// `copy_file_range` allocates no buffer, so this is only an upper bound on how
/// much one call may move; the kernel stops at EOF. Kept small so the progress
/// counter moves smoothly; the syscall cost per 8 MiB is negligible.
#[cfg(target_os = "linux")]
const CFR_MAX: u64 = 8 << 20;

/// Process-wide totals for the progress display. Monotonic; readers diff them.
pub static BYTES: AtomicU64 = AtomicU64::new(0);
pub static FILES: AtomicU64 = AtomicU64::new(0);

/// The `(source device, destination device)` pair a rung has refused, 0 while
/// undecided. Asking the whole ladder costs three syscalls per file and none of
/// them move a byte; 0 is not a device number, so it never matches a real pair.
#[cfg(target_os = "linux")]
static NO_REF_LINK: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "linux")]
static NO_CFR: AtomicU64 = AtomicU64::new(0);

/// Both devices go into the key, not just the destination: `FICLONE` answers
/// `EXDEV` for a pair on two mounts and `EOPNOTSUPP` for two files on one, and
/// a `paste` of a cut recorded on two filesystems would otherwise learn "no
/// reflink" from the first file and skip the rung for every file after it. A
/// hash collision only costs one wasted probe.
#[cfg(target_os = "linux")]
fn pair(src: u64, dst: u64) -> u64 {
    src.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ dst.rotate_left(32)
}

/// Copy a regular file's contents, trying the cheapest mechanism the
/// destination filesystem supports. Returns which rung paid off.
///
/// The two rungs below it are Linux syscalls; elsewhere only `copy_stream` runs.
/// `src_st` is unused there, and the fd stays unread until then.
pub fn clone_file(src: &OwnedFd, dst: &OwnedFd, src_st: &Stat) -> Result<Method> {
    let method = clone_any(src, dst, src_st)?;
    FILES.fetch_add(1, Relaxed);
    Ok(method)
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn clone_any(src: &OwnedFd, dst: &OwnedFd, src_st: &Stat) -> Result<Method> {
    #[cfg(target_os = "linux")]
    {
        let key = pair(src_st.st_dev, rustix::fs::fstat(dst)?.st_dev);

        if NO_REF_LINK.load(Relaxed) != key {
            match try_reflink(src, dst) {
                Ok(()) => {
                    BYTES.fetch_add(src_st.st_size.max(0) as u64, Relaxed);
                    return Ok(Method::Reflink);
                }
                Err(e) if unavailable(e) => NO_REF_LINK.store(key, Relaxed),
                // Transient: let the next rung have a go rather than give up.
                Err(_) => {}
            }
        }

        // `copy_file_range` is one syscall per 8 MiB instead of per 4 KiB, and on
        // kernels that support it across mounts stays entirely in the kernel.
        //
        // rustix only ships it on Linux and `std::fs::copy_file_range` is still
        // unstable, so elsewhere the rung is skipped and `copy_stream` pays. macOS
        // has its own `clonefile`; add it as a rung before anyone needs the speed.
        //
        // `st_size` is a hint for the progress display, never the stopping point:
        // `/proc` and `/sys` report 0 and still have content, and a file that
        // grows while it is read must not be cut short at the size the stat saw.
        // So copy until the kernel says EOF, which is also where `cp` stops.
        let mut copied = false;
        while NO_CFR.load(Relaxed) != key {
            match copy_file_range(src, None, dst, None, CFR_MAX as usize) {
                Ok(0) if copied => return Ok(Method::CopyFileRange),
                Ok(0) => break,
                Ok(n) => {
                    copied = true;
                    BYTES.fetch_add(n as u64, Relaxed);
                }
                Err(e) if unavailable(e) => {
                    NO_CFR.store(key, Relaxed);
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }

    copy_stream(src, dst)?;
    Ok(Method::Stream)
}

/// The destination says this mechanism does not exist here, as opposed to
/// failing for a reason that might not repeat. `EBADF` and `EFAULT` land here
/// too: they mean the argument was read as a pointer and was not one, which is
/// a shape problem rather than a missing feature.
#[cfg(target_os = "linux")]
fn unavailable(e: Errno) -> bool {
    matches!(
        e,
        Errno::XDEV
            | Errno::NOSYS
            | Errno::OPNOTSUPP
            | Errno::NOTTY
            | Errno::INVAL
            | Errno::BADF
            | Errno::FAULT
    )
}

/// Copy-on-write clone. Instant and writes zero bytes, but the destination must
/// be empty and the filesystem must support reflinks (btrfs, XFS, and a few
/// others). Everything else fails and falls through.
///
/// The ioctl is issued on the destination and its argument is the source fd
/// number, which is what `fs/ioctl.c`'s `ioctl_ficlone` reads and what btrfs's
/// `btrfs_ioctl_ficlone` hands to `fget`. `IntegerSetter` puts that number into
/// the `int` the `_IOW` encoding reserves room for, so one shape is right and
/// there is nothing to retry.
#[cfg(target_os = "linux")]
fn try_reflink(src: &OwnedFd, dst: &OwnedFd) -> Result<()> {
    let source_fd = src.as_fd().as_raw_fd();
    // SAFETY: `source_fd` is a live descriptor owned by the caller and stays
    // open for the duration of the call.
    unsafe { ioctl(dst, IntegerSetter::<FICLONE>::new_usize(source_fd as usize)) }.map(drop)
}

fn copy_stream(src: &OwnedFd, dst: &OwnedFd) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut filled = 0;
    loop {
        if filled == buf.len() {
            write_all(dst, &buf)?;
            filled = 0;
        }
        let n = rustix::io::read(src, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    if filled > 0 {
        write_all(dst, &buf[..filled])?;
    }
    Ok(())
}

fn write_all(dst: &OwnedFd, buf: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < buf.len() {
        let n = rustix::io::write(dst, &buf[written..])?;
        written += n;
        BYTES.fetch_add(n as u64, Relaxed);
    }
    Ok(())
}

/// Flush to disk before the caller unlinks the source, so a crash cannot leave
/// neither file.
pub fn commit<Fd: rustix::fd::AsFd>(dst: &Fd) -> Result<()> {
    fsync(dst)
}

/// Preserve permission bits. Symlinks and special files carry their own
/// handling in the walker.
pub fn preserve_mode(fd: &OwnedFd, mode: Mode) -> Result<()> {
    fchmod(fd, mode)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Reflink,
    CopyFileRange,
    Stream,
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            Self(std::env::temp_dir().join(format!("cb-copy-{name}-{}", std::process::id())))
        }

        fn open(&self) -> OwnedFd {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&self.0)
                .unwrap()
                .into()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn stream_roundtrip(name: &str, len: usize) {
        let (src_tmp, dst_tmp) = (
            Tmp::new(&format!("{name}-src")),
            Tmp::new(&format!("{name}-dst")),
        );
        let data = pattern(len);
        fs::write(&src_tmp.0, &data).unwrap();
        let src: OwnedFd = File::open(&src_tmp.0).unwrap().into();
        let dst = dst_tmp.open();

        copy_stream(&src, &dst).unwrap();

        assert_eq!(fs::read(&dst_tmp.0).unwrap(), data);
    }

    #[test]
    fn copy_stream_empty() {
        stream_roundtrip("empty", 0);
    }

    #[test]
    fn copy_stream_smaller_than_chunk() {
        stream_roundtrip("small", 1000);
    }

    #[test]
    fn copy_stream_exactly_one_chunk() {
        stream_roundtrip("one-chunk", CHUNK);
    }

    #[test]
    fn copy_stream_one_byte_over_chunk() {
        stream_roundtrip("chunk-plus-one", CHUNK + 1);
    }

    #[test]
    fn copy_stream_exact_multiple_of_chunk() {
        stream_roundtrip("two-chunks", 2 * CHUNK);
    }

    #[test]
    fn copy_stream_many_chunks_with_tail() {
        stream_roundtrip("many", 3 * CHUNK + 17);
    }

    #[test]
    fn copy_stream_read_error_propagates() {
        let tmp = Tmp::new("read-err");
        let dst = tmp.open();
        let write_only: OwnedFd = OpenOptions::new().write(true).open(&tmp.0).unwrap().into();
        assert!(copy_stream(&write_only, &dst).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_copy_falls_back_when_copy_file_range_returns_zero() {
        let (src_path, dst_tmp) = ("/proc/self/cmdline", Tmp::new("procfs"));
        let expected = fs::read(src_path).unwrap();
        let src: OwnedFd = File::open(src_path).unwrap().into();
        let st = rustix::fs::fstat(&src).unwrap();

        clone_file(&src, &dst_tmp.open(), &st).unwrap();

        assert!(!expected.is_empty());
        assert_eq!(fs::read(&dst_tmp.0).unwrap(), expected);
    }

    #[test]
    fn clone_file_counts_bytes_and_files() {
        let (src_tmp, dst_tmp) = (Tmp::new("count-src"), Tmp::new("count-dst"));
        let data = pattern(3 * CHUNK + 5);
        fs::write(&src_tmp.0, &data).unwrap();
        let src: OwnedFd = File::open(&src_tmp.0).unwrap().into();
        let st = rustix::fs::fstat(&src).unwrap();
        let (bytes0, files0) = (BYTES.load(Relaxed), FILES.load(Relaxed));

        clone_file(&src, &dst_tmp.open(), &st).unwrap();

        // Other tests share the counters, so only a lower bound holds.
        assert!(BYTES.load(Relaxed) - bytes0 >= data.len() as u64);
        assert!(FILES.load(Relaxed) - files0 >= 1);
    }

    /// A file that grows during the copy used to be cut short at the size the
    /// stat saw.
    #[test]
    fn a_file_that_grows_during_the_copy_is_copied_whole() {
        let (src_tmp, dst_tmp) = (Tmp::new("grow-src"), Tmp::new("grow-dst"));
        fs::write(&src_tmp.0, b"first").unwrap();
        let src: OwnedFd = File::open(&src_tmp.0).unwrap().into();
        let dst = dst_tmp.open();
        // Stat before the append, so `st_size` is one half of what is there by
        // the time the copy runs.
        let st = rustix::fs::fstat(&src).unwrap();
        let mut src_file = File::options().append(true).open(&src_tmp.0).unwrap();
        src_file.write_all(b"second").unwrap();
        src_file.flush().unwrap();

        clone_file(&src, &dst, &st).unwrap();

        assert_eq!(fs::read(&dst_tmp.0).unwrap(), b"firstsecond");
    }

    #[test]
    fn write_all_writes_whole_buffer() {
        let tmp = Tmp::new("write-all");
        let dst = tmp.open();
        let data = pattern(3 * CHUNK + 5);

        write_all(&dst, &data).unwrap();

        assert_eq!(fs::read(&tmp.0).unwrap(), data);
    }

    #[test]
    fn write_all_appends_across_calls() {
        let tmp = Tmp::new("write-all-twice");
        let dst = tmp.open();

        write_all(&dst, b"foo").unwrap();
        write_all(&dst, b"bar").unwrap();

        assert_eq!(fs::read(&tmp.0).unwrap(), b"foobar");
    }

    #[test]
    fn write_all_empty_buffer_is_noop() {
        let tmp = Tmp::new("write-all-empty");
        let dst = tmp.open();

        write_all(&dst, &[]).unwrap();

        assert!(fs::read(&tmp.0).unwrap().is_empty());
    }

    #[test]
    fn write_all_propagates_error() {
        let tmp = Tmp::new("write-all-ro");
        drop(tmp.open());
        let read_only: OwnedFd = File::open(&tmp.0).unwrap().into();
        assert!(write_all(&read_only, b"x").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_all_reports_full_device() {
        let full: OwnedFd = OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap()
            .into();
        assert_eq!(write_all(&full, b"x"), Err(Errno::NOSPC));
    }

    #[test]
    fn commit_succeeds_on_regular_file() {
        let tmp = Tmp::new("commit");
        let dst = tmp.open();
        write_all(&dst, b"data").unwrap();

        commit(&dst).unwrap();

        assert_eq!(fs::read(&tmp.0).unwrap(), b"data");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn commit_propagates_error() {
        let null: OwnedFd = OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap()
            .into();
        assert_eq!(commit(&null), Err(Errno::INVAL));
    }
}
