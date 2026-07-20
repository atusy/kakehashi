#!/usr/bin/env python3

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from collect_worker_memory import (
    descendant_pids,
    order,
    parse_footprint_bytes,
    parse_ps_table,
    scenario_arguments,
    summarize_samples,
)


class WorkerMemoryCollectorTest(unittest.TestCase):
    def test_order_reverses_each_batch(self):
        self.assertEqual(order(0), ("legacy", "authoritative"))
        self.assertEqual(order(1), ("authoritative", "legacy"))

    def test_scenario_arguments_preserve_requested_scale(self):
        self.assertEqual(
            scenario_arguments("rust", 5000, 10),
            [
                "--lang", "rust", "--size", "5000", "--requests", "10",
                "--warm-semantic-cache", "--settle", "0.5",
                "--hold-open", "1.0",
            ],
        )

    def test_parse_footprint_prefers_shared_aware_summary(self):
        output = (
            "parent [1]: Footprint: 100 B\n"
            "worker [2]: Footprint: 200 B\n"
            "Summary Footprint: 250 B\n"
        )
        self.assertEqual(parse_footprint_bytes(output), 250)
        self.assertEqual(parse_footprint_bytes("proc: Footprint: 123 B\n"), 123)

    def test_parse_ps_table_preserves_commands_with_spaces(self):
        table = parse_ps_table(
            "  10   1  2048 python3 drive.py --size 10\n"
            "  11  10  4096 /tmp/kakehashi --tree-worker\n"
        )
        self.assertEqual(table[10], {"ppid": 1, "rss_kib": 2048,
                                     "command": "python3 drive.py --size 10"})
        self.assertEqual(table[11]["command"], "/tmp/kakehashi --tree-worker")

    def test_descendant_pids_walks_the_full_process_tree(self):
        table = {
            10: {"ppid": 1, "rss_kib": 1, "command": "driver"},
            11: {"ppid": 10, "rss_kib": 2, "command": "parent"},
            12: {"ppid": 11, "rss_kib": 3, "command": "worker"},
            13: {"ppid": 1, "rss_kib": 4, "command": "unrelated"},
        }
        self.assertEqual(descendant_pids(table, 10), {11, 12})

    def test_summarize_samples_reports_peak_and_stable_median(self):
        samples = [
            {"total_rss_kib": 10, "process_count": 1},
            {"total_rss_kib": 30, "process_count": 2},
            {"total_rss_kib": 20, "process_count": 2},
            {"total_rss_kib": 40, "process_count": 2},
        ]
        self.assertEqual(
            summarize_samples(samples),
            {
                "sample_count": 4,
                "peak_total_rss_kib": 40,
                "stable_median_total_rss_kib": 30,
                "peak_process_count": 2,
            },
        )

    def test_summarize_samples_rejects_empty_input(self):
        with self.assertRaises(ValueError):
            summarize_samples([])


if __name__ == "__main__":
    unittest.main()
