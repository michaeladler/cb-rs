use std::env;
use std::path::PathBuf;

pub const DEFAULT_NAME: &str = "0";
const DATA: &str = "data";
const METADATA: &str = "metadata";
const ORIGINALS: &str = "originals";

/// Same on-disk layout as the C++ implementation, so entries survive a switch
/// between the two binaries.
pub struct Clipboard {
    pub root: PathBuf,
}

impl Clipboard {
    pub fn open(name: &str) -> Self {
        Self {
            root: state_root().join(name),
        }
    }

    pub fn data(&self) -> PathBuf {
        self.root.join(DATA)
    }

    pub fn originals(&self) -> PathBuf {
        self.root.join(METADATA).join(ORIGINALS)
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.data())?;
        std::fs::create_dir_all(self.root.join(METADATA))
    }

    /// A new copy or cut replaces the whole clipboard entry.
    pub fn reset(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_dir_all(self.data());
        let _ = std::fs::remove_file(self.originals());
        self.ensure()
    }

    /// Absolute source paths recorded by `cut`, in the order they were given.
    pub fn cut_sources(&self) -> Vec<PathBuf> {
        let Ok(contents) = std::fs::read_to_string(self.originals()) else {
            return Vec::new();
        };
        contents.lines().map(PathBuf::from).collect()
    }

    /// Only paths that did not complete stay recorded, so an interrupted paste
    /// can be retried.
    pub fn set_cut_sources(&self, paths: &[PathBuf]) -> std::io::Result<()> {
        if paths.is_empty() {
            let _ = std::fs::remove_file(self.originals());
            return Ok(());
        }
        self.ensure()?;
        let mut text = String::new();
        for path in paths {
            text.push_str(&path.to_string_lossy());
            text.push('\n');
        }
        std::fs::write(self.originals(), text)
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
