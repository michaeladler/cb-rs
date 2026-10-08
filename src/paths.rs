use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags, flock};

pub const DEFAULT_NAME: &str = "0";
/// Own state directory, deliberately not the C++ `cb`'s `clipboard/`: sharing it
/// means a `cut` here wipes the other tool's staged bytes, since a new cut
/// resets the whole clipboard entry.
pub const STATE_DIR: &str = "cb-rs";
const METADATA: &str = "metadata";
const ORIGINALS: &str = "originals";
const COPIES: &str = "copies";
const LOCK: &str = "lock";

/// `originals` holds what `paste` moves and `copies` what it copies; neither
/// holds bytes, because neither `cut` nor `copy` reads a file, so nothing is
/// staged anywhere.
pub struct Clipboard {
    pub root: PathBuf,
}

impl Clipboard {
    pub fn open(name: &str) -> io::Result<Self> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid clipboard name",
            ));
        }
        Ok(Self {
            root: state_root().join(name),
        })
    }

    /// Absolute sources `paste` moves.
    pub fn originals(&self) -> PathBuf {
        self.root.join(METADATA).join(ORIGINALS)
    }

    /// Absolute sources `paste` copies.
    pub fn copies(&self) -> PathBuf {
        self.root.join(METADATA).join(COPIES)
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        self.ensure_at(&state_root())
    }

    fn ensure_at(&self, state: &Path) -> std::io::Result<()> {
        let fallback = self.root.starts_with(state) && state == fallback_root();
        if fallback {
            ensure_fallback_root(state)?;
        }
        let metadata = self.root.join(METADATA);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(&metadata)?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(metadata, std::fs::Permissions::from_mode(0o700))
    }

    /// A new copy or cut replaces the whole clipboard entry.
    pub fn reset(&self) -> std::io::Result<()> {
        for file in [self.originals(), self.copies()] {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        self.ensure()
    }

    /// Hold this for a whole read-modify-write of the recorded lists: two `cb`
    /// processes that both read, both append and both write otherwise lose one
    /// another's paths. The kernel drops the lock when the process dies, so a
    /// crash cannot wedge the clipboard.
    ///
    /// Held only for the bookkeeping, never across a copy: the work that
    /// follows a paste is slow, and holding this through it would block every
    /// other `cb` command. A paste that released the lock uses
    /// [`Self::consume`] to write its result back instead.
    pub fn lock(&self) -> std::io::Result<OwnedFd> {
        self.ensure()?;
        let file = self.root.join(METADATA).join(LOCK);
        // `RWUSR`, not `RWXU`: the lock file names nothing, but the lists beside
        // it do, and a state directory another user can list is a clipboard they
        // can read.
        let fd = rustix::fs::open(
            &file,
            OFlags::RDWR | OFlags::CREATE,
            Mode::RUSR | Mode::WUSR,
        )?;
        flock(&fd, FlockOperation::LockExclusive)?;
        Ok(fd)
    }

    /// NUL-separated raw bytes. A newline inside a path would split one entry
    /// into two, so a `cut` could later move a path the user never selected, and
    /// `to_string_lossy` would corrupt a path that is not valid UTF-8. A file
    /// holding such a path also failed `read_to_string` outright, so the whole
    /// list read as empty.
    pub fn read_list(&self, file: &Path) -> Vec<PathBuf> {
        let Ok(contents) = std::fs::read(file) else {
            return Vec::new();
        };
        contents
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| PathBuf::from(OsStr::from_bytes(entry)))
            .collect()
    }

    /// An empty list removes the file, so a consumed clipboard leaves nothing
    /// behind for the next read.
    ///
    /// Callers hold [`Self::lock`]. The write goes to a temporary that is synced
    /// before rename, so a reader that ignores the lock sees the whole old list or
    /// the whole new one, never half of either.
    pub fn write_list(&self, file: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
        if paths.is_empty() {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
            let parent = file
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            return std::fs::File::open(parent)?.sync_all();
        }
        self.ensure()?;
        let mut bytes = Vec::new();
        for path in paths {
            bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
            bytes.push(0);
        }
        let tmp = file.with_extension("new");
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let result = (|| {
            output.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            output.write_all(&bytes)?;
            output.sync_all()?;
            drop(output);
            std::fs::rename(&tmp, file)?;
            let parent = file
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Remove consumed paths from `file`, preserving concurrent additions.
    /// Returns whether `file` still matched `expected`.
    pub fn consume(
        &self,
        file: &Path,
        expected: &[PathBuf],
        remaining: &[PathBuf],
    ) -> std::io::Result<bool> {
        let _lock = self.lock()?;
        let current = self.read_list(file);
        let matched = current == expected;
        if matched {
            self.write_list(file, remaining)?;
            return Ok(true);
        }
        let mut pending = HashMap::new();
        for path in remaining {
            *pending.entry(path).or_insert(0usize) += 1;
        }
        let mut consumed = HashMap::new();
        for path in expected {
            match pending.get_mut(path) {
                Some(count) if *count > 0 => *count -= 1,
                _ => *consumed.entry(path.clone()).or_insert(0usize) += 1,
            }
        }
        let merged = current
            .into_iter()
            .filter(|path| match consumed.get_mut(path) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    false
                }
                _ => true,
            })
            .collect::<Vec<_>>();
        self.write_list(file, &merged)?;
        Ok(false)
    }
}

