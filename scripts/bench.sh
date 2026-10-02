#!/usr/bin/env bash
# Compare the Rust rewrite against the C++ `cb` on copy / cut / paste.
#
# Two filesystems are in play: $WORK is tmpfs (RAM), ./mount is the btrfs loop
# volume. Each clipboard staging directory is placed next to the data it moves,
# so "same fs" means same fs for both binaries.
#
#   local   : tmpfs source -> tmpfs clipboard -> tmpfs destination
#   crossfs : tmpfs source -> tmpfs clipboard -> btrfs destination
#   reflink : btrfs source -> btrfs clipboard -> btrfs destination
#
# Usage: scripts/bench.sh [repetitions]
set -uo pipefail

REPS=${1:-5}
root=$(cd "$(dirname "$0")/.." && pwd)
RS=$root/target/release/cb-rs
BTRFS=$root/mount
WORK=$HOME/bench

export CLIPBOARD_PERSISTDIR=$WORK/rsstate   # cb-rs staging root
export CLIPBOARD_TMPDIR=$WORK/cbclip        # cb staging root

mkdir -p "$WORK" "$BTRFS/bench"

# ------------------------------------------------------------------ fixtures
mk_small() {  # dir, file count, bytes per file
    python3 - "$@" <<'EOF'
import os, sys
d, n, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
blob = os.urandom(size)
for i in range(n):
    sub = os.path.join(d, "d%03d" % (i % 200))
    os.makedirs(sub, exist_ok=True)
    with open(os.path.join(sub, "f%06d" % i), "wb") as f:
        f.write(blob)
EOF
}

