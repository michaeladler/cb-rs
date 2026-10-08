use std::ffi::OsStr;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use rustix::fs::{AtFlags, CWD, RenameFlags, linkat, renameat, renameat_with, statat, unlinkat};
use rustix::io::{Errno, Result as IoResult};

use crate::copy;
use crate::policy::Policy;
use crate::walk;
use crate::walk::{Failure, Walked};

type DestinationIdentity = (u128, u128);

/// What became of one move attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Moved,
    /// The destination holds the whole tree, but the source could not be removed
    /// whole, so what is left of it is named here. The move stands: the entry is
    /// consumed, not left to be pasted over the destination again.
    MovedWithLeftover(Failure),
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
    let walked = Walked::default();
    let outcome = stage_and_commit(src, &dst, &staged, policy, Some(&walked), None);
    if matches!(outcome, Ok(Outcome::Moved)) {
        return remove_moved_source(src, &walked);
    }
    outcome
}

/// Finish a committed cross-device move: unlink the source, but only while it
/// still holds what the copy took.
///
/// The copy is a walk, and a walk is not atomic: a file written into the source
/// after the walk read that directory was never copied, and deleting the source
/// whole would lose it. The destination is already on disk at this point, so a
/// source that changed is reported as a failure and left for the user: the entry
/// stays in `originals`, and the next paste copies the whole thing again.
///
/// A failure to unlink is not that. The commit already happened, so the
/// destination is complete while the source is only partly removed. Leaving the
/// entry recorded would have the next paste copy what is left of the source and
/// replace the tree that is already there, so the move counts and only the
/// leftover is reported.
fn remove_moved_source(src: &Path, walked: &Walked) -> Result<Outcome, Vec<Failure>> {
    let changed = walked.changed();
    if !changed.is_empty() {
        return Err(changed
            .into_iter()
            .map(|path| Failure {
                path,
                reason: "source changed while it was being copied, so it was not removed"
                    .to_owned(),
            })
            .collect());
    }
    match walk::remove_any(src) {
        Ok(()) => Ok(Outcome::Moved),
        Err(e) => Ok(Outcome::MovedWithLeftover(Failure {
            path: src.to_path_buf(),
            reason: format!("moved, but the source could not be removed whole: {e}"),
        })),
    }
}

/// Copy `src` into `dst_dir`, like [`move_into`] but leaving the source alone.
///
/// Staged and renamed in like a move. An existing destination is approved before
/// the copy, then re-approved only if its inode changes before commit.
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
    let approved_dst = destination_identity(&dst).map_err(|e| one(src, e))?;
    if approved_dst.is_some() && !policy.resolve(&dst).map_err(|e| one(src, e))? {
        return Ok(Outcome::Skipped);
    }
    let staged = walk::staged_path(&dst).map_err(|e| one(src, e))?;
    // A copy consumes nothing, so there is no source to check and nothing to record.
    stage_and_commit(src, &dst, &staged, policy, None, approved_dst)
}

