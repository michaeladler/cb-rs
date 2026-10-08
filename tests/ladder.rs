//! Integration checks for the copy ladder and the move path. Plain asserts, no
//! framework: every case here corresponds to a way the C++ implementation loses
//! or corrupts data.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use cb_rs::mover::{Outcome, copy_into, move_into};
use cb_rs::policy::Policy;
use cb_rs::walk::{self, copy_any, remove_any};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cb-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Copied trees may have arrived with read-only modes.
        restore_modes(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn restore_modes(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
            restore_modes(&path);
        }
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[test]
fn copies_regular_file_byte_for_byte() {
    let sandbox = Sandbox::new("regular");
    let expected: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    sandbox.write("src.bin", &expected);
    let dst = sandbox.path("dst.bin");

    let failures = copy_any(&sandbox.path("src.bin"), &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::read(&dst).unwrap(), expected);
}

#[test]
fn copies_a_large_file() {
    let sandbox = Sandbox::new("large");
    let data = vec![0xabu8; 3 * 1024 * 1024];
    sandbox.write("big.bin", &data);
    let dst = sandbox.path("big-copy.bin");

    let failures = copy_any(&sandbox.path("big.bin"), &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::metadata(&dst).unwrap().len(), data.len() as u64);
    assert_eq!(fs::read(&dst).unwrap(), data);
}

#[test]
fn reproduces_symlinks_rather_than_their_targets() {
    let sandbox = Sandbox::new("symlink");
    sandbox.write("target.txt", b"payload");
    symlink("target.txt", sandbox.path("link")).unwrap();
    let dst = sandbox.path("copied-link");

    let failures = copy_any(&sandbox.path("link"), &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(fs::symlink_metadata(&dst).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&dst).unwrap(), Path::new("target.txt"));
    assert_eq!(
        fs::metadata(&dst).unwrap().len(),
        7,
        "a symlink must not be copied as data"
    );
}

#[test]
fn reproduces_nested_directory_tree() {
    let sandbox = Sandbox::new("tree");
    let root = sandbox.path("tree");
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::write(root.join("a/b/c/deep.txt"), b"deep").unwrap();
    fs::write(root.join("a/one.txt"), b"one").unwrap();
    fs::write(root.join("top.txt"), b"top").unwrap();
    let dst = sandbox.path("tree-copy");

    let failures = copy_any(&root, &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
    assert_eq!(fs::read(dst.join("a/one.txt")).unwrap(), b"one");
    assert_eq!(fs::read(dst.join("a/b/c/deep.txt")).unwrap(), b"deep");
}

#[test]
fn preserves_permission_bits() {
    let sandbox = Sandbox::new("mode");
    let src = sandbox.write("script.sh", b"#!/bin/sh\n");
    fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
    let dst = sandbox.path("script-copy.sh");

    let failures = copy_any(&src, &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(mode_of(&dst), 0o750);
}

#[test]
fn destination_is_never_an_alias_of_the_original() {
    let sandbox = Sandbox::new("inode");
    let src = sandbox.write("src.bin", &vec![7u8; 512 * 1024]);
    let dst = sandbox.path("dst.bin");

    let failures = copy_any(&src, &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::read(&src).unwrap(), fs::read(&dst).unwrap());
    assert_ne!(
        fs::metadata(&src).unwrap().ino(),
        fs::metadata(&dst).unwrap().ino(),
        "destination must be a separate inode, never a hardlink to the user's file"
    );
}

/// The regression the C++ design is vulnerable to: it deletes originals after a
/// copy it never re-verified. Here the source must survive a move that cannot
/// land. Nothing can be written into the destination, so neither the rename nor
/// a staged copy can complete.
#[test]
fn move_leaves_source_intact_when_the_destination_cannot_be_written() {
    let sandbox = Sandbox::new("move-verify");
    let src = sandbox.write("keepme.txt", b"important");
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(&dst_dir).unwrap();
    fs::set_permissions(&dst_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let result = move_into(&src, &dst_dir, Policy::Replace);

    fs::set_permissions(&dst_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        result.is_err(),
        "a blocked destination must not report success"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        b"important",
        "source must survive a failed move"
    );
}

/// Plain `rename` replaces a file over a directory name only once the directory
/// is gone. Parking it means the move lands instead of emptying the user's
/// directory and then failing with `EISDIR`.
#[test]
fn a_file_moves_over_an_existing_non_empty_directory() {
    let sandbox = Sandbox::new("move-over-dir");
    let src = sandbox.write("keepme.txt", b"important");
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(dst_dir.join("keepme.txt")).unwrap();
    fs::write(dst_dir.join("keepme.txt/blocker"), b"x").unwrap();

    let outcome = move_into(&src, &dst_dir, Policy::Replace).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    assert_eq!(fs::read(dst_dir.join("keepme.txt")).unwrap(), b"important");
    assert!(!src.exists());
    assert_eq!(
        fs::read_dir(&dst_dir).unwrap().count(),
        1,
        "no parking leftovers in the destination directory"
    );
}

/// Under `replace`, a directory pasted over an existing one used to merge into
/// it, so files the source no longer has stayed behind. A move replaces the
/// whole name, and so does a copy.
#[test]
fn a_replaced_directory_is_not_merged_into() {
    let sandbox = Sandbox::new("replace-merge");
    let src = sandbox.path("tree");
    fs::create_dir_all(src.join("inner")).unwrap();
    fs::write(src.join("inner/new.txt"), b"new").unwrap();
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(dst_dir.join("tree/inner")).unwrap();
    fs::write(dst_dir.join("tree/stale.txt"), b"stale").unwrap();
    fs::write(dst_dir.join("tree/inner/stale.txt"), b"stale").unwrap();

    let outcome = copy_into(&src, &dst_dir, Policy::Replace).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    let dst = dst_dir.join("tree");
    assert_eq!(fs::read(dst.join("inner/new.txt")).unwrap(), b"new");
    assert!(
        !dst.join("stale.txt").exists() && !dst.join("inner/stale.txt").exists(),
        "replace must not merge: {:?}",
        fs::read_dir(dst.join("inner")).unwrap().flatten().collect::<Vec<_>>()
    );
}

/// A file pasted over an existing directory used to fail with `EISDIR`.
#[test]
fn a_file_replaces_a_directory() {
    let sandbox = Sandbox::new("replace-dir-with-file");
    let src = sandbox.write("file.txt", b"new");
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(dst_dir.join("file.txt")).unwrap();
    fs::write(dst_dir.join("file.txt/stale.txt"), b"stale").unwrap();

    let outcome = copy_into(&src, &dst_dir, Policy::Replace).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    assert_eq!(fs::read(dst_dir.join("file.txt")).unwrap(), b"new");
}

/// A read-only destination cannot be opened `O_WRONLY`, so copying over it used
/// to fail with `EACCES` even under `--on-conflict replace`.
#[test]
fn a_read_only_destination_is_replaced() {
    let sandbox = Sandbox::new("ro-dst");
    let src = sandbox.write("src.txt", b"new");
    let dst = sandbox.write("dst.txt", b"old");
    fs::set_permissions(&dst, fs::Permissions::from_mode(0o444)).unwrap();

    let failures = copy_any(&src, &dst);

    fs::set_permissions(&dst, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::read(&dst).unwrap(), b"new");
}

/// Writing the destination in place changed every other name that inode answers
/// to. The copy lands under a temporary name and is renamed in, so only the
/// pasted name changes.
#[test]
fn replacing_a_hard_linked_destination_leaves_the_other_names_alone() {
    let sandbox = Sandbox::new("hardlink-dst");
    let src = sandbox.write("src.txt", b"new");
    let dst = sandbox.write("dst.txt", b"old");
    let other = sandbox.path("other-name.txt");
    fs::hard_link(&dst, &other).unwrap();

    let failures = copy_any(&src, &dst);

    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fs::read(&dst).unwrap(), b"new");
    assert_eq!(
        fs::read(&other).unwrap(),
        b"old",
        "the sibling name of the old inode must not change"
    );
}

/// `cut f` in the folder that holds `f`, then `paste` there: the destination is
/// the source. The replace policy used to empty the source before a rename that
/// does nothing.
#[test]
fn move_into_the_folder_the_source_lives_in_keeps_the_file() {
    let sandbox = Sandbox::new("self-move");
    let src = sandbox.write("file.txt", b"payload");
    let dst_dir = src.parent().unwrap().to_path_buf();

    let outcome = move_into(&src, &dst_dir, Policy::Replace).unwrap();

    assert!(
        matches!(outcome, Outcome::Skipped),
        "a paste onto the source itself must be a no-op"
    );
    assert_eq!(fs::read(&src).unwrap(), b"payload");
}

/// The same for a copy: `copy_regular_path(src, src)` opens the destination
/// `O_TRUNC`, so the file is emptied before it is read.
#[test]
fn copy_into_the_folder_the_source_lives_in_keeps_the_file() {
    let sandbox = Sandbox::new("self-copy");
    let src = sandbox.write("file.txt", b"payload");

    let failures = copy_any(&src, &src);

    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(fs::read(&src).unwrap(), b"payload");
}

/// `cb copy d; cd d/sub; cb paste` would otherwise copy the copy, forever.
/// `rename` answers `EINVAL`, which the mover used to read as "no renameat2
/// here" and answer with a copy-then-delete into the source itself.
#[test]
fn move_into_its_own_subtree_is_refused() {
    let sandbox = Sandbox::new("subtree-move");
    let src = sandbox.path("tree");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("sub/inner.txt"), b"inner").unwrap();
    let dst_dir = src.join("sub");

    let failures = move_into(&src, &dst_dir, Policy::Replace).unwrap_err();

    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].reason, rustix::io::Errno::INVAL.to_string());
    assert_eq!(fs::read(src.join("sub/inner.txt")).unwrap(), b"inner");
    assert_eq!(
        fs::read_dir(&dst_dir).unwrap().count(),
        1,
        "nothing may be written into the source"
    );
}

