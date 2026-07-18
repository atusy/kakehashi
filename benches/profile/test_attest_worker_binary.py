import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from attest_worker_binary import (
    archive_source,
    controlled_build_environment,
    environment_tool_version,
    native_toolchain_metadata,
    require_isolated_source,
)


class BinaryAttestationTest(unittest.TestCase):
    def test_tool_version_captures_stderr_only_tools(self):
        output = environment_tool_version(
            [sys.executable, "-c", "import sys; print('linker 1.0', file=sys.stderr)"],
            {},
        )

        self.assertEqual(output, "linker 1.0")

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

    def test_isolated_source_rejects_checkout_descendant(self):
        with tempfile.TemporaryDirectory() as directory:
            checkout = pathlib.Path(directory) / "repository"
            source = checkout / "tmp/source"
            source.mkdir(parents=True)

            with self.assertRaisesRegex(ValueError, "inside source checkout"):
                require_isolated_source(source, checkout)

    def test_isolated_source_rejects_ancestor_cargo_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            checkout = root / "repository"
            checkout.mkdir()
            source = root / "temporary/source"
            source.mkdir(parents=True)
            cargo_config = root / "temporary/.cargo/config.toml"
            cargo_config.parent.mkdir()
            cargo_config.write_text("[build]\n")

            with self.assertRaisesRegex(ValueError, "Cargo configuration"):
                require_isolated_source(source, checkout)

    def test_records_native_compiler_target_and_sdk(self):
        versions = {
            ("rustc", "-vV"): "rustc 1.95.0\nhost: aarch64-apple-darwin",
            ("cc", "--version"): "Apple clang version 17.0.0",
            ("xcrun", "--show-sdk-path"): "/SDKs/MacOSX.sdk",
            ("xcrun", "--show-sdk-version"): "26.5",
        }

        metadata = native_toolchain_metadata(
            {},
            system="Darwin",
            version=lambda command, _environment: versions[tuple(command)],
            linker_version=lambda _environment, _system: (
                "@(#)PROGRAM:ld PROJECT:ld-1234"
            ),
        )

        self.assertEqual(metadata, {
            "rustc_verbose": "rustc 1.95.0\nhost: aarch64-apple-darwin",
            "cc": "Apple clang version 17.0.0",
            "linker": "@(#)PROGRAM:ld PROJECT:ld-1234",
            "sdk_path": "/SDKs/MacOSX.sdk",
            "sdk_version": "26.5",
        })


if __name__ == "__main__":
    unittest.main()
