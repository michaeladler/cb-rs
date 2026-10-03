#!/usr/bin/env python3
"""Render bench.log as a page of tables.

bench.py prints a fixed-width text log because a text log is the artifact worth
uploading. On the page the same numbers read better side by side, so this pairs
each cb-rs row with its cb row and prints the ratio.

Usage: scripts/benchhtml.py bench.log index.html
"""

import datetime
import html
import os
import platform
import re
import sys
from collections import namedtuple

IMPLS = ("cb-rs", "cb")
ANSI = re.compile(r"\x1b\[[0-9;]*m")  # banner() writes bold escapes
COLUMN = re.compile(r"\S {2,}\S")  # df-style output: gaps of two spaces or more


# ------------------------------------------------------------------- parsing

def row(line):
    """One measurement row, or None. cb-rs and cb differ only in the name, so a
    row is (op, workload, impl, median_ms, min_ms, note)."""
    toks = line.split()
    for i, tok in enumerate(toks):
        if tok in IMPLS:
            break
    else:
        return None
    rest = toks[i + 1:]
    # <median> ms min <low> [ms] [note...]
    if len(rest) < 4 or rest[1] != "ms" or rest[2] != "min" or not rest[3].isdigit():
        return None
    tail = rest[4:]
    if tail[:1] == ["ms"]:  # retention rows put the unit on the minimum too
        tail = tail[1:]
    note = " ".join(tail)
    return (toks[0], " ".join(toks[1:i]), toks[i], int(rest[0]), int(rest[3]), note)


Pair = namedtuple("Pair", "op workload got notes")


def pairs(lines):
    """[Pair, ...] in log order. A row whose partner is missing stays on its own
    rather than being dropped."""
    out = []
    for line in lines:
        r = row(line)
        if r is None:
            continue
        op, workload, impl, med, low, note = r
        # cut blocks interleave (cb-rs cut, cb-rs paste, cb cut, cb paste), so the
        # partner is the most recent entry with this key still missing this impl,
        # not simply the previous one.
        partner = next((p for p in reversed(out)
                        if (p.op, p.workload) == (op, workload)
                        and impl not in p.got), None)
        if partner is None:
            out.append(Pair(op, workload, {impl: (med, low)},
                            {impl: note} if note else {}))
        else:
            partner.got[impl] = (med, low)
            if note:
                partner.notes[impl] = note
    return out


# ------------------------------------------------------------------ rendering

def ratio(cb, rs):
    if not cb or not rs:
        return "&mdash;"
    r = cb / rs
    return ("%.1f&times;" % r) if r < 10 else ("%d&times;" % r)


def cell(impl, p):
    """Median with the minimum behind it, then that impl's own note. The note
    goes here rather than in a shared column because the retention rows print a
    different clipboard size per impl, and one merged cell would read as two
    contradictory claims. Green only the faster of the pair."""
    if impl not in p.got:
        return "<td>&mdash;</td>"
    win = p.got.get(IMPLS[1] if impl == IMPLS[0] else IMPLS[0])
    win = win is not None and p.got[impl][0] < win[0]
    note = p.notes.get(impl)
    return "<td%s><b>%d ms</b> <small>%d</small>%s</td>" % (
        ' class="win"' if win else "", p.got[impl][0], p.got[impl][1],
        '<small class="note">%s</small>' % html.escape(note) if note else "")


HEAD = ("<thead><tr><th>op</th><th>workload</th>"
        "<th>cb-rs</th><th>cb</th><th>speedup</th></tr></thead>")


def table(entries, head=True):
    """head=False for a continuation table under the same h2: repeating the
    column labels on every cut+paste sub-block reads as a new dataset."""
    rows = []
    for p in entries:
        got = p.got
        rs, cb = got.get(IMPLS[0]), got.get(IMPLS[1])
        speed = "<td>%s</td>" % ratio(cb[0], rs[0]) if rs and cb else "<td></td>"
        rows.append("<tr><td>%s</td><td>%s</td>%s%s</tr>" % (
            html.escape(p.op), html.escape(p.workload),
            "".join(cell(i, p) for i in IMPLS), speed))
    return "<table>%s<tbody>%s</tbody></table>" % (
        HEAD if head else "", "".join(rows))