#[test]
fn copy_into_its_own_subtree_is_refused() {
    let sandbox = Sandbox::new("subtree-copy");
    let src = sandbox.path("tree");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("sub/inner.txt"), b"inner").unwrap();

    let failures = copy_any(&src, &src.join("sub/tree"));

    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(!src.join("sub/tree").exists());
}

#[test]
fn move_uses_rename_and_leaves_no_source() {
    let sandbox = Sandbox::new("move-rename");
    let src = sandbox.write("file.txt", b"contents");
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(&dst_dir).unwrap();

    let outcome = move_into(&src, &dst_dir, Policy::Skip).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    assert_eq!(fs::read(dst_dir.join("file.txt")).unwrap(), b"contents");
    assert!(!src.exists(), "source must be gone after a successful move");
}

/// A writable directory on a filesystem other than the temp dir's, or `None`.
/// `renameat2` answers `EXDEV` for two mounts, and only `EXDEV` takes the mover
/// down its copy-then-delete branch, so this cannot be faked with a second
/// directory on the same filesystem. `scripts/testvol.sh` provides one.
fn other_device_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("CB_TESTVOL_DIR")?)
        .join(format!("cb-xdev-{}", std::process::id()));
    fs::create_dir_all(&dir).ok()?;
    let temp_dev = fs::metadata(std::env::temp_dir()).ok()?.dev();
    (fs::metadata(&dir).ok()?.dev() != temp_dev).then_some(dir)
}

