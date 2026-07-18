import hashlib
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).parent))

import collect_worker_proxy as collector
from collect_worker_proxy import (
    CLEANUP_SIGNALS,
    build_driver_command,
    collect_pairs,
    bounded_run,
    controlled_environment,
    estimated_tree_compute_budget,
    artifact_provenance,
    parse_capture_pilot_summary,
    parse_driver_summary,
    parse_linux_cpu_model,
    official_revision_blobs,
    load_binary_attestation,
    parser_library_suffix,
    require_posix as require_collector_posix,
    require_benchmark_artifacts,
    run_order,
    run_with_staging_cleanup,
    restore_signal_state,
    signal_process_group,
    shasum_tree_digest,
    stage_measurement_inputs,
    verify_file_sha256,
    verify_official_revision,
)


class CollectionHelpersTest(unittest.TestCase):
    def test_driver_summary_accepts_scientific_wall_time(self):
        output = (
            "tokens/req=42 wall=1.25e-05ms\n"
            "method=textDocument/semanticTokens/full "
            "count=1 ok=1 canceled=0 null=0 errors=0 "
            "p50=1.0ms p90=1.0ms p95=1.0ms p99=1.0ms "
            "max=1.0ms wire=1.0KiB"
        )

        summary = parse_driver_summary(output, expected_count=1)

        self.assertEqual(summary["wall"], 1.25e-05)

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups")
    def test_bounded_run_does_not_wait_for_escaped_pipe_holder(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "escaped.pid"
            escaped = (
                "import os,time; "
                f"open({str(pid_file)!r}, 'w').write(str(os.getpid())); "
                "time.sleep(2)"
            )
            driver = (
                "import subprocess,sys,time; "
                f"subprocess.Popen([sys.executable, '-c', {escaped!r}], "
                "start_new_session=True); time.sleep(30)"
            )
            started = time.monotonic()
            escaped_pid = None
            try:
                with self.assertRaises(subprocess.TimeoutExpired):
                    bounded_run(
                        [sys.executable, "-c", driver],
                        {},
                        timeout_seconds=0.1,
                        termination_grace_seconds=0.1,
                    )
                self.assertLess(time.monotonic() - started, 1.0)
            finally:
                if pid_file.exists():
                    escaped_pid = int(pid_file.read_text())
                if escaped_pid is not None:
                    try:
                        os.kill(escaped_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_staging_cleanup_runs_when_collection_fails(self):
        staging = mock.Mock()

        with self.assertRaisesRegex(RuntimeError, "collection failed"):
            run_with_staging_cleanup(
                staging,
                lambda: (_ for _ in ()).throw(RuntimeError("collection failed")),
            )

        staging.cleanup.assert_called_once_with()

    def test_process_group_signal_tolerates_disappearing_process(self):
        process = mock.Mock(pid=12345)

        with mock.patch.object(
            collector.os, "killpg", side_effect=ProcessLookupError("gone")
        ):
            signal_process_group(process, signal.SIGTERM)

    def test_process_group_signal_does_not_hide_permission_errors(self):
        process = mock.Mock(pid=12345)

        with (
            mock.patch.object(
                collector.os, "killpg", side_effect=PermissionError("denied")
            ),
            self.assertRaises(PermissionError),
        ):
            signal_process_group(process, signal.SIGTERM)

    def test_process_group_termination_stops_after_graceful_exit(self):
        process = mock.Mock(pid=12345)
        process.communicate.return_value = ("stdout", "stderr")

        with mock.patch.object(collector, "signal_process_group") as send_signal:
            output = collector.terminate_process_group(process, 3)

        self.assertEqual(output, ("stdout", "stderr"))
        send_signal.assert_called_once_with(process, signal.SIGTERM)
        process.communicate.assert_called_once_with(timeout=3)

    def test_attested_build_requires_committed_lockfile(self):
        self.assertIn("--locked", collector.ATTESTED_BUILD_COMMAND)

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
                "target=int(__import__('sys').argv[1]); "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                f"open({str(pid_file)!r}, 'w').write(str(os.getpid())); "
                "time.sleep(0.1); os.kill(target, signal.SIGINT); "
                "time.sleep(0.05); os.kill(target, signal.SIGINT); "
                "time.sleep(30)"
            )
            driver = (
                "import os,subprocess,sys,time; "
                f"subprocess.Popen([sys.executable, '-c', {descendant!r}, "
                "str(os.getppid())]); "
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

    @unittest.skipUnless(os.name == "posix", "requires POSIX signal masks")
    def test_bounded_run_reaps_when_handler_installation_is_interrupted(self):
        process = mock.Mock(pid=12345)
        with (
            mock.patch.object(collector.subprocess, "Popen", return_value=process),
            mock.patch.object(
                collector, "install_termination_handlers",
                side_effect=KeyboardInterrupt,
            ),
            mock.patch.object(collector, "terminate_process_group") as terminate,
        ):
            with self.assertRaises(KeyboardInterrupt):
                bounded_run(["driver"], {}, timeout_seconds=5)

        terminate.assert_called_once_with(process, 3)

    def test_cleanup_signals_include_interrupt(self):
        self.assertIn(signal.SIGINT, CLEANUP_SIGNALS)

    def test_signal_handlers_are_restored_before_signals_are_unblocked(self):
        calls = []
        handlers = {signal.SIGTERM: signal.SIG_DFL}
        previous_mask = {signal.SIGINT}
        with (
            mock.patch.object(
                collector, "restore_signal_handlers",
                side_effect=lambda value: calls.append(("handlers", value)),
            ),
            mock.patch.object(
                collector.signal, "pthread_sigmask",
                side_effect=lambda operation, value: calls.append(
                    ("mask", operation, value)
                ),
            ),
        ):
            restore_signal_state(handlers, previous_mask)

        self.assertEqual(calls, [
            ("handlers", handlers),
            ("mask", signal.SIG_SETMASK, previous_mask),
        ])

    def test_termination_signals_only_use_available_platform_signals(self):
        expected = tuple(
            getattr(signal, name)
            for name in ("SIGHUP", "SIGTERM")
            if hasattr(signal, name)
        )

        self.assertEqual(collector.TERMINATION_SIGNALS, expected)

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
            subprocess.run(
                [
                    "git", "-C", checkout, "remote", "add", "origin",
                    "https://github.com/nvim-treesitter/nvim-treesitter",
                ],
                check=True,
            )
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

            official_blobs = {
                "lua/nvim-treesitter/parsers.lua": metadata,
                "runtime/queries/rust/highlights.scm": query,
            }
            with mock.patch.object(
                collector, "official_revision_blobs",
                return_value=("f" * 40, official_blobs),
            ):
                provenance = artifact_provenance(data, checkout)

                self.assertRegex(
                    provenance["nvim_treesitter_revision"], r"^[0-9a-f]{40}$"
                )
                (data / "queries/rust/highlights.scm").write_bytes(b"different")
                with self.assertRaisesRegex(ValueError, "does not match"):
                    artifact_provenance(data, checkout)

    def test_artifact_provenance_rejects_unofficial_checkout_origin(self):
        with tempfile.TemporaryDirectory() as directory:
            checkout = pathlib.Path(directory) / "nvim-treesitter"
            checkout.mkdir()
            subprocess.run(["git", "init", "-q", checkout], check=True)
            subprocess.run(
                [
                    "git", "-C", checkout, "remote", "add", "origin",
                    "https://github.com/example/fork",
                ],
                check=True,
            )

            with self.assertRaisesRegex(ValueError, "unexpected.*origin"):
                artifact_provenance(pathlib.Path(directory) / "data", checkout)

    def test_official_revision_must_be_ancestor_of_fetched_main(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            upstream = root / "upstream"
            checkout = root / "checkout"
            upstream.mkdir()
            subprocess.run(["git", "init", "-q", "-b", "main", upstream], check=True)
            (upstream / "file").write_text("official")
            subprocess.run(["git", "-C", upstream, "add", "."], check=True)
            subprocess.run(
                [
                    "git", "-C", upstream,
                    "-c", "user.name=benchmark-test",
                    "-c", "user.email=benchmark@example.invalid",
                    "commit", "-qm", "official",
                ],
                check=True,
            )
            subprocess.run(["git", "clone", "-q", upstream, checkout], check=True)
            official = subprocess.run(
                ["git", "-C", checkout, "rev-parse", "HEAD"],
                check=True, text=True, stdout=subprocess.PIPE,
            ).stdout.strip()

            subprocess.run(
                [
                    "git", "-C", checkout, "config",
                    "remote.origin.uploadpack", "/bin/false",
                ],
                check=True,
            )
            self.assertEqual(
                verify_official_revision(official, str(upstream)), official
            )
            subprocess.run(["git", "-C", checkout, "checkout", "-q", "--orphan", "local"], check=True)
            (checkout / "file").write_text("local")
            subprocess.run(["git", "-C", checkout, "add", "."], check=True)
            subprocess.run(
                [
                    "git", "-C", checkout,
                    "-c", "user.name=benchmark-test",
                    "-c", "user.email=benchmark@example.invalid",
                    "commit", "-qm", "local",
                ],
                check=True,
            )
            local = subprocess.run(
                ["git", "-C", checkout, "rev-parse", "HEAD"],
                check=True, text=True, stdout=subprocess.PIPE,
            ).stdout.strip()
            subprocess.run(
                ["git", "-C", checkout, "replace", official, local], check=True
            )

            fetched, blobs = official_revision_blobs(
                official, ["file"], str(upstream)
            )
            self.assertEqual(fetched, official)
            self.assertEqual(blobs["file"], b"official")

            with self.assertRaisesRegex(ValueError, "official origin/main"):
                verify_official_revision(local, str(upstream))

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

    def test_stages_private_measurement_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            binary = root / "kakehashi"
            binary.write_bytes(b"binary-v1")
            data = root / "data"
            query = data / "queries/rust/highlights.scm"
            query.parent.mkdir(parents=True)
            query.write_bytes(b"query-v1")
            scripts = root / "scripts"
            scripts.mkdir()
            for name in collector.HARNESS_SCRIPT_NAMES:
                (scripts / name).write_text(f"# {name} v1\n")

            staging, staged_binary, staged_data, staged_scripts = (
                stage_measurement_inputs(binary, data, scripts)
            )
            self.addCleanup(staging.cleanup)
            binary.write_bytes(b"binary-v2")
            query.write_bytes(b"query-v2")
            (scripts / "drive.py").write_text("# drive v2\n")

            self.assertEqual(staged_binary.read_bytes(), b"binary-v1")
            self.assertEqual(
                (staged_data / "queries/rust/highlights.scm").read_bytes(),
                b"query-v1",
            )
            self.assertEqual(
                (staged_scripts / "drive.py").read_text(),
                "# drive.py v1\n",
            )

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
                    "cargo", "build", "--locked", "--release", "--bin", "kakehashi"
                ],
                "rustc": "rustc 1.95.0",
                "cargo": "cargo 1.95.0",
                "native_toolchain": {
                    "rustc_verbose": "rustc 1.95.0\nhost: test-target",
                    "cc": "cc 1.0",
                    "linker": "ld 1.0",
                    "sdk_path": "not applicable",
                    "sdk_version": "not applicable",
                },
                "build_environment": {
                    "PATH": "/bin",
                    "CARGO_TARGET_DIR": "/tmp/fresh-target",
                    "CARGO_HOME": "/tmp/fresh-target/cargo-home",
                },
                "built_in_fresh_target": True,
                "source_isolated_archive": True,
                "cargo_config_ancestry_clean": True,
                "binary_relative": "target/release/kakehashi",
                "binary_sha256": hashlib.sha256(b"release").hexdigest(),
            }))

            loaded = load_binary_attestation(attestation, binary)

            self.assertEqual(loaded["source_commit"], "a" * 40)
            without_ancestry_proof = json.loads(attestation.read_text())
            del without_ancestry_proof["cargo_config_ancestry_clean"]
            attestation.write_text(json.dumps(without_ancestry_proof))
            with self.assertRaisesRegex(ValueError, "missing required fields"):
                load_binary_attestation(attestation, binary)
            attestation.write_text(json.dumps(loaded))
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
                "native_toolchain": {},
                "build_environment": {},
                "built_in_fresh_target": False,
                "source_isolated_archive": False,
                "cargo_config_ancestry_clean": False,
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

    def test_reads_linux_cpu_model_name(self):
        cpuinfo = """processor : 0
vendor_id : GenuineIntel
model name : Example CPU 9000 @ 3.00GHz
processor : 1
model name : Example CPU 9000 @ 3.00GHz
"""

        self.assertEqual(
            parse_linux_cpu_model(cpuinfo),
            "Example CPU 9000 @ 3.00GHz",
        )

    def test_warms_both_paths_before_collecting_measured_pairs(self):
        calls = []

        def run(kind):
            calls.append(kind)
            return {"call": len(calls)}

        pairs = collect_pairs(run, 2)

        self.assertEqual(calls, [
            "direct", "relay",
            "direct", "relay",
            "relay", "direct",
        ])
        self.assertEqual(pairs[0]["direct"], {"call": 3})
        self.assertEqual(pairs[1]["relay"], {"call": 5})

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
