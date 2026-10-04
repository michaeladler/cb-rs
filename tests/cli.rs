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

fn run(args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(args)
        // A throwaway state root, so no test reads or writes the real clipboard.
        .env("CLIPBOARD_PERSISTDIR", "/nonexistent/cb-cli-test-state")
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
    let got = expect(&["copy", "/nonexistent/cb-cli-test-src"], 1);
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
    expect(&["copy", "--", "/nonexistent/cb-cli-test-src"], 1);
}
