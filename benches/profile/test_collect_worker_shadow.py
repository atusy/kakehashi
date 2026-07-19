#!/usr/bin/env python3

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from collect_worker_shadow import delta_percent, order, parse_comparisons


class ShadowCollectorTest(unittest.TestCase):
    def test_order_reverses_each_batch(self):
        self.assertEqual(order(0), ("disabled", "shadow"))
        self.assertEqual(order(1), ("shadow", "disabled"))

    def test_parse_comparisons_requires_a_complete_match(self):
        output = "shadow comparisons matched=201 mismatched=0 superseded=0 pending=0"
        self.assertEqual(parse_comparisons(output)["matched"], 201)
        with self.assertRaises(ValueError):
            parse_comparisons(output.replace("pending=0", "pending=1"))

    def test_delta_percent_uses_control_as_baseline(self):
        self.assertEqual(delta_percent({"wall": 100}, {"wall": 112.5}, "wall"), 12.5)


if __name__ == "__main__":
    unittest.main()
