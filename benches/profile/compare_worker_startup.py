#!/usr/bin/env python3
"""Compare worker handshake and LSP cold start across two Linux binaries."""

import argparse
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile
import time

from collect_worker_proxy import cpu_model, parse_driver_summary, sha256_file


def run_order(index):
    return ("baseline", "candidate") if index % 2 == 0 else ("candidate", "baseline")


def collect_pairs(pair_count, run):
    pairs = []
    for index in range(pair_count):
        pair = {"run": index + 1, "order": "-".join(run_order(index))}
        for kind in run_order(index):
            pair[kind] = run(kind)
        pairs.append(pair)
        print(f"measured pair {index + 1}/{pair_count}", file=sys.stderr)
    return pairs


def parse_benchmark_json(output):
    decoder = json.JSONDecoder()
    for offset, character in enumerate(output):
        if character != "{":
            continue
        try:
            value, _end = decoder.raw_decode(output[offset:])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "cold_start_us" in value:
            return value
    raise ValueError(f"worker benchmark emitted no result JSON:\n{output}")


def summarize_pairs(pairs):
    summary = {}
    for metric in ("handshake_us", "lsp_elapsed_ms", "lsp_request_ms"):
        baseline = [pair["baseline"][metric] for pair in pairs]
        candidate = [pair["candidate"][metric] for pair in pairs]
        paired_delta_percent = [
            (new - old) / old * 100 for old, new in zip(baseline, candidate)
        ]
        summary[metric] = {
            "baseline_median": statistics.median(baseline),
            "candidate_median": statistics.median(candidate),
            "median_paired_delta_percent": statistics.median(paired_delta_percent),
            "baseline_min": min(baseline),
            "baseline_max": max(baseline),
            "candidate_min": min(candidate),
            "candidate_max": max(candidate),
        }
    return summary


def run_handshake(bench, binary, parser):
    command = [
        str(bench), "--bin", str(binary), "--parser", str(parser),
        "--requests", "1", "--threads", "4", "--lines", "15",
    ]
    completed = subprocess.run(
        command, check=True, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, timeout=30,
    )
    result = parse_benchmark_json(completed.stdout)
    return result["cold_start_us"]


def run_lsp(binary, data_dir, script_dir):
    with tempfile.TemporaryDirectory(prefix="kakehashi-stage22-xdg-") as root:
        environment = dict(os.environ)
        environment["XDG_CONFIG_HOME"] = str(pathlib.Path(root) / "config")
        environment["XDG_STATE_HOME"] = str(pathlib.Path(root) / "state")
        command = [
            sys.executable, str(script_dir / "drive.py"),
            "--bin", str(binary), "--data-dir", str(data_dir),
            "--lang", "rust", "--size", "15", "--requests", "1",
            "--settle", "0",
        ]
        started = time.perf_counter()
        completed = subprocess.run(
            command, check=True, text=True, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, env=environment, timeout=60,
        )
        elapsed_ms = (time.perf_counter() - started) * 1_000
    driver = parse_driver_summary(completed.stderr, expected_count=1)
    return elapsed_ms, driver["wall"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-bin", type=pathlib.Path, required=True)
    parser.add_argument("--candidate-bin", type=pathlib.Path, required=True)
    parser.add_argument("--bench", type=pathlib.Path, required=True)
    parser.add_argument("--parser", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--baseline-commit", required=True)
    parser.add_argument("--candidate-commit", required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--pairs", type=int, default=12)
    parser.add_argument("--warmup-pairs", type=int, default=2)
    args = parser.parse_args()
    if args.pairs <= 0 or args.warmup_pairs < 0:
        parser.error("pairs must be positive and warmup pairs non-negative")

    binaries = {"baseline": args.baseline_bin, "candidate": args.candidate_bin}
    script_dir = pathlib.Path(__file__).resolve().parent

    def run(kind):
        binary = binaries[kind]
        handshake_us = run_handshake(args.bench, binary, args.parser)
        lsp_elapsed_ms, lsp_request_ms = run_lsp(binary, args.data_dir, script_dir)
        return {
            "handshake_us": handshake_us,
            "lsp_elapsed_ms": lsp_elapsed_ms,
            "lsp_request_ms": lsp_request_ms,
        }

    for index in range(args.warmup_pairs):
        for kind in run_order(index):
            run(kind)
    pairs = collect_pairs(args.pairs, run)
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-stage22-linux-pdeathsig",
        "environment": {
            "platform": platform.platform(),
            "cpu": cpu_model(),
            "python": platform.python_version(),
            "baseline_commit": args.baseline_commit,
            "candidate_commit": args.candidate_commit,
            "baseline_binary_sha256": sha256_file(args.baseline_bin),
            "candidate_binary_sha256": sha256_file(args.candidate_bin),
        },
        "scenario": {
            "worker_threads": 4,
            "language": "rust",
            "generated_size": 15,
            "measured_requests": 1,
            "fresh_xdg_per_lsp_run": True,
            "order": "baseline-candidate on odd runs; candidate-baseline on even runs",
            "handshake_definition": "Client::spawn through HandshakeReady",
            "lsp_elapsed_definition": "process spawn through validated request and clean shutdown",
            "lsp_request_definition": "semantic request send through validated response",
        },
        "warmup_pairs": args.warmup_pairs,
        "measured_pairs": args.pairs,
        "pairs": pairs,
        "summary": summarize_pairs(pairs),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
