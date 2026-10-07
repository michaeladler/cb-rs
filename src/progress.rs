use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::copy::{BYTES, FILES};

/// Copies shorter than this never draw, so quick pastes stay silent.
const DELAY: Duration = Duration::from_millis(300);
const TICK: Duration = Duration::from_millis(100);
const UNKNOWN: u64 = u64::MAX;

/// Run `f`, the copy of `src`, showing progress on stderr while it runs. Only
/// when stderr is a terminal: stdout may be piped while a human watches.
///
/// The total comes from a stat walk running beside the copy, so the copy never
/// waits for it. Until that walk finishes there is no bar and no ETA.
pub fn track<T>(src: &Path, f: impl FnOnce() -> T) -> T {
    if !std::io::stderr().is_terminal() {
        return f();
    }
    let (bytes0, files0) = (BYTES.load(Relaxed), FILES.load(Relaxed));
    let total = AtomicU64::new(UNKNOWN);
    let stop = AtomicBool::new(false);
    let (done, wait) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let (total, stop) = (&total, &stop);
        scope.spawn(move || {
            if let Some(size) = tree_size(src, stop) {
                total.store(size, Relaxed);
            }
        });
        scope.spawn(move || {
            draw(&wait, total, bytes0, files0);
            // The stat walk may still be running on a huge tree the copy
            // already finished, e.g. all reflinks.
            stop.store(true, Relaxed);
        });
        // Dropped on return or unwind, so both threads always exit.
        let _done = done;
        f()
    })
}

fn draw(wait: &mpsc::Receiver<()>, total: &AtomicU64, bytes0: u64, files0: u64) {
    if wait.recv_timeout(DELAY) != Err(RecvTimeoutError::Timeout) {
        return;
    }
    let bytes = || BYTES.load(Relaxed) - bytes0;
    let files = || format!("{} files", FILES.load(Relaxed) - files0);
    let bar = ProgressBar::hidden().with_style(style(
        "{spinner} {bytes} · {bytes_per_sec} · {msg} · {elapsed}",
    ));
    // Feed the bytes moved during DELAY to the rate estimator, then forget the
    // spike: otherwise they read as moved in no time. `with_position` skips
    // the estimator, so the next update would spike instead.
    bar.set_message(files());
    bar.set_position(bytes());
    bar.reset_eta();
    bar.set_draw_target(ProgressDrawTarget::stderr());
    loop {
        let size = total.load(Relaxed);
        if bar.length().is_none() && size != UNKNOWN {
            bar.set_length(size);
            bar.set_style(style(
                "{spinner} {bar:24} {bytes}/{total_bytes} · {bytes_per_sec} · {msg} · {eta} left",
            ));
        }
        bar.set_message(files());
        bar.set_position(bytes());
        bar.tick();
        if wait.recv_timeout(TICK) != Err(RecvTimeoutError::Timeout) {
            break;
        }
    }
    bar.finish_and_clear();
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap()
        .progress_chars("━╸ ")
}

/// Bytes `clone_file` will move for `path`: regular file sizes only, symlinks
/// not followed. Unreadable entries count 0; the copy reports them. `None`
/// once `stop` is set.
fn tree_size(path: &Path, stop: &AtomicBool) -> Option<u64> {
    if stop.load(Relaxed) {
        return None;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Some(0);
    };
    if !meta.is_dir() {
        return Some(if meta.is_file() { meta.len() } else { 0 });
    }
    let mut size = 0;
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        size += tree_size(&entry.path(), stop)?;
    }
    Some(size)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn tree_size_sums_regular_files_only() {
        let dir = std::env::temp_dir().join(format!("cb-progress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a"), [0; 10]).unwrap();
        fs::write(dir.join("sub/b"), [0; 5]).unwrap();
        symlink("a", dir.join("l")).unwrap();

        let size = tree_size(&dir, &AtomicBool::new(false));
        let stopped = tree_size(&dir, &AtomicBool::new(true));
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(size, Some(15));
        assert_eq!(stopped, None);
    }
}
