use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicU32, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};

use crossbeam_deque::{Injector, Steal, Worker};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Gid, Mode, OFlags, Stat, Timespec, Timestamps, Uid, chownat,
    fchmod, fchown, mkdirat, openat, readlinkat, renameat, statat, symlinkat, unlinkat, utimensat,
};
use rustix::io::{Errno, Result as IoResult};
use rustix::path::Arg;

#[cfg(not(target_vendor = "apple"))]
use rustix::fs::mknodat;

/// `openat2` is Linux-only, and refusing a symlink in any component of the path
/// is what `open_dir` needs it for.
#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, openat2};

/// macOS has no `mknodat` — it was never taken up by the BSDs — and rustix
/// ships no path-based `mknod` to fall back on, so go through libc's `mknod`
/// on a path. `getpath` recovers the directory an fd names, which is what the
/// path has to be relative to.
#[cfg(target_vendor = "apple")]
fn mknodat<P: rustix::path::Arg, Fd: rustix::fd::AsFd>(
    dirfd: Fd,
    path: P,
    file_type: FileType,
    mode: Mode,
    dev: rustix::fs::Dev,
) -> IoResult<()> {
    use std::os::fd::{AsFd, AsRawFd};

    // `CWD` is not a real fd, so `getpath` cannot name it; the process working
    // directory is the directory it stands for.
    let dir = if dirfd.as_fd().as_raw_fd() == CWD.as_fd().as_raw_fd() {
        let cwd = std::env::current_dir().map_err(|_| Errno::IO)?;
        cwd.into_os_string().into_encoded_bytes()
    } else {
        rustix::fs::getpath(dirfd)?.into_bytes()
    };

    path.into_with_c_str(|path| {
        let bytes = path.to_bytes();
        let full = if bytes.starts_with(b"/") {
            bytes.to_vec()
        } else {
            let mut full = dir.clone();
            full.push(b'/');
            full.extend_from_slice(bytes);
            full
        };
        let Ok(full) = CString::new(full) else {
            return Err(Errno::INVAL);
        };
        // SAFETY: `full` is NUL-terminated and outlives the call.
        if unsafe { libc::mknod(full.as_ptr(), mode.bits() | file_type.as_raw_mode(), dev) } == 0 {
            Ok(())
        } else {
            Err(Errno::from_raw_os_error(unsafe { *libc::__error() }))
        }
    })
}

use crate::copy;

/// Which threads walked a directory, so a test can tell a parallel walk from one
/// that ran on a single thread.
#[cfg(test)]
fn walkers() -> &'static Mutex<Vec<std::thread::ThreadId>> {
    static WALKERS: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());
    &WALKERS
}

#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Default)]
struct Report {
    failures: Mutex<Vec<Failure>>,
    /// `None` unless the caller can use it. A copy has no use for it, so
    /// recording is not free by default: one `Stat` and a `PathBuf` per
    /// directory would be spent on nothing.
    walked: Option<Walked>,
}

impl Report {
    fn fail(&self, path: &Path, errno: Errno) {
        self.reject(path, &errno.to_string());
    }

    fn reject(&self, path: &Path, reason: &str) {
        self.failures.lock().unwrap().push(Failure {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        });
    }

    fn take(&self) -> Vec<Failure> {
        std::mem::take(&mut *self.failures.lock().unwrap())
    }

    fn note(&self, path: &Path, st: &Stat, entries: usize) {
        if let Some(walked) = &self.walked {
            walked.note(path, st, entries);
        }
    }
}

/// One source directory, as the walk saw it.
struct Seen {
    path: PathBuf,
    st: Stat,
    entries: usize,
}

/// What the copy saw of the source tree, so the caller can tell whether the
/// source still holds what was copied.
#[derive(Clone, Default)]
pub struct Walked(Arc<Mutex<Vec<Seen>>>);

impl Walked {
    fn note(&self, path: &Path, st: &Stat, entries: usize) {
        self.0.lock().unwrap().push(Seen {
            path: path.to_path_buf(),
            st: *st,
            entries,
        });
    }

    /// The source directories that no longer hold what the copy took: gone,
    /// swapped for another inode, or changed inside.
    ///
    /// A cross-device move deletes the source only when this is empty. Anything
    /// written into the source after the walk read a directory was never
    /// copied, and deleting the source would lose it.
    pub fn changed(&self) -> Vec<PathBuf> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| !unchanged(seen))
            .map(|seen| seen.path.clone())
            .collect()
    }
}

/// Whether the directory still holds the inode, the timestamps and the entry
/// count the walk saw. Opened by name: anything a swap does in between turns
/// into a refusal to delete, never into a deletion of something else.
fn unchanged(seen: &Seen) -> bool {
    let Ok(fd) = openat(
        CWD,
        &seen.path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    ) else {
        return false;
    };
    let Ok(st) = rustix::fs::fstat(&fd) else {
        return false;
    };
    if st.st_dev != seen.st.st_dev
        || st.st_ino != seen.st.st_ino
        || st.st_mtime != seen.st.st_mtime
        || st.st_size != seen.st.st_size
    {
        return false;
    }
    // The timestamps alone would do for almost everything, but a directory's
    // mtime comes from the kernel's coarse clock, so a write landing in the
    // same tick as the `readdir` leaves it untouched. The entry count is what
    // catches that one.
    count_entries(&fd) == Some(seen.entries)
}

fn count_entries(dir_fd: &OwnedFd) -> Option<usize> {
    let dir = Dir::read_from(dir_fd).ok()?;
    let mut count = 0;
    for entry in dir {
        let entry = entry.ok()?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            count += 1;
        }
    }
    Some(count)
}

/// Copy `src` to `dst` (creating it), recursing into directories in parallel.
/// Returns every entry that could not be copied.
pub fn copy_any(src: &Path, dst: &Path) -> Vec<Failure> {
    copy_walking(src, dst, None)
}

