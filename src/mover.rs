use std::ffi::OsStr;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use rustix::fs::{AtFlags, CWD, RenameFlags, linkat, renameat, renameat_with, statat, unlinkat};
use rustix::io::{Errno, Result as IoResult};

use crate::copy;
use crate::policy::Policy;
use crate::walk;
use crate::walk::Failure;

/// What became of one move attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Moved,
    /// Destination exists and policy said leave it alone.
    Skipped,
}

/// Move `src` to `dst_dir/<basename>`.
///
/// Same filesystem: one `renameat2`. Different filesystems: copy to a private
/// staging path, rename that onto the destination, and only then unlink the
/// source. The C++ version deletes originals after a copy it never re-checked,
/// which loses data if the copy silently came up short.
pub fn move_into(src: &Path, dst_dir: &Path, policy: Policy) -> Result<Outcome, Vec<Failure>> {
    let name = src.file_name().ok_or_else(|| {
        vec![Failure {
            path: src.to_path_buf(),
            reason: "no file name".to_owned(),
        }]
    })?;
    let dst = dst_dir.join(name);

    // Pasting into the folder the source already lives in. `renameat2` reports
    // `EXIST`, and with `--on-conflict replace` that would empty the source
    // before a rename that then does nothing.
    if walk::same_file(src, &dst).unwrap_or(false) {
        return Ok(Outcome::Skipped);
    }
    // Pasting a directory into its own subtree: `rename` answers `EINVAL`, which
    // must not be read as "no renameat2 here" and turned into a copy-then-delete
    // that copies the source into itself until the disk fills.
    if walk::inside_source(src, &dst).unwrap_or(false) {
        return Err(one(src, Errno::INVAL));
    }

    match rename_noreplace(src, &dst) {
        Ok(()) => return Ok(Outcome::Moved),
        Err(Errno::EXIST | Errno::NOTEMPTY | Errno::ISDIR) => {
            if !policy.resolve(&dst).map_err(|e| one(src, e))? {
                return Ok(Outcome::Skipped);
            }
            return replace_by_rename(src, &dst)
                .map(|()| Outcome::Moved)
                .map_err(|e| one(src, e));
        }
        // Cross-device, or no `renameat2`: fall back to copy-then-delete.
        // `EINVAL` is deliberately not here: it is what `rename` answers when
        // the destination is inside the source, and copying there instead of
        // failing is how the source ends up duplicated inside itself. The
        // filesystem-does-not-support-the-flag case is reported by
        // `rename_noreplace` as `OPNOTSUPP`.
        Err(Errno::XDEV | Errno::NOSYS | Errno::OPNOTSUPP) => {}
        Err(e) => return Err(one(src, e)),
    }

    // A cross-device move copies into a private sibling of the destination and
    // renames that onto place, so a destination that appears while the copy
    // runs is only ever replaced by a finished tree.
    let staged = walk::staged_path(&dst).map_err(|e| one(src, e))?;
    let outcome = stage_and_commit(src, &dst, &staged, policy);
    if matches!(outcome, Ok(Outcome::Moved)) {
        // The source goes only after the destination is on disk. A copy that came
        // up short leaves the source in place, which is the whole point.
        walk::remove_any(src).map_err(|e| one(src, e))?;
    }
    outcome
}

/// Copy `src` into `dst_dir`, like [`move_into`] but leaving the source alone.
///
/// Staged and renamed in like a move, so the policy is consulted against the
/// destination twice: once before the copy and once after it. A destination that
/// appears while the copy runs is then either declined or replaced whole, never
/// merged into.
pub fn copy_into(src: &Path, dst_dir: &Path, policy: Policy) -> Result<Outcome, Vec<Failure>> {
    let name = src.file_name().ok_or_else(|| {
        vec![Failure {
            path: src.to_path_buf(),
            reason: "no file name".to_owned(),
        }]
    })?;
    let dst = dst_dir.join(name);
    if walk::same_file(src, &dst).unwrap_or(false) {
        return Ok(Outcome::Skipped);
    }
    if walk::inside_source(src, &dst).unwrap_or(false) {
        return Err(vec![Failure {
            path: src.to_path_buf(),
            reason: "destination is inside the source".to_owned(),
        }]);
    }
    if exists(&dst).map_err(|e| one(src, e))? && !policy.resolve(&dst).map_err(|e| one(src, e))? {
        return Ok(Outcome::Skipped);
    }
    let staged = walk::staged_path(&dst).map_err(|e| one(src, e))?;
    stage_and_commit(src, &dst, &staged, policy)
}

