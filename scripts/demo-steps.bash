#!/usr/bin/env bash
#
# Demo content for cb-rs. Run it through scripts/record-demo.sh, which records
# this with asciinema and renders it with agg. Not meant to be run by hand: it is
# non-interactive (NO_WAIT=true) and assumes an empty HOME with `cb` on PATH.
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

p "# a playground"
pe "mkdir -p ~/demo/notes ~/demo/build ~/demo/images/raw && cd ~/demo"
hold
pe "echo 'ship it' > notes/todo.md"
pe "echo 'int main() {}' > build/main.c"
pe "head -c 8M /dev/urandom > images/raw/photo.bin"
hold
pe "ls -R"

p "# copy records the paths, so the originals stay put"
pe "cb copy notes images"
pe "cb list"
hold

p "# paste does the reading, and does not consume the clipboard"
pe "mkdir -p out && cb paste -d out"
hold
pe "ls out images/raw"

p "# a second paste skips, since the entries are already there"
pe "cb paste -d out"
hold 2.5

p "# cut records paths too, and paste moves them"
pe "cb cut build"
hold

p "# --amend adds to the clipboard instead of replacing it"
pe "cb copy --amend notes/todo.md"
pe "cb list"
hold

p "# one paste moves the cut entry and copies the amended one"
pe "mkdir -p mixed && cb paste -d mixed"
hold
pe "ls mixed"

p "# only the cut was consumed"
pe "cb list"
hold 2.5

# `pe` swallows the exit status of each step, so assert the end state here:
# the recorder fails the run when this script exits non-zero.
test -d ~/demo/mixed/build && test -f ~/demo/mixed/todo.md &&
    ! test -e ~/demo/build && test -f ~/demo/notes/todo.md
