"""Check report contracts with small synthetic workloads, independent of raw archives."""
import json
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from summarize import distribution, summarize
from validate_retained import validate


class DistributionTest(unittest.TestCase):
    def test_p90_excludes_a_lone_outlier(self):
        self.assertEqual(distribution([100, 1, 10, 3, 9, 2, 8, 4, 7, 5, 6]),
                         {"n": 11, "median": 6, "p90": 10, "max": 100})


class ComparisonEligibilityTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write_workload(documents=1)

    def write_workload(self, documents, mode="native"):
        # Three cycles make medians explicit. Logged runs are deliberately much
        # slower, so including them in A/B statistics changes the expected result.
        matrix = {"status": "complete", "arguments": {
            "sizes": ["small"], "modes": [mode], "documents": [documents],
            "repeats": 1, "requests": 3, "warmup": 0,
            "inputs": "/profile", "output": "/results"},
            "binaries": {variant: {"path": "/bin/" + variant, "sha256": "synthetic-" + variant}
                         for variant in ("baseline", "candidate")},
            "inputs": {"runtime": "/runtime", "fixtures": [
                {"path": "rust-small.rs", "sha256": "rust-fixture"},
                {"path": "markdown-small.md", "sha256": "markdown-fixture"}]}, "runs": []}
        injections = {("rust", "wildcard"): ("comment",),
                      ("markdown", "narrow"): ("lua",),
                      ("markdown", "wildcard"): ("markdown_inline", "lua")}
        for language, extension in (("rust", "rs"), ("markdown", "md")):
            uris = [f"file:///profile/input-{index}.{extension}?kakehashi-profile-document={index}"
                    for index in range(documents)]
            opened = []
            for index, uri in enumerate(uris):
                if mode == "wildcard":
                    opened.append({"uri": uri, "language": language})
                for injected in injections.get((language, mode), ()):
                    opened.append({"uri": f"file:///profile/kakehashi-virtual-uri-region-{index}-{injected}.{injected}"
                                          f"?kakehashi-profile-document={index}",
                                   "language": injected})
            for variant, profile, seconds in (("baseline", False, [0.01, 0.02, 0.03]),
                                               ("candidate", False, [0.02, 0.04, 0.06]),
                                               ("candidate", True, [0.1, 0.2, 0.3])):
                name = f"{language}-small-{mode}-d{documents}-{variant}-0"
                if profile:
                    name += "-profile"
                matrix["runs"].append({"name": name, "language": language, "size": "small",
                                       "mode": mode, "documents": documents, "variant": variant,
                                       "repetition": 0, "profile": profile, "status": "complete",
                                       "command": ["python3", "drive.py", "--file",
                                           f"/profile/{language}-small.{extension}",
                                           "--json-output", f"/results/{name}.json"],
                                       "peers": [{"opened_documents": opened}] if opened else []})
                result = {"binary_sha256": "synthetic-" + variant,
                          "fixture_sha256": language + "-fixture",
                          "arguments": {"bin": "/bin/" + variant,
                              "file": f"/profile/{language}-small.{extension}",
                              "server_arg": ["--config-file", f"/profile/{mode}.toml"],
                              "data_dir": "/runtime", "requests": 3, "documents": documents,
                              "warmup": 0, "edits": 1, "edit_delay_ms": 0,
                              "tag_document_uris": True, "json_output": f"/results/{name}.json"},
                          "document_uris": uris, "cycle_seconds": seconds,
                          "request_samples": {"textDocument/semanticTokens/full": [
                              {"uri": uri, "status": "ok", "token_count": 7 + cycle,
                               "seconds": duration / 2, "wire_bytes": 1024}
                              for cycle, duration in enumerate(seconds) for uri in uris]},
                          "children_user_seconds": 1, "children_system_seconds": 2,
                          "children_maxrss_bytes": 8 * 1024**2, "server_warnings_and_errors": []}
                (self.root / (name + ".json")).write_text(json.dumps(result))
                if profile:
                    phases = [{"phase": "discovery", "elapsed_us": 100, "details": "completed=true"},
                              {"phase": "discovery", "elapsed_us": 300, "details": "completed=true"},
                              {"phase": "host_tokens", "elapsed_us": 500, "details": ""}]
                    (self.root / (name + ".phases.json")).write_text(json.dumps(phases))
        (self.root / "matrix.json").write_text(json.dumps(matrix))
        self.sample_file = self.root / f"rust-small-{mode}-d{documents}-baseline-0.json"

    def change_result(self, change):
        result = json.loads(self.sample_file.read_text())
        change(result)
        self.sample_file.write_text(json.dumps(result))

    def test_failed_final_verification_cannot_produce_a_comparison(self):
        path = self.root / "matrix.json"
        for status in ("running", "failed"):
            with self.subTest(status=status):
                matrix = json.loads(path.read_text())
                matrix["status"] = status
                path.write_text(json.dumps(matrix))
                with self.assertRaisesRegex(ValueError, "final verification"):
                    summarize(self.root)

    def test_unattested_legacy_matrix_is_rejected(self):
        path = self.root / "matrix.json"
        matrix = json.loads(path.read_text())
        matrix.pop("status")
        path.write_text(json.dumps(matrix))
        with self.assertRaisesRegex(ValueError, "final verification"):
            summarize(self.root)

    def duplicate_record(self):
        path = self.root / "matrix.json"
        matrix = json.loads(path.read_text())
        matrix["runs"][-1] = dict(matrix["runs"][0])
        path.write_text(json.dumps(matrix))

    def test_duplicate_cannot_replace_a_missing_scenario(self):
        self.duplicate_record()
        with self.assertRaisesRegex(ValueError, "scenario"):
            summarize(self.root)

    def test_artifact_names_cannot_alias_another_run(self):
        path = self.root / "matrix.json"
        matrix = json.loads(path.read_text())
        matrix["runs"][0]["name"] = matrix["runs"][1]["name"]
        path.write_text(json.dumps(matrix))
        with self.assertRaisesRegex(ValueError, "artifact name"):
            summarize(self.root)

    def test_retained_audit_also_rejects_duplicate_scenarios(self):
        self.duplicate_record()
        output = self.root / "audit.json"
        result = subprocess.run([
            sys.executable, str(Path(__file__).with_name("validate_retained.py")),
            str(self.root), str(output),
        ], capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("scenario", result.stderr)
        self.assertFalse(output.exists())

    def test_successes_produce_expected_comparison_and_audit(self):
        rows = summarize(self.root)
        self.assertEqual(len(rows), 2)
        row = rows[0]
        self.assertEqual(row["baseline"]["cycles_ms"], {"n": 3, "median": 20, "p90": 30, "max": 30})
        self.assertEqual(row["candidate"]["cycles_ms"], {"n": 3, "median": 40, "p90": 60, "max": 60})
        self.assertEqual(row["baseline"]["requests_ms"], {"n": 3, "median": 10, "p90": 15, "max": 15})
        self.assertEqual(row["pairs"], [{"repetition": 0, "baseline": 20, "candidate": 40,
                                        "delta_percent": 100}])
        self.assertEqual(row["baseline"]["session_cpu_seconds"]["median"], 3)
        self.assertEqual(row["baseline"]["maxrss_mib"]["median"], 8)
        self.assertEqual(row["phases_us"]["discovery"],
                         {"n": 2, "median": 200, "p90": 300, "max": 300, "incomplete": 0})
        self.assertEqual(row["phases_us"]["host_tokens"]["median"], 500)
        audit = validate(self.root)
        self.assertEqual(audit["statuses"], {"ok": 18})
        self.assertEqual(audit["minimum_token_count"], 7)

    def test_complete_multi_document_workloads_remain_comparable(self):
        self.write_workload(documents=4)
        rows = summarize(self.root)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["baseline"]["cycles_ms"]["n"], 3)
        self.assertEqual(rows[0]["baseline"]["requests_ms"]["n"], 12)
        self.assertEqual(validate(self.root)["statuses"], {"ok": 72})

    def test_truncated_cycles_cannot_produce_a_comparison(self):
        self.change_result(lambda result: result["cycle_seconds"].pop())
        with self.assertRaisesRegex(ValueError, "cycle count"):
            summarize(self.root)

    def test_truncated_responses_cannot_produce_a_comparison(self):
        self.change_result(lambda result: result["request_samples"]["textDocument/semanticTokens/full"].pop())
        with self.assertRaisesRegex(ValueError, "response count"):
            summarize(self.root)

    def test_document_inventory_cannot_duplicate_a_missing_uri(self):
        self.write_workload(documents=4)
        self.change_result(lambda result: result["document_uris"].__setitem__(1, result["document_uris"][0]))
        with self.assertRaisesRegex(ValueError, "document inventory"):
            summarize(self.root)

    def test_response_totals_cannot_mask_missing_document_work(self):
        self.write_workload(documents=4)

        def duplicate_uri(result):
            first, second = result["document_uris"][:2]
            for sample in result["request_samples"]["textDocument/semanticTokens/full"]:
                if sample["uri"] == second:
                    sample["uri"] = first

        self.change_result(duplicate_uri)
        with self.assertRaisesRegex(ValueError, "per-document response count"):
            summarize(self.root)

    def test_profile_workload_must_also_be_complete(self):
        self.sample_file = self.root / "rust-small-native-d1-candidate-0-profile.json"
        self.change_result(lambda result: result["cycle_seconds"].pop())
        with self.assertRaisesRegex(ValueError, "cycle count"):
            summarize(self.root)

    def test_retained_audit_also_rejects_truncated_workload(self):
        self.change_result(lambda result: result["cycle_seconds"].pop())
        output = self.root / "audit.json"
        result = subprocess.run([
            sys.executable, str(Path(__file__).with_name("validate_retained.py")),
            str(self.root), str(output),
        ], capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cycle count", result.stderr)
        self.assertFalse(output.exists())

    def test_cancellation_cannot_produce_a_comparison(self):
        self.change_result(lambda result: result["request_samples"]["textDocument/semanticTokens/full"][0].update(status="canceled"))
        with self.assertRaisesRegex(ValueError, "successful and nonempty"):
            summarize(self.root)

    def test_empty_success_cannot_produce_a_comparison(self):
        self.change_result(lambda result: result["request_samples"]["textDocument/semanticTokens/full"][0].update(token_count=0))
        with self.assertRaisesRegex(ValueError, "successful and nonempty"):
            summarize(self.root)

    def test_missing_explicit_count_cannot_use_response_size(self):
        self.change_result(lambda result: result["request_samples"]["textDocument/semanticTokens/full"][0].pop("token_count"))
        with self.assertRaisesRegex(ValueError, "successful and nonempty"):
            summarize(self.root)

    def test_profile_empty_response_is_rejected_by_both_consumers(self):
        self.sample_file = self.root / "rust-small-native-d1-candidate-0-profile.json"
        self.change_result(lambda result: result["request_samples"]["textDocument/semanticTokens/full"][0].update(token_count=0))
        for consumer in (summarize, validate):
            with self.subTest(consumer=consumer.__name__):
                with self.assertRaisesRegex(ValueError, "successful and nonempty"):
                    consumer(self.root)

    def test_per_host_bridge_evidence_remains_valid(self):
        for mode in ("narrow", "wildcard"):
            with self.subTest(mode=mode):
                self.write_workload(documents=4, mode=mode)
                self.assertEqual(len(summarize(self.root)), 2)
                self.assertEqual(len(validate(self.root)["runs"]), 6)

    def change_markdown_peer(self, mode, change, profile=False):
        self.write_workload(documents=4, mode=mode)
        path = self.root / "matrix.json"
        matrix = json.loads(path.read_text())
        run = next(run for run in matrix["runs"]
                   if run["language"] == "markdown" and run["profile"] == profile)
        change(run)
        path.write_text(json.dumps(matrix))

    def require_bridge_rejection(self):
        for consumer in (summarize, validate):
            with self.subTest(consumer=consumer.__name__):
                with self.assertRaisesRegex(ValueError, "bridge"):
                    consumer(self.root)

    def test_one_hosts_injections_cannot_cover_other_hosts(self):
        def collapse(run):
            for peer in run["peers"]:
                for document in peer["opened_documents"]:
                    if document["language"] == "lua":
                        document["uri"] = document["uri"].split("?", 1)[0] + "?kakehashi-profile-document=0"
        for mode in ("narrow", "wildcard"):
            with self.subTest(mode=mode):
                self.change_markdown_peer(mode, collapse)
                self.require_bridge_rejection()

    def test_wildcard_injections_do_not_replace_a_host_open(self):
        def remove_host(run):
            for peer in run["peers"]:
                peer["opened_documents"] = [document for document in peer["opened_documents"]
                                            if not (document["language"] == "markdown" and
                                                    document["uri"].endswith("kakehashi-profile-document=1"))]
        self.change_markdown_peer("wildcard", remove_host)
        self.require_bridge_rejection()

    def test_profile_run_requires_per_host_bridge_evidence(self):
        def remove_injections(run):
            for peer in run["peers"]:
                peer["opened_documents"] = [document for document in peer["opened_documents"]
                                            if document["language"] != "lua"]
        self.change_markdown_peer("narrow", remove_injections, profile=True)
        self.require_bridge_rejection()

    def test_explicit_token_counts_support_new_binaries(self):
        def change(result):
            result["binary_sha256"] = "another-server"
            for sample in result["request_samples"]["textDocument/semanticTokens/full"]:
                sample["token_count"] = 1
        self.change_result(change)
        matrix_path = self.root / "matrix.json"
        matrix = json.loads(matrix_path.read_text())
        matrix["binaries"]["baseline"]["sha256"] = "another-server"
        matrix_path.write_text(json.dumps(matrix))
        for run in matrix["runs"]:
            if run["variant"] == "baseline":
                path = self.root / (run["name"] + ".json")
                result = json.loads(path.read_text())
                result["binary_sha256"] = "another-server"
                path.write_text(json.dumps(result))
        self.assertEqual(len(summarize(self.root)), 2)

    def test_relative_matrix_cli_paths_keep_recorded_invocation_identity(self):
        path = self.root / "matrix.json"
        matrix = json.loads(path.read_text())
        matrix["arguments"].update(inputs="../profile", output="results")
        path.write_text(json.dumps(matrix))
        self.assertEqual(len(summarize(self.root)), 2)
        self.assertEqual(validate(self.root)["statuses"], {"ok": 18})

    def test_results_are_bound_to_the_declared_measurement(self):
        original = self.sample_file.read_text()
        mutations = [
            lambda r: r.update(binary_sha256="other-binary"),
            lambda r: r.update(fixture_sha256="other-fixture"),
            lambda r: r["arguments"].update(file="/profile/rust-large.rs"),
            lambda r: r["arguments"].update(server_arg=["--config-file", "/profile/wildcard.toml"]),
            lambda r: r["arguments"].update(warmup=99),
            lambda r: r["arguments"].update(json_output="/results/another-run.json"),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(mutation=index):
                self.sample_file.write_text(original)
                self.change_result(mutate)
                for consumer in (summarize, validate):
                    with self.assertRaisesRegex(ValueError, "identity"):
                        consumer(self.root)

    def test_renamed_results_cannot_replace_another_run(self):
        for target in ("markdown-small-native-d1-baseline-0",
                       "rust-small-native-d1-candidate-0-profile"):
            with self.subTest(target=target):
                self.write_workload(documents=1)
                (self.root / (target + ".json")).write_text(self.sample_file.read_text())
                for consumer in (summarize, validate):
                    with self.assertRaisesRegex(ValueError, "identity"):
                        consumer(self.root)


if __name__ == "__main__":
    unittest.main()
