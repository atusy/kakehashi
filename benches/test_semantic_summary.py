import unittest

from semantic_summary import conventional_median, summarize_pairs


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


if __name__ == "__main__":
    unittest.main()
