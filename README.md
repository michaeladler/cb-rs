[![ci](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml)
[![bench](https://github.com/michaeladler/cb-rs/actions/workflows/bench.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/bench.yml)

# cb-rs

A faster, safer Rust rewrite of the file copy/cut/paste core of
[Clipboard](https://github.com/Slackadays/Clipboard) (`cb`), the cut/copy/paste
tool for the command line. It uses the same `$XDG_STATE_HOME/clipboard/<name>`
location and the same `metadata/originals` cut-tracking file, but stores copied
files one level shallower (no per-entry subdirectory) and implements only the
file-based commands. A drop-in for the copy/cut/paste workflow, not for `cb`'s
text clipboard, history, or its other commands.

## Example

```sh
cb copy notes.txt ~/images     # copy, originals stay
cb cut old-build/              # record for moving
cb paste -d /tmp/out           # write into /tmp/out
cb list
```

`--on-conflict skip|replace|ask` decides what happens when the destination already
exists; the default is `skip`.

## Why a rewrite

Upstream `cb` is practically unmaintained and has known bugs, some of them
destructive. A headless `cb paste` after a `cb cut` consumes the clipboard and
deletes the originals without writing anything to the destination; the overwrite
prompt spins at 100% CPU on EOF. See the caveats below for the full list. It can
also be slow (20000 small files, cross-filesystem moves, reflink-capable
filesystems).
This rewrite is **significantly faster** on those shapes, and its move
path fsyncs the destination before unlinking the source, so it's also generally **safer**.

## Build

```sh
cargo build --release
```

## Test

```sh
cargo test
```

**Note**: The reflink tests need a CoW directory.

`scripts/testvol.sh <btrfs|xfs|ext4|zfs> [mountpoint]` creates and mounts a sparse
loopback image of that type on `./mount-<fs>` and chowns the mount root to you.
Point `CB_TESTVOL_DIR` at the mount and `cargo test` runs against it; with no volume
and no env var the reflink and cross-device tests skip.

## Where the clipboard lives

`$XDG_STATE_HOME/clipboard/<name>` (falling back to `~/.local/state/clipboard`), or
whatever `CLIPBOARD_PERSISTDIR` points at. `<name>/data` holds the files,
`<name>/metadata/originals` holds the absolute sources recorded by `cut`.

Copied files land directly in `<name>/data/`, one per top-level item. The C++
`cb` nests them under `<name>/data/<entry>/`, since it keeps a per-clipboard
history of numbered entries. The two tools therefore do not read each
other's clipboard data, even though they share the same root and
`originals` file.

## How it differs from the C++ implementation

- **`cut` is metadata only.** It records absolute source paths and moves them at
  paste time. The original `cb` copies every byte into its clipboard when you cut, so cut plus
  paste costs two copies; here it costs one, and a same-filesystem paste is a
  single `renameat2`.
- **Copy climbs a ladder.** Reflink (`FICLONE`) first, then `copy_file_range`, then
  a 1 MiB buffered stream.
- **Directory walks are work-stealing.** One crossbeam deque per thread over
  `openat`-relative paths, so thousands of **small files are copied in parallel**.
  `cb` walks them one at a time.
- **Cross-filesystem moves are verified before the source is deleted.** Copy,
  fsync the destination, then unlink. The original `cb` deletes originals after a copy it never
  verified.
- **Symlinks are recreated, never followed**, and permission bits are preserved
  on everything that has a mode.

## Benchmarks

`scripts/bench.py [reps]` compares `cb-rs` against `cb` 0.10.0 on copy, cut, and
paste, over three shapes: local (tmpfs → tmpfs), cross-filesystem (tmpfs → btrfs),
and reflink (btrfs → btrfs). The `bench` workflow runs it weekly and publishes the
results at <https://michaeladler.github.io/cb-rs/>.

Headlines from a local run (median of five):

| op    | workload           | cb-rs        | cb           | speedup |
|-------|--------------------|--------------|--------------|---------|
| copy  | 20 000 small files | **52 ms**    | 309 ms       | 5.9×    |
| paste | same tree          | **51 ms**    | 284 ms       | 5.6×    |
| cut   | 20 000 files       | **1 ms**     | 310 ms       | 310×    |
| paste | after that cut     | **1 ms**     | 457 ms       | 457×    |
| copy  | 512 MiB file       | **175 ms**   | 178 ms       | 1.0×    |

Reflink copies collapse to ~1 ms against ~1.2 s, since `cb` has no `FICLONE`
path. Cut is metadata only, so a same-filesystem paste is a single `renameat2`;
cross-filesystem, `cb-rs` copies once at paste while `cb` copies at cut and again
at paste. Where the clipboard is not emptied between copies, `cb` accumulates one
`data/N` directory per copy (5 × 512 MiB leaves 2.6 GB behind, holes
materialized) and `cb-rs` unlinks first.

Read the page for full tables, including the bimodal 512 MiB cross-filesystem
pastes and the per-rung `copy_file_range` numbers behind `CFR_MAX`.
