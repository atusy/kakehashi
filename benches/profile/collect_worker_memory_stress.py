#!/usr/bin/env python3
"""Collect repeated reduced-limit worker memory-admission stress runs."""

import argparse
import json
import os
import pathlib
import platform
import subprocess
import sys
import threading
import time

from collect_worker_memory import (
    descendant_pids,
    read_process_table,
    read_total_footprint_bytes,
    summarize_samples,
)
from collect_worker_proxy import (
    artifact_identity,
    controlled_environment,
    cpu_model,
    portable_environment,
    require_benchmark_artifacts,
    require_posix,
    sha256_file,
    tool_version,
    verify_file_sha256,
)


def parse_report(stdout):
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        raise ValueError("stress driver must emit exactly one JSON report")
    report = json.loads(lines[0])
    if not report.get("recomputed_tokens_match"):
        raise ValueError("recomputed semantic tokens did not match")
    if not report.get("node_identity_survived"):
        raise ValueError("node identity did not survive cache pressure")
    for key in ("full_sync_rejection", "incremental_edit_rejection"):
        if "non-evictable budget" not in report.get(key, ""):
            raise ValueError(f"{key} was not a contained hard-budget rejection")
    a = report["a_after_pressure"]
    b = report["b_after_pressure"]
    if a["result_cache_bytes"] != 0 or a["auxiliary_cache_bytes"] != 0:
        raise ValueError("non-current document A was not fully evicted")
    if b["result_cache_bytes"] <= 0 or b["auxiliary_cache_bytes"] != 0:
        raise ValueError("current document B did not retain only its result cache")
    return report


def sample_process_tree(
    root_pid, stop, interval_seconds, footprint_interval_seconds, samples
):
    next_footprint = 0.0
    while not stop.wait(interval_seconds):
        table = read_process_table()
        if root_pid not in table:
            continue
        pids = {root_pid, *descendant_pids(table, root_pid)}
        sample = {
            "total_rss_kib": sum(table[pid]["rss_kib"] for pid in pids),
            "process_count": len(pids),
        }
        now = time.monotonic()
        if sys.platform == "darwin" and now >= next_footprint:
            try:
                sample["total_footprint_bytes"] = read_total_footprint_bytes(pids)
            except (OSError, subprocess.SubprocessError, ValueError):
                pass
            next_footprint = now + footprint_interval_seconds
        samples.append(sample)


def run_once(args):
    command = [
        str(args.driver),
        "--bin",
        str(args.bin),
        "--data-dir",
        str(args.data_dir),
        "--derived-cache-soft-bytes",
        str(args.derived_cache_soft_bytes),
        "--non-evictable-estimate-hard-bytes",
        str(args.non_evictable_estimate_hard_bytes),
        "--hold-open",
        str(args.hold_open),
    ]
    process = subprocess.Popen(
        command,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=controlled_environment(os.environ),
        start_new_session=True,
    )
    stop = threading.Event()
    samples = []
    sampler = threading.Thread(
        target=sample_process_tree,
        args=(
            process.pid,
            stop,
            args.sample_interval_ms / 1000,
            args.footprint_interval_ms / 1000,
            samples,
        ),
        daemon=True,
    )
    sampler.start()
    try:
        stdout, stderr = process.communicate(timeout=args.run_timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, 15)
        except ProcessLookupError:
            pass
        stdout, stderr = process.communicate(timeout=5)
        raise subprocess.TimeoutExpired(command, args.run_timeout, stdout, stderr)
    finally:
        stop.set()
        sampler.join(timeout=5)
    if process.returncode:
        raise subprocess.CalledProcessError(
            process.returncode, command, output=stdout, stderr=stderr
        )
    return {
        "report": parse_report(stdout),
        "memory": summarize_samples(samples),
    }


def git_output(repo, *arguments):
    return subprocess.run(
        ["git", *arguments],
        cwd=repo,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()


def collect(args):
    repo = pathlib.Path(__file__).resolve().parents[2]
    if git_output(repo, "status", "--porcelain"):
        raise RuntimeError("stress measurement requires a clean source tree")
    source_commit = git_output(repo, "rev-parse", "HEAD")
    worker_sha256 = sha256_file(args.bin)
    driver_sha256 = sha256_file(args.driver)
    artifacts = artifact_identity(args.data_dir)
    batches = []
    for index in range(args.batches):
        batches.append({"batch": index + 1, **run_once(args)})
        print(f"batch {index + 1}/{args.batches}", file=sys.stderr)
    verify_file_sha256(args.bin, worker_sha256)
    verify_file_sha256(args.driver, driver_sha256)
    if artifact_identity(args.data_dir) != artifacts:
        raise RuntimeError("runtime artifacts changed during stress collection")
    if git_output(repo, "rev-parse", "HEAD") != source_commit:
        raise RuntimeError("source commit changed during stress collection")
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-stage18-memory-boundary-stress",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "rustc": tool_version(["rustc", "--version"]),
            "cpu_model": cpu_model(),
            "source_commit": source_commit,
            "worker_binary_sha256": worker_sha256,
            "driver_binary_sha256": driver_sha256,
            "worker_build_command": "cargo build --locked --release --bin kakehashi",
            "driver_build_command": (
                "cargo build --locked --release --example tree_worker_memory_stress"
            ),
            "features": "default",
            "retained_environment": portable_environment(os.environ),
        },
        "artifacts": artifacts,
        "collector": {
            "batches": args.batches,
            "sample_interval_ms": args.sample_interval_ms,
            "footprint_interval_ms": args.footprint_interval_ms,
            "metric": "sum of RSS/footprint for the stress driver and worker child",
        },
        "scenario": {
            "derived_cache_soft_bytes": args.derived_cache_soft_bytes,
            "non_evictable_estimate_hard_bytes": (
                args.non_evictable_estimate_hard_bytes
            ),
            "hold_open_seconds": args.hold_open,
            "documents": 2,
            "language": "rust",
        },
        "batches": batches,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


def main():
    require_posix()
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument("--driver", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--batches", type=int, default=6)
    parser.add_argument("--derived-cache-soft-bytes", type=int, default=20)
    parser.add_argument("--non-evictable-estimate-hard-bytes", type=int, default=4096)
    parser.add_argument("--hold-open", type=float, default=2.0)
    parser.add_argument("--sample-interval-ms", type=float, default=20)
    parser.add_argument("--footprint-interval-ms", type=float, default=250)
    parser.add_argument("--run-timeout", type=float, default=30)
    args = parser.parse_args()
    if min(
        args.batches,
        args.derived_cache_soft_bytes,
        args.non_evictable_estimate_hard_bytes,
        args.hold_open,
        args.sample_interval_ms,
        args.footprint_interval_ms,
        args.run_timeout,
    ) <= 0:
        parser.error("budgets, batches, intervals, and timeout must be positive")
    require_benchmark_artifacts(args.data_dir)
    collect(args)


if __name__ == "__main__":
    main()
