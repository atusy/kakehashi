#!/usr/bin/env python3
"""Controlled sync-only LSP peer for routing-cost measurements.

This performs no language analysis. It isolates kakehashi's routing and document
synchronization costs; its results do not predict a real downstream analyzer's
CPU usage or diagnostic latency.
"""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import sys


def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode("ascii").split(":", 1)
        if name.lower() == "content-length":
            length = int(value)
    if length is None:
        raise ValueError("missing Content-Length")
    return json.loads(sys.stdin.buffer.read(length))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary-dir", type=Path)
    args = parser.parse_args()
    methods = Counter()
    languages = Counter()
    opened_documents = set()
    summary_written = False

    def write_summary():
        nonlocal summary_written
        if args.summary_dir and not summary_written:
            args.summary_dir.mkdir(parents=True, exist_ok=True)
            (args.summary_dir / f"{os.getpid()}.json").write_text(
                json.dumps({"methods": methods, "opened_languages": languages,
                            "opened_documents": [{"uri": uri, "language": language}
                                                 for uri, language in sorted(opened_documents)]},
                           indent=2) + "\n",
                encoding="utf-8",
            )
            # Never truncate the acknowledged summary during exit: the parent
            # may send SIGTERM as soon as it receives our shutdown response.
            summary_written = True

    try:
        while (message := read_message()) is not None:
            method = message.get("method")
            if method is None:
                continue
            methods[method] += 1
            if method == "exit":
                break
            if method == "textDocument/didOpen":
                document = message["params"]["textDocument"]
                languages[document["languageId"]] += 1
                opened_documents.add((document["uri"], document["languageId"]))
            if "id" not in message:
                continue
            reply = {"jsonrpc": "2.0", "id": message["id"]}
            if method == "initialize":
                reply["result"] = {"capabilities": {"textDocumentSync": 1}}
            elif method == "shutdown":
                # The parent can terminate us immediately after this response;
                # persist observations before acknowledging graceful shutdown.
                write_summary()
                reply["result"] = None
            else:
                reply["error"] = {"code": -32601, "message": "Method not found"}
            body = json.dumps(reply).encode("utf-8")
            sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
            sys.stdout.buffer.flush()
    finally:
        write_summary()


if __name__ == "__main__":
    main()
