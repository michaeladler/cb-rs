#!/usr/bin/env bash
#
# Demo content for cb-rs. Run it through scripts/record-demo.sh, which records
# this with asciinema and renders it with agg. Not meant to be run by hand: it is
# non-interactive (NO_WAIT=true) and expects record-demo.sh's fixture (an empty
# HOME with `cb` on PATH and a ~/demo playground) to already be there.
#
# demo-magic.sh comes from the demo-magic package (see devenv.nix). Override
# the location with DEMO_MAGIC if it is not on PATH or in the nix store.

source "${DEMO_MAGIC:-$(command -v demo-magic.sh)}"

TYPE_SPEED=15
NO_WAIT=true
SHOW_CMD_NUMS=true

# demo-magic renders this prompt by running bash without readline, and that
# path expands \e but not \[ \] -- those only matter to readline's column
# count, so use real escapes here or the brackets show up as literal text.
DEMO_PROMPT=$'\e[1;32mcb-demo\e[0m:\e[1;34m\\W\e[0m$ '

# pe = print, type, execute. The sleeps are the only pacing: they hold the
# frame long enough to read, since no one presses ENTER during a recording.
hold() { sleep "${1:-1.6}"; }

cd ~/demo
p "# a playground"
pe "tree"

p "# copy records the paths, so the originals stay put"
pe "cb copy notes images"

p "# paste does the reading, and does not consume the clipboard"
pe "cb paste -d out"
pe "tree out"

p "# cut records paths too, and paste moves them"
pe "cb cut build"

p "# --amend adds to the clipboard instead of replacing it"
pe "cb copy --amend notes/todo.md"

p "# one paste moves the cut entry and copies the amended one"
pe "cb paste -d mixed"
pe "tree mixed"

# `pe` swallows the exit status of each step, so assert the end state here:
# the recorder fails the run when this script exits non-zero.
test -d ~/demo/mixed/build && test -f ~/demo/mixed/todo.md &&
    ! test -e ~/demo/build && test -f ~/demo/notes/todo.md