/// `copy_any`, recording what the walk saw of the source tree for a caller that
/// has to know whether the source still holds what was copied.
///
/// A copy deletes nothing, so `walked` is `None` there and nothing is recorded.
pub fn copy_walking(src: &Path, dst: &Path, walked: Option<&Walked>) -> Vec<Failure> {
    let report = Arc::new(Report::default());
    if same_file(src, dst).unwrap_or(false) {
        report.reject(src, "source and destination are the same file");
        return report.take();
    }
    if inside_source(src, dst).unwrap_or(false) {
        report.reject(src, "destination is inside the source");
        return report.take();
    }
    crate::progress::track(src, || copy_tree(src, dst, walked))
}

/// Whether `dst` would land inside the `src` tree, so the copy recurses into its
/// own output and the move falls back to copying the source into itself.
///
/// Both paths are resolved first: the source is recorded absolute and
/// symlink-free, but the destination is whatever the user typed, so a symlinked
/// destination directory would otherwise slip past a prefix test on the path.
pub fn inside_source(src: &Path, dst: &Path) -> IoResult<bool> {
    let (Ok(src), Some(dst_dir)) = (src.canonicalize(), dst.parent()) else {
        return Ok(false);
    };
    let Ok(dst_dir) = dst_dir.canonicalize() else {
        return Ok(false);
    };
    Ok(dst_dir.starts_with(src))
}

/// Whether two paths name the same inode, no final symlink followed.
///
/// Pasting into the folder an item already lives in makes the destination the
/// source: the copy would truncate the file it is reading, and the move would
/// empty the directory it is moving.
pub fn same_file(a: &Path, b: &Path) -> IoResult<bool> {
    let (a, b) = (
        statat(CWD, a, AtFlags::SYMLINK_NOFOLLOW)?,
        statat(CWD, b, AtFlags::SYMLINK_NOFOLLOW)?,
    );
    Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
}

fn copy_tree(src: &Path, dst: &Path, walked: Option<&Walked>) -> Vec<Failure> {
    let report = Arc::new(Report {
        walked: walked.cloned(),
        ..Report::default()
    });
    let st = match statat(CWD, src, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(e) => {
            report.fail(src, e);
            return report.take();
        }
    };

    match FileType::from_raw_mode(st.st_mode) {
        FileType::Directory => {
            if let Err(e) = mkdirat(CWD, dst, Mode::RWXU)
                && e != Errno::EXIST
            {
                report.fail(src, e);
                return report.take();
            }
            // The node applies the source mode via `fchmod` once the whole
            // subtree is in place. A read-only or execute-only source
            // directory must still be writable while its children are being
            // created, so the chmod cannot happen here.
            let node = DirNode::new(st, dst.to_path_buf(), None, &report);
            walk(
                DirTask {
                    src: src.to_path_buf(),
                    node,
                },
                &report,
            );
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

/// Completion bookkeeping for one destination directory.
///
/// A directory is created writable (`RWXU`) and later restored to the source's
/// metadata, because a read-only or execute-only source directory must still be
/// writable while its children are being created. Restoration cannot happen
/// until all subdirectory tasks finish.
///
/// Each node counts outstanding work beneath it and restores metadata when the
/// count reaches zero.
///
/// The node keeps only the path, never an fd: holding one open descriptor per
/// queued task exhausts the process limit on a wide tree. The finalizer opens
/// the destination just before restoring metadata, with `NOFOLLOW`.
struct DirNode {
    st: Stat,
    dst: PathBuf,
    pending: AtomicUsize,
    parent: Option<Arc<DirNode>>,
    report: Arc<Report>,
}

impl DirNode {
    fn new(
        st: Stat,
        dst: PathBuf,
        parent: Option<Arc<DirNode>>,
        report: &Arc<Report>,
    ) -> Arc<Self> {
        // Keep the parent's count above zero until this child is done.
        if let Some(p) = &parent {
            // Release so the child's own registration happens-before any
            // parent finalizer that observes its count.
            p.pending.fetch_add(1, Release);
        }
        Arc::new(Self {
            st,
            dst,
            pending: AtomicUsize::new(1),
            parent,
            report: Arc::clone(report),
        })
    }

    /// Called once by the worker that processed this directory's task, and
    /// once per subdirectory when its subtree completes.
    fn done(&self) {
        // Acquire pairs with the Release of the last child's registration,
        // and the open sees every `mkdirat` the workers performed, because
        // each of those happened-before the registration.
        if self.pending.fetch_sub(1, AcqRel) == 1 {
            // `NOFOLLOW` so a symlink swapped in at `dst` fails the open
            // instead of being chmod-followed.
            match openat(
                CWD,
                &self.dst,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            ) {
                Ok(fd) => {
                    let result = preserve_metadata(&fd, CWD, &self.dst, &self.st);
                    if let Err(e) = result {
                        self.report.fail(&self.dst, e);
                    }
                }
                Err(e) => self.report.fail(&self.dst, e),
            }
            if let Some(p) = &self.parent {
                p.done();
            }
        }
    }
}

struct DirTask {
    src: PathBuf,
    node: Arc<DirNode>,
}

/// Work-stealing over directories: each worker drains its own deque, then steals
/// from peers. Thousands of small files are latency-bound, so this is where the
/// wall clock goes.
/// The shared queue of directories, with a count of what has been pushed and not
/// yet finished.
///
/// `Injector::is_empty` on its own cannot end the walk: a worker part-way
/// through a directory has not pushed that directory's subdirectories yet, so
/// every other worker finds the queue empty and exits, and the rest of the tree
/// is walked by whichever thread happened to take the root. The count says
/// whether any thread can still produce work.
///
/// It cannot reach zero while another worker holds a task: a worker counts a
/// subdirectory in before it finishes the directory it was walking, so the count
/// never dips to zero until the last task in flight is done.
struct Queue {
    injector: Injector<DirTask>,
    live: AtomicUsize,
}

impl Queue {
    fn with_root(root: DirTask) -> Self {
        let queue = Self {
            injector: Injector::new(),
            live: AtomicUsize::new(0),
        };
        queue.push(root);
        queue
    }

    fn push(&self, task: DirTask) {
        // Counted before the task is visible, so a worker can never observe an
        // empty queue and a zero count for work that is already on its way.
        self.live.fetch_add(1, Release);
        self.injector.push(task);
    }

    /// Whether nothing is left to do, not merely queued.
    fn drained(&self) -> bool {
        self.live.load(Acquire) == 0
    }
}

fn walk(root: DirTask, report: &Arc<Report>) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let queue = Queue::with_root(root);

    if threads <= 1 {
        let deque = Worker::new_lifo();
        run_worker(&deque, &queue, report);
        return;
    }

    let queue = &queue;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(move || {
                let deque = Worker::new_lifo();
                run_worker(&deque, queue, report);
            });
        }
    });
}

