#!/usr/bin/env python3
"""Audit explicit token counts and per-host bridge evidence in the retained matrix."""
import argparse
from collections import Counter
import json
from pathlib import Path

from matrix_contract import bridge_coverage, load_matrix, require_identity, require_comparable, require_workload


def validate(root):
    matrix = load_matrix(root / "matrix.json")
    audit = {"nonempty_basis": "Explicit per-response token counts; not a latest-edit token oracle.",
             "bridge_basis": "Expected host/injection opens for each tagged host URI over the whole session, including warmup.",
             "runs": []}
    statuses, warnings = Counter(), Counter()
    minimum_tokens = None
    for run in matrix["runs"]:
        result = json.loads((root / (run["name"] + ".json")).read_text())
        require_identity(matrix, run, result)
        require_workload(run, result, matrix["arguments"]["requests"])
        require_comparable(run["name"], result)
        coverage = bridge_coverage(run, result)
        samples = result["request_samples"]["textDocument/semanticTokens/full"]
        uris = {}
        for uri in result["document_uris"]:
            selected = [sample for sample in samples if sample["uri"] == uri]
            uris[uri] = {"statuses": dict(Counter(sample["status"] for sample in selected)),
                         "minimum_token_count": min(sample["token_count"] for sample in selected)}
        statuses.update(sample["status"] for sample in samples)
        run_minimum = min(sample["token_count"] for sample in samples)
        minimum_tokens = run_minimum if minimum_tokens is None else min(minimum_tokens, run_minimum)
        warnings.update(item["message"] for item in result["server_warnings_and_errors"])
        if any(item["type"] == 1 for item in result["server_warnings_and_errors"]):
            raise ValueError(f"Server error in {run['name']}")
        audit["runs"].append({"name": run["name"], "uris": uris, "bridge_coverage": coverage})
    audit.update(statuses=dict(statuses), minimum_token_count=minimum_tokens, warnings=dict(warnings))
    return audit


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.write_text(json.dumps(validate(args.results), indent=2) + "\n", encoding="utf-8")
