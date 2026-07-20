#!/usr/bin/env python3
"""Compare isolated tree-worker startup across two harness generations."""

import argparse
import json
import pathlib
import platform
import statistics
import subprocess
import sys

from collect_worker_proxy import cpu_model, sha256_file


def run_order(index):
    return ("baseline", "candidate") if index % 2 == 0 else ("candidate", "baseline")


def parse_harness_json(output):
    decoder = json.JSONDecoder()
    for offset, character in enumerate(output):
        if character != "{":
            continue
        try:
            value, _end = decoder.raw_decode(output[offset:])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "handshake_us" in value:
            return value
    raise ValueError(f"worker startup harness emitted no result JSON:\n{output}")


def summarize_pairs(pairs):
    summary = {}
    for metric in ("handshake_us", "shutdown_us"):
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


def run_harness(path, threads):
    completed = subprocess.run(
        [str(path), "--threads", str(threads)],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=30,
    )
    return parse_harness_json(completed.stdout)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-harness", type=pathlib.Path, required=True)
    parser.add_argument("--candidate-harness", type=pathlib.Path, required=True)
    parser.add_argument("--baseline-commit", required=True)
    parser.add_argument("--candidate-commit", required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--pairs", type=int, default=30)
    parser.add_argument("--warmup-pairs", type=int, default=5)
    parser.add_argument("--threads", type=int, default=4)
    args = parser.parse_args()
    if args.pairs <= 0 or args.warmup_pairs < 0 or args.threads <= 0:
        parser.error("pairs and threads must be positive; warmup pairs must be non-negative")

    harnesses = {
        "baseline": args.baseline_harness,
        "candidate": args.candidate_harness,
    }
    for index in range(args.warmup_pairs):
        for kind in run_order(index):
            run_harness(harnesses[kind], args.threads)

    pairs = []
    for index in range(args.pairs):
        pair = {"run": index + 1, "order": "-".join(run_order(index))}
        for kind in run_order(index):
            pair[kind] = run_harness(harnesses[kind], args.threads)
        pairs.append(pair)
        print(f"measured pair {index + 1}/{args.pairs}", file=sys.stderr)

    result = {
        "schema": 1,
        "experiment": "single-tree-worker-stage23-windows-supervision",
        "environment": {
            "platform": platform.platform(),
            "cpu": cpu_model(),
            "python": platform.python_version(),
            "baseline_commit": args.baseline_commit,
            "candidate_commit": args.candidate_commit,
            "baseline_harness_sha256": sha256_file(args.baseline_harness),
            "candidate_harness_sha256": sha256_file(args.candidate_harness),
        },
        "scenario": {
            "worker_threads": args.threads,
            "order": "baseline-candidate on odd runs; candidate-baseline on even runs",
            "handshake_definition": "Client::spawn through HandshakeReady",
            "shutdown_definition": "close request transport through reaped worker exit",
            "parser_loading": False,
        },
        "warmup_pairs": args.warmup_pairs,
        "measured_pairs": args.pairs,
        "pairs": pairs,
        "summary": summarize_pairs(pairs),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
