#!/usr/bin/env python3
"""Synchronously drive the kakehashi server through a heavy semanticTokens load.

Unlike piping a static session, this waits for each response before sending the
next request, so the server never coalesces/cancels a superseded request (it
would otherwise answer most with `-32800 Canceled` and do no real work). Run the
server under a sampler (samply/flamegraph) with this as the driver so the sampled
process actually spends its time in the semantic-tokens hot path.

Usage:
    drive.py --bin ./target/profiling/kakehashi --lang rust --size 150 --requests 300
"""
import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass
import json
import math
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen_session import gen_rust, gen_markdown_injections  # noqa: E402


@dataclass(frozen=True)
class RequestSample:
    seconds: float
    wire_bytes: int
    status: str


@dataclass(frozen=True)
class RequestSummary:
    count: int
    ok: int
    canceled: int
    errors: int
    p50_ms: float
    p90_ms: float
    max_ms: float
    wire_bytes: int


def summarize_samples(samples: list[RequestSample]) -> RequestSummary:
    if not samples:
        raise ValueError("cannot summarize an empty request sample")
    latencies = sorted(sample.seconds * 1000 for sample in samples)

    def percentile(q: float) -> float:
        return latencies[max(0, math.ceil(q * len(latencies)) - 1)]

    statuses = Counter(sample.status for sample in samples)
    return RequestSummary(
        count=len(samples),
        ok=statuses["ok"],
        canceled=statuses["canceled"],
        errors=statuses["error"],
        p50_ms=percentile(0.5),
        p90_ms=percentile(0.9),
        max_ms=latencies[-1],
        wire_bytes=sum(sample.wire_bytes for sample in samples),
    )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--server-arg", action="append", default=[],
                    help="extra argument passed to the server (repeatable), e.g. "
                         "--server-arg=--config-file --server-arg=/path/lsp.toml "
                         "to reproduce a real user configuration")
    ap.add_argument("--lang", choices=["rust", "markdown"], default="rust")
    ap.add_argument("--size", type=int, default=150)
    ap.add_argument("--requests", type=int, default=300)
    ap.add_argument("--file", help="drive with this file's content instead of a "
                                   "generated document (language inferred from the "
                                   "extension, falling back to --lang)")
    ap.add_argument(
        "--captures", action="store_true",
        help="also send kakehashi/captures/full (injection mode) per request, "
             "mirroring a captures-protocol highlighter client")
    ap.add_argument(
        "--concurrent-captures", action="store_true",
        help="send semantic tokens and both captures requests as one concurrent "
             "batch; implies --captures and exposes shared-pool/output contention")
    ap.add_argument(
        "--settle", type=float, default=0.3,
        help="seconds to wait after didOpen before the first request "
             "(0 reproduces a client that requests immediately)")
    ap.add_argument(
        "--edits", type=int, default=0,
        help="simulate typing: before each request, send this many incremental "
             "didChange edits (appending/removing a char at the end of the first "
             "line), then request tokens. Exercises the edit->reparse->recompute "
             "cycle instead of the unchanged-document cache hit; token counts "
             "may reflect a trailing snapshot (serve-stale).")
    ap.add_argument(
        "--data-dir", default=os.path.join(os.getcwd(), "deps/test/kakehashi"),
        help="parser/query data dir; must already contain installed parsers "
             "(populated by `cargo test --features e2e` or `make deps/tree-sitter`), "
             "else the server auto-installs on first request")
    args = ap.parse_args()
    if args.requests <= 0:
        ap.error("--requests must be positive")  # avoids divide-by-zero in the summary

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
        uri, lang, text = "file:///profile/inj.md", "markdown", gen_markdown_injections(args.size)

    env = dict(os.environ, KAKEHASHI_DATA_DIR=args.data_dir)
    # Let the server's stderr through (it's silent unless RUST_LOG is set) so a
    # crash or panic is visible instead of being swallowed during profiling.
    srv = subprocess.Popen([args.bin, *args.server_arg], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           env=env)
    rid = 0
    request_samples = defaultdict(list)
    notification_counts = Counter()
    notification_bytes = Counter()

    def send(obj):
        body = json.dumps(obj).encode()
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
                raise RuntimeError("server closed stdout")
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

    def read_until(want_id):
        while True:
            m, wire_bytes = read_message()
            if m.get("method"):
                record_notification(m, wire_bytes)
                continue  # server->client notification/request
            if m.get("id") == want_id:
                return m, wire_bytes

    def response_status(message):
        error = message.get("error")
        if not error:
            return "ok"
        return "canceled" if error.get("code") == -32800 else "error"

    def measured_request(method, params):
        started = time.perf_counter()
        response, wire_bytes = request(method, params)
        request_samples[method].append(RequestSample(
            seconds=time.perf_counter() - started,
            wire_bytes=wire_bytes,
            status=response_status(response),
        ))
        return response

    def measured_batch(requests):
        pending = {}
        for method, params in requests:
            request_id = send_request(method, params)
            pending[request_id] = (method, time.perf_counter())

        responses = {}
        while pending:
            message, wire_bytes = read_message()
            if message.get("method"):
                record_notification(message, wire_bytes)
                continue
            request_id = message.get("id")
            if request_id not in pending:
                continue
            method, started = pending.pop(request_id)
            request_samples[method].append(RequestSample(
                seconds=time.perf_counter() - started,
                wire_bytes=wire_bytes,
                status=response_status(message),
            ))
            responses[method] = message
        return responses

    # Drive inside try/finally so any error (e.g. a server error raised in the
    # loop, or a shutdown timeout) still reaps the server instead of leaving a
    # stray process behind.
    try:
        request("initialize", {"processId": None, "rootUri": None, "capabilities": {
            "textDocument": {"semanticTokens": {"requests": {"full": {"delta": True}},
                                                "tokenTypes": [], "tokenModifiers": [],
                                                "formats": ["relative"]}}}})
        notify("initialized", {})
        notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": lang, "version": 1, "text": text}})
        if args.settle > 0:
            time.sleep(args.settle)  # let the initial parse settle

        ok, canceled, tokens = 0, 0, 0
        version = 1
        # LSP `character` offsets are UTF-16 code units, not Unicode code
        # points — a non-ASCII first line would make the edit range invalid.
        first_line_len = len(text.split("\n", 1)[0].encode("utf-16-le")) // 2
        t0 = time.time()
        req_times = []
        line_has_extra = False
        for i in range(args.requests):
            for j in range(args.edits):
                # Toggle a trailing char on line 0 so the text genuinely changes
                # each time (a no-op didChange would be deduped by hashes).
                grow = not line_has_extra
                version += 1
                if grow:
                    change = {"range": {"start": {"line": 0, "character": first_line_len},
                                        "end": {"line": 0, "character": first_line_len}},
                              "text": "x"}
                else:
                    change = {"range": {"start": {"line": 0, "character": first_line_len},
                                        "end": {"line": 0, "character": first_line_len + 1}},
                              "text": ""}
                line_has_extra = grow
                notify("textDocument/didChange", {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [change]})
                # a beat for the off-ingress reparse to run (the profiled work)
                time.sleep(0.01)
            t_req = time.perf_counter()
            semantic_request = (
                "textDocument/semanticTokens/full",
                {"textDocument": {"uri": uri}},
            )
            captures_requests = [
                ("kakehashi/captures/full",
                 {"textDocument": {"uri": uri}, "kind": "highlights", "injection": True}),
                ("kakehashi/captures/full/delta",
                 {"textDocument": {"uri": uri}, "kind": "highlights",
                  "previousResultId": "warm-miss"}),
            ]
            if args.concurrent_captures:
                # Queue captures first to model an editor whose highlighting work
                # is already in flight when semantic tokens arrive.
                responses = measured_batch([*captures_requests, semantic_request])
                resp = responses[semantic_request[0]]
            else:
                resp = measured_request(*semantic_request)
                if args.captures:
                    for captures_request in captures_requests:
                        measured_request(*captures_request)
            req_times.append(time.perf_counter() - t_req)
            if "error" in resp:
                # Only -32800 (request cancelled) is expected here; any other
                # error means a broken setup that would make the profile
                # meaningless, so surface it instead of counting it as cancelled.
                if resp["error"].get("code") == -32800:
                    canceled += 1
                else:
                    raise RuntimeError(f"server error (not a cancellation): {resp['error']}")
            else:
                ok += 1
                # `result` may be null (the server can answer Ok(None)); `or {}`
                # guards against `None.get`, which a `{}` default would not.
                tokens = len((resp.get("result") or {}).get("data", [])) // 5
        elapsed = time.time() - t0

        request("shutdown", None)
        notify("exit", {})
        srv.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass  # graceful shutdown didn't land in time; the finally kills it
    finally:
        if srv.poll() is None:
            srv.kill()
            srv.wait()

    n_bytes = len(text.encode("utf-8"))
    n_lines = len(text.splitlines())
    source = (f"file={args.file} ({n_bytes}B/{n_lines}L)"
              if args.file else f"size={args.size}")
    firsts = " ".join(f"{t*1000:.0f}" for t in req_times[:5])
    sys.stderr.write(f"[drive] first-cycle ms: {firsts}\n")
    sys.stderr.write(
        f"[drive] lang={lang} {source} requests={args.requests} "
        f"ok={ok} canceled={canceled} tokens/req={tokens} "
        f"wall={elapsed*1000:.0f}ms ({elapsed/args.requests*1000:.2f}ms/req)\n")
    for method, samples in request_samples.items():
        summary = summarize_samples(samples)
        sys.stderr.write(
            f"[drive] method={method} count={summary.count} ok={summary.ok} "
            f"canceled={summary.canceled} errors={summary.errors} "
            f"p50={summary.p50_ms:.1f}ms p90={summary.p90_ms:.1f}ms "
            f"max={summary.max_ms:.1f}ms wire={summary.wire_bytes / 1024:.1f}KiB\n")
    if notification_counts:
        total_count = sum(notification_counts.values())
        total_bytes = sum(notification_bytes.values())
        top = ", ".join(
            f"{method}:{notification_bytes[method] / 1024:.1f}KiB"
            for method, _ in notification_bytes.most_common(5)
        )
        sys.stderr.write(
            f"[drive] notifications count={total_count} wire={total_bytes / 1024:.1f}KiB "
            f"top=[{top}]\n")


if __name__ == "__main__":
    main()
