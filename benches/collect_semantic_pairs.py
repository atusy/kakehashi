#!/usr/bin/env python3
"""Collect reproducible AB/BA semantic-token benchmark pairs."""

from __future__ import annotations

import argparse
import atexit
import hashlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

from semantic_summary import summarize_pairs, validate_collection


DEFAULT_SCENARIOS = ",".join(
    [
        "rust_large/full_cache_hit",
        "rust_large/typing_delta",
        "rust_sparse_32k_minus/typing_delta",
        "rust_sparse_32k_exact/typing_delta",
        "rust_sparse_32k_plus/typing_delta",
        "rust_sparse_64k/typing_delta",
        "rust_large/typing_burst",
        "rust_xlarge/cancel_burst",
        "markdown_injections_large/full_cache_hit",
        "markdown_injections/typing_delta",
        "markdown_injections/typing_burst",
        "unicode_rust/full_cache_hit",
    ]
)


def terminate_process_group(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()


def raise_keyboard_interrupt(_signum: int, _frame: Any) -> None:
    raise KeyboardInterrupt


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    timeout: int = 1800,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(command), flush=True)
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
        start_new_session=True,
    )
    try:
        stdout, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        terminate_process_group(process)
        raise RuntimeError(f"command timed out after {timeout}s: {' '.join(command)}")
    except BaseException:
        terminate_process_group(process)
        raise
    if process.returncode != 0:
        if stdout:
            print(stdout, end="")
        raise subprocess.CalledProcessError(process.returncode, command, stdout)
    return subprocess.CompletedProcess(command, process.returncode, stdout, None)


def output(
    command: list[str], *, cwd: Path, env: dict[str, str] | None = None
) -> str:
    return subprocess.check_output(command, cwd=cwd, env=env, text=True).strip()


def optional_output(command: list[str], *, cwd: Path, env: dict[str, str]) -> str | None:
    try:
        return output(command, cwd=cwd, env=env)
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def isolated_environment(temp: Path) -> dict[str, str]:
    path = os.environ.get("PATH")
    if not path:
        raise RuntimeError("PATH is required to locate the Rust and native toolchains")
    environment = {
        "PATH": path,
        "HOME": str(temp / "home"),
        "TMPDIR": str(temp / "tmp"),
        "CARGO_HOME": str(temp / "cargo-home"),
        "CARGO_TERM_COLOR": "never",
        "LANG": "C",
        "LC_ALL": "C",
    }
    rustup_home = os.environ.get("RUSTUP_HOME")
    if rustup_home is None and (ambient_home := os.environ.get("HOME")):
        default_rustup_home = Path(ambient_home) / ".rustup"
        if default_rustup_home.is_dir():
            rustup_home = str(default_rustup_home)
    if rustup_home:
        environment["RUSTUP_HOME"] = rustup_home
    for directory in (environment["HOME"], environment["TMPDIR"], environment["CARGO_HOME"]):
        Path(directory).mkdir(parents=True)
    return environment


def recorded_environment(environment: dict[str, str], temp: Path) -> dict[str, str]:
    prefix = str(temp)
    return {
        key: value.replace(prefix, "<TEMP>") for key, value in sorted(environment.items())
    }


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def normalize_captured_stdout(stdout: str) -> str:
    """Keep captured output readable without introducing whitespace errors."""
    return stdout.rstrip() + "\n" if stdout else ""


def remove_tree_if_exists(path: Path) -> None:
    if path.exists():
        shutil.rmtree(path)


def tree_manifest(root: Path) -> list[dict[str, Any]]:
    entries = []
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            entries.append(
                {"path": relative, "type": "symlink", "target": os.readlink(path)}
            )
        elif path.is_file():
            entries.append(
                {
                    "path": relative,
                    "type": "file",
                    "size": path.stat().st_size,
                    "sha256": file_sha256(path),
                }
            )
    return entries


def manifest_sha256(entries: list[dict[str, Any]]) -> str:
    encoded = json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def set_tree_read_only(root: Path) -> None:
    for path in root.rglob("*"):
        if not path.is_symlink():
            path.chmod(0o555 if path.is_dir() else 0o444)
    root.chmod(0o555)


