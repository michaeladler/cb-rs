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
cb copy notes.txt ~/images     # record the paths, originals stay
cb list
cb paste -d /tmp/out           # copy still in the clipboard, paste again is fine

cb cut old-build/              # wipes the clipboard, records the paths
cb paste -d /tmp/out           # moves it
```

Neither verb reads a file. Both record absolute source paths, and `paste` does
the reading, so a recorded clipboard costs a few bytes per path no matter how
large the tree is, and a copy that is never pasted costs nothing at all.

That makes them composable. `--amend` adds to the current clipboard instead of
replacing it, so one paste can move some entries and copy others:

```sh
cb cut old-build/              # wipes the clipboard, records the paths
cb copy --amend notes.txt      # adds to it instead of replacing it
cb paste -d /tmp/out           # moves old-build/, copies notes.txt
```

`paste` writes into an existing directory; it does not create one. Note:
`--on-conflict skip|replace|ask` decides what happens when the destination
already exists; the default is `skip`.

## Clipboards

Every verb takes `-n`/`--name <NAME>` to pick a clipboard other than the
default `0`. The flag is global: `cb -n work list` and `cb list -n work` are the
same command.

`list` prints one tab-separated line per recorded path, `cut` or `copy` followed
by the absolute source:

```
cut	/home/me/old-build
copy	/home/me/notes.txt
```

Exit codes are the ones `cb` has always used: `2` for a usage error, with the
message on stderr, and `1` for a runtime failure, with the details on stderr and
a count on stdout. A failed path never aborts the run — `copy` records the paths
that resolved, exits 1, and reports how many did not.

## Install

```sh
cargo install --git https://github.com/michaeladler/cb-rs.git
```

Or from a clone: `cargo build --release`.

## Caveats

Read these before pointing `cb-rs` at anything you care about.

- **A new `copy` or `cut` wipes the clipboard** unless you pass `--amend`. Both lists are removed first. There is no history.
- **`paste` needs an existing destination.** `-d` is never created for you.
- **`copy` records paths, not bytes.** Just like `cut`, nothing is read at copy time. Editing, moving, or deleting a copied file before you paste means paste copies whatever is at that path now, or fails. It is not a snapshot, so `cb copy f && rm f` followed by a paste will not produce `f`. Use `cp` if you want the bytes now.
- **`cut` records paths, not bytes.** Editing, moving, or deleting a cut file before you paste means paste moves whatever is at that path now, or fails.
- **Paste of copied paths does not empty the clipboard.** The sources stay recorded, so a second paste copies them again. Only `cut` consumes.
- **`--on-conflict` is per top-level entry, and paste never merges.** An existing directory is not merged into; the whole entry is skipped, replaced, or, under `replace`, **emptied and renamed over** — replacing a directory deletes everything in it.
- **`ask` needs a terminal.** With stdin not a tty it answers no, so it behaves like `skip`.
- **A cross-filesystem move is verified by "no syscall failed, then fsync", not by comparing content.** It is far better than the original, which deleted the source after an unverified copy, but it is not a checksum.
- **Only permission bits are preserved.** Hardlinks, ownership, timestamps, xattrs, ACLs, and sparse holes are not.
- **Symlinks are recreated, never followed.** A tree whose links point outside itself pastes links that may dangle until the destination has them too.
- **Failures are per entry and do not abort the run.** The count is printed and the exit status is 1, but the remaining items still move.
- **macOS loses fifos and device nodes.** There is no `mknodat` there, so those entries fail; and only the streaming rung of the copy ladder is compiled in, so big-file copies there are at `cb` parity, not better.
- **The clipboard is not shared with the C++ `cb`.** Not the contents and not the directory. See [State](#state).

## Man page and shell completions

See [`man/cb.1`](man/cb.1) and [`completions/`](completions). Copy them where your shell and your system look:

```sh
install -Dm644 man/cb.1              /usr/local/share/man/man1/cb.1
install -Dm644 completions/cb.bash   /etc/bash_completion.d/cb
install -Dm644 completions/_cb       ~/.local/share/zsh/site-functions/_cb
install -Dm644 completions/cb.fish   ~/.config/fish/completions/cb.fish
install -Dm644 completions/cb.elv    ~/.config/elvish/lib/cb.elv
install -Dm644 completions/_cb.ps1   ~/.config/powershell/cb.ps1
```

## Test

```sh
cargo test
```

The reflink tests need a CoW directory.

`scripts/testvol.sh <btrfs|xfs|ext4|zfs> [mountpoint]` creates and mounts a sparse loopback image of that type on `./mount-<fs>` and chowns the mount root to you.
Point `CB_TESTVOL_DIR` at the mount and `cargo test` runs against it; with no volume and no env var the reflink and cross-device tests skip.

## State

`$XDG_STATE_HOME/cb-rs/<name>` (falling back to `~/.local/state/cb-rs`), or
whatever `CLIPBOARD_PERSISTDIR` points at. `<name>` is `0` unless you pass
`-n`/`--name`.
`<name>/metadata/originals` holds the absolute sources recorded by `cut`, `<name>/metadata/copies` the ones recorded by `copy`. Neither holds file data: `paste` reads the sources themselves, so a large tree costs one line per top-level entry.

This is deliberately a different directory from the C++ `cb`'s
`$XDG_STATE_HOME/clipboard`. The two tools cannot read each other's clipboards,
and sharing the directory would have been worse than useless: a new `cut` wipes
the whole clipboard entry, which under the shared root deleted the other tool's
staged bytes. `cb-rs` stages nothing, so it keeps two small lists and nothing
else.

Upgrading from a `cb-rs` that shared the C++ `cb` root leaves that old clipboard
where it is. Delete `~/.local/state/clipboard` by hand once you are sure you
have nothing pending in the C++ `cb`; `cb-rs` no longer reads or writes it.

## How it differs from the C++ implementation

Caveats above cover the semantics; this is where the speed comes from.

- **`copy` and `cut` record paths only,** so recording is O(paths) rather than O(bytes), a copy that is never pasted costs nothing, and a cross-filesystem copy pays one transfer instead of two. A same-filesystem `cut` paste is a single `renameat2`.
- **`--amend` does not exist upstream.** Upstream has no way to add to a clipboard, and its `cut` copies the bytes as well as recording them.
- **Copy climbs a ladder.** Reflink (`FICLONE`) first, then `copy_file_range`, then a 1 MiB buffered stream. This now runs at paste time rather than copy time.
- **Directory walks are work-stealing.** One crossbeam deque per thread over `openat`-relative paths, so thousands of **small files are copied in parallel**. The original `cb` walks them one at a time.
- **Cross-filesystem moves fsync the destination before unlinking the source.** The original `cb` deletes source files after a copy it never verified.

## Benchmarks

`scripts/bench.py [reps]` compares `cb-rs` against `cb` 0.10.0 over three shapes: local (tmpfs → tmpfs), cross-filesystem (tmpfs → loop volume), and reflink (loop volume → loop volume). The volume is `./mount-btrfs` by default and `CB_BENCH_VOL` elsewhere, so the same script runs against `./mount-xfs`. Each row is a whole `copy` + `paste` or `cut` + `paste` round trip on one clock, because that is the unit a user asks for: `cb-rs` records paths, so timing its `copy` alone measures a path-list write while `cb`'s copies the data. The round trip is also where the two designs differ honestly — `cb` moves the bytes twice, `cb-rs` once.
It also runs a retention round — five copies in a row with nothing emptied in between — which is where recording paths shows up as reclaimed disk, since `cb-rs` overwrites two small lists where `cb` stages a fresh `data/N` every time and never frees the last one.
The `bench` workflow runs it weekly against both loop volumes, one run each, and publishes the results at <https://michaeladler.github.io/cb-rs/>.

### Measured

Median of 5 repetitions, whole round trip, on a 16-core machine with the loop volumes from `scripts/testvol.sh`. The local rows are tmpfs on both sides and do not depend on the volume, so they are from the btrfs run.

| round trip               | workload            |   cb-rs | cb 0.10.0 | speedup |
| ------------------------ | ------------------- | ------: | --------: | ------: |
| local (tmpfs → tmpfs)    | copy 20 000 × 4 KiB |   58 ms |    591 ms |     10× |
| local (tmpfs → tmpfs)    | copy 512 MiB        |  134 ms |    359 ms |    2.7× |
| local (tmpfs → tmpfs)    | cut 20 000 × 4 KiB  |    2 ms |    771 ms |    385× |
| local (tmpfs → tmpfs)    | cut 512 MiB         |    2 ms |    424 ms |    212× |
| cross-fs (tmpfs → btrfs) | copy 512 MiB        | 1742 ms |   1758 ms |    1.0× |
| cross-fs (tmpfs → btrfs) | copy 4000 × 4 KiB   |  108 ms |    215 ms |    2.0× |
| cross-fs (tmpfs → btrfs) | cut 512 MiB         | 1122 ms |   1734 ms |    1.5× |
| cross-fs (tmpfs → btrfs) | cut 4000 × 4 KiB    |  250 ms |    236 ms |    0.9× |
| reflink (btrfs → btrfs)  | copy 512 MiB        |    2 ms |   2973 ms |   1487× |
| cross-fs (tmpfs → xfs)   | copy 512 MiB        | 1330 ms |   1747 ms |    1.3× |
| cross-fs (tmpfs → xfs)   | copy 4000 × 4 KiB   |   54 ms |    190 ms |    3.5× |
| cross-fs (tmpfs → xfs)   | cut 512 MiB         | 1065 ms |   1609 ms |    1.5× |
| cross-fs (tmpfs → xfs)   | cut 4000 × 4 KiB    |  251 ms |    214 ms |    0.8× |
| reflink (xfs → xfs)      | copy 512 MiB        |    1 ms |   2840 ms |   2840× |

Read the table with its shape, not as one verdict. The local and reflink rows are where the two designs actually differ: a same-filesystem `cut` is one `renameat2` for `cb-rs`, and a reflink paste is one `FICLONE`, so the round trip collapses to a few milliseconds while `cb` moves every byte twice through user space. The cross-filesystem rows are both bound by the device writing 512 MiB — the ratios there are 1× or worse because there is nothing left to win once the write is the whole cost, and the small-file cross-fs cut is where `cb-rs` actually loses, on its serial `fsync`-before-unlink that `cb` skips entirely.

The 512 MiB cross-filesystem rows are the noisy ones: both binaries spend the run on real I/O to the loop device, and the per-run minimum swung between 200 ms and 1.3 s within a single row. Treat the median as "about a second each" and the ratio there as noise.

Retention, five copies of a 512 MiB file with nothing emptied in between: `cb-rs` 1 ms and 4 KiB of clipboard, `cb` 146 ms (btrfs run) / 192 ms (xfs run) and 2.6 GiB of staged copy it never frees.

Since `copy` and `cut` only record paths, recording 20 000 files or a 512 MiB file is the same handful of microseconds either way: there is no data to read. The data cost lands entirely on `paste`, which walks the tree with the copy ladder — and on `cb`'s `copy`, which stages it first.
