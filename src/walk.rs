use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crossbeam_deque::{Injector, Steal, Worker};
use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, chmodat, mkdirat, openat, readlinkat, renameat,
    statat, symlinkat, unlinkat,
};
use rustix::io::{Errno, Result as IoResult};

#[cfg(not(target_vendor = "apple"))]
use rustix::fs::mknodat;

/// macOS has no `mknodat` — it was never taken up by the BSDs — and rustix
/// ships no path-based `mknod` to fall back on, so a fifo or device node
/// cannot be recreated from an open directory fd. Report it like any other
/// failure rather than dropping the entry silently.
///
/// ponytail: a macOS paste of a tree holding a fifo or device node fails that
/// entry, and a cut of one fails outright with the source left in place. Fix:
/// `libc::mknod` on the path `rustix::fs::getpath` returns for `dirfd`, behind a
/// target-scoped `libc` dependency.
#[cfg(target_vendor = "apple")]
fn mknodat<P: rustix::path::Arg, Fd: rustix::fd::AsFd>(
    _dirfd: Fd,
    _path: P,
    _file_type: FileType,
    _mode: Mode,
    _dev: rustix::fs::Dev,
) -> IoResult<()> {
    Err(Errno::PERM)
}

use crate::copy;

#[derive(Debug)]
pub struct Failure {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Default)]
struct Report {
    failures: Mutex<Vec<Failure>>,
}

impl Report {
    fn fail(&self, path: &Path, errno: Errno) {
        self.failures.lock().unwrap().push(Failure {
            path: path.to_path_buf(),
            reason: errno.to_string(),
        });
    }

    fn take(self) -> Vec<Failure> {
        self.failures.into_inner().unwrap()
    }
}

/// Copy `src` to `dst` (creating it), recursing into directories in parallel.
/// Returns every entry that could not be copied.
pub fn copy_any(src: &Path, dst: &Path) -> Vec<Failure> {
    crate::progress::track(src, || copy_tree(src, dst))
}

fn copy_tree(src: &Path, dst: &Path) -> Vec<Failure> {
    let report = Report::default();
    let Ok(st) = statat(CWD, src, AtFlags::SYMLINK_NOFOLLOW) else {
        report.fail(src, Errno::NOENT);
        return report.take();
    };

    match FileType::from_raw_mode(st.st_mode) {
        FileType::Directory => {
            if let Err(e) = mkdirat(CWD, dst, Mode::RWXU)
                && e != Errno::EXIST
            {
                report.fail(src, e);
                return report.take();
            }
            walk(DirTask::new(src.to_path_buf(), dst.to_path_buf()), &report);
            // Applied last: a read-only or execute-only source directory must
            // still be writable while its children are being created.
            if let Err(e) = chmodat(CWD, dst, Mode::from_raw_mode(st.st_mode), AtFlags::empty()) {
                report.fail(dst, e);
            }
        }
        FileType::Symlink => {
            if let Err(e) = copy_symlink_path(src, dst) {
                report.fail(src, e);
            }
        }
        FileType::RegularFile => {
            if let Err(e) = copy_regular_path(src, dst) {
                report.fail(src, e);
            }
        }
        other => {
            if let Err(e) = copy_other_path(src, dst, other) {
                report.fail(src, e);
            }
        }
    }
    report.take()
}

struct DirTask {
    src: PathBuf,
    dst: PathBuf,
}

impl DirTask {
    fn new(src: PathBuf, dst: PathBuf) -> Self {
        Self { src, dst }
    }
}

/// Work-stealing over directories: each worker drains its own deque, then steals
/// from peers. Thousands of small files are latency-bound, so this is where the
/// wall clock goes.
fn walk(root: DirTask, report: &Report) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let injector = Injector::new();
    injector.push(root);

    if threads <= 1 {
        let deque = Worker::new_lifo();
        run_worker(&deque, &injector, report);
        return;
    }

    let injector = &injector;
    let report = &report;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(move || {
                let deque = Worker::new_lifo();
                run_worker(&deque, injector, report);
            });
        }
    });
}

fn run_worker(deque: &Worker<DirTask>, injector: &Injector<DirTask>, report: &Report) {
    while let Some(task) = next_task(deque, injector) {
        // `NOFOLLOW` throughout: the task hands out paths, so a directory that
        // was swapped for a symlink between hand-off and open would otherwise
        // be walked, and written through, somewhere else entirely.
        let src_fd = match openat(
            CWD,
            &task.src,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                report.fail(&task.src, e);
                continue;
            }
        };
        let dst_fd = match openat(
            CWD,
            &task.dst,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::RWXU,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                report.fail(&task.dst, e);
                continue;
            }
        };
        let mut dir = match Dir::read_from(&src_fd) {
            Ok(dir) => dir,
            Err(e) => {
                report.fail(&task.src, e);
                continue;
            }
        };
        for entry in dir.by_ref() {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            handle_entry(
                &src_fd,
                &dst_fd,
                &task,
                name,
                entry.file_type(),
                injector,
                report,
            );
        }
    }
}