mk_big() {  # dir, name, bytes
    python3 - "$@" <<'EOF'
import os, sys
d, name, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
os.makedirs(d, exist_ok=True)
with open(os.path.join(d, name), "wb") as f:
    f.truncate(size)
    f.seek(size // 2)
    f.write(b"x" * 4096)
EOF
}

count_of() { find "$1" -type f 2>/dev/null | wc -l; }

# cb paste silently does nothing without a terminal, so every cb paste runs
# under a pty. The pty also has to answer cb's overwrite prompt: on EOF it spins
# forever at 100% CPU. cb copy/cut work headless and are timed directly.
ANSWERS=$WORK/answers
cb_paste() {  # destination directory
    [[ -f $ANSWERS ]] || seq 200 | sed 's/.*/n/' >"$ANSWERS"
    (cd "$1" && python3 -c 'import pty, sys; sys.exit(pty.spawn(["cb", "paste"]))' \
        <"$ANSWERS" >/dev/null 2>&1)
}

# -------------------------------------------------------------------- timing
RESET=  # optional command run (untimed) before every repetition

run() {  # median of $REPS runs of the command in "$@"
    local -a t=()
    local i rc
    for ((i = 0; i < REPS; i++)); do
        local s e
        [[ -n $RESET ]] && eval "$RESET"
        s=$(date +%s%N); "$@" >/dev/null 2>&1; rc=$?; e=$(date +%s%N)
        t+=( $(( (e - s) / 1000000 )) )
    done
    printf '%s\n' "${t[@]}" | sort -n | awk -v rc="$rc" '
        { v[NR]=$1 } END { printf "%7.0f ms   min %6d   rc %d", v[int((NR+1)/2)], v[1], rc }'
}

check() {  # path, expected file count
    local got
    got=$(count_of "$1")
    if [[ $got -eq $2 ]]; then printf '   %s files\n' "$got"
    else printf '   !! expected %s files, found %s\n' "$2" "$got"; fi
}

banner() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

rm_rf() { rm -rf "$@" 2>/dev/null; }

# ================================================================== local
banner "local: tmpfs -> tmpfs"
rm_rf "$WORK/src" "$WORK/d_r" "$WORK/d_c" "$WORK/clip"
mkdir -p "$WORK/src" "$WORK/d_r" "$WORK/d_c" "$WORK/clip"
mk_small "$WORK/src/small" 20000 4096
mk_big "$WORK/src/big" big.img $((512 * 1024 * 1024))
echo "fixture: $(count_of "$WORK/src/small") small files, 512 MiB single file"

echo "copy   small   cb-rs"; run "$RS" copy "$WORK/src/small"; check "$WORK/rsstate/0/data/small" 20000
echo "copy   small   cb  "; run cb copy "$WORK/src/small"
RESET='rm -rf "$WORK/d_r/small"'
echo "paste  small   cb-rs"; run "$RS" paste -d "$WORK/d_r"; check "$WORK/d_r/small" 20000
echo "paste  small   cb  "; RESET='rm -rf "$WORK/d_c/small"' run cb_paste "$WORK/d_c"; check "$WORK/d_c/small" 20000

echo "copy   big     cb-rs"; run "$RS" copy "$WORK/src/big"; check "$WORK/rsstate/0/data/big" 1
echo "copy   big     cb  "; run cb copy "$WORK/src/big"
echo "paste  big     cb-rs"; RESET='rm -rf "$WORK/d_r/big"' run "$RS" paste -d "$WORK/d_r"; check "$WORK/d_r/big" 1
echo "paste  big     cb  "; RESET='rm -rf "$WORK/d_c/big"' run cb_paste "$WORK/d_c"; check "$WORK/d_c/big" 1

# cut consumes the source, so each repetition needs a fresh copy of it
cut_bench() {  # source name [dest for cb-rs] [dest for cb]
    local name=$1 dr=${2:-$WORK/cut_r} dc=${3:-$WORK/cut_c}
    mkdir -p "$WORK/cutsrc" "$dr" "$dc"
    echo "--- cut+paste $name -> $dr"
    for impl in rs cb; do
        local -a t=()
        local i
        for ((i = 0; i < REPS; i++)); do
            rm_rf "$WORK/cutsrc/$name"
            cp -a "$WORK/src/$name" "$WORK/cutsrc/$name"
            local s e
            s=$(date +%s%N)
            if [[ $impl == rs ]]; then "$RS" cut "$WORK/cutsrc/$name" >/dev/null 2>&1
            else cb cut "$WORK/cutsrc/$name" >/dev/null 2>&1; fi
            e=$(date +%s%N)
            t+=( $(( (e - s) / 1000000 )) )
            rm_rf "$dr/$name" "$dc/$name"
        done
        printf 'cut    %-7s %s' "$name" "$impl"
        printf '%s\n' "${t[@]}" | sort -n | \
            awk '{ v[NR]=$1 } END { printf "%7.0f ms   min %6d\n", v[int((NR+1)/2)], v[1] }'

        local -a cmd=()
        [[ $impl == rs ]] && cmd=("$RS" paste -d "$dr") || cmd=(cb_paste "$dc")

        local -a pt=()
        for ((i = 0; i < REPS; i++)); do
            rm_rf "$dr/$name" "$dc/$name" "$WORK/cutsrc/$name"
            cp -a "$WORK/src/$name" "$WORK/cutsrc/$name"
            if [[ $impl == rs ]]; then "$RS" cut "$WORK/cutsrc/$name" >/dev/null 2>&1
            else cb cut "$WORK/cutsrc/$name" >/dev/null 2>&1; fi
            local s e
            s=$(date +%s%N); "${cmd[@]}" >/dev/null 2>&1; e=$(date +%s%N)
            pt+=( $(( (e - s) / 1000000 )) )
        done
        printf 'paste  %-7s %s' "$name" "$impl"
        printf '%s\n' "${pt[@]}" | sort -n | \
            awk '{ v[NR]=$1 } END { printf "%7.0f ms   min %6d   ", v[int((NR+1)/2)], v[1] }'
        local got
        got=$(count_of "$dr/$name"); [[ $impl == cb ]] && got=$(count_of "$dc/$name")
        if [[ $got -eq 0 ]]; then printf '!! nothing landed in the destination\n'
        else printf '%s files\n' "$got"; fi
    done
}

cut_bench small
cut_bench big

rm_rf "$WORK/d_r" "$WORK/d_c" "$WORK/cutsrc" "$WORK/rsstate" "$WORK/cbclip"

# =============================================================== cross fs
banner "cross-fs: tmpfs -> btrfs"
rm_rf "$WORK/src" "$BTRFS/bench/d_r" "$BTRFS/bench/d_c" "$WORK/clip"
mkdir -p "$WORK/src" "$BTRFS/bench/d_r" "$BTRFS/bench/d_c"
mk_big "$WORK/src/x" big.img $((512 * 1024 * 1024))
mk_small "$WORK/src/y" 4000 4096

echo "copy   512M    cb-rs"; run "$RS" copy "$WORK/src/x"
echo "copy   512M    cb  "; run cb copy "$WORK/src/x"
echo "paste  512M    cb-rs"; RESET='rm -rf "$BTRFS/bench/d_r/x"' run "$RS" paste -d "$BTRFS/bench/d_r"; check "$BTRFS/bench/d_r/x" 1
echo "paste  512M    cb  "; RESET='rm -rf "$BTRFS/bench/d_c/x"' run cb_paste "$BTRFS/bench/d_c"; check "$BTRFS/bench/d_c/x" 1
echo "copy   4k x4000 cb-rs"; run "$RS" copy "$WORK/src/y"
echo "copy   4k x4000 cb  "; run cb copy "$WORK/src/y"
echo "paste  4k x4000 cb-rs"; RESET='rm -rf "$BTRFS/bench/d_r/y"' run "$RS" paste -d "$BTRFS/bench/d_r"; check "$BTRFS/bench/d_r/y" 4000
echo "paste  4k x4000 cb  "; RESET='rm -rf "$BTRFS/bench/d_c/y"' run cb_paste "$BTRFS/bench/d_c"; check "$BTRFS/bench/d_c/y" 4000

banner "cross-fs cut+paste: tmpfs -> btrfs"
cut_bench x "$BTRFS/bench/cut_r" "$BTRFS/bench/cut_c"
cut_bench y "$BTRFS/bench/cut_r" "$BTRFS/bench/cut_c"

# ================================================================= reflink
banner "reflink: btrfs -> btrfs"
rm_rf "$BTRFS/bench/src" "$BTRFS/bench/d_r" "$BTRFS/bench/d_c" "$WORK/clip"
mkdir -p "$BTRFS/bench/src" "$BTRFS/bench/d_r" "$BTRFS/bench/d_c"
export CLIPBOARD_PERSISTDIR=$BTRFS/bench/rsstate
export CLIPBOARD_TMPDIR=$BTRFS/bench/cbclip
mk_big "$BTRFS/bench/src" big.img $((512 * 1024 * 1024))

echo "copy   512M    cb-rs"; run "$RS" copy "$BTRFS/bench/src"
echo "copy   512M    cb  "; run cb copy "$BTRFS/bench/src"
echo "paste  512M    cb-rs"; RESET='rm -rf "$BTRFS/bench/d_r/src"' run "$RS" paste -d "$BTRFS/bench/d_r"; check "$BTRFS/bench/d_r/src" 1
echo "paste  512M    cb  "; RESET='rm -rf "$BTRFS/bench/d_c/src"' run cb_paste "$BTRFS/bench/d_c"; check "$BTRFS/bench/d_c/src" 1

banner "space"
df -h "$WORK" "$BTRFS" | tail -n +2