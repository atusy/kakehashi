import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

try:
    import tomllib
except ImportError:
    tomllib = None

import prepare_discovery


@unittest.skipIf(tomllib is None, "TOML parser requires Python 3.11")
class PrepareUnicodeTest(unittest.TestCase):
    def test_non_bmp_paths_remain_valid_toml(self):
        with tempfile.TemporaryDirectory(prefix="discovery-🚀-") as temporary:
            directory = Path(temporary)
            runtime = directory / "runtime"
            (runtime / "parser").mkdir(parents=True)
            (runtime / "queries").mkdir()
            output = directory / "inputs"
            arguments = ["prepare_discovery.py", "--runtime", str(runtime),
                         "--output", str(output)]
            with patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()), \
                    patch.object(prepare_discovery, "rust_fixture", return_value="// comment\n"), \
                    patch.object(prepare_discovery, "markdown_fixture", return_value="prose\n"):
                prepare_discovery.main()
            for mode in ("native", "narrow", "wildcard"):
                config = tomllib.loads((output / f"{mode}.toml").read_text(encoding="utf-8"))
                self.assertEqual(config["searchPaths"], [str(runtime.resolve())])
                if mode != "native":
                    self.assertEqual(config["languageServers"]["profile"]["cmd"][-1],
                                     str(output.resolve() / "peer-summaries"))


if __name__ == "__main__":
    unittest.main()