/// Every worker shares one injector as the source of new directories; each
/// keeps the one it is working on locally.
fn next_task(deque: &Worker<DirTask>, injector: &Injector<DirTask>) -> Option<DirTask> {
    loop {
        if let Some(task) = deque.pop() {
            return Some(task);
        }
        match injector.steal_batch_and_pop(deque) {
            Steal::Success(task) => return Some(task),
            Steal::Retry => continue,
            Steal::Empty => {}
        }
        if injector.is_empty() {
            return None;
        }
        std::thread::yield_now();
    }
}

fn handle_entry(
    src_fd: &OwnedFd,
    dst_fd: &OwnedFd,
    task: &DirTask,
    name: &CStr,
    file_type: FileType,
    injector: &Injector<DirTask>,
    report: &Report,
) {
    let src_path = task.src.join(OsStr::from_bytes(name.to_bytes()));
    let dst_path = task.dst.join(OsStr::from_bytes(name.to_bytes()));

    if file_type == FileType::Directory {
        if let Err(e) = mkdirat(dst_fd, name, Mode::RWXU)
            && e != Errno::EXIST
        {
            report.fail(&src_path, e);
            return;
        }
        if let Ok(st) = statat(src_fd, name, AtFlags::SYMLINK_NOFOLLOW)
            && let Err(e) = chmodat(
                dst_fd,
                name,
                Mode::from_raw_mode(st.st_mode),
                AtFlags::empty(),
            )
        {
            report.fail(&dst_path, e);
        }
        injector.push(DirTask::new(src_path, dst_path));
        return;
    }

    // `d_type` is DT_UNKNOWN on some filesystems; fall back to a real stat.
    let file_type = if file_type == FileType::Unknown {
        match statat(src_fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => FileType::from_raw_mode(st.st_mode),
            Err(e) => {
                report.fail(&src_path, e);
                return;
            }
        }
    } else {
        file_type
    };

    let result = match file_type {
        FileType::Symlink => copy_symlink(src_fd, name, dst_fd, name),
        FileType::RegularFile => copy_regular(src_fd, name, dst_fd, name),
        other => match statat(src_fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => replace_entry(dst_fd, name, |tmp| {
                mknodat(
                    dst_fd,
                    tmp,
                    other,
                    Mode::from_raw_mode(st.st_mode),
                    st.st_rdev,
                )
            }),
            Err(e) => Err(e),
        },
    };
    if let Err(e) = result {
        report.fail(&src_path, e);
    }
}

/// Create `dst_name` under a private name and `renameat` it into place.
///
/// Unlinking first leaves a window in which a competing writer's file is gone
/// and nothing has taken its place, so a lost race destroys the destination
/// rather than replacing it. `renameat` replaces atomically, and the temporary
/// is cleaned up on both failure paths.
fn replace_entry<F>(dst_dir: &OwnedFd, dst_name: &CStr, create: F) -> IoResult<()>
where
    F: FnOnce(&CStr) -> IoResult<()>,
{
    let tmp = temp_name(dst_name);
    if let Err(e) = create(&tmp) {
        let _ = unlinkat(dst_dir, &tmp, AtFlags::empty());
        return Err(e);
    }
    match renameat(dst_dir, &tmp, dst_dir, dst_name) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = unlinkat(dst_dir, &tmp, AtFlags::empty());
            Err(e)
        }
    }
}

/// `replace_entry` for a whole path, where the temporary lands beside it so the
/// rename stays on one filesystem.
fn replace_entry_path<F>(dst: &Path, create: F) -> IoResult<()>
where
    F: FnOnce(&Path) -> IoResult<()>,
{
    let name = dst.file_name().ok_or(Errno::INVAL)?;
    let tmp = dst.with_file_name(OsStr::from_bytes(&temp_bytes(name.as_bytes())));
    if let Err(e) = create(&tmp) {
        let _ = rustix::fs::unlink(&tmp);
        return Err(e);
    }
    match rustix::fs::rename(&tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = rustix::fs::unlink(&tmp);
            Err(e)
        }
    }
}

static TEMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// Private enough that another `cb` will not collide: the pid, plus a counter
/// for the many temporaries one process creates under one directory fd.
fn temp_name(dst_name: &CStr) -> CString {
    CString::new(temp_bytes(dst_name.to_bytes())).unwrap_or_else(|_| c".cb-tmp".to_owned())
}

fn temp_bytes(dst_name: &[u8]) -> Vec<u8> {
    let n = TEMP_SEQ.fetch_add(1, Relaxed);
    let mut bytes = format!(".cb-tmp.{}.{n}.", std::process::id()).into_bytes();
    bytes.extend_from_slice(dst_name);
    bytes
}

/// A private sibling of `path`, on the same filesystem so a rename from it
/// stays atomic. Each call names a different file.
pub fn staged_path(path: &Path) -> IoResult<PathBuf> {
    let name = path.file_name().ok_or(Errno::INVAL)?;
    Ok(path.with_file_name(OsStr::from_bytes(&temp_bytes(name.as_bytes()))))
}

fn copy_regular(
    src_dir: &OwnedFd,
    src_name: &CStr,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
) -> IoResult<()> {
    let src_fd = openat(
        src_dir,
        src_name,
        OFlags::RDONLY | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let st = rustix::fs::fstat(&src_fd)?;
    let dst_fd = openat(
        dst_dir,
        dst_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )?;
    copy::clone_file(&src_fd, &dst_fd, &st)?;
    copy::preserve_mode(&dst_fd, Mode::from_raw_mode(st.st_mode))
}

/// Symlinks are recreated, never followed: following one would copy data the
/// user did not select and could escape the tree.
fn copy_symlink(
    src_dir: &OwnedFd,
    src_name: &CStr,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
) -> IoResult<()> {
    let target = readlinkat(src_dir, src_name, Vec::new())?;
    replace_entry(dst_dir, dst_name, |tmp| {
        symlinkat(target.as_bytes(), dst_dir, tmp)
    })
}

/// Top-level entry points: whole paths rather than names under an open
/// directory fd, which is what the recursive worker passes around.
fn copy_symlink_path(src: &Path, dst: &Path) -> IoResult<()> {
    let target = rustix::fs::readlink(src, Vec::new())?;
    replace_entry_path(dst, |tmp| rustix::fs::symlink(target.as_bytes(), tmp))
}

fn copy_regular_path(src: &Path, dst: &Path) -> IoResult<()> {
    let src_fd = rustix::fs::open(src, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())?;
    let st = rustix::fs::fstat(&src_fd)?;
    let dst_fd = rustix::fs::open(
        dst,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )?;
    copy::clone_file(&src_fd, &dst_fd, &st)?;
    copy::preserve_mode(&dst_fd, Mode::from_raw_mode(st.st_mode))
}

fn copy_other_path(src: &Path, dst: &Path, file_type: FileType) -> IoResult<()> {
    let st = statat(CWD, src, AtFlags::SYMLINK_NOFOLLOW)?;
    replace_entry_path(dst, |tmp| {
        mknodat(
            CWD,
            tmp,
            file_type,
            Mode::from_raw_mode(st.st_mode),
            st.st_rdev,
        )
    })
}

/// Remove a tree bottom-up. Only used to finish a move that `renameat2` could
/// not perform because the two paths are on different filesystems.
pub fn remove_any(path: &Path) -> IoResult<()> {
    let st = match statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(e) if e == Errno::NOENT => return Ok(()),
        Err(e) => return Err(e),
    };
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
        return unlinkat(CWD, path, AtFlags::empty());
    }
    // `NOFOLLOW`: the `statat` above named a directory, and a symlink swapped in
    // between must fail the open rather than send the recursive delete into
    // whatever it points at.
    let dir_fd = openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let mut dir = Dir::read_from(&dir_fd)?;
    let mut children = Vec::new();
    for entry in dir.by_ref() {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        children.push(path.join(OsStr::from_bytes(name.to_bytes())));
    }
    for child in children {
        let _ = remove_any(&child);
    }
    unlinkat(CWD, path, AtFlags::REMOVEDIR)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::unix::ffi::OsStrExt;
    #[cfg(not(target_vendor = "apple"))]
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::symlink;

    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cb-walk-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn subdir(&self, name: &str) -> OwnedFd {
            let dir = self.path(name);
            fs::create_dir(&dir).unwrap();
            File::open(dir).unwrap().into()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn copy_symlink_recreates_link_without_following() {
        let tmp = Tmp::new("sym");
        let (src_dir, dst_dir) = (tmp.subdir("src"), tmp.subdir("dst"));
        symlink("../target-not-there", tmp.path("src/l")).unwrap();

        copy_symlink(&src_dir, c"l", &dst_dir, c"l").unwrap();

        let copied = tmp.path("dst/l");
        assert!(fs::symlink_metadata(&copied).unwrap().is_symlink());
        assert_eq!(
            fs::read_link(&copied).unwrap().as_os_str().as_bytes(),
            b"../target-not-there"
        );
    }

    #[test]
    fn copy_symlink_replaces_existing_destination() {
        let tmp = Tmp::new("sym-replace");
        let (src_dir, dst_dir) = (tmp.subdir("src"), tmp.subdir("dst"));
        symlink("new", tmp.path("src/l")).unwrap();
        fs::write(tmp.path("dst/l"), b"old").unwrap();

        copy_symlink(&src_dir, c"l", &dst_dir, c"l").unwrap();

        assert_eq!(fs::read_link(tmp.path("dst/l")).unwrap(), Path::new("new"));
    }

    #[test]
    fn replace_entry_keeps_the_destination_when_the_create_fails() {
        let tmp = Tmp::new("replace-fail");
        let dir = tmp.subdir("dst");
        fs::write(tmp.path("dst/l"), b"old").unwrap();

        assert_eq!(
            replace_entry(&dir, c"l", |_| Err(Errno::PERM)),
            Err(Errno::PERM)
        );
        assert_eq!(fs::read(tmp.path("dst/l")).unwrap(), b"old");
        assert_eq!(
            fs::read_dir(tmp.path("dst")).unwrap().count(),
            1,
            "no temporary may be left behind"
        );
    }

    #[test]
    fn remove_any_unlinks_a_symlinked_directory_rather_than_its_target() {
        let tmp = Tmp::new("rm-symlink");
        fs::create_dir(tmp.path("real")).unwrap();
        fs::write(tmp.path("real/keep.txt"), b"keep").unwrap();
        symlink(tmp.path("real"), tmp.path("link")).unwrap();

        remove_any(&tmp.path("link")).unwrap();

        assert!(fs::symlink_metadata(tmp.path("link")).is_err());
        assert!(
            tmp.path("real/keep.txt").exists(),
            "the link target must survive"
        );
    }

    #[test]
    fn copy_symlink_missing_source_reports_noent() {
        let tmp = Tmp::new("sym-missing");
        let (src_dir, dst_dir) = (tmp.subdir("src"), tmp.subdir("dst"));

        assert_eq!(
            copy_symlink(&src_dir, c"l", &dst_dir, c"l"),
            Err(Errno::NOENT)
        );
    }

    #[test]
    fn copy_symlink_non_symlink_source_reports_inval() {
        let tmp = Tmp::new("sym-regular");
        let (src_dir, dst_dir) = (tmp.subdir("src"), tmp.subdir("dst"));
        fs::write(tmp.path("src/f"), b"x").unwrap();

        assert_eq!(
            copy_symlink(&src_dir, c"f", &dst_dir, c"f"),
            Err(Errno::INVAL)
        );
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn copy_other_path_recreates_fifo() {
        let tmp = Tmp::new("fifo");
        let (src, dst) = (tmp.path("src"), tmp.path("dst"));
        mknodat(CWD, &src, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();

        copy_other_path(&src, &dst, FileType::Fifo).unwrap();

        assert!(fs::symlink_metadata(&dst).unwrap().file_type().is_fifo());
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn copy_other_path_replaces_existing_destination() {
        let tmp = Tmp::new("fifo-replace");
        let (src, dst) = (tmp.path("src"), tmp.path("dst"));
        mknodat(CWD, &src, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
        fs::write(&dst, b"old").unwrap();

        copy_other_path(&src, &dst, FileType::Fifo).unwrap();

        assert!(fs::symlink_metadata(&dst).unwrap().file_type().is_fifo());
    }

    #[test]
    fn copy_other_path_missing_source_reports_noent() {
        let tmp = Tmp::new("other-missing");
        assert_eq!(
            copy_other_path(&tmp.path("nope"), &tmp.path("dst"), FileType::Fifo),
            Err(Errno::NOENT)
        );
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn copy_other_path_missing_source_keeps_destination() {
        let tmp = Tmp::new("other-keep");
        let dst = tmp.path("dst");
        fs::write(&dst, b"keep").unwrap();

        assert!(copy_other_path(&tmp.path("nope"), &dst, FileType::Fifo).is_err());

        assert_eq!(fs::read(&dst).unwrap(), b"keep");
    }
}
