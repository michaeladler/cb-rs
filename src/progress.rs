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

/// Before the walk lands: bytes moved, no bar, no ETA.
const SPINNER: &str = "{spinner} {bytes} · {bytes_per_sec} · {msg} · {elapsed}";
/// After it lands.
const BAR: &str = "{spinner} {bar:24} {bytes}/{total_bytes} · {bytes_per_sec} · {msg} · {eta} left";

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
    let bar = ProgressBar::hidden().with_style(style(SPINNER));
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
            bar.set_style(style(BAR));
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
///
/// An explicit stack, so a deep tree costs heap rather than the call stack, and
/// one directory open at a time: this runs beside the copy, which is the one
/// thing that cannot afford to run out of descriptors on its own account.
fn tree_size(path: &Path, stop: &AtomicBool) -> Option<u64> {
    let mut size = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(path) = stack.pop() {
        if stop.load(Relaxed) {
            return None;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            size += if meta.is_file() { meta.len() } else { 0 };
            continue;
        }
        stack.extend(
            std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path()),
        );
    }
    Some(size)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use indicatif::TermLike;

    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("cb-progress-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Debug, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Recorder {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl TermLike for Recorder {
        fn width(&self) -> u16 {
            100
        }
        fn move_cursor_up(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn write_line(&self, s: &str) -> io::Result<()> {
            self.0.lock().unwrap().extend_from_slice(s.as_bytes());
            self.0.lock().unwrap().push(b'\n');
            Ok(())
        }
        fn write_str(&self, s: &str) -> io::Result<()> {
            self.0.lock().unwrap().extend_from_slice(s.as_bytes());
            Ok(())
        }
        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Draw `template` once, as `draw` sets it up.
    fn render(template: &str, len: Option<u64>) -> String {
        let term = Recorder::default();
        let bar = ProgressBar::with_draw_target(
            len,
            ProgressDrawTarget::term_like(Box::new(term.clone())),
        );
        bar.set_style(style(template));
        bar.set_message("7 files");
        bar.set_position(1 << 20);
        bar.tick();
        term.text()
    }

    fn draw_now(wait: &mpsc::Receiver<()>, total: &AtomicU64) -> Duration {
        let started = Instant::now();
        draw(wait, total, BYTES.load(Relaxed), FILES.load(Relaxed));
        started.elapsed()
    }

    #[test]
    fn tree_size_sums_regular_files_only() {
        let dir = Tmp::new("sum");
        fs::create_dir(dir.0.join("sub")).unwrap();
        fs::write(dir.0.join("a"), [0; 10]).unwrap();
        fs::write(dir.0.join("sub/b"), [0; 5]).unwrap();
        symlink("a", dir.0.join("l")).unwrap();

        assert_eq!(tree_size(&dir.0, &AtomicBool::new(false)), Some(15));
        assert_eq!(tree_size(&dir.0, &AtomicBool::new(true)), None);
    }

    #[test]
    fn tree_size_single_file() {
        let dir = Tmp::new("file");
        fs::write(dir.0.join("a"), [0; 7]).unwrap();

        assert_eq!(
            tree_size(&dir.0.join("a"), &AtomicBool::new(false)),
            Some(7)
        );
    }

    #[test]
    fn tree_size_empty_dir() {
        assert_eq!(
            tree_size(&Tmp::new("empty").0, &AtomicBool::new(false)),
            Some(0)
        );
    }

    #[test]
    fn tree_size_missing_path() {
        let dir = Tmp::new("missing");

        assert_eq!(
            tree_size(&dir.0.join("nope"), &AtomicBool::new(false)),
            Some(0)
        );
    }

    #[test]
    fn tree_size_does_not_follow_dir_symlink() {
        let dir = Tmp::new("dirlink");
        fs::create_dir(dir.0.join("sub")).unwrap();
        fs::write(dir.0.join("sub/a"), [0; 10]).unwrap();
        symlink("sub", dir.0.join("l")).unwrap();

        assert_eq!(tree_size(&dir.0, &AtomicBool::new(false)), Some(10));
    }

    #[test]
    fn style_renders_spinner_without_total() {
        let out = render(SPINNER, None);

        assert!(out.contains("7 files"), "{out:?}");
        assert!(out.contains("MiB"), "{out:?}");
        assert!(!out.contains("left"), "{out:?}");
    }

    #[test]
    fn style_renders_bar_with_total() {
        let out = render(BAR, Some(4 << 20));

        assert!(out.contains("7 files"), "{out:?}");
        assert!(out.contains("4.00 MiB"), "{out:?}");
        assert!(out.contains("left"), "{out:?}");
    }

    #[test]
    fn track_returns_closure_value() {
        assert_eq!(track(Path::new("/no/such/tree"), || 7), 7);
    }

    #[test]
    fn draw_skips_bar_when_copy_beats_delay() {
        let (done, wait) = mpsc::channel();
        done.send(()).unwrap();

        assert!(draw_now(&wait, &AtomicU64::new(UNKNOWN)) < DELAY);
    }

    #[test]
    fn draw_stops_when_copy_goes_away() {
        let (done, wait) = mpsc::channel();
        drop(done);

        assert!(draw_now(&wait, &AtomicU64::new(UNKNOWN)) < DELAY);
    }

    #[test]
    fn draw_runs_until_copy_done() {
        let (done, wait) = mpsc::channel();
        let send = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DELAY + TICK);
            let _ = send.send(());
        });
        // A known total takes the length-setting branch; UNKNOWN skips it.
        let total = AtomicU64::new(4 << 20);

        let elapsed = draw_now(&wait, &total);

        assert!(elapsed >= DELAY, "{elapsed:?}");
    }
}
