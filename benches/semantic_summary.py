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


def validate_collection(
    documents: Iterable[dict[str, Any]],
    *,
    pair_count: int,
    scenarios: set[str],
    iterations: int,
    warmup: int,
    binary_sha256: dict[str, str],
    harness_commit: str,
    harness_sha256: str,
    fixture_sha256: str,
) -> list[dict[str, Any]]:
    documents = list(documents)
    if len(documents) != pair_count:
        raise ValueError(f"expected {pair_count} pair documents, got {len(documents)}")

    by_pair: dict[int, dict[str, Any]] = {}
    document_bytes_by_scenario: dict[str, int] = {}
    for document in documents:
        if document.get("schema_version") != 3:
            raise ValueError(f"unexpected raw schema: {document.get('schema_version')}")
        pair_index = int(document["pair_index"])
        if pair_index in by_pair:
            raise ValueError(f"duplicate pair document: {pair_index}")
        by_pair[pair_index] = document

    expected_indexes = set(range(1, pair_count + 1))
    if set(by_pair) != expected_indexes:
        raise ValueError(
            f"pair indexes do not match: expected {sorted(expected_indexes)}, got {sorted(by_pair)}"
        )

    for pair_index, document in sorted(by_pair.items()):
        expected_order = "AB" if pair_index % 2 else "BA"
        if document.get("order") != expected_order:
            raise ValueError(
                f"pair {pair_index} order must be {expected_order}, got {document.get('order')}"
            )
        if document.get("iterations") != iterations:
            raise ValueError(f"pair {pair_index} iteration count does not match")
        if document.get("warmup_iterations") != warmup:
            raise ValueError(f"pair {pair_index} warmup count does not match")
        if document.get("harness_commit") != harness_commit:
            raise ValueError(f"pair {pair_index} harness commit does not match")
        if document.get("harness_sha256") != harness_sha256:
            raise ValueError(f"pair {pair_index} harness hash does not match")
        if document.get("fixture_sha256") != fixture_sha256:
            raise ValueError(f"pair {pair_index} fixture hash does not match")

        binaries = document.get("binaries", [])
        if [binary.get("label") for binary in binaries] != list(expected_order):
            raise ValueError(f"pair {pair_index} binary order does not match")
        if {
            str(binary["label"]): str(binary["sha256"]) for binary in binaries
        } != binary_sha256:
            raise ValueError(f"pair {pair_index} binary hashes do not match")

        runs_by_scenario: dict[str, list[dict[str, Any]]] = defaultdict(list)
        for run in document.get("runs", []):
            runs_by_scenario[str(run["scenario"])].append(run)
        if set(runs_by_scenario) != scenarios:
            raise ValueError(
                f"pair {pair_index} scenarios do not match: "
                f"expected {sorted(scenarios)}, got {sorted(runs_by_scenario)}"
            )
        for scenario, runs in runs_by_scenario.items():
            if [str(run["binary_label"]) for run in runs] != list(expected_order):
                raise ValueError(
                    f"pair {pair_index}, scenario {scenario} run order does not match"
                )
            for run in runs:
                label = str(run["binary_label"])
                document_bytes = run.get("document_bytes")
                if not isinstance(document_bytes, int) or document_bytes < 1:
                    raise ValueError(
                        f"pair {pair_index}, scenario {scenario}, {label} has invalid document size"
                    )
                previous_bytes = document_bytes_by_scenario.setdefault(
                    scenario, document_bytes
                )
                if document_bytes != previous_bytes:
                    raise ValueError(
                        f"pair {pair_index}, scenario {scenario}, {label} document size changed"
                    )
                if run.get("binary_sha256") != binary_sha256[label]:
                    raise ValueError(
                        f"pair {pair_index}, scenario {scenario}, {label} hash does not match"
                    )
                if len(run.get("samples_ns", [])) != iterations:
                    raise ValueError(
                        f"pair {pair_index}, scenario {scenario}, {label} sample count does not match"
                    )
                validation = run.get("validation", {})
                if validation.get("retained_samples") != iterations:
                    raise ValueError(
                        f"pair {pair_index}, scenario {scenario}, {label} retained count does not match"
                    )
                if validation.get("discarded_attempts") != 0:
                    raise ValueError(
                        f"pair {pair_index}, scenario {scenario}, {label} has discarded attempts"
                    )
    return [by_pair[index] for index in sorted(by_pair)]


def summarize_pairs(documents: Iterable[dict[str, Any]]) -> dict[str, Any]:
    documents = list(documents)
    paired: dict[str, dict[int, dict[str, float]]] = defaultdict(
        lambda: defaultdict(dict)
    )
    orders: dict[int, str] = {}

    for document in documents:
        pair_index = int(document["pair_index"])
        if pair_index in orders:
            raise ValueError(f"duplicate pair document: {pair_index}")
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
    expected_pairs = set(orders)
    for scenario, pairs in sorted(paired.items()):
        if set(pairs) != expected_pairs:
            raise ValueError(
                f"scenario {scenario} has pairs {sorted(pairs)}, expected {sorted(expected_pairs)}"
            )
        pair_rows = []
        for pair_index, values in sorted(pairs.items()):
            if set(values) != {"A", "B"}:
                raise ValueError(
                    f"incomplete pair {pair_index} for {scenario}: {sorted(values)}"
                )
            a_ns = values["A"]
            b_ns = values["B"]
            if a_ns <= 0 or b_ns <= 0:
                raise ValueError(
                    f"non-positive median for pair {pair_index}, {scenario}: "
                    f"A={a_ns}, B={b_ns}"
                )
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
