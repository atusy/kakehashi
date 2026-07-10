import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from drive import (
    RequestSample,
    count_semantic_outcomes,
    next_toggle_change,
    server_request_result,
    summarize_samples,
)


class RequestSummaryTest(unittest.TestCase):
    def test_summarizes_latency_status_and_wire_bytes(self):
        samples = [
            RequestSample(seconds=0.010, wire_bytes=100, status="ok"),
            RequestSample(seconds=0.030, wire_bytes=300, status="canceled"),
            RequestSample(seconds=0.020, wire_bytes=200, status="ok"),
        ]

        summary = summarize_samples(samples)

        self.assertEqual(summary.count, 3)
        self.assertEqual(summary.ok, 2)
        self.assertEqual(summary.canceled, 1)
        self.assertEqual(summary.p50_ms, 20.0)
        self.assertEqual(summary.p90_ms, 30.0)
        self.assertEqual(summary.max_ms, 30.0)
        self.assertEqual(summary.wire_bytes, 600)

    def test_counts_mixed_burst_outcomes_and_last_completed_payload(self):
        responses = [
            {"error": {"code": -32800, "message": "cancelled"}},
            {"result": {"data": [0, 0, 3, 1, 0, 0, 4, 2, 1, 0]}},
            {"result": None},
            {"error": {"code": -32800, "message": "cancelled"}},
        ]

        ok, canceled, superseded, tokens = count_semantic_outcomes(
            responses, previous_tokens=7
        )

        self.assertEqual((ok, canceled, superseded, tokens), (1, 2, 1, 2))

    def test_toggle_change_alternates_insert_and_delete(self):
        insert, has_extra = next_toggle_change(first_line_len=5, line_has_extra=False)
        delete, has_extra = next_toggle_change(5, has_extra)

        self.assertEqual(insert["range"]["start"]["character"], 5)
        self.assertEqual(insert["range"]["end"]["character"], 5)
        self.assertEqual(insert["text"], "x")
        self.assertEqual(delete["range"]["start"]["character"], 5)
        self.assertEqual(delete["range"]["end"]["character"], 6)
        self.assertEqual(delete["text"], "")
        self.assertFalse(has_extra)

    def test_server_request_results_match_expected_shapes(self):
        self.assertEqual(
            server_request_result({
                "method": "workspace/configuration",
                "params": {"items": [{}, {}]},
            }),
            [None, None],
        )
        self.assertEqual(
            server_request_result({"method": "workspace/workspaceFolders"}),
            [],
        )
        self.assertIsNone(
            server_request_result({"method": "client/registerCapability"})
        )


if __name__ == "__main__":
    unittest.main()
