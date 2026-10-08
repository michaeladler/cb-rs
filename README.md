[![ci](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml)
[![bench](https://github.com/michaeladler/cb-rs/actions/workflows/bench.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/bench.yml)
[![codecov](https://codecov.io/github/michaeladler/cb-rs/graph/badge.svg?token=hGEBYzrSIB)](https://codecov.io/github/michaeladler/cb-rs)

# cb-rs

[![demo](./demo/demo.gif)](./demo/demo.gif)

A [faster, safer and smaller](#benchmarks) Rust rewrite of [Clipboard](https://github.com/Slackadays/Clipboard) (`cb`), the cut/copy/paste tool for the command line.
It is a drop-in for the copy/cut/paste workflow, not for `cb`'s text clipboard, history, or its other commands.

## Why a rewrite

Upstream `cb` is practically unmaintained, and its bugs are mostly data loss:

- `cb paste` ignores the directory you give it and pastes into the current one, and a paste after `cb cut` deletes the originals whether or not the copy worked. Combined, that is how a `cut` plus a headless `paste` loses both the files and the bytes. When the destination entry already exists there is no prompt either: a non-tty run replaces it silently, and the prompt that a forced-tty run does reach (`CLIPBOARD_FORCETTY=1`) spins at 100% CPU on EOF instead of reading a line.
- Nothing is ever verified. `cb` has no move at all: `cut` copies the bytes into the clipboard, `paste` copies them back out, and then the originals are deleted. No rename, no fsync, no size or checksum check. `cb-rs` at least fsyncs the destination before unlinking ([why that is still not enough](#caveats)).
- Upstream keeps a history of clipboard entries, but `cb clear`, and history trimming under a byte, age or count limit, delete them outright. Under a shared state root those deletions take the other tool's staged bytes with them. `cb-rs` records paths, stages nothing, and keeps its lists in a [directory of its own](#state).

It is also slow on the workloads that matter: 20 000 small files, cross-filesystem moves, reflink-capable filesystems.
This rewrite fixes the destructive paths and is [**significantly faster**](#benchmarks) on exactly those workloads.

## Benchmarks

`scripts/bench.py [reps]` compares `cb-rs` against `cb` 0.10.0 on whole `copy` + `paste` and `cut` + `paste` round trips (the round trip is the only fair unit, because `cb-rs` records paths, whereas `cb` copies the actual data).
The `bench` workflow runs it weekly against both loop volumes and publishes the results at <https://michaeladler.github.io/cb-rs/>.

| round trip              | workload            |  cb-rs | cb 0.10.0 | speedup |
| ----------------------- | ------------------- | -----: | --------: | ------: |
| local (ext4 → ext4)     | copy 20 000 × 4 KiB | 680 ms |   1623 ms |    2.4× |
| local (ext4 → ext4)     | copy 512 MiB        | 160 ms |    341 ms |    2.1× |
| local (ext4 → ext4)     | cut 20 000 × 4 KiB  |   2 ms |   2315 ms |   1157× |
| local (ext4 → ext4)     | cut 512 MiB         |   1 ms |    400 ms |    400× |
| cross-fs (ext4 → btrfs) | copy 512 MiB        | 606 ms |   1783 ms |    2.9× |
| cross-fs (ext4 → btrfs) | copy 4000 × 4 KiB   | 285 ms |    409 ms |    1.4× |
| cross-fs (ext4 → btrfs) | cut 512 MiB         | 668 ms |   1469 ms |    2.2× |
| cross-fs (ext4 → btrfs) | cut 4000 × 4 KiB    | 430 ms |    560 ms |    1.3× |
| reflink (btrfs → btrfs) | copy 512 MiB        |   2 ms |    910 ms |    455× |
| cross-fs (ext4 → xfs)   | copy 512 MiB        | 251 ms |    799 ms |    3.2× |
| cross-fs (ext4 → xfs)   | copy 4000 × 4 KiB   | 154 ms |    372 ms |    2.4× |
| cross-fs (ext4 → xfs)   | cut 512 MiB         | 302 ms |    330 ms |    1.1× |
| cross-fs (ext4 → xfs)   | cut 4000 × 4 KiB    | 354 ms |    519 ms |    1.5× |
| reflink (xfs → xfs)     | copy 512 MiB        |   2 ms |    179 ms |   89.5× |

**`cb-rs` is faster on every row, from 1.1× to 1157×.**

The binary is smaller too: 393 KiB against 1.3 MiB for `cb` 0.10.0, a 3.4× reduction, from `opt-level = "z"`, fat LTO, `panic = "abort"`, and stripping.

## Example

```sh
cb copy notes.txt ~/images     # record the paths, originals stay
cb list
cb paste -d /tmp/out          # copy still in the clipboard, paste again is fine

cb cut old-build/             # wipes the clipboard, records the paths
cb paste -d /tmp/out          # moves it
```

Neither verb reads a file. Both record absolute source paths, and `paste` does the reading, so a recorded clipboard costs a few bytes per path no matter how large the tree is, and a copy that is never pasted costs nothing at all.

That makes them composable. `--amend` adds to the current clipboard instead of replacing it, so one paste can move some entries and copy others:

```sh
cb cut old-build/             # wipes the clipboard, records the paths
cb copy --amend notes.txt     # adds to it instead of replacing it
cb paste -d /tmp/out          # moves old-build/, copies notes.txt
```

## Install

```sh
cargo install --git https://github.com/michaeladler/cb-rs.git
```

Or from a clone: `cargo build --release`.

## Caveats

Read these before pointing `cb-rs` at anything you care about.

- A new `copy` or `cut` wipes the clipboard unless you pass `--amend`. Both lists are removed first. There is no history.
- `paste` creates `-d` if it is missing, `mkdir -p` style, whole missing parents included, and fails if the path exists as something other than a directory. `--on-conflict skip|replace|ask` (default `skip`) decides what happens when a top-level entry already exists, and paste never merges: the whole entry is skipped, replaced, or, under `replace`, **emptied and renamed over** — replacing a directory deletes everything in it.
- `copy` and `cut` record paths, not bytes, and nothing is read at record time. Editing, moving, or deleting a source before you paste means paste acts on whatever is at that path now, or fails. It is not a snapshot, so `cb copy f && rm f` followed by a paste will not produce `f`. Use `cp` if you want the bytes now.
- Paste of copied paths does not empty the clipboard. The sources stay recorded, so a second paste copies them again. Only `cut` consumes.
- `ask` needs a terminal. With stdin not a tty it answers no, so it behaves like `skip`.
- A cross-filesystem move is verified by "no syscall failed, then fsync", not by comparing content. The original copied into its clipboard, copied back out, and deleted the originals with nothing checked at any step, but this is still not a checksum.
- Permission bits and access/modification times are preserved on copies and cross-filesystem moves. When run as root, `cb-rs` also preserves owner and group. Same-filesystem moves use rename and keep filesystem metadata; copies do not preserve xattrs, ACLs, sparse holes, or hardlink relationships (each link becomes its own file).
- Symlinks are recreated, never followed. A tree whose links point outside itself pastes links that may dangle until the destination has them too.
- Failures are per entry and do not abort the run. The count is printed and the exit status is 1, but the remaining items still move.
- macOS lacks `mknodat`; fifo and device-node creation uses path-based `mknod` and can fail where OS permissions deny it. Only the streaming rung of the copy ladder is compiled in, so big-file copies there are at `cb` parity, not better.
- The clipboard is not shared with the C++ `cb`. Not the contents and not the directory. See [State](#state).

## Clipboards

Every verb takes `-n`/`--name <NAME>` to pick a clipboard other than the default `0`. The flag is global: `cb -n work list` and `cb list -n work` are the same command.

`list` prints one tab-separated line per recorded path, `cut` or `copy` followed by the absolute source:

```
cut	/home/me/old-build
copy	/home/me/notes.txt
```

## Exit codes

The ones `cb` has always used: `2` for a usage error, with the message on stderr, and `1` for a runtime failure, with the details on stderr and a count on stdout.

## How it differs from the C++ implementation

Caveats above cover the semantics; this is where the speed comes from.

- `copy` and `cut` record paths only, so recording is O(paths) rather than O(bytes), a copy that is never pasted costs nothing, and a cross-filesystem copy pays one transfer instead of two. A same-filesystem `cut` paste is a single `renameat2`.
- `--amend` does not exist upstream. Upstream has no way to add to a clipboard, and its `cut` copies the bytes as well as recording them.
- Copy climbs a ladder: reflink (`FICLONE`) first, then `copy_file_range`, then a 1 MiB buffered stream. This now runs at paste time rather than copy time.
- Directory walks are work-stealing: one crossbeam deque per thread over `openat`-relative paths, so thousands of small files are copied in parallel. The original `cb` walks them one at a time.
- Cross-filesystem moves fsync the destination before unlinking the source. The original `cb` has no move path: it copies into the clipboard, copies back out, and deletes the sources without verifying either copy.

## State

`$XDG_STATE_HOME/cb-rs/<name>` (falling back to `~/.local/state/cb-rs`), or whatever `CLIPBOARD_PERSISTDIR` points at. `<name>` is `0` unless you pass `-n`/`--name`.
`<name>/metadata/originals` holds the absolute sources recorded by `cut`, `<name>/metadata/copies` the ones recorded by `copy`. Neither holds file data: `paste` reads the sources themselves, so a large tree costs one line per top-level entry.

This is deliberately a different directory from the C++ `cb`'s `$XDG_STATE_HOME/clipboard`. The two tools cannot read each other's clipboards, and sharing the directory would have been worse than useless: every upstream entry holds real bytes, and `cb clear` or a history trim under a byte, age or count limit deletes an entry outright, which under the shared root took the other tool's staged data with it. `cb-rs` stages nothing, so it keeps two small lists and nothing else.

Upgrading from a `cb-rs` that shared the C++ `cb` root leaves that old clipboard where it is. Delete `~/.local/state/clipboard` by hand once you are sure you have nothing pending in the C++ `cb`; `cb-rs` no longer reads or writes it.

## Development

`nix develop` gives you cargo and rustc and hydrates `target/` with dependencies that are already compiled in the Nix store, so `cargo build` compiles only this crate. That works because the shell resolves crates from the same vendored store path the store build used; when the two disagree, cargo marks every dependency dirty (`PathToSourceChanged`) and rebuilds the lot.

The flip side is that crates.io is replaced by that vendor directory, so `cargo add` cannot fetch anything new inside the shell:

```sh
unset CARGO_HOME   # back to crates.io for this shell; deps recompile once
```

Changing `Cargo.toml` or `Cargo.lock` needs a fresh `nix develop` so the vendor directory matches the new lock, which direnv does for you on save. `reseed-target` deletes `target/` and re-hydrates it from the store; reach for it after a rustc bump leaves stale fingerprints behind.

## Test

```sh
cargo test
```

The reflink and cross-device tests need a CoW volume: `scripts/testvol.sh <btrfs|xfs|ext4|zfs> [mountpoint]` creates and mounts a sparse loopback image on `./mount-<fs>` and prints where it is. Point `CB_TESTVOL_DIR` at the mount; with no volume and no env var those tests skip.

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
