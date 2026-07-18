#!/usr/bin/env python3
"""Build a clean checkout and attest the resulting benchmark binary."""

import argparse
import json
import pathlib
import subprocess

from collect_worker_proxy import ATTESTED_BUILD_COMMAND, sha256_file, tool_version

BINARY_RELATIVE = pathlib.Path("target/release/kakehashi")


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
    subprocess.run(ATTESTED_BUILD_COMMAND, cwd=args.checkout, check=True)
    require_clean(args.checkout)
    binary = args.checkout / BINARY_RELATIVE
    result = {
        "schema": 1,
        "source_repository": source_repository,
        "source_commit": source_commit,
        "source_checkout_clean": True,
        "build_command": ATTESTED_BUILD_COMMAND,
        "rustc": tool_version(["rustc", "--version"]),
        "cargo": tool_version(["cargo", "--version"]),
        "binary_relative": BINARY_RELATIVE.as_posix(),
        "binary_sha256": sha256_file(binary),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