def render(log):
    """Walk the log in order. Rows between two non-row lines become one table, so
    a `fixture:` or `---` line stays where bench.py printed it instead of drifting
    into the next section."""
    out, pending, pending_lines = [], [], []
    headed = False  # the first *table* in a section carries the column header

    def flush():
        nonlocal headed
        if pending:
            out.append(table(pairs(pending), head=not headed))
            headed = True
            pending.clear()
        if pending_lines:
            out.append("<pre>%s</pre>" % html.escape("\n".join(pending_lines)))
            pending_lines.clear()

    for line in ANSI.sub("", log).splitlines():
        stripped = line.strip()
        if stripped.startswith("== "):
            flush()
            headed = False
            out.append("<h2>%s</h2>" % html.escape(stripped[3:]))
        elif row(line) is not None:
            pending.append(line)
        elif stripped:
            # A run of df-style lines is one pre block, so it must not flush
            # itself out on every line.
            if COLUMN.search(stripped) and not stripped.startswith("--- "):
                if not pending_lines:
                    flush()
                pending_lines.append(line)
                continue
            flush()
            if stripped.startswith("--- "):
                out.append("<h3>%s</h3>" % html.escape(stripped[4:]))
            else:
                out.append('<p class="note">%s</p>' % html.escape(stripped))
        # blank lines only separate blocks; they carry nothing
    flush()
    return "\n".join(out)


CSS = """
:root { color-scheme: light dark; --fg: #1b1b1b; --dim: #6a6a6a; --bg: #fff;
        --line: #e3e3e3; --win: #0a6b3d; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #e6e6e6; --dim: #9a9a9a; --bg: #14161a; --line: #2a2e35;
          --win: #4ade80; }
}
body { background: var(--bg); color: var(--fg);
       font: 15px/1.55 system-ui, sans-serif;
       max-width: 52rem; margin: 3rem auto; padding: 0 1.25rem; }
h1 { font-size: 1.6rem; margin-bottom: .25rem; }
h2 { font-size: 1.05rem; margin: 2.25rem 0 .5rem; padding-bottom: .3rem;
     border-bottom: 1px solid var(--line); }
p.lede { color: var(--dim); margin-top: 0; }
dl { display: grid; grid-template-columns: max-content 1fr; gap: .2rem 1rem;
     margin: 1.25rem 0; font-size: .92rem; }
dt { font-weight: 600; } dd { margin: 0; color: var(--dim); }
table { border-collapse: collapse; width: 100%; font-size: .92rem;
        font-variant-numeric: tabular-nums; }
th, td { text-align: left; padding: .4rem .6rem; border-bottom: 1px solid var(--line); }
h3 { font-size: .92rem; font-weight: 600; margin: 1.4rem 0 .2rem; color: var(--dim); }
p.note { color: var(--dim); font-size: .88rem; margin: .3rem 0; }
small.note { display: block; }
th { font-weight: 600; color: var(--dim); font-size: .82rem;
     text-transform: uppercase; letter-spacing: .04em; }
th:nth-child(n+3), td:nth-child(n+3) { text-align: right; }
td small, small { color: var(--dim); }
.win b { color: var(--win); }
pre { font: 13px/1.5 ui-monospace, monospace; color: var(--dim);
      overflow-x: auto; margin: .6rem 0;
      background: color-mix(in srgb, var(--fg) 5%, transparent);
      padding: .8rem 1rem; border-radius: 6px; }
a { color: inherit; }
"""


def page(log, reps, date, sha, repo, runner):
    return """<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>cb-rs benchmarks</title>
<style>%s</style>
<h1>cb-rs benchmarks</h1>
<p class="lede">scripts/bench.py, %s reps per row, against <code>cb</code> 0.10.0.
Median of the reps, minimum in brackets; the faster side of each pair is green.</p>
<dl>
  <dt>run</dt><dd>%s</dd>
  <dt>commit</dt><dd><a href="%scommit/%s">%s</a></dd>
  <dt>runner</dt><dd>%s</dd>
</dl>
<p>See the <a href="%s#benchmarks">README</a> for what each shape measures and how
to run this yourself.</p>
%s
</html>
""" % (CSS, html.escape(reps), date, repo, sha, sha,
       html.escape(runner), repo, render(log))


def main():
    log, out = sys.argv[1], sys.argv[2]
    with open(log, errors="replace") as f:
        text = f.read()
    sha = os.environ.get("GITHUB_SHA", "local")
    repo = os.environ.get("GITHUB_REPOSITORY", "michaeladler/cb-rs")
    with open(out, "w") as f:
        f.write(page(
            text,
            os.environ.get("REPS", "5"),
            datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
            html.escape(sha[:12]),
            html.escape(repo).join(("https://github.com/", "/")),
            html.escape("%s, %s cores" % (platform.platform(), os.cpu_count())),
        ))


if __name__ == "__main__":
    main()