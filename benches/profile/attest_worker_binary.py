#!/usr/bin/env python3
"""Build a clean checkout and attest the resulting benchmark binary."""

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile

from collect_worker_proxy import ATTESTED_BUILD_COMMAND, sha256_file, tool_version

BINARY_RELATIVE = pathlib.Path("target/release/kakehashi")
BUILD_ENVIRONMENT_KEYS = (
    "PATH", "SYSTEMROOT", "WINDIR", "TMPDIR", "TMP", "TEMP", "LANG",
    "LC_ALL", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET",
)


def controlled_build_environment(source, target_dir):
    environment = {
        key: source[key] for key in BUILD_ENVIRONMENT_KEYS if key in source
    }
    environment["CARGO_TARGET_DIR"] = str(target_dir)
    environment["CARGO_HOME"] = str(pathlib.Path(target_dir) / "cargo-home")
    return environment


def environment_tool_version(command, environment):
    return subprocess.run(
        command, check=True, text=True, stdout=subprocess.PIPE, env=environment
    ).stdout.strip()


def git(checkout, *arguments):
    return tool_version(["git", "-C", str(checkout), *arguments])


def require_clean(checkout):
    status = git(checkout, "status", "--porcelain")
    if status:
        raise ValueError(f"binary source checkout is dirty:\n{status}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--checkout", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()

    require_clean(args.checkout)
    source_commit = git(args.checkout, "rev-parse", "HEAD")
    source_repository = git(args.checkout, "remote", "get-url", "origin")
    with tempfile.TemporaryDirectory(prefix="kakehashi-attested-target-") as target:
        build_environment = controlled_build_environment(os.environ, target)
        subprocess.run(
            ATTESTED_BUILD_COMMAND,
            cwd=args.checkout,
            check=True,
            env=build_environment,
        )
        built_binary = pathlib.Path(target) / "release/kakehashi"
        binary = args.checkout / BINARY_RELATIVE
        binary.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(built_binary, binary)
        rustc = environment_tool_version(["rustc", "--version"], build_environment)
        cargo = environment_tool_version(["cargo", "--version"], build_environment)
    require_clean(args.checkout)
    result = {
        "schema": 1,
        "source_repository": source_repository,
        "source_commit": source_commit,
        "source_checkout_clean": True,
        "build_command": ATTESTED_BUILD_COMMAND,
        "rustc": rustc,
        "cargo": cargo,
        "build_environment": build_environment,
        "built_in_fresh_target": True,
        "binary_relative": BINARY_RELATIVE.as_posix(),
        "binary_sha256": sha256_file(binary),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
