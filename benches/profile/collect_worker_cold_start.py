#!/usr/bin/env python3
"""Collect alternating validated fresh-process direct/relay timings."""

import argparse
import json
import os
import pathlib
import platform
import sys
import time

from collect_worker_proxy import (
    artifact_identity,
    artifact_provenance,
    bounded_run,
    build_driver_command,
    controlled_environment,
    harness_identity,
    load_binary_attestation,
    parse_driver_summary,
    require_benchmark_artifacts,
    require_posix,
    run_with_staging_cleanup,
    run_order,
    sha256_file,
    stage_measurement_inputs,
    verify_file_sha256,
)


COLD_START_ARGUMENTS = [
    "--lang", "rust", "--size", "15", "--requests", "1", "--settle", "0"
]


def collect_pairs(pair_count, run):
    pairs = []
    for index in range(pair_count):
        pair = {"run": index + 1, "order": "-".join(run_order(index))}
        for kind in run_order(index):
            pair[kind] = run(kind)
        pairs.append(pair)
    return pairs


def timed_driver(kind, binary, data_dir, script_dir, timeout_seconds):
    environment = controlled_environment(os.environ)
    if kind == "relay":
        environment["KAKEHASHI_WORKER_PROXY_BIN"] = str(binary)
    command = build_driver_command(
        kind, binary, data_dir, COLD_START_ARGUMENTS, script_dir
    )
    started = time.perf_counter()
    completed = bounded_run(command, environment, timeout_seconds)
    elapsed = time.perf_counter() - started
    summary = parse_driver_summary(completed.stderr, expected_count=1)
    return {
        "elapsed_seconds": elapsed,
        "validated_responses": summary["ok"],
        "tokens_per_request": summary["tokens_per_request"],
    }


def main():
    require_posix()
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument(
        "--binary-attestation", type=pathlib.Path, required=True
    )
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--pairs", type=int, default=20)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--run-timeout", type=float, default=60)
    parser.add_argument(
        "--nvim-treesitter-checkout", type=pathlib.Path, required=True
    )
    args = parser.parse_args()
    if args.pairs <= 0 or args.warmups < 0 or args.run_timeout <= 0:
        parser.error("pairs and timeout must be positive; warmups non-negative")

    require_benchmark_artifacts(args.data_dir)
    initial_artifact_identity = artifact_identity(args.data_dir)
    initial_binary_sha256 = sha256_file(args.bin)
    script_dir = pathlib.Path(__file__).resolve().parent
    initial_harness_identity = harness_identity(script_dir)
    binary_attestation = load_binary_attestation(
        args.binary_attestation, args.bin
    )
    staging, binary, data_dir, staged_script_dir = stage_measurement_inputs(
        args.bin, args.data_dir, script_dir
    )
    run_with_staging_cleanup(
        staging,
        lambda: collect_staged(
            args,
            binary,
            data_dir,
            staged_script_dir,
            script_dir,
            initial_harness_identity,
            initial_artifact_identity,
            initial_binary_sha256,
            binary_attestation,
        ),
    )


def collect_staged(
    args,
    binary,
    data_dir,
    staged_script_dir,
    script_dir,
    initial_harness_identity,
    initial_artifact_identity,
    initial_binary_sha256,
    binary_attestation,
):
    if sha256_file(binary) != initial_binary_sha256:
        raise RuntimeError("staged binary does not match attested source")
    if artifact_identity(data_dir) != initial_artifact_identity:
        raise RuntimeError("staged runtime artifacts do not match source")
    require_benchmark_artifacts(data_dir)
    load_binary_attestation(args.binary_attestation, binary)
    if harness_identity(staged_script_dir) != initial_harness_identity:
        raise RuntimeError("staged harness does not match source")

    def run(kind):
        return timed_driver(
            kind, binary, data_dir, staged_script_dir, args.run_timeout
        )

    for index in range(args.warmups):
        for kind in run_order(index):
            run(kind)
    pairs = collect_pairs(args.pairs, run)
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-phase0-cold-start",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "binary": str(args.bin.resolve()),
            "binary_execution": "private staged copy",
            "binary_sha256": initial_binary_sha256,
            "data_dir": str(args.data_dir.resolve()),
            "data_dir_execution": "private staged copy",
            "parser_query_file_count": initial_artifact_identity[0],
            "parser_query_tree_sha256": initial_artifact_identity[1],
            "retained_environment": controlled_environment(os.environ),
        },
        "artifacts": artifact_provenance(
            data_dir, args.nvim_treesitter_checkout
        ),
        "binary_attestation": binary_attestation,
        "harness": {
            "execution": "private staged driver and relay helpers",
            "source_files_sha256": initial_harness_identity,
        },
        "arguments": COLD_START_ARGUMENTS,
        "warmup_pairs": args.warmups,
        "measured_pairs": args.pairs,
        "order": "alternating direct-relay and relay-direct pairs",
        "pairs": pairs,
    }
    if artifact_identity(data_dir) != initial_artifact_identity:
        raise RuntimeError("runtime artifacts changed during cold-start collection")
    verify_file_sha256(binary, initial_binary_sha256)
    if harness_identity(script_dir) != initial_harness_identity:
        raise RuntimeError("benchmark harness changed during collection")
    if harness_identity(staged_script_dir) != initial_harness_identity:
        raise RuntimeError("staged benchmark harness changed during collection")
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
