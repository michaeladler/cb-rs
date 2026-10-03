import contextlib, io, os, re, sys, unittest
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "scripts"))
import bench
import benchhtml

LOG = """Filesystem      Size  Used Avail Use% Mounted on
tmpfs           8.0G  606M  7.5G   8% /home/runner/bench

== local: tmpfs -> tmpfs
fixture: 20000 small files, 512 MiB single file
copy  small cb-rs       318 ms   min    318   20000 files
copy  small cb          479 ms   min    479
paste small cb-rs       316 ms   min    316   20000 files
paste small cb          479 ms   min    479   20000 files
--- cut+paste small -> /home/runner/bench/cut_r
cut   small cb-rs         1 ms   min      1
paste small cb-rs         0 ms   min      0   20000 files
cut   small cb          475 ms   min    475
paste small cb          700 ms   min    700   20000 files

== retention: 5 copies in a row
copy x5 cb-rs   558 ms   min  558 ms   clipboard now holds cb-rs 512M   cb 4.0K
copy x5 cb     1069 ms   min 1069 ms   clipboard now holds cb-rs 0      cb 513M

== space
"""


class Test(unittest.TestCase):
    def test_row(self):
        self.assertEqual(
            benchhtml.row("copy  small cb-rs       318 ms   min    318   20000 files"),
            ("copy", "small", "cb-rs", 318, 318, "20000 files"))
        # retention rows carry a "ms" after the minimum
        self.assertEqual(
            benchhtml.row("copy x5 cb     1069 ms   min 1069 ms   clipboard now holds cb 0"),
            ("copy", "x5", "cb", 1069, 1069, "clipboard now holds cb 0"))
        # headers, notes and empty lines are not rows
        for ln in ("fixture: 20000 small files,", "Filesystem  Size  Used",
                   "== local: tmpfs", "", "--- cut+paste small -> /x"):
            self.assertIsNone(benchhtml.row(ln), ln)

    def test_pairs(self):
        entries = benchhtml.pairs(LOG.splitlines())
        got = {(e.op, e.workload): e.got for e in entries}
        self.assertEqual(got[("copy", "small")], {"cb-rs": (318, 318), "cb": (479, 479)})
        self.assertEqual(got[("copy", "x5")], {"cb-rs": (558, 558), "cb": (1069, 1069)})
        # cut rows interleave, they still pair up
        self.assertEqual(got[("cut", "small")], {"cb-rs": (1, 1), "cb": (475, 475)})
        self.assertEqual(got[("paste", "small")],
                         {"cb-rs": (0, 0), "cb": (700, 700)})
        # every row appears, nothing silently dropped
        self.assertEqual(len(entries), 5)

    def test_render(self):
        out = benchhtml.render(LOG)
        self.assertIn("<h2>local: tmpfs -&gt; tmpfs</h2>", out)
        self.assertIn("<h2>space</h2>", out)
        self.assertIn("<td>1.5&times;</td>", out)   # 479/318
        self.assertIn('class="win"', out)
        # both impls get a cell, and the shared file count is not doubled
        self.assertRegex(out, re.escape('<b>318 ms</b> <small>318</small>'
                                        '<small class="note">20000 files</small></td>'
                                        "<td><b>479 ms</b>"))
        # each impl's note rides in its own cell, never merged into one blob
        self.assertIn('<small class="note">20000 files</small>', out)
        self.assertNotIn("20000 files 20000 files", out)
        self.assertIn('<small class="note">clipboard now holds cb-rs 512M'
                      " cb 4.0K</small>", out)
        # non-row lines stay put: df keeps its columns in one pre, fixture is a
        # note, --- is a subheading
        self.assertIn("<pre>Filesystem      Size  Used Avail Use% Mounted on\n"
                      "tmpfs           8.0G  606M  7.5G   8% /home/runner/bench"
                      "</pre>", out)
        self.assertEqual(out.count("<pre>"), 1)
        self.assertIn('<p class="note">fixture: 20000 small files,'
                      " 512 MiB single file</p>", out)
        self.assertIn("<h3>cut+paste small -&gt; /home/runner/bench/cut_r</h3>", out)
        # one column header per h2, not one per cut+paste sub-block
        self.assertEqual(out.count("<thead>"), 2)

    def test_ratio(self):
        p = benchhtml.Pair("copy", "small", {"cb-rs": (318, 318), "cb": (479, 479)}, {})
        self.assertEqual(benchhtml.ratio(p), "1.5&times;")
        # 0 ms is a median under the clock's resolution, not a missing row: it
        # gets a bound, and it is the biggest win the bench can print.
        zero = benchhtml.Pair("paste", "small", {"cb-rs": (0, 0), "cb": (700, 700)}, {})
        self.assertEqual(benchhtml.ratio(zero), "&gt;700&times;")
        self.assertEqual(benchhtml.ratio(
            benchhtml.Pair("cut", "small", {"cb-rs": (0, 0), "cb": (0, 0)}, {})), "&mdash;")
        self.assertEqual(benchhtml.ratio(
            benchhtml.Pair("cut", "small", {"cb-rs": (1, 1)}, {})), "&mdash;")

    def test_bench_py_format(self):
        """bench.py writes the rows, so its format string is the contract. LOG is
        hand-copied and cannot see bench.py change under it."""
        for args in (("copy", "small", "cb-rs", 318, 318, 0, "20000 files"),
                     ("paste", "4k x4000", "cb", 733, 733, 1, ""),  # rc != 0
                     ("cut", "big", "cb-rs", 0, 0, 0, "")):
            with contextlib.redirect_stdout(io.StringIO()) as f:
                bench.row(*args)
            self.assertIsNotNone(benchhtml.row(f.getvalue()), f.getvalue())

        # The retention block does not go through row(): it prints its own line,
        # with the unit on the minimum too.
        with contextlib.redirect_stdout(io.StringIO()) as f:
            print("copy x5 %-6s %4d ms   min %4d ms   clipboard now holds"
                  " cb-rs %-6s cb %-6s" % ("cb", 1069, 1069, "0", "513M"))
        self.assertEqual(benchhtml.row(f.getvalue()),
                         ("copy", "x5", "cb", 1069, 1069,
                          "clipboard now holds cb-rs 0 cb 513M"))

    def test_escapes(self):
        out = benchhtml.render("== a\ncopy x cb-rs 1 ms min 1 &<>\"'\n")
        self.assertNotIn("<>", out)
        self.assertIn("&amp;&lt;&gt;", out)


if __name__ == "__main__":
    unittest.main()