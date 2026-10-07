use std::ffi::OsStr;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, RenameFlags, linkat, renameat, renameat_with, statat, unlinkat};
use rustix::io::{Errno, Result as IoResult};

use crate::copy;
use crate::policy::Policy;
use crate::walk;

/// What became of one move attempt.
pub enum Outcome {
    Moved,
    /// Destination exists and policy said leave it alone.
    Skipped,
}

/// Move `src` to `dst_dir/<basename>`.
///
/// Same filesystem: one `renameat2`. Different filesystems: copy, verify the
/// destination is on disk, and only then unlink the source. The C++ version
/// deletes originals after a copy it never re-checked, which loses data if the
/// copy silently came up short.
pub fn move_into(src: &Path, dst_dir: &Path, policy: Policy) -> IoResult<Outcome> {
    let name = src.file_name().ok_or(Errno::INVAL)?;
    let dst = dst_dir.join(name);

    match rename_noreplace(src, &dst) {
        Ok(()) => return Ok(Outcome::Moved),
        Err(Errno::EXIST | Errno::NOTEMPTY | Errno::ISDIR) => {
            if !policy.resolve(&dst)? {
                return Ok(Outcome::Skipped);
            }
            // Plain `renameat` replaces atomically for files. Directories must
            // be emptied first, since `rename` will not overwrite a non-empty
            // directory.
            clear_destination(&dst)?;
            return renameat(CWD, src, CWD, &dst).map(|()| Outcome::Moved);
        }
        // Cross-device, or no `renameat2`: fall back to copy-then-delete.
        Err(Errno::XDEV | Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP) => {
            // The copy truncates whatever it finds, so the policy has to be
            // consulted here too, not only on the `EXIST` answer from above.
            //
            // ponytail: the check and the copy are not atomic together, so a
            // destination created in between is still overwritten. A tree
            // cannot be created exclusively in one step. Upgrade path: build
            // into a private directory and `renameat` it over the destination
            // once it is complete.
            if exists(&dst)? {
                if !policy.resolve(&dst)? {
                    return Ok(Outcome::Skipped);
                }
                clear_destination(&dst)?;
            }
        }
        Err(e) => return Err(e),
    }

    let failures = walk::copy_any(src, &dst);
    if !failures.is_empty() {
        return Err(Errno::IO);
    }
    sync_path(&dst)?;
    walk::remove_any(src)?;
    Ok(Outcome::Moved)
}

fn rename_noreplace(src: &Path, dst: &Path) -> IoResult<()> {
    match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
        // Pre-3.15 kernels and filesystems that never grew the flag. A plain
        // `renameat` here would clobber whatever landed since the last check, so
        // fall back to `linkat` + `unlinkat`, which fails with `EXIST` instead of
        // overwriting. Directories cannot be linked; the caller turns that back
        // into copy-then-delete.
        Err(e @ (Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP)) => {
            let st = statat(CWD, src, AtFlags::SYMLINK_NOFOLLOW)?;
            if rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Directory {
                return Err(e);
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

/// Empty `dst` when it is a directory. Plain `rename` will not overwrite a
/// non-empty one.
fn clear_destination(dst: &Path) -> IoResult<()> {
    let st = statat(CWD, dst, AtFlags::SYMLINK_NOFOLLOW)?;
    if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::Directory {
        return Ok(());
    }
    // `NOFOLLOW`: the `statat` above named a directory, and a symlink swapped in
    // between must fail the open rather than empty whatever it points at.
    let dir_fd = rustix::fs::openat(
        CWD,
        dst,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?;
    let mut dir = rustix::fs::Dir::read_from(&dir_fd)?;
    let mut children = Vec::new();
    for entry in dir.by_ref() {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        children.push(dst.join(OsStr::from_bytes(name.to_bytes())));
    }
    for child in children {
        walk::remove_any(&child)?;
    }
    Ok(())
}

/// Flush a freshly copied destination so a power loss cannot leave the source
/// deleted and the destination empty.
fn sync_path(path: &Path) -> IoResult<()> {
    let fd = rustix::fs::openat(
        CWD,
        path,
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
    )?;
    let result = copy::commit(&fd);
    drop(fd);
    if result.is_err() {
        // Directories and special files cannot be opened for fsync this way;
        // fsync the parent directory instead, which is what makes the rename
        // or unlink durable.
        if let Some(parent) = path.parent() {
            let dir = rustix::fs::openat(
                CWD,
                parent,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
            )?;
            copy::commit(&dir)?;
        }
    }
    Ok(())
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
    use std::path::PathBuf;

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
    fn sync_path_regular_file() {
        let tmp = Tmp::new("sync-file");
        let file = tmp.0.join("f");
        fs::write(&file, b"data").unwrap();

        sync_path(&file).unwrap();

        assert_eq!(fs::read(&file).unwrap(), b"data");
    }

    #[test]
    fn sync_path_directory() {
        let tmp = Tmp::new("sync-dir");
        let dir = tmp.0.join("d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("f"), b"x").unwrap();

        sync_path(&dir).unwrap();
    }

    #[test]
    fn sync_path_missing_reports_noent() {
        let tmp = Tmp::new("sync-missing");
        assert_eq!(sync_path(&tmp.0.join("nope")), Err(Errno::NOENT));
    }

    #[test]
    fn sync_path_unreadable_file_propagates_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = Tmp::new("sync-eacces");
        let file = tmp.0.join("f");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&file).is_ok() {
            return;
        }

        assert_eq!(sync_path(&file), Err(Errno::ACCESS));
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

    #[test]
    fn exists_counts_a_dangling_symlink_as_taken() {
        let tmp = Tmp::new("exists-symlink");
        std::os::unix::fs::symlink("missing", tmp.0.join("l")).unwrap();

        assert!(exists(&tmp.0.join("l")).unwrap());
        assert!(!exists(&tmp.0.join("nope")).unwrap());
        assert!(!Path::new(&tmp.0.join("l")).exists());
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
