import dataclasses
import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from drive import (
    RequestSample,
    TransportFrame,
    classify_semantic_blocking,
    count_semantic_outcomes,
    next_toggle_change,
    parse_stdout_metric_line,
    response_result_id,
    server_request_result,
    summarize_samples,
    summarize_samples_by_status,
    summarize_transport_frames,
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
            server_request_result(
                {
                    "method": "workspace/configuration",
                    "params": {"items": [{}, {}]},
                }
            ),
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

    def test_parses_and_summarizes_stdout_transport_metrics(self):
        line = json.dumps(
            {
                "event": "stdout_frame",
                "frame": {
                    "ready_sequence": 3,
                    "method": "textDocument/semanticTokens/full",
                    "id": 9,
                    "body_bytes": 100,
                    "frame_bytes": 123,
                    "ready_us": 1000,
                    "write_start_us": 1500,
                    "last_byte_us": 2500,
                    "flush_complete_us": 3000,
                },
            }
        )

        frame = parse_stdout_metric_line(line)

        self.assertEqual(
            frame,
            TransportFrame(
                method="textDocument/semanticTokens/full",
                request_id=9,
                frame_bytes=123,
                ready_us=1000,
                write_start_us=1500,
                last_byte_us=2500,
                flush_complete_us=3000,
            ),
        )
        summary = summarize_transport_frames([frame])
        self.assertEqual(summary.count, 1)
        self.assertEqual(summary.p90_ready_to_write_complete_ms, 1.5)
        self.assertEqual(summary.p90_ready_to_flush_complete_ms, 2.0)
        self.assertEqual(summary.max_frame_bytes, 123)

    def test_rejects_non_metric_stderr_lines_and_unattributed_notifications(self):
        self.assertIsNone(parse_stdout_metric_line("ordinary server log"))
        self.assertIsNone(parse_stdout_metric_line(json.dumps({"event": "other"})))
        frame = parse_stdout_metric_line(
            json.dumps(
                {
                    "event": "stdout_frame",
                    "frame": {
                        "ready_sequence": None,
                        "method": "window/logMessage",
                        "id": None,
                        "body_bytes": 10,
                        "frame_bytes": 32,
                        "ready_us": None,
                        "write_start_us": 10,
                        "last_byte_us": 10,
                        "flush_complete_us": None,
                    },
                }
            )
        )
        self.assertEqual(frame.method, "window/logMessage")
        with self.assertRaises(ValueError):
            summarize_transport_frames([frame])

    def test_classifies_scheduler_opportunity_before_large_write_starts(self):
        semantic = TransportFrame(
            method="textDocument/semanticTokens/full",
            request_id=2,
            frame_bytes=100,
            ready_us=100,
            write_start_us=500,
            last_byte_us=510,
            flush_complete_us=520,
        )
        large = TransportFrame(
            method="kakehashi/captures/full/delta",
            request_id=1,
            frame_bytes=1_000_000,
            ready_us=90,
            write_start_us=200,
            last_byte_us=450,
            flush_complete_us=460,
        )

        self.assertEqual(
            classify_semantic_blocking([large, semantic], 64 * 1024),
            (1, 0),
        )
        semantic_after_start = dataclasses.replace(semantic, ready_us=300)
        self.assertEqual(
            classify_semantic_blocking([large, semantic_after_start], 64 * 1024),
            (0, 1),
        )


if __name__ == "__main__":
    unittest.main()
