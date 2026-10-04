#!/usr/bin/env python3
"""Compare cb-rs against cb 0.10.0 on a copy+paste and a cut+paste round trip.

Copy and paste are timed as one operation, not two rows. cb-rs records the
source paths, so its copy alone reads a few bytes of state, while cb stages the
whole tree: the copy rows would compare a path-list write against a data copy.
The round trip is the work a user actually asks for, and both binaries are
built around it.

Two filesystems are in play: WORK is tmpfs (RAM), VOL is the loop volume
($CB_BENCH_VOL, ./mount-btrfs by default). Each clipboard staging directory is placed next to the data it moves,
so "same fs" means same fs for both binaries.

    local   : tmpfs source -> tmpfs clipboard -> tmpfs destination
    crossfs : tmpfs source -> tmpfs clipboard -> btrfs destination
    reflink : btrfs source -> btrfs clipboard -> btrfs destination

Needs $HOME/bench to be a tmpfs and the loop volume mounted (scripts/testvol.sh
btrfs or xfs). The bench workflow does both and publishes the output on gh-pages.

Usage: CB_BENCH_VOL=./mount-xfs scripts/bench.py [reps]
"""

import os
import pty
import select
import shutil
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = os.path.join(os.environ["HOME"], "bench")
VOL = os.environ.get("CB_BENCH_VOL") or os.path.join(ROOT, "mount-btrfs")
VOL_WORK = os.path.join(VOL, "bench")
VOL_FS = "?"  # resolved in main(), once VOL exists: findmnt needs the path
BIG = 512 << 20
SMALL_LOCAL = 20000
SMALL_CROSS = 4000
REPS = 5

# cb paste holds the pty open past its own exit by way of children that outlive
# it, so the drain needs a ceiling or the run waits out the workflow timeout.
# Generous: the slowest paste measured here is well under a second.
LIMIT = 120.0
GAP = 5.0  # poll interval only; a quiet read is not a reason to give up

QUIET = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}


# ----------------------------------------------------------------- binaries

class Impl:
    """A binary under test, and the clipboard directory it stages into."""

    def __init__(self, name, bin, paste):
        self.name, self.bin, self._paste = name, bin, paste
        self.clip = None

    def copy(self, src):
        return [self.bin, "copy", src]

    def cut(self, src):
        return [self.bin, "cut", src]

    def paste(self, dest):
        return lambda: self._paste(self.bin, dest)

    def use_clip(self, path):
        self.clip = path
        os.environ[self.env] = path

    @property
    def env(self):
        return "CLIPBOARD_TMPDIR" if self.name == "cb" else "CLIPBOARD_PERSISTDIR"


# ------------------------------------------------------------------ helpers

def run(cmd):
    """Run cmd: either a plain argv list or a zero-argument callable."""
    return cmd() if callable(cmd) else subprocess.call(cmd, **QUIET)


def rm_rf(path):
    shutil.rmtree(path, ignore_errors=True)


def count_of(path):
    return sum(len(files) for _, _, files in os.walk(path))


def mk_small(d, n, size):
    """n files of size bytes, spread 200 to a subdirectory."""
    blob = os.urandom(size)
    for i in range(n):
        sub = os.path.join(d, "d%03d" % (i % 200))
        os.makedirs(sub, exist_ok=True)
        with open(os.path.join(sub, "f%06d" % i), "wb") as f:
            f.write(blob)


