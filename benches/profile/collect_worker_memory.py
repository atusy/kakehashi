#!/usr/bin/env python3
"""Measure aggregate parent-plus-worker RSS for paired LSP workloads."""

import argparse
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import re

from collect_worker_proxy import (
    artifact_identity,
    build_driver_command,
    controlled_environment,
    cpu_model,
    harness_identity,
    parse_driver_summary,
    portable_environment,
    require_benchmark_artifacts,
    require_posix,
    sha256_file,
    stage_measurement_inputs,
    tool_version,
    verify_file_sha256,
)


def scenario_arguments(language, size, requests):
    return [
        "--lang", language, "--size", str(size), "--requests", str(requests),
        "--warm-semantic-cache", "--settle", "0.5", "--hold-open", "1.0",
    ]


def parse_ps_table(output):
    table = {}
    for line in output.splitlines():
        fields = line.split(None, 3)
        if len(fields) != 4:
            continue
        try:
            pid, ppid, rss_kib = map(int, fields[:3])
        except ValueError:
            continue
        table[pid] = {
            "ppid": ppid,
            "rss_kib": rss_kib,
            "command": fields[3],
        }
    return table


def descendant_pids(table, root_pid):
    descendants = set()
    frontier = {root_pid}
    while frontier:
        children = {
            pid for pid, row in table.items()
            if row["ppid"] in frontier and pid not in descendants
        }
        descendants.update(children)
        frontier = children
    return descendants


