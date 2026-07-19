#!/usr/bin/env python3
"""Collect paired Stage 3 shadow/control runs with reproducible provenance."""

import argparse
import json
import os
import pathlib
import platform
import re
import sys
import tempfile

from collect_worker_proxy import (
    artifact_identity,
    artifact_provenance,
    bounded_run,
    build_driver_command,
    controlled_environment,
    cpu_model,
    harness_identity,
    load_binary_attestation,
    parse_driver_summary,
    portable_environment,
    require_benchmark_artifacts,
    require_posix,
    sha256_file,
    stage_measurement_inputs,
    tool_version,
    verify_file_sha256,
)

SCENARIO = [
    "--lang", "rust", "--size", "200", "--requests", "200",
    "--edits", "1", "--warm-semantic-cache", "--settle", "0.5",
]
COMPARISON_RE = re.compile(
    r"shadow comparisons matched=(\d+) mismatched=(\d+) "
    r"superseded=(\d+) pending=(\d+)"
)


def order(batch_index):
    return ("disabled", "shadow") if batch_index % 2 == 0 else ("shadow", "disabled")


def parse_comparisons(output, expected=201):
    matches = COMPARISON_RE.findall(output)
    if len(matches) != 1:
        raise ValueError(f"expected one complete shadow summary, got {len(matches)}")
    values = map(int, matches[0])
    result = dict(
        zip(("matched", "mismatched", "superseded", "pending"), values, strict=True)
    )
    if (
        result["matched"] != expected
        or result["mismatched"]
        or result["superseded"]
        or result["pending"]
    ):
        raise ValueError(f"shadow validation did not converge: {result}")
    return result


def delta_percent(control, shadow, key):
    return round((shadow[key] / control[key] - 1) * 100, 2)


def run_variant(kind, binary, data_dir, script_dir, threads, timeout):
    environment = controlled_environment(os.environ)
    environment["KAKEHASHI_TREE_WORKER_SHADOW"] = "true" if kind == "shadow" else "false"
    environment["KAKEHASHI_TREE_WORKER_THREADS"] = str(threads)
    environment["RUST_LOG"] = (
        "kakehashi::tree_worker_shadow=info,"
        "kakehashi::tree_worker_shadow_metrics=info"
    )
    with tempfile.TemporaryDirectory(prefix="kakehashi-shadow-xdg-") as xdg:
        environment["XDG_CONFIG_HOME"] = str(pathlib.Path(xdg) / "config")
        environment["XDG_STATE_HOME"] = str(pathlib.Path(xdg) / "state")
        completed = bounded_run(
            build_driver_command("direct", binary, data_dir, SCENARIO, script_dir),
            environment,
            timeout_seconds=timeout,
        )
    result = parse_driver_summary(completed.stderr, 200)
    if kind == "shadow":
        result["comparisons"] = parse_comparisons(completed.stderr)
    elif "shadow comparisons matched=" in completed.stderr:
        raise ValueError("control run unexpectedly enabled shadow mode")
    return result


def collect(args):
    source_script_dir = pathlib.Path(__file__).resolve().parent
    binary_sha256 = sha256_file(args.bin)
    attestation = load_binary_attestation(args.binary_attestation, args.bin)
    source_harness = harness_identity(source_script_dir)
    source_artifacts = artifact_identity(args.data_dir)
    staging, binary, data_dir, script_dir = stage_measurement_inputs(
        args.bin, args.data_dir, source_script_dir
    )
    try:
        provenance = artifact_provenance(data_dir, args.nvim_treesitter_checkout)
        batches = []
        for index in range(args.batches):
            batch = {"batch": index + 1, "order": list(order(index))}
            for kind in order(index):
                batch[kind] = run_variant(
                    kind, binary, data_dir, script_dir, args.worker_threads, args.run_timeout
                )
            batch["shadow_delta_percent"] = {
                key: delta_percent(batch["disabled"], batch["shadow"], key)
                for key in ("wall", "p50", "p90", "p95", "p99")
            }
            batches.append(batch)
            print(f"batch {index + 1}/{args.batches}", file=sys.stderr)
        if artifact_identity(data_dir) != source_artifacts:
            raise RuntimeError("runtime artifacts changed during collection")
        verify_file_sha256(binary, binary_sha256)
        if harness_identity(source_script_dir) != source_harness:
            raise RuntimeError("benchmark harness changed during collection")
        result = {
            "schema": 1,
            "experiment": "single-tree-worker-stage3-shadow",
            "environment": {
                "platform": platform.platform(),
                "python": platform.python_version(),
                "rustc": tool_version(["rustc", "--version"]),
                "cpu_model": cpu_model(),
                "binary_sha256": binary_sha256,
                "retained_environment": portable_environment(os.environ),
            },
            "artifacts": provenance,
            "binary_attestation": attestation,
            "harness": {
                "execution": "private staged driver and collector helpers",
                "source_files_sha256": source_harness,
            },
            "collector": {
                "batches": args.batches,
                "order": "disabled-shadow on odd batches; shadow-disabled on even batches",
                "fresh_xdg_per_variant": True,
                "shadow_compute_threads": args.worker_threads,
            },
            "steady_state": {"rust_edit_semantic_tokens": {
                "arguments": SCENARIO,
                "batches": batches,
            }},
        }
        args.output.write_text(json.dumps(result, indent=2) + "\n")
    finally:
        staging.cleanup()


def main():
    require_posix()
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--binary-attestation", type=pathlib.Path, required=True)
    parser.add_argument("--nvim-treesitter-checkout", type=pathlib.Path, required=True)
    parser.add_argument("--batches", type=int, default=2)
    parser.add_argument("--worker-threads", type=int, default=4)
    parser.add_argument("--run-timeout", type=float, default=120)
    args = parser.parse_args()
    if args.batches <= 0 or args.worker_threads <= 0 or args.run_timeout <= 0:
        parser.error("batches, worker-threads, and run-timeout must be positive")
    require_benchmark_artifacts(args.data_dir)
    collect(args)


if __name__ == "__main__":
    main()
