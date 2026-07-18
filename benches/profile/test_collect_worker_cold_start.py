import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from collect_worker_cold_start import collect_pairs


class ColdStartCollectorTest(unittest.TestCase):
    def test_collects_complete_alternating_pairs(self):
        calls = []

        def run(kind):
            calls.append(kind)
            return {"elapsed_seconds": len(calls) / 100}

        pairs = collect_pairs(2, run)

        self.assertEqual(calls, ["direct", "relay", "relay", "direct"])
        self.assertEqual(pairs[0]["order"], "direct-relay")
        self.assertEqual(pairs[1]["order"], "relay-direct")
        self.assertEqual(pairs[0]["direct"]["elapsed_seconds"], 0.01)
        self.assertEqual(pairs[1]["direct"]["elapsed_seconds"], 0.04)


if __name__ == "__main__":
    unittest.main()
