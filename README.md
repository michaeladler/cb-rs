# cb-rs

A Rust rewrite of [Clipboard](https://github.com/Slackadays/Clipboard) (`cb`), the
cut/copy/paste tool for the command line. Same commands, same on-disk clipboard
layout, so you can switch between the two binaries without losing your clipboard.

```sh
cb-rs copy notes.txt ~/images     # copy, originals stay
cb-rs cut old-build/              # record for moving
cb-rs paste -d /tmp/out           # write into /tmp/out
cb-rs list
```

`--on-conflict skip|replace|ask` decides what happens when the destination already
exists; the default is `skip`.

## Build

```sh
cargo build --release      # target/release/cb-rs
cargo test                 # tests/reflink.rs skips itself without a CoW volume
```

The reflink tests need a CoW directory: btrfs, or XFS formatted with
`mkfs.xfs -m reflink=1`. They default to `./mount`, or `CB_BTRFS_DIR` when the
volume lives elsewhere, and skip when the filesystem cannot reflink.
`scripts/btrfs-testvol.sh` creates and mounts the loopback btrfs image at `./mount`
for them and for the cross-filesystem benchmark below.

## Where the clipboard lives

`$XDG_STATE_HOME/clipboard/<name>` (falling back to `~/.local/state/clipboard`), or
whatever `CLIPBOARD_PERSISTDIR` points at. `<name>/data` holds the files,
`<name>/metadata/originals` holds the absolute sources recorded by `cut`.

## How it differs from the C++ implementation

- **`cut` is metadata only.** It records absolute source paths and moves them at
  paste time. `cb` copies every byte into its clipboard when you cut, so cut plus
  paste costs two copies; here it costs one, and a same-filesystem paste is a
  single `renameat2`.
- **Copy climbs a ladder.** Reflink (`FICLONE`) first, then `copy_file_range`, then
  a 1 MiB buffered stream. `copy.rs` reports which rung paid off.
- **Directory walks are work-stealing.** One crossbeam deque per thread over
  `openat`-relative paths, so thousands of small files are copied in parallel.
  `cb` walks them one at a time.
- **Cross-filesystem moves are verified before the source is deleted.** Copy,
  fsync the destination, then unlink. `cb` deletes originals after a copy it never
  re-checked.
- **Symlinks are recreated, never followed**, and permission bits are preserved
  on everything that has a mode.

## Benchmarks

`scripts/bench.sh [reps]` compares `cb-rs` against `cb` 0.10.0 on copy, cut, and
paste. It stages each clipboard next to the data it moves, so "same filesystem"
means the same filesystem for both binaries, and it exercises three shapes:

| shape    | source | clipboard | destination       |
|----------|--------|-----------|-------------------|
| local    | tmpfs  | tmpfs     | tmpfs             |
| cross-fs | tmpfs  | tmpfs     | btrfs (`./mount`) |
| reflink  | btrfs  | btrfs     | btrfs             |

Every row is the median of five runs; the minimum is next to it. All file counts
were verified after the fact.

### Local: tmpfs → tmpfs

Fixture: 20 000 files of 4 KiB across 200 directories, plus one 512 MiB file.

| op    | workload           | cb-rs            | cb               | speedup |
|-------|--------------------|------------------|------------------|---------|
| copy  | 20 000 small files | **129 ms** (48)  | 307 ms (304)     | 2.4×    |
| paste | same tree          | **47 ms** (46)   | 299 ms (297)     | 6.4×    |
| copy  | 512 MiB file       | 226 ms (222)     | **151 ms** (139) | 0.7×    |
| paste | same file          | **191 ms** (184) | 200 ms (200)     | 1.0×    |
| cut   | 20 000 files       | **4 ms** (2)     | 306 ms (303)     | 77×     |
| paste | after that cut     | **3 ms** (2)     | 468 ms (463)     | 156×    |
| cut   | 512 MiB file       | **3 ms** (2)     | 135 ms (132)     | 45×     |
| paste | after that cut     | **3 ms** (3)     | 243 ms (231)     | 81×     |

### Cross-filesystem: tmpfs → btrfs

`./mount` is mounted `compress=zstd`, so every byte written there pays compression.

| op          | workload         | cb-rs           | cb               |
|-------------|------------------|-----------------|------------------|
| copy        | 512 MiB          | 232 ms (178)    | **148 ms** (141) |
| paste       | 512 MiB          | 1667 ms (202)   | 1697 ms (1688)   |
| copy        | 4 000 × 4 KiB    | **31 ms** (28)  | 73 ms (69)       |
| paste       | 4 000 × 4 KiB    | **101 ms** (91) | 150 ms (145)     |
| cut + paste | 512 MiB move     | 3 + 1487 ms     | 145 + 1586 ms    |
| cut + paste | 4 000 files move | 4 + 227 ms      | 68 + 182 ms      |

The 512 MiB paste medians are dominated by zstd compression of the write and are
wildly bimodal (mins of 202 and 1688 ms); read them as a tie. The 4 000-file move is
`cb`'s one good cross-filesystem row: `cb-rs` fsyncs the destination before it
unlinks the source, which is the price of not losing data when a copy comes up
short.

### Reflink: btrfs → btrfs

| op            | cb-rs        | cb            |
|---------------|--------------|---------------|
| copy 512 MiB  | **3 ms** (3) | 1627 ms (370) |
| paste 512 MiB | **3 ms** (2) | 1704 ms (707) |

`cb` has no reflink path, so the same filesystem copy it makes is a full 512 MiB
write; `FICLONE` writes zero bytes and both times collapse to the time it takes to
start the process.

### Where the time goes

- **Cut.** Metadata only versus a full copy. Same-filesystem paste is then one
  `renameat2`, which is why the cut rows are three-digit factors rather than
  double digits.
- **Small files.** The work-stealing walk plus `openat`-relative copying is worth
  2.4× on copy and 6.4× on paste over a 20 000-file tree.
- **Reflink.** The whole ladder in `copy.rs` pays off once the destination is CoW:
  3 ms against 1.7 s.
- **Big files on tmpfs.** `cb` still wins, by 1.5× on a sparse 512 MiB fixture and
  1.3× on a fully dense one (209 ms against 164 ms). Not the per-file metadata
  work: `fstat` plus `fchmod` is two syscalls, microseconds against a 200 ms copy.
  It is the `copy_file_range` rung. Measured on this machine for 512 MiB:

  | rung | median | min |
  |---|---|---|
  | `copy_file_range`, 1 MiB per call | 222 ms | 177 ms |
  | `copy_file_range`, 8 MiB per call | 222 ms | 217 ms |
  | `copy_file_range`, 64 MiB per call | 220 ms | 167 ms |
  | read/write, 1 MiB buffer | 245 ms | 240 ms |
  | read/write, 8 MiB buffer | 298 ms | 292 ms |

  `CFR_MAX` was raised from 1 MiB to 64 MiB on that evidence and took the fixture
  from 251 ms to 226 ms. The remaining gap to `cb` is inside the rung it uses, and
  this machine has no `ptrace`, so no `strace` to attribute it.
- **Cross-filesystem moves.** `cb-rs` copies once, at paste; `cb` copies at cut and
  again at paste, which shows up directly in the two `cut + paste` rows.

### Caveats

- tmpfs is the only fast storage on this machine and caches were not dropped
  between runs, so the absolute numbers are RAM-bandwidth numbers, not disk.
- 16 cores were available; the small-file rows scale with `available_parallelism`,
  and `cb` never uses more than one.
- `cb paste` silently does nothing without a terminal, so the harness runs every
  `cb paste` under a pty (`python3 -c 'import pty; pty.spawn(["cb", "paste"])'`,
  about 25 ms of overhead, included above). The pty is also fed `n` answers, since
  on EOF `cb`'s overwrite prompt spins at 100% CPU forever.
- That no-TTY path has a real bug behind it: a headless `cb paste` after a `cb cut`
  consumes the clipboard and deletes the originals without writing anything to the
  destination. Reproducible on 0.10.0.