/// Copy `src` to `staged`, then rename it onto `dst`. A staged tree that did not
/// reach the destination is removed here rather than by the caller, which cannot
/// tell which outcome it got before looking.
fn stage_and_commit(
    src: &Path,
    dst: &Path,
    staged: &Path,
    policy: Policy,
) -> Result<Outcome, Vec<Failure>> {
    let result = copy_then_commit(src, dst, staged, policy);
    if !matches!(result, Ok(Outcome::Moved)) {
        let _ = walk::remove_any(staged);
    }
    result
}

fn copy_then_commit(
    src: &Path,
    dst: &Path,
    staged: &Path,
    policy: Policy,
) -> Result<Outcome, Vec<Failure>> {
    let failures = walk::copy_any(src, staged);
    if !failures.is_empty() {
        return Err(failures);
    }
    // Re-read rather than trusting the answer from above the copy: the staged
    // tree went in under a private name, so only now does it overwrite.
    if exists(dst).map_err(|e| one(src, e))? && !policy.resolve(dst).map_err(|e| one(src, e))? {
        return Ok(Outcome::Skipped);
    }
    replace_by_rename(staged, dst).map_err(|e| one(src, e))?;
    // The whole tree, then the parent: a crash after the source is unlinked must
    // find the destination readable, contents included, and holding the name.
    walk::sync_tree(dst).map_err(|e| one(src, e))?;
    sync_parent(dst).map_err(|e| one(src, e))?;
    Ok(Outcome::Moved)
}

/// One entry, for a failure that is about `path` rather than about one file
/// inside it.
fn one(path: &Path, errno: Errno) -> Vec<Failure> {
    vec![Failure {
        path: path.to_path_buf(),
        reason: errno.to_string(),
    }]
}

fn rename_noreplace(src: &Path, dst: &Path) -> IoResult<()> {
    match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
        // Pre-3.15 kernels and filesystems that never grew the flag. A plain
        // `renameat` here would clobber whatever landed since the last check, so
        // fall back to `linkat` + `unlinkat`, which fails with `EXIST` instead of
        // overwriting. Directories cannot be linked; the caller turns that back
        // into copy-then-delete.
        Err(Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP) => {
            let st = statat(CWD, src, AtFlags::SYMLINK_NOFOLLOW)?;
            if rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Directory {
                // Distinguished from the `EINVAL` that means a destination inside
                // the source, so the caller reads this as a missing feature.
                return Err(Errno::OPNOTSUPP);
            }
            linkat(CWD, src, CWD, dst, AtFlags::empty())?;
            // The source is now reachable under both names, so an unlink failure
            // leaves a copy rather than data loss.
            unlinkat(CWD, src, AtFlags::empty())?;
            Ok(())
        }
        other => other,
    }
}

/// Whether `path` is taken, without following a final symlink. `Path::exists`
/// follows it, so a dangling link reads as free and gets overwritten.
pub fn exists(path: &Path) -> IoResult<bool> {
    match statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Rename `src` onto an existing `dst`, whatever `dst` is.
///
/// Emptying a destination directory first loses it whenever the rename that
/// follows fails: a file onto a directory answers `EISDIR`, a directory onto a
/// file `ENOTDIR`, and neither `EACCES` nor a read-only filesystem is worth
/// betting a tree on. Moving the old destination aside instead keeps it until
/// the rename lands, and puts it back if the rename does not.
fn replace_by_rename(src: &Path, dst: &Path) -> IoResult<()> {
    if !exists(dst)? {
        return rustix::fs::rename(src, dst);
    }
    // A private sibling, so the old destination stays on the same filesystem and
    // the rename that restores it is atomic too.
    let parked = walk::staged_path(dst)?;
    renameat(CWD, dst, CWD, &parked)?;
    match rustix::fs::rename(src, dst) {
        Ok(()) => {
            let _ = walk::remove_any(&parked);
            Ok(())
        }
        Err(e) => {
            // Best effort: the destination name is free again either way, and a
            // failure here has already been reported to the caller.
            let _ = renameat(CWD, &parked, CWD, dst);
            Err(e)
        }
    }
}

/// Flush the directory holding `path`, which is what makes the rename that put
/// it there durable. No `NOFOLLOW`: the parent's ordinary path semantics are what
/// the user's destination means.
fn sync_parent(path: &Path) -> IoResult<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        // A bare name names an entry of the working directory.
        _ => Path::new("."),
    };
    let dir = rustix::fs::openat(
        CWD,
        parent,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )?;
    copy::commit(&dir)
}

