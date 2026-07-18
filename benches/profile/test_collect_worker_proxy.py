import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from collect_worker_proxy import parse_driver_summary, run_order


class CollectionHelpersTest(unittest.TestCase):
    def test_alternates_direct_and_relay_order(self):
        self.assertEqual(run_order(0), ("direct", "relay"))
        self.assertEqual(run_order(1), ("relay", "direct"))

    def test_parses_driver_summary(self):
        output = """
[drive] lang=rust cycles=100 wall=1574ms
[drive] method=textDocument/semanticTokens/full count=100 ok=100 canceled=0 null=0 errors=0 p50=3.6ms p90=4.0ms p95=4.3ms p99=5.2ms max=7.8ms wire=1430.1KiB
"""

        summary = parse_driver_summary(output)

        self.assertEqual(summary["wall"], 1574)
        self.assertEqual(summary["p50"], 3.6)
        self.assertEqual(summary["p95"], 4.3)
        self.assertEqual(summary["p99"], 5.2)
        self.assertEqual(summary["wire_kib"], 1430.1)


if __name__ == "__main__":
    unittest.main()
