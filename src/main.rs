use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use argh::FromArgs;

use cb_rs::mover;
use cb_rs::paths::{self, Clipboard};
use cb_rs::policy::Policy;
use cb_rs::walk::Failure;

// argh has no global options, so `-n/--name` is declared on the top level and on
// every subcommand. That reproduces clap's `global = true`: the flag is accepted
// before or after the subcommand.
//
// These fields cannot come from a `macro_rules!`, since derive macros do not
// expand macro invocations in field position.
//
// The comment above is `//` and not a doc comment on purpose: argh turns a doc
// comment on the struct into the command description in `--help`.
#[derive(FromArgs)]
/// Cut, copy, and paste files
#[argh(help_triggers("-h", "--help", "help"))]
struct Cli {
    #[argh(subcommand)]
    command: Command,

    /// clipboard to use, instead of the default.
    #[argh(option, short = 'n')]
    name: Option<String>,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Command {
    Copy(Copy),
    Cut(Cut),
    Paste(Paste),
    List(List),
}

impl Command {
    fn name(&self) -> Option<&String> {
        match self {
            Self::Copy(c) => c.name.as_ref(),
            Self::Cut(c) => c.name.as_ref(),
            Self::Paste(c) => c.name.as_ref(),
            Self::List(c) => c.name.as_ref(),
        }
    }
}

#[derive(FromArgs)]
/// Record files to be copied when pasted, leaving the originals in place
#[argh(subcommand, name = "copy", help_triggers("-h", "--help"))]
struct Copy {
    /// clipboard to use, instead of the default.
    #[argh(option, short = 'n')]
    name: Option<String>,

    /// add to the recorded paths instead of replacing them.
    #[argh(switch, short = 'a')]
    amend: bool,

    /// files or directories to copy.
    #[argh(positional)]
    paths: Vec<PathBuf>,
}

#[derive(FromArgs)]
/// Record files to be moved when pasted. The originals stay in place until then
#[argh(subcommand, name = "cut", help_triggers("-h", "--help"))]
struct Cut {
    /// clipboard to use, instead of the default.
    #[argh(option, short = 'n')]
    name: Option<String>,

    /// add to the recorded paths instead of replacing them.
    #[argh(switch, short = 'a')]
    amend: bool,

    /// files or directories to move on paste.
    #[argh(positional)]
    paths: Vec<PathBuf>,
}

#[derive(FromArgs)]
/// Write the clipboard's files into the current directory
#[argh(subcommand, name = "paste", help_triggers("-h", "--help"))]
struct Paste {
    /// destination directory, defaults to the current one.
    #[argh(option, short = 'd')]
    directory: Option<PathBuf>,

    /// clipboard to use, instead of the default.
    #[argh(option, short = 'n')]
    name: Option<String>,

    /// what to do when a destination file already exists.
    #[argh(option, default = "Policy::Skip", from_str_fn(parse_policy))]
    on_conflict: Policy,
}

#[derive(FromArgs)]
/// List what the clipboard holds
#[argh(subcommand, name = "list", help_triggers("-h", "--help"))]
struct List {
    /// clipboard to use, instead of the default.
    #[argh(option, short = 'n')]
    name: Option<String>,
}

fn parse_policy(value: &str) -> Result<Policy, String> {
    value
        .parse()
        .map_err(|_| "expected \"skip\", \"replace\" or \"ask\"".into())
}

/// argh accepts neither `--flag=value` nor an attached short value (`-nfoo`),
/// both of which cb accepted under clap. Rewrite them into the two-token form
/// before argh sees them.
fn normalise(argv: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut literals = false;
    for arg in argv {
        if literals {
            out.push(arg);
        } else if arg == "--" {
            literals = true;
            out.push(arg);
        } else if let Some((flag, value)) = arg.split_once('=').filter(|_| arg.starts_with("--")) {
            out.push(flag.to_string());
            out.push(value.to_string());
        } else if (arg.starts_with("-n") || arg.starts_with("-d"))
            && let Some((split, _)) = arg.char_indices().nth(2)
        {
            let (flag, value) = arg.split_at(split);
            out.push(flag.to_string());
            out.push(value.to_string());
        } else {
            out.push(arg);
        }
    }
    out
}

/// argh has no `--version` (the Fuchsia spec it follows has none) and exits 1 on
/// every usage error, where cb has always exited 2. `EarlyExit` carries the
/// message and whether it was an error, so both are handled here.
fn parse_args(argv: Vec<String>) -> Result<Cli, ExitCode> {
    if argv.first().is_some_and(|a| a == "--version" || a == "-V") {
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "cb-rs {}", env!("CARGO_PKG_VERSION"));
        return Err(ExitCode::SUCCESS);
    }
    let argv = normalise(argv);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    match Cli::from_args(&["cb"], &argv) {
        Ok(cli) => Ok(cli),
        Err(exit) => {
            // argh sends everything to stdout; usage errors belong on stderr.
            if exit.status.is_err() {
                let _ = std::io::stderr().write_all(exit.output.as_bytes());
                Err(ExitCode::from(2))
            } else {
                let _ = std::io::stdout().write_all(exit.output.as_bytes());
                Err(ExitCode::SUCCESS)
            }
        }
    }
}