pub fn prompt_replace(name: &OsStr) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    let mut answer = String::new();
    eprint!("Replace {}? [y/N] ", name.to_string_lossy());
    let _ = std::io::stderr().flush();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    is_yes(&answer)
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use crate::policy::Policy;

    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cb-mover-{name}-{}", std::process::id()));
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

    #[test]
    fn sync_parent_flushes_the_holding_directory() {
        let tmp = Tmp::new("sync-parent");
        let file = tmp.0.join("f");
        fs::write(&file, b"data").unwrap();

        sync_parent(&file).unwrap();

        assert_eq!(fs::read(&file).unwrap(), b"data");
    }

    #[test]
    fn sync_parent_missing_reports_noent() {
        let tmp = Tmp::new("sync-parent-missing");
        assert_eq!(sync_parent(&tmp.0.join("gone/f")), Err(Errno::NOENT));
    }

    #[test]
    fn sync_parent_tolerates_a_bare_name() {
        sync_parent(Path::new("f")).unwrap();
    }

    #[test]
    fn rename_noreplace_keeps_existing_destination() {
        let tmp = Tmp::new("noreplace");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();

        let result = rename_noreplace(&src, &dst);

        assert_eq!(result, Err(Errno::EXIST));
        assert_eq!(fs::read(&dst).unwrap(), b"old");
        assert_eq!(fs::read(&src).unwrap(), b"new");
    }

    #[test]
    fn rename_noreplace_replaces_dangling_symlink() {
        let tmp = Tmp::new("noreplace-symlink");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        fs::write(&src, b"new").unwrap();
        std::os::unix::fs::symlink("missing", &dst).unwrap();

        assert_eq!(rename_noreplace(&src, &dst), Err(Errno::EXIST));
        assert!(fs::symlink_metadata(&dst).unwrap().is_symlink());
    }

    #[test]
    fn rename_noreplace_moves_when_destination_free() {
        let tmp = Tmp::new("noreplace-free");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        fs::write(&src, b"new").unwrap();

        rename_noreplace(&src, &dst).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"new");
        assert!(!src.exists());
    }

    /// The destination used to be emptied before the rename, so a rename that
    /// then failed left nothing where the user's file had been.
    #[test]
    fn replace_by_rename_restores_the_destination_when_the_rename_fails() {
        let tmp = Tmp::new("replace-restore");
        let dst = tmp.0.join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("keep.txt"), b"keep").unwrap();
        let missing = tmp.0.join("gone");

        assert_eq!(
            replace_by_rename(&missing, &dst),
            Err(Errno::NOENT),
            "a missing source cannot be renamed"
        );

        assert_eq!(
            fs::read(dst.join("keep.txt")).unwrap(),
            b"keep",
            "the old destination must be put back"
        );
        let left: Vec<_> = fs::read_dir(&tmp.0).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "no parked copy may survive: {left:?}");
    }

    /// Plain `rename` will not replace a directory with a file, so the old
    /// destination used to be emptied and the rename then failed.
    #[test]
    fn a_file_replaces_a_non_empty_directory() {
        let sandbox_dir = Tmp::new("replace-dir");
        let src = sandbox_dir.0.join("src.txt");
        fs::write(&src, b"new").unwrap();
        let dst = sandbox_dir.0.join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("stale.txt"), b"old").unwrap();

        replace_by_rename(&src, &dst).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"new");
        assert!(!src.exists(), "the source is consumed by the rename");
        assert_eq!(
            fs::read_dir(&sandbox_dir.0).unwrap().count(),
            1,
            "the parked directory must not survive"
        );
    }

    #[test]
    fn exists_counts_a_dangling_symlink_as_taken() {
        let tmp = Tmp::new("exists-symlink");
        std::os::unix::fs::symlink("missing", tmp.0.join("l")).unwrap();

        assert!(exists(&tmp.0.join("l")).unwrap());
        assert!(!exists(&tmp.0.join("nope")).unwrap());
        assert!(!Path::new(&tmp.0.join("l")).exists());
    }

    #[test]
    fn stage_and_commit_keeps_the_destination_when_the_policy_declines() {
        let tmp = Tmp::new("stage-decline");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        let staged = walk::staged_path(&dst).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();

        assert_eq!(
            stage_and_commit(&src, &dst, &staged, Policy::Skip).map_err(|f| f.len()),
            Ok(Outcome::Skipped)
        );

        assert_eq!(fs::read(&dst).unwrap(), b"old");
        assert_eq!(
            fs::read(&src).unwrap(),
            b"new",
            "a decline must not consume the source"
        );
        assert!(
            !staged.exists(),
            "the staged copy must not survive a declined commit"
        );
    }

    #[test]
    fn stage_and_commit_replaces_a_directory_and_leaves_the_source() {
        let tmp = Tmp::new("stage-commit");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        let staged = walk::staged_path(&dst).unwrap();
        fs::create_dir(&src).unwrap();
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested/f"), b"new").unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("stale"), b"old").unwrap();

        assert_eq!(
            stage_and_commit(&src, &dst, &staged, Policy::Replace).map_err(|f| f.len()),
            Ok(Outcome::Moved)
        );

        assert_eq!(fs::read(dst.join("nested/f")).unwrap(), b"new");
        assert!(!dst.join("stale").exists(), "the old contents must be gone");
        assert!(
            src.join("nested/f").exists(),
            "the caller unlinks the source, not the commit"
        );
        assert!(!staged.exists(), "the staging path must not survive");
        assert_eq!(
            fs::read_dir(&tmp.0).unwrap().count(),
            2,
            "no staging leftovers in the destination directory"
        );
    }

    /// `copy_into` stages under a private name and renames in, so the policy is
    /// consulted against the destination again after the copy, not once.
    #[test]
    fn copy_into_declines_an_existing_destination_and_keeps_the_source() {
        let tmp = Tmp::new("copy-decline");
        let src = tmp.0.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), b"new").unwrap();
        fs::write(tmp.0.join("dst"), b"old").unwrap();

        let outcome = copy_into(&src, &tmp.0, Policy::Skip).map_err(|f| f.len());

        assert_eq!(outcome, Ok(Outcome::Skipped));
        assert_eq!(fs::read(tmp.0.join("dst")).unwrap(), b"old");
        assert!(src.join("f").exists(), "a copy never consumes its source");
        assert_eq!(
            fs::read_dir(&tmp.0).unwrap().count(),
            2,
            "no staging leftovers: {:?}",
            fs::read_dir(&tmp.0)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        );
    }

    /// The copy goes in under a private name and is renamed in, so a copy that
    /// does not finish never has touched the destination.
    #[test]
    fn a_copy_that_does_not_finish_leaves_the_destination_alone() {
        let tmp = Tmp::new("copy-partial");
        let src = tmp.0.join("tree");
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("top"), b"new").unwrap();
        fs::write(src.join("inner/secret"), b"hidden").unwrap();
        let dst_dir = tmp.0.join("out");
        let dst = dst_dir.join("tree");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("existing"), b"keep").unwrap();
        fs::set_permissions(src.join("inner"), fs::Permissions::from_mode(0o000)).unwrap();

        let failures = copy_into(&src, &dst_dir, Policy::Replace).unwrap_err();

        fs::set_permissions(src.join("inner"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            !failures.is_empty(),
            "a failed copy must be reported: {failures:?}"
        );
        assert_eq!(
            fs::read(dst.join("existing")).unwrap(),
            b"keep",
            "the destination must not be touched by a copy that failed"
        );
        assert!(
            !dst.join("top").exists(),
            "no part of the copy may land on the destination"
        );
    }

    #[test]
    fn staged_path_is_a_private_sibling_that_differs_per_call() {
        let dst = Path::new("/tmp/dst");
        let a = walk::staged_path(dst).unwrap();
        let b = walk::staged_path(dst).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), dst.parent(), "must stay on the same filesystem");
        assert!(
            a.file_name()
                .unwrap()
                .as_encoded_bytes()
                .starts_with(b".cb-tmp."),
            "got {:?}",
            a.file_name().unwrap()
        );
    }

    #[test]
    fn prompt_replace_declines_without_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        assert!(!prompt_replace(OsStr::new("name")));
    }

    #[test]
    fn is_yes_accepts_y_and_yes() {
        for a in ["y", "Y", "yes", "Yes"] {
            assert!(is_yes(a), "{a:?} should count as yes");
            assert!(is_yes(&format!("{a}\n")), "{a:?} with newline should count");
            assert!(is_yes(&format!("  {a}  ")), "{a:?} padded should count");
        }
    }

    #[test]
    fn is_yes_rejects_everything_else() {
        for a in [
            "", "n", "N", "no", "no!", "yep", "ye", "YES", "yEs", "1", "true",
        ] {
            assert!(!is_yes(a), "{a:?} should not count as yes");
        }
    }
}