fn run_worker(deque: &Worker<DirTask>, queue: &Queue, report: &Arc<Report>) {
    while let Some(task) = next_task(deque, queue) {
        #[cfg(test)]
        {
            let mut seen = walkers().lock().unwrap();
            let id = std::thread::current().id();
            if !seen.contains(&id) {
                seen.push(id);
            }
        }
        process(&task, queue, report);
        // This directory's own entry list is done; once every subdirectory it
        // pushed is also done, `done` applies the source mode.
        task.node.done();
        queue.live.fetch_sub(1, Release);
    }
}

fn process(task: &DirTask, queue: &Queue, report: &Arc<Report>) {
    // `NOFOLLOW` throughout: the task hands out paths, so a directory that
    // was swapped for a symlink between hand-off and open would otherwise
    // be walked, and written through, somewhere else entirely. The
    // destination fd is held only for this one iteration, never queued.
    let src_fd = match openat(
        CWD,
        &task.src,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            report.fail(&task.src, e);
            return;
        }
    };
    let dst_fd = match openat(
        CWD,
        &task.node.dst,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            report.fail(&task.node.dst, e);
            return;
        }
    };
    let mut dir = match Dir::read_from(&src_fd) {
        Ok(dir) => dir,
        Err(e) => {
            report.fail(&task.src, e);
            return;
        }
    };
    let mut count = 0;
    for entry in dir.by_ref() {
        // An entry the walk never read is never copied, yet the copy still
        // reports success, and a cross-device move then deletes the source.
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report.fail(&task.src, e);
                continue;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        count += 1;
        handle_entry(
            &src_fd,
            &dst_fd,
            task,
            name,
            entry.file_type(),
            queue,
            report,
        );
    }
    // What the directory held once the walk was done with it, so a move can
    // tell afterwards whether the source still holds what was copied.
    match rustix::fs::fstat(&src_fd) {
        Ok(st) => report.note(&task.src, &st, count),
        // Without the stat the source cannot be checked at all, and an
        // unverifiable source is one a move must not delete.
        Err(e) => report.fail(&task.src, e),
    }
}