/// argh reads a `Vec` positional as zero-or-more, so `cb copy` with no paths
/// would otherwise parse clean and report success having copied nothing.
fn require_paths(sub: &str, paths: &[PathBuf]) -> Result<(), ExitCode> {
    if !paths.is_empty() {
        return Ok(());
    }
    let _ = std::io::stderr().write_all(
        format!(
            "error: the following required arguments were not provided:\n  \
             <PATHS>...\n\nUsage: cb {sub} <PATHS>...\n\n\
             For more information, try '--help'.\n"
        )
        .as_bytes(),
    );
    Err(ExitCode::from(2))
}

fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1).collect()) {
        Ok(cli) => cli,
        Err(code) => return code,
    };

    let name = cli.command.name().or(cli.name.as_ref());
    let clipboard = match Clipboard::open(name.map_or(paths::DEFAULT_NAME, String::as_str)) {
        Ok(clipboard) => clipboard,
        Err(error) => {
            eprintln!("cb: {error}");
            return ExitCode::FAILURE;
        }
    };

    let result = match cli.command {
        Command::Copy(c) => match require_paths("copy", &c.paths) {
            Ok(()) => record(
                &clipboard,
                clipboard.copies(),
                &c.paths,
                c.amend,
                "copy",
                "copy",
            ),
            Err(code) => return code,
        },
        Command::Cut(c) => match require_paths("cut", &c.paths) {
            Ok(()) => record(
                &clipboard,
                clipboard.originals(),
                &c.paths,
                c.amend,
                "cut",
                "move",
            ),
            Err(code) => return code,
        },
        Command::Paste(p) => do_paste(
            &clipboard,
            p.directory.as_deref().unwrap_or(Path::new(".")),
            p.on_conflict,
        ),
        Command::List(_) => do_list(&clipboard),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cb: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The absolute path recorded for `item`: its parent made absolute and
/// canonical, with the original file name joined back on unread.
fn absolute_source(item: &Path) -> std::io::Result<PathBuf> {
    // Existence check that does not resolve the final component, so the error
    // for a missing source still names it.
    item.symlink_metadata()?;
    match (item.parent(), item.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            Ok(parent.canonicalize()?.join(name))
        }
        // A root path has no parent to canonicalize away; take it whole.
        _ => item.canonicalize(),
    }
}

