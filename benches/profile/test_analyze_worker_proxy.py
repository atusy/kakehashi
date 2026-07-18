import pathlib
import random
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from analyze_worker_proxy import analyze_data, summarize_cold_start, summarize_pairs


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

    def test_summarizes_cold_start_samples_in_milliseconds(self):
        summary = summarize_cold_start([0.100, 0.110, 0.120])

        self.assertAlmostEqual(summary["mean_ms"], 110.0)
        self.assertAlmostEqual(summary["stddev_ms"], 10.0)
        self.assertAlmostEqual(summary["min_ms"], 100.0)
        self.assertAlmostEqual(summary["max_ms"], 120.0)

    def test_single_cold_start_sample_has_zero_dispersion(self):
        summary = summarize_cold_start([0.100])

        self.assertEqual(summary["stddev_ms"], 0.0)

    def test_bootstrap_uses_bulk_sampling(self):
        pairs = [
            {"direct": {"p50": 1.0}, "relay": {"p50": 1.1}},
            {"direct": {"p50": 2.0}, "relay": {"p50": 2.2}},
        ]

        with mock.patch.object(
            random.Random, "choice", side_effect=AssertionError("scalar sampling")
        ):
            summarize_pairs(pairs, "p50", seed=7, resamples=10)

    def test_analyzes_dedicated_cold_start_pairs(self):
        data = {
            "experiment": "single-tree-worker-phase0-cold-start",
            "pairs": [
                {
                    "direct": {"elapsed_seconds": 0.1},
                    "relay": {"elapsed_seconds": 0.13},
                },
                {
                    "direct": {"elapsed_seconds": 0.11},
                    "relay": {"elapsed_seconds": 0.15},
                },
            ],
        }

        summary = analyze_data(data, seed=7, resamples=100)

        self.assertAlmostEqual(summary["cold_start"]["mean_delta"], 0.035)

    def test_can_slice_steady_state_pairs_for_sensitivity_analysis(self):
        def result(value):
            return {metric: value for metric in ("p50", "p95", "p99", "wall")}

        data = {
            "experiment": "single-tree-worker-phase0-raw-relay",
            "steady_state": {
                "rust_edit": {
                    "pairs": [
                        {"direct": result(1.0), "relay": result(4.0)},
                        {"direct": result(2.0), "relay": result(2.1)},
                        {"direct": result(3.0), "relay": result(3.1)},
                    ]
                }
            },
        }

        without_first = analyze_data(
            data, seed=7, resamples=100, drop_first_pairs=1
        )
        last_two = analyze_data(data, seed=7, resamples=100, last_pairs=2)

        self.assertAlmostEqual(without_first["rust_edit"]["p99"]["mean_delta"], 0.1)
        self.assertEqual(without_first, last_two)


if __name__ == "__main__":
    unittest.main()
