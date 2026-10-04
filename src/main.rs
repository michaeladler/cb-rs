use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use argh::FromArgs;

use cb_rs::mover;
use cb_rs::paths::{self, Clipboard};
use cb_rs::policy::Policy;
use cb_rs::walk;
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
        } else if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            out.push(arg[..2].to_string());
            out.push(arg[2..].to_string());
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
    let argv = normalise(argv);
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "cb-rs {}", env!("CARGO_PKG_VERSION"));
        return Err(ExitCode::SUCCESS);
    }
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
    let clipboard = Clipboard::open(name.map_or(paths::DEFAULT_NAME, String::as_str));

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
        // given in is not the one it will be resolved from.
        match std::fs::canonicalize(item) {
            Ok(path) => sources.push(path),
            Err(e) => errors.push(format!("{}: {e}", item.display())),
        }
    }

    let mut list = if amend {
        clipboard.read_list(&file)
    } else {
        clipboard.reset().map_err(|e| e.to_string())?;
        Vec::new()
    };
    let recorded = list.len() + sources.len();
    list.append(&mut sources);
    clipboard
        .write_list(&file, &list)
        .map_err(|e| e.to_string())?;

    for error in &errors {
        eprintln!("cb: {error}");
    }
    let more = if amend { " more" } else { "" };
    let failed = if errors.is_empty() {
        String::new()
    } else {
        format!(", {} failed", errors.len())
    };
    println!("{verb} {recorded}{more} item(s), will {future} on paste{failed}");
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} failure(s)", errors.len()))
    }
}

fn do_paste(clipboard: &Clipboard, dst_dir: &Path, policy: Policy) -> Result<(), String> {
    let moves = clipboard.read_list(&clipboard.originals());
    let copies = clipboard.read_list(&clipboard.copies());
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
        match mover::move_into(source, dst_dir, policy) {
            Ok(mover::Outcome::Moved) => moved += 1,
            Ok(mover::Outcome::Skipped) => {
                skipped += 1;
                remaining.push(source.clone());
            }
            Err(e) => {
                eprintln!("cb: {}: {e}", source.display());
                failed += 1;
                remaining.push(source.clone());
            }
        }
    }
    clipboard
        .write_list(&clipboard.originals(), &remaining)
        .map_err(|e| e.to_string())?;
    if skipped > 0 {
        println!("skipped {skipped} existing item(s)");
    }
    println!("moved {moved} item(s)");
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} failure(s)"))
    }
}

/// Copies do not consume the clipboard, so a second paste reads the same sources
/// again. That is only sound because the source is read at paste time and not
/// snapshotted at copy time.
fn paste_copies(dst_dir: &Path, policy: Policy, sources: &[PathBuf]) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut skipped = 0usize;
    let mut pasted = 0usize;
    for source in sources {
        let Some(name) = source.file_name() else {
            failures.push(Failure {
                path: source.clone(),
                reason: "no file name".to_owned(),
            });
            continue;
        };
        let dst = dst_dir.join(name);
        if dst.exists() && !policy.resolve(&dst).map_err(|e| e.to_string())? {
            skipped += 1;
            continue;
        }
        let before = failures.len();
        failures.extend(walk::copy_any(source, &dst));
        if failures.len() == before {
            pasted += 1;
        }
    }
    if skipped > 0 {
        println!("skipped {skipped} existing item(s)");
    }
    report(pasted, &failures, "pasted")
}

fn do_list(clipboard: &Clipboard) -> Result<(), String> {
    for source in clipboard.read_list(&clipboard.originals()) {
        println!("cut\t{}", source.display());
    }
    for source in clipboard.read_list(&clipboard.copies()) {
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
