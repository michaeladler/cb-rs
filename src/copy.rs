#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
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
use rustix::ioctl::{IntegerSetter, Setter, ioctl, opcode};

/// `_IOW(0x94, 9, int)`: clone the open file into the destination inode.
#[cfg(target_os = "linux")]
const FICLONE: rustix::ioctl::Opcode = opcode::write::<i32>(0x94, 9);

const CHUNK: usize = 1 << 20;
/// `copy_file_range` allocates no buffer, so this is only an upper bound on how
/// much one call may move; the kernel stops at EOF.
#[cfg(target_os = "linux")]
const CFR_MAX: u64 = 64 << 20;

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
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn clone_file(src: &OwnedFd, dst: &OwnedFd, src_st: &Stat) -> Result<Method> {
    #[cfg(target_os = "linux")]
    {
        let key = pair(src_st.st_dev, rustix::fs::fstat(dst)?.st_dev);

        if NO_REF_LINK.load(Relaxed) != key {
            match try_reflink(src, dst) {
                Ok(()) => return Ok(Method::Reflink),
                Err(e) if unavailable(e) => NO_REF_LINK.store(key, Relaxed),
                // Transient: let the next rung have a go rather than give up.
                Err(_) => {}
            }
        }

        // `copy_file_range` is one syscall per 1 MiB instead of per 4 KiB, and on
        // kernels that support it across mounts stays entirely in the kernel.
        //
        // rustix only ships it on Linux and `std::fs::copy_file_range` is still
        // unstable, so elsewhere the rung is skipped and `copy_stream` pays. macOS
        // has its own `clonefile`; add it as a rung before anyone needs the speed.
        let mut remaining = src_st.st_size.max(0) as u64;
        while remaining > 0 && NO_CFR.load(Relaxed) != key {
            match copy_file_range(src, None, dst, None, remaining.min(CFR_MAX) as usize) {
                Ok(0) => break,
                Ok(n) => remaining -= n as u64,
                Err(e) if unavailable(e) => {
                    NO_CFR.store(key, Relaxed);
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if remaining == 0 {
            return Ok(Method::CopyFileRange);
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
/// The two argument conventions disagree in the kernel. `fs/ioctl.c`'s
/// `ioctl_ficlone` reads the source fd out of a user pointer, while btrfs's
/// `btrfs_ioctl_ficlone` calls `fget(arg)` on the value itself. The wrong shape
/// returns `EBADF` (btrfs) or `EFAULT` (the generic path), so the raw value is
/// tried first and the pointer form follows only when one of those two says the
/// kernel read the argument as an address. Every other errno is the
/// filesystem's real answer, and re-asking in the other shape after it is one
/// wasted syscall per file.
#[cfg(target_os = "linux")]
fn try_reflink(src: &OwnedFd, dst: &OwnedFd) -> Result<()> {
    // The ioctl is issued on the destination; the argument is the *source*
    // file descriptor number.
    let source_fd = src.as_fd().as_raw_fd();
    // SAFETY: `source_fd` is a live descriptor owned by the caller and stays
    // open for the duration of the call.
    match unsafe { ioctl(dst, IntegerSetter::<FICLONE>::new_usize(source_fd as usize)) } {
        Ok(_) => return Ok(()),
        Err(Errno::BADF | Errno::FAULT) => {}
        Err(e) => return Err(e),
    }
    // SAFETY: as above; `Setter` passes a pointer to the same `int`.
    unsafe { ioctl(dst, Setter::<FICLONE, i32>::new(source_fd)) }.map(drop)
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
        written += rustix::io::write(dst, &buf[written..])?;
    }
    Ok(())
}

/// Flush to disk before the caller unlinks the source, so a crash cannot leave
/// neither file.
pub fn commit(dst: &OwnedFd) -> Result<()> {
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
