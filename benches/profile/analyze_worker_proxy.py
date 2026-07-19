#!/usr/bin/env python3
"""Recompute paired Phase 0 relay summaries from committed run data."""

import argparse
import json
import math
import pathlib
import random
import statistics


def mean(values):
    return sum(values) / len(values)


def nearest_rank(values, quantile):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(quantile * len(ordered)) - 1)]


def summarize_pairs(pairs, metric, seed=123_456_789, resamples=100_000):
    if resamples <= 0:
        raise ValueError("resamples must be positive")
    direct = [pair["direct"][metric] for pair in pairs]
    relay = [pair["relay"][metric] for pair in pairs]
    deltas = [right - left for left, right in zip(direct, relay)]
    rng = random.Random(seed)
    bootstrapped_means = [
        mean(rng.choices(deltas, k=len(deltas)))
        for _ in range(resamples)
    ]
    direct_mean = mean(direct)
    relay_mean = mean(relay)
    mean_delta = mean(deltas)
    return {
        "direct_mean": direct_mean,
        "relay_mean": relay_mean,
        "mean_delta": mean_delta,
        "percent_delta": mean_delta / direct_mean * 100 if direct_mean else 0,
        "ci95": [
            nearest_rank(bootstrapped_means, 0.025),
            nearest_rank(bootstrapped_means, 0.975),
        ],
    }


def summarize_cold_start(times):
    return {
        "mean_ms": mean(times) * 1_000,
        "stddev_ms": statistics.stdev(times) * 1_000 if len(times) > 1 else 0.0,
        "min_ms": min(times) * 1_000,
        "max_ms": max(times) * 1_000,
    }


def select_pairs(pairs, drop_first_pairs=0, last_pairs=None):
    if drop_first_pairs and last_pairs is not None:
        raise ValueError("pair sensitivity slices are mutually exclusive")
    selected = pairs[-last_pairs:] if last_pairs is not None else pairs[drop_first_pairs:]
    if not selected:
        raise ValueError("pair sensitivity slice is empty")
    return selected


def analyze_data(
    data,
    seed=123_456_789,
    resamples=100_000,
    drop_first_pairs=0,
    last_pairs=None,
):
    if not isinstance(data, dict):
        raise ValueError("input data must be a JSON object")
    if data.get("experiment") == "single-tree-worker-phase0-cold-start":
        return {
            "cold_start": summarize_pairs(
                select_pairs(data["pairs"], drop_first_pairs, last_pairs),
                "elapsed_seconds", seed, resamples,
            )
        }
    summaries = {}
    for scenario, details in data["steady_state"].items():
        summaries[scenario] = {
            metric: summarize_pairs(
                select_pairs(details["pairs"], drop_first_pairs, last_pairs),
                metric, seed, resamples,
            )
            for metric in ("p50", "p95", "p99", "wall")
        }
    if data.get("cold_start"):
        summaries["cold_start"] = {
            path: summarize_cold_start(details["times"])
            for path, details in data["cold_start"].items()
            if path in ("direct", "relay")
        }
    return summaries


def main():
    default_data = pathlib.Path(__file__).with_name("results") / (
        "single_worker_phase0_2026-07-19.json"
    )
    parser = argparse.ArgumentParser()
    parser.add_argument("data", nargs="?", type=pathlib.Path, default=default_data)
    parser.add_argument("--seed", type=int, default=123_456_789)
    parser.add_argument("--resamples", type=int, default=100_000)
    sensitivity = parser.add_mutually_exclusive_group()
    sensitivity.add_argument("--drop-first-pairs", type=int, default=0)
    sensitivity.add_argument("--last-pairs", type=int)
    args = parser.parse_args()
    if args.drop_first_pairs < 0 or (
        args.last_pairs is not None and args.last_pairs <= 0
    ):
        parser.error("pair slice counts must be positive (or zero for drop-first)")
    if args.resamples <= 0:
        parser.error("resamples must be positive")

    data = json.loads(args.data.read_text())
    summaries = analyze_data(
        data,
        args.seed,
        args.resamples,
        args.drop_first_pairs,
        args.last_pairs,
    )
    print(json.dumps(summaries, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
