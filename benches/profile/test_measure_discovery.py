"""Matrix admission regressions with fabricated driver artifacts, no processes."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit, urlunsplit

sys.path.insert(0, str(Path(__file__).parent))
import measure_discovery


@unittest.skipUnless(os.name == "posix", "matrix runner requires POSIX process groups")
class MatrixTokenWorkTest(unittest.TestCase):
    def run_matrix(self, cycles, mode="native", peer_languages=None, phase_log=None,
                   final_verification_error=None, changed_harness=None, peer_documents=None,
                   rewrite_inputs=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original_verify_inputs = measure_discovery.verify_inputs
            original_file_sha256 = measure_discovery.file_sha256
            if rewrite_inputs:
                inputs = root / "inputs"
                inputs.mkdir()
                fixture = inputs / "fixture.txt"
                fixture.write_text("original fixture")
                manifest = {"runtime": str(root), "fixtures": [
                    {"path": fixture.name, "sha256": original_file_sha256(fixture)}],
                    "configs": [], "assets": [], "scripts": {}}
                (inputs / "inputs.json").write_text(json.dumps(manifest))
            uris = [f"file:///profile/input-{index}.md?kakehashi-profile-document={index}"
                    for index in range(4)]
            samples = [
                {"uri": uri, "status": status, "token_count": count}
                for cycle in cycles
                for uri, (status, count) in zip(uris, cycle)
            ]

            def fake_driver(command, env, log, timeout):
                # Exercise the actual matrix main, its validators, and its
                # artifact status handling; only the external process is fake.
                if rewrite_inputs:
                    # Simulate regenerating an input and its self-consistent
                    # manifest after the initial provenance snapshot was recorded.
                    fixture.write_text("replacement fixture")
                    manifest["fixtures"][0]["sha256"] = original_file_sha256(fixture)
                    (inputs / "inputs.json").write_text(json.dumps(manifest))
                output = Path(command[command.index("--json-output") + 1])
                running_matrix = json.loads((output.parent / "matrix.json").read_text())
                self.assertEqual(running_matrix.get("status"), "running")
                if peer_languages is not None:
                    peer_dir = root / "inputs" / "peer-summaries"
                    peer_dir.mkdir(parents=True, exist_ok=True)
                    language = output.name.split("-", 1)[0]
                    languages = peer_languages(language)
                    opened_documents = (peer_documents(language, uris) if peer_documents else
                                        self.bridge_documents(language, uris, languages))
                    (peer_dir / output.name).write_text(json.dumps({
                        "opened_languages": languages,
                        "opened_documents": opened_documents,
                    }), encoding="utf-8")
                output.write_text(json.dumps({
                    "binary_sha256": "fixed-binary-hash",
                    "request_samples": {"textDocument/semanticTokens/full": samples},
                    "last_semantic_token_count": 3,
                    "document_uris": uris,
                    "server_exit_code": 0,
                }), encoding="utf-8")
                log.write("[drive] measurement-start\n")
                log.write(phase_log if phase_log is not None else
                          "phase=discovery elapsed_us=1 completed=true regions=1\n")
                log.write("[drive] measurement-end\n")

            arguments = [
                "measure_discovery.py", "--baseline", str(root / "baseline"),
                "--candidate", str(root / "candidate"),
                "--inputs", str(root / "inputs"), "--output", str(root / "results"),
                "--requests", str(len(cycles)), "--warmup", "0", "--repeats", "1",
                "--documents", "4", "--sizes", "small", "--modes", mode,
            ]
            verification_calls = 0

            def fake_verify_inputs(directory, *args):
                nonlocal verification_calls
                verification_calls += 1
                if verification_calls > 1 and final_verification_error is not None:
                    raise ValueError(final_verification_error)
                if rewrite_inputs:
                    return original_verify_inputs(directory, *args)
                return {"runtime": str(root)}

            def fake_file_sha256(path):
                if rewrite_inputs and Path(path).resolve() == fixture.resolve():
                    return original_file_sha256(path)
                if verification_calls > 1 and Path(path).name == changed_harness:
                    return "changed-harness-hash"
                return "fixed-binary-hash"

            with patch.object(sys, "argv", arguments), \
                    patch.object(measure_discovery, "verify_inputs", side_effect=fake_verify_inputs), \
                    patch.object(measure_discovery, "file_sha256", side_effect=fake_file_sha256), \
                    patch.object(measure_discovery, "run_driver", side_effect=fake_driver), \
                    patch("builtins.print"):
                try:
                    measure_discovery.main()
                finally:
                    self.last_matrix_record = json.loads(
                        (root / "results" / "matrix.json").read_text())
            record = json.loads((root / "results" / "matrix.json").read_text())
            self.assertEqual(len(record["runs"]), 6)
            self.assertEqual(record.get("status"), "complete")
            self.assertTrue(all(run["status"] == "complete" for run in record["runs"]))

    @staticmethod
    def bridge_documents(host_language, uris, languages):
        opened = []
        for index, uri in enumerate(uris):
            for language in languages:
                parts = urlsplit(uri)
                path = f"/profile/kakehashi-virtual-uri-region-{index}-{language}.{language}"
                opened_uri = uri if language == host_language else urlunsplit(parts._replace(path=path))
                opened.append({"uri": opened_uri, "language": language})
        return opened

    def test_one_documents_many_regions_cannot_cover_other_documents(self):
        for mode in ("narrow", "wildcard"):
            with self.subTest(mode=mode):
                def languages(language):
                    if mode == "narrow":
                        return {"lua": 4} if language == "markdown" else {}
                    return ({"rust": 4, "comment": 4} if language == "rust" else
                            {"markdown": 4, "markdown_inline": 4, "lua": 4})

                def opened(language, uris):
                    values = self.bridge_documents(language, uris, languages(language))
                    if language == "markdown":
                        for index, item in enumerate(values):
                            if item["language"] == "lua":
                                parts = urlsplit(item["uri"])
                                item["uri"] = urlunsplit(parts._replace(
                                    query="kakehashi-profile-document=0",
                                    path=f"/profile/kakehashi-virtual-uri-extra-{index}.lua"))
                    return values

                with self.assertRaisesRegex(ValueError, "bridge"):
                    self.run_matrix([[("ok", 3)] * 4], mode=mode,
                                    peer_languages=languages, peer_documents=opened)

    def test_virtual_rust_opens_cannot_substitute_for_wildcard_host_opens(self):
        def languages(language):
            return ({"rust": 4, "comment": 4} if language == "rust" else
                    {"markdown": 4, "markdown_inline": 4, "lua": 4})

        def opened(language, uris):
            values = self.bridge_documents(language, uris, languages(language))
            for index, item in enumerate(values):
                if item["language"] == language:
                    parts = urlsplit(item["uri"])
                    item["uri"] = urlunsplit(parts._replace(
                        path=f"/profile/kakehashi-virtual-uri-host-substitute-{index}.{language}"))
            return values

        with self.assertRaisesRegex(ValueError, "bridge host open"):
            self.run_matrix([[("ok", 3)] * 4], mode="wildcard",
                            peer_languages=languages, peer_documents=opened)

    def test_replaced_input_and_manifest_cannot_redefine_final_verification(self):
        with self.assertRaisesRegex(ValueError, "input manifest changed"):
            self.run_matrix([[("ok", 3)] * 4], rewrite_inputs=True)
        self.assertEqual(len(self.last_matrix_record["runs"]), 6)
        self.assertTrue(all(run["status"] == "complete" for run in self.last_matrix_record["runs"]))
        self.assertEqual(self.last_matrix_record["status"], "failed")
        self.assertIn("input manifest changed", self.last_matrix_record["error"])

    def test_records_final_verification_failure_after_successful_runs(self):
        with self.assertRaisesRegex(ValueError, "runtime changed during measurement"):
            self.run_matrix([[("ok", 3)] * 4],
                            final_verification_error="runtime changed during measurement")
        self.assertTrue(all(run["status"] == "complete" for run in self.last_matrix_record["runs"]))
        self.assertEqual(self.last_matrix_record.get("status"), "failed")
        self.assertEqual(self.last_matrix_record.get("error"), "runtime changed during measurement")

    def test_records_changed_measurement_harness_after_successful_runs(self):
        for name in ("drive.py", "measure_discovery.py"):
            with self.subTest(name=name):
                with self.assertRaisesRegex(ValueError, "measurement harness changed"):
                    self.run_matrix([[("ok", 3)] * 4], changed_harness=name)
                self.assertTrue(all(run["status"] == "complete" for run in self.last_matrix_record["runs"]))
                self.assertEqual(self.last_matrix_record.get("status"), "failed")
                self.assertIn(name, self.last_matrix_record.get("error", ""))

    def test_rejects_documents_with_only_empty_successful_responses(self):
        # The old validator accepts this: all URIs have status=ok, and
        # the last document supplies a nonzero global final token count.
        cycle = [("ok", 0), ("ok", 0), ("ok", 0), ("ok", 3)]
        with self.assertRaisesRegex(ValueError, "token work"):
            self.run_matrix([cycle, cycle])

    def test_rejects_legacy_token_timings_without_discovery_attribution(self):
        with self.assertRaisesRegex(ValueError, "discovery"):
            self.run_matrix(
                [[("ok", 3)] * 4],
                phase_log="compute phases: compute=8us host=3us injections=4us finalize=1us uri=file:///profile/input.md\n",
            )
        self.assertEqual(self.last_matrix_record.get("status"), "failed")
        self.assertIn("discovery", self.last_matrix_record.get("error", ""))
        self.assertEqual(self.last_matrix_record["runs"][-1]["status"], "failed")

    def test_rejects_incomplete_or_outside_window_discovery(self):
        for phase_log in (
            "phase=discovery elapsed_us=1 completed=false regions=0\n",
            "phase=parse elapsed_us=1 completed=true\n",
            "[drive] measurement-end\nphase=discovery elapsed_us=1 completed=true regions=1\n",
        ):
            with self.subTest(phase_log=phase_log), self.assertRaisesRegex(ValueError, "discovery"):
                self.run_matrix([[("ok", 3)] * 4], phase_log=phase_log)

    def test_accepts_cancellation_when_each_document_eventually_has_tokens(self):
        self.run_matrix([
            [("canceled", None), ("null", None), ("ok", 0), ("ok", 3)],
            [("ok", 3), ("ok", 3), ("ok", 3), ("ok", 3)],
        ])

    def test_rejects_wildcard_host_opens_without_injections(self):
        cycle = [("ok", 3)] * 4
        with self.assertRaisesRegex(ValueError, "bridge"):
            self.run_matrix([cycle], mode="wildcard",
                            peer_languages=lambda language: {language: 4})

    def test_rejects_markdown_wildcard_opens_without_lua(self):
        cycle = [("ok", 3)] * 4
        with self.assertRaisesRegex(ValueError, "bridge"):
            self.run_matrix(
                [cycle], mode="wildcard",
                peer_languages=lambda language: (
                    {"rust": 4, "comment": 4} if language == "rust" else
                    {"markdown": 4, "markdown_inline": 4}
                ),
            )

    def test_accepts_wildcard_host_and_injection_opens(self):
        self.run_matrix(
            [[("ok", 3)] * 4], mode="wildcard",
            peer_languages=lambda language: (
                {"rust": 4, "comment": 4} if language == "rust" else
                {"markdown": 4, "markdown_inline": 4, "lua": 4}
            ),
        )

    def test_rejects_duplicate_sizes_before_inputs_or_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "results"
            arguments = [
                "measure_discovery.py", "--baseline", "unused-baseline",
                "--candidate", "unused-candidate", "--inputs", "unused-inputs",
                "--output", str(output), "--sizes", "small", "small",
            ]
            # The sentinel proves parser validation happens before filesystem
            # inspection. Old code exits with 88 at verify_inputs, not 2.
            with patch.object(sys, "argv", arguments), \
                    patch.object(measure_discovery, "verify_inputs", side_effect=SystemExit(88)) as verify, \
                    patch("sys.stderr"):
                with self.assertRaises(SystemExit) as caught:
                    measure_discovery.main()
            self.assertEqual(caught.exception.code, 2)
            verify.assert_not_called()
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
