//! The man page and the completion scripts ship in the repository, not in the
//! binary. They are frozen artefacts now that nothing generates them from the
//! CLI at runtime, so check they are present and still describe cb.

use std::path::Path;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{relative} is missing or unreadable: {e}"))
}

#[test]
fn every_shell_has_a_completion_script() {
    for (file, marker) in [
        ("completions/cb.bash", "cb"),
        ("completions/_cb", "cb"),
        ("completions/cb.fish", "cb"),
        ("completions/cb.elv", "cb"),
        ("completions/_cb.ps1", "cb"),
    ] {
        assert!(read(file).contains(marker), "{file} is empty");
    }
}

#[test]
fn completions_cover_every_subcommand() {
    for file in [
        "completions/cb.bash",
        "completions/_cb",
        "completions/cb.fish",
        "completions/cb.elv",
        "completions/_cb.ps1",
    ] {
        let script = read(file);
        for sub in ["copy", "cut", "paste", "list"] {
            assert!(script.contains(sub), "{file} does not complete `{sub}`");
        }
        assert!(
            script.contains("amend"),
            "{file} does not complete `--amend`"
        );
    }
}

/// The README is the only place some of this is written down, so the facts that
/// already drifted once are pinned here: the state directory, the global `-n`,
/// `list`'s output shape, and the exit codes. Each assertion below is a claim
/// that was once wrong or missing, not a restatement of the prose.
#[test]
fn readme_states_the_contracts_it_used_to_get_wrong() {
    let readme = read("README.md");
    let paths = read("src/paths.rs");

    // The state directory is a constant in the code: check the two agree rather
    // than hardcoding "cb-rs" twice.
    let dir = paths
        .lines()
        .find_map(|l| l.split("STATE_DIR: &str = \"").nth(1))
        .and_then(|l| l.split('"').next())
        .expect("src/paths.rs does not define STATE_DIR");
    assert!(
        readme.contains(&format!("{dir}/<name>")),
        "README does not document the state directory {dir}/<name>:\n{readme}"
    );
    assert!(
        !readme.contains("clipboard/<name>"),
        "README still points at the C++ cb's state directory:\n{readme}"
    );

    for expected in [
        "-n`/`--name",
        "`--name <NAME>`",
        "`2` for a usage error",
        "`1` for a runtime failure",
        // The `list` sample carries real tabs, as `cb list` emits.
        "cut\t",
        "copy\t",
    ] {
        assert!(
            readme.contains(expected),
            "README is missing {expected:?}:\n{readme}"
        );
    }
}

#[test]
fn man_page_documents_the_commands() {
    let man = read("man/cb.1");
    for expected in [
        ".TH cb 1",
        "Cut, copy, and paste files",
        ".SH COMMANDS",
        ".SS cb paste",
        "on\\-conflict",
        "\\-\\-amend",
    ] {
        assert!(
            man.contains(expected),
            "man page missing {expected:?}:\n{man}"
        );
    }
    // Subcommand pages are inlined, never referenced: cb-copy(1) is not shipped.
    assert!(
        !man.contains("(1)"),
        "man page references a page it does not generate:\n{man}"
    );
}
