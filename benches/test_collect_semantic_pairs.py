import tempfile
import unittest
from pathlib import Path
from unittest import mock

from collect_semantic_pairs import (
    DEFAULT_SCENARIOS,
    manifest_sha256,
    normalize_captured_stdout,
    parse_server_env,
    require_semantic_bench_instrumentation,
    requires_semantic_bench_instrumentation,
    redact_temporary_paths,
    recorded_environment,
    scenario_filter_terms,
    set_tree_read_only,
    set_tree_writable,
    tree_manifest,
    validate_requested_scenario_filters,
    write_installed_data_manifest,
)


class CollectSemanticPairsTest(unittest.TestCase):
    def test_only_fanout_scenario_requires_benchmark_instrumentation(self):
        self.assertTrue(
            requires_semantic_bench_instrumentation(
                "rust_xlarge/same_snapshot_fanout"
            )
        )
        self.assertTrue(requires_semantic_bench_instrumentation("fanout"))
        self.assertFalse(
            requires_semantic_bench_instrumentation("rust_large/typing_delta")
        )

    def test_fanout_instrumentation_requires_feature_on_measured_ref(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            (source / "Cargo.toml").write_text("[features]\ne2e = []\n")
            with self.assertRaisesRegex(
                RuntimeError, "predates semantic benchmark instrumentation"
            ):
                require_semantic_bench_instrumentation(source)
            (source / "Cargo.toml").write_text(
                "[features]\nsemantic-bench-instrumentation = []\n"
            )
            require_semantic_bench_instrumentation(source)

    def test_scenario_filters_are_normalized_for_execution_and_manifest(self):
        self.assertEqual(
            scenario_filter_terms(" rust, , markdown "),
            ["rust", "markdown"],
        )

    def test_python_cache_cannot_dirty_collector_checkout(self):
        ignore_patterns = (
            (Path(__file__).parent.parent / ".gitignore").read_text().splitlines()
        )
        self.assertIn("__pycache__/", ignore_patterns)

    def test_every_requested_scenario_filter_must_match(self):
        measured = {
            "rust_large/full_cache_hit",
            "markdown_injections/typing_delta",
        }
        validate_requested_scenario_filters(
            "rust_large/full_cache_hit,markdown_injections/typing_delta",
            measured,
        )
        with self.assertRaisesRegex(ValueError, "typo"):
            validate_requested_scenario_filters(
                "rust_large/full_cache_hit,typo",
                measured,
            )

    def test_server_environment_cannot_override_isolation(self):
        for key in [
            "PATH",
            "HOME",
            "TMPDIR",
            "CARGO_HOME",
            "CARGO_TERM_COLOR",
            "LANG",
            "LC_ALL",
            "RUSTUP_HOME",
        ]:
            with self.subTest(key=key):
                with self.assertRaisesRegex(ValueError, "reserved collector key"):
                    parse_server_env([f"{key}=override"])

        self.assertEqual(
            parse_server_env(["KAKEHASHI_TREE_WORKER_MODE=legacy"]),
            {"KAKEHASHI_TREE_WORKER_MODE": "legacy"},
        )

    def test_server_environment_rejects_unpublished_controls(self):
        with self.assertRaisesRegex(ValueError, "unsupported server env key"):
            parse_server_env(["API_TOKEN=secret"])

    def test_default_scenarios_exist_in_rust_harness(self):
        harness = (Path(__file__).parent / "semantic_tokens.rs").read_text()
        for scenario in DEFAULT_SCENARIOS.split(","):
            with self.subTest(scenario=scenario):
                self.assertIn(f'name: "{scenario}"', harness)

    def test_captured_stdout_has_exactly_one_terminal_newline(self):
        self.assertEqual(normalize_captured_stdout("result\n\n"), "result\n")
        self.assertEqual(normalize_captured_stdout("result"), "result\n")
        self.assertEqual(normalize_captured_stdout("result  \t\n"), "result  \t\n")
        self.assertEqual(normalize_captured_stdout(""), "")

    def test_recorded_environment_hides_user_specific_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            temp = Path(temporary)
            environment = {
                "HOME": str(temp / "home"),
                "PATH": "/Users/example/.local/bin:/usr/bin:/Users/example/.cache/tool/bin",
                "RUSTUP_HOME": "/Users/example/.rustup",
            }
            with mock.patch.dict("os.environ", {"HOME": "/Users/example"}):
                recorded = recorded_environment(environment, temp)

            self.assertEqual(recorded["HOME"], "<TEMP>/home")
            self.assertEqual(
                recorded["PATH"],
                "<USER_HOME_PATH>:/usr/bin:<USER_HOME_PATH>",
            )
            self.assertEqual(recorded["RUSTUP_HOME"], "<USER_HOME>/.rustup")
            self.assertNotIn("example", str(recorded))

    def test_redacts_temporary_paths_recursively(self):
        with tempfile.TemporaryDirectory() as temporary:
            temp = Path(temporary)
            value = {
                "binaries": [{"path": str(temp / "target-a" / "kakehashi")}],
                "nested": ["unchanged", str(temp / "fixture")],
            }

            self.assertEqual(
                redact_temporary_paths(value, temp),
                {
                    "binaries": [{"path": "<TEMP>/target-a/kakehashi"}],
                    "nested": ["unchanged", "<TEMP>/fixture"],
                },
            )

    def test_tree_attestation_changes_with_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root / "fixture"
            fixture.mkdir()
            file = fixture / "parser.bin"
            file.write_bytes(b"before")
            before = manifest_sha256(tree_manifest(fixture))
            file.write_bytes(b"after")
            self.assertNotEqual(before, manifest_sha256(tree_manifest(fixture)))

    def test_installed_data_manifest_does_not_publish_parser_or_query_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifact_dir = Path(temporary)
            metadata = write_installed_data_manifest(
                artifact_dir,
                [{"path": "parser/rust.dylib", "sha256": "parser-hash"}],
            )

            self.assertEqual(metadata["fixture_manifest"], "fixture-manifest.json")
            self.assertNotIn("fixture_archive", metadata)
            self.assertEqual(
                list(artifact_dir.iterdir()),
                [artifact_dir / "fixture-manifest.json"],
            )

    def test_fixture_can_be_frozen_and_restored_for_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary) / "fixture"
            nested = fixture / "queries" / "rust"
            nested.mkdir(parents=True)
            file = nested / "highlights.scm"
            file.write_text("(identifier) @variable")

            set_tree_read_only(fixture)
            self.assertEqual(fixture.stat().st_mode & 0o777, 0o555)
            self.assertEqual(file.stat().st_mode & 0o777, 0o444)

            set_tree_writable(fixture)
            self.assertEqual(fixture.stat().st_mode & 0o777, 0o755)
            self.assertEqual(file.stat().st_mode & 0o777, 0o644)


if __name__ == "__main__":
    unittest.main()
