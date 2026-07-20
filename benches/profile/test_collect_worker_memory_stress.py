import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from collect_worker_memory_stress import parse_report


class ParseReportTests(unittest.TestCase):
    def test_rejects_a_report_that_did_not_preserve_identity(self):
        report = {
            "recomputed_tokens_match": True,
            "node_identity_survived": False,
            "full_sync_rejection": "non-evictable budget exceeded",
            "incremental_edit_rejection": "non-evictable budget exceeded",
            "a_after_pressure": {
                "result_cache_bytes": 0,
                "auxiliary_cache_bytes": 0,
            },
            "b_after_pressure": {
                "result_cache_bytes": 20,
                "auxiliary_cache_bytes": 0,
            },
        }

        with self.assertRaisesRegex(ValueError, "node identity"):
            parse_report(json.dumps(report))


if __name__ == "__main__":
    unittest.main()
