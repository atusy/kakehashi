import pathlib
import os
import signal
import sys
import time
import unittest
import subprocess

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from drive import (
    benchmark_clock,
    capture_result_id,
    RequestSample,
    count_semantic_outcomes,
    next_toggle_change,
    response_result_id,
    server_request_result,
    summarize_samples,
    summarize_samples_by_status,
    terminate_server,
    warm_semantic_tokens,
)


class RequestSummaryTest(unittest.TestCase):
    def test_capture_result_validates_full_and_delta_lineage(self):
        self.assertEqual(capture_result_id({
            "result": {"resultId": "full", "matches": [], "skipped": []}
        }, expected_shape="full"), "full")
        self.assertEqual(capture_result_id({
            "result": {"resultId": "delta", "edits": []}
        }, expected_shape="delta"), "delta")
        with self.assertRaisesRegex(RuntimeError, "expected delta"):
            capture_result_id(
                {"result": {"resultId": "fallback", "matches": [], "skipped": []}},
                expected_shape="delta",
            )
        with self.assertRaisesRegex(RuntimeError, "expected delta"):
            capture_result_id(
                {"result": {
                    "resultId": "hybrid", "matches": [], "skipped": [],
                    "edits": [],
                }},
                expected_shape="delta",
            )
        with self.assertRaisesRegex(RuntimeError, "advance capture lineage"):
            capture_result_id(
                {"result": {"resultId": "same", "edits": []}},
                previous_result_id="same",
            )
        for response in (
            {"error": {"code": -32800}},
            {"result": None},
            {"result": {"resultId": "", "edits": []}},
            {"result": {"resultId": "bad"}},
        ):
            with self.subTest(response=response):
                with self.assertRaisesRegex(RuntimeError, "capture"):
                    capture_result_id(response)

    def test_server_termination_escalates_after_bounded_grace(self):
        class StubbornServer:
            def __init__(self):
                self.actions = []

            def poll(self):
                return None

            def terminate(self):
                self.actions.append("terminate")

            def wait(self, timeout=None):
                self.actions.append(("wait", timeout))
                if timeout is not None:
                    raise subprocess.TimeoutExpired("server", timeout)

            def kill(self):
                self.actions.append("kill")

        server = StubbornServer()

        terminate_server(server, timeout_seconds=0.25)

        self.assertEqual(server.actions, [
            "terminate", ("wait", 0.25), "kill", ("wait", None),
        ])

    @unittest.skipUnless(
        os.name == "posix" and hasattr(signal, "SIGTERM"),
        "requires POSIX SIGTERM",
    )
    def test_server_termination_lets_relay_reap_term_ignoring_child(self):
        proxy = pathlib.Path(__file__).with_name("worker_proxy.py")
        environment = dict(
            os.environ,
            KAKEHASHI_WORKER_PROXY_BIN=sys.executable,
        )
        process = subprocess.Popen(
            [
                sys.executable,
                str(proxy),
                "-c",
                "import os,signal,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                "print(os.getpid(), flush=True); time.sleep(30)",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            text=True,
        )
        child_pid = int(process.stdout.readline())
        try:
            terminate_server(process)
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)
        finally:
            terminate_server(process)
            try:
                os.kill(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.stdin.close()
            process.stdout.close()
            process.stderr.close()

    def test_aggregate_timing_uses_monotonic_clock(self):
        self.assertIs(benchmark_clock, time.perf_counter)

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
        self.assertEqual(summary.p95_ms, 30.0)
        self.assertEqual(summary.p99_ms, 30.0)
        self.assertEqual(summary.max_ms, 30.0)
        self.assertEqual(summary.wire_bytes, 600)

        by_status = summarize_samples_by_status(samples)
        self.assertEqual(by_status["ok"].count, 2)
        self.assertEqual(by_status["ok"].p50_ms, 10.0)
        self.assertEqual(by_status["ok"].p90_ms, 20.0)
        self.assertEqual(by_status["ok"].p95_ms, 20.0)
        self.assertEqual(by_status["ok"].p99_ms, 20.0)
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

    def test_rejects_malformed_successful_semantic_response(self):
        for response in (
            {},
            {"result": {}},
            {"result": {"data": []}},
            {"result": {"data": [0, 0]}},
            {"result": {"data": [0, 0, 1, "type", 0]}},
        ):
            with self.subTest(response=response):
                with self.assertRaisesRegex(RuntimeError, "invalid semantic-token"):
                    count_semantic_outcomes([response], previous_tokens=0)

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

    def test_semantic_warmup_is_an_unmeasured_full_request(self):
        calls = []

        warm_semantic_tokens(
            lambda method, params: (
                calls.append((method, params))
                or ({"result": {"data": [0, 0, 1, 0, 0]}}, 0)
            ),
            "file:///profile/input.rs",
        )

        self.assertEqual(calls, [(
            "textDocument/semanticTokens/full",
            {"textDocument": {"uri": "file:///profile/input.rs"}},
        )])

    def test_semantic_warmup_rejects_non_result_response(self):
        def canceled(_method, _params):
            return {"error": {"code": -32800, "message": "cancelled"}}, 0

        with self.assertRaisesRegex(RuntimeError, "warmup failed"):
            warm_semantic_tokens(canceled, "file:///profile/input.rs")


if __name__ == "__main__":
    unittest.main()
