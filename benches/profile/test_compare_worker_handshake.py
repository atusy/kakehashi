import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

import compare_worker_handshake as comparison


class CompareWorkerHandshakeTests(unittest.TestCase):
    def test_run_order_reverses_each_pair(self):
        self.assertEqual(comparison.run_order(0), ("baseline", "candidate"))
        self.assertEqual(comparison.run_order(1), ("candidate", "baseline"))

    def test_parse_harness_json_ignores_leading_output(self):
        value = comparison.parse_harness_json(
            'diagnostic\n{"handshake_us": 12.5, "shutdown_us": 4.0}\n'
        )
        self.assertEqual(value["handshake_us"], 12.5)

    def test_summarize_pairs_uses_paired_median_delta(self):
        pairs = [
            {
                "baseline": {"handshake_us": 100.0, "shutdown_us": 50.0},
                "candidate": {"handshake_us": 110.0, "shutdown_us": 45.0},
            },
            {
                "baseline": {"handshake_us": 200.0, "shutdown_us": 100.0},
                "candidate": {"handshake_us": 180.0, "shutdown_us": 110.0},
            },
        ]
        summary = comparison.summarize_pairs(pairs)
        self.assertEqual(summary["handshake_us"]["baseline_median"], 150.0)
        self.assertEqual(summary["handshake_us"]["candidate_median"], 145.0)
        self.assertAlmostEqual(
            summary["handshake_us"]["median_paired_delta_percent"], 0.0
        )
        self.assertAlmostEqual(
            summary["shutdown_us"]["median_paired_delta_percent"], 0.0
        )


if __name__ == "__main__":
    unittest.main()
