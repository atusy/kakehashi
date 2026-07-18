import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from attest_worker_binary import controlled_build_environment


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


if __name__ == "__main__":
    unittest.main()
