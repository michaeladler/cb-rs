use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crossbeam_deque::{Injector, Steal, Worker};
use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, chmodat, mkdirat, mknodat, openat, readlinkat,
    statat, symlinkat, unlinkat,
};
use rustix::io::{Errno, Result as IoResult};

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
        let src_fd = match openat(
            CWD,
            &task.src,
            OFlags::RDONLY | OFlags::DIRECTORY,
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
            OFlags::RDONLY | OFlags::DIRECTORY,
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
            Ok(st) => {
                let _ = unlinkat(dst_fd, name, AtFlags::empty());
                mknodat(
                    dst_fd,
                    name,
                    other,
                    Mode::from_raw_mode(st.st_mode),
                    st.st_rdev,
                )
            }
            Err(e) => Err(e),
        },
    };
    if let Err(e) = result {
        report.fail(&src_path, e);
    }
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
    let _ = unlinkat(dst_dir, dst_name, AtFlags::empty());
    symlinkat(target.as_bytes(), dst_dir, dst_name)
}

/// Top-level entry points: whole paths rather than names under an open
/// directory fd, which is what the recursive worker passes around.
fn copy_symlink_path(src: &Path, dst: &Path) -> IoResult<()> {
    let target = rustix::fs::readlink(src, Vec::new())?;
    let _ = rustix::fs::unlink(dst);
    rustix::fs::symlink(target.as_bytes(), dst)
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
    let _ = rustix::fs::unlink(dst);
    mknodat(
        CWD,
        dst,
        file_type,
        Mode::from_raw_mode(st.st_mode),
        st.st_rdev,
    )
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
    let dir_fd = openat(CWD, path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
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
