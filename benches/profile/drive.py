#!/usr/bin/env python3
"""Drive the kakehashi server through a heavy semanticTokens load.

By default this waits for each response before sending the next request, so every
request completes real work. `--burst N --burst-edits` instead reproduces rapid
typing and measures how promptly superseded work releases the compute pool. Run
the server under a sampler (samply/flamegraph) with the synchronous default so
the sampled process actually spends its time in the semantic-tokens hot path.

Usage:
    drive.py --bin ./target/profiling/kakehashi --lang rust --size 150 --requests 300
"""

import argparse
from collections import Counter, defaultdict, deque
from dataclasses import dataclass
import json
import math
import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen_session import gen_rust, gen_markdown_injections  # noqa: E402


@dataclass(frozen=True)
class RequestSample:
    seconds: float
    wire_bytes: int
    status: str
    completed_at: float = 0.0


@dataclass(frozen=True)
class RequestSummary:
    count: int
    ok: int
    canceled: int
    null: int
    errors: int
    p50_ms: float
    p90_ms: float
    max_ms: float
    wire_bytes: int


@dataclass(frozen=True)
class TransportFrame:
    method: str | None
    request_id: int | str | None
    frame_bytes: int
    ready_us: int | None
    write_start_us: int
    last_byte_us: int
    flush_complete_us: int | None
    id_unattributed: bool = False


@dataclass(frozen=True)
class TransportSummary:
    count: int
    censored_flushes: int
    p50_ready_to_write_complete_ms: float
    p90_ready_to_write_complete_ms: float
    p50_ready_to_flush_complete_ms: float | None
    p90_ready_to_flush_complete_ms: float | None
    max_frame_bytes: int


def parse_stdout_metric_line(line: str) -> TransportFrame | None:
    try:
        metric = json.loads(line)
    except (json.JSONDecodeError, TypeError):
        return None
    if not isinstance(metric, dict) or metric.get("event") != "stdout_frame":
        return None
    frame = metric.get("frame")
    if not isinstance(frame, dict):
        return None
    return TransportFrame(
        method=frame.get("method"),
        request_id=frame.get("id"),
        frame_bytes=frame["frame_bytes"],
        ready_us=frame.get("ready_us"),
        write_start_us=frame["write_start_us"],
        last_byte_us=frame["last_byte_us"],
        flush_complete_us=frame.get("flush_complete_us"),
        id_unattributed=frame.get("id_unattributed", False),
    )


def percentile(values: list[float], q: float) -> float:
    if not values:
        raise ValueError("cannot calculate percentile of empty values")
    ordered = sorted(values)
    return ordered[max(0, math.ceil(q * len(ordered)) - 1)]


def summarize_transport_frames(frames: list[TransportFrame]) -> TransportSummary:
    if not frames:
        raise ValueError("cannot summarize empty transport frames")
    if any(frame.ready_us is None for frame in frames):
        raise ValueError("transport response frames must have ready timestamps")
    write_complete = [(frame.last_byte_us - frame.ready_us) / 1000 for frame in frames]
    flush_complete = [
        (frame.flush_complete_us - frame.ready_us) / 1000
        for frame in frames
        if frame.flush_complete_us is not None
    ]
    return TransportSummary(
        count=len(frames),
        censored_flushes=len(frames) - len(flush_complete),
        p50_ready_to_write_complete_ms=percentile(write_complete, 0.5),
        p90_ready_to_write_complete_ms=percentile(write_complete, 0.9),
        p50_ready_to_flush_complete_ms=(
            percentile(flush_complete, 0.5) if flush_complete else None
        ),
        p90_ready_to_flush_complete_ms=(
            percentile(flush_complete, 0.9) if flush_complete else None
        ),
        max_frame_bytes=max(frame.frame_bytes for frame in frames),
    )


