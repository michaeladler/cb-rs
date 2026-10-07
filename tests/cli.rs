//! Golden checks for the command line: exit codes, and which stream each kind of
//! output goes to.
//!
//! Both are contractual and neither is covered by the library tests. cb exits 2
//! on a usage error and 1 on a runtime error, and sends usage errors to stderr —
//! argh defaults to 1 for both and to stdout, so both are restored by hand in
//! `main.rs`. This file fails if that stops happening, or if argh's leniency for
//! a repeated positional ever turns `cb copy` with no paths into a silent
//! success.

use std::process::Command;

#[derive(Debug)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// A path that cannot exist, so `copy` always has a source to fail on.
fn missing_source() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("cb-cli-absent-{}", std::process::id()))
}

fn run(args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(args)
        // A throwaway state root, so no test reads or writes the real clipboard.
        // Must be writable: `/nonexistent` is user-owned on some systems and
        // missing on others, and cb fails before reporting anything if it
        // cannot create the state directory.
        .env(
            "CLIPBOARD_PERSISTDIR",
            std::env::temp_dir().join(format!("cb-cli-state-{}", std::process::id())),
        )
        .output()
        .unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

#[track_caller]
fn expect(args: &[&str], code: i32) -> Run {
    let got = run(args);
    assert_eq!(got.code, code, "{args:?} exited {}:\n{got:?}", got.code);
    got
}

#[test]
fn version_is_reported() {
    for flag in ["--version", "-V"] {
        let got = expect(&[flag], 0);
        assert_eq!(
            got.stdout,
            concat!("cb-rs ", env!("CARGO_PKG_VERSION"), "\n")
        );
        assert!(got.stderr.is_empty());
    }
}

#[test]
fn help_is_reported_for_the_top_level_and_every_subcommand() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["copy", "--help"],
        vec!["copy", "-h"],
        vec!["paste", "--help"],
        vec!["cut", "-h"],
        vec!["list", "--help"],
    ] {
        let got = expect(&args, 0);
        assert!(!got.stdout.is_empty(), "{args:?} printed no help");
        assert!(got.stderr.is_empty(), "{args:?} wrote help to stderr");
    }
}

#[test]
fn help_output_carries_the_usage_line() {
    for (args, want) in [
        (vec!["--help"], "Usage: cb"),
        (vec!["paste", "--help"], "Usage: cb paste"),
        (vec!["paste", "--help"], "on-conflict"),
    ] {
        let got = run(&args);
        assert!(
            got.stdout.contains(want),
            "{args:?} help is missing {want:?}:\n{}",
            got.stdout
        );
    }
}

/// argh reads a `Vec` positional as zero-or-more, so without the check in
/// `require_paths` these would exit 0 reporting nothing copied.
#[test]
fn copy_and_cut_require_at_least_one_path() {
    for sub in ["copy", "cut"] {
        let got = expect(&[sub], 2);
        assert!(
            got.stderr.contains("required arguments were not provided"),
            "{sub} with no paths: {got:?}"
        );
        assert!(
            got.stdout.is_empty(),
            "{sub} usage error went to stdout: {got:?}"
        );
    }
}

/// argh accepts neither `--flag=value` nor `-nvalue`; `normalise` rewrites both.
#[test]
fn name_accepts_every_spelling() {
    for args in [
        vec!["-n", "probe", "list"],
        vec!["--name", "probe", "list"],
        vec!["--name=probe", "list"],
        vec!["-nprobe", "list"],
        // The flag is global, so it works after the subcommand too.
        vec!["list", "--name=probe"],
        vec!["list", "-n", "probe"],
    ] {
        let got = expect(&args, 0);
        assert!(got.stdout.is_empty(), "{args:?} printed {got:?}");
    }
}

#[test]
fn on_conflict_is_validated() {
    expect(&["paste", "--on-conflict", "replace"], 0);
    expect(&["paste", "--on-conflict=replace"], 0);
    let got = expect(&["paste", "--on-conflict=bogus"], 2);
    assert!(
        got.stderr.contains("skip") && got.stderr.contains("ask"),
        "expected the permitted values, got {got:?}"
    );
}