/// `$HOME/.local/state`, or somewhere absolute when there is no `HOME`.
///
/// A relative fallback put the clipboard in whatever directory cb happened to run
/// in, so the same `cut` was a different clipboard per directory, and pasting in
/// a checkout left state in the repository.
fn state_root() -> PathBuf {
    root_from(
        env::var_os("CLIPBOARD_PERSISTDIR"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
}

fn fallback_root() -> PathBuf {
    PathBuf::from("/tmp").join(format!(
        "{STATE_DIR}-{}",
        rustix::process::getuid().as_raw()
    ))
}

fn ensure_fallback_root(root: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => validate_fallback_root(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            match builder.mode(0o700).create(root) {
                Ok(()) => {
                    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            validate_fallback_root(&std::fs::symlink_metadata(root)?)
        }
        Err(error) => Err(error),
    }
}

fn validate_fallback_root(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    if metadata.file_type().is_dir()
        && metadata.uid() == rustix::process::getuid().as_raw()
        && metadata.mode() & 0o7777 == 0o700
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe clipboard fallback directory",
        ))
    }
}

fn root_from(persist: Option<OsString>, xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(dir) = persist {
        return PathBuf::from(dir);
    }
    if let Some(dir) = xdg {
        return PathBuf::from(dir).join(STATE_DIR);
    }
    match home {
        Some(home) => PathBuf::from(home).join(".local/state").join(STATE_DIR),
        // One directory per user, so two of them cannot share a clipboard. The
        // uid is the only identity a process without `HOME` still has.
        None => fallback_root(),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cb-paths-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn clipboard(&self) -> Clipboard {
            Clipboard {
                root: self.0.join("0"),
            }
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reset_propagates_list_removal_errors() {
        let tmp = Tmp::new("reset-error");
        let clipboard = tmp.clipboard();
        std::fs::create_dir_all(clipboard.root.join(METADATA)).unwrap();
        std::fs::write(clipboard.originals(), b"old original").unwrap();
        std::fs::create_dir(clipboard.copies()).unwrap();

        let error = clipboard.reset().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::IsADirectory);
        assert!(!clipboard.originals().exists());
    }

    #[test]
    fn clipboard_names_cannot_escape_state_root() {
        for name in ["../foo", "a/b", "/abs/dir", ".", "..", ""] {
            assert!(Clipboard::open(name).is_err(), "accepted {name:?}");
        }
        assert!(Clipboard::open("work").is_ok());
    }

    #[test]
    fn the_lock_excludes_a_second_holder() {
        let tmp = Tmp::new("lock");
        let held = tmp.clipboard().lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let root = tmp.clipboard().root;
        std::thread::spawn(move || {
            let _second = Clipboard { root }.lock().unwrap();
            tx.send(()).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a second lock must wait while the first is held"
        );
        drop(held);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the lock must be released when it is dropped");
    }

    #[test]
    fn ensure_does_not_change_state_root_permissions() {
        let tmp = Tmp::new("state-mode");
        let state_mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&tmp.0, state_mode).unwrap();
        let clipboard = tmp.clipboard();

        clipboard.ensure_at(&tmp.0).unwrap();

        assert_eq!(
            std::fs::metadata(&tmp.0).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&clipboard.root)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(clipboard.root.join(METADATA))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn clipboard_state_and_lists_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = Tmp::new("private-mode");
        let clipboard = tmp.clipboard();
        drop(clipboard.lock().unwrap());

        let root_mode = std::fs::metadata(&clipboard.root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
        let metadata = std::fs::metadata(clipboard.root.join("metadata")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        let lock_mode = std::fs::metadata(clipboard.root.join("metadata").join("lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(lock_mode, 0o600);
        let file = clipboard.originals();
        clipboard
            .write_list(&file, &[PathBuf::from("/secret")])
            .unwrap();
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// The state root must be absolute whatever the environment says, or the
    /// same `cut` is a different clipboard per working directory and a paste in a
    /// checkout leaves state in the repository.
    #[test]
    fn the_state_root_is_always_absolute() {
        let none: Option<OsString> = None;
        for (persist, xdg, home) in [
            (Some(OsString::from("/persist")), none.clone(), none.clone()),
            (
                none.clone(),
                Some(OsString::from("/xdg")),
                Some(OsString::from("/home/someone")),
            ),
            (
                none.clone(),
                none.clone(),
                Some(OsString::from("/home/someone")),
            ),
            (none.clone(), none.clone(), none),
        ] {
            let root = root_from(persist.clone(), xdg.clone(), home.clone());
            assert!(
                root.is_absolute(),
                "{root:?} from {persist:?}/{xdg:?}/{home:?}"
            );
        }
    }

    #[test]
    fn no_home_falls_back_to_a_per_user_directory() {
        let root = root_from(None, None, None);
        assert!(
            root.starts_with("/tmp") && root.file_name().unwrap() != STATE_DIR,
            "a HOME-less process must not share one clipboard with the next user: {root:?}"
        );
    }

    #[test]
    fn fallback_rejects_symlinks_and_unsafe_directories() {
        use std::os::unix::fs::symlink;

        let tmp = Tmp::new("fallback");
        let target = tmp.0.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.0.join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            ensure_fallback_root(&link).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            ensure_fallback_root(&target).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        ensure_fallback_root(&target).unwrap();
        let created = tmp.0.join("created");
        ensure_fallback_root(&created).unwrap();
        let metadata = std::fs::symlink_metadata(created).unwrap();
        assert_eq!(metadata.uid(), rustix::process::getuid().as_raw());
        assert_eq!(metadata.mode() & 0o777, 0o700);
    }

    #[test]
    fn write_list_leaves_no_temporary_behind() {
        let tmp = Tmp::new("write");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        let paths = vec![PathBuf::from("/one"), PathBuf::from("/two")];

        clipboard.write_list(&file, &paths).unwrap();

        assert_eq!(clipboard.read_list(&file), paths);
        let left: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n != "lock")
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("originals")]);
    }

    /// A newline inside a path used to split one entry into two, so a later
    /// `paste` moved a path that was never selected.
    #[test]
    fn a_path_containing_a_newline_stays_one_entry() {
        let tmp = Tmp::new("newline");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        let paths = vec![PathBuf::from("/one\n/two"), PathBuf::from("/three")];

        clipboard.write_list(&file, &paths).unwrap();

        assert_eq!(clipboard.read_list(&file), paths);
    }

    /// `to_string_lossy` would have replaced each invalid byte with U+FFFD, so
    /// the paste aimed at a path that does not exist.
    #[test]
    fn a_path_that_is_not_utf8_survives_a_round_trip() {
        let tmp = Tmp::new("non-utf8");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        let paths = vec![PathBuf::from(OsStr::from_bytes(b"/\xff\xfe/caf\xc3\xa9"))];

        clipboard.write_list(&file, &paths).unwrap();

        assert_eq!(clipboard.read_list(&file), paths);
        assert!(
            clipboard.read_list(&file)[0]
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(b"/\xff")
        );
    }

    #[test]
    fn an_empty_list_leaves_nothing_to_read() {
        let tmp = Tmp::new("empty-list");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        clipboard
            .write_list(&file, &[PathBuf::from("/one")])
            .unwrap();

        clipboard.write_list(&file, &[]).unwrap();

        assert!(clipboard.read_list(&file).is_empty());
    }

    #[test]
    fn consume_writes_when_the_list_is_unchanged() {
        let tmp = Tmp::new("consume-match");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        let expected = vec![PathBuf::from("/one"), PathBuf::from("/two")];
        clipboard.write_list(&file, &expected).unwrap();

        assert!(
            clipboard
                .consume(&file, &expected, &[expected[0].clone()])
                .unwrap()
        );

        assert_eq!(clipboard.read_list(&file), vec![PathBuf::from("/one")]);
    }

    #[test]
    fn consume_merges_when_another_process_records_a_path() {
        let tmp = Tmp::new("consume-merge");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        let expected = vec![PathBuf::from("/one"), PathBuf::from("/two")];
        clipboard.write_list(&file, &expected).unwrap();
        // Another `cb` records a path after the paste read its snapshot.
        clipboard
            .write_list(
                &file,
                &[
                    PathBuf::from("/one"),
                    PathBuf::from("/two"),
                    PathBuf::from("/fresh"),
                ],
            )
            .unwrap();

        assert!(
            !clipboard
                .consume(&file, &expected, &[PathBuf::from("/two")])
                .unwrap()
        );

        assert_eq!(
            clipboard.read_list(&file),
            vec![PathBuf::from("/two"), PathBuf::from("/fresh")],
            "a concurrent cut survives while consumed entries are removed"
        );
    }
}
