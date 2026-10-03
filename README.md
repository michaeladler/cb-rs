[![ci](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml)
[![bench](https://github.com/michaeladler/cb-rs/actions/workflows/bench.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/bench.yml)

# cb-rs

[![demo](./demo/demo.gif)](./demo/demo.gif)

A faster, safer Rust rewrite of [Clipboard](https://github.com/Slackadays/Clipboard) (`cb`), the cut/copy/paste tool for the command line.
It is a drop-in for the copy/cut/paste workflow, not for `cb`'s text clipboard, history, or its other commands.

## Why a rewrite

Upstream `cb` is practically unmaintained and has known bugs, some of them destructive.
A headless `cb paste` after a `cb cut` consumes the clipboard and deletes the originals without writing anything to the destination; the overwrite prompt spins at 100% CPU on EOF.
It can also be slow (20000 small files, cross-filesystem moves, reflink-capable filesystems).
This rewrite is **significantly faster** on those shapes, and its move path fsyncs the destination before unlinking the source, so it's also generally **safer**.

## Example

```sh
cb copy notes.txt ~/images     # copy, originals stay
cb list
cb paste -d /tmp/out           # copy still in the clipboard, paste again is fine

cb cut old-build/              # wipes the clipboard, records the paths
cb paste -d /tmp/out           # moves it
```

Note: `--on-conflict skip|replace|ask` decides what happens when the destination already exists; the default is `skip`.

## Install

```sh
cargo install --git https://github.com/michaeladler/cb-rs.git
```

Or from a clone: `cargo build --release`.

## Caveats

Read these before pointing `cb-rs` at anything you care about.

- **A new `copy` or `cut` wipes the clipboard.** `data/` and `originals` are removed first. There is no history and no stacking, by design.
- **`cut` records paths, not bytes.** Nothing is copied at cut time, so editing, moving, or deleting a cut file before you paste means paste moves whatever is at that path now, or fails. It is not a snapshot.
- **Paste of copied data does not empty the clipboard.** The files stay, so a second paste copies them again. Only `cut` consumes.
- **`--on-conflict` is per top-level entry, and paste never merges.** An existing directory is not merged into; the whole entry is skipped, replaced, or, under `replace`, **emptied and renamed over** — replacing a directory deletes everything in it.
- **`ask` needs a terminal.** With stdin not a tty it answers no, so it behaves like `skip`.
- **A cross-filesystem move is verified by "no syscall failed, then fsync", not by comparing content.** It is far better than the original, which deleted the source after an unverified copy, but it is not a checksum.
- **Only permission bits are preserved.** Hardlinks, ownership, timestamps, xattrs, ACLs, and sparse holes are not.
- **Symlinks are recreated, never followed.** A tree whose links point outside itself pastes links that may dangle until the destination has them too.
- **Failures are per entry and do not abort the run.** The count is printed and the exit status is 1, but the remaining items still move.
- **macOS loses fifos and device nodes.** There is no `mknodat` there, so those entries fail; and only the streaming rung of the copy ladder is compiled in, so big-file copies there are at `cb` parity, not better.
- **Clipboard data is not shared with the C++ `cb`**, even though the root directory is. See [State](#state).

## Man page and shell completions

Both are generated from the CLI, so they cannot drift:

```sh
cb man > cb.1
cb completions bash > /etc/bash_completion.d/cb
cb completions zsh  > ~/.local/share/zsh/site-functions/_cb
cb completions fish > ~/.config/fish/completions/cb.fish
```

`elvish` and `powershell` work the same way. `cb completions --help` explains the command itself, which is fine inside a completion script.

## Test

```sh
cargo test
```

The reflink tests need a CoW directory.

`scripts/testvol.sh <btrfs|xfs|ext4|zfs> [mountpoint]` creates and mounts a sparse loopback image of that type on `./mount-<fs>` and chowns the mount root to you.
Point `CB_TESTVOL_DIR` at the mount and `cargo test` runs against it; with no volume and no env var the reflink and cross-device tests skip.

## State

`$XDG_STATE_HOME/clipboard/<name>` (falling back to `~/.local/state/clipboard`), or whatever `CLIPBOARD_PERSISTDIR` points at.
`<name>/data` holds the files, `<name>/metadata/originals` holds the absolute sources recorded by `cut`.

Copied files land directly in `<name>/data/`, one per top-level item.
The C++ `cb` nests them under `<name>/data/<entry>/`, since it keeps a per-clipboard history of numbered entries.
The two tools therefore do not read each other's clipboard data, even though they share the same root and `originals` file.

## How it differs from the C++ implementation

Caveats above cover the semantics; this is where the speed comes from.

- **`cut` records paths only,** so cut plus paste costs one copy, not two, and a same-filesystem paste is a single `renameat2`.
- **Copy climbs a ladder.** Reflink (`FICLONE`) first, then `copy_file_range`, then a 1 MiB buffered stream.
- **Directory walks are work-stealing.** One crossbeam deque per thread over `openat`-relative paths, so thousands of **small files are copied in parallel**. The original `cb` walks them one at a time.
- **Cross-filesystem moves fsync the destination before unlinking the source.** The original `cb` deletes source files after a copy it never verified.

## Benchmarks

`scripts/bench.py [reps]` compares `cb-rs` against `cb` 0.10.0 on copy, cut, and paste, over three shapes: local (tmpfs → tmpfs), cross-filesystem (tmpfs → btrfs), and reflink (btrfs → btrfs).
The `bench` workflow runs it weekly and publishes the results at <https://michaeladler.github.io/cb-rs/>.

Headlines from a local run (median of five):

| op    | workload           | cb-rs        | cb           | speedup |
|-------|--------------------|--------------|--------------|---------|
| copy  | 20 000 small files | **52 ms**    | 309 ms       | 5.9×    |
| paste | same tree          | **51 ms**    | 284 ms       | 5.6×    |
| cut   | 20 000 files       | **1 ms**     | 310 ms       | 310×    |
| paste | after that cut     | **1 ms**     | 457 ms       | 457×    |
| copy  | 512 MiB file       | **175 ms**   | 178 ms       | 1.0×    |
