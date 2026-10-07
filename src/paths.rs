use std::env;
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
    pub fn open(name: &str) -> Self {
        Self {
            root: state_root().join(name),
        }
    }

    /// Absolute sources `paste` moves, one per line.
    pub fn originals(&self) -> PathBuf {
        self.root.join(METADATA).join(ORIGINALS)
    }

    /// Absolute sources `paste` copies, one per line.
    pub fn copies(&self) -> PathBuf {
        self.root.join(METADATA).join(COPIES)
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.root.join(METADATA))
    }

    /// A new copy or cut replaces the whole clipboard entry.
    pub fn reset(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_file(self.originals());
        let _ = std::fs::remove_file(self.copies());
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
        let fd = rustix::fs::open(&file, OFlags::RDWR | OFlags::CREATE, Mode::RWXU)?;
        flock(&fd, FlockOperation::LockExclusive)?;
        Ok(fd)
    }

    pub fn read_list(&self, file: &Path) -> Vec<PathBuf> {
        let Ok(contents) = std::fs::read_to_string(file) else {
            return Vec::new();
        };
        contents.lines().map(PathBuf::from).collect()
    }

    /// An empty list removes the file, so a consumed clipboard leaves nothing
    /// behind for the next read.
    ///
    /// Callers hold [`Self::lock`]. The write still goes to a temporary that is
    /// renamed over the target, so a reader that ignores the lock still sees
    /// either the whole old list or the whole new one, never half of either.
    pub fn write_list(&self, file: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
        if paths.is_empty() {
            let _ = std::fs::remove_file(file);
            return Ok(());
        }
        self.ensure()?;
        let mut text = String::new();
        for path in paths {
            text.push_str(&path.to_string_lossy());
            text.push('\n');
        }
        let tmp = file.with_extension("new");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, file)
    }

    /// Replace `file` with `remaining` only if it still holds exactly `expected`.
    /// Returns whether the write happened.
    ///
    /// A paste does its slow work with the lock released, so the list may have
    /// moved on by the time it wants to record what it consumed. Writing then
    /// would discard a `cut` recorded in between; refusing leaves the newer
    /// entry alone and costs the caller a retry.
    pub fn consume(
        &self,
        file: &Path,
        expected: &[PathBuf],
        remaining: &[PathBuf],
    ) -> std::io::Result<bool> {
        let _lock = self.lock()?;
        if self.read_list(file) != expected {
            return Ok(false);
        }
        self.write_list(file, remaining)?;
        Ok(true)
    }
}

fn state_root() -> PathBuf {
    if let Some(dir) = env::var_os("CLIPBOARD_PERSISTDIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join(STATE_DIR);
    }
    home().join(".local/state").join(STATE_DIR)
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
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
    fn consume_refuses_when_another_process_recorded_a_path() {
        let tmp = Tmp::new("consume-clobber");
        let clipboard = tmp.clipboard();
        let file = clipboard.originals();
        let expected = vec![PathBuf::from("/one")];
        clipboard.write_list(&file, &expected).unwrap();
        // Another `cb` records a path after the paste read its snapshot.
        clipboard
            .write_list(&file, &[PathBuf::from("/one"), PathBuf::from("/fresh")])
            .unwrap();

        assert!(!clipboard.consume(&file, &expected, &[]).unwrap());

        assert!(
            clipboard.read_list(&file) == vec![PathBuf::from("/one"), PathBuf::from("/fresh")],
            "a concurrent cut must survive the paste's rewrite"
        );
    }
}
