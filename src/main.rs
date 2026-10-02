use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};

use cb_rs::mover;
use cb_rs::paths::{self, Clipboard};
use cb_rs::policy::Policy;
use cb_rs::walk;
use cb_rs::walk::Failure;

#[derive(Parser)]
#[command(about = "Cut, copy, and paste files", version)]
struct Cli {
    /// Clipboard to use, instead of the default.
    #[arg(short, long, global = true)]
    name: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy files into the clipboard, leaving the originals in place.
    Copy {
        #[arg(required = true, action = ArgAction::Append)]
        paths: Vec<PathBuf>,
    },
    /// Record files to be moved when pasted. The originals stay in place until
    /// then.
    Cut {
        #[arg(required = true, action = ArgAction::Append)]
        paths: Vec<PathBuf>,
    },
    /// Write the clipboard's files into the current directory.
    Paste {
        /// Destination directory, defaults to the current one.
        #[arg(short, long)]
        directory: Option<PathBuf>,
        /// What to do when a destination file already exists.
        #[arg(long, default_value = "skip")]
        on_conflict: Policy,
    },
    /// List what the clipboard holds.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let clipboard = Clipboard::open(cli.name.as_deref().unwrap_or(paths::DEFAULT_NAME));

    let result = match cli.command {
        Command::Copy { paths } => do_copy(&clipboard, &paths),
        Command::Cut { paths } => do_cut(&clipboard, &paths),
        Command::Paste {
            directory,
            on_conflict,
        } => do_paste(
            &clipboard,
            directory.as_deref().unwrap_or(Path::new(".")),
            on_conflict,
        ),
        Command::List => do_list(&clipboard),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cb: {err}");
            ExitCode::FAILURE
        }
    }
}

fn do_copy(clipboard: &Clipboard, items: &[PathBuf]) -> Result<(), String> {
    clipboard.reset().map_err(|e| e.to_string())?;
    let mut failures = Vec::new();
    let mut copied = 0usize;
    for item in items {
        let name = item
            .file_name()
            .ok_or_else(|| format!("{}: not a path", item.display()))?;
        let dst = clipboard.data().join(name);
        let before = failures.len();
        failures.extend(walk::copy_any(item, &dst));
        if failures.len() == before {
            copied += 1;
        }
    }
    report(copied, &failures, "copied")
}

fn do_cut(clipboard: &Clipboard, items: &[PathBuf]) -> Result<(), String> {
    let mut sources = Vec::new();
    for item in items {
        let absolute =
            std::fs::canonicalize(item).map_err(|e| format!("{}: {e}", item.display()))?;
        sources.push(absolute);
    }
    clipboard.reset().map_err(|e| e.to_string())?;
    clipboard
        .set_cut_sources(&sources)
        .map_err(|e| e.to_string())?;
    println!("cut {} item(s), will move on paste", sources.len());
    Ok(())
}

fn do_paste(clipboard: &Clipboard, dst_dir: &Path, policy: Policy) -> Result<(), String> {
    let cut = clipboard.cut_sources();
    if cut.is_empty() {
        return paste_copied(clipboard, dst_dir, policy);
    }
    paste_cut(clipboard, &cut, dst_dir, policy)
}

fn paste_copied(clipboard: &Clipboard, dst_dir: &Path, policy: Policy) -> Result<(), String> {
    let entries = match std::fs::read_dir(clipboard.data()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let mut failures = Vec::new();
    let mut skipped = 0usize;
    let mut pasted = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let dst = dst_dir.join(entry.file_name());
        if dst.exists() && !policy.resolve(&dst).map_err(|e| e.to_string())? {
            skipped += 1;
            continue;
        }
        let before = failures.len();
        failures.extend(walk::copy_any(&entry.path(), &dst));
        if failures.len() == before {
            pasted += 1;
        }
    }
    if skipped > 0 {
        println!("skipped {skipped} existing item(s)");
    }
    report(pasted, &failures, "pasted")
}

fn paste_cut(
    clipboard: &Clipboard,
    sources: &[PathBuf],
    dst_dir: &Path,
    policy: Policy,
) -> Result<(), String> {
    let mut remaining = Vec::new();
    let mut moved = 0usize;
    let mut skipped = 0usize;
    for source in sources {
        match mover::move_into(source, dst_dir, policy) {
            Ok(mover::Outcome::Moved) => moved += 1,
            Ok(mover::Outcome::Skipped) => {
                skipped += 1;
                remaining.push(source.clone());
            }
            Err(e) => {
                eprintln!("cb: {}: {e}", source.display());
                remaining.push(source.clone());
            }
        }
    }
    // Only paths that did not complete stay recorded, so `paste` can be retried.
    clipboard
        .set_cut_sources(&remaining)
        .map_err(|e| e.to_string())?;
    if skipped > 0 {
        println!("skipped {skipped} existing item(s)");
    }
    println!("moved {moved} item(s)");
    Ok(())
}

fn do_list(clipboard: &Clipboard) -> Result<(), String> {
    let cut = clipboard.cut_sources();
    if !cut.is_empty() {
        for source in cut {
            println!("cut\t{}", source.display());
        }
        return Ok(());
    }
    let entries = match std::fs::read_dir(clipboard.data()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        println!("{}", entry.path().display());
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
