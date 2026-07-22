import tempfile
import unittest
from pathlib import Path

from collect_semantic_pairs import (
    manifest_sha256,
    set_tree_read_only,
    set_tree_writable,
    tree_manifest,
)


class CollectSemanticPairsTest(unittest.TestCase):
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