def classify_semantic_blocking(
    frames: list[TransportFrame], large_frame_bytes: int
) -> tuple[int, int]:
    """Return (schedulable-before-start, already-writing) semantic blockers."""
    schedulable = 0
    already_writing = 0
    semantics = [
        frame
        for frame in frames
        if frame.ready_us is not None
        and (frame.method or "").startswith("textDocument/semanticTokens/")
    ]
    for semantic in semantics:
        blockers = [
            frame
            for frame in frames
            if frame is not semantic
            and frame.frame_bytes >= large_frame_bytes
            and frame.write_start_us < semantic.write_start_us
            and (frame.flush_complete_us or frame.last_byte_us) >= semantic.ready_us
        ]
        if not blockers:
            continue
        blocker = max(blockers, key=lambda frame: frame.last_byte_us)
        if semantic.ready_us <= blocker.write_start_us:
            schedulable += 1
        else:
            already_writing += 1
    return schedulable, already_writing


def validate_transport_metrics(
    frames: list[TransportFrame],
    expected_counts: dict[str, int],
    partial_frame_count: int,
    collector_errors: list[BaseException],
) -> None:
    if collector_errors:
        raise RuntimeError(f"stdout metric collector failed: {collector_errors[0]}")
    if partial_frame_count:
        raise RuntimeError(
            f"stdout metrics contain {partial_frame_count} partial frames"
        )
    unattributed = [frame for frame in frames if frame.id_unattributed]
    if unattributed:
        raise RuntimeError(
            f"stdout metrics contain {len(unattributed)} unattributed frames"
        )
    censored = [frame for frame in frames if frame.flush_complete_us is None]
    if censored:
        raise RuntimeError(f"stdout metrics contain {len(censored)} censored flushes")
    for method, expected in expected_counts.items():
        actual = sum(
            frame.ready_us is not None and frame.method == method for frame in frames
        )
        if actual != expected:
            raise RuntimeError(
                f"stdout metric count mismatch for {method}: expected {expected}, got {actual}"
            )


def summarize_samples(samples: list[RequestSample]) -> RequestSummary:
    if not samples:
        raise ValueError("cannot summarize an empty request sample")
    latencies = sorted(sample.seconds * 1000 for sample in samples)

    statuses = Counter(sample.status for sample in samples)
    return RequestSummary(
        count=len(samples),
        ok=statuses["ok"],
        canceled=statuses["canceled"],
        null=statuses["null"],
        errors=statuses["error"],
        p50_ms=percentile(latencies, 0.5),
        p90_ms=percentile(latencies, 0.9),
        max_ms=latencies[-1],
        wire_bytes=sum(sample.wire_bytes for sample in samples),
    )


def summarize_samples_by_status(
    samples: list[RequestSample],
) -> dict[str, RequestSummary]:
    return {
        status: summarize_samples(
            [sample for sample in samples if sample.status == status]
        )
        for status in ("ok", "canceled", "null", "error")
        if any(sample.status == status for sample in samples)
    }


def response_status(message: dict) -> str:
    error = message.get("error")
    if not error:
        return "null" if "result" in message and message["result"] is None else "ok"
    return "canceled" if error.get("code") == -32800 else "error"


def server_request_result(message: dict):
    """Return a minimal protocol-valid result for common server requests."""
    method = message.get("method")
    if method == "workspace/configuration":
        items = (message.get("params") or {}).get("items") or []
        return [None] * len(items)
    if method == "workspace/workspaceFolders":
        return []
    return None


def response_result_id(message: dict):
    result = message.get("result")
    return result.get("resultId") if isinstance(result, dict) else None


def count_semantic_outcomes(
    responses: list[dict], previous_tokens: int
) -> tuple[int, int, int, int]:
    ok = 0
    canceled = 0
    superseded = 0
    tokens = previous_tokens
    for response in responses:
        status = response_status(response)
        if status == "canceled":
            canceled += 1
        elif status == "null":
            superseded += 1
        elif status == "error":
            raise RuntimeError(
                f"server error (not a cancellation): {response['error']}"
            )
        else:
            ok += 1
            tokens = len((response.get("result") or {}).get("data", [])) // 5
    return ok, canceled, superseded, tokens