/// `--on-conflict` belongs to `paste` only.
#[test]
fn subcommands_reject_their_neighbours_flags() {
    let got = expect(&["copy", "--on-conflict=replace"], 2);
    assert!(got.stderr.contains("--on-conflict"), "{got:?}");
    let got = expect(&["copy", "-d", "/tmp"], 2);
    assert!(got.stderr.contains("-d"), "{got:?}");
}

#[test]
fn usage_errors_exit_2_and_go_to_stderr() {
    for args in [
        vec!["bogus"],
        vec!["pst"],
        vec!["list", "extra"],
        vec!["--name"],
        vec![], // no subcommand at all
    ] {
        let got = expect(&args, 2);
        assert!(
            !got.stderr.is_empty(),
            "{args:?} exited 2 with nothing on stderr"
        );
        assert!(
            got.stdout.is_empty(),
            "{args:?} put a usage error on stdout: {got:?}"
        );
    }
}

/// Runtime failures stay on exit 1, distinct from usage errors.
#[test]
fn runtime_failures_exit_1() {
    let got = expect(&["copy", &missing_source().to_string_lossy()], 1);
    assert!(got.stdout.contains("1 failed"), "{got:?}");
    assert!(got.stderr.starts_with("cb: "), "{got:?}");
}

/// The man page and completion scripts moved out of the binary and into the
/// repository; nothing generates them at runtime any more.
#[test]
fn doc_generators_are_gone() {
    for args in [
        vec!["man"],
        vec!["completions"],
        vec!["completions", "bash"],
    ] {
        expect(&args, 2);
    }
}

#[test]
fn double_dash_ends_option_parsing() {
    expect(&["copy", "--", &missing_source().to_string_lossy()], 1);
}

/// A clipboard is two path lists, so one paste can move some entries and copy
/// others. Nothing is read at record time, which is what makes this cheap
/// enough to be the normal case.
struct Sandbox {
    root: std::path::PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["work", "out", "state"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("work/moved.txt"), b"m").unwrap();
        std::fs::create_dir_all(root.join("work/kept")).unwrap();
        std::fs::write(root.join("work/kept/k.txt"), b"k").unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cb"))
            .args(args)
            .env("CLIPBOARD_PERSISTDIR", self.root.join("state"))
            .current_dir(self.root.join("work"))
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> std::process::Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn work(&self, rel: &str) -> bool {
        self.root.join("work").join(rel).exists()
    }