/// Copy `src` to `staged`, then rename it onto `dst`. A staged tree that did not
/// reach the destination is removed here rather than by the caller, which cannot
/// tell which outcome it got before looking.
fn stage_and_commit(
    src: &Path,
    dst: &Path,
    staged: &Path,
    policy: Policy,
    walked: Option<&Walked>,
    approved_dst: Option<DestinationIdentity>,
) -> Result<Outcome, Vec<Failure>> {
    let result = copy_then_commit(src, dst, staged, policy, walked, approved_dst);
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
    walked: Option<&Walked>,
    approved_dst: Option<DestinationIdentity>,
) -> Result<Outcome, Vec<Failure>> {
    let failures = walk::copy_walking(src, staged, walked);
    if !failures.is_empty() {
        return Err(failures);
    }
    // Re-check after staging: approve newly appeared or replaced destinations.
    let current_dst = destination_identity(dst).map_err(|e| one(src, e))?;
    if current_dst != approved_dst
        && current_dst.is_some()
        && !policy.resolve(dst).map_err(|e| one(src, e))?
    {
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
    Ok(destination_identity(path)?.is_some())
}

fn destination_identity(path: &Path) -> IoResult<Option<DestinationIdentity>> {
    match statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some((stat.st_dev as u128, stat.st_ino as u128))),
        Err(Errno::NOENT) => Ok(None),
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
    use std::path::{Path, PathBuf};

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
            stage_and_commit(&src, &dst, &staged, Policy::Skip, None, None).map_err(|f| f.len()),
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
            stage_and_commit(&src, &dst, &staged, Policy::Replace, None, None).map_err(|f| f.len()),
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

    /// The cross-device half of `move_into`, which a same-filesystem test never
    /// reaches: commit a tree into `out/src` and hand back what the walk saw.
    fn committed(src: &Path, out: &Path) -> Walked {
        let dst = out.join(src.file_name().unwrap());
        let staged = walk::staged_path(&dst).unwrap();
        let walked = Walked::default();
        assert_eq!(
            stage_and_commit(src, &dst, &staged, Policy::Replace, Some(&walked), None)
                .map_err(|f| f.len()),
            Ok(Outcome::Moved)
        );
        walked
    }

    /// The move deleted the whole source tree after the copy, so a file written
    /// into the source while the walk was elsewhere in it was destroyed without
    /// ever having been copied.
    #[test]
    fn a_write_into_the_source_during_the_copy_keeps_the_source() {
        let tmp = Tmp::new("changed-source");
        let src = tmp.0.join("src");
        let out = tmp.0.join("out");
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("inner/f"), b"new").unwrap();
        fs::create_dir(&out).unwrap();

        let walked = committed(&src, &out);
        fs::write(src.join("inner/late"), b"late").unwrap();

        let failures = remove_moved_source(&src, &walked).unwrap_err();
        assert_eq!(fs::read(src.join("inner/late")).unwrap(), b"late");
        assert!(
            !out.join("src/inner/late").exists(),
            "the premise: the late file was never copied"
        );
        assert!(
            failures.iter().any(|f| f.path == src.join("inner")),
            "the directory that changed must be named: {failures:?}"
        );
    }

    #[test]
    fn an_unchanged_source_is_removed_after_a_cross_device_copy() {
        let tmp = Tmp::new("unchanged-source");
        let src = tmp.0.join("src");
        let out = tmp.0.join("out");
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("inner/f"), b"new").unwrap();
        fs::create_dir(&out).unwrap();

        let walked = committed(&src, &out);

        assert!(walked.changed().is_empty());
        assert_eq!(remove_moved_source(&src, &walked), Ok(Outcome::Moved));
        assert!(
            !src.exists(),
            "a completed cross-device move consumes its source"
        );
    }

    /// The source was emptied but the directory holding it stayed put, so
    /// `remove_any` failed with the source already half gone. Reporting that as a
    /// failed move left the entry in `originals`, and the next paste copied the
    /// fragment that was left and replaced the whole destination with it.
    #[test]
    fn a_half_removed_source_is_a_moved_leftover_not_a_failed_move() {
        let tmp = Tmp::new("leftover");
        let src = tmp.0.join("src");
        let out = tmp.0.join("out");
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("inner/f"), b"new").unwrap();
        fs::create_dir(&out).unwrap();

        let walked = committed(&src, &out);
        // The copy reads the source and writes to the destination, so a parent
        // that cannot be written to still lets the commit land.
        fs::set_permissions(&tmp.0, fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = remove_moved_source(&src, &walked);
        fs::set_permissions(&tmp.0, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            src.exists() && !src.join("inner/f").exists(),
            "the premise: the removal emptied the source and then failed"
        );
        assert_eq!(fs::read(out.join("src/inner/f")).unwrap(), b"new");
        match outcome {
            Ok(Outcome::MovedWithLeftover(leftover)) => {
                assert_eq!(leftover.path, src, "the leftover source must be named");
                assert!(
                    leftover.reason.contains("could not be removed"),
                    "got {}",
                    leftover.reason
                );
            }
            other => panic!("the move stands, only the cleanup failed: {other:?}"),
        }
    }

    /// A dangling destination symlink counts as occupied during commit. The
    /// commit path used to flush it by opening `O_RDONLY`, which followed the
    /// link: `ENOENT` after rename left the source behind despite the commit.
    #[test]
    fn a_dangling_symlink_reaches_the_destination() {
        let tmp = Tmp::new("sym-commit");
        let src = tmp.0.join("link");
        let out = tmp.0.join("out");
        fs::create_dir(&out).unwrap();
        let dst = out.join("link");
        let staged = walk::staged_path(&dst).unwrap();
        std::os::unix::fs::symlink("nowhere", &src).unwrap();

        let outcome =
            copy_then_commit(&src, &dst, &staged, Policy::Skip, None, None).map_err(|f| f.len());

        assert_eq!(outcome, Ok(Outcome::Moved));
        assert_eq!(fs::read_link(&dst).unwrap(), Path::new("nowhere"));
    }

    #[test]
    fn copy_commit_reuses_approval_for_the_same_destination_inode() {
        let tmp = Tmp::new("copy-approved");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        let staged = walk::staged_path(&dst).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();
        let approved_dst = destination_identity(&dst).unwrap();

        let result = stage_and_commit(&src, &dst, &staged, Policy::Skip, None, approved_dst);

        assert_eq!(result, Ok(Outcome::Moved));
        assert_eq!(fs::read(&dst).unwrap(), b"new");
    }

    #[test]
    fn copy_commit_rechecks_approval_after_destination_inode_changes() {
        let tmp = Tmp::new("copy-changed-dst");
        let src = tmp.0.join("src");
        let dst = tmp.0.join("dst");
        let staged = walk::staged_path(&dst).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();
        let approved_dst = destination_identity(&dst).unwrap();
        let replacement = tmp.0.join("replacement");
        fs::write(&replacement, b"changed").unwrap();
        fs::rename(replacement, &dst).unwrap();

        let result = stage_and_commit(&src, &dst, &staged, Policy::Skip, None, approved_dst);

        assert_eq!(result, Ok(Outcome::Skipped));
        assert_eq!(fs::read(&dst).unwrap(), b"changed");
    }

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