/// Every worker shares one queue as the source of new directories; each keeps the
/// one it is working on locally.
///
/// `None` means the walk is over, so it waits here while another worker is still
/// walking a directory that has not pushed its own subdirectories yet.
fn next_task(deque: &Worker<DirTask>, queue: &Queue) -> Option<DirTask> {
    loop {
        if let Some(task) = deque.pop() {
            return Some(task);
        }
        match queue.injector.steal_batch_and_pop(deque) {
            Steal::Success(task) => return Some(task),
            Steal::Retry => continue,
            Steal::Empty => {}
        }
        if queue.drained() {
            return None;
        }
        // The only work left is inside another worker, which will push before it
        // finishes. Parking briefly beats burning a core on a directory that is
        // one `readdir` away from handing over its subdirectories.
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

fn handle_entry(
    src_fd: &OwnedFd,
    dst_fd: &OwnedFd,
    task: &DirTask,
    name: &CStr,
    file_type: FileType,
    queue: &Queue,
    report: &Arc<Report>,
) {
    let src_path = task.src.join(OsStr::from_bytes(name.to_bytes()));
    let dst_path = task.node.dst.join(OsStr::from_bytes(name.to_bytes()));

    // `d_type` is DT_UNKNOWN on some filesystems; fall back to a real stat so
    // directories are recognised before the non-directory match below.
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

    if file_type == FileType::Directory {
        if let Err(e) = mkdirat(dst_fd, name, Mode::RWXU)
            && e != Errno::EXIST
        {
            report.fail(&src_path, e);
            return;
        }
        let st = match statat(src_fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(e) => {
                report.fail(&src_path, e);
                return;
            }
        };
        // No fd here: the node is just a path and a counter, so a wide
        // directory queues many tasks without touching the fd limit. The
        // destination fd opens only when a worker processes the task.
        let node = DirNode::new(st, dst_path, Some(Arc::clone(&task.node)), report);
        queue.push(DirTask {
            src: src_path,
            node,
        });
        return;
    }

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
                )?;
                preserve_metadata_at(dst_fd, tmp, &st)
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

/// Regular files go through a temporary name like symlinks and device files do,
/// not into the destination itself. Writing in place truncates it up front, so a
/// failure partway through leaves a half-written file where a working one was,
/// every other name of a hard-linked destination changes with it, and a
/// read-only destination cannot be opened at all.
/// fsync everything under `path`, so a move that unlinks its source cannot lose
/// both to a power cut. Directories are synced after their children, so a name
/// is only ever recorded once what it points at is.
///
/// A symlink has no contents to flush, and a fifo or device node has none that
/// can be read without blocking on a writer or sending bytes to hardware, so
/// only regular files and directories are opened. The name itself is durable
/// once its parent directory has been synced.
pub fn sync_tree(path: &Path) -> IoResult<()> {
    let st = statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)?;
    match FileType::from_raw_mode(st.st_mode) {
        FileType::Directory => {
            let (dir_fd, name) = anchor(path)?;
            sync_dir(&dir_fd, &name)
        }
        FileType::RegularFile => {
            // `NOFOLLOW`: a symlink swapped in since the `statat` must fail here
            // rather than flush whatever it points at.
            let file = openat(CWD, path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())?;
            copy::commit(&file)
        }
        _ => Ok(()),
    }
}

/// One step of a walk over a tree: `Enter` opens a directory and queues what is
/// under it, `Leave` comes back to it once everything below it is done.
///
/// A stack on the heap rather than the call stack, and one directory open at a
/// time: a recursion holds a frame and a descriptor per level, and a tree deep
/// enough to run out of either takes the walk down with no way to say which
/// directory it gave up on.
enum Step {
    Enter(CString),
    Leave(CString),
}

/// `rel` plus one entry name. `readdir` names hold neither `/` nor NUL, so the
/// joined path names exactly one thing below `rel`.
fn below(rel: &CStr, name: &CStr) -> CString {
    let mut bytes = Vec::with_capacity(rel.to_bytes().len() + 1 + name.to_bytes().len());
    bytes.extend_from_slice(rel.to_bytes());
    bytes.push(b'/');
    bytes.extend_from_slice(name.to_bytes());
    // Unreachable: the name came out of a directory listing.
    CString::new(bytes).unwrap_or_else(|_| name.to_owned())
}

/// Open `rel` under `dir_fd` as a directory to walk.
///
/// The anchor descriptor plus a relative path is what keeps a walk to a single
/// open directory at a time; a chain of descriptors down the tree runs out of
/// `RLIMIT_NOFILE` first, and on a tree deep enough to hit it the walk stops
/// without saying where.
fn open_dir<Fd: AsFd>(dir_fd: &Fd, rel: &CStr) -> IoResult<OwnedFd> {
    let oflags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    // `O_NOFOLLOW` covers the last component only, so a directory swapped for a
    // symlink higher in `rel` would send the walk somewhere else entirely.
    #[cfg(target_os = "linux")]
    match openat2(
        dir_fd,
        rel,
        oflags,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    ) {
        // A kernel older than 5.6 has no `openat2`, and one that has it without
        // `RESOLVE_NO_SYMLINKS` answers `EINVAL`.
        // ponytail: there, and on every platform but Linux, `O_NOFOLLOW` alone
        // applies, so a swap higher in `rel` can still redirect a walk. Upgrade
        // path: `openat2`, where the kernel has it.
        Err(Errno::NOSYS | Errno::INVAL) => openat(dir_fd, rel, oflags, Mode::empty()),
        other => other,
    }
    #[cfg(not(target_os = "linux"))]
    openat(dir_fd, rel, oflags, Mode::empty())
}

/// Entry names in `dir_fd`, `.` and `..` skipped. A `readdir` that fails part-way
/// has names it never yielded, so the failure goes in `first_err`: answering
/// "no entries" there is what lets a move report a tree it never saw whole.
fn entry_names<Fd: AsFd>(dir_fd: Fd, first_err: &mut Option<Errno>) -> Vec<CString> {
    let mut names = Vec::new();
    let dir = match Dir::read_from(dir_fd) {
        Ok(dir) => dir,
        Err(e) => {
            first_err.get_or_insert(e);
            return names;
        }
    };
    for entry in dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                first_err.get_or_insert(e);
                break;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        names.push(name.to_owned());
    }
    names
}

/// The directory holding `path`, and `path`'s own name inside it: the anchor a
/// walk below is held to.
///
/// The parent resolves with ordinary path semantics, so a symlinked directory
/// on the way to `path` is followed, as the user's path means it. Everything
/// under that name is reached from the descriptor instead.
fn anchor(path: &Path) -> IoResult<(OwnedFd, CString)> {
    // The name came from the filesystem or the command line, so it cannot hold
    // a NUL: the one thing the `*at` calls cannot carry.
    let name = path
        .file_name()
        .ok_or(Errno::INVAL)
        .and_then(|name| CString::new(name.as_bytes()).map_err(|_| Errno::INVAL))?;
    let parent = path.parent().ok_or(Errno::INVAL)?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let dir_fd = openat(
        CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )?;
    Ok((dir_fd, name))
}

/// fsync `rel` under `dir_fd` and everything under it, one directory open at a
/// time.
fn sync_dir<Fd: AsFd>(dir_fd: &Fd, rel: &CStr) -> IoResult<()> {
    let mut stack = vec![Step::Enter(rel.to_owned())];
    while let Some(step) = stack.pop() {
        let rel = match step {
            Step::Leave(rel) => {
                let fd = match open_dir(dir_fd, &rel) {
                    Ok(fd) => fd,
                    // Raced with a deletion in the copy that produced this tree:
                    // there is nothing left under the name to flush.
                    Err(Errno::NOENT) => continue,
                    Err(e) => return Err(e),
                };
                copy::commit(&fd)?;
                continue;
            }
            Step::Enter(rel) => rel,
        };
        let dir = open_dir(dir_fd, &rel)?;
        let mut read_err = None;
        let names = entry_names(&dir, &mut read_err);
        // Same as the walk: a `readdir` that fails part-way leaves names that
        // were never listed, so the destination is not the whole tree. Answering
        // `Ok` here is what lets the move unlink the source anyway.
        if let Some(e) = read_err {
            return Err(e);
        }
        let mut subdirs = Vec::new();
        for name in &names {
            match statat(&dir, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => match FileType::from_raw_mode(st.st_mode) {
                    FileType::Directory => subdirs.push(below(&rel, name.as_c_str())),
                    FileType::RegularFile => {
                        let file = openat(
                            &dir,
                            name.as_c_str(),
                            OFlags::RDONLY | OFlags::NOFOLLOW,
                            Mode::empty(),
                        )?;
                        copy::commit(&file)?;
                    }
                    _ => {}
                },
                // Raced with a deletion in the copy that produced this tree.
                Err(Errno::NOENT) => {}
                Err(e) => return Err(e),
            }
        }
        // Below first, this directory last: a name is only durable once what it
        // points at is.
        stack.push(Step::Leave(rel));
        stack.extend(subdirs.into_iter().map(Step::Enter));
    }
    Ok(())
}

