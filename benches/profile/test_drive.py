import pathlib
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
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
    validate_profile_marker_paths,
    wait_for_marker,
)


class RequestSummaryTest(unittest.TestCase):
    def test_profile_ready_follows_warmup_and_start_gates_measurement(self):
        fake_server_source = textwrap.dedent(
            """\
            import json
            import pathlib
            import sys
            import time

            log_path = pathlib.Path(sys.argv[1])
            warmup_release = pathlib.Path(sys.argv[2])
            semantic_requests = 0

            def read_message():
                length = None
                while True:
                    line = sys.stdin.buffer.readline()
                    if not line:
                        raise EOFError
                    line = line.strip()
                    if not line:
                        break
                    if line.lower().startswith(b"content-length:"):
                        length = int(line.split(b":", 1)[1])
                return json.loads(sys.stdin.buffer.read(length))

            def send(message):
                body = json.dumps(message).encode()
                sys.stdout.buffer.write(
                    f"Content-Length: {len(body)}\\r\\n\\r\\n".encode() + body
                )
                sys.stdout.buffer.flush()

            while True:
                try:
                    message = read_message()
                except EOFError:
                    break
                method = message.get("method")
                with log_path.open("a") as log:
                    log.write(f"{method}\\n")
                if method == "exit":
                    break
                if "id" not in message:
                    continue
                if method == "initialize":
                    result = {"capabilities": {}}
                elif method == "textDocument/semanticTokens/full":
                    semantic_requests += 1
                    if semantic_requests == 1:
                        while not warmup_release.exists():
                            time.sleep(0.01)
                    result = {"data": []}
                else:
                    result = None
                send({"jsonrpc": "2.0", "id": message["id"], "result": result})
            """
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            fake_server = directory / "fake_server.py"
            fake_server.write_text(fake_server_source)
            log = directory / "methods.log"
            warmup_release = directory / "warmup-release"
            ready = directory / "ready"
            start = directory / "start"
            done = directory / "done"
            process = subprocess.Popen(
                [
                    sys.executable,
                    str(pathlib.Path(__file__).parent / "drive.py"),
                    "--bin",
                    sys.executable,
                    f"--server-arg={fake_server}",
                    f"--server-arg={log}",
                    f"--server-arg={warmup_release}",
                    "--requests=1",
                    "--size=1",
                    "--settle=0",
                    f"--profile-ready-file={ready}",
                    f"--profile-start-file={start}",
                    f"--profile-done-file={done}",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 3
                while (
                    not log.exists()
                    or log.read_text()
                    .splitlines()
                    .count("textDocument/semanticTokens/full")
                    < 1
                ):
                    if time.monotonic() >= deadline:
                        self.fail("fake server did not receive semantic warmup")
                    time.sleep(0.01)
                self.assertFalse(ready.exists())
                time.sleep(0.05)
                self.assertFalse(ready.exists())

                publish_marker(warmup_release, "")
                wait_for_marker(ready, 3)
                methods_at_ready = log.read_text().splitlines()
                self.assertEqual(
                    methods_at_ready.count("textDocument/semanticTokens/full"),
                    1,
                )
                time.sleep(0.05)
                self.assertEqual(log.read_text().splitlines(), methods_at_ready)

                publish_marker(start, "")
                wait_for_marker(done, 3)
                _, stderr = process.communicate(timeout=3)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual(
                    log.read_text()
                    .splitlines()
                    .count("textDocument/semanticTokens/full"),
                    2,
                )
            finally:
                publish_marker(warmup_release, "")
                publish_marker(start, "")
                if process.poll() is None:
                    try:
                        process.communicate(timeout=1)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()

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

    def test_profile_marker_paths_must_be_distinct(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            marker = directory / "ready"
            with self.assertRaisesRegex(ValueError, "must be distinct"):
                validate_profile_marker_paths(
                    [marker, directory / "nested" / ".." / "ready"]
                )
            with self.assertRaisesRegex(ValueError, "must be distinct"):
                validate_profile_marker_paths([marker, directory / "READY"])

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
