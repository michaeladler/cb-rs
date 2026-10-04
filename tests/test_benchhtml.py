import contextlib, io, os, re, shutil, sys, tempfile, unittest
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "scripts"))
import bench
import benchhtml

LOG = """Filesystem      Size  Used Avail Use% Mounted on
tmpfs           8.0G  606M  7.5G   8% /home/runner/bench

== local: tmpfs -> tmpfs
fixture: 20000 small files, 512 MiB single file
copy+paste small cb-rs     634 ms   min    634   recorded, 20000 files
copy+paste small cb        958 ms   min    958   20000 files
--- cut+paste small -> /home/runner/bench/cut_r
cut+paste small cb-rs        1 ms   min      1   20000 files
cut+paste small cb         1175 ms   min   1175   20000 files

== retention: 5 copies in a row
copy x5 cb-rs   558 ms   min  558 ms   clipboard now holds cb-rs 512M   cb 4.0K
copy x5 cb     1069 ms   min 1069 ms   clipboard now holds cb-rs 0      cb 513M

== space
"""


class Test(unittest.TestCase):
    def test_row(self):
        self.assertEqual(
            benchhtml.row("copy+paste small cb-rs     634 ms   min    634   recorded"),
            ("copy+paste", "small", "cb-rs", 634, 634, "recorded"))
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
        self.assertEqual(got[("copy+paste", "small")],
                         {"cb-rs": (634, 634), "cb": (958, 958)})
        self.assertEqual(got[("copy", "x5")], {"cb-rs": (558, 558), "cb": (1069, 1069)})
        # cut+paste rows interleave with the copy+paste table above them, they
        # still pair up on their own op
        self.assertEqual(got[("cut+paste", "small")],
                         {"cb-rs": (1, 1), "cb": (1175, 1175)})
        # every row appears, nothing silently dropped
        self.assertEqual(len(entries), 3)

    def test_render(self):
        out = benchhtml.render(LOG)
        self.assertIn("<h2>local: tmpfs -&gt; tmpfs</h2>", out)
        self.assertIn("<h2>space</h2>", out)
        self.assertIn("<td>1.5&times;</td>", out)   # 958/634
        self.assertIn('class="win"', out)
        # both impls get a cell, and each impl's own note rides in its own cell
        self.assertIn('<td class="win"><b>634 ms</b> <small>634</small>'
                      '<small class="note">recorded, 20000 files</small></td>'
                      "<td><b>958 ms</b> <small>958</small>"
                      '<small class="note">20000 files</small></td>', out)
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
        for args in (("copy+paste", "small", "cb-rs", 318, 318, 0, "20000 files"),
                     ("copy+paste", "4k x4000", "cb", 733, 733, 1, ""),  # rc != 0
                     ("cut+paste", "big", "cb-rs", 0, 0, 0, "")):
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

    def test_cb_paste_drains_a_slow_child(self):
        """cb_paste must keep reading until EOF, not until the child goes quiet.

        A drain with a timeout abandons the pty on a gap longer than the
        timeout; the child then blocks writing into a full buffer and the wait
        never returns. The gap here is 6s, past the 5s that used to be there.
        """
        d = tempfile.mkdtemp()
        # cb_paste runs [bin, "paste"], so the child is an executable script
        # that ignores the argument rather than a -c string.
        child = os.path.join(d, "slow")
        with open(child, "w") as f:
            f.write("#!/usr/bin/env python3\n"
                    "import sys, time\n"
                    "sys.argv[1:]\n"
                    "time.sleep(6)\n"          # quiet past the 5s that was there
                    "for _ in range(4000):\n"
                    "    print('x' * 200)\n"  # ~800 KB: overflows the pty buffer
                    "time.sleep(1)\n")
        os.chmod(child, 0o755)
        dest = os.path.join(d, "dest")
        os.makedirs(dest)
        try:
            self.assertEqual(bench.cb_paste(child, dest), 0)
        finally:
            shutil.rmtree(d, ignore_errors=True)

    def test_escapes(self):
        out = benchhtml.render("== a\ncopy x cb-rs 1 ms min 1 &<>\"'\n")
        self.assertNotIn("<>", out)
        self.assertIn("&amp;&lt;&gt;", out)


if __name__ == "__main__":
    unittest.main()