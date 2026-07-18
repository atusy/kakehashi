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
    "rust_cache_hit": [
        "--lang", "rust", "--size", "15", "--requests", "1000",
        "--warm-semantic-cache",
    ],
    "rust_edit": [
        "--lang", "rust", "--size", "15", "--requests", "100", "--edits", "1"
    ],
    "markdown_edit": [
        "--lang", "markdown", "--size", "150", "--requests", "100", "--edits", "1"
    ],
}


def run_order(zero_based_run):
    return ("direct", "relay") if zero_based_run % 2 == 0 else ("relay", "direct")


def estimated_tree_compute_budget(logical_cpus):
    return max(1, logical_cpus - 2)


def parse_driver_summary(output, expected_count):
    wall_match = re.search(r"\bwall=([\d.]+)ms", output)
    method_match = re.search(
        r"method=textDocument/semanticTokens/full "
        r"count=(\d+) ok=(\d+) canceled=(\d+) null=(\d+) errors=(\d+) "
        r"p50=([\d.]+)ms p90=([\d.]+)ms p95=([\d.]+)ms "
        r"p99=([\d.]+)ms max=([\d.]+)ms wire=([\d.]+)KiB",
        output,
    )
    if not wall_match or not method_match:
        raise ValueError(f"could not parse driver output:\n{output}")
    count, ok, canceled, null, errors = map(
        int, method_match.groups()[:5]
    )
    if count != expected_count:
        raise ValueError(
            f"expected {expected_count} responses, got {count}"
        )
    if ok != count or canceled or null or errors:
        raise ValueError(
            "driver reported non-success responses: "
            f"count={count} ok={ok} canceled={canceled} "
            f"null={null} errors={errors}"
        )
    return {
        "count": count,
        "ok": ok,
        "canceled": canceled,
        "null": null,
        "errors": errors,
        "p50": float(method_match.group(6)),
        "p90": float(method_match.group(7)),
        "p95": float(method_match.group(8)),
        "p99": float(method_match.group(9)),
        "max": float(method_match.group(10)),
        "wall": float(wall_match.group(1)),
        "wire_kib": float(method_match.group(11)),
    }


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def shasum_tree_digest(root):
    """Match `find . | sort | xargs shasum | shasum` from the report."""
    digest = hashlib.sha256()
    for path in sorted(
        (
            item for item in root.rglob("*")
            if item.is_file() and not item.is_symlink()
        ),
        key=lambda item: item.relative_to(root).as_posix(),
    ):
        relative = path.relative_to(root).as_posix()
        digest.update(f"{sha256_file(path)}  ./{relative}\n".encode())
    return digest.hexdigest()


def cpu_model():
    try:
        return tool_version(["sysctl", "-n", "machdep.cpu.brand_string"])
    except (FileNotFoundError, subprocess.CalledProcessError):
        return platform.processor() or "unknown"


def tool_version(command):
    return subprocess.run(
        command, check=True, text=True, stdout=subprocess.PIPE
    ).stdout.strip()


def build_driver_command(kind, binary, data_dir, scenario_args, script_dir):
    if kind == "direct":
        server = str(binary)
        server_args = []
    else:
        server = sys.executable
        server_args = [
            "--server-arg", str(script_dir / "worker_proxy.py")
        ]
    return [
        sys.executable,
        str(script_dir / "drive.py"),
        "--bin",
        server,
        *server_args,
        "--data-dir",
        str(data_dir),
        *scenario_args,
    ]


def option_int(arguments, option, default):
    for index, argument in enumerate(arguments):
        if argument == option:
            return int(arguments[index + 1])
        prefix = f"{option}="
        if argument.startswith(prefix):
            return int(argument.removeprefix(prefix))
    return default


def run_driver(kind, binary, data_dir, scenario_args, script_dir):
    env = os.environ.copy()
    if kind == "relay":
        env["KAKEHASHI_WORKER_PROXY_BIN"] = str(binary)
    command = build_driver_command(
        kind, binary, data_dir, scenario_args, script_dir
    )
    completed = subprocess.run(
        command,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    expected_count = option_int(scenario_args, "--requests", 300) * option_int(
        scenario_args, "--burst", 1
    )
    return parse_driver_summary(completed.stderr, expected_count)


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
    logical_cpus = os.cpu_count() or 1
    data_files = [
        path for path in args.data_dir.rglob("*")
        if path.is_file() and not path.is_symlink()
    ]
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-phase0-raw-relay",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "rustc": tool_version(["rustc", "--version"]),
            "cpu_model": cpu_model(),
            "logical_cpus": logical_cpus,
            "estimated_tree_compute_budget": estimated_tree_compute_budget(
                logical_cpus
            ),
            "estimated_tree_compute_budget_source": (
                "os.cpu_count approximation of available_parallelism - 2 policy; "
                "not the binary's reported effective pool size"
            ),
            "binary": str(args.bin.resolve()),
            "binary_sha256": sha256_file(args.bin),
            "data_dir": str(args.data_dir.resolve()),
            "parser_query_file_count": len(data_files),
            "parser_query_tree_sha256": shasum_tree_digest(args.data_dir),
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
