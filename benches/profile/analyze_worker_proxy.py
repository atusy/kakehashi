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
        "stddev_ms": statistics.stdev(times) * 1_000,
        "min_ms": min(times) * 1_000,
        "max_ms": max(times) * 1_000,
    }


def main():
    default_data = pathlib.Path(__file__).with_name("results") / (
        "single_worker_phase0_2026-07-19.json"
    )
    parser = argparse.ArgumentParser()
    parser.add_argument("data", nargs="?", type=pathlib.Path, default=default_data)
    parser.add_argument("--seed", type=int, default=123_456_789)
    parser.add_argument("--resamples", type=int, default=100_000)
    args = parser.parse_args()

    data = json.loads(args.data.read_text())
    summaries = {}
    for scenario, details in data["steady_state"].items():
        summaries[scenario] = {
            metric: summarize_pairs(
                details["pairs"], metric, args.seed, args.resamples
            )
            for metric in ("p50", "p95", "p99", "wall")
        }
    summaries["cold_start"] = {
        path: summarize_cold_start(details["times"])
        for path, details in data["cold_start"].items()
        if path in ("direct", "relay")
    }
    print(json.dumps(summaries, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