/// `cut` and `copy` store the same thing: absolute source paths. They differ
/// only in which list the paths land in, so `paste` knows to move or to copy
/// them. Nothing is read here, so a recorded clipboard costs a few bytes per
/// path no matter how large the tree is.
fn record(
    clipboard: &Clipboard,
    file: PathBuf,
    items: &[PathBuf],
    amend: bool,
    verb: &str,
    future: &str,
) -> Result<(), String> {
    let mut sources = Vec::with_capacity(items.len());
    let mut errors = Vec::new();
    for item in items {
        // Absolute, because paste runs somewhere else: the directory a path was
        // given in is not the one it will be resolved from. Only the parent is
        // canonicalized: canonicalizing the item would follow a symlink and
        // record its target, so `cut mylink` would move the target under the
        // target's name. Joining the name back on keeps the link itself, which
        // is what the walker recreates.
        match absolute_source(item) {
            Ok(path) => sources.push(path),
            Err(e) => errors.push(format!("{}: {e}", item.display())),
        }
    }

    if sources.is_empty() {
        for error in &errors {
            eprintln!("cb: {error}");
        }
        println!(
            "{verb} 0 item(s), will {future} on paste, {} failed",
            errors.len()
        );
        return Err(format!("{} failure(s)", errors.len()));
    }

    // Guards the reset, the amend and the write below as one step.
    let _lock = clipboard.lock().map_err(|e| e.to_string())?;
    let mut list = if amend {
        clipboard.read_list(&file)
    } else {
        clipboard.reset().map_err(|e| e.to_string())?;
        Vec::new()
    };
    // "more" only when there was something to add to.
    let added_to_existing = amend && !list.is_empty();
    let recorded = sources.len();
    list.append(&mut sources);
    clipboard
        .write_list(&file, &list)
        .map_err(|e| e.to_string())?;

    for error in &errors {
        eprintln!("cb: {error}");
    }
    let failed = if errors.is_empty() {
        String::new()
    } else {
        format!(", {} failed", errors.len())
    };
    let more = if added_to_existing { " more" } else { "" };
    println!("{verb} {recorded}{more} item(s), will {future} on paste{failed}");
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} failure(s)", errors.len()))
    }
}

fn do_paste(clipboard: &Clipboard, dst_dir: &Path, policy: Policy) -> Result<(), String> {
    // The lock covers only reading the lists. Holding it across the paste would
    // block every other `cb` command for the length of the copy; the rewrite
    // that consumes a move goes through `Clipboard::consume`, which takes the
    // lock again and merges consumed paths into any newer list.
    let (moves, copies) = read_lists(clipboard)?;
    if moves.is_empty() && copies.is_empty() {
        println!("clipboard is empty");
        return Ok(());
    }
    // Create destination once, mkdir -p style, so a failure is reported once
    // against destination rather than once per source.
    if let Err(e) = std::fs::create_dir_all(dst_dir) {
        return Err(format!("{}: {e}", dst_dir.display()));
    }
    let mut result = Ok(());
    if !moves.is_empty() {
        result = paste_moves(clipboard, dst_dir, policy, &moves);
    }
    if !copies.is_empty()
        && let Err(e) = paste_copies(dst_dir, policy, &copies)
    {
        result = Err(e);
    }
    result
}

/// Moves consume the clipboard: only the paths that did not complete stay
/// recorded, so an interrupted paste can be retried.
fn paste_moves(
    clipboard: &Clipboard,
    dst_dir: &Path,
    policy: Policy,
    sources: &[PathBuf],
) -> Result<(), String> {
    let mut remaining = Vec::new();
    let mut moved = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for source in sources {
        let outcome = mover::move_into(source, dst_dir, policy);
        if stays_recorded(&outcome) {
            remaining.push(source.clone());
        }
        match outcome {
            Ok(mover::Outcome::Moved) => moved += 1,
            Ok(mover::Outcome::MovedWithLeftover(leftover)) => {
                eprintln!("cb: {}: {}", leftover.path.display(), leftover.reason);
                moved += 1;
                failed += 1;
            }
            Ok(mover::Outcome::Skipped) => skipped += 1,
            Err(failures) => {
                for failure in &failures {
                    eprintln!("cb: {}: {}", failure.path.display(), failure.reason);
                }
                failed += 1;
            }
        }
    }
    let consume_error = match clipboard.consume(&clipboard.originals(), sources, &remaining) {
        Ok(true) => None,
        Ok(false) => {
            eprintln!("cb: clipboard changed during paste, merged consumed paths");
            None
        }
        Err(e) => Some(e.to_string()),
    };
    if skipped > 0 {
        println!("skipped {skipped} existing item(s)");
    }
    println!("moved {moved} item(s)");
    if let Some(error) = consume_error {
        return Err(error);
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} failure(s)"))
    }
}

