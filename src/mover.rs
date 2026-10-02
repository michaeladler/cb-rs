use std::ffi::OsStr;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, RenameFlags, renameat, renameat_with, statat};
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
        // Cross-device or no `renameat2`: fall back to copy-then-delete.
        Err(Errno::XDEV | Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP) => {}
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
        // Pre-3.15 kernels and some filesystems reject the flags word.
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
            if statat(CWD, dst, AtFlags::empty()).is_ok() {
                Err(Errno::EXIST)
            } else {
                renameat(CWD, src, CWD, dst)
            }
        }
        other => other,
    }
}

fn clear_destination(dst: &Path) -> IoResult<()> {
    let st = statat(CWD, dst, AtFlags::SYMLINK_NOFOLLOW)?;
    if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::Directory {
        return Ok(());
    }
    let dir_fd = rustix::fs::openat(
        CWD,
        dst,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
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
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
}
