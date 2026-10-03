#!/usr/bin/env bash
#
# Record scripts/demo-steps.bash and render it as a GIF.
#
#   scripts/record-demo.sh [outdir]
#
# Produces outdir/demo.cast and outdir/demo.gif. The run is hermetic: HOME is a
# temp dir, so it cannot touch the real clipboard or your files.

set -o errexit -o nounset -o pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/demo}
cols=100
rows=24
session=cb-demo-$$

for tool in tmux asciinema agg cargo; do
    command -v $tool >/dev/null || {
                                  echo "missing: $tool" >&2
                                                             exit 1
    }
done

cargo build --release --quiet --manifest-path "$root/Cargo.toml"

work=$(mktemp -d)
trap 'tmux kill-session -t "$session" 2>/dev/null || true; rm -rf "$work" /tmp/cb-demo-state' EXIT
mkdir -p "$out" "$work/home"
export HOME=$work/home
# A fixed, short state path keeps `cb list` output readable in the demo.
export CLIPBOARD_PERSISTDIR=/tmp/cb-demo-state
rm -rf "$CLIPBOARD_PERSISTDIR"
export PATH=$root/target/release:$PATH
export TERM=xterm-256color
unset XDG_STATE_HOME

# tmux only supplies the geometry and a pty, so the cast is the same size on
# every machine; asciinema then records without touching a real terminal. The
# pane command goes through bash, since a non-bash default shell aborts the
# chain when the recording exits non-zero.
tmux new-session -d -s "$session" -x "$cols" -y "$rows" \
    "bash -c 'asciinema rec --headless --return --overwrite --window-size ${cols}x${rows} --idle-time-limit 2 --title cb-rs --command \"bash $root/scripts/demo-steps.bash\" $out/demo.cast; echo \$? > $work/rec.rc'"

while tmux has-session -t "$session" 2>/dev/null; do sleep 0.2; done
# A failed demo step is a broken demo, not a broken recording.
test "$(cat "$work/rec.rc" 2>/dev/null)" = 0 || {
                                                  echo "demo steps failed" >&2
                                                                                exit 1
}

# DejaVu Sans Mono is what agg falls back to here; naming it keeps the output
# stable wherever the font stack differs.
agg --quiet --font-family "DejaVu Sans Mono" --font-size 15 --line-height 1.35 \
    --theme dracula --idle-time-limit 2 --fps-cap 30 --last-frame-duration 3 \
    "$out/demo.cast" "$out/demo.gif"

echo "$out/demo.gif"