def next_toggle_change(first_line_len: int, line_has_extra: bool) -> tuple[dict, bool]:
    grow = not line_has_extra
    if grow:
        change = {
            "range": {
                "start": {"line": 0, "character": first_line_len},
                "end": {"line": 0, "character": first_line_len},
            },
            "text": "x",
        }
    else:
        change = {
            "range": {
                "start": {"line": 0, "character": first_line_len},
                "end": {"line": 0, "character": first_line_len + 1},
            },
            "text": "",
        }
    return change, grow


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument(
        "--server-arg",
        action="append",
        default=[],
        help="extra argument passed to the server (repeatable), e.g. "
        "--server-arg=--config-file --server-arg=/path/lsp.toml "
        "to reproduce a real user configuration",
    )
    ap.add_argument("--lang", choices=["rust", "markdown"], default="rust")
    ap.add_argument("--size", type=int, default=150)
    ap.add_argument("--requests", type=int, default=300)
    ap.add_argument(
        "--burst",
        type=int,
        default=1,
        help="send this many semantic-token requests back-to-back per cycle; "
        "reproduces supersession/cancellation pressure",
    )
    ap.add_argument(
        "--burst-edits",
        action="store_true",
        help="toggle a real edit between requests in each burst so every request "
        "targets a newer snapshot",
    )
    ap.add_argument(
        "--burst-delay-ms",
        type=float,
        default=1.0,
        help="typing interval between a burst request and the next edit",
    )
    ap.add_argument(
        "--file",
        help="drive with this file's content instead of a "
        "generated document (language inferred from the "
        "extension, falling back to --lang)",
    )
    ap.add_argument(
        "--captures",
        action="store_true",
        help="warm kakehashi/captures/full once, then send a real injection-mode "
        "captures delta per cycle",
    )
    ap.add_argument(
        "--concurrent-captures",
        action="store_true",
        help="send semantic tokens and the captures delta as one concurrent "
        "batch; implies --captures and exposes shared-pool/output contention",
    )
    ap.add_argument(
        "--scenario",
        choices=(
            "semantic-only",
            "captures-delta",
            "captures-full",
            "diagnostics-burst",
        ),
        help="run one of issue #665's stdout head-of-line scenarios",
    )
    ap.add_argument(
        "--diagnostics-burst-size",
        type=int,
        default=4,
        help="diagnostic requests queued before semantic tokens in the diagnostics-burst scenario",
    )
    ap.add_argument(
        "--settle",
        type=float,
        default=0.3,
        help="seconds to wait after didOpen before the first request "
        "(0 reproduces a client that requests immediately)",
    )
    ap.add_argument(
        "--edits",
        type=int,
        default=0,
        help="simulate typing: before each request, send this many incremental "
        "didChange edits (appending/removing a char at the end of the first "
        "line), then request tokens. Exercises the edit->reparse->recompute "
        "cycle instead of the unchanged-document cache hit; the request "
        "parks until the matching parse snapshot is current.",
    )
    ap.add_argument(
        "--data-dir",
        default=os.path.join(os.getcwd(), "deps/test/kakehashi"),
        help="parser/query data dir; must already contain installed parsers "
        "(populated by `cargo test --features e2e` or `make deps/tree-sitter`), "
        "else the server auto-installs on first request",
    )
    ap.add_argument(
        "--stdout-metrics",
        action="store_true",
        help="enable and summarize response-ready to stdout-write transport metrics",
    )
    ap.add_argument(
        "--large-frame-kib",
        type=float,
        default=64,
        help="minimum competing frame size for scheduler-opportunity classification",
    )
    args = ap.parse_args()
    if args.requests <= 0:
        ap.error("--requests must be positive")  # avoids divide-by-zero in the summary
    if args.burst <= 0:
        ap.error("--burst must be positive")
    if args.burst_edits and args.burst <= 1:
        ap.error("--burst-edits requires --burst greater than 1")
    if args.burst_delay_ms < 0:
        ap.error("--burst-delay-ms must be non-negative")
    if args.diagnostics_burst_size <= 0:
        ap.error("--diagnostics-burst-size must be positive")
    if args.large_frame_kib <= 0:
        ap.error("--large-frame-kib must be positive")
    if args.scenario and (args.captures or args.concurrent_captures):
        ap.error("--scenario cannot be combined with legacy captures flags")
    captures_full_fallback = args.scenario == "captures-full"
    diagnostics_burst = args.scenario == "diagnostics-burst"
    if args.scenario in ("captures-delta", "captures-full"):
        args.captures = True
        args.concurrent_captures = True
    if args.burst > 1 and (args.captures or args.concurrent_captures):
        ap.error("--burst cannot be combined with captures modes")
    if args.concurrent_captures:
        args.captures = True

    if args.file:
        if args.file.endswith(".md"):
            lang, ext = "markdown", "md"
        elif args.file.endswith(".rs"):
            lang, ext = "rust", "rs"
        else:
            lang = args.lang
            ext = "rs" if lang == "rust" else "md"
        with open(args.file, encoding="utf-8") as f:
            uri, text = f"file:///profile/input.{ext}", f.read()
    elif args.lang == "rust":
        uri, lang, text = "file:///profile/large.rs", "rust", gen_rust(args.size)
    else:
        uri, lang, text = (
            "file:///profile/inj.md",
            "markdown",
            gen_markdown_injections(args.size),
        )

    env = dict(os.environ, KAKEHASHI_DATA_DIR=args.data_dir)
    if args.stdout_metrics:
        env["KAKEHASHI_STDOUT_METRICS"] = "1"
    srv = subprocess.Popen(
        [args.bin, *args.server_arg],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    rid = 0
    request_samples = defaultdict(list)
    notification_counts = Counter()
    notification_bytes = Counter()
    server_request_counts = Counter()
    server_request_bytes = Counter()
    send_lock = threading.Lock()
    response_condition = threading.Condition()
    received_responses = defaultdict(deque)
    reader_errors = []
    reader_done = False
    transport_frames = []
    stderr_reader_errors = []
    partial_frame_count = 0

    def send(obj):
        body = json.dumps(obj).encode()
        with send_lock:
            srv.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
            srv.stdin.flush()

    def send_request(method, params):
        nonlocal rid
        rid += 1
        send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return rid

    def request(method, params):
        request_id = send_request(method, params)
        return read_until(request_id)

    def notify(method, params):
        send({"jsonrpc": "2.0", "method": method, "params": params})

    def read_message():
        length = None
        while True:
            line = srv.stdout.readline()
            if not line:
                raise EOFError("server closed stdout")
            line = line.strip()
            if not line:
                break
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        if length is None:
            # Without a length, read(None) would block until EOF on the
            # persistent server — fail fast instead.
            raise RuntimeError("missing Content-Length header from server")
        return json.loads(srv.stdout.read(length)), length

    def record_notification(message, wire_bytes):
        method = message.get("method", "<unknown>")
        notification_counts[method] += 1
        notification_bytes[method] += wire_bytes

    def handle_server_method(message, wire_bytes):
        method = message.get("method")
        if not method:
            return False
        if "id" in message:
            server_request_counts[method] += 1
            server_request_bytes[method] += wire_bytes
            send(
                {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": server_request_result(message),
                }
            )
        else:
            record_notification(message, wire_bytes)
        return True

    def collect_stdout():
        nonlocal reader_done
        try:
            while True:
                message, wire_bytes = read_message()
                if handle_server_method(message, wire_bytes):
                    continue
                with response_condition:
                    received_responses[message.get("id")].append(
                        (message, wire_bytes, time.perf_counter())
                    )
                    response_condition.notify_all()
        except EOFError:
            pass
        except BaseException as error:
            reader_errors.append(error)
        finally:
            with response_condition:
                reader_done = True
                response_condition.notify_all()

    def collect_stderr():
        nonlocal partial_frame_count
        for raw_line in srv.stderr:
            try:
                line = raw_line.decode(errors="replace").rstrip("\n")
                frame = parse_stdout_metric_line(line)
                if frame is not None:
                    transport_frames.append(frame)
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    event = None
                if (
                    isinstance(event, dict)
                    and event.get("event") == "stdout_partial_frame"
                ):
                    partial_frame_count += 1
                else:
                    sys.stderr.write(line + "\n")
            except BaseException as error:
                stderr_reader_errors.append(error)

    def wait_response(want_id):
        with response_condition:
            while not received_responses[want_id]:
                if reader_errors:
                    raise reader_errors[0]
                if reader_done:
                    raise RuntimeError(
                        f"server closed stdout before response id={want_id}"
                    )
                response_condition.wait()
            return received_responses[want_id].popleft()

    def read_until(want_id):
        message, wire_bytes, _ = wait_response(want_id)
        return message, wire_bytes

    def measured_request(method, params):
        started = time.perf_counter()
        request_id = send_request(method, params)
        response, wire_bytes, completed_at = wait_response(request_id)
        request_samples[method].append(
            RequestSample(
                seconds=completed_at - started,
                wire_bytes=wire_bytes,
                status=response_status(response),
                completed_at=completed_at,
            )
        )
        return response

    def measured_batch(requests):
        pending = {}
        for method, params in requests:
            started = time.perf_counter()
            request_id = send_request(method, params)
            pending[request_id] = (method, started)

        responses = []
        for request_id, (method, started) in pending.items():
            message, wire_bytes, completed_at = wait_response(request_id)
            request_samples[method].append(
                RequestSample(
                    seconds=completed_at - started,
                    wire_bytes=wire_bytes,
                    status=response_status(message),
                    completed_at=completed_at,
                )
            )
            responses.append(message)
        return responses

    def measured_repeated(method, params, count, before_next=None, delay_seconds=0):
        nonlocal rid
        request_ids = list(range(rid + 1, rid + count + 1))
        rid += count
        pending = {}
        responses = {}
        for index, request_id in enumerate(request_ids):
            pending[request_id] = time.perf_counter()
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                }
            )
            if index + 1 < count and before_next is not None:
                if delay_seconds > 0:
                    time.sleep(delay_seconds)
                before_next()
        for request_id in request_ids:
            message, wire_bytes, completed_at = wait_response(request_id)
            request_samples[method].append(
                RequestSample(
                    seconds=completed_at - pending[request_id],
                    wire_bytes=wire_bytes,
                    status=response_status(message),
                    completed_at=completed_at,
                )
            )
            responses[request_id] = message
        return [responses[request_id] for request_id in request_ids]

    stdout_reader = threading.Thread(target=collect_stdout, daemon=True)
    stderr_reader = threading.Thread(target=collect_stderr, daemon=True)
    stdout_reader.start()
    stderr_reader.start()

    # Drive inside try/finally so any error (e.g. a server error raised in the
    # loop, or a shutdown timeout) still reaps the server instead of leaving a
    # stray process behind.
    try:
        request(
            "initialize",
            {
                "processId": None,
                "rootUri": None,
                "capabilities": {
                    "textDocument": {
                        "semanticTokens": {
                            "requests": {"full": {"delta": True}},
                            "tokenTypes": [],
                            "tokenModifiers": [],
                            "formats": ["relative"],
                        }
                    }
                },
            },
        )
        notify("initialized", {})
        notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri,
                    "languageId": lang,
                    "version": 1,
                    "text": text,
                }
            },
        )
        if args.settle > 0:
            time.sleep(args.settle)  # let the initial parse settle

        captures_result_id = None
        if args.captures:
            seed, _ = request(
                "kakehashi/captures/full",
                {
                    "textDocument": {"uri": uri},
                    "kind": "highlights",
                    "injection": True,
                },
            )
            captures_result_id = response_result_id(seed)
            if captures_result_id is None:
                raise RuntimeError(
                    "captures warmup did not return a resultId; "
                    "cannot measure a real delta lineage"
                )

        ok, canceled, superseded, tokens = 0, 0, 0, 0
        version = 1
        # LSP `character` offsets are UTF-16 code units, not Unicode code
        # points — a non-ASCII first line would make the edit range invalid.
        first_line_len = len(text.split("\n", 1)[0].encode("utf-16-le")) // 2
        t0 = time.time()
        req_times = []
        cycle_success_times = []
        line_has_extra = False

        def send_toggle_edit():
            nonlocal version, line_has_extra
            change, line_has_extra = next_toggle_change(first_line_len, line_has_extra)
            version += 1
            notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [change],
                },
            )

        for i in range(args.requests):
            for j in range(args.edits):
                # A no-op didChange would be deduped by hashes.
                send_toggle_edit()
                # a beat for the off-ingress reparse to run (the profiled work)
                time.sleep(0.01)
            t_req = time.perf_counter()
            semantic_sample_start = len(
                request_samples["textDocument/semanticTokens/full"]
            )
            semantic_request = (
                "textDocument/semanticTokens/full",
                {"textDocument": {"uri": uri}},
            )
            captures_requests = [
                (
                    "kakehashi/captures/full/delta",
                    {
                        "textDocument": {"uri": uri},
                        "kind": "highlights",
                        "previousResultId": (
                            "stale-profile-lineage"
                            if captures_full_fallback
                            else captures_result_id
                        ),
                    },
                ),
            ]
            captures_responses = {}
            if args.concurrent_captures:
                # Queue captures first to model an editor whose highlighting work
                # is already in flight when semantic tokens arrive.
                responses = measured_batch([*captures_requests, semantic_request])
                semantic_responses = [responses[-1]]
                captures_responses = {
                    method: response
                    for (method, _), response in zip(
                        captures_requests, responses[:-1], strict=True
                    )
                }
                captures_result = (
                    captures_responses["kakehashi/captures/full/delta"].get("result")
                    or {}
                )
                expected_field = "matches" if captures_full_fallback else "edits"
                if expected_field not in captures_result:
                    raise RuntimeError(
                        f"captures scenario expected {expected_field}, got "
                        f"{captures_result.keys()}"
                    )
            elif diagnostics_burst:
                diagnostic_request = (
                    "textDocument/diagnostic",
                    {"textDocument": {"uri": uri}},
                )
                responses = measured_batch(
                    [
                        *([diagnostic_request] * args.diagnostics_burst_size),
                        semantic_request,
                    ]
                )
                for response in responses[:-1]:
                    result = response.get("result")
                    if response_status(response) != "ok" or not isinstance(
                        result, dict
                    ):
                        raise RuntimeError(
                            f"diagnostics scenario expected a report, got {response}"
                        )
                    if result.get("kind") not in ("full", "unchanged"):
                        raise RuntimeError(
                            f"diagnostics scenario returned invalid report kind: {result}"
                        )
                semantic_responses = [responses[-1]]
            elif args.burst > 1:
                if args.burst_edits:
                    send_toggle_edit()
                semantic_responses = measured_repeated(
                    *semantic_request,
                    count=args.burst,
                    before_next=send_toggle_edit if args.burst_edits else None,
                    delay_seconds=args.burst_delay_ms / 1000,
                )
            else:
                semantic_responses = [measured_request(*semantic_request)]
                if args.captures:
                    for captures_request in captures_requests:
                        captures_responses[captures_request[0]] = measured_request(
                            *captures_request
                        )
            for method, _ in reversed(captures_requests):
                next_result_id = response_result_id(captures_responses.get(method, {}))
                if next_result_id is not None:
                    captures_result_id = next_result_id
                    break
            req_times.append(time.perf_counter() - t_req)
            cycle_semantic_samples = request_samples[
                "textDocument/semanticTokens/full"
            ][semantic_sample_start:]
            successes = [
                sample.completed_at - t_req
                for sample in cycle_semantic_samples
                if sample.status == "ok"
            ]
            if successes:
                cycle_success_times.append(max(successes))
            cycle_ok, cycle_canceled, cycle_superseded, tokens = (
                count_semantic_outcomes(semantic_responses, tokens)
            )
            ok += cycle_ok
            canceled += cycle_canceled
            superseded += cycle_superseded
        elapsed = time.time() - t0

        request("shutdown", None)
        notify("exit", {})
        srv.wait(timeout=5)
        stdout_reader.join(timeout=5)
        stderr_reader.join(timeout=5)
    except subprocess.TimeoutExpired:
        pass  # graceful shutdown didn't land in time; the finally kills it
    finally:
        if srv.poll() is None:
            srv.kill()
            srv.wait()
        stdout_reader.join(timeout=5)
        stderr_reader.join(timeout=5)

    if args.stdout_metrics:
        expected_transport_counts = {
            method: len(samples) for method, samples in request_samples.items()
        }
        validate_transport_metrics(
            transport_frames,
            expected_transport_counts,
            partial_frame_count,
            stderr_reader_errors,
        )

    n_bytes = len(text.encode("utf-8"))
    n_lines = len(text.splitlines())
    source = (
        f"file={args.file} ({n_bytes}B/{n_lines}L)"
        if args.file
        else f"size={args.size}"
    )
    firsts = " ".join(f"{t * 1000:.0f}" for t in req_times[:5])
    measured_responses = sum(len(samples) for samples in request_samples.values())
    sys.stderr.write(f"[drive] first-cycle ms: {firsts}\n")
    sys.stderr.write(
        f"[drive] scenario={args.scenario or 'custom'} lang={lang} {source} "
        f"cycles={args.requests} burst={args.burst} "
        f"responses={measured_responses} "
        f"ok={ok} canceled={canceled} superseded={superseded} tokens/req={tokens} "
        f"wall={elapsed * 1000:.0f}ms "
        f"({elapsed / args.requests * 1000:.2f}ms/cycle, "
        f"{elapsed / measured_responses * 1000:.2f}ms/measured-response)\n"
    )
    for method, samples in request_samples.items():
        summary = summarize_samples(samples)
        sys.stderr.write(
            f"[drive] method={method} count={summary.count} ok={summary.ok} "
            f"canceled={summary.canceled} null={summary.null} errors={summary.errors} "
            f"p50={summary.p50_ms:.1f}ms p90={summary.p90_ms:.1f}ms "
            f"max={summary.max_ms:.1f}ms wire={summary.wire_bytes / 1024:.1f}KiB\n"
        )
        for status, status_summary in summarize_samples_by_status(samples).items():
            sys.stderr.write(
                f"[drive] method={method} status={status} "
                f"count={status_summary.count} p50={status_summary.p50_ms:.1f}ms "
                f"p90={status_summary.p90_ms:.1f}ms "
                f"max={status_summary.max_ms:.1f}ms\n"
            )
    if cycle_success_times:
        success_samples = [
            RequestSample(seconds=seconds, wire_bytes=0, status="ok")
            for seconds in cycle_success_times
        ]
        summary = summarize_samples(success_samples)
        sys.stderr.write(
            f"[drive] time-to-last-semantic-success cycles={summary.count} "
            f"p50={summary.p50_ms:.1f}ms p90={summary.p90_ms:.1f}ms "
            f"max={summary.max_ms:.1f}ms\n"
        )
    if notification_counts:
        total_count = sum(notification_counts.values())
        total_bytes = sum(notification_bytes.values())
        top = ", ".join(
            f"{method}:{notification_bytes[method] / 1024:.1f}KiB"
            for method, _ in notification_bytes.most_common(5)
        )
        sys.stderr.write(
            f"[drive] notifications count={total_count} wire={total_bytes / 1024:.1f}KiB "
            f"top=[{top}]\n"
        )
    if server_request_counts:
        total_count = sum(server_request_counts.values())
        total_bytes = sum(server_request_bytes.values())
        sys.stderr.write(
            f"[drive] server-requests count={total_count} "
            f"wire={total_bytes / 1024:.1f}KiB methods={dict(server_request_counts)}\n"
        )
    transport_by_method = defaultdict(list)
    for frame in transport_frames:
        if frame.ready_us is not None and frame.method is not None:
            transport_by_method[frame.method].append(frame)
    for method, frames in transport_by_method.items():
        summary = summarize_transport_frames(frames)
        flush_p90 = (
            f"{summary.p90_ready_to_flush_complete_ms:.1f}ms"
            if summary.p90_ready_to_flush_complete_ms is not None
            else "n/a"
        )
        sys.stderr.write(
            f"[stdout] method={method} count={summary.count} "
            f"ready-to-write-complete-p50="
            f"{summary.p50_ready_to_write_complete_ms:.1f}ms "
            f"p90={summary.p90_ready_to_write_complete_ms:.1f}ms "
            f"ready-to-flush-p90={flush_p90} "
            f"censored={summary.censored_flushes} "
            f"max-frame={summary.max_frame_bytes / 1024:.1f}KiB\n"
        )
    if args.stdout_metrics:
        schedulable, already_writing = classify_semantic_blocking(
            transport_frames, int(args.large_frame_kib * 1024)
        )
        sys.stderr.write(
            f"[stdout] semantic-large-frame-overlap "
            f"threshold={args.large_frame_kib:g}KiB "
            f"schedulable-before-write={schedulable} "
            f"already-writing={already_writing}\n"
        )


if __name__ == "__main__":
    main()
