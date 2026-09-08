#!/usr/bin/env python3
"""Summarize the retained matrix without adding overlapping phase durations."""
import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path
import statistics

from matrix_contract import bridge_coverage, load_matrix, require_identity, require_comparable, require_workload


def distribution(values):
    values = sorted(values)
    if not values:
        return None
    return {
        "n": len(values),
        "median": statistics.median(values),
        "p90": values[max(0, math.ceil(0.9 * len(values)) - 1)],
        "max": values[-1],
    }


def summarize(root):
    matrix = load_matrix(root / "matrix.json")
    groups = defaultdict(list)
    results = {}
    dimensions = ("language", "size", "mode", "documents")
    for run in matrix["runs"]:
        result = json.loads((root / (run["name"] + ".json")).read_text())
        require_identity(matrix, run, result)
        require_workload(run, result, matrix["arguments"]["requests"])
        require_comparable(run["name"], result)
        bridge_coverage(run, result)
        results[run["name"]] = result
        groups[tuple(run[key] for key in dimensions)].append(run)
    summary = []
    for key, runs in groups.items():
        row = dict(zip(dimensions, key))
        pairs = defaultdict(dict)
        for variant in ("baseline", "candidate"):
            selected = [run for run in runs if run["variant"] == variant and not run["profile"]]
            data = [results[run["name"]] for run in selected]
            cycles = [seconds * 1000 for result in data for seconds in result["cycle_seconds"]]
            responses = [sample for result in data
                         for sample in result["request_samples"]["textDocument/semanticTokens/full"]]
            row[variant] = {
                "cycles_ms": distribution(cycles),
                "requests_ms": distribution([sample["seconds"] * 1000 for sample in responses]),
                "statuses": dict(Counter(sample["status"] for sample in responses)),
                "session_cpu_seconds": distribution([
                    result["children_user_seconds"] + result["children_system_seconds"] for result in data
                ]),
                "maxrss_mib": distribution([result["children_maxrss_bytes"] / 1024**2 for result in data]),
            }
            for run, result in zip(selected, data):
                pairs[run["repetition"]][variant] = statistics.median(result["cycle_seconds"]) * 1000
        row["pairs"] = [
            {"repetition": repetition, **pair,
             "delta_percent": (pair["candidate"] / pair["baseline"] - 1) * 100}
            for repetition, pair in pairs.items() if len(pair) == 2
        ]
        row["phases_us"] = {}
        for run in runs:
            if run["profile"]:
                phases = json.loads((root / (run["name"] + ".phases.json")).read_text())
                phase_groups = defaultdict(list)
                for sample in phases:
                    phase_groups[sample["phase"]].append(sample)
                row["phases_us"] = {
                    phase: {
                        **distribution([sample["elapsed_us"] for sample in samples]),
                        "incomplete": sum("completed=false" in sample["details"] for sample in samples),
                    }
                    for phase, samples in phase_groups.items()
                }
        summary.append(row)
    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.write_text(json.dumps(summarize(args.results), indent=2) + "\n", encoding="utf-8")