def summarize_samples(samples):
    if not samples:
        raise ValueError("memory sampler did not observe a server process")
    stable = samples[len(samples) // 2:]
    result = {
        "sample_count": len(samples),
        "peak_total_rss_kib": max(row["total_rss_kib"] for row in samples),
        "stable_median_total_rss_kib": int(statistics.median(
            row["total_rss_kib"] for row in stable
        )),
        "peak_process_count": max(row["process_count"] for row in samples),
    }
    footprint = [
        row["total_footprint_bytes"]
        for row in samples
        if "total_footprint_bytes" in row
    ]
    if footprint:
        result.update({
            "footprint_sample_count": len(footprint),
            "peak_total_footprint_bytes": max(footprint),
            "stable_median_total_footprint_bytes": int(statistics.median(
                footprint[len(footprint) // 2:]
            )),
        })
    return result


def parse_footprint_bytes(output):
    summary = re.search(r"Summary Footprint:\s*(\d+) B", output)
    if summary:
        return int(summary.group(1))
    process = re.search(r"(?m)^.*?Footprint:\s*(\d+) B", output)
    if process:
        return int(process.group(1))
    raise ValueError("footprint output did not contain a byte total")


def read_total_footprint_bytes(pids):
    command = ["footprint"]
    for pid in sorted(pids):
        command.extend(("-p", str(pid)))
    command.extend(("-f", "bytes", "--noCategories"))
    output = subprocess.run(
        command,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    ).stdout
    return parse_footprint_bytes(output)


def read_process_table():
    output = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss=,command="],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout
    return parse_ps_table(output)


def sample_descendants(
    root_pid, stop, interval_seconds, footprint_interval_seconds, samples
):
    next_footprint = 0.0
    while not stop.wait(interval_seconds):
        table = read_process_table()
        pids = descendant_pids(table, root_pid)
        if not pids:
            continue
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


def run_variant(
    kind, binary, data_dir, script_dir, scenario, requests, threads, timeout,
    interval, footprint_interval
):
    environment = controlled_environment(os.environ)
    if kind == "authoritative":
        environment["KAKEHASHI_TREE_WORKER_MODE"] = "authoritative"
        environment["KAKEHASHI_TREE_WORKER_THREADS"] = str(threads)
    command = build_driver_command(
        "direct", binary, data_dir, scenario, script_dir
    )
    with tempfile.TemporaryDirectory(prefix="kakehashi-memory-xdg-") as xdg:
        environment["XDG_CONFIG_HOME"] = str(pathlib.Path(xdg) / "config")
        environment["XDG_STATE_HOME"] = str(pathlib.Path(xdg) / "state")
        process = subprocess.Popen(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            start_new_session=True,
        )
        stop = threading.Event()
        samples = []
        sampler = threading.Thread(
            target=sample_descendants,
            args=(process.pid, stop, interval, footprint_interval, samples),
            daemon=True,
        )
        sampler.start()
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, 15)
            except ProcessLookupError:
                pass
            stdout, stderr = process.communicate(timeout=5)
            raise subprocess.TimeoutExpired(command, timeout, stdout, stderr)
        finally:
            stop.set()
            sampler.join(timeout=5)
    if process.returncode:
        raise subprocess.CalledProcessError(
            process.returncode, command, output=stdout, stderr=stderr
        )
    result = parse_driver_summary(stderr, requests)
    result["memory"] = summarize_samples(samples)
    return result


def order(batch_index):
    if batch_index % 2 == 0:
        return ("legacy", "authoritative")
    return ("authoritative", "legacy")


def collect(args):
    source_script_dir = pathlib.Path(__file__).resolve().parent
    binary_sha256 = sha256_file(args.bin)
    source_harness = harness_identity(source_script_dir)
    source_artifacts = artifact_identity(args.data_dir)
    staging, binary, data_dir, script_dir = stage_measurement_inputs(
        args.bin, args.data_dir, source_script_dir
    )
    scenario = scenario_arguments(args.lang, args.size, args.requests)
    try:
        batches = []
        for index in range(args.batches):
            batch = {"batch": index + 1, "order": list(order(index))}
            for kind in order(index):
                batch[kind] = run_variant(
                    kind,
                    binary,
                    data_dir,
                    script_dir,
                    scenario,
                    args.requests,
                    args.worker_threads,
                    args.run_timeout,
                    args.sample_interval_ms / 1000,
                    args.footprint_interval_ms / 1000,
                )
            batches.append(batch)
            print(f"batch {index + 1}/{args.batches}", file=sys.stderr)
        if artifact_identity(data_dir) != source_artifacts:
            raise RuntimeError("runtime artifacts changed during collection")
        verify_file_sha256(binary, binary_sha256)
        if harness_identity(source_script_dir) != source_harness:
            raise RuntimeError("benchmark harness changed during collection")
    finally:
        staging.cleanup()
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-stage15-memory",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "rustc": tool_version(["rustc", "--version"]),
            "cpu_model": cpu_model(),
            "binary_sha256": binary_sha256,
            "retained_environment": portable_environment(os.environ),
        },
        "artifacts": source_artifacts,
        "harness": {"source_files_sha256": source_harness},
        "collector": {
            "batches": args.batches,
            "order": "legacy-authoritative on odd batches; reversed on even batches",
            "worker_threads": args.worker_threads,
            "sample_interval_ms": args.sample_interval_ms,
            "footprint_interval_ms": args.footprint_interval_ms,
            "metric": "sum of RSS for every descendant of the benchmark driver",
        },
        "scenario": scenario,
        "batches": batches,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


def main():
    require_posix()
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--batches", type=int, default=4)
    parser.add_argument("--lang", choices=("rust", "markdown"), default="markdown")
    parser.add_argument("--size", type=int, default=150)
    parser.add_argument("--requests", type=int, default=500)
    parser.add_argument("--worker-threads", type=int, default=4)
    parser.add_argument("--sample-interval-ms", type=float, default=10)
    parser.add_argument("--footprint-interval-ms", type=float, default=250)
    parser.add_argument("--run-timeout", type=float, default=120)
    args = parser.parse_args()
    if min(
        args.batches,
        args.size,
        args.requests,
        args.worker_threads,
        args.sample_interval_ms,
        args.footprint_interval_ms,
        args.run_timeout,
    ) <= 0:
        parser.error("batch, thread, interval, and timeout values must be positive")
    require_benchmark_artifacts(args.data_dir)
    collect(args)


if __name__ == "__main__":
    main()
