"""Runtime provenance checks against fixtures prepared by the real generator."""
import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import measure_discovery
import prepare_discovery


class DiscoveryInputsTest(unittest.TestCase):
    def prepare_inputs(self, root):
        runtime = root / "runtime"
        (runtime / "parser").mkdir(parents=True)
        (runtime / "queries" / "rust").mkdir(parents=True)
        (runtime / "parser" / "rust.so").write_bytes(b"fixed parser bytes")
        (runtime / "queries" / "rust" / "highlights.scm").write_text(
            "(identifier) @variable\n", encoding="utf-8")
        output = root / "inputs"
        arguments = ["prepare_discovery.py", "--runtime", str(runtime),
                     "--output", str(output)]
        with patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()), \
                patch.object(prepare_discovery, "rust_fixture", return_value="// comment\n"), \
                patch.object(prepare_discovery, "markdown_fixture", return_value="prose\n"):
            prepare_discovery.main()
        measure_discovery.verify_inputs(output)
        return runtime, output

    def test_rejects_added_runtime_query(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime, inputs = self.prepare_inputs(Path(directory))
            (runtime / "queries" / "rust" / "injections.scm").write_text(
                "(line_comment) @injection.content\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "runtime inventory changed"):
                measure_discovery.verify_inputs(inputs)

    def test_rejects_added_parser_or_removed_query(self):
        for change in ("add parser", "remove query"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                runtime, inputs = self.prepare_inputs(Path(directory))
                if change == "add parser":
                    (runtime / "parser" / "lua.so").write_bytes(b"another parser")
                else:
                    (runtime / "queries" / "rust" / "highlights.scm").unlink()
                with self.assertRaisesRegex(ValueError, "runtime inventory changed"):
                    measure_discovery.verify_inputs(inputs)

    def test_still_rejects_changed_bytes_with_the_same_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime, inputs = self.prepare_inputs(Path(directory))
            (runtime / "parser" / "rust.so").write_bytes(b"different parser bytes")
            with self.assertRaisesRegex(ValueError, "runtime changed"):
                measure_discovery.verify_inputs(inputs)


if __name__ == "__main__":
    unittest.main()