def set_tree_writable(root: Path) -> None:
    if not root.exists():
        return
    root.chmod(0o755)
    for path in root.rglob("*"):
        if not path.is_symlink():
            path.chmod(0o755 if path.is_dir() else 0o644)


def parse_server_env(values: list[str]) -> dict[str, str]:
    parsed = {}
    for value in values:
        key, separator, item = value.partition("=")
        if not separator or not key:
            raise ValueError(f"server env must be KEY=VALUE: {value}")
        parsed[key] = item
    return parsed


def ensure_clean(repo: Path) -> None:
    status = output(["git", "status", "--porcelain"], cwd=repo)
    if status:
        raise RuntimeError("collector requires a clean committed harness checkout")


def add_worktree(repo: Path, path: Path, commit: str) -> None:
    run(["git", "worktree", "add", "--detach", str(path), commit], cwd=repo)


def remove_worktree(repo: Path, path: Path) -> str | None:
    completed = subprocess.run(
        ["git", "worktree", "remove", "--force", str(path)],
        cwd=repo,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if completed.returncode:
        return completed.stdout.strip() or f"exit status {completed.returncode}"
    return None


def assert_attestations(expected: dict[str, str], paths: dict[str, Path]) -> None:
    actual = {
        key: (
            manifest_sha256(tree_manifest(path))
            if key == "fixture"
            else file_sha256(path)
        )
        for key, path in paths.items()
    }
    if actual != expected:
        raise RuntimeError(f"benchmark inputs changed: expected={expected}, actual={actual}")


def main() -> None:
    if os.name != "posix":
        raise RuntimeError("semantic pair collection currently requires POSIX")
    signal.signal(signal.SIGTERM, raise_keyboard_interrupt)
    if hasattr(signal, "SIGHUP"):
        signal.signal(signal.SIGHUP, raise_keyboard_interrupt)
    parser = argparse.ArgumentParser()
    parser.add_argument("a_ref")
    parser.add_argument("b_ref")
    parser.add_argument("output_dir", type=Path)
    parser.add_argument("--pairs", type=int, default=4)
    parser.add_argument("--iters", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=6)
    parser.add_argument("--scenarios", default=DEFAULT_SCENARIOS)
    parser.add_argument("--server-env", action="append", default=[])
    parser.add_argument("--run-timeout", type=int, default=900)
    args = parser.parse_args()
    if args.pairs < 1 or args.iters < 1 or args.warmup < 0:
        parser.error("pairs and iters must be positive; warmup must be non-negative")

    repo = Path(output(["git", "rev-parse", "--show-toplevel"], cwd=Path.cwd()))
    ensure_clean(repo)
    a_commit = output(["git", "rev-parse", f"{args.a_ref}^{{commit}}"], cwd=repo)
    b_commit = output(["git", "rev-parse", f"{args.b_ref}^{{commit}}"], cwd=repo)
    harness_commit = output(["git", "rev-parse", "HEAD^{commit}"], cwd=repo)
    server_env = parse_server_env(args.server_env)

    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise RuntimeError(f"output directory is not empty: {output_dir}")
    if output_dir.exists():
        output_dir.rmdir()
    output_dir.parent.mkdir(parents=True, exist_ok=True)
    artifact_dir = Path(
        tempfile.mkdtemp(
            prefix=f".{output_dir.name}.staging-", dir=output_dir.parent
        )
    )
    atexit.register(remove_tree_if_exists, artifact_dir)
    published = False

    with tempfile.TemporaryDirectory(prefix="kakehashi-semantic-pairs-") as temporary:
        temp = Path(temporary)
        base_environment = isolated_environment(temp)
        worktrees = {name: temp / name for name in ("a-src", "b-src", "harness-src")}
        added_worktrees: list[Path] = []
        fixture: Path | None = None
        try:
            add_worktree(repo, worktrees["a-src"], a_commit)
            added_worktrees.append(worktrees["a-src"])
            add_worktree(repo, worktrees["b-src"], b_commit)
            added_worktrees.append(worktrees["b-src"])
            add_worktree(repo, worktrees["harness-src"], harness_commit)
            added_worktrees.append(worktrees["harness-src"])

            binaries = {}
            for label, source in (("A", worktrees["a-src"]), ("B", worktrees["b-src"])):
                target = temp / f"target-{label.lower()}"
                env = base_environment | {"CARGO_TARGET_DIR": str(target)}
                run(
                    ["cargo", "build", "--release", "--locked", "--bin", "kakehashi"],
                    cwd=source,
                    env=env,
                )
                binaries[label] = target / "release" / "kakehashi"

            harness_target = temp / "target-harness"
            harness_env = base_environment | {"CARGO_TARGET_DIR": str(harness_target)}
            run(
                [
                    "cargo",
                    "bench",
                    "--bench",
                    "semantic_tokens",
                    "--features",
                    "e2e",
                    "--no-run",
                    "--locked",
                ],
                cwd=worktrees["harness-src"],
                env=harness_env,
            )
            candidates = [
                path
                for path in (harness_target / "release" / "deps").glob("semantic_tokens-*")
                if path.is_file() and os.access(path, os.X_OK)
            ]
            if len(candidates) != 1:
                raise RuntimeError(f"expected one benchmark executable, found {candidates}")
            harness = candidates[0]

            fixture = temp / "fixture"
            run(
                [str(harness)],
                cwd=worktrees["harness-src"],
                env=base_environment
                | {"KAKEHASHI_BENCH_PREPARE_DATA_DIR": str(fixture)},
            )
            fixture_entries = tree_manifest(fixture)
            fixture_archive = artifact_dir / "fixture"
            shutil.copytree(fixture, fixture_archive, symlinks=True)
            if tree_manifest(fixture_archive) != fixture_entries:
                raise RuntimeError("archived fixture differs from measured fixture")
            (artifact_dir / "fixture-manifest.json").write_text(
                json.dumps(fixture_entries, indent=2, sort_keys=True) + "\n"
            )
            set_tree_read_only(fixture)

            paths = {
                "binary_a": binaries["A"],
                "binary_b": binaries["B"],
                "harness": harness,
                "fixture": fixture,
            }
            attestations = {
                "binary_a": file_sha256(binaries["A"]),
                "binary_b": file_sha256(binaries["B"]),
                "harness": file_sha256(harness),
                "fixture": manifest_sha256(fixture_entries),
            }
            raw_documents = []
            raw_files = []
            for pair_index in range(1, args.pairs + 1):
                order = "AB" if pair_index % 2 else "BA"
                first, second = order
                raw_path = artifact_dir / f"pair-{pair_index}.json"
                text_path = artifact_dir / f"pair-{pair_index}.txt"
                assert_attestations(attestations, paths)
                env = base_environment | server_env | {
                    "KAKEHASHI_BENCH_BIN_A": str(binaries[first]),
                    "KAKEHASHI_BENCH_LABEL_A": first,
                    "KAKEHASHI_BENCH_SHA256_A": attestations[f"binary_{first.lower()}"],
                    "KAKEHASHI_BENCH_BIN_B": str(binaries[second]),
                    "KAKEHASHI_BENCH_LABEL_B": second,
                    "KAKEHASHI_BENCH_SHA256_B": attestations[f"binary_{second.lower()}"],
                    "KAKEHASHI_BENCH_DATA_DIR": str(fixture),
                    "KAKEHASHI_BENCH_FIXTURE_SHA256": attestations["fixture"],
                    "KAKEHASHI_BENCH_HARNESS_COMMIT": harness_commit,
                    "KAKEHASHI_BENCH_HARNESS_SHA256": attestations["harness"],
                    "KAKEHASHI_BENCH_PAIR_INDEX": str(pair_index),
                    "KAKEHASHI_BENCH_ORDER": order,
                    "KAKEHASHI_BENCH_ITERS": str(args.iters),
                    "KAKEHASHI_BENCH_WARMUP": str(args.warmup),
                    "KAKEHASHI_BENCH_SCENARIOS": args.scenarios,
                    "KAKEHASHI_BENCH_SAMPLES_FILE": str(raw_path),
                }
                completed = run(
                    [str(harness)],
                    cwd=worktrees["harness-src"],
                    env=env,
                    timeout=args.run_timeout,
                    capture=True,
                )
                captured_stdout = normalize_captured_stdout(completed.stdout or "")
                text_path.write_text(captured_stdout)
                print(captured_stdout, end="")
                assert_attestations(attestations, paths)
                raw_documents.append(json.loads(raw_path.read_text()))
                raw_files.append(
                    {
                        "pair_index": pair_index,
                        "order": order,
                        "samples": raw_path.name,
                        "samples_sha256": file_sha256(raw_path),
                        "stdout": text_path.name,
                        "stdout_sha256": file_sha256(text_path),
                    }
                )

            measured_scenarios = {
                str(run["scenario"]) for run in raw_documents[0].get("runs", [])
            }
            raw_documents = validate_collection(
                raw_documents,
                pair_count=args.pairs,
                scenarios=measured_scenarios,
                iterations=args.iters,
                warmup=args.warmup,
                binary_sha256={
                    "A": attestations["binary_a"],
                    "B": attestations["binary_b"],
                },
                harness_commit=harness_commit,
                harness_sha256=attestations["harness"],
                fixture_sha256=attestations["fixture"],
            )
            summary = summarize_pairs(raw_documents)
            (artifact_dir / "summary.json").write_text(
                json.dumps(summary, indent=2, sort_keys=True) + "\n"
            )
            source_files = [
                "benches/semantic_tokens.rs",
                "benches/support/semantic_baseline.rs",
                "benches/collect_semantic_pairs.py",
                "benches/semantic_summary.py",
            ]
            manifest = {
                "schema_version": 1,
                "source": {
                    "a_ref": args.a_ref,
                    "a_commit": a_commit,
                    "b_ref": args.b_ref,
                    "b_commit": b_commit,
                    "harness_commit": harness_commit,
                    "harness_sources_sha256": {
                        path: file_sha256(worktrees["harness-src"] / path)
                        for path in source_files
                    },
                },
                "attestations": attestations,
                "configuration": {
                    "pairs": args.pairs,
                    "iterations": args.iters,
                    "warmup_iterations": args.warmup,
                    "scenario_filters": args.scenarios.split(","),
                    "scenarios": sorted(measured_scenarios),
                    "server_env": server_env,
                    "run_timeout_seconds": args.run_timeout,
                },
                "environment": {
                    "platform": platform.platform(),
                    "machine": platform.machine(),
                    "processor": platform.processor(),
                    "build": recorded_environment(base_environment, temp),
                    "runtime": recorded_environment(
                        base_environment | server_env, temp
                    ),
                    "toolchains": {
                        label: {
                            "cargo": output(
                                ["cargo", "-Vv"], cwd=source, env=base_environment
                            ),
                            "rustc": output(
                                ["rustc", "-Vv"], cwd=source, env=base_environment
                            ),
                        }
                        for label, source in {
                            "a": worktrees["a-src"],
                            "b": worktrees["b-src"],
                            "harness": worktrees["harness-src"],
                        }.items()
                    },
                    "cc": optional_output(["cc", "--version"], cwd=repo, env=base_environment),
                    "clang": optional_output(
                        ["clang", "--version"], cwd=repo, env=base_environment
                    ),
                    "sdk": optional_output(
                        ["xcrun", "--show-sdk-version"], cwd=repo, env=base_environment
                    ),
                },
                "raw_files": raw_files,
                "fixture_manifest": "fixture-manifest.json",
                "fixture_manifest_sha256": file_sha256(
                    artifact_dir / "fixture-manifest.json"
                ),
                "fixture_archive": "fixture",
                "fixture_archive_sha256": manifest_sha256(
                    tree_manifest(fixture_archive)
                ),
                "summary": "summary.json",
                "summary_sha256": file_sha256(artifact_dir / "summary.json"),
            }
            (artifact_dir / "manifest.json").write_text(
                json.dumps(manifest, indent=2, sort_keys=True) + "\n"
            )
        finally:
            if fixture is not None:
                set_tree_writable(fixture)
            cleanup_errors = []
            for path in reversed(added_worktrees):
                if error := remove_worktree(repo, path):
                    cleanup_errors.append(f"{path}: {error}")
            if cleanup_errors:
                remove_tree_if_exists(artifact_dir)
                raise RuntimeError(
                    "failed to remove benchmark worktrees: " + "; ".join(cleanup_errors)
                )
            if not artifact_dir.exists():
                raise RuntimeError("benchmark staging directory disappeared before publication")
        os.replace(artifact_dir, output_dir)
        published = True
        atexit.unregister(remove_tree_if_exists)

    print(f"wrote attested benchmark evidence to {output_dir}")


if __name__ == "__main__":
    main()
