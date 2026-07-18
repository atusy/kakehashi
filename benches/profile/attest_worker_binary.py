#!/usr/bin/env python3
"""Build a clean checkout and attest the resulting benchmark binary."""

import argparse
import json
import os
import pathlib
import platform
import shutil
import subprocess
import tarfile
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
        command, check=True, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, env=environment,
    ).stdout.strip()


def compiler_linker_version(environment, system):
    linker_flag = "-Wl,-v" if system == "Darwin" else "-Wl,--version"
    return subprocess.run(
        ["cc", linker_flag, "-x", "c", "-", "-o", os.devnull],
        check=True,
        text=True,
        input="int main(void) { return 0; }\n",
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env=environment,
    ).stdout.strip()


def native_toolchain_metadata(
    environment,
    system=None,
    version=environment_tool_version,
    linker_version=compiler_linker_version,
):
    system = system or platform.system()
    metadata = {
        "rustc_verbose": version(["rustc", "-vV"], environment),
        "cc": version(["cc", "--version"], environment),
        "linker": linker_version(environment, system),
        "sdk_path": "not applicable",
        "sdk_version": "not applicable",
    }
    if system == "Darwin":
        metadata["sdk_path"] = version(
            ["xcrun", "--show-sdk-path"], environment
        )
        metadata["sdk_version"] = version(
            ["xcrun", "--show-sdk-version"], environment
        )
    return metadata


def extract_source_archive(source, destination):
    if getattr(tarfile, "data_filter", None) is None:
        # The archive is produced from this already-trusted local Git commit.
        # Python < 3.12 lacks PEP 706's extraction filter.
        source.extractall(destination)
    else:
        source.extractall(destination, filter="data")


def archive_source(checkout, revision, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    archive = destination.parent / "source.tar"
    git_environment = dict(os.environ)
    git_environment.update({
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_NO_REPLACE_OBJECTS": "1",
    })
    subprocess.run(
        [
            "git", "-C", str(checkout), "archive", "--format=tar",
            "--output", str(archive), revision,
        ],
        check=True,
        env=git_environment,
    )
    destination.mkdir()
    try:
        with tarfile.open(archive) as source:
            extract_source_archive(source, destination)
    finally:
        archive.unlink(missing_ok=True)


def require_isolated_source(source, checkout):
    source = source.resolve()
    checkout = checkout.resolve()
    if source == checkout or checkout in source.parents:
        raise ValueError(f"temporary build source is inside source checkout: {source}")
    for directory in (source, *source.parents):
        for name in ("config", "config.toml"):
            config = directory / ".cargo" / name
            if config.is_file():
                raise ValueError(
                    f"temporary build source inherits Cargo configuration: {config}"
                )


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
    with tempfile.TemporaryDirectory(prefix="kakehashi-attested-build-") as root:
        isolated_root = pathlib.Path(root)
        source = isolated_root / "source"
        require_isolated_source(source, args.checkout)
        archive_source(args.checkout, source_commit, source)
        target = isolated_root / "target"
        build_environment = controlled_build_environment(os.environ, target)
        subprocess.run(
            ATTESTED_BUILD_COMMAND,
            cwd=source,
            check=True,
            env=build_environment,
        )
        built_binary = target / "release/kakehashi"
        binary = args.checkout / BINARY_RELATIVE
        binary.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(built_binary, binary)
        rustc = environment_tool_version(["rustc", "--version"], build_environment)
        cargo = environment_tool_version(["cargo", "--version"], build_environment)
        native_toolchain = native_toolchain_metadata(build_environment)
    require_clean(args.checkout)
    result = {
        "schema": 1,
        "source_repository": source_repository,
        "source_commit": source_commit,
        "source_checkout_clean": True,
        "build_command": ATTESTED_BUILD_COMMAND,
        "rustc": rustc,
        "cargo": cargo,
        "native_toolchain": native_toolchain,
        "build_environment": build_environment,
        "built_in_fresh_target": True,
        "source_isolated_archive": True,
        "cargo_config_ancestry_clean": True,
        "binary_relative": BINARY_RELATIVE.as_posix(),
        "binary_sha256": sha256_file(binary),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
