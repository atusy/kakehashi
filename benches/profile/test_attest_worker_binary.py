import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from attest_worker_binary import archive_source, controlled_build_environment


class BinaryAttestationTest(unittest.TestCase):
    def test_build_environment_drops_ambient_build_overrides(self):
        environment = controlled_build_environment(
            {
                "PATH": "/bin",
                "TMPDIR": "/tmp",
                "HOME": "/ambient-home",
                "RUSTFLAGS": "-C target-cpu=native",
                "CARGO_BUILD_TARGET": "custom-target",
                "RUSTC_WRAPPER": "wrapper",
            },
            "/tmp/fresh-target",
        )

        self.assertEqual(environment, {
            "PATH": "/bin",
            "TMPDIR": "/tmp",
            "CARGO_TARGET_DIR": "/tmp/fresh-target",
            "CARGO_HOME": "/tmp/fresh-target/cargo-home",
        })

    def test_source_archive_excludes_parent_cargo_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            checkout = root / "parent/repository"
            checkout.mkdir(parents=True)
            parent_config = root / "parent/.cargo/config.toml"
            parent_config.parent.mkdir()
            parent_config.write_text('[build]\nrustflags = ["--cfg", "ambient"]\n')
            (checkout / "source.txt").write_text("committed")
            subprocess.run(["git", "init", "-q", checkout], check=True)
            subprocess.run(["git", "-C", checkout, "add", "."], check=True)
            subprocess.run(
                [
                    "git", "-C", checkout,
                    "-c", "user.name=benchmark-test",
                    "-c", "user.email=benchmark@example.invalid",
                    "commit", "-qm", "source",
                ],
                check=True,
            )
            revision = subprocess.run(
                ["git", "-C", checkout, "rev-parse", "HEAD"],
                check=True, text=True, stdout=subprocess.PIPE,
            ).stdout.strip()
            destination = root / "isolated/source"

            archive_source(checkout, revision, destination)

            self.assertEqual((destination / "source.txt").read_text(), "committed")
            self.assertFalse((destination / ".cargo/config.toml").exists())


if __name__ == "__main__":
    unittest.main()
