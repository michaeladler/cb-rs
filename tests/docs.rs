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
