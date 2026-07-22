import tempfile
import unittest
from pathlib import Path
from unittest import mock

from collect_semantic_pairs import (
    DEFAULT_SCENARIOS,
    manifest_sha256,
    normalize_captured_stdout,
    parse_server_env,
    redact_temporary_paths,
    recorded_environment,
    set_tree_read_only,
    set_tree_writable,
    tree_manifest,
    validate_requested_scenario_filters,
)


class CollectSemanticPairsTest(unittest.TestCase):
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
