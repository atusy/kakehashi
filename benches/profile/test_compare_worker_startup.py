import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from compare_worker_startup import collect_pairs, parse_benchmark_json, summarize_pairs


class WorkerStartupComparisonTest(unittest.TestCase):
    def test_collects_order_reversed_complete_pairs(self):
        calls = []

        def run(kind):
            calls.append(kind)
            return {"handshake_us": len(calls) * 100.0, "lsp_elapsed_ms": 2000.0}

        pairs = collect_pairs(2, run)

        self.assertEqual(calls, ["baseline", "candidate", "candidate", "baseline"])
        self.assertEqual(pairs[0]["order"], "baseline-candidate")
        self.assertEqual(pairs[1]["order"], "candidate-baseline")
        self.assertEqual(pairs[0]["candidate"]["handshake_us"], 200.0)
        self.assertEqual(pairs[1]["baseline"]["handshake_us"], 400.0)

    def test_extracts_benchmark_json_after_cargo_output(self):
        output = "Finished release profile\n{\n  \"cold_start_us\": 1234.5\n}\n"

        self.assertEqual(parse_benchmark_json(output)["cold_start_us"], 1234.5)

    def test_summarizes_medians_and_paired_delta(self):
        pairs = [
            {
                "baseline": {"handshake_us": 100.0, "lsp_elapsed_ms": 2000.0, "lsp_request_ms": 1000.0},
                "candidate": {"handshake_us": 110.0, "lsp_elapsed_ms": 1980.0, "lsp_request_ms": 990.0},
            },
            {
                "baseline": {"handshake_us": 120.0, "lsp_elapsed_ms": 2020.0, "lsp_request_ms": 1010.0},
                "candidate": {"handshake_us": 125.0, "lsp_elapsed_ms": 2010.0, "lsp_request_ms": 1005.0},
            },
        ]

        summary = summarize_pairs(pairs)

        self.assertEqual(summary["handshake_us"]["baseline_median"], 110.0)
        self.assertEqual(summary["handshake_us"]["candidate_median"], 117.5)
        self.assertAlmostEqual(
            summary["handshake_us"]["median_paired_delta_percent"], 7.0833333333
        )
        self.assertEqual(summary["lsp_elapsed_ms"]["baseline_median"], 2010.0)
        self.assertEqual(summary["lsp_elapsed_ms"]["candidate_median"], 1995.0)


if __name__ == "__main__":
    unittest.main()
