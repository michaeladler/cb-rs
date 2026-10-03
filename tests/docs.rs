//! `cb completions` and `cb man` must emit their output and exit 0, without
//! needing a clipboard.

use std::process::Command;

fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_cb"))
        .args(args)
        // A bogus state dir would be created if a clipboard were opened.
        .env("CLIPBOARD_PERSISTDIR", "/nonexistent/cb-test-state")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?} exited {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn every_shell_completes() {
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let script = run(&["completions", shell]);
        assert!(script.contains("cb"), "{shell} script is empty: {script:?}");
    }
}

#[test]
fn man_page_documents_the_commands() {
    let man = run(&["man"]);
    for expected in [
        ".TH cb 1",
        "Cut, copy, and paste files",
        "COMMANDS",
        "cb\\-paste(1)",
    ] {
        assert!(
            man.contains(expected),
            "man page missing {expected:?}:\n{man}"
        );
    }
}