def mk_big(d, name, size):
    """One sparse file with a real write in the middle, so no rung of the copy
    ladder can satisfy it out of a hole."""
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "wb") as f:
        f.truncate(size)
        f.seek(size // 2)
        f.write(b"x" * 4096)


def du(path):
    out = subprocess.run(["du", "-sh", path], capture_output=True, text=True)
    return out.stdout.split()[0] if out.stdout.strip() else "-"


def fs_of(path):
    """Filesystem backing path. The banners name what the run is actually on,
    so a machine without the btrfs volume says so rather than claiming CoW."""
    out = subprocess.run(["findmnt", "-n", "-o", "FSTYPE", "-T", path],
                         capture_output=True, text=True)
    return out.stdout.strip() or "?"


# cb paste silently does nothing without a terminal, so every cb paste runs
# under a pty. The pty also has to answer cb's overwrite prompt: on EOF it spins
# forever at 100% CPU. cb copy/cut work headless and are timed directly.
def _drain(master, deadline):
    """Read the pty until cb closes it. True if EOF was reached, False on deadline."""
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            return False
        if not select.select([master], [], [], min(left, GAP))[0]:
            continue  # quiet, not gone: a slow paste is not a wedged one
        try:
            if not os.read(master, 65536):
                return True
        except OSError:
            return True  # EOF read raises on a pty


def cb_paste(bin, dest):
    """Paste into dest with cb's stdin/stdout/stderr on a pty. 200 `n` answers
    are queued up front, which is more prompts than any paste here can produce."""
    master, slave = pty.openpty()
    try:
        p = subprocess.Popen([bin, "paste"], cwd=dest, stdin=slave,
                             stdout=slave, stderr=slave)
        os.close(slave)  # the child holds the only other copy: dropping it here
                         # is what lets the read below reach EOF
        os.write(master, b"n\n" * 200)
        # cb only prompts on a tty, so its output cannot go to /dev/null, and
        # the pty has to be drained to EOF or cb blocks writing into a full
        # buffer and p.wait() blocks forever. The drain gets a deadline, not a
        # per-read timeout: cb paste forks children that outlive it and keep
        # the slave fd open, so EOF can never arrive on its own and the job
        # would sit until the workflow timeout.
        if not _drain(master, time.monotonic() + LIMIT):
            p.kill()
            _drain(master, time.monotonic() + LIMIT)  # unblock the corpse
            return p.wait()
        return p.wait()
    finally:
        os.close(master)


RS = Impl("cb-rs", os.path.join(ROOT, "target/release/cb"),
          lambda bin, dest: run([bin, "paste", "-d", dest]))
CB = Impl("cb", "cb", cb_paste)
IMPLS = (RS, CB)


# -------------------------------------------------------------------- timing

def timed(cmds, reset=None):
    """Median, minimum, and return code of cmds run back to back on one clock.

    cmds is always a list, even for a single command: one argv is itself a list
    of strings, so a bare list would be ambiguous. reset runs untimed before
    each repetition; it may be a list of callables.
    """
    reset = reset or []
    times, rc = [], 0
    for _ in range(REPS):
        for step in reset:
            step()
        start = time.perf_counter_ns()
        for cmd in cmds:
            cmd_rc = run(cmd)
            rc = rc or cmd_rc  # first failure wins, not the last one to hide it
        times.append((time.perf_counter_ns() - start) // 1_000_000)
    return statistics.median(times), min(times), rc


def row(op, workload, impl, med, low, rc, note=""):
    """rc is only worth a column when it is not 0: a failing copy is otherwise
    indistinguishable from a fast one."""
    print("%-11s %-13s %7.0f ms   min %6d   %s"
          % (op, workload + " " + impl, med, low,
             note if rc == 0 else "rc %d" % rc))


def landed(path, expected):
    got = count_of(path)
    if got == expected:
        return "%d files" % got
    return "!! expected %d files, found %d" % (expected, got)


def recorded(impl, src):
    """cb-rs records the source path instead of staging bytes, so the
    post-condition of a copy is a line in the clipboard. `list` is asked rather
    than the state file read, so this does not track the state layout. The file
    count is no longer knowable here -- nothing has been read yet -- but the
    paste row verifies it."""
    out = subprocess.run([impl.bin, "list"], capture_output=True, text=True)
    got = [line.split("\t", 1)[1] for line in out.stdout.splitlines()
           if "\t" in line]
    if os.path.realpath(src) in [os.path.realpath(p) for p in got]:
        return "recorded"
    return "!! %s not recorded" % src


def banner(title):
    print("\n\033[1m== %s\033[0m" % title)


def empty_clip(impl):
    """A new copy or cut replaces the whole clipboard entry, so the entry the
    last run left behind is part of what the next run costs. cb-rs unlinks it;
    cb 0.10.0 copies into a fresh `data/N` directory and never frees the one
    before it. Only the clipboard under test is emptied: emptying both would
    wipe the copy a neighbouring row is about to paste."""
    return lambda: rm_rf(impl.clip)


# ============================================================ copy + paste

def sweep(src, dr, dc, workload, expect, check_copy=False):
    """Copy then paste one workload on both binaries, timed as one round trip.

    The clipboard and the destination both start empty on every repetition: an
    entry left behind by the previous run is part of what the next one costs.
    """
    name = os.path.basename(src)
    for impl in IMPLS:
        dest = dr if impl is RS else dc
        step = os.path.join(dest, name)
        med, low, rc = timed([impl.copy(src), impl.paste(dest)],
                             [empty_clip(impl), lambda: rm_rf(step)])
        note = landed(step, expect)
        if check_copy and impl is RS:
            note = recorded(impl, src) + ", " + note
        row("copy+paste", workload, impl.name, med, low, rc, note)


def cut_bench(name, dr, dc):
    """cut, then paste of the cut, on one clock. cut consumes the source, so
    every repetition needs a fresh copy of it."""
    src = os.path.join(WORK, "src", name)
    cutsrc = os.path.join(WORK, "cutsrc", name)
    for d in (os.path.dirname(cutsrc), dr, dc):
        os.makedirs(d, exist_ok=True)
    print("--- cut+paste %s -> %s" % (name, dr))

    for impl in IMPLS:
        dest = dr if impl is RS else dc
        step = os.path.join(dest, name)

        def fresh():
            rm_rf(cutsrc)
            rm_rf(os.path.join(dr, name))
            rm_rf(os.path.join(dc, name))
            subprocess.run(["cp", "-a", src, cutsrc])
            rm_rf(impl.clip)

        med, low, rc = timed([impl.cut(cutsrc), impl.paste(dest)], [fresh])
        note = landed(step, 1 if name in ("big", "x") else count_of(src))
        row("cut+paste", name, impl.name, med, low, rc, note)


# ===================================================================== main

def main():
    global REPS
    REPS = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    # Piped to tee, print() block-buffers while subprocess writes straight to
    # fd 1, so unbuffered puts df output back under the banner that asked for it.
    sys.stdout.reconfigure(line_buffering=True)
    os.makedirs(WORK, exist_ok=True)
    os.makedirs(VOL_WORK, exist_ok=True)
    global VOL_FS
    VOL_FS = fs_of(VOL)
    for impl in IMPLS:
        impl.use_clip(os.path.join(WORK, "rsstate" if impl is RS else "cbclip"))

    src = os.path.join(WORK, "src")
    d_r, d_c = os.path.join(WORK, "d_r"), os.path.join(WORK, "d_c")

    banner("local: tmpfs -> tmpfs")
    for d in (src, d_r, d_c):
        rm_rf(d)
        os.makedirs(d)
    mk_small(os.path.join(src, "small"), SMALL_LOCAL, 4 * 1024)
    mk_big(os.path.join(src, "big"), "big.img", BIG)
    print("fixture: %d small files, %d MiB single file"
          % (count_of(os.path.join(src, "small")), BIG >> 20))

    sweep(os.path.join(src, "small"), d_r, d_c, "small", SMALL_LOCAL, check_copy=True)
    sweep(os.path.join(src, "big"), d_r, d_c, "big", 1, check_copy=True)

    cut_bench("small", os.path.join(WORK, "cut_r"), os.path.join(WORK, "cut_c"))
    cut_bench("big", os.path.join(WORK, "cut_r"), os.path.join(WORK, "cut_c"))
    for d in (d_r, d_c, os.path.join(WORK, "cutsrc")):
        rm_rf(d)

    banner("cross-fs: tmpfs -> %s" % VOL_FS)
    dr, dc = os.path.join(VOL_WORK, "d_r"), os.path.join(VOL_WORK, "d_c")
    rm_rf(src)
    for d in (src, dr, dc):
        os.makedirs(d, exist_ok=True)
    mk_big(os.path.join(src, "x"), "big.img", BIG)
    mk_small(os.path.join(src, "y"), SMALL_CROSS, 4 * 1024)
    # Both clipboards sit on $WORK (tmpfs) here, so the copy half of the round
    # trip is tmpfs to tmpfs for both binaries; "cross-fs" describes the paste.
    sweep(os.path.join(src, "x"), dr, dc, "%dM" % (BIG >> 20), 1)
    sweep(os.path.join(src, "y"), dr, dc, "4k x%d" % SMALL_CROSS, SMALL_CROSS)

    banner("cross-fs cut+paste: tmpfs -> %s" % VOL_FS)
    cut_bench("x", dr, dc)
    cut_bench("y", dr, dc)

    banner("reflink: %s -> %s" % (VOL_FS, VOL_FS))
    bsrc = os.path.join(VOL_WORK, "src")
    for d in (bsrc, dr, dc):
        rm_rf(d)
        os.makedirs(d, exist_ok=True)
    for impl in IMPLS:
        impl.use_clip(os.path.join(VOL_WORK, "rsstate" if impl is RS else "cbclip"))
    mk_big(bsrc, "big.img", BIG)
    # A reflink row on a filesystem without FICLONE is a copy row wearing the
    # wrong name, so the shape is skipped rather than renamed.
    if VOL_FS not in ("btrfs", "xfs", "zfs"):
        print("skipped: %s has no FICLONE, so this would be a plain copy"
              % VOL_FS)
    else:
        sweep(bsrc, dr, dc, "%dM" % (BIG >> 20), 1)

    banner("retention: 5 copies in a row, nothing emptied in between")
    # Round trips above empty the clipboard first, so the entry a copy leaves
    # behind never lands on the clock. This is the other half of the trade: it
    # costs cb-rs nothing and costs cb a staged copy that is never freed, which
    # the du columns read.
    for impl in IMPLS:
        impl.use_clip(os.path.join(WORK, "rsstate" if impl is RS else "cbclip"))
    ret = os.path.join(WORK, "ret")
    mk_big(ret, "big.img", BIG)
    for impl in IMPLS:
        med, low, _ = timed([impl.copy(ret)])
        print("copy x5 %-6s %4d ms   min %4d ms   clipboard now holds cb-rs %-6s cb %-6s"
              % (impl.name, med, low, du(RS.clip), du(CB.clip)))
        rm_rf(impl.clip)
    rm_rf(ret)

    banner("space")
    subprocess.run(["df", "-h", WORK, VOL])


if __name__ == "__main__":
    main()
