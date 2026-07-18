#!/usr/bin/env python3
"""Collect the validated concurrent-captures Phase 0 smoke result."""

import argparse
import json
import os
import pathlib
import platform
import sys

from collect_worker_proxy import (
    artifact_identity,
    artifact_provenance,
    bounded_run,
    build_driver_command,
    controlled_environment,
    parse_capture_pilot_summary,
    load_binary_attestation,
    require_benchmark_artifacts,
    require_posix,
    sha256_file,
    verify_file_sha256,
)


CAPTURE_ARGUMENTS = [
    "--lang", "markdown", "--size", "150", "--requests", "100",
    "--edits", "1", "--concurrent-captures",
]


def collect_path(kind, binary, data_dir, script_dir, timeout_seconds):
    environment = controlled_environment(os.environ)
    if kind == "relay":
        environment["KAKEHASHI_WORKER_PROXY_BIN"] = str(binary)
    command = build_driver_command(
        kind, binary, data_dir, CAPTURE_ARGUMENTS, script_dir
    )
    completed = bounded_run(command, environment, timeout_seconds)
    return parse_capture_pilot_summary(completed.stderr, expected_count=100)


def main():
    require_posix()
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument(
        "--binary-attestation", type=pathlib.Path, required=True
    )
    parser.add_argument("--run-timeout", type=float, default=60)
    parser.add_argument(
        "--nvim-treesitter-checkout", type=pathlib.Path, required=True
    )
    args = parser.parse_args()
    if args.run_timeout <= 0:
        parser.error("--run-timeout must be positive")

    require_benchmark_artifacts(args.data_dir)
    initial_identity = artifact_identity(args.data_dir)
    initial_binary_sha256 = sha256_file(args.bin)
    binary_attestation = load_binary_attestation(
        args.binary_attestation, args.bin
    )
    script_dir = pathlib.Path(__file__).resolve().parent
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-phase0-concurrent-captures-pilot",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "binary": str(args.bin.resolve()),
            "binary_sha256": initial_binary_sha256,
            "data_dir": str(args.data_dir.resolve()),
            "parser_query_file_count": initial_identity[0],
            "parser_query_tree_sha256": initial_identity[1],
            "retained_environment": controlled_environment(os.environ),
        },
        "artifacts": artifact_provenance(
            args.data_dir, args.nvim_treesitter_checkout
        ),
        "binary_attestation": binary_attestation,
        "arguments": CAPTURE_ARGUMENTS,
        "independent_pairs": 1,
        "order": "direct then relay; smoke result only",
        "direct": collect_path(
            "direct", args.bin, args.data_dir, script_dir, args.run_timeout
        ),
        "relay": collect_path(
            "relay", args.bin, args.data_dir, script_dir, args.run_timeout
        ),
    }
    final_identity = artifact_identity(args.data_dir)
    if final_identity != initial_identity:
        raise RuntimeError(
            "runtime artifacts changed during collection: "
            f"before={initial_identity} after={final_identity}"
        )
    verify_file_sha256(args.bin, initial_binary_sha256)
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