/// Open a source that was classified as a regular file, and check it still is.
///
/// `O_NONBLOCK`: the name is classified by `d_type` and opened some syscalls
/// later, so a fifo swapped in between would otherwise block this process and
/// every other copy behind it, waiting for a writer that may never come. On a
/// regular file the flag changes nothing, so it stays.
///
/// `fstat` after the open, and only then: a device node opened `O_WRONLY` sends
/// the bytes to the device, which the open itself already did.
fn open_regular<P: rustix::path::Arg>(dir: impl AsFd, path: P) -> IoResult<OwnedFd> {
    let fd = openat(
        dir,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    if FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(Errno::INVAL);
    }
    Ok(fd)
}

fn timestamps(st: &Stat) -> Timestamps {
    Timestamps {
        last_access: Timespec {
            tv_sec: st.st_atime,
            tv_nsec: st.st_atime_nsec as _,
        },
        last_modification: Timespec {
            tv_sec: st.st_mtime,
            tv_nsec: st.st_mtime_nsec as _,
        },
    }
}

fn preserve_metadata<Fd: AsFd, Dir: AsFd, P: Arg>(
    fd: Fd,
    dir: Dir,
    path: P,
    st: &Stat,
) -> IoResult<()> {
    if rustix::process::geteuid().is_root() {
        fchown(
            &fd,
            Some(Uid::from_raw(st.st_uid)),
            Some(Gid::from_raw(st.st_gid)),
        )?;
    }
    fchmod(&fd, Mode::from_raw_mode(st.st_mode))?;
    utimensat(dir, path, &timestamps(st), AtFlags::SYMLINK_NOFOLLOW)
}

