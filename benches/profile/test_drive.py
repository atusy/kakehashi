import pathlib
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from drive import (
    RequestSample,
    count_semantic_outcomes,
    next_toggle_change,
    publish_marker,
    response_result_id,
    server_request_result,
    summarize_samples,
    summarize_samples_by_status,
    wait_for_marker,
)


class RequestSummaryTest(unittest.TestCase):
    def test_profile_markers_are_published_and_waited_for(self):
        with tempfile.TemporaryDirectory() as temporary:
            marker = pathlib.Path(temporary) / "nested" / "start"
            observed = []
            waiter = threading.Thread(
                target=lambda: observed.append(wait_for_marker(marker, 1.0))
            )
            waiter.start()
            publish_marker(marker, "ready\n")
            waiter.join(timeout=2)

            self.assertEqual(observed, [marker])
            self.assertEqual(marker.read_text(), "ready\n")

    def test_profile_marker_wait_has_a_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            marker = pathlib.Path(temporary) / "missing"
            with self.assertRaisesRegex(TimeoutError, "missing"):
                wait_for_marker(marker, 0.01)

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

        by_status = summarize_samples_by_status(samples)
        self.assertEqual(by_status["ok"].count, 2)
        self.assertEqual(by_status["ok"].p50_ms, 10.0)
        self.assertEqual(by_status["ok"].p90_ms, 20.0)
        self.assertEqual(by_status["canceled"].count, 1)
        self.assertEqual(by_status["canceled"].p50_ms, 30.0)

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

    def test_response_result_id_accepts_full_or_delta_result(self):
        self.assertEqual(
            response_result_id({"result": {"resultId": "42", "edits": []}}),
            "42",
        )
        self.assertIsNone(response_result_id({"result": None}))
        self.assertIsNone(response_result_id({"error": {"code": -32800}}))


if __name__ == "__main__":
    unittest.main()
