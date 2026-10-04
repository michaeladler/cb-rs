use std::env;
use std::path::{Path, PathBuf};

pub const DEFAULT_NAME: &str = "0";
const DATA: &str = "data";
const METADATA: &str = "metadata";
const ORIGINALS: &str = "originals";
const COPIES: &str = "copies";

/// Same state root and `originals` format as the C++ implementation, so both
/// tools read the move list the same way. `originals` holds what `paste` moves
/// and `copies` what it copies; neither holds bytes, because neither `cut` nor
/// `copy` reads a file. Nothing is staged, so `data/` is only a legacy wipe.
pub struct Clipboard {
    pub root: PathBuf,
}

impl Clipboard {
    pub fn open(name: &str) -> Self {
        Self {
            root: state_root().join(name),
        }
    }

    /// Absolute sources `paste` moves, one per line. The C++ `cb` reads this
    /// same file with the same meaning.
    pub fn originals(&self) -> PathBuf {
        self.root.join(METADATA).join(ORIGINALS)
    }

    /// Absolute sources `paste` copies, one per line. cb-rs only: the C++ `cb`
    /// stages copied bytes instead and so has no such list.
    pub fn copies(&self) -> PathBuf {
        self.root.join(METADATA).join(COPIES)
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.root.join(METADATA))
    }

    /// A new copy or cut replaces the whole clipboard entry. `data/` is no
    /// longer written, but a version that did write it left real bytes on disk,
    /// so reclaim them here rather than leaking them on upgrade.
    pub fn reset(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_dir_all(self.root.join(DATA));
        let _ = std::fs::remove_file(self.originals());
        let _ = std::fs::remove_file(self.copies());
        self.ensure()
    }

    pub fn read_list(&self, file: &Path) -> Vec<PathBuf> {
        let Ok(contents) = std::fs::read_to_string(file) else {
            return Vec::new();
        };
        contents.lines().map(PathBuf::from).collect()
    }

    /// An empty list removes the file, so a consumed clipboard leaves nothing
    /// behind for the next read.
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
        std::fs::write(file, text)
    }
}

fn state_root() -> PathBuf {
    if let Some(dir) = env::var_os("CLIPBOARD_PERSISTDIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("clipboard");
    }
    home().join(".local/state/clipboard")
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}
