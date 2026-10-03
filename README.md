[![ci](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/michaeladler/cb-rs/actions/workflows/ci.yml)

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
paste. It stages each clipboard next to the data it moves, so "same filesystem"
means the same filesystem for both binaries, and it exercises three shapes:

| shape    | source | clipboard | destination       |
|----------|--------|-----------|-------------------|
| local    | tmpfs  | tmpfs     | tmpfs             |
| cross-fs | tmpfs  | tmpfs     | btrfs (`./mount-btrfs`) |
| reflink  | btrfs  | btrfs     | btrfs             |

Every row is the median of five runs; the minimum is next to it. All file counts
were verified after the fact.

### Local: tmpfs → tmpfs

Fixture: 20 000 files of 4 KiB across 200 directories, plus one sparse 512 MiB
file (a hole on either side of one 4 KiB real write, so no rung of the copy
ladder can answer it out of a hole).

| op    | workload           | cb-rs            | cb               | speedup |
|-------|--------------------|------------------|------------------|---------|
| copy  | 20 000 small files | **52 ms** (48)   | 309 ms (304)     | 5.9×    |
| paste | same tree          | **51 ms** (46)   | 284 ms (279)     | 5.6×    |
| copy  | 512 MiB file       | **175 ms** (170) | 178 ms (173)     | 1.0×    |
| paste | same file          | 221 ms (219)     | **212 ms** (208) | 0.96×   |
| cut   | 20 000 files       | **1 ms** (1)     | 310 ms (309)     | 310×    |
| paste | after that cut     | **1 ms** (0)     | 457 ms (451)     | 457×    |
| cut   | 512 MiB file       | **1 ms** (1)     | 181 ms (180)     | 181×    |
| paste | after that cut     | **1 ms** (0)     | 283 ms (276)     | 283×    |

### Cross-filesystem: tmpfs → btrfs

`./mount-btrfs` is a plain btrfs loop volume, no compression, so the cross-filesystem
rows measure the copy path and nothing else.

| op          | workload         | cb-rs              | cb               |
|-------------|------------------|--------------------|------------------|
| copy        | 512 MiB          | 182 ms (176)       | 180 ms (163)     |
| paste       | 512 MiB          | 3196 ms (189)      | 1727 ms (1454)   |
| copy        | 4 000 × 4 KiB    | **19 ms** (15)     | 71 ms (66)       |
| paste       | 4 000 × 4 KiB    | **85 ms** (78)     | 140 ms (131)     |
| cut + paste | 512 MiB move     | 1 + 1165 ms        | 190 + 1125 ms    |
| cut + paste | 4 000 files move | 1 + 229 ms         | 70 + 166 ms      |

Both 512 MiB pastes are bimodal: `cb-rs` ranged from 189 ms to 3.2 s and `cb`
from 1454 ms to 1727 ms. On the minimum `cb-rs` wins by 7.7×; on the median `cb`
wins. Read that row as unstable, not as a verdict. The 4 000-file move is `cb`'s
one good cross-filesystem row: `cb-rs` fsyncs the destination before it unlinks
the source, which is the price of not losing data when a copy comes up short.

### Reflink: btrfs → btrfs

| op            | cb-rs        | cb             |
|---------------|--------------|----------------|
| copy 512 MiB  | **1 ms** (0) | 1156 ms (1026) |
| paste 512 MiB | **1 ms** (1) | 1689 ms (149)  |

`cb` has no reflink path, so the same filesystem copy it makes is a full 512 MiB
write; `FICLONE` writes zero bytes and both times collapse to the time it takes to
start the process.

### Retention: five copies in a row, nothing emptied in between

Every copy row above empties the clipboard first, because that is the only way to
time a copy rather than the cost of the previous run's leftovers. The other half
of the trade is what happens when nothing empties it.

| op              | cb-rs        | cb         |
|-----------------|--------------|------------|
| 5 × copy 512 MiB | 260 ms (199) | 165 ms (148) |
| clipboard after | 512 M        | 2.6 G      |

`cb-rs` unlinks the previous entry before staging the new one, so the clipboard
holds one payload either way. `cb` copies into a fresh `data/N` directory and
never frees the one before it, so five copies of a sparse 512 MiB file cost
2.6 GB: 5.2× the source, since `cb` materializes the holes.

### Where the time goes

- **Cut.** Metadata only versus a full copy. Same-filesystem paste is then one
  `renameat2`, which is why the cut rows are three-digit factors rather than
  double digits.
- **Small files.** The work-stealing walk plus `openat`-relative copying is worth
  5.9× on copy and 5.6× on paste over a 20 000-file tree.
- **Reflink.** The whole ladder in `copy.rs` pays off once the destination is CoW:
  1 ms against 1.2 s.
- **Big files on tmpfs.** A tie, and it used not to be: an earlier run had `cb`
  ahead by 1.5× here (226 ms against 151 ms). Both binaries now sit at ~175 ms for
  the copy and ~215 ms for the paste, i.e. inside the `copy_file_range` rung plus
  process startup. Per-file metadata work is not the difference either way:
  `fstat` plus `fchmod` is two syscalls against a 200 ms copy. The rung, measured
  separately on this machine for 512 MiB:

  | rung | median | min |
  |---|---|---|
  | `copy_file_range`, 1 MiB per call | 222 ms | 177 ms |
  | `copy_file_range`, 8 MiB per call | 222 ms | 217 ms |
  | `copy_file_range`, 64 MiB per call | 220 ms | 167 ms |
  | read/write, 1 MiB buffer | 245 ms | 240 ms |
  | read/write, 8 MiB buffer | 298 ms | 292 ms |

  `CFR_MAX` was raised from 1 MiB to 64 MiB on that evidence. What is left is
  inside the rung both binaries use, and this machine has no `ptrace`, so no
  `strace` to attribute it further.
- **Cross-filesystem moves.** `cb-rs` copies once, at paste; `cb` copies at cut and
  again at paste, which shows up directly in the two `cut + paste` rows.

### Caveats

- tmpfs is the only fast storage on this machine and caches were not dropped
  between runs, so the absolute numbers are RAM-bandwidth numbers, not disk.
- 16 cores were available; the small-file rows scale with `available_parallelism`,
  and `cb` never uses more than one.
- `cb paste` silently does nothing without a terminal, so the harness runs every
  `cb paste` under a pty. That path costs a median of 3 ms on an empty clipboard,
  and it is included above. The pty is also fed `n` answers, since on EOF `cb`'s
  overwrite prompt spins at 100% CPU forever.
- That no-TTY path has a real bug behind it: a headless `cb paste` after a `cb cut`
  consumes the clipboard and deletes the originals without writing anything to the
  destination. Reproducible on 0.10.0.
