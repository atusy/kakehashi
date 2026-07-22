#!/usr/bin/env python3
"""Summarize attested semantic-token A/B pair files."""

from __future__ import annotations

import argparse
import json
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable


def conventional_median(values: Iterable[int | float]) -> float:
    values = list(values)
    if not values:
        raise ValueError("cannot summarize empty samples")
    return float(statistics.median(values))


def summarize_pairs(documents: Iterable[dict[str, Any]]) -> dict[str, Any]:
    paired: dict[str, dict[int, dict[str, float]]] = defaultdict(
        lambda: defaultdict(dict)
    )
    orders: dict[int, str] = {}

    for document in documents:
        pair_index = int(document["pair_index"])
        orders[pair_index] = str(document["order"])
        for run in document["runs"]:
            scenario = str(run["scenario"])
            label = str(run["binary_label"])
            if label not in {"A", "B"}:
                raise ValueError(f"unexpected binary label: {label}")
            if label in paired[scenario][pair_index]:
                raise ValueError(
                    f"duplicate {label} run for pair {pair_index}, scenario {scenario}"
                )
            paired[scenario][pair_index][label] = conventional_median(
                run["samples_ns"]
            )

    scenarios = []
    for scenario, pairs in sorted(paired.items()):
        pair_rows = []
        for pair_index, values in sorted(pairs.items()):
            if set(values) != {"A", "B"}:
                raise ValueError(
                    f"incomplete pair {pair_index} for {scenario}: {sorted(values)}"
                )
            a_ns = values["A"]
            b_ns = values["B"]
            delta_percent = (b_ns - a_ns) / a_ns * 100.0
            pair_rows.append(
                {
                    "pair_index": pair_index,
                    "order": orders[pair_index],
                    "a_median_ns": a_ns,
                    "b_median_ns": b_ns,
                    "delta_percent": delta_percent,
                }
            )

        deltas = [row["delta_percent"] for row in pair_rows]
        scenarios.append(
            {
                "scenario": scenario,
                "pair_count": len(pair_rows),
                "a_median_ns": conventional_median(
                    row["a_median_ns"] for row in pair_rows
                ),
                "b_median_ns": conventional_median(
                    row["b_median_ns"] for row in pair_rows
                ),
                "paired_delta_percent": {
                    "median": conventional_median(deltas),
                    "min": min(deltas),
                    "max": max(deltas),
                },
                "pairs": pair_rows,
            }
        )

    return {"schema_version": 1, "scenarios": scenarios}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("inputs", nargs="+", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    documents = [json.loads(path.read_text()) for path in args.inputs]
    summary = summarize_pairs(documents)
    rendered = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(rendered)
    else:
        print(rendered, end="")


if __name__ == "__main__":
    main()
