import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from analyze_worker_proxy import summarize_pairs


class PairedSummaryTest(unittest.TestCase):
    def test_reports_deterministic_paired_mean_and_interval(self):
        pairs = [
            {"direct": {"p50": 1.0}, "relay": {"p50": 1.1}},
            {"direct": {"p50": 2.0}, "relay": {"p50": 2.3}},
            {"direct": {"p50": 3.0}, "relay": {"p50": 3.2}},
        ]

        first = summarize_pairs(pairs, "p50", seed=7, resamples=1_000)
        second = summarize_pairs(pairs, "p50", seed=7, resamples=1_000)

        self.assertEqual(first, second)
        self.assertAlmostEqual(first["direct_mean"], 2.0)
        self.assertAlmostEqual(first["relay_mean"], 2.2)
        self.assertAlmostEqual(first["mean_delta"], 0.2)
        self.assertLessEqual(first["ci95"][0], first["mean_delta"])
        self.assertGreaterEqual(first["ci95"][1], first["mean_delta"])


if __name__ == "__main__":
    unittest.main()