    fn out(&self, rel: &str) -> bool {
        self.root.join("out").join(rel).exists()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn amend_accumulates_a_move_and_a_copy_into_one_paste() {
    let sandbox = Sandbox::new("amend");

    sandbox.ok(&["cut", "moved.txt"]);
    sandbox.ok(&["copy", "--amend", "kept"]);
    sandbox.ok(&["paste", "-d", "../out"]);

    assert!(!sandbox.work("moved.txt"), "cut entry must be moved");
    assert!(
        sandbox.out("moved.txt"),
        "cut entry must land in the destination"
    );
    assert!(sandbox.work("kept/k.txt"), "copy entry must stay put");
    assert!(
        sandbox.out("kept/k.txt"),
        "copy entry must land in the destination"
    );
}

/// "more" is only true when there was something to add to, so amending an
/// empty clipboard does not claim it added to one.
#[test]
fn amend_reports_more_only_when_the_clipboard_had_entries() {
    let sandbox = Sandbox::new("amend-more");

    let first = sandbox.ok(&["copy", "--amend", "kept"]);
    assert_eq!(
        String::from_utf8_lossy(&first.stdout).trim(),
        "copy 1 item(s), will copy on paste"
    );

    let second = sandbox.ok(&["copy", "--amend", "moved.txt"]);
    assert_eq!(
        String::from_utf8_lossy(&second.stdout).trim(),
        "copy 2 more item(s), will copy on paste"
    );
}

/// Without `--amend` a copy replaces the whole clipboard, pending move included.
/// This is the documented wipe, and it is the reason `--amend` exists.
#[test]
fn a_plain_copy_discards_the_pending_move() {
    let sandbox = Sandbox::new("replace");

    sandbox.ok(&["cut", "moved.txt"]);
    sandbox.ok(&["copy", "kept"]);
    sandbox.ok(&["paste", "-d", "../out"]);

    assert!(
        sandbox.work("moved.txt"),
        "the cut was replaced, so nothing moves"
    );
    assert!(
        !sandbox.out("moved.txt"),
        "a replaced move must not be pasted"
    );
    assert!(sandbox.work("kept/k.txt"));
    assert!(sandbox.out("kept/k.txt"));
}

/// A copy stays recorded, so a second paste reads the source again rather than
/// finding an empty clipboard.
#[test]
fn a_copy_is_not_consumed_by_pasting() {
    let sandbox = Sandbox::new("repeat");

    sandbox.ok(&["copy", "kept"]);
    sandbox.ok(&["paste", "-d", "../out"]);
    sandbox.ok(&["paste", "-d", "../out", "--on-conflict", "replace"]);

    assert_eq!(
        std::fs::read(sandbox.root.join("out/kept/k.txt")).unwrap(),
        b"k"
    );
}

/// The default state layout is this tool's own contract with anyone scripting
/// against it, and it moved off the C++ `cb`'s directory so a cut here cannot
/// wipe the other tool's staged bytes. `CLIPBOARD_PERSISTDIR` replaces the root
/// wholesale, as it always has, so only the default path carries `cb-rs/`.
#[test]
fn default_state_lives_under_its_own_directory() {
    let sandbox = Sandbox::new("state");
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(["copy", "kept"])
        .env_remove("CLIPBOARD_PERSISTDIR")
        .env("XDG_STATE_HOME", sandbox.root.join("state"))
        .current_dir(sandbox.root.join("work"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);

    let state = sandbox.root.join("state");
    assert!(
        state.join("cb-rs/0/metadata/copies").exists(),
        "expected the clipboard under state/cb-rs, found: {:?}",
        std::fs::read_dir(&state).map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        })
    );
    assert!(
        !state.join("clipboard").exists(),
        "the C++ cb's directory must not be used"
    );
}

/// `-d` is created like `mkdir -p`, whole missing parents included. A failure
/// is still reported once, naming the destination: left to the per-entry path
/// it produced one "No such file or directory" per source, which blames the
/// source and sends the user looking in the wrong place.
#[test]
fn paste_creates_the_destination_and_names_a_blocked_one_once() {
    let sandbox = Sandbox::new("nodest");
    sandbox.ok(&["copy", "kept", "moved.txt"]);

    let deep = sandbox.root.join("deep/nested/dest");
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(["paste", "-d"])
        .arg(&deep)
        .env("CLIPBOARD_PERSISTDIR", sandbox.root.join("state"))
        .current_dir(sandbox.root.join("out"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(deep.join("kept/k.txt").exists());
    assert!(deep.join("moved.txt").exists());

    // A file in the way is not a directory and cannot become one.
    let blocked = sandbox.root.join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(["paste", "-d"])
        .arg(&blocked)
        .env("CLIPBOARD_PERSISTDIR", sandbox.root.join("state"))
        .current_dir(sandbox.root.join("out"))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert_eq!(
        err.matches("File exists").count(),
        1,
        "the error must be reported once: {err}"
    );
    assert!(err.contains("blocked"), "{err}");
}

/// Paths are stored absolute, because paste runs in a directory the user never
/// named. A relative path recorded from `work` would not resolve from `out`.
#[test]
fn recorded_paths_do_not_depend_on_the_paste_directory() {
    let sandbox = Sandbox::new("absolute");

    sandbox.ok(&["copy", "kept"]);
    let dest = sandbox.root.join("elsewhere");
    std::fs::create_dir_all(&dest).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(["paste"])
        .env("CLIPBOARD_PERSISTDIR", sandbox.root.join("state"))
        .current_dir(&dest)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dest.join("kept/k.txt").exists());
}