/// Whether a cut entry stays recorded after one move attempt.
///
/// Only a move that landed consumes the entry. A skip leaves the source where it
/// is, and a failure before the destination held the whole tree leaves a source
/// that is still worth pasting again.
///
/// A move that landed but could not unlink its source is done, though: the
/// destination has the whole tree and what is left of the source is a leftover.
/// Pasting the entry again would copy that fragment over the destination and
/// replace it, losing everything the fragment no longer has.
fn stays_recorded(outcome: &Result<mover::Outcome, Vec<Failure>>) -> bool {
    matches!(outcome, Err(_) | Ok(mover::Outcome::Skipped))
}

/// Copies do not consume the clipboard, so a second paste reads the same sources
/// again. That is only sound because the source is read at paste time and not
/// snapshotted at copy time.
fn paste_copies(dst_dir: &Path, policy: Policy, sources: &[PathBuf]) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut skipped = 0usize;
    let mut pasted = 0usize;
    for source in sources {
        // Staged and renamed in, so a destination that appears between the check
        // above the copy and the write below it is declined or replaced whole
        // rather than merged into.
        match mover::copy_into(source, dst_dir, policy) {
            // A copy consumes nothing, so there is no source left to remove.
            Ok(mover::CopyOutcome::Copied) => pasted += 1,
            Ok(mover::CopyOutcome::Skipped) => skipped += 1,
            Err(mut copy_failures) => failures.append(&mut copy_failures),
        }
    }
    if skipped > 0 {
        println!("skipped {skipped} existing item(s)");
    }
    report(pasted, &failures, "pasted")
}

fn read_lists(clipboard: &Clipboard) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let _lock = clipboard.lock().map_err(|e| e.to_string())?;
    Ok((
        clipboard.read_list(&clipboard.originals()),
        clipboard.read_list(&clipboard.copies()),
    ))
}

fn do_list(clipboard: &Clipboard) -> Result<(), String> {
    let (moves, copies) = read_lists(clipboard)?;
    for source in moves {
        println!("cut\t{}", source.display());
    }
    for source in copies {
        println!("copy\t{}", source.display());
    }
    Ok(())
}

fn report(succeeded: usize, failures: &[Failure], verb: &str) -> Result<(), String> {
    for failure in failures {
        eprintln!("cb: {}: {}", failure.path.display(), failure.reason);
    }
    println!("{verb} {succeeded} item(s), {} failed", failures.len());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} failure(s)", failures.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_paste_leaves_requested_destination_uncreated() {
        let root = std::env::temp_dir().join(format!("cb-empty-paste-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dst = root.join("not-created");
        let clipboard = Clipboard {
            root: root.join("state"),
        };

        do_paste(&clipboard, &dst, Policy::Skip).unwrap();

        assert!(!dst.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normalise_splits_only_attached_value_options_at_char_boundaries() {
        assert_eq!(
            normalise(
                ["-nprobe", "-d/tmp", "-an", "-weird", "-évalue"]
                    .map(str::to_owned)
                    .to_vec()
            ),
            ["-n", "probe", "-d", "/tmp", "-an", "-weird", "-évalue"]
                .map(str::to_owned)
                .to_vec()
        );
    }

    fn leftover() -> Failure {
        Failure {
            path: PathBuf::from("/src/kept"),
            reason: "moved, but the source could not be removed whole".to_owned(),
        }
    }

    #[test]
    fn a_move_consumes_its_entry() {
        assert!(!stays_recorded(&Ok(mover::Outcome::Moved)));
    }

    /// The source was already half removed when the unlink failed, so the next
    /// paste copied what was left of it and replaced the complete destination
    /// with that fragment.
    #[test]
    fn a_move_that_left_a_leftover_still_consumes_its_entry() {
        assert!(
            !stays_recorded(&Ok(mover::Outcome::MovedWithLeftover(leftover()))),
            "the destination already holds the whole tree"
        );
    }

    #[test]
    fn an_unfinished_move_stays_recorded() {
        assert!(stays_recorded(&Ok(mover::Outcome::Skipped)));
        let failed: Result<mover::Outcome, Vec<Failure>> = Err(vec![leftover()]);
        assert!(
            stays_recorded(&failed),
            "the source is whole and can be retried"
        );
    }
}
