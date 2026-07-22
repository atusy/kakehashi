import unittest

from semantic_summary import conventional_median, summarize_pairs, validate_collection


def pair_document(pair_index, order, a_samples, b_samples):
    runs = {
        "A": {
            "binary_label": "A",
            "scenario": "rust/typing_delta",
            "samples_ns": a_samples,
        },
        "B": {
            "binary_label": "B",
            "scenario": "rust/typing_delta",
            "samples_ns": b_samples,
        },
    }
    return {
        "pair_index": str(pair_index),
        "order": order,
        "runs": [runs[label] for label in order],
    }


def attested_document(pair_index, order="AB"):
    document = pair_document(pair_index, order, [90, 110], [72, 88])
    document.update(
        {
            "schema_version": 3,
            "iterations": 2,
            "warmup_iterations": 1,
            "harness_commit": "harness-commit",
            "harness_sha256": "harness-hash",
            "fixture_sha256": "fixture-hash",
            "binaries": [
                {"label": label, "sha256": f"{label}-hash"} for label in order
            ],
        }
    )
    for run in document["runs"]:
        label = run["binary_label"]
        run["binary_sha256"] = f"{label}-hash"
        run["validation"] = {"retained_samples": 2, "discarded_attempts": 0}
    return document


class SemanticSummaryTest(unittest.TestCase):
    def test_even_median_uses_both_middle_samples(self):
        self.assertEqual(conventional_median([1, 2, 100, 200]), 51.0)

    def test_summarizes_canonical_labels_across_ab_and_ba_order(self):
        summary = summarize_pairs(
            [
                pair_document(1, "AB", [90, 110], [72, 88]),
                pair_document(2, "BA", [100, 100], [90, 90]),
            ]
        )

        scenario = summary["scenarios"][0]
        self.assertEqual(scenario["pair_count"], 2)
        self.assertEqual(scenario["a_median_ns"], 100.0)
        self.assertEqual(scenario["b_median_ns"], 85.0)
        self.assertEqual(scenario["paired_delta_percent"]["median"], -15.0)
        self.assertEqual(scenario["paired_delta_percent"]["min"], -20.0)
        self.assertEqual(scenario["paired_delta_percent"]["max"], -10.0)
        self.assertEqual([row["order"] for row in scenario["pairs"]], ["AB", "BA"])

    def test_rejects_incomplete_pairs(self):
        document = pair_document(1, "AB", [1], [2])
        document["runs"].pop()
        with self.assertRaisesRegex(ValueError, "incomplete pair"):
            summarize_pairs([document])

    def test_validates_complete_attested_collection(self):
        documents = [attested_document(1), attested_document(2, "BA")]
        validated = validate_collection(
            documents,
            pair_count=2,
            scenarios={"rust/typing_delta"},
            iterations=2,
            warmup=1,
            binary_sha256={"A": "A-hash", "B": "B-hash"},
            harness_commit="harness-commit",
            harness_sha256="harness-hash",
            fixture_sha256="fixture-hash",
        )
        self.assertEqual([int(document["pair_index"]) for document in validated], [1, 2])

    def test_rejects_duplicate_pair_documents(self):
        document = attested_document(1)
        with self.assertRaisesRegex(ValueError, "duplicate pair document"):
            validate_collection(
                [document, document],
                pair_count=2,
                scenarios={"rust/typing_delta"},
                iterations=2,
                warmup=1,
                binary_sha256={"A": "A-hash", "B": "B-hash"},
                harness_commit="harness-commit",
                harness_sha256="harness-hash",
                fixture_sha256="fixture-hash",
            )

    def test_rejects_discarded_cancellation_attempts(self):
        documents = [attested_document(1), attested_document(2, "BA")]
        documents[1]["runs"][0]["validation"]["discarded_attempts"] = 1
        with self.assertRaisesRegex(ValueError, "discarded attempts"):
            validate_collection(
                documents,
                pair_count=2,
                scenarios={"rust/typing_delta"},
                iterations=2,
                warmup=1,
                binary_sha256={"A": "A-hash", "B": "B-hash"},
                harness_commit="harness-commit",
                harness_sha256="harness-hash",
                fixture_sha256="fixture-hash",
            )


if __name__ == "__main__":
    unittest.main()
