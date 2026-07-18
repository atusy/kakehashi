import hashlib
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from collect_worker_proxy import (
    build_driver_command,
    estimated_tree_compute_budget,
    parse_driver_summary,
    run_order,
    shasum_tree_digest,
)


class CollectionHelpersTest(unittest.TestCase):
    def test_estimated_budget_applies_current_policy(self):
        self.assertEqual(estimated_tree_compute_budget(10), 8)
        self.assertEqual(estimated_tree_compute_budget(1), 1)

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
[drive] lang=rust cycles=100 wall=1574ms
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

    def test_rejects_driver_summary_with_failed_responses(self):
        output = """
[drive] lang=rust cycles=100 wall=10ms
[drive] method=textDocument/semanticTokens/full count=100 ok=0 canceled=0 null=0 errors=100 p50=0.1ms p90=0.1ms p95=0.1ms p99=0.1ms max=0.1ms wire=1.0KiB
"""

        with self.assertRaisesRegex(ValueError, "non-success responses"):
            parse_driver_summary(output, expected_count=100)

    def test_rejects_incomplete_driver_summary(self):
        output = """
[drive] lang=rust cycles=100 wall=10ms
[drive] method=textDocument/semanticTokens/full count=99 ok=99 canceled=0 null=0 errors=0 p50=0.1ms p90=0.1ms p95=0.1ms p99=0.1ms max=0.1ms wire=1.0KiB
"""

        with self.assertRaisesRegex(ValueError, "expected 100 responses"):
            parse_driver_summary(output, expected_count=100)

    def test_data_tree_digest_is_stable_and_content_sensitive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "b").write_bytes(b"two")
            (root / "a").write_bytes(b"one")

            first = shasum_tree_digest(root)
            second = shasum_tree_digest(root)
            (root / "a").write_bytes(b"changed")

            self.assertEqual(first, second)
            self.assertNotEqual(first, shasum_tree_digest(root))

    def test_data_tree_digest_sorts_relative_posix_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for relative in ("a-/x", "a/x"):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(relative.encode())

            expected_input = b"".join(
                f"{hashlib.sha256(relative.encode()).hexdigest()}  ./{relative}\n".encode()
                for relative in ("a-/x", "a/x")
            )

            self.assertEqual(
                shasum_tree_digest(root), hashlib.sha256(expected_input).hexdigest()
            )

    def test_data_tree_digest_excludes_file_symlinks_like_find_type_f(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            target = root / "target"
            target.write_bytes(b"content")
            without_link = shasum_tree_digest(root)
            try:
                (root / "link").symlink_to(target)
            except OSError as error:
                self.skipTest(f"symlinks unavailable: {error}")

            self.assertEqual(shasum_tree_digest(root), without_link)


if __name__ == "__main__":
    unittest.main()