/// The cross-filesystem move: copy the whole tree, get it on disk, and only then
/// unlink the original. A tree rather than a file so the walk, the directory
/// `fsync` and the recursive delete all take the cross-device path too.
#[test]
fn a_move_across_devices_copies_verifies_then_deletes() {
    let Some(xdev) = other_device_dir() else {
        eprintln!("skipping: no second filesystem (set CB_TESTVOL_DIR)");
        return;
    };
    let sandbox = Sandbox::new("xdev-move");
    let src = sandbox.path("tree");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("sub/inner.txt"), b"inner").unwrap();
    fs::write(src.join("top.txt"), b"top").unwrap();
    let dst_dir = xdev.join("dst");
    fs::create_dir_all(&dst_dir).unwrap();

    let outcome = move_into(&src, &dst_dir, Policy::Skip).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    assert_eq!(
        fs::read(dst_dir.join("tree/sub/inner.txt")).unwrap(),
        b"inner"
    );
    assert_eq!(fs::read(dst_dir.join("tree/top.txt")).unwrap(), b"top");
    assert!(
        !src.exists(),
        "the source is unlinked only after the copy is verified, so a completed \
         move must leave nothing behind"
    );
    let _ = fs::remove_dir_all(&xdev);
}

/// The cross-device copy truncates whatever sits at the destination, so the
/// policy has to be asked there too. Without this the default `skip` is honoured
/// only on the same-filesystem path.
#[test]
fn a_move_across_devices_skips_an_existing_destination() {
    let Some(xdev) = other_device_dir() else {
        eprintln!("skipping: no second filesystem (set CB_TESTVOL_DIR)");
        return;
    };
    let sandbox = Sandbox::new("xdev-policy");
    let src = sandbox.write("file.txt", b"source");
    let dst_dir = xdev.join("policy-dst");
    fs::create_dir_all(&dst_dir).unwrap();
    fs::write(dst_dir.join("file.txt"), b"existing").unwrap();

    let outcome = move_into(&src, &dst_dir, Policy::Skip).unwrap();

    assert!(matches!(outcome, Outcome::Skipped));
    assert_eq!(fs::read(dst_dir.join("file.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(&src).unwrap(), b"source");
    let _ = fs::remove_dir_all(&xdev);
}

#[test]
fn move_skips_an_existing_destination_by_default() {
    let sandbox = Sandbox::new("move-skip");
    let src = sandbox.write("file.txt", b"source");
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(&dst_dir).unwrap();
    fs::write(dst_dir.join("file.txt"), b"existing").unwrap();

    let outcome = move_into(&src, &dst_dir, Policy::Skip).unwrap();

    assert!(
        matches!(outcome, Outcome::Skipped),
        "default policy must not clobber an existing file"
    );
    assert_eq!(fs::read(dst_dir.join("file.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(&src).unwrap(), b"source");
}

#[test]
fn move_replaces_when_policy_says_so() {
    let sandbox = Sandbox::new("move-replace");
    let src = sandbox.write("file.txt", b"source");
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(&dst_dir).unwrap();
    fs::write(dst_dir.join("file.txt"), b"existing").unwrap();

    let outcome = move_into(&src, &dst_dir, Policy::Replace).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    assert_eq!(fs::read(dst_dir.join("file.txt")).unwrap(), b"source");
    assert!(!src.exists());
}

#[test]
fn moves_a_directory_tree_intact() {
    let sandbox = Sandbox::new("move-tree");
    let root = sandbox.path("tree");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/inner.txt"), b"inner").unwrap();
    let dst_dir = sandbox.path("dst");
    fs::create_dir_all(&dst_dir).unwrap();

    let outcome = move_into(&root, &dst_dir, Policy::Skip).unwrap();

    assert!(matches!(outcome, Outcome::Moved));
    assert_eq!(
        fs::read(dst_dir.join("tree/sub/inner.txt")).unwrap(),
        b"inner"
    );
    assert!(!root.exists());
}

/// A read-only source directory cannot be written into while it is copied, so
/// the destination mode must be applied after its children are created.
#[test]
fn copies_a_read_only_directory() {
    let sandbox = Sandbox::new("readonly-dir");
    let root = sandbox.path("locked");
    fs::create_dir_all(root.join("inner")).unwrap();
    fs::write(root.join("inner/data.txt"), b"data").unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();

    let failures = copy_any(&root, &sandbox.path("locked-copy"));

    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        fs::read(sandbox.path("locked-copy/inner/data.txt")).unwrap(),
        b"data"
    );
    assert_eq!(mode_of(&sandbox.path("locked-copy")), 0o500);
}

#[test]
fn reports_a_missing_source_instead_of_creating_an_empty_copy() {
    let sandbox = Sandbox::new("missing");
    let failures = copy_any(&sandbox.path("nope.txt"), &sandbox.path("out.txt"));

    assert_eq!(
        failures.len(),
        1,
        "missing source must be reported as a failure"
    );
    assert!(!sandbox.path("out.txt").exists());
}

#[test]
fn empty_directory_survives_a_round_trip() {
    let sandbox = Sandbox::new("empty-dir");
    fs::create_dir_all(sandbox.path("nothing")).unwrap();
    let dst = sandbox.path("nothing-copy");

    let failures = copy_any(&sandbox.path("nothing"), &dst);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(dst.is_dir());
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
}

#[test]
fn many_small_files_all_arrive() {
    let sandbox = Sandbox::new("many");
    let root = sandbox.path("many");
    fs::create_dir_all(&root).unwrap();
    for i in 0..500 {
        fs::write(root.join(format!("f{i:04}.txt")), i.to_string()).unwrap();
    }
    let dst = sandbox.path("many-copy");

    let failures = copy_any(&root, &dst);
    assert!(
        failures.is_empty(),
        "{} failures, first {:?}",
        failures.len(),
        failures.first()
    );
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 500);
    assert_eq!(fs::read(dst.join("f0499.txt")).unwrap(), b"499");
}

#[test]
fn remove_any_takes_a_whole_tree() {
    let sandbox = Sandbox::new("remove");
    let root = sandbox.path("doomed");
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/file.txt"), b"x").unwrap();
    fs::write(root.join("top.txt"), b"y").unwrap();

    remove_any(&root).unwrap();

    assert!(!root.exists());
}

#[test]
fn walker_reports_failures_instead_of_silently_skipping() {
    let sandbox = Sandbox::new("unreadable");
    let root = sandbox.path("tree");
    fs::create_dir_all(root.join("locked")).unwrap();
    fs::write(root.join("readable.txt"), b"fine").unwrap();
    fs::write(root.join("locked/secret.txt"), b"hidden").unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let failures = copy_any(&root, &sandbox.path("tree-copy"));

    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !failures.is_empty(),
        "an unreadable directory must be reported, not dropped"
    );
    assert_eq!(
        fs::read(sandbox.path("tree-copy/readable.txt")).unwrap(),
        b"fine"
    );
}

#[test]
fn walker_module_is_reachable() {
    // Guards the module path the binary depends on.
    let _: fn(&Path, &Path) -> Vec<walk::Failure> = walk::copy_any;
}
