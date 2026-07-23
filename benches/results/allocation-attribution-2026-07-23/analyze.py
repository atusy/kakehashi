#!/usr/bin/env python3
"""Verify retained allocation records and print deterministic summary stats."""

import argparse
import csv
import math
import re
import statistics
from pathlib import Path

FIELDS = (
    "sample",
    "allocations",
    "allocated_bytes",
    "deallocations",
    "deallocated_bytes",
)
METRICS = ("allocations", "allocated_bytes")
RECORD = re.compile(
    r"^\[SEMANTIC_TOKENS\] compute rust allocations: "
    r"allocations=(?P<allocations>\d+) "
    r"allocated_bytes=(?P<allocated_bytes>\d+) "
    r"deallocations=(?P<deallocations>\d+) "
    r"deallocated_bytes=(?P<deallocated_bytes>\d+) "
    r"scope=process_global_alloc_delta "
    r"consistency=non_atomic_snapshot$"
)


def read_rows(path: Path) -> list[dict[str, int]]:
    with path.open(newline="") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        if tuple(reader.fieldnames or ()) != FIELDS:
            raise SystemExit(f"{path}: unexpected columns: {reader.fieldnames}")
        return [{key: int(value) for key, value in row.items()} for row in reader]


def percentile(values: list[int], fraction: float) -> int:
    """Nearest-rank percentile: sorted_values[ceil(fraction * n) - 1]."""
    ordered = sorted(values)
    return ordered[math.ceil(fraction * len(ordered)) - 1]


def read_log(path: Path) -> list[dict[str, int]]:
    rows = []
    for line_number, line in enumerate(path.read_text().splitlines(), 1):
        match = RECORD.fullmatch(line)
        if match is None:
            raise SystemExit(f"{path}:{line_number}: unexpected allocation record")
        rows.append(
            {"sample": len(rows) + 1}
            | {key: int(value) for key, value in match.groupdict().items()}
        )
    return rows


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("raw_log", type=Path)
    parser.add_argument("all_records", type=Path)
    parser.add_argument("retained_records", type=Path)
    parser.add_argument("--warmups", type=int, default=6)
    args = parser.parse_args()

    raw_rows = read_log(args.raw_log)
    all_rows = read_rows(args.all_records)
    if all_rows != raw_rows:
        raise SystemExit(f"{args.all_records}: rows differ from {args.raw_log}")
    retained = read_rows(args.retained_records)
    expected = all_rows[args.warmups :]
    for sample, row in enumerate(expected, 1):
        row["sample"] = sample
    if retained != expected:
        raise SystemExit(
            f"{args.retained_records}: rows differ from {args.all_records} "
            f"after discarding {args.warmups} warmups"
        )

    for metric in METRICS:
        values = [row[metric] for row in retained]
        print(
            f"{metric}: n={len(values)} mean={statistics.mean(values):.1f} "
            f"p50={percentile(values, 0.50)} p90={percentile(values, 0.90)} "
            f"min={min(values)} max={max(values)}"
        )


if __name__ == "__main__":
    main()
