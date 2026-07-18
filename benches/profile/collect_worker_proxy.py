#!/usr/bin/env python3
"""Collect alternating direct/relay run summaries for the Phase 0 probe."""

import argparse
import hashlib
import json
import os
import pathlib
import platform
import re
import subprocess
import sys


SCENARIOS = {
    "rust_cache_hit": ["--lang", "rust", "--size", "15", "--requests", "1000"],
    "rust_edit": [
        "--lang", "rust", "--size", "15", "--requests", "100", "--edits", "1"
    ],
    "markdown_edit": [
        "--lang", "markdown", "--size", "150", "--requests", "100", "--edits", "1"
    ],
}


def run_order(zero_based_run):
    return ("direct", "relay") if zero_based_run % 2 == 0 else ("relay", "direct")


def parse_driver_summary(output):
    wall_match = re.search(r"\bwall=([\d.]+)ms", output)
    method_match = re.search(
        r"method=textDocument/semanticTokens/full count=\d+ .*?"
        r"p50=([\d.]+)ms p90=([\d.]+)ms p95=([\d.]+)ms "
        r"p99=([\d.]+)ms max=([\d.]+)ms wire=([\d.]+)KiB",
        output,
    )
    if not wall_match or not method_match:
        raise ValueError(f"could not parse driver output:\n{output}")
    return {
        "p50": float(method_match.group(1)),
        "p90": float(method_match.group(2)),
        "p95": float(method_match.group(3)),
        "p99": float(method_match.group(4)),
        "max": float(method_match.group(5)),
        "wall": float(wall_match.group(1)),
        "wire_kib": float(method_match.group(6)),
    }


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tool_version(command):
    return subprocess.run(
        command, check=True, text=True, stdout=subprocess.PIPE
    ).stdout.strip()


def run_driver(kind, binary, data_dir, scenario_args, script_dir):
    env = os.environ.copy()
    if kind == "direct":
        server = binary
    else:
        server = script_dir / "worker_proxy.py"
        env["KAKEHASHI_WORKER_PROXY_BIN"] = str(binary)
    command = [
        sys.executable,
        str(script_dir / "drive.py"),
        "--bin",
        str(server),
        "--data-dir",
        str(data_dir),
        *scenario_args,
    ]
    completed = subprocess.run(
        command,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    return parse_driver_summary(completed.stderr)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--pairs", type=int, default=10)
    parser.add_argument(
        "--scenario",
        action="append",
        choices=sorted(SCENARIOS),
        dest="scenarios",
    )
    args = parser.parse_args()
    if args.pairs <= 0:
        parser.error("--pairs must be positive")

    script_dir = pathlib.Path(__file__).resolve().parent
    selected = args.scenarios or list(SCENARIOS)
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-phase0-raw-relay",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "rustc": tool_version(["rustc", "--version"]),
            "binary": str(args.bin.resolve()),
            "binary_sha256": sha256_file(args.bin),
            "data_dir": str(args.data_dir.resolve()),
        },
        "collector": {
            "pairs": args.pairs,
            "order": "direct-relay on odd runs; relay-direct on even runs",
        },
        "steady_state": {},
        "cold_start": {},
    }
    for scenario in selected:
        pairs = []
        for index in range(args.pairs):
            pair = {
                "run": index + 1,
                "order": "-".join(run_order(index)),
            }
            for kind in run_order(index):
                pair[kind] = run_driver(
                    kind,
                    args.bin,
                    args.data_dir,
                    SCENARIOS[scenario],
                    script_dir,
                )
            pairs.append(pair)
            print(f"{scenario}: pair {index + 1}/{args.pairs}", file=sys.stderr)
        result["steady_state"][scenario] = {
            "arguments": SCENARIOS[scenario],
            "pairs": pairs,
        }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