fn preserve_metadata_at<Fd: AsFd, P: Arg>(dir: Fd, path: P, st: &Stat) -> IoResult<()> {
    path.into_with_c_str(|path| {
        if rustix::process::geteuid().is_root() {
            chownat(
                &dir,
                path,
                Some(Uid::from_raw(st.st_uid)),
                Some(Gid::from_raw(st.st_gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
        }
        utimensat(dir, path, &timestamps(st), AtFlags::SYMLINK_NOFOLLOW)
    })
}

fn copy_regular(
    src_dir: &OwnedFd,
    src_name: &CStr,
    dst_dir: &OwnedFd,
    dst_name: &CStr,
) -> IoResult<()> {
    let src_fd = open_regular(src_dir, src_name)?;
    let st = rustix::fs::fstat(&src_fd)?;
    replace_entry(dst_dir, dst_name, |tmp| {
        let dst_fd = openat(
            dst_dir,
            tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?;
        copy::clone_file(&src_fd, &dst_fd, &st)?;
        preserve_metadata(&dst_fd, dst_dir, tmp, &st)
    })
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
    let st = statat(src_dir, src_name, AtFlags::SYMLINK_NOFOLLOW)?;
    replace_entry(dst_dir, dst_name, |tmp| {
        symlinkat(target.as_bytes(), dst_dir, tmp)?;
        preserve_metadata_at(dst_dir, tmp, &st)
    })
}

/// Top-level entry points: whole paths rather than names under an open
/// directory fd, which is what the recursive worker passes around.
fn copy_symlink_path(src: &Path, dst: &Path) -> IoResult<()> {
    let target = rustix::fs::readlink(src, Vec::new())?;
    let st = statat(CWD, src, AtFlags::SYMLINK_NOFOLLOW)?;
    replace_entry_path(dst, |tmp| {
        rustix::fs::symlink(target.as_bytes(), tmp)?;
        preserve_metadata_at(CWD, tmp, &st)
    })
}

fn copy_regular_path(src: &Path, dst: &Path) -> IoResult<()> {
    let src_fd = open_regular(CWD, src)?;
    let st = rustix::fs::fstat(&src_fd)?;
    replace_entry_path(dst, |tmp| {
        let dst_fd = rustix::fs::open(
            tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?;
        copy::clone_file(&src_fd, &dst_fd, &st)?;
        preserve_metadata(&dst_fd, CWD, tmp, &st)
    })
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
        )?;
        preserve_metadata_at(CWD, tmp, &st)
    })
}

/// Remove a tree bottom-up. Only used to finish a move that `renameat2` could
/// not perform because the two paths are on different filesystems.
pub fn remove_any(path: &Path) -> IoResult<()> {
    let (dir_fd, name) = match anchor(path) {
        Ok(anchor) => anchor,
        // Nothing to remove, and nothing that ever was.
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(e),
    };
    remove_entry(&dir_fd, &name)
}

/// Remove `rel` under `dir_fd` and everything under it, bottom-up, one
/// directory open at a time.
///
/// What is not a directory goes while its own directory is still open, by name
/// in that directory: the anchor and a relative path are enough to reach a
/// directory safely, but not to unlink from a level the walk has left.
fn remove_entry<Fd: AsFd>(dir_fd: &Fd, rel: &CStr) -> IoResult<()> {
    let mut first_err = None;
    let mut stack = vec![Step::Enter(rel.to_owned())];
    while let Some(step) = stack.pop() {
        let rel = match step {
            Step::Leave(rel) => {
                // Empty by now, so the name itself can go. `NOENT` is a
                // directory that was never there.
                match unlinkat(dir_fd, &rel, AtFlags::REMOVEDIR) {
                    Err(Errno::NOENT) => {}
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                    Ok(()) => {}
                }
                continue;
            }
            Step::Enter(rel) => rel,
        };
        let st = match statat(dir_fd, &rel, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => continue,
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        };
        if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
            if let Err(e) = unlinkat(dir_fd, &rel, AtFlags::empty()) {
                first_err.get_or_insert(e);
            }
            continue;
        }
        let mut subdirs = Vec::new();
        match open_dir(dir_fd, &rel) {
            Ok(dir) => {
                let mut read_err = None;
                let names = entry_names(&dir, &mut read_err);
                // A `readdir` that fails part-way has names it never yielded,
                // but the ones it did yield still go: leaving them is what makes
                // the leftover worse than the failure already is.
                if let Some(e) = read_err {
                    first_err.get_or_insert(e);
                }
                for name in &names {
                    match statat(&dir, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
                            subdirs.push(below(&rel, name.as_c_str()));
                        }
                        // Raced with a deletion: there is nothing left to unlink.
                        Err(Errno::NOENT) => {}
                        Ok(_) => {
                            if let Err(e) = unlinkat(&dir, name.as_c_str(), AtFlags::empty()) {
                                first_err.get_or_insert(e);
                            }
                        }
                        Err(e) => {
                            first_err.get_or_insert(e);
                        }
                    }
                }
            }
            // The cause, not the `ENOTEMPTY` the `rmdir` below would answer.
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
        stack.push(Step::Leave(rel));
        stack.extend(subdirs.into_iter().map(Step::Enter));
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};

    use super::*;

    /// Which threads actually did work, so a test can tell a parallel walk from
    /// one that ran on a single thread.
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
            // Best effort: a test that intentionally made something read-only
            // may leave the tree unresettable, so walk it and chmod dirs first.
            fn reset(p: &Path) {
                if let Ok(rd) = fs::read_dir(p) {
                    for e in rd.flatten() {
                        let child = e.path();
                        if fs::symlink_metadata(&child)
                            .map(|m| m.is_dir())
                            .unwrap_or(false)
                        {
                            reset(&child);
                        }
                    }
                }
                let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
            }
            reset(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The source is classified by `d_type` and opened some syscalls later.
    /// Swapped for a fifo in between, `O_RDONLY` blocks until a writer arrives.
    #[test]
    fn a_fifo_source_is_refused_rather_than_opened() {
        let tmp = Tmp::new("fifo-src");
        let (src_dir, dst_dir) = (tmp.subdir("src"), tmp.subdir("dst"));
        mknodat(&src_dir, c"p", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();

        assert_eq!(
            copy_regular(&src_dir, c"p", &dst_dir, c"p"),
            Err(Errno::INVAL)
        );
        assert_eq!(fs::read_dir(tmp.path("dst")).unwrap().count(), 0);
    }

    /// A device node opened `O_WRONLY` sends the bytes to the device, so the
    /// destination must never be opened by name.
    #[test]
    fn a_fifo_destination_is_not_written_to() {
        let tmp = Tmp::new("fifo-dst");
        let (src_dir, dst_dir) = (tmp.subdir("src"), tmp.subdir("dst"));
        fs::write(tmp.path("src/f"), b"payload").unwrap();
        mknodat(&dst_dir, c"p", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();

        copy_regular(&src_dir, c"f", &dst_dir, c"p").unwrap();

        assert_eq!(
            fs::read(tmp.path("dst/p")).unwrap(),
            b"payload",
            "the copy must replace the fifo, not block on or write to it"
        );
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
    fn remove_any_unlinks_a_symlinked_child_rather_than_its_target() {
        let tmp = Tmp::new("rm-child-link");
        let outside = tmp.path("outside");
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(outside.join("sub/keep.txt"), b"keep").unwrap();
        let root = tmp.path("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        symlink(&outside, root.join("link")).unwrap();

        remove_any(&root).unwrap();

        assert!(!root.exists());
        assert!(
            outside.join("sub/keep.txt").exists(),
            "the child link's target must survive"
        );
    }

    #[test]
    fn remove_any_stays_put_when_a_parent_is_swapped_for_a_symlink() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let tmp = Tmp::new("rm-swap");
        let root = tmp.path("outer/mid/leaf");
        fs::create_dir_all(&root).unwrap();
        // A mirror of the tree under a second name: if the delete is ever
        // resolved through the swapped symlink, this is what gets destroyed.
        let victim = tmp.path("victim");
        fs::create_dir_all(victim.join("leaf")).unwrap();
        const FILES: usize = 10_000;
        for i in 0..FILES {
            fs::write(root.join(format!("f{i}")), b"x").unwrap();
            fs::write(victim.join(format!("leaf/f{i}")), b"keep").unwrap();
        }

        let mid = tmp.path("outer/mid");
        let mid_parked = tmp.path("outer/mid.bak");
        let (mid_c, parked_c, victim_c) = (mid.clone(), mid_parked.clone(), victim.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        let swapper = std::thread::spawn(move || {
            // Wait past `remove_any`'s entry open (microseconds), then hold the
            // swap for the rest of the delete: a path-based recursion
            // re-resolves the parent on every child and on the final rmdir, so
            // any timing of the delete lands on the symlink. The fd-based
            // delete holds its own handles and is blind to it.
            std::thread::sleep(Duration::from_millis(5));
            let _ = fs::rename(&mid_c, &parked_c);
            let _ = symlink(&victim_c, &mid_c);
            // Hard ceiling: a panicked test never sets `stop`, and a live
            // thread would hold the test binary open forever.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !stop_c.load(Relaxed) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        remove_any(&root).unwrap();
        stop.store(true, Relaxed);
        swapper.join().unwrap();
        // The swap is still in place; restore before asserting on the path.
        let _ = fs::remove_file(&mid);
        let _ = fs::rename(&mid_parked, &mid);

        assert!(!root.exists());
        let remaining = fs::read_dir(victim.join("leaf")).unwrap().count();
        assert_eq!(
            remaining, FILES,
            "the symlink target must survive a redirected delete: {remaining}/{FILES} files left"
        );
    }

    /// The parent resolves the way the user's path means it: a symlinked
    /// directory on the way to the tree is followed, not refused.
    #[test]
    fn remove_any_follows_a_symlink_on_the_way_to_the_tree() {
        let tmp = Tmp::new("rm-link-parent");
        fs::create_dir_all(tmp.path("real/tree")).unwrap();
        fs::write(tmp.path("real/tree/f"), b"x").unwrap();
        symlink(tmp.path("real"), tmp.path("link")).unwrap();

        remove_any(&tmp.path("link/tree")).unwrap();

        assert!(!tmp.path("real/tree").exists());
        assert!(tmp.path("real").exists());
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

    #[test]
    fn copy_other_path_recreates_fifo() {
        let tmp = Tmp::new("fifo");
        let (src, dst) = (tmp.path("src"), tmp.path("dst"));
        mknodat(CWD, &src, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();

        copy_other_path(&src, &dst, FileType::Fifo).unwrap();

        assert!(fs::symlink_metadata(&dst).unwrap().file_type().is_fifo());
    }

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

    #[test]
    fn copy_other_path_missing_source_keeps_destination() {
        let tmp = Tmp::new("other-keep");
        let dst = tmp.path("dst");
        fs::write(&dst, b"keep").unwrap();

        assert!(copy_other_path(&tmp.path("nope"), &dst, FileType::Fifo).is_err());

        assert_eq!(fs::read(&dst).unwrap(), b"keep");
    }

    /// The cross-device move unlinks the source right after the copy, so
    /// everything the copy wrote has to be on disk first.
    #[test]
    fn sync_tree_walks_files_and_directories() {
        let tmp = Tmp::new("sync-tree");
        let tree = tmp.path("tree");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("top"), b"top").unwrap();
        fs::write(tree.join("a/mid"), b"mid").unwrap();
        fs::write(tree.join("a/b/deep"), b"deep").unwrap();
        // A dangling link: opening it `O_RDONLY` answers `ENOENT`, which after a
        // rename that already happened left the move reporting a failure and the
        // source still in place.
        symlink("nowhere", tmp.path("link")).unwrap();

        sync_tree(&tree).unwrap();
        sync_tree(&tmp.0.join("link")).unwrap();
        sync_tree(&tree.join("top")).unwrap();

        assert_eq!(
            fs::read_link(tmp.path("link")).unwrap(),
            Path::new("nowhere")
        );

        assert_eq!(fs::read(tree.join("a/b/deep")).unwrap(), b"deep");
    }

    #[test]
    fn sync_tree_does_not_open_a_fifo() {
        let tmp = Tmp::new("sync-fifo");
        // `O_RDONLY` on a fifo blocks until a writer arrives; the walk must
        // reach the parent and not the node.
        let fifo = tmp.path("p");
        mknodat(CWD, &fifo, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();

        sync_tree(&fifo).unwrap();
    }

    #[test]
    fn sync_tree_missing_reports_noent() {
        let tmp = Tmp::new("sync-missing");
        assert_eq!(sync_tree(&tmp.path("gone")), Err(Errno::NOENT));
    }

    /// A `readdir` that fails part-way leaves names the walk never saw. If that
    /// answers "no entries", the tree looks complete, the move deletes the
    /// source, and files are gone that were never copied.
    #[test]
    fn a_failed_readdir_is_not_an_empty_directory() {
        let tmp = Tmp::new("readdir-fail");
        let file = File::create(tmp.path("f")).unwrap();

        assert_eq!(sync_dir(&file, c"f").unwrap_err(), Errno::NOTDIR);
        assert_eq!(remove_entry(&file, c"f").unwrap_err(), Errno::NOTDIR);
    }

    /// The walk's own directory open is the one failure a copy reports for every
    /// entry underneath, so a source it cannot read must not look copied.
    #[test]
    fn an_unreadable_source_directory_is_reported() {
        use std::os::unix::fs::MetadataExt;

        if std::fs::metadata(".").unwrap().uid() == 0 {
            eprintln!("skipping: root reads anything");
            return;
        }
        let tmp = Tmp::new("unreadable-src");
        fs::create_dir_all(tmp.path("src/ro")).unwrap();
        fs::write(tmp.path("src/ro/secret"), b"x").unwrap();
        fs::set_permissions(tmp.path("src/ro"), fs::Permissions::from_mode(0o000)).unwrap();

        let failures = copy_tree(&tmp.path("src"), &tmp.path("dst"), None);
        fs::set_permissions(tmp.path("src/ro"), fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            failures.iter().any(|f| f.path == tmp.path("src/ro")),
            "a directory the walk cannot read must be a failure, not a silent skip: {failures:?}"
        );
        assert!(!tmp.path("dst/ro/secret").exists());
    }

    #[test]
    fn copy_tree_does_not_chmod_through_a_symlink() {
        let tmp = Tmp::new("copy-nofollow");
        fs::create_dir(tmp.path("src")).unwrap();
        let victim = tmp.path("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o700)).unwrap();
        // `dst` already exists as a symlink to `victim`, so `mkdirat` reports
        // EEXIST and the eventual `fchmod` must not follow it.
        symlink(&victim, tmp.path("dst")).unwrap();

        let failures = copy_tree(&tmp.path("src"), &tmp.path("dst"), None);
        assert!(
            failures.iter().any(|f| f.path == tmp.path("dst")),
            "the symlinked destination must be reported: {failures:?}"
        );
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o700,
        );
    }

    /// A worker part-way through a directory has not pushed its subdirectories
    /// yet. `Injector::is_empty` alone let every other worker exit there, so the
    /// rest of the tree was walked by whichever thread took the root.
    #[test]
    fn the_queue_is_only_drained_once_the_last_task_is_finished() {
        let report = Arc::new(Report::default());
        let tmp = Tmp::new("queue");
        let src = tmp.0.join("src");
        let st = statat(CWD, &tmp.0, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        let queue = Queue::with_root(DirTask {
            src,
            node: DirNode::new(st, PathBuf::from("/nowhere"), None, &report),
        });
        let deque = Worker::new_lifo();

        let task = next_task(&deque, &queue).expect("the root is queued");
        assert!(
            !queue.drained(),
            "an empty injector must not end the walk while a task is in flight"
        );
        drop(task);

        queue.live.fetch_sub(1, Release);
        assert!(queue.drained());
        assert!(next_task(&deque, &queue).is_none());
    }

    /// The walk must spread over the workers, not run on one.
    #[test]
    fn a_wide_tree_is_walked_by_more_than_one_thread() {
        walkers().lock().unwrap().clear();
        if std::thread::available_parallelism().is_ok_and(|n| n.get() <= 1) {
            eprintln!("skipping: one thread available");
            return;
        }
        let tmp = Tmp::new("parallel");
        let src = tmp.path("src");
        // Wide and deep enough that one thread leaves the rest waiting on it.
        for i in 0..8 {
            fs::create_dir_all(src.join(format!("d{i}/inner"))).unwrap();
            for j in 0..8 {
                fs::write(src.join(format!("d{i}/inner/f{j}")), b"x").unwrap();
            }
        }

        assert!(copy_tree(&src, &tmp.path("dst"), None).is_empty());

        let threads = walkers().lock().unwrap().len();
        assert!(
            threads > 1,
            "the copy of a wide tree ran on {threads} thread(s)"
        );
    }

    #[test]
    fn copy_wide_directory_stays_under_the_fd_limit() {
        // `RLIMIT_NOFILE` is per-process, so the lowered limit must live in a
        // re-exec of this binary: dropping it in-process would break file
        // opens of the other tests that run concurrently in this one.
        if std::env::var_os("CB_WIDE_FDS_REEXEC").is_some() {
            wide_fds_child();
            return;
        }
        let bin = std::env::args().next().expect("test binary path");
        let status = std::process::Command::new(&bin)
            .env("CB_WIDE_FDS_REEXEC", "1")
            .arg("--exact")
            .arg("walk::tests::copy_wide_directory_stays_under_the_fd_limit")
            .status()
            .unwrap();
        assert!(status.success(), "the re-exec'd copy failed: {status}");
    }

    fn wide_fds_child() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

        let tmp = Tmp::new("wide-fds");
        let src = tmp.path("src");
        let dst = tmp.path("dst");
        let dirs = 200u32;
        fs::create_dir_all(&src).unwrap();
        for i in 0..dirs {
            fs::create_dir(src.join(format!("d{i}"))).unwrap();
        }

        let old = getrlimit(Resource::Nofile);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as u64;
        // The walk holds a src and a dst fd per worker, plus the finalizers'
        // brief opens; 200 queued children (the old behaviour) must breach
        // the limit well below that.
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(3 + 3 * threads + 32),
                maximum: old.maximum,
            },
        )
        .unwrap();

        let failures = copy_tree(&src, &dst, None);
        assert!(failures.is_empty(), "{failures:?}");
        let count = fs::read_dir(&dst).unwrap().count();
        assert_eq!(count, usize::try_from(dirs).unwrap());
    }

    /// The sync and the delete walked one directory per descriptor held open, so
    /// a tree deeper than `RLIMIT_NOFILE` stopped them mid-tree and reported a
    /// failure with the rest of the source still there.
    #[test]
    fn a_deep_tree_stays_under_the_fd_limit() {
        // The lowered limit is per-process, so for the same reason as the walk's
        // own it lives in a re-exec of this binary.
        if std::env::var_os("CB_DEEP_FDS_REEXEC").is_some() {
            deep_fds_child();
            return;
        }
        let bin = std::env::args().next().expect("test binary path");
        let status = std::process::Command::new(&bin)
            .env("CB_DEEP_FDS_REEXEC", "1")
            .arg("--exact")
            .arg("walk::tests::a_deep_tree_stays_under_the_fd_limit")
            .status()
            .unwrap();
        assert!(status.success(), "the re-exec'd walk failed: {status}");
    }

    fn deep_fds_child() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

        let tmp = Tmp::new("deep-fds");
        let root = tmp.path("tree");
        let depth = 256u32;
        fs::create_dir(&root).unwrap();
        let mut path = root.clone();
        for _ in 0..depth {
            path = path.join("d");
            fs::create_dir(&path).unwrap();
        }
        fs::write(path.join("f"), b"x").unwrap();

        let old = getrlimit(Resource::Nofile);
        // One directory at a time, plus the anchor and the listing: `depth`
        // descriptors held open at once would breach this early.
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(32),
                maximum: old.maximum,
            },
        )
        .unwrap();

        let synced = sync_tree(&root);
        let removed = remove_any(&root);
        setrlimit(Resource::Nofile, old).unwrap();

        synced.unwrap();
        removed.unwrap();
        assert!(!root.exists(), "the deep tree must be gone whole");
    }

    #[test]
    fn copy_defers_read_only_directory_mode_until_children_exist() {
        let tmp = Tmp::new("ro-dir");
        let src = tmp.path("src");
        let dst = tmp.path("dst");
        fs::create_dir_all(src.join("ro")).unwrap();
        fs::write(src.join("ro/child.txt"), b"hi").unwrap();
        // 0o555 is readable and traversable but not writable: the destination
        // copy must still be able to create `child.txt` inside `ro`.
        fs::set_permissions(src.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();

        let failures = copy_tree(&src, &dst, None);
        // Restore the source so the guard's cleanup can walk it.
        fs::set_permissions(src.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(fs::read(dst.join("ro/child.txt")).unwrap(), b"hi");
        let mode = fs::metadata(dst.join("ro")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "source mode must still be applied");
    }
}
