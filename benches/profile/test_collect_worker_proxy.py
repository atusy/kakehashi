import hashlib
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from collect_worker_proxy import (
    build_driver_command,
    bounded_run,
    controlled_environment,
    estimated_tree_compute_budget,
    artifact_provenance,
    parse_capture_pilot_summary,
    parse_driver_summary,
    load_binary_attestation,
    parser_library_suffix,
    require_posix as require_collector_posix,
    require_benchmark_artifacts,
    run_order,
    shasum_tree_digest,
    verify_file_sha256,
)


class CollectionHelpersTest(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix", "requires POSIX signal masks")
    def test_bounded_run_unblocks_termination_signals_in_child(self):
        command = [
            sys.executable,
            "-c",
            "import signal; blocked=signal.pthread_sigmask(signal.SIG_BLOCK, []); "
            "print(signal.SIGTERM in blocked, signal.SIGHUP in blocked)",
        ]

        completed = bounded_run(command, {}, timeout_seconds=5)

        self.assertEqual(completed.stdout, "False False\n")

    def test_parser_suffix_matches_supported_posix_platform(self):
        self.assertEqual(parser_library_suffix("Darwin"), ".dylib")
        self.assertEqual(parser_library_suffix("Linux"), ".so")

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups")
    def test_bounded_run_reaps_term_ignoring_descendant(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "child.pid"
            child_script = (
                "import os,signal,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                f"open({str(pid_file)!r}, 'w').write(str(os.getpid())); "
                "time.sleep(30)"
            )
            script = (
                "import subprocess,sys,time; "
                f"subprocess.Popen([sys.executable, '-c', {child_script!r}]); "
                "time.sleep(30)"
            )

            with self.assertRaises(subprocess.TimeoutExpired):
                bounded_run(
                    [sys.executable, "-c", script],
                    {},
                    timeout_seconds=0.2,
                    termination_grace_seconds=0.2,
                )
            child_pid = int(pid_file.read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups")
    def test_bounded_run_reaps_group_when_collector_is_interrupted(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "child.pid"
            descendant = (
                "import os,signal,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                f"open({str(pid_file)!r}, 'w').write(str(os.getpid())); "
                "time.sleep(30)"
            )
            driver = (
                "import os,signal,subprocess,sys,time; "
                f"subprocess.Popen([sys.executable, '-c', {descendant!r}]); "
                "time.sleep(0.1); os.kill(os.getppid(), signal.SIGINT); "
                "time.sleep(30)"
            )
            child_pid = None
            try:
                with self.assertRaises(KeyboardInterrupt):
                    bounded_run(
                        [sys.executable, "-c", driver],
                        {},
                        timeout_seconds=5,
                        termination_grace_seconds=0.2,
                    )
                child_pid = int(pid_file.read_text())
                with self.assertRaises(ProcessLookupError):
                    os.kill(child_pid, 0)
            finally:
                if child_pid is None and pid_file.exists():
                    child_pid = int(pid_file.read_text())
                if child_pid is not None:
                    try:
                        os.kill(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups")
    def test_bounded_run_reaps_group_on_external_termination_signal(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "child.pid"
            descendant = (
                "import os,signal,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                f"open({str(pid_file)!r}, 'w').write(str(os.getpid())); "
                "time.sleep(30)"
            )
            driver = (
                "import os,signal,subprocess,sys,time; "
                f"subprocess.Popen([sys.executable, '-c', {descendant!r}]); "
                "time.sleep(0.1); os.kill(os.getppid(), signal.SIGTERM); "
                "time.sleep(0.05); os.kill(os.getppid(), signal.SIGTERM); "
                "time.sleep(30)"
            )
            child_pid = None
            try:
                with self.assertRaises(SystemExit) as exit_context:
                    bounded_run(
                        [sys.executable, "-c", driver],
                        {},
                        timeout_seconds=5,
                        termination_grace_seconds=0.2,
                    )
                self.assertEqual(exit_context.exception.code, 128 + signal.SIGTERM)
                child_pid = int(pid_file.read_text())
                with self.assertRaises(ProcessLookupError):
                    os.kill(child_pid, 0)
            finally:
                if child_pid is None and pid_file.exists():
                    child_pid = int(pid_file.read_text())
                if child_pid is not None:
                    try:
                        os.kill(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    @unittest.skipUnless(os.name == "posix", "requires POSIX signal masking")
    def test_bounded_run_handles_signal_racing_process_launch(self):
        driver = (
            "import os,signal,time; "
            "os.kill(os.getppid(), signal.SIGTERM); time.sleep(30)"
        )

        with self.assertRaises(SystemExit) as exit_context:
            bounded_run(
                [sys.executable, "-c", driver],
                {},
                timeout_seconds=5,
                termination_grace_seconds=0.2,
            )

        self.assertEqual(exit_context.exception.code, 128 + signal.SIGTERM)

    def test_collector_rejects_non_posix_lifecycle(self):
        with self.assertRaisesRegex(SystemExit, "POSIX"):
            require_collector_posix("nt")

    def test_benchmark_artifacts_require_every_exercised_language(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "missing benchmark artifact"):
                require_benchmark_artifacts(pathlib.Path(directory))

    def test_artifact_provenance_verifies_metadata_and_queries_at_revision(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            checkout = root / "nvim-treesitter"
            data = root / "data"
            metadata = b"return { rust = {} }\n"
            query = b"(identifier) @variable\n"
            for base, relative, content in (
                (checkout, "lua/nvim-treesitter/parsers.lua", metadata),
                (checkout, "runtime/queries/rust/highlights.scm", query),
                (data, "cache/parsers.lua", metadata),
                (data, "queries/rust/highlights.scm", query),
            ):
                path = base / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
            subprocess.run(["git", "init", "-q", checkout], check=True)
            subprocess.run(["git", "-C", checkout, "add", "."], check=True)
            subprocess.run(
                [
                    "git", "-C", checkout,
                    "-c", "user.name=benchmark-test",
                    "-c", "user.email=benchmark@example.invalid",
                    "commit", "-qm", "fixture",
                ],
                check=True,
            )

            provenance = artifact_provenance(data, checkout)

            self.assertRegex(
                provenance["nvim_treesitter_revision"], r"^[0-9a-f]{40}$"
            )
            (data / "queries/rust/highlights.scm").write_bytes(b"different")
            with self.assertRaisesRegex(ValueError, "does not match"):
                artifact_provenance(data, checkout)

    def test_controlled_environment_drops_behavior_overrides(self):
        environment = controlled_environment({
            "PATH": "/bin",
            "TMPDIR": "/tmp",
            "RUST_LOG": "trace",
            "KAKEHASHI_EXPERIMENTAL": "1",
            "RAYON_NUM_THREADS": "99",
        })

        self.assertEqual(environment, {"PATH": "/bin", "TMPDIR": "/tmp"})

    def test_estimated_budget_applies_current_policy(self):
        self.assertEqual(estimated_tree_compute_budget(10), 8)
        self.assertEqual(estimated_tree_compute_budget(1), 1)

    def test_rejects_binary_changed_during_collection(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "kakehashi"
            binary.write_bytes(b"initial")
            expected = hashlib.sha256(b"initial").hexdigest()
            binary.write_bytes(b"replacement")

            with self.assertRaisesRegex(RuntimeError, "binary changed"):
                verify_file_sha256(binary, expected)

    def test_loads_attestation_only_for_matching_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            binary = root / "kakehashi"
            binary.write_bytes(b"release")
            attestation = root / "attestation.json"
            attestation.write_text(json.dumps({
                "schema": 1,
                "source_repository": "https://github.com/atusy/kakehashi",
                "source_commit": "a" * 40,
                "source_checkout_clean": True,
                "build_command": [
                    "cargo", "build", "--release", "--bin", "kakehashi"
                ],
                "rustc": "rustc 1.95.0",
                "cargo": "cargo 1.95.0",
                "binary_relative": "target/release/kakehashi",
                "binary_sha256": hashlib.sha256(b"release").hexdigest(),
            }))

            loaded = load_binary_attestation(attestation, binary)

            self.assertEqual(loaded["source_commit"], "a" * 40)
            binary.write_bytes(b"other")
            with self.assertRaisesRegex(ValueError, "attested binary digest"):
                load_binary_attestation(attestation, binary)

    def test_rejects_attestation_without_clean_release_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            binary = root / "kakehashi"
            binary.write_bytes(b"release")
            attestation = root / "attestation.json"
            attestation.write_text(json.dumps({
                "schema": 1,
                "source_repository": "https://github.com/atusy/kakehashi",
                "source_commit": "a" * 40,
                "source_checkout_clean": False,
                "build_command": ["cargo", "build"],
                "rustc": "",
                "cargo": "",
                "binary_relative": "target/release/kakehashi",
                "binary_sha256": hashlib.sha256(b"release").hexdigest(),
            }))

            with self.assertRaisesRegex(ValueError, "attestation schema"):
                load_binary_attestation(attestation, binary)

    def test_relay_invokes_proxy_through_python(self):
        command = build_driver_command(
            "relay",
            pathlib.Path("/tmp/kakehashi"),
            pathlib.Path("/tmp/data"),
            ["--requests", "1"],
            pathlib.Path("/tmp/profile"),
        )

        self.assertEqual(command[0], sys.executable)
        self.assertEqual(command[2:6], [
            "--bin", sys.executable,
            "--server-arg", "/tmp/profile/worker_proxy.py",
        ])

    def test_alternates_direct_and_relay_order(self):
        self.assertEqual(run_order(0), ("direct", "relay"))
        self.assertEqual(run_order(1), ("relay", "direct"))

    def test_parses_driver_summary(self):
        output = """
[drive] lang=rust cycles=100 tokens/req=42 wall=1574ms
[drive] method=textDocument/semanticTokens/full count=100 ok=100 canceled=0 null=0 errors=0 p50=3.6ms p90=4.0ms p95=4.3ms p99=5.2ms max=7.8ms wire=1430.1KiB
"""

        summary = parse_driver_summary(output, expected_count=100)

        self.assertEqual(summary["wall"], 1574)
        self.assertEqual(summary["p50"], 3.6)
        self.assertEqual(summary["p95"], 4.3)
        self.assertEqual(summary["p99"], 5.2)
        self.assertEqual(summary["wire_kib"], 1430.1)
        self.assertEqual(summary["count"], 100)
        self.assertEqual(summary["ok"], 100)
        self.assertEqual(summary["tokens_per_request"], 42)

    def test_parses_and_validates_capture_pilot_summary(self):
        output = """
[drive] lang=markdown cycles=100 tokens/req=42 wall=1574ms
[drive] method=kakehashi/captures/full/delta count=100 ok=100 canceled=0 null=0 errors=0 p50=31.0ms p90=33.0ms p95=34.1ms p99=35.2ms max=36.0ms wire=1430.1KiB
[drive] method=textDocument/semanticTokens/full count=100 ok=100 canceled=0 null=0 errors=0 p50=4.8ms p90=4.9ms p95=4.9ms p99=5.2ms max=7.8ms wire=1430.1KiB
[drive] capture-validation count=100 delta_shapes=100 lineage_advances=100 full_fallbacks=0
"""

        summary = parse_capture_pilot_summary(output, expected_count=100)

        self.assertEqual(summary["semantic_p50"], 4.8)
        self.assertEqual(summary["semantic_p95"], 4.9)
        self.assertEqual(summary["captures_p50"], 31.0)
        self.assertEqual(summary["captures_p95"], 34.1)
        self.assertEqual(summary["semantic_outcomes"]["ok"], 100)
        self.assertEqual(summary["capture_outcomes"]["ok"], 100)
        self.assertEqual(summary["capture_full_fallbacks"], 0)
        self.assertEqual(summary["capture_delta_shapes"], 100)
        self.assertEqual(summary["capture_lineage_advances"], 100)

    def test_rejects_capture_pilot_without_validation_summary(self):
        output = """
[drive] method=kakehashi/captures/full/delta count=1 ok=1 canceled=0 null=0 errors=0 p50=1.0ms p90=1.0ms p95=1.0ms p99=1.0ms max=1.0ms wire=1.0KiB
[drive] method=textDocument/semanticTokens/full count=1 ok=1 canceled=0 null=0 errors=0 p50=1.0ms p90=1.0ms p95=1.0ms p99=1.0ms max=1.0ms wire=1.0KiB
"""

        with self.assertRaisesRegex(ValueError, "capture validation"):
            parse_capture_pilot_summary(output, expected_count=1)

    def test_rejects_driver_summary_with_failed_responses(self):
        output = """
[drive] lang=rust cycles=100 tokens/req=42 wall=10ms
[drive] method=textDocument/semanticTokens/full count=100 ok=0 canceled=0 null=0 errors=100 p50=0.1ms p90=0.1ms p95=0.1ms p99=0.1ms max=0.1ms wire=1.0KiB
"""

        with self.assertRaisesRegex(ValueError, "non-success responses"):
            parse_driver_summary(output, expected_count=100)

    def test_rejects_incomplete_driver_summary(self):
        output = """
[drive] lang=rust cycles=100 tokens/req=42 wall=10ms
[drive] method=textDocument/semanticTokens/full count=99 ok=99 canceled=0 null=0 errors=0 p50=0.1ms p90=0.1ms p95=0.1ms p99=0.1ms max=0.1ms wire=1.0KiB
"""

        with self.assertRaisesRegex(ValueError, "expected 100 responses"):
            parse_driver_summary(output, expected_count=100)

    def test_rejects_empty_known_workload(self):
        output = """
[drive] lang=rust cycles=100 tokens/req=0 wall=10ms
[drive] method=textDocument/semanticTokens/full count=100 ok=100 canceled=0 null=0 errors=0 p50=0.1ms p90=0.1ms p95=0.1ms p99=0.1ms max=0.1ms wire=1.0KiB
"""

        with self.assertRaisesRegex(ValueError, "empty semantic-token"):
            parse_driver_summary(output, expected_count=100)

    def test_data_tree_digest_is_stable_and_content_sensitive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            queries = root / "queries"
            queries.mkdir()
            (queries / "b").write_bytes(b"two")
            (queries / "a").write_bytes(b"one")

            first = shasum_tree_digest(root)
            second = shasum_tree_digest(root)
            (queries / "a").write_bytes(b"changed")

            self.assertEqual(first, second)
            self.assertNotEqual(first, shasum_tree_digest(root))

    def test_data_tree_digest_sorts_relative_posix_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for relative in ("queries/a-/x", "queries/a/x"):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(relative.encode())

            expected_input = b"".join(
                f"{hashlib.sha256(relative.encode()).hexdigest()}  ./{relative}\n".encode()
                for relative in ("queries/a-/x", "queries/a/x")
            )

            self.assertEqual(
                shasum_tree_digest(root), hashlib.sha256(expected_input).hexdigest()
            )

    def test_data_tree_digest_excludes_file_symlinks_like_find_type_f(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            target = root / "parser/target"
            target.parent.mkdir()
            target.write_bytes(b"content")
            without_link = shasum_tree_digest(root)
            try:
                (root / "parser/link").symlink_to(target)
            except OSError as error:
                self.skipTest(f"symlinks unavailable: {error}")

            self.assertEqual(shasum_tree_digest(root), without_link)

    def test_data_tree_digest_ignores_non_runtime_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            query = root / "queries/rust/highlights.scm"
            query.parent.mkdir(parents=True)
            query.write_bytes(b"query")
            runtime_digest = shasum_tree_digest(root)
            extra = root / "query-assets/parser/dockerfile.dylib"
            extra.parent.mkdir(parents=True)
            extra.write_bytes(b"unrelated")
            (root / ".installed").touch()

            self.assertEqual(shasum_tree_digest(root), runtime_digest)


if __name__ == "__main__":
    unittest.main()
